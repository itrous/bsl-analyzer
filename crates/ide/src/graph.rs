//! Agent-facing projection of the whole-config call graph.
//!
//! The MCP surface speaks in **durable string ids**, not interned handles:
//! `MethodId` keys a method by an interned name whose id is assigned per
//! process, so a handle is meaningful only inside the server that issued it.
//! The durable id is a path-derived qualified name (`method/common/
//! РаботаСКонтрагентами/ПроверитьИНН`) that survives edits and re-resolves to the
//! current revision's handle per request. BSL identifiers cannot contain `/`,
//! `:` or `.`, so the structural separators never collide with names.
//!
//! Modules that the metadata index does not key by name (forms, commands) fall
//! back to a workspace-relative path id (`method/file/<relpath>::<method>`),
//! resolvable only when a workspace root is supplied.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use stdx::case::CaseExt;

use bsl_metadata::MdoType;
use code_chunk::{base_chunk_name, ChunkKind, Chunker};
use hir::call_graph::{EdgeKind, MethodDispatch};
use hir::graph_index::{
    display_scope, encode_scope, form_qualified_prefix, form_scope, project_batch_call_edges,
    project_batch_form_edges, project_batch_query_edges, project_form_binding_edges,
    project_workspace_catalog_edges, project_workspace_register_records_edges,
    project_workspace_role_edges, project_workspace_subscription_edges,
    project_workspace_subsystem_edges, EdgeRow, GraphBuildState, GraphIndex, GraphRowEncoder,
    NodeRow,
};
use hir::{
    module_key_for_path, ConfigsDatabase, DefDatabase, GraphNode, MethodId, ModuleId, ModuleIndex,
    ModuleKey, Semantics, WorkspaceCallEdge, WorkspaceCallGraph,
};
use ide_db::base_db::{RootQueryDb, SourceDatabase, SourceRoot, SourceRootId};
use rustc_hash::{FxHashMap, FxHashSet};
use serde::Serialize;
use vfs::FileId;

use crate::{Analysis, RootDatabaseImpl};

/// How much detail to materialise per node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GraphDetail {
    /// Id + name + kind only.
    Names,
    /// Adds the declaration line and dispatch.
    #[default]
    Signatures,
    /// Adds the full method source.
    Bodies,
}

/// Traversal direction over the call graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Callers (incoming edges).
    In,
    /// Callees (outgoing edges).
    Out,
    /// Both.
    Both,
}

/// A node, projected for the agent. `dispatch` is surfaced top-level (not nested
/// under a type) because client/server is a first-order BSL concern.
/// `Eq` is deliberately absent: `location` holds a `serde_json::Value`, which does not
/// implement it. Nothing keys a set or a map by a node — the id string does that — so
/// `PartialEq` is all this type has ever needed.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct NodeRef {
    pub id: String,
    pub kind: &'static str,
    pub name: String,
    /// The display path of a metadata node (russian type name + object path). Omitted
    /// for code nodes (`method`/`module`), where it would only restate `module` + `name`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub qualified: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub module: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// The `source` (a `detail=bodies` body) was cut short — or, when `source` is
    /// absent, dropped entirely — to stay within the output budget. Mirrors
    /// [`SourceItem::truncated`] so a truncated body is not mistaken for a complete
    /// one (or a dropped body for a method with no body) without reading the
    /// envelope. Set by the serving budget pass, never at projection time.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub dispatch: Vec<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_export: Option<bool>,
    /// The module's own methods, populated only for a `module` node so an agent can
    /// discover a module's members directly from `node(module/…)` without a separate
    /// traversal. `None` for every other node kind.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub methods: Option<Vec<ModuleMethod>>,
    /// Whether this id round-trips back to a node on its own. `false` for
    /// path-fallback nodes seen only as an edge endpoint. Serialized only when
    /// `false` (the informative case); absent means addressable.
    #[serde(skip_serializing_if = "is_true")]
    pub addressable: bool,
    /// Where this node lives, under the MCP location contract. Carried as an opaque
    /// value because the contract's types live in the serving layer: giving this crate a
    /// typed twin would mean a second serializer for one published shape.
    ///
    /// Exactly one of this and `location_unavailable` is set on every node a tool serves.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<serde_json::Value>,
    /// Why there is no location: the node has no source file by construction, or the
    /// answering snapshot had no root table. Never both absent and `location` absent on a
    /// served node — silence would read as "no place exists" for a method that has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location_unavailable: Option<&'static str>,
}

/// The location contract's reason for a node that has no source file at all — a metadata
/// object, a form, an attribute. Spelled here as a literal rather than imported from the
/// serving layer, which sits above this crate; the serving layer asserts the two agree.
pub const NO_SOURCE_LOCATION: &str = "no_source_location";

/// The reason a node that DOES have a file still carries no location: the answering
/// snapshot had no root table, so no `(root_id, path)` pair could be formed. The in-memory
/// projection never has one, which is also what keeps it byte-identical to the SQLite
/// projection serving without roots.
pub const ROOTS_UNAVAILABLE: &str = "roots_unavailable";

/// `skip_serializing_if` helper: the field is emitted only when `false`.
fn is_true(v: &bool) -> bool {
    *v
}

/// A member method of a `module` node, surfaced in [`NodeRef::methods`].
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ModuleMethod {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_export: Option<bool>,
}

/// A resolved call edge, projected for the agent. In a neighbours response the
/// traversal root's id is carried once in `root`, not repeated per edge: an absent
/// `from`/`to` means that endpoint is the root node.
///
/// `Eq` is deliberately absent, for the reason it is absent on [`NodeRef`]: the call sites
/// are `serde_json::Value`s, which the serving layer owns the shape of.
///
/// The four `call_site*` fields are all absent unless the caller asked for call sites, and
/// that silence is load-bearing: "this edge has no place" and "nobody asked where this edge
/// is" are different answers, and an edge that answered the first to a caller who asked
/// neither would be read as the second.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct EdgeRef {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    pub kind: &'static str,
    pub provenance: &'static str,
    /// Where the call is written, one place per site, under the MCP location contract.
    /// Never a partial list: a site that failed verification takes the whole list with it
    /// (see `call_sites_unavailable`), so a shorter list than `call_sites_total` means the
    /// display budget or the per-edge cap cut it, and `call_sites_truncated` says so.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_sites: Option<Vec<serde_json::Value>>,
    /// How many sites the artefact records for this edge, counted before any of them is
    /// shown. Present exactly when `call_sites` is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_sites_total: Option<usize>,
    /// The shown list is shorter than `call_sites_total` because the cap or the output
    /// budget cut it — never because a site was dropped.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub call_sites_truncated: bool,
    /// Why there is no place for this edge, from the location contract's closed vocabulary.
    /// Exactly one of this and `call_sites` is set on an edge whose caller asked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_sites_unavailable: Option<&'static str>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub crosses_client_to_server: bool,
}

/// The location contract's code for an edge that stands for no call in code at all.
/// Re-exported from `hir-def` so the serving layer can assert one vocabulary, the same way
/// it does for [`NO_SOURCE_LOCATION`].
pub const NO_CALL_SITE: &str = hir::NO_CALL_SITE;

/// The location contract's code for an edge whose call site this build does not record.
pub const CALL_SITE_NOT_RECORDED: &str = hir::CALL_SITE_NOT_RECORDED;

/// The location contract's code for a span the file has moved out from under.
/// Spelled here as a literal for the same reason as its neighbours; the serving layer
/// asserts the two agree.
pub const SOURCE_DRIFTED: &str = "source_drifted";

/// Why the rows an edge was grouped from carry no span — or `None` when at least one of
/// them does, which leaves the question to whoever holds the root table and the file.
///
/// The answer is read off the rows, never off [`EdgeRef::kind`]: the kind of a call read out
/// of a body is reassigned during resolution (a manager, notify or idle edge gets its kind
/// after extraction), so a kind-driven rule would call a recorded span absent.
pub fn call_site_absence_reason(sites: &[hir::CallSite]) -> Option<&'static str> {
    if sites.iter().any(|site| matches!(site, hir::CallSite::Recorded(_))) {
        return None;
    }
    // Homogeneous in practice, since one pass produces one edge kind. Answering "never"
    // for a group holding one "not yet" would still be the worse of the two mistakes.
    if sites.iter().any(|site| matches!(site, hir::CallSite::NotRecorded)) {
        Some(CALL_SITE_NOT_RECORDED)
    } else {
        Some(NO_CALL_SITE)
    }
}

/// Cold-start orientation for an unfamiliar project.
#[derive(Debug, Clone, Serialize)]
pub struct GraphOverview {
    pub modules: usize,
    pub methods: usize,
    pub mdos: usize,
    pub attributes: usize,
    pub tabular_sections: usize,
    pub forms: usize,
    pub form_items: usize,
    pub form_attributes: usize,
    pub nodes: usize,
    pub edges: usize,
    pub top_by_centrality: Vec<NodeRef>,
    pub edge_provenance: BTreeMap<&'static str, usize>,
    pub client_to_server_edges: usize,
}

/// One node plus optional neighbour expansion.
#[derive(Debug, Clone, Serialize)]
pub struct NodeResult {
    pub node: NodeRef,
}

/// Neighbour traversal result.
#[derive(Debug, Clone, Serialize)]
pub struct NeighborsResult {
    pub root: NodeRef,
    pub nodes: Vec<NodeRef>,
    pub edges: Vec<EdgeRef>,
    /// Total distinct neighbours discovered (excluding the root), before the
    /// `max_nodes` cap. Lets an agent see the true fan-out even when only the
    /// top-centrality slice is returned in `nodes`.
    pub total: usize,
    /// Count of neighbours returned in `nodes` (after the `max_nodes` cap). Always
    /// equals `nodes.len()`; surfaced explicitly so an agent need not count.
    pub returned: usize,
    /// Count of neighbours dropped by the `max_nodes` cap (`total - returned`). The
    /// `dropped` field is a bounded sample of these, not the full set.
    pub dropped_count: usize,
    /// A bounded sample of the ids dropped by the `max_nodes` cap, taken from the
    /// ranked tail so the highest-centrality dropped nodes (those just past the cut)
    /// come first. Capped at [`MAX_DROPPED_SAMPLE`]; the full count is `dropped_count`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub dropped: Vec<String>,
    /// Distribution of the discovered neighbourhood's edges by kind (deduped, over the
    /// full neighbourhood before the node cap), so an agent can size an `edge_kinds`
    /// filter without fetching every edge. Empty when there are no edges.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub by_kind: BTreeMap<&'static str, usize>,
    /// The same distribution by edge provenance.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub by_provenance: BTreeMap<&'static str, usize>,
    /// One-glance trust summary of the shown edges derived from `by_provenance`, so a
    /// consumer (e.g. an impact analysis before a rename/delete) need not reduce the
    /// histogram itself: `resolved_only` — every edge is a direct static resolution;
    /// `contains_inferred` — at least one edge is metadata-inferred or string-dispatched
    /// (a concrete target, lower trust than a direct call). Unresolvable edges are dropped
    /// from the graph, so this rates the shown edges, not recall. Omitted when the
    /// neighbourhood has no edges (no trust claim to make).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<&'static str>,
    /// `true` when the `max_nodes` cap dropped a node that was the endpoint of a
    /// neighbourhood edge, so some edges were omitted from `edges` — a heads-up that the
    /// returned `nodes` can include some whose connecting edge is not shown.
    pub connectors_dropped: bool,
    /// Distinct callees (out-edge targets) discovered, present only when the traversal
    /// went outward (`dir=out`/`both`). Lets a `dir=both` caller see a small outbound
    /// count even when inbound callers dominate the `max_nodes` cap, instead of assuming
    /// there are none — then refine with `dir=out`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub out_total: Option<usize>,
    /// Distinct callers (in-edge sources) discovered, present only when the traversal went
    /// inward (`dir=in`/`both`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub in_total: Option<usize>,
}

/// Upper bound on the `dropped` id sample returned in [`NeighborsResult`]; a hot
/// node can have tens of thousands of low-centrality callers and emitting them all
/// would bloat the response without helping the agent (the count lives in `total`,
/// and the sample only needs to show what kind of nodes sit just past the cut).
pub const MAX_DROPPED_SAMPLE: usize = 10;

/// Source for one requested node.
#[derive(Debug, Clone, Serialize)]
pub struct SourceItem {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<GraphError>,
    /// The source was cut short to stay within the output budget.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    /// No source at all was served because an earlier item already exhausted the budget —
    /// distinct from a method that genuinely has no body. Retry with a larger
    /// `max_output_tokens` or request this id alone.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub skipped_budget_exhausted: bool,
}

/// Budgeted source for a set of nodes.
#[derive(Debug, Clone, Serialize)]
pub struct SourceResult {
    pub items: Vec<SourceItem>,
    /// The token budget was reached; later items carry no source.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub budget_exhausted: bool,
}

/// Why a graph request could not be served.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "error", rename_all = "snake_case")]
pub enum GraphError {
    /// The id is well-formed but resolves to no node in the current revision.
    NotFound { id: String },
    /// The id is malformed.
    BadId { id: String, reason: String },
    /// The operation is not supported for this id (e.g. a path id with no root).
    Unsupported { id: String, reason: String },
}

/// Parameters for [`Analysis::graph_neighbors`].
#[derive(Debug, Clone)]
pub struct NeighborsParams<'a> {
    pub id: &'a str,
    pub dir: Direction,
    pub depth: usize,
    pub max_nodes: usize,
    pub detail: GraphDetail,
    /// Keep only edges whose provenance is in this set, when non-empty.
    pub provenance_filter: Vec<String>,
    /// Keep only edges whose kind label (call/manager_access/query_ref/contains/…) is in
    /// this set, when non-empty. Independent of `provenance_filter` (both must pass).
    pub edge_kind_filter: Vec<String>,
    /// Whether the caller asked where each edge's call is written. `false` leaves every
    /// `call_site*` field off the edge, which is how a consumer tells "nobody asked" from
    /// "this edge has no place".
    pub call_sites: bool,
    /// At most this many places per edge. What the cap cuts is declared, never silent.
    pub max_call_sites: usize,
}

/// A method's outbound graph context, rendered for embedding enrichment. A semantic
/// indexer prepends [`GraphContext::render`] to the method body before embedding, so
/// the vector carries what the method *does* (dispatch, signature, calls, metadata
/// reads), not just its source text. See `.omc/plans/graph-enriched-embeddings.md`.
///
/// Every field derives from the method's own module summaries (no whole-config fold,
/// no inbound/caller facts), so the rendered text — and thus the embedding cache key
/// computed from it — is stable unless the method's own body or its callees' export
/// tables change.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GraphContext {
    /// Client/server dispatch labels (`client` / `server`).
    pub dispatch: Vec<&'static str>,
    /// The declaration header (collapsed to one line), when available.
    pub signature: Option<String>,
    /// Names of the user methods this method calls (sorted, deduped by name).
    pub calls: Vec<String>,
    /// Metadata this method touches or reads, as `Тип.Объект[.Реквизит]` (Russian
    /// spelling, the dominant form in real BSL), sorted and deduped.
    pub reads: Vec<String>,
}

/// Cap on each rendered list, so a method with a huge call/read set cannot bloat the
/// embed text. The overflow count is appended so truncation is visible and stable.
const GRAPH_CONTEXT_LIST_CAP: usize = 32;

impl GraphContext {
    /// The text block prepended to the method body before embedding. Deterministic:
    /// fixed line order, sorted lists, fixed RU/EN dispatch order.
    pub fn render(&self) -> String {
        let mut out = String::new();
        if !self.dispatch.is_empty() {
            let labels: Vec<&str> = self
                .dispatch
                .iter()
                .map(|d| match *d {
                    "server" => "server | сервер",
                    "client" => "client | клиент",
                    other => other,
                })
                .collect();
            out.push_str("Dispatch: ");
            out.push_str(&labels.join(", "));
            out.push('\n');
        }
        if let Some(sig) = &self.signature {
            out.push_str("Signature: ");
            out.push_str(sig);
            out.push('\n');
        }
        render_capped_list(&mut out, "Calls", &self.calls);
        render_capped_list(&mut out, "Reads", &self.reads);
        out
    }

    /// True when there is nothing to embed beyond the body — used by the producer to
    /// skip attaching an empty block.
    pub fn is_empty(&self) -> bool {
        self.dispatch.is_empty()
            && self.signature.is_none()
            && self.calls.is_empty()
            && self.reads.is_empty()
    }
}

fn render_capped_list(out: &mut String, label: &str, items: &[String]) {
    if items.is_empty() {
        return;
    }
    let shown = items.len().min(GRAPH_CONTEXT_LIST_CAP);
    out.push_str(label);
    out.push_str(": ");
    out.push_str(&items[..shown].join(", "));
    if items.len() > shown {
        out.push_str(&format!(" (+{})", items.len() - shown));
    }
    out.push('\n');
}

impl Analysis {
    /// Per-method outbound graph context for embedding enrichment, rendered live from
    /// the per-module Salsa summaries — NO whole-config fold (so this is cheap enough
    /// to call per method at index time, and stays fresh without a graph rebuild).
    ///
    /// `None` only when `method_name` does not resolve to a method in `file_id`; a
    /// resolved leaf method (no calls/reads) still returns its signature + dispatch.
    pub fn graph_context_for_method(
        &self,
        file_id: FileId,
        method_name: &str,
    ) -> Option<GraphContext> {
        let db = self.database();
        let method = Semantics::new(db).find_method(file_id, method_name)?;
        let facts = hir::method_outbound_facts(db, method.id());

        let signature = match (method.name_range(), method.sig_end()) {
            (Some(name), Some(sig_end)) => signature_line(db, file_id, name.start(), sig_end),
            _ => None,
        };

        let mut calls: Vec<String> = facts
            .callees
            .iter()
            .map(|c| hir::Method::new(db, *c).name().as_str().to_string())
            .collect();
        calls.sort();
        calls.dedup();

        let mut reads: Vec<String> = Vec::new();
        for m in &facts.manager_refs {
            reads.push(format!("{}.{}", m.mdo_type.russian_name(), m.object_name));
        }
        for (ty, obj) in &facts.query_reads {
            reads.push(format!("{}.{}", ty.russian_name(), obj));
        }
        for (ty, obj, attr) in &facts.query_attr_reads {
            reads.push(format!("{}.{}.{}", ty.russian_name(), obj, attr));
        }
        reads.sort();
        reads.dedup();

        Some(GraphContext { dispatch: dispatch_labels(facts.dispatch), signature, calls, reads })
    }

    /// Cold-start overview: module/method/edge counts, the most-called methods,
    /// and the provenance/dispatch profile.
    pub fn graph_overview(
        &self,
        source_root_id: SourceRootId,
        workspace_root: Option<&StripRoot>,
        top_n: usize,
    ) -> GraphOverview {
        let ctx = GraphCtx::new(self.database(), source_root_id, workspace_root);
        ctx.overview(top_n)
    }

    /// Resolve a durable id to a single node, at the requested detail.
    pub fn graph_node(
        &self,
        source_root_id: SourceRootId,
        workspace_root: Option<&StripRoot>,
        id: &str,
        detail: GraphDetail,
    ) -> Result<NodeResult, GraphError> {
        let ctx = GraphCtx::new(self.database(), source_root_id, workspace_root);
        let node = ctx.resolve_id(id)?;
        Ok(NodeResult { node: ctx.node_ref(node, detail) })
    }

    /// Rank candidate durable ids for an imprecise `query` (wrong casing, bare name, or
    /// partial id), capped at `limit`, so an agent can recover a canonical id.
    pub fn graph_resolve(
        &self,
        source_root_id: SourceRootId,
        workspace_root: Option<&StripRoot>,
        query: &str,
        limit: usize,
    ) -> ResolveResult {
        let ctx = GraphCtx::new(self.database(), source_root_id, workspace_root);
        ctx.resolve(query, limit)
    }

    /// Traverse callers/callees from a node up to `depth`, bounded by `max_nodes`.
    pub fn graph_neighbors(
        &self,
        source_root_id: SourceRootId,
        workspace_root: Option<&StripRoot>,
        params: &NeighborsParams<'_>,
    ) -> Result<NeighborsResult, GraphError> {
        let ctx = GraphCtx::new(self.database(), source_root_id, workspace_root);
        ctx.neighbors(params)
    }

    /// Fetch method source for a set of durable ids, stopping once the rough
    /// output budget (`max_output_tokens`, ~4 chars/token) is reached. Returns
    /// raw source — the MCP adapter is responsible for any redaction.
    pub fn graph_source(
        &self,
        source_root_id: SourceRootId,
        workspace_root: Option<&StripRoot>,
        ids: &[String],
        max_output_tokens: usize,
    ) -> SourceResult {
        let ctx = GraphCtx::new(self.database(), source_root_id, workspace_root);
        ctx.source(ids, max_output_tokens)
    }
}

/// Tallies from a streaming graph build, for logging and metadata.
#[derive(Debug, Default, Clone)]
pub struct GraphBuildSummary {
    pub modules: usize,
    /// Node rows handed to the sink (before id de-duplication).
    pub node_rows: usize,
    pub edges: usize,
    /// Per-module signature hash (see [`GraphIndex::module_sig_hash`]), captured from
    /// the resident index so a build can persist it for incremental drift checks. One
    /// entry per indexed module.
    pub module_sig_hashes: FxHashMap<ModuleId, u64>,
    /// Objects seen with inconsistent casing across modules, as lowercased
    /// `englishtype/object` keys. Persisted so an incremental rebuild refuses the
    /// body-only fast path for them (their cross-module first-seen ordering is not
    /// reconstructable from the canonicalised store). Empty for the common,
    /// consistently-cased configuration.
    pub casing_variant_objects: Vec<String>,
    /// Module-located-but-unresolved qualified/manager call sites, as
    /// `(target durable scope, lowercased method, caller file)`. Persisted as the
    /// reverse index that lets an incremental rebuild find callers that would newly
    /// resolve when a target module gains (or exports) a method.
    pub unresolved_calls: Vec<(String, String, String)>,
}

/// A per-batch persistence sink: it receives one batch's freshly-encoded node and
/// edge rows and is responsible for storing them. The error is boxed so this layer
/// stays agnostic of the storage backend's error type.
pub type GraphRowSink<'s> =
    dyn FnMut(&[NodeRow], &[EdgeRow]) -> Result<(), Box<dyn std::error::Error + Send + Sync>> + 's;

/// Opens a fresh database holding only the given batch of modules' texts (with
/// their original [`FileId`]s) plus the configuration metadata. Each call's
/// database is dropped before the next, so no more than one batch's Salsa state is
/// resident at a time — the property that keeps the build's peak RAM bounded.
///
/// A whole-config single database does NOT bound RAM: resolving every module in
/// one database accumulates each module's lowering until it exhausts memory (a
/// 25k-module ERP blows past 8 GB). Cross-batch call targets are instead resolved
/// through the resident [`GraphIndex`], never another batch's database, so a batch
/// database needs only its own texts.
pub type BatchDbOpener<'s> = dyn FnMut(&[ModuleId]) -> RootDatabaseImpl + 's;

/// Release checkpoints of a single batch database. The caller receives the work
/// summary at each checkpoint so lifecycle observers can record telemetry, but
/// the helper always performs the drop and cache cleanup regardless of what the
/// observer does.
pub enum BatchDbRelease<'a, S> {
    DatabaseDropped(&'a S),
    NodeCachesCleared(&'a S),
}

/// Release the green-node caches accumulated on the driver thread and every pool
/// worker. The parser dedups subtrees through a thread-local `NodeCache` that
/// holds strong green-node references and never evicts; across a whole-workspace
/// build (tens of thousands of unrelated files, re-parsed once per pass) it would
/// otherwise grow without bound and pin every parsed tree's green storage long
/// after its `Parse`/Salsa memo and per-batch database are dropped. Clearing it
/// between batches bounds that residency to a single batch's worth of trees.
pub(crate) fn clear_node_caches(pool: &rayon::ThreadPool) {
    syntax::clear_shared_node_cache();
    pool.broadcast(|_| syntax::clear_shared_node_cache());
}

/// Open one batch database, run `run` against it, explicitly drop it, then clear
/// parser node caches. `observe` is called after the drop and after the cache
/// clear so callers can emit lifecycle events at exactly those boundaries.
///
/// This helper exists because the same open/drop/clear sequence is repeated in
/// every batched graph build: the call-hierarchy compact index, the workspace
/// graph rows, and incremental reprojection.
pub fn run_batch_db<S>(
    batch: &[ModuleId],
    open_batch: &mut BatchDbOpener<'_>,
    pool: &rayon::ThreadPool,
    run: impl FnOnce(&RootDatabaseImpl) -> S,
    mut observe: impl FnMut(BatchDbRelease<'_, S>),
) -> S {
    let db = open_batch(batch);
    let summary = run(&db);
    drop(db);
    observe(BatchDbRelease::DatabaseDropped(&summary));
    clear_node_caches(pool);
    observe(BatchDbRelease::NodeCachesCleared(&summary));
    summary
}

/// A code chunk projected as a byproduct of the graph pass, for the fused search
/// index. Mirrors the rows the standalone indexer derives from
/// [`code_chunk::Chunker`], plus the outbound graph context attached to method
/// chunks — rendered from the SAME projected edges the graph persists, so it is
/// byte-identical to [`Analysis::graph_context_for_method`] /
/// `GraphDb::graph_context`. `graph_context` is `None` for module-header chunks and
/// for a method the graph does not resolve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkRow {
    pub path: String,
    pub symbol: String,
    pub kind: ChunkKind,
    pub is_export: bool,
    pub annotations: Vec<String>,
    pub line_start: u32,
    pub line_end: u32,
    pub text: String,
    pub graph_context: Option<String>,
}

/// Per-batch sink for the fused chunk stream produced alongside the graph rows.
/// Receives one batch's complete [`ChunkRow`]s (text + already-rendered context) as
/// each batch's edges are finalised, so the producer never holds more than one
/// batch's chunk text — the same bounded-RAM property the graph row sink relies on.
/// The error is boxed so this layer stays agnostic of the storage backend.
pub trait FusedChunkSink {
    fn emit_chunks(
        &mut self,
        chunks: &[ChunkRow],
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;
}

/// Accumulated outbound facts for one source method, gathered from the projected
/// call/query edges across the build passes, then rendered into a [`GraphContext`].
#[derive(Default)]
struct MethodEdgeFacts {
    calls: Vec<String>,
    reads: Vec<String>,
}

/// Fold a batch's projected edges into the per-method context accumulator, mirroring
/// `GraphDb::graph_context`'s edge-kind filtering exactly: `call` edges contribute
/// callee names, metadata-touch edges (`manager_*` / `query_ref`) to an `Mdo` or
/// `Attribute` contribute `Тип.Объект[.Реквизит]` reads. Other edge kinds and
/// targets are ignored, matching the stored-graph renderer.
fn accumulate_method_edges(
    edges: &[WorkspaceCallEdge],
    acc: &mut FxHashMap<MethodId, MethodEdgeFacts>,
    index: &GraphIndex,
) {
    for edge in edges {
        let from = match &edge.from {
            GraphNode::Method(m) => *m,
            _ => continue,
        };
        match edge.kind {
            EdgeKind::DirectLocal | EdgeKind::DirectQualifiedModule => {
                if let GraphNode::Method(to) = &edge.to {
                    if let Some(entry) = index.method_entry(*to) {
                        acc.entry(from).or_default().calls.push(entry.name.as_str().to_string());
                    }
                }
            }
            EdgeKind::ManagerCreates | EdgeKind::ManagerAccess | EdgeKind::QueryRef => {
                let read = match &edge.to {
                    GraphNode::Mdo { mdo_type, object_name } => {
                        Some(format!("{}.{}", mdo_type.russian_name(), object_name.as_str()))
                    }
                    GraphNode::Attribute { mdo_type, object_name, attr_name } => Some(format!(
                        "{}.{}.{}",
                        mdo_type.russian_name(),
                        object_name.as_str(),
                        attr_name.as_str()
                    )),
                    _ => None,
                };
                if let Some(read) = read {
                    acc.entry(from).or_default().reads.push(read);
                }
            }
            _ => {}
        }
    }
}

/// Chunk one batch's modules and stream the rows with their graph context attached.
/// Method chunks get the context rendered from `facts` (the accumulated projected
/// edges) plus the resident index's effective dispatch and the declaration
/// signature; module-header chunks carry no context, mirroring the standalone
/// indexer. Holds only this batch's chunk text, then hands it to `fused`.
fn emit_fused_chunks(
    db: &RootDatabaseImpl,
    batch: &[ModuleId],
    paths: &FxHashMap<FileId, String>,
    index: &GraphIndex,
    facts: &mut FxHashMap<MethodId, MethodEdgeFacts>,
    fused: &mut dyn FusedChunkSink,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut rows: Vec<ChunkRow> = Vec::new();
    for &module in batch {
        let Some(path) = paths.get(&module.file_id) else { continue };
        let file_id = module.file_id;

        // Render each method's outbound context once, keyed by lowercased name so a
        // split method's parts (which share a base name) all resolve to it.
        let mut ctx_by_name: FxHashMap<String, String> = FxHashMap::default();
        if let Some(entries) = index.module_method_entries(module) {
            for entry in entries {
                let mid = MethodId { module, local_id: entry.local_id };
                let dispatch = index
                    .dispatch(&GraphNode::Method(mid))
                    .map(dispatch_labels)
                    .unwrap_or_default();
                let signature =
                    signature_line(db, file_id, entry.name_range.start(), entry.sig_end);
                // Take (not borrow) the method's facts: each method is rendered exactly
                // once, in its own batch's query pass, so removing it here bounds the
                // accumulator to the methods not yet emitted instead of holding every
                // method's calls/reads for the whole build.
                let (mut calls, mut reads) = match facts.remove(&mid) {
                    Some(f) => (f.calls, f.reads),
                    None => (Vec::new(), Vec::new()),
                };
                calls.sort();
                calls.dedup();
                reads.sort();
                reads.dedup();
                let ctx = GraphContext { dispatch, signature, calls, reads };
                ctx_by_name.insert(entry.name.as_str().fold_lower(), ctx.render());
            }
        }

        let parse = db.parse(file_id);
        let source = db.file_text(file_id);
        for chunk in Chunker::chunk_parsed(&parse.syntax_node(), &source) {
            let graph_context = match chunk.kind {
                ChunkKind::Procedure | ChunkKind::Function => {
                    ctx_by_name.get(&base_chunk_name(&chunk.name).fold_lower()).cloned()
                }
                ChunkKind::ModuleHeader => None,
            };
            rows.push(ChunkRow {
                path: path.clone(),
                symbol: chunk.name,
                kind: chunk.kind,
                is_export: chunk.is_export,
                annotations: chunk.annotations,
                line_start: chunk.line_start,
                line_end: chunk.line_end,
                text: chunk.text,
                graph_context,
            });
        }
    }
    fused.emit_chunks(&rows)
}

/// Materialise the per-root metadata memos a batch database will need BEFORE its
/// parallel region consumes it, one module per config root present in the batch.
///
/// Without this the first module of an *extension* root computes
/// `merged_configuration` — a whole-config deep clone — inside a pool worker
/// while every sibling worker parks on that memo: the widest cross-thread
/// blocking window the build has. Warming on the caller's thread closes it. Only
/// roots actually represented in `batch_files` are touched, so batches without
/// extension modules pay nothing.
///
/// Roots are taken from the files themselves, not from the declared config
/// paths: the two sets part company on a workspace whose configuration discovery
/// never found, where the declared root is the workspace and the files attribute
/// to nested directories under it.
pub fn warm_batch_config_roots(
    db: &RootDatabaseImpl,
    batch_files: &[(FileId, std::path::PathBuf)],
) {
    let modules: Vec<_> = batch_files.iter().map(|(file_id, _)| ModuleId::new(*file_id)).collect();
    db.warm_config_roots(&modules);
}

/// Live position of a streaming graph build, shared with an external watchdog.
///
/// A deadlocked parallel region leaves the process alive but silent — no log
/// line, no CPU, no disk growth — so the only way to notice (and to say WHERE
/// it stopped) is a heartbeat the build stamps as it moves. The build calls
/// [`note`](Self::note) at every phase/batch boundary; a monitor thread reads
/// [`ms_since_progress`](Self::ms_since_progress) and dumps
/// [`position`](Self::position) when the build stops advancing.
pub struct GraphBuildTicker {
    started: std::time::Instant,
    /// Milliseconds from `started` at the last `note`, atomically readable.
    last_progress_ms: std::sync::atomic::AtomicU64,
    position: std::sync::Mutex<String>,
    eta_started: std::sync::OnceLock<std::time::Instant>,
    eta_total_intervals: std::sync::atomic::AtomicU64,
    eta_completed_intervals: std::sync::atomic::AtomicU64,
    eta_sampled_elapsed_ns: std::sync::atomic::AtomicU64,
}

impl Default for GraphBuildTicker {
    fn default() -> Self {
        Self {
            started: std::time::Instant::now(),
            last_progress_ms: std::sync::atomic::AtomicU64::new(0),
            position: std::sync::Mutex::new("created".to_owned()),
            eta_started: std::sync::OnceLock::new(),
            eta_total_intervals: std::sync::atomic::AtomicU64::new(0),
            eta_completed_intervals: std::sync::atomic::AtomicU64::new(0),
            eta_sampled_elapsed_ns: std::sync::atomic::AtomicU64::new(0),
        }
    }
}

impl GraphBuildTicker {
    /// Configure a cold full build's equal-weight interval estimate before its first note.
    pub fn set_eta_total_intervals(&self, total: usize) {
        debug_assert!(self.eta_started.get().is_none());
        self.eta_total_intervals.store(total as u64, std::sync::atomic::Ordering::Release);
    }

    /// A remaining-time estimate from the latest completed interval sample.
    pub fn eta_seconds(&self) -> Option<u64> {
        estimate_graph_build_eta_seconds(
            self.eta_total_intervals.load(std::sync::atomic::Ordering::Acquire) as usize,
            self.eta_completed_intervals.load(std::sync::atomic::Ordering::Acquire) as usize,
            std::time::Duration::from_nanos(
                self.eta_sampled_elapsed_ns.load(std::sync::atomic::Ordering::Acquire),
            ),
        )
    }

    /// Record forward progress: the build is entering `batch` (0-based) of
    /// `total` in `phase`, whose first module is `first_path` (empty when the
    /// phase has no per-batch granularity). Also emits the heartbeat trace line
    /// (target `bsl_graph`), so the on-disk log reconstructs the build timeline.
    pub fn note(&self, phase: &str, batch: usize, total: usize, first_path: &str) {
        let now = std::time::Instant::now();
        if phase != "method_nodes" {
            if let Some(started) = self.eta_started.get() {
                let elapsed_ns =
                    now.duration_since(*started).as_nanos().min(u64::MAX as u128) as u64;
                let completed =
                    self.eta_completed_intervals.load(std::sync::atomic::Ordering::Relaxed);
                let total = self.eta_total_intervals.load(std::sync::atomic::Ordering::Relaxed);
                if total > 0 && completed < total {
                    self.eta_sampled_elapsed_ns
                        .store(elapsed_ns, std::sync::atomic::Ordering::Relaxed);
                    self.eta_completed_intervals.fetch_add(1, std::sync::atomic::Ordering::Release);
                }
            } else {
                let _ = self.eta_started.set(now);
            }
        }
        let elapsed = self.started.elapsed().as_millis() as u64;
        self.last_progress_ms.store(elapsed, std::sync::atomic::Ordering::Relaxed);
        let label = if total > 0 {
            format!("{phase} batch {}/{total} (first: {first_path})", batch + 1)
        } else {
            phase.to_owned()
        };
        tracing::info!(target: "bsl_graph", elapsed_ms = elapsed, "graph build: {label}");
        *self.position.lock().unwrap() = label;
    }

    /// Milliseconds since the last [`note`](Self::note).
    pub fn ms_since_progress(&self) -> u64 {
        let elapsed = self.started.elapsed().as_millis() as u64;
        let last = self.last_progress_ms.load(std::sync::atomic::Ordering::Relaxed);
        elapsed.saturating_sub(last)
    }

    /// Human-readable last recorded position (phase + batch + first module).
    pub fn position(&self) -> String {
        self.position.lock().unwrap().clone()
    }
}

// shortcut: Equal interval weights are a heuristic, not phase-calibrated; collect phase costs
// before replacing it if production evidence shows material bias.
fn estimate_graph_build_eta_seconds(
    total_intervals: usize,
    completed_intervals: usize,
    sampled_elapsed: std::time::Duration,
) -> Option<u64> {
    let remaining = total_intervals.checked_sub(completed_intervals)?;
    if total_intervals == 0
        || completed_intervals == 0
        || remaining == 0
        || sampled_elapsed.is_zero()
    {
        return None;
    }
    let estimate_ns = sampled_elapsed
        .as_nanos()
        .checked_mul(remaining as u128)?
        .div_ceil(completed_intervals as u128);
    let seconds = estimate_ns.div_ceil(1_000_000_000);
    u64::try_from(seconds).ok().map(|seconds| seconds.max(1))
}

/// Project the whole-workspace call graph into durable node/edge rows in bounded
/// batches, streaming each batch to `sink` rather than materialising the full
/// graph in memory. The compact [`GraphIndex`] (a per-module method table) is
/// resident throughout; everything else lives one batch at a time — both the
/// index build and the edge projection load each batch's database via `open_batch`
/// and drop it before the next, so peak RAM is bounded by the batch size.
///
/// The emitted rows carry the SAME durable ids as the in-memory serving path (a
/// parity test guards the encoder), and the node set mirrors the in-memory
/// graph's: every method node (including call-free ones) plus every edge endpoint.
///
/// `modules` is every module in the workspace, in a stable order; `paths` maps
/// every file id to its path for id encoding. `batches` are the contiguous runs of
/// `modules` loaded and projected together (see [`stdx::batch::chunks_by_budget`]);
/// their union must be `modules` in order.
#[allow(
    clippy::too_many_arguments,
    reason = "each argument is a distinct borrowed channel of the streaming build \
              (inputs, batch opener, row/chunk sinks, progress); a bundling struct \
              would carry the same lifetimes one level deeper for no clarity gain"
)]
pub fn build_workspace_graph_rows(
    modules: &[ModuleId],
    paths: &FxHashMap<FileId, String>,
    workspace_root: Option<&Path>,
    mdo_files: &hir::graph_index::MdoFiles,
    batches: &[&[ModuleId]],
    open_batch: &mut BatchDbOpener<'_>,
    sink: &mut GraphRowSink<'_>,
    mut fused: Option<&mut dyn FusedChunkSink>,
    ticker: Option<&GraphBuildTicker>,
) -> Result<GraphBuildSummary, Box<dyn std::error::Error + Send + Sync>> {
    debug_assert_eq!(
        batches.iter().map(|batch| batch.len()).sum::<usize>(),
        modules.len(),
        "the batches must cover every module exactly once"
    );
    let batches_total = batches.len();
    // Node rows are flushed to the sink in runs of the largest batch's size.
    let flush_rows = batches.iter().map(|batch| batch.len()).max().unwrap_or(1).max(1);
    // Heartbeat at every phase/batch boundary: which batch is entered and its first
    // module's path. Enough to localise a stalled build to one batch of modules.
    let note = |phase: &str, batch_idx: usize, batch: &[ModuleId]| {
        if let Some(ticker) = ticker {
            let first =
                batch.first().and_then(|m| paths.get(&m.file_id)).map_or("", String::as_str);
            ticker.note(phase, batch_idx, batches_total, first);
        }
    };
    let mark = |phase: &str| {
        if let Some(ticker) = ticker {
            ticker.note(phase, 0, 0, "");
        }
    };

    // A dedicated thread pool for this build's intra-batch parallelism. Each build
    // gets its own pool so concurrent builds never share a worker thread: Salsa
    // attaches at most one database per thread, and the databases here are distinct
    // (per-build, and cloned per job), so a shared pool could attach two databases to
    // one thread and panic. Keeping the pool per build also confines any salsa query
    // that parallelises internally (e.g. metadata loading) to this build's threads.
    let pool = rayon::ThreadPoolBuilder::new().build()?;

    // Build the index batch-by-batch: it must cover every resolution target, but
    // only one batch's item trees are resident while it is assembled.
    let mut index = GraphIndex::new();
    for (i, &batch) in batches.iter().enumerate() {
        note("index", i, batch);
        let db = open_batch(batch);
        index.add_batch(&pool, &db, batch);
        clear_node_caches(&pool);
    }

    // Capture each module's body-free signature hash from the resident index, for
    // persisting alongside the per-file fingerprint so an incremental rebuild can tell
    // a body-only edit (sig unchanged) from a resolution-affecting one.
    let module_sig_hashes: FxHashMap<ModuleId, u64> =
        modules.iter().filter_map(|&m| index.module_sig_hash(m).map(|h| (m, h))).collect();

    // The encoder strips the same rel from the same walked paths, so it has to be given the
    // same root — otherwise a link in the declared spelling would leave the build minting
    // basenames while everything served here minted the real rel.
    let strip_root = workspace_root.map(StripRoot::resolve);
    let encoder = GraphRowEncoder::new(
        &index,
        paths,
        strip_root.as_ref().map(StripRoot::resolved),
        mdo_files,
    );
    let mut summary =
        GraphBuildSummary { modules: modules.len(), module_sig_hashes, ..Default::default() };

    // Phase A — every method node (the fold's dispatch-seeded set), including
    // isolated methods that no edge references. No database needed: the index and
    // path map carry every fact. Flushed in batches of node rows.
    mark("method_nodes");
    let mut node_batch: Vec<NodeRow> = Vec::with_capacity(flush_rows);
    for method in index.method_nodes() {
        node_batch.push(encoder.node_row(&GraphNode::Method(method)));
        if node_batch.len() >= flush_rows {
            summary.node_rows += node_batch.len();
            sink(&node_batch, &[])?;
            node_batch.clear();
        }
    }
    if !node_batch.is_empty() {
        summary.node_rows += node_batch.len();
        sink(&node_batch, &[])?;
        node_batch.clear();
    }

    // Phase B — edges plus the non-method endpoint nodes (ModuleCode/Mdo/Attribute)
    // that only edges introduce. Method endpoints are already covered by Phase A and
    // skipped here; a resident id set keeps a hub object's node from being re-emitted
    // for every edge that targets it.
    //
    // The two edge kinds run as two global passes — call/manager edges across all
    // batches, THEN query edges across all batches — sharing one `GraphBuildState`.
    // This mirrors the fold's Pass-2-then-Pass-3 order, so an object's first-seen
    // (canonical) Mdo/Attribute node spelling, and thus its durable id, is identical
    // to the in-memory graph's. A per-batch call-then-query order would diverge.
    let mut state = GraphBuildState::new();
    let mut seen_aux: FxHashSet<String> = FxHashSet::default();
    let emit = |edges: &[WorkspaceCallEdge],
                summary: &mut GraphBuildSummary,
                seen_aux: &mut FxHashSet<String>,
                sink: &mut GraphRowSink<'_>|
     -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let edge_rows: Vec<EdgeRow> = edges.iter().map(|e| encoder.edge_row(e)).collect();
        let mut aux_nodes: Vec<NodeRow> = Vec::new();
        for edge in edges {
            for endpoint in [&edge.from, &edge.to] {
                if matches!(endpoint, GraphNode::Method(_)) {
                    continue;
                }
                let row = encoder.node_row(endpoint);
                if seen_aux.insert(row.id.clone()) {
                    aux_nodes.push(row);
                }
            }
        }
        summary.node_rows += aux_nodes.len();
        summary.edges += edge_rows.len();
        sink(&aux_nodes, &edge_rows)
    };

    // `paths`-derived scope/file for an unresolved-call ref's target/caller modules.
    let scope_of = |m: ModuleId| -> Option<String> {
        paths.get(&m.file_id).and_then(|p| module_key_for_path(p)).map(|k| encode_scope(&k))
    };
    let file_of = |m: ModuleId| -> Option<String> { paths.get(&m.file_id).cloned() };

    // Per-method outbound facts for the fused chunk context, accumulated from the
    // SAME projected edges the graph persists (so the rendered context is byte-
    // identical to the stored-graph renderer). Lightweight — names and read strings,
    // never method bodies — so holding it across all batches stays bounded. Only
    // populated when a fused sink is attached.
    let mut method_edge_facts: FxHashMap<MethodId, MethodEdgeFacts> = FxHashMap::default();

    let mut unresolved_calls: Vec<(String, String, String)> = Vec::new();
    for (i, &batch) in batches.iter().enumerate() {
        note("call_edges", i, batch);
        let db = open_batch(batch);
        let proj = project_batch_call_edges(&pool, &db, batch, &index, &mut state);
        if fused.is_some() {
            accumulate_method_edges(&proj.edges, &mut method_edge_facts, &index);
        }
        emit(&proj.edges, &mut summary, &mut seen_aux, sink)?;
        for (caller, target, method_lower) in proj.unresolved {
            if let (Some(scope), Some(file)) = (scope_of(target), file_of(caller)) {
                unresolved_calls.push((scope, method_lower, file));
            }
        }
        for (caller, scope, method_lower) in proj.unresolved_declared_common {
            if let Some(file) = file_of(caller) {
                unresolved_calls.push((scope, method_lower, file));
            }
        }
        clear_node_caches(&pool);
    }
    for (i, &batch) in batches.iter().enumerate() {
        note("query_edges", i, batch);
        let db = open_batch(batch);
        let edges = project_batch_query_edges(&pool, &db, batch, &mut state);
        if fused.is_some() {
            accumulate_method_edges(&edges, &mut method_edge_facts, &index);
        }
        emit(&edges, &mut summary, &mut seen_aux, sink)?;
        // Phase 2 of the fused chunk pass: every batch's call/manager edges are now
        // accumulated and this batch's query reads were just folded in, so each
        // method in this batch has its complete outbound fact set. Chunk the batch's
        // modules (one parse, the batch db is still open), attach the rendered
        // context to method chunks, and stream the rows — holding only this batch's
        // chunk text.
        if let Some(fused) = fused.as_deref_mut() {
            emit_fused_chunks(&db, batch, paths, &index, &mut method_edge_facts, fused)?;
        }
        clear_node_caches(&pool);
    }
    summary.unresolved_calls = unresolved_calls;

    // Phase C — `contains` edges from form metadata: `mdo → form` (object forms) and
    // `form → form_item` per element. Runs after the call/query passes (sharing the
    // same `state`) so a form's owner object inherits the canonical spelling those
    // passes assigned; a divergent form-directory casing is recorded as a variant for
    // the gate, exactly like any other. The Form/FormItem/Mdo endpoint nodes are
    // materialised by `emit`. Full-build only — the incremental reprojection never
    // runs it (form structure lives in form XML, so any form-structure change is a
    // metadata drift that already forces a full rebuild).
    for (i, &batch) in batches.iter().enumerate() {
        note("form_edges", i, batch);
        let db = open_batch(batch);
        let edges = project_batch_form_edges(&pool, &db, batch, paths, &mut state);
        emit(&edges, &mut summary, &mut seen_aux, sink)?;
        clear_node_caches(&pool);
    }

    // Phase D — `contains` edges from the metadata catalog: `mdo → attribute`,
    // `mdo → tabular_section`, `tabular_section → attribute`, for EVERY object in
    // every visible configuration (not just code-referenced ones). Runs once, last,
    // sharing `state` so a code-referenced object keeps its code spelling and a
    // metadata-only object is first-seen here. Sequential on the driver thread (Salsa
    // attaches one database per thread; the config loader fans out over its own
    // scope), reusing one batch database for its config access. Full-build only — the
    // catalog is stable under body edits, so the incremental path never re-derives it.
    if let Some(&first) = batches.first() {
        mark("catalog_edges");
        let db = open_batch(first);
        let edges = project_workspace_catalog_edges(&db, first[0].file_id, &mut state);
        emit(&edges, &mut summary, &mut seen_aux, sink)?;
    }

    // Phase E — `data_binding` edges linking the form data model gathered in Phase C to
    // the catalog structure built in Phase D: a UI element's `data_path` field →
    // `attribute`/`ts_attr` node, and a Ref-typed form attribute → `mdo` node. Pure (no
    // database), driver thread, after the catalog so every target node exists (the
    // binding pass gates on the catalog index, so no edge dangles). Full-build only,
    // like the form/catalog passes it joins.
    mark("binding_edges");
    let binding_edges = project_form_binding_edges(&state);
    emit(&binding_edges, &mut summary, &mut seen_aux, sink)?;

    // Phase F — `event_subscription` edges: each `ПодпискаНаСобытие` → its exported
    // handler method. Config-level, resolved through the resident index, sharing `state`
    // so the subscription's `Mdo` node is first-seen here (its `EventSubscription` type
    // never collides with the data objects above). Full-build only: a handler change that
    // could invalidate the edge moves the handler module's signature hash, forcing a full
    // rebuild rather than a body-only reproject.
    if let Some(&first) = batches.first() {
        mark("subscription_edges");
        let db = open_batch(first);
        let edges = project_workspace_subscription_edges(&db, first[0].file_id, &index, &mut state);
        emit(&edges, &mut summary, &mut seen_aux, sink)?;
    }

    // Phase G — `subsystem_membership` edges: each subsystem → its member objects and
    // child subsystems. Config-level, pure metadata, sharing `state` so member names
    // canonicalize to the same spelling as their own nodes from the catalog pass.
    // Full-build only, like the metadata passes above.
    if let Some(&first) = batches.first() {
        mark("subsystem_edges");
        let db = open_batch(first);
        let edges = project_workspace_subsystem_edges(&db, first[0].file_id, &mut state);
        emit(&edges, &mut summary, &mut seen_aux, sink)?;
    }

    // Phase H — `role_reference` edges: each role → the metadata objects it grants rights on
    // (direct object-rights `resolved`, plus objects named inside an RLS restriction condition
    // `inferred`). Config-level, pure metadata, sharing `state` so object names canonicalize to
    // the same spelling as their own nodes. Full-build only, like the metadata passes above.
    if let Some(&first) = batches.first() {
        mark("role_edges");
        let db = open_batch(first);
        let edges = project_workspace_role_edges(&db, first[0].file_id, &mut state);
        emit(&edges, &mut summary, &mut seen_aux, sink)?;
    }

    // Phase I — `register_records` edges: each document → every register it declares it posts
    // (its `RegisterRecords` metadata). Config-level, pure metadata, sharing `state` so register
    // names canonicalize to the same spelling as their own nodes. Full-build only, like the
    // metadata passes above.
    if let Some(&first) = batches.first() {
        mark("register_records_edges");
        let db = open_batch(first);
        let edges = project_workspace_register_records_edges(&db, first[0].file_id, &mut state);
        emit(&edges, &mut summary, &mut seen_aux, sink)?;
    }

    // After both passes the canonicalization state knows every object's spelling(s);
    // record the inconsistently-cased ones for the incremental fast-path gate. Sorted
    // so the persisted set is deterministic across builds (the raw `FxHashSet` order
    // is not), keeping it byte-stable between full and incremental rebuilds.
    let mut variants: Vec<String> = state
        .casing_variant_keys()
        .into_iter()
        .map(|(ty, obj)| format!("{}/{}", ty.fold_lower(), obj))
        .collect();
    variants.sort();
    variants.dedup();
    summary.casing_variant_objects = variants;

    mark("done");
    Ok(summary)
}

/// The durable scope segment for a module path (e.g. `common/Б`, `manager/Catalog/X`),
/// or `None` for a path the metadata index does not key by name. Lets the incremental
/// reverse-index lookup key by the same scope the build recorded.
pub fn scope_for_path(path: &str) -> Option<String> {
    module_key_for_path(path).map(|k| encode_scope(&k))
}

/// Folded durable scope for a common module path. Used for reverse references
/// captured from semantic qualified calls when the declared module has no body
/// path yet; Unicode case folding must happen in Rust rather than SQLite `lower()`.
pub fn folded_common_scope_for_path(path: &str) -> Option<String> {
    match module_key_for_path(path)? {
        hir::ModuleKey::Common { name } => Some(format!("common/{}", name.fold_lower())),
        _ => None,
    }
}

/// Rows for a body-only incremental update: only the `changed` modules' method nodes
/// and outgoing edges (plus the aux endpoint nodes those edges introduce), resolved
/// through a full resident [`GraphIndex`] over `all_modules` so cross-module targets
/// land identically to a full build. Aux node/edge ids carry the changed modules'
/// own object spelling; canonicalising them against the existing store (which alone
/// knows the persisted first-seen casing) is the caller's job.
pub struct ReprojectedRows {
    /// The changed modules' method nodes (kind `method`) plus the aux endpoint nodes
    /// (kind `module`/`mdo`/`attribute`) their edges reference. Distinguish by `kind`.
    pub nodes: Vec<NodeRow>,
    /// The changed modules' outgoing edges (every `from_id` is a changed module's node).
    pub edges: Vec<EdgeRow>,
    /// Signature hash per changed module, for refreshing the persisted `files` rows.
    pub sig_hashes: FxHashMap<ModuleId, u64>,
    /// Casing variants observed AMONG the changed modules (lowercased
    /// `englishtype/object`). A multi-file edit can introduce a newly inconsistent
    /// object; merging these into the persisted set keeps a future reload from taking
    /// the fast path for it. (Changed-vs-unchanged inconsistency is caught separately
    /// by the stored-spelling drift gate.)
    pub casing_variant_objects: Vec<String>,
    /// The reprojected modules' unresolved qualified/manager call sites
    /// `(target scope, lowercased method, caller file)`. The patch replaces these
    /// modules' rows in the persisted reverse index, keeping it accurate.
    pub unresolved_calls: Vec<(String, String, String)>,
}

/// Reproject ONLY `changed` modules for a body-only incremental update. Builds the
/// full resident index over `all_modules` (resolution must see every target), then
/// emits Phase-A method nodes and Phase-B edges + aux endpoints for the changed
/// modules only — every unchanged module's rows are left for the caller to keep in
/// place. The caller must have proven the body-only preconditions (each changed
/// module's `sig_hash` unchanged, no file add/remove, no `.xml` drift).
///
/// `changed` MUST be a subset of `all_modules` in the same (file-id) order, so a
/// brand-new aux object introduced by an edit gets the same first-seen spelling a
/// full build would give it.
#[allow(
    clippy::too_many_arguments,
    reason = "the same distinct borrowed channels as the full build it mirrors; \
              the two signatures are read side by side and are kept alike"
)]
pub fn reproject_changed_modules(
    all_modules: &[ModuleId],
    changed: &[ModuleId],
    paths: &FxHashMap<FileId, String>,
    workspace_root: Option<&Path>,
    mdo_files: &hir::graph_index::MdoFiles,
    batches: &[&[ModuleId]],
    open_batch: &mut BatchDbOpener<'_>,
    ticker: Option<&GraphBuildTicker>,
) -> Result<ReprojectedRows, Box<dyn std::error::Error + Send + Sync>> {
    debug_assert_eq!(
        batches.iter().map(|batch| batch.len()).sum::<usize>(),
        all_modules.len(),
        "the batches must cover every module exactly once"
    );
    let pool = rayon::ThreadPoolBuilder::new().build()?;
    let batches_total = batches.len();
    let note = |phase: &str, batch_idx: usize, batch: &[ModuleId]| {
        if let Some(ticker) = ticker {
            let first =
                batch.first().and_then(|m| paths.get(&m.file_id)).map_or("", String::as_str);
            ticker.note(phase, batch_idx, batches_total, first);
        }
    };

    // Full index over every module: a changed module's qualified/manager call into an
    // unchanged module must still resolve, so the index cannot be limited to `changed`.
    // The same batch runners as the full build → the same stall class; hence the
    // same heartbeat.
    let mut index = GraphIndex::new();
    for (i, &batch) in batches.iter().enumerate() {
        note("reproject_index", i, batch);
        let db = open_batch(batch);
        index.add_batch(&pool, &db, batch);
        clear_node_caches(&pool);
    }

    // The encoder strips the same rel from the same walked paths, so it has to be given the
    // same root — otherwise a link in the declared spelling would leave the build minting
    // basenames while everything served here minted the real rel.
    let strip_root = workspace_root.map(StripRoot::resolve);
    let encoder = GraphRowEncoder::new(
        &index,
        paths,
        strip_root.as_ref().map(StripRoot::resolved),
        mdo_files,
    );
    let changed_set: FxHashSet<ModuleId> = changed.iter().copied().collect();

    // Phase A — method nodes for the changed modules only.
    let mut nodes: Vec<NodeRow> = index
        .method_nodes()
        .filter(|m| changed_set.contains(&m.module))
        .map(|m| encoder.node_row(&GraphNode::Method(m)))
        .collect();

    // Phase B — project the changed modules' edges (call pass then query pass, the
    // fold's order) over one database holding just their texts, with a fresh state.
    // The resulting aux spellings are first-seen WITHIN the changed set, which the
    // caller overrides against the store for objects an unchanged module already owns;
    // a genuinely new object is owned by the changed set in both paths.
    note("reproject_edges", 0, changed);
    let db = open_batch(changed);
    let mut state = GraphBuildState::new();
    let call_proj = project_batch_call_edges(&pool, &db, changed, &index, &mut state);
    let mut projected = call_proj.edges;
    projected.extend(project_batch_query_edges(&pool, &db, changed, &mut state));

    let mut seen_aux: FxHashSet<String> = FxHashSet::default();
    let mut edges: Vec<EdgeRow> = Vec::with_capacity(projected.len());
    for edge in &projected {
        for endpoint in [&edge.from, &edge.to] {
            // Method endpoints are covered by Phase A (changed) or already in the store
            // (unchanged target) — never re-emitted here.
            if matches!(endpoint, GraphNode::Method(_)) {
                continue;
            }
            let row = encoder.node_row(endpoint);
            if seen_aux.insert(row.id.clone()) {
                nodes.push(row);
            }
        }
        edges.push(encoder.edge_row(edge));
    }

    let sig_hashes: FxHashMap<ModuleId, u64> =
        changed.iter().filter_map(|&m| index.module_sig_hash(m).map(|h| (m, h))).collect();
    let mut casing_variant_objects: Vec<String> = state
        .casing_variant_keys()
        .into_iter()
        .map(|(ty, obj)| format!("{}/{}", ty.fold_lower(), obj))
        .collect();
    casing_variant_objects.sort();
    casing_variant_objects.dedup();

    // The reprojected modules' unresolved-call refs, for refreshing the reverse index.
    let scope_of = |m: ModuleId| -> Option<String> {
        paths.get(&m.file_id).and_then(|p| module_key_for_path(p)).map(|k| encode_scope(&k))
    };
    let mut unresolved_calls: Vec<(String, String, String)> = call_proj
        .unresolved
        .into_iter()
        .filter_map(|(caller, target, method_lower)| {
            Some((scope_of(target)?, method_lower, paths.get(&caller.file_id)?.clone()))
        })
        .collect();
    unresolved_calls.extend(call_proj.unresolved_declared_common.into_iter().filter_map(
        |(caller, scope, method_lower)| {
            Some((scope, method_lower, paths.get(&caller.file_id)?.clone()))
        },
    ));

    Ok(ReprojectedRows { nodes, edges, sig_hashes, casing_variant_objects, unresolved_calls })
}

/// Project only graph rows owned by the changed metadata objects and form modules.
/// The caller supplies the owner MDO ids from both the installed and current snapshot,
/// so removals and renames clear their previous rows in the same SQLite transaction.
pub fn reproject_metadata_owners<DB: ConfigsDatabase + Clone + Send>(
    db: &DB,
    representative: FileId,
    form_modules: &[ModuleId],
    paths: &FxHashMap<FileId, String>,
    workspace_root: Option<&Path>,
    mdo_files: &hir::graph_index::MdoFiles,
    owner_ids: &FxHashSet<String>,
) -> (Vec<NodeRow>, Vec<EdgeRow>) {
    let mut state = GraphBuildState::default();
    let mut projected = project_workspace_catalog_edges(db, representative, &mut state);
    if !form_modules.is_empty() {
        let pool =
            rayon::ThreadPoolBuilder::new().build().expect("metadata form projection thread pool");
        projected.extend(project_batch_form_edges(&pool, db, form_modules, paths, &mut state));
        projected.extend(project_form_binding_edges(&state));
    }
    projected.extend(project_workspace_subsystem_edges(db, representative, &mut state));
    projected.extend(project_workspace_role_edges(db, representative, &mut state));
    projected.extend(project_workspace_register_records_edges(db, representative, &mut state));

    let empty_index = GraphIndex::new();
    let strip_root = workspace_root.map(StripRoot::resolve);
    let encoder = GraphRowEncoder::new(
        &empty_index,
        paths,
        strip_root.as_ref().map(StripRoot::resolved),
        mdo_files,
    );
    let form_key = |owner: &Option<(MdoType, String)>, form_name: &str| {
        (owner.as_ref().map(|(kind, name)| (*kind, name.fold_lower())), form_name.fold_lower())
    };
    let form_keys: FxHashSet<(Option<(MdoType, String)>, String)> = form_modules
        .iter()
        .filter_map(|module| {
            let path = paths.get(&module.file_id)?;
            let (owner, form_name) = form_key_for_path(path)?;
            Some(form_key(&owner, &form_name))
        })
        .collect();
    let owner_affected = |node: &GraphNode| {
        let direct_id = owner_ids.contains(&encoder.encode(node).0);
        let owner = match node {
            GraphNode::Mdo { mdo_type, object_name }
            | GraphNode::Attribute { mdo_type, object_name, .. }
            | GraphNode::TabularSection { mdo_type, object_name, .. }
            | GraphNode::TabularSectionAttribute { mdo_type, object_name, .. } => {
                encoder
                    .encode(&GraphNode::Mdo {
                        mdo_type: *mdo_type,
                        object_name: object_name.clone(),
                    })
                    .0
            }
            GraphNode::Form { owner, .. }
            | GraphNode::FormItem { owner, .. }
            | GraphNode::FormAttribute { owner, .. } => owner
                .as_ref()
                .map(|(mdo_type, name)| {
                    encoder
                        .encode(&GraphNode::Mdo { mdo_type: *mdo_type, object_name: name.clone() })
                        .0
                })
                .unwrap_or_default(),
            GraphNode::Method(_) | GraphNode::ModuleCode(_) => String::new(),
        };
        direct_id || owner_ids.contains(&owner)
    };
    let form_affected =
        |node: &GraphNode, keys: &FxHashSet<(Option<(MdoType, String)>, String)>| match node {
            GraphNode::Form { owner, form_name }
            | GraphNode::FormItem { owner, form_name, .. }
            | GraphNode::FormAttribute { owner, form_name, .. } => {
                let owner = owner.as_ref().map(|(kind, name)| (*kind, name.as_str().to_owned()));
                keys.contains(&form_key(&owner, form_name.as_str()))
            }
            _ => false,
        };

    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    let mut seen_nodes = FxHashSet::default();
    let mut seen_edges = FxHashSet::default();
    for edge in projected {
        let owner_delta = owner_affected(&edge.from)
            || owner_affected(&edge.to)
            || form_affected(&edge.from, &form_keys)
            || form_affected(&edge.to, &form_keys);
        if !owner_delta {
            continue;
        }
        for node in [&edge.from, &edge.to] {
            let row = encoder.node_row(node);
            if seen_nodes.insert(row.id.clone()) {
                nodes.push(row);
            }
        }
        let row = encoder.edge_row(&edge);
        if seen_edges.insert((
            row.from_id.clone(),
            row.to_id.clone(),
            row.kind,
            row.provenance,
            row.call_start,
            row.call_end,
            row.call_site_absent,
            row.crosses,
        )) {
            edges.push(row);
        }
    }
    (nodes, edges)
}

/// Parsed owner and form name for a form module path, exposed to the MCP adapter without
/// making it depend on HIR's module-path parser.
pub fn form_key_for_path(path: &str) -> Option<(Option<(MdoType, String)>, String)> {
    let key = hir::parse_form_module_path(path)?;
    Some((key.owner.map(|(kind, name)| (kind, name.as_str().to_owned())), key.form_name))
}

struct GraphCtx<'a> {
    db: &'a RootDatabaseImpl,
    graph: Arc<WorkspaceCallGraph>,
    index: Arc<ModuleIndex>,
    source_root: SourceRoot,
    /// The base this view's ids are stripped against, handed in already resolved by whoever
    /// established the file universe — see [`StripRoot`].
    strip_root: Option<StripRoot>,
}

impl<'a> GraphCtx<'a> {
    fn new(
        db: &'a RootDatabaseImpl,
        source_root_id: SourceRootId,
        workspace_root: Option<&'a StripRoot>,
    ) -> Self {
        let graph = db.workspace_call_graph(source_root_id);
        let index = db.module_index(source_root_id);
        let source_root = db.source_root_input(source_root_id).root(db).clone();
        Self { db, graph, index, source_root, strip_root: workspace_root.cloned() }
    }

    fn path_for(&self, file_id: FileId) -> Option<String> {
        let vfs_path = self.source_root.file_set().path_for_file(&file_id)?;
        Some(vfs_path.as_path().to_str()?.replace('\\', "/"))
    }

    fn rel_path(&self, abs: &str) -> Option<String> {
        self.strip_root.as_ref()?.rel(abs)
    }

    // ---- id encoding --------------------------------------------------------

    fn encode_node(&self, node: &GraphNode) -> (String, bool) {
        match node {
            GraphNode::Method(method) => self.encode_method(*method),
            GraphNode::ModuleCode(module) => self.encode_module(*module),
            GraphNode::Mdo { mdo_type, object_name } => {
                (format!("mdo/{}/{}", mdo_type.english_name(), object_name.as_str()), true)
            }
            GraphNode::Attribute { mdo_type, object_name, attr_name } => (
                format!(
                    "attribute/{}/{}/{}",
                    mdo_type.english_name(),
                    object_name.as_str(),
                    attr_name.as_str()
                ),
                true,
            ),
            GraphNode::Form { owner, form_name } => {
                (format!("form/{}/{}", form_scope(owner), form_name.as_str()), true)
            }
            GraphNode::FormItem { owner, form_name, item_name } => (
                format!(
                    "form_item/{}/{}/{}",
                    form_scope(owner),
                    form_name.as_str(),
                    item_name.as_str()
                ),
                true,
            ),
            GraphNode::FormAttribute { owner, form_name, attr_name } => (
                format!(
                    "form_attr/{}/{}/{}",
                    form_scope(owner),
                    form_name.as_str(),
                    attr_name.as_str()
                ),
                true,
            ),
            GraphNode::TabularSection { mdo_type, object_name, section_name } => (
                format!(
                    "tabular_section/{}/{}/{}",
                    mdo_type.english_name(),
                    object_name.as_str(),
                    section_name.as_str()
                ),
                true,
            ),
            GraphNode::TabularSectionAttribute {
                mdo_type,
                object_name,
                section_name,
                attr_name,
            } => (
                format!(
                    "ts_attr/{}/{}/{}/{}",
                    mdo_type.english_name(),
                    object_name.as_str(),
                    section_name.as_str(),
                    attr_name.as_str()
                ),
                true,
            ),
        }
    }

    fn encode_method(&self, method: MethodId) -> (String, bool) {
        let method_name = self.method_name(method);
        let path = self.path_for(method.module.file_id);
        if let Some(key) = path.as_deref().and_then(module_key_for_path) {
            let scope = encode_scope(&key);
            (format!("method/{scope}/{method_name}"), true)
        } else if let Some(rel) = path.as_deref().and_then(|p| self.rel_path(p)) {
            (format!("method/file/{rel}::{method_name}"), true)
        } else {
            let basename = path.as_deref().and_then(basename).unwrap_or("?");
            (format!("method/file/{basename}::{method_name}"), false)
        }
    }

    fn encode_module(&self, module: ModuleId) -> (String, bool) {
        let path = self.path_for(module.file_id);
        if let Some(key) = path.as_deref().and_then(module_key_for_path) {
            (format!("module/{}", encode_scope(&key)), true)
        } else if let Some(rel) = path.as_deref().and_then(|p| self.rel_path(p)) {
            (format!("module/file/{rel}"), true)
        } else {
            let basename = path.as_deref().and_then(basename).unwrap_or("?");
            (format!("module/file/{basename}"), false)
        }
    }

    // ---- id resolution ------------------------------------------------------

    fn resolve_id(&self, id: &str) -> Result<GraphNode, GraphError> {
        let not_found = || GraphError::NotFound { id: id.to_string() };
        match classify_graph_id(id)? {
            GraphIdKind::MethodFile { rel, name } => {
                let file_id = self.resolve_rel_path(&rel).ok_or_else(not_found)?;
                self.resolve_method_in(file_id, &name, id)
            }
            GraphIdKind::ModuleFile { rel } => {
                let file_id = self.resolve_rel_path(&rel).ok_or_else(not_found)?;
                Ok(GraphNode::ModuleCode(ModuleId::new(file_id)))
            }
            GraphIdKind::Method { scope, name } => {
                let file_id = self.index.resolve_module_key(&scope).ok_or_else(not_found)?;
                self.resolve_method_in(file_id, &name, id)
            }
            GraphIdKind::Module { scope } => {
                let file_id = self.index.resolve_module_key(&scope).ok_or_else(not_found)?;
                Ok(GraphNode::ModuleCode(ModuleId::new(file_id)))
            }
            GraphIdKind::Mdo { mdo_type, object } => self.find_mdo_node(mdo_type, &object, id),
            GraphIdKind::Attribute { mdo_type, object, attr } => {
                self.find_attribute_node(mdo_type, &object, &attr, id)
            }
            GraphIdKind::Form { owner, form_name } => self.find_form_node(&owner, &form_name, id),
            GraphIdKind::FormItem { owner, form_name, item_name } => {
                self.find_form_item_node(&owner, &form_name, &item_name, id)
            }
            GraphIdKind::FormAttribute { owner, form_name, attr_name } => {
                self.find_form_attribute_node(&owner, &form_name, &attr_name, id)
            }
            GraphIdKind::TabularSection { mdo_type, object, section } => {
                self.find_tabular_section_node(mdo_type, &object, &section, id)
            }
            GraphIdKind::TabularSectionAttribute { mdo_type, object, section, attr } => {
                self.find_tabular_section_attribute_node(mdo_type, &object, &section, &attr, id)
            }
        }
    }

    fn resolve_method_in(
        &self,
        file_id: FileId,
        method_name: &str,
        id: &str,
    ) -> Result<GraphNode, GraphError> {
        let sema = Semantics::new(self.db);
        let method = sema
            .find_method(file_id, method_name)
            .ok_or_else(|| GraphError::NotFound { id: id.to_string() })?;
        Ok(GraphNode::Method(method.id()))
    }

    /// The metadata-object node for `(mdo_type, object)`, if the workspace graph
    /// references it. Case-insensitive on the object name (BSL is case-insensitive);
    /// returns the graph's canonical spelling.
    fn find_mdo_node(
        &self,
        mdo_type: MdoType,
        object: &str,
        id: &str,
    ) -> Result<GraphNode, GraphError> {
        let object_lower = object.fold_lower();
        self.graph
            .nodes()
            .find(|n| {
                matches!(n, GraphNode::Mdo { mdo_type: mt, object_name }
                    if *mt == mdo_type && object_name.as_str().fold_lower() == object_lower)
            })
            .ok_or_else(|| GraphError::NotFound { id: id.to_string() })
    }

    /// The attribute node for `(mdo_type, object, attr)`, if the graph references
    /// it. Case-insensitive on object and attribute names; returns the canonical node.
    fn find_attribute_node(
        &self,
        mdo_type: MdoType,
        object: &str,
        attr: &str,
        id: &str,
    ) -> Result<GraphNode, GraphError> {
        let object_lower = object.fold_lower();
        let attr_lower = attr.fold_lower();
        self.graph
            .nodes()
            .find(|n| {
                matches!(n, GraphNode::Attribute { mdo_type: mt, object_name, attr_name }
                    if *mt == mdo_type
                        && object_name.as_str().fold_lower() == object_lower
                        && attr_name.as_str().fold_lower() == attr_lower)
            })
            .ok_or_else(|| GraphError::NotFound { id: id.to_string() })
    }

    /// The form node for `(owner, form_name)`, if the graph references it.
    /// Case-insensitive on the owner object and form name; returns the canonical node.
    fn find_form_node(
        &self,
        owner: &Option<(MdoType, String)>,
        form_name: &str,
        id: &str,
    ) -> Result<GraphNode, GraphError> {
        let form_lower = form_name.fold_lower();
        self.graph
            .nodes()
            .find(|n| match n {
                GraphNode::Form { owner: o, form_name: fname } => {
                    form_owner_matches(o.as_ref().map(|(t, n)| (*t, n.as_str())), owner)
                        && fname.as_str().fold_lower() == form_lower
                }
                _ => false,
            })
            .ok_or_else(|| GraphError::NotFound { id: id.to_string() })
    }

    /// The form-item node for `(owner, form_name, item_name)`, if the graph
    /// references it. Case-insensitive on all name components.
    fn find_form_item_node(
        &self,
        owner: &Option<(MdoType, String)>,
        form_name: &str,
        item_name: &str,
        id: &str,
    ) -> Result<GraphNode, GraphError> {
        let form_lower = form_name.fold_lower();
        let item_lower = item_name.fold_lower();
        self.graph
            .nodes()
            .find(|n| match n {
                GraphNode::FormItem { owner: o, form_name: fname, item_name: iname } => {
                    form_owner_matches(o.as_ref().map(|(t, n)| (*t, n.as_str())), owner)
                        && fname.as_str().fold_lower() == form_lower
                        && iname.as_str().fold_lower() == item_lower
                }
                _ => false,
            })
            .ok_or_else(|| GraphError::NotFound { id: id.to_string() })
    }

    /// The form-attribute node for `(owner, form_name, attr_name)`, if the graph
    /// references it. Case-insensitive on all name components.
    fn find_form_attribute_node(
        &self,
        owner: &Option<(MdoType, String)>,
        form_name: &str,
        attr_name: &str,
        id: &str,
    ) -> Result<GraphNode, GraphError> {
        let form_lower = form_name.fold_lower();
        let attr_lower = attr_name.fold_lower();
        self.graph
            .nodes()
            .find(|n| match n {
                GraphNode::FormAttribute { owner: o, form_name: fname, attr_name: aname } => {
                    form_owner_matches(o.as_ref().map(|(t, n)| (*t, n.as_str())), owner)
                        && fname.as_str().fold_lower() == form_lower
                        && aname.as_str().fold_lower() == attr_lower
                }
                _ => false,
            })
            .ok_or_else(|| GraphError::NotFound { id: id.to_string() })
    }

    /// The tabular-section node for `(mdo_type, object, section)`, if the graph
    /// references it. Case-insensitive on object and section names.
    fn find_tabular_section_node(
        &self,
        mdo_type: MdoType,
        object: &str,
        section: &str,
        id: &str,
    ) -> Result<GraphNode, GraphError> {
        let object_lower = object.fold_lower();
        let section_lower = section.fold_lower();
        self.graph
            .nodes()
            .find(|n| {
                matches!(n, GraphNode::TabularSection { mdo_type: mt, object_name, section_name }
                    if *mt == mdo_type
                        && object_name.as_str().fold_lower() == object_lower
                        && section_name.as_str().fold_lower() == section_lower)
            })
            .ok_or_else(|| GraphError::NotFound { id: id.to_string() })
    }

    /// The tabular-section column node for `(mdo_type, object, section, attr)`.
    /// Case-insensitive on all name components.
    fn find_tabular_section_attribute_node(
        &self,
        mdo_type: MdoType,
        object: &str,
        section: &str,
        attr: &str,
        id: &str,
    ) -> Result<GraphNode, GraphError> {
        let object_lower = object.fold_lower();
        let section_lower = section.fold_lower();
        let attr_lower = attr.fold_lower();
        self.graph
            .nodes()
            .find(|n| {
                matches!(n, GraphNode::TabularSectionAttribute { mdo_type: mt, object_name, section_name, attr_name }
                    if *mt == mdo_type
                        && object_name.as_str().fold_lower() == object_lower
                        && section_name.as_str().fold_lower() == section_lower
                        && attr_name.as_str().fold_lower() == attr_lower)
            })
            .ok_or_else(|| GraphError::NotFound { id: id.to_string() })
    }

    fn resolve_rel_path(&self, rel: &str) -> Option<FileId> {
        for file_id in self.source_root.iter() {
            let abs = match self.path_for(file_id) {
                Some(p) => p,
                None => continue,
            };
            match self.rel_path(&abs) {
                Some(file_rel) if file_rel == rel => return Some(file_id),
                _ => {}
            }
        }
        None
    }

    // ---- projection ---------------------------------------------------------

    fn method_name(&self, method: MethodId) -> String {
        hir::Method::new(self.db, method).name().as_str().to_string()
    }

    fn node_ref(&self, node: GraphNode, detail: GraphDetail) -> NodeRef {
        let (id, addressable) = self.encode_node(&node);
        match node {
            GraphNode::Method(method) => self.method_node_ref(method, id, addressable, detail),
            GraphNode::ModuleCode(module) => self.module_node_ref(module, id, addressable),
            GraphNode::Mdo { mdo_type, object_name } => {
                self.mdo_node_ref(mdo_type, object_name.as_str(), id, addressable)
            }
            GraphNode::Attribute { mdo_type, object_name, attr_name } => {
                let name = attr_name.as_str().to_string();
                let qualified =
                    format!("{}.{}.{name}", mdo_type.russian_name(), object_name.as_str());
                NodeRef {
                    id,
                    kind: "attribute",
                    name,
                    qualified: Some(qualified),
                    module: None,
                    signature: None,
                    source: None,
                    truncated: false,
                    dispatch: Vec::new(),
                    is_export: None,
                    methods: None,
                    addressable,
                    location: None,
                    location_unavailable: Some(NO_SOURCE_LOCATION),
                }
            }
            GraphNode::Form { owner, form_name } => NodeRef {
                qualified: Some(format!(
                    "{}.Форма.{}",
                    form_qualified_prefix(&owner),
                    form_name.as_str()
                )),
                name: form_name.as_str().to_string(),
                kind: "form",
                id,
                module: None,
                signature: None,
                source: None,
                truncated: false,
                dispatch: Vec::new(),
                is_export: None,
                methods: None,
                addressable,
                location: None,
                location_unavailable: Some(NO_SOURCE_LOCATION),
            },
            GraphNode::FormItem { owner, form_name, item_name } => NodeRef {
                qualified: Some(format!(
                    "{}.Форма.{}.{}",
                    form_qualified_prefix(&owner),
                    form_name.as_str(),
                    item_name.as_str()
                )),
                name: item_name.as_str().to_string(),
                kind: "form_item",
                id,
                module: None,
                signature: None,
                source: None,
                truncated: false,
                dispatch: Vec::new(),
                is_export: None,
                methods: None,
                addressable,
                location: None,
                location_unavailable: Some(NO_SOURCE_LOCATION),
            },
            GraphNode::FormAttribute { owner, form_name, attr_name } => NodeRef {
                qualified: Some(format!(
                    "{}.Форма.{}.Реквизит.{}",
                    form_qualified_prefix(&owner),
                    form_name.as_str(),
                    attr_name.as_str()
                )),
                name: attr_name.as_str().to_string(),
                kind: "form_attribute",
                id,
                module: None,
                signature: None,
                source: None,
                truncated: false,
                dispatch: Vec::new(),
                is_export: None,
                methods: None,
                addressable,
                location: None,
                location_unavailable: Some(NO_SOURCE_LOCATION),
            },
            GraphNode::TabularSection { mdo_type, object_name, section_name } => NodeRef {
                qualified: Some(format!(
                    "{}.{}.ТабличнаяЧасть.{}",
                    mdo_type.russian_name(),
                    object_name.as_str(),
                    section_name.as_str()
                )),
                name: section_name.as_str().to_string(),
                kind: "tabular_section",
                id,
                module: None,
                signature: None,
                source: None,
                truncated: false,
                dispatch: Vec::new(),
                is_export: None,
                methods: None,
                addressable,
                location: None,
                location_unavailable: Some(NO_SOURCE_LOCATION),
            },
            GraphNode::TabularSectionAttribute {
                mdo_type,
                object_name,
                section_name,
                attr_name,
            } => NodeRef {
                qualified: Some(format!(
                    "{}.{}.{}.{}",
                    mdo_type.russian_name(),
                    object_name.as_str(),
                    section_name.as_str(),
                    attr_name.as_str()
                )),
                name: attr_name.as_str().to_string(),
                kind: "attribute",
                id,
                module: None,
                signature: None,
                source: None,
                truncated: false,
                dispatch: Vec::new(),
                is_export: None,
                methods: None,
                addressable,
                location: None,
                location_unavailable: Some(NO_SOURCE_LOCATION),
            },
        }
    }

    fn mdo_node_ref(
        &self,
        mdo_type: MdoType,
        object_name: &str,
        id: String,
        addressable: bool,
    ) -> NodeRef {
        let name = object_name.to_string();
        let qualified = format!("{}.{name}", mdo_type.russian_name());
        NodeRef {
            id,
            kind: "mdo",
            name,
            qualified: Some(qualified),
            module: None,
            signature: None,
            source: None,
            truncated: false,
            dispatch: Vec::new(),
            is_export: None,
            methods: None,
            addressable,
            location: None,
            location_unavailable: Some(NO_SOURCE_LOCATION),
        }
    }

    fn method_node_ref(
        &self,
        method: MethodId,
        id: String,
        addressable: bool,
        detail: GraphDetail,
    ) -> NodeRef {
        let m = hir::Method::new(self.db, method);
        let name = m.name().as_str().to_string();
        let module_display = self.module_display(method.module);
        let dispatch = self
            .graph
            .dispatch(&GraphNode::Method(method))
            .map(dispatch_labels)
            .unwrap_or_default();

        let mut node = NodeRef {
            id,
            kind: "method",
            name,
            qualified: None,
            module: module_display,
            signature: None,
            source: None,
            truncated: false,
            dispatch,
            is_export: Some(m.is_export()),
            methods: None,
            addressable,
            location: None,
            location_unavailable: Some(ROOTS_UNAVAILABLE),
        };

        if matches!(detail, GraphDetail::Signatures | GraphDetail::Bodies) {
            // The full declaration header, from the keyword line through the closing
            // `)` / export keyword, with wrapped parameter lines collapsed to one.
            node.signature = match (m.name_range(), m.sig_end()) {
                (Some(name), Some(sig_end)) => {
                    self.signature_at(method.module.file_id, name.start(), sig_end)
                }
                _ => None,
            };
            if detail == GraphDetail::Bodies {
                node.source = m.source_range().and_then(|r| self.slice(method.module.file_id, r));
            }
        }
        node
    }

    fn module_node_ref(&self, module: ModuleId, id: String, addressable: bool) -> NodeRef {
        let display = self.module_display(module);
        let name = display.clone().unwrap_or_else(|| "<модуль>".to_string());
        NodeRef {
            id,
            kind: "module",
            name,
            qualified: None,
            module: display,
            signature: None,
            source: None,
            truncated: false,
            dispatch: self
                .graph
                .dispatch(&GraphNode::ModuleCode(module))
                .map(dispatch_labels)
                .unwrap_or_default(),
            is_export: None,
            // The member-method list is served from the on-disk graph (the production
            // path); the in-memory serving path resolves the module node itself but does
            // not enumerate members here.
            methods: None,
            addressable,
            location: None,
            location_unavailable: Some(ROOTS_UNAVAILABLE),
        }
    }

    fn module_display(&self, module: ModuleId) -> Option<String> {
        let path = self.path_for(module.file_id)?;
        match module_key_for_path(&path) {
            Some(key) => Some(display_scope(&key)),
            None => self.rel_path(&path).or_else(|| basename(&path).map(str::to_string)),
        }
    }

    fn slice(&self, file_id: FileId, range: syntax::TextRange) -> Option<String> {
        let text = self.db.file_text(file_id);
        let start = u32::from(range.start()) as usize;
        let end = u32::from(range.end()) as usize;
        // Line endings are normalized to LF before redaction and budget clamping:
        // consumers read the source, they do not need byte-exact CRLF, and the CR
        // would only inflate the JSON escaping.
        text.get(start..end).map(|s| s.replace("\r\n", "\n"))
    }

    fn method_source(&self, method: MethodId) -> Option<String> {
        let range = hir::Method::new(self.db, method).source_range()?;
        self.slice(method.module.file_id, range)
    }

    fn source(&self, ids: &[String], max_output_tokens: usize) -> SourceResult {
        let budget_chars = max_output_tokens.saturating_mul(4).max(1);
        let mut used = 0usize;
        let mut budget_exhausted = false;
        let mut items = Vec::with_capacity(ids.len());

        for id in ids {
            let item = match self.resolve_id(id) {
                Err(err) => SourceItem {
                    id: id.clone(),
                    source: None,
                    error: Some(err),
                    truncated: false,
                    skipped_budget_exhausted: false,
                },
                Ok(GraphNode::Method(method)) => match self.method_source(method) {
                    Some(src) => {
                        if used >= budget_chars {
                            budget_exhausted = true;
                            SourceItem {
                                id: id.clone(),
                                source: None,
                                error: None,
                                truncated: true,
                                skipped_budget_exhausted: true,
                            }
                        } else {
                            let remaining = budget_chars - used;
                            let (text, truncated) = clamp_source(src, remaining);
                            used += text.len();
                            budget_exhausted |= truncated;
                            SourceItem {
                                id: id.clone(),
                                source: Some(text),
                                error: None,
                                truncated,
                                skipped_budget_exhausted: false,
                            }
                        }
                    }
                    None => SourceItem {
                        id: id.clone(),
                        source: None,
                        error: Some(GraphError::NotFound { id: id.clone() }),
                        truncated: false,
                        skipped_budget_exhausted: false,
                    },
                },
                Ok(GraphNode::ModuleCode(_)) => SourceItem {
                    id: id.clone(),
                    source: None,
                    error: Some(GraphError::Unsupported {
                        id: id.clone(),
                        reason: "module-body source is not served; request a method".to_string(),
                    }),
                    truncated: false,
                    skipped_budget_exhausted: false,
                },
                Ok(GraphNode::Mdo { .. })
                | Ok(GraphNode::Attribute { .. })
                | Ok(GraphNode::Form { .. })
                | Ok(GraphNode::FormItem { .. })
                | Ok(GraphNode::FormAttribute { .. })
                | Ok(GraphNode::TabularSection { .. })
                | Ok(GraphNode::TabularSectionAttribute { .. }) => SourceItem {
                    id: id.clone(),
                    source: None,
                    error: Some(GraphError::Unsupported {
                        id: id.clone(),
                        reason: "a metadata node has no source; request a method".to_string(),
                    }),
                    truncated: false,
                    skipped_budget_exhausted: false,
                },
            };
            items.push(item);
        }

        SourceResult { items, budget_exhausted }
    }

    /// The full signature slice: from the keyword line containing `name_offset`
    /// through the header end `sig_end`, with internal whitespace (including the
    /// newlines of a wrapped parameter list) collapsed to single spaces.
    fn signature_at(
        &self,
        file_id: FileId,
        name_offset: syntax::TextSize,
        sig_end: syntax::TextSize,
    ) -> Option<String> {
        signature_line(self.db, file_id, name_offset, sig_end)
    }

    // ---- queries ------------------------------------------------------------

    fn overview(&self, top_n: usize) -> GraphOverview {
        let mut methods = 0usize;
        let mut mdos = 0usize;
        let mut attributes = 0usize;
        let mut tabular_sections = 0usize;
        let mut forms = 0usize;
        let mut form_items = 0usize;
        let mut form_attributes = 0usize;
        let mut node_count = 0usize;
        // The true module population: every module that owns a method, plus any module
        // body seen as a node (a `module/<scope>` node exists only when it is an edge
        // endpoint, so counting those alone undercounts — see the method-derived union).
        let mut module_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
        for node in self.graph.nodes() {
            node_count += 1;
            match node {
                GraphNode::Method(_) => {
                    methods += 1;
                    let (id, _) = self.encode_node(&node);
                    if let Some(module) = module_id_of_method(&id) {
                        module_ids.insert(module);
                    }
                }
                GraphNode::ModuleCode(_) => {
                    let (id, _) = self.encode_node(&node);
                    module_ids.insert(id);
                }
                GraphNode::Mdo { .. } => mdos += 1,
                GraphNode::Attribute { .. } => attributes += 1,
                GraphNode::TabularSectionAttribute { .. } => attributes += 1,
                GraphNode::TabularSection { .. } => tabular_sections += 1,
                GraphNode::Form { .. } => forms += 1,
                GraphNode::FormItem { .. } => form_items += 1,
                GraphNode::FormAttribute { .. } => form_attributes += 1,
            }
        }
        let modules = module_ids.len();

        let mut edge_provenance: BTreeMap<&'static str, usize> = BTreeMap::new();
        let mut client_to_server_edges = 0usize;
        for edge in self.graph.edges() {
            *edge_provenance.entry(provenance_label(edge)).or_default() += 1;
            if edge.crosses_client_to_server {
                client_to_server_edges += 1;
            }
        }

        let mut ranked: Vec<(usize, GraphNode)> = self
            .graph
            .nodes()
            .map(|n| (self.graph.in_degree(&n), n))
            .filter(|(d, _)| *d > 0)
            .collect();
        ranked.sort_by_key(|&(degree, _)| std::cmp::Reverse(degree));
        let top_by_centrality = ranked
            .into_iter()
            .take(top_n)
            .map(|(_, n)| self.node_ref(n, GraphDetail::Signatures))
            .collect();

        GraphOverview {
            modules,
            methods,
            mdos,
            attributes,
            tabular_sections,
            forms,
            form_items,
            form_attributes,
            nodes: node_count,
            edges: self.graph.edge_count(),
            top_by_centrality,
            edge_provenance,
            client_to_server_edges,
        }
    }

    /// Near-miss id lookup: rank every node's durable id against an imprecise `query`
    /// (wrong casing, bare method/object name, or partial id), so an agent can recover a
    /// canonical id from a `not_found` without guessing.
    fn resolve(&self, query: &str, limit: usize) -> ResolveResult {
        let (candidates, total) = rank_resolve_candidates(
            self.graph.nodes().map(|node| {
                let (id, _) = self.encode_node(&node);
                (id, graph_node_kind(&node))
            }),
            query,
            limit,
        );
        ResolveResult::new(query, candidates, total)
    }

    fn neighbors(&self, params: &NeighborsParams<'_>) -> Result<NeighborsResult, GraphError> {
        let root = self.resolve_id(params.id)?;
        let depth = params.depth.max(1);

        let mut visited: Vec<GraphNode> = vec![root.clone()];
        let mut seen: std::collections::HashSet<GraphNode> = std::collections::HashSet::new();
        seen.insert(root.clone());
        let mut out_edges: Vec<&WorkspaceCallEdge> = Vec::new();
        // Distinct non-root nodes reached downstream (as an edge target) vs upstream (as
        // an edge source), so a `Both` traversal can report each direction's fan-out.
        let mut out_reached: std::collections::HashSet<GraphNode> =
            std::collections::HashSet::new();
        let mut in_reached: std::collections::HashSet<GraphNode> = std::collections::HashSet::new();
        let mut frontier = vec![root.clone()];

        for _ in 0..depth {
            let mut next: Vec<GraphNode> = Vec::new();
            for node in &frontier {
                for edge in self.directed_edges(node, params.dir) {
                    if !self.provenance_allowed(edge, &params.provenance_filter)
                        || !edge_kind_allowed(edge.kind, &params.edge_kind_filter)
                    {
                        continue;
                    }
                    out_edges.push(edge);
                    let downstream = &edge.from == node;
                    let other = if downstream { &edge.to } else { &edge.from };
                    if *other != root {
                        if downstream {
                            out_reached.insert(other.clone());
                        } else {
                            in_reached.insert(other.clone());
                        }
                    }
                    if seen.insert(other.clone()) {
                        next.push(other.clone());
                        visited.push(other.clone());
                    }
                }
            }
            if next.is_empty() {
                break;
            }
            frontier = next;
        }
        let out_total =
            matches!(params.dir, Direction::Out | Direction::Both).then_some(out_reached.len());
        let in_total =
            matches!(params.dir, Direction::In | Direction::Both).then_some(in_reached.len());

        // Centrality-ranked tail-drop of discovered (non-root) nodes. Tie-break by
        // durable id so a cut through equal-centrality nodes keeps/drops the same
        // set as the SQLite serve path (`graph_query::neighbors`).
        let mut discovered: Vec<GraphNode> = visited.into_iter().filter(|n| *n != root).collect();
        let total = discovered.len();
        discovered.sort_by_cached_key(|n| {
            (std::cmp::Reverse(self.graph.in_degree(n)), self.encode_node(n).0)
        });
        let mut dropped: Vec<String> = Vec::new();
        if discovered.len() > params.max_nodes {
            for node in discovered.split_off(params.max_nodes).iter().take(MAX_DROPPED_SAMPLE) {
                dropped.push(self.encode_node(node).0);
            }
        }
        let kept: std::collections::HashSet<GraphNode> = discovered.iter().cloned().collect();

        let nodes = discovered.iter().map(|n| self.node_ref(n.clone(), params.detail)).collect();
        // A `Direction::Both` sweep visits an edge from each endpoint, so a
        // self-call surfaces twice; dedup by `(from, to, kind)` so the two
        // manager edge kinds between the same pair are not collapsed.
        // The rows of one edge are its call sites: the projection keeps one row per site, so
        // grouping is what turns them back into an edge that carries every place.
        let mut seen_edges: std::collections::HashMap<
            (GraphNode, GraphNode, EdgeKind),
            Vec<hir::CallSite>,
        > = std::collections::HashMap::new();
        let mut order: Vec<(GraphNode, GraphNode, EdgeKind)> = Vec::new();
        let mut representative: std::collections::HashMap<
            (GraphNode, GraphNode, EdgeKind),
            &WorkspaceCallEdge,
        > = std::collections::HashMap::new();
        for e in out_edges.iter().filter(|e| {
            (e.from == root || kept.contains(&e.from)) && (e.to == root || kept.contains(&e.to))
        }) {
            let key = (e.from.clone(), e.to.clone(), e.kind);
            match seen_edges.entry(key.clone()) {
                std::collections::hash_map::Entry::Vacant(slot) => {
                    slot.insert(vec![e.call_site]);
                    order.push(key.clone());
                    representative.insert(key, e);
                }
                std::collections::hash_map::Entry::Occupied(mut slot) => {
                    slot.get_mut().push(e.call_site);
                }
            }
        }
        let edges = order
            .iter()
            .map(|key| {
                let edge = representative[key];
                let sites = &seen_edges[key];
                self.edge_ref(edge, &root, sites, params)
            })
            .collect();

        // Distribution + connector-loss over the deduped full neighbourhood (every
        // discovered edge, before the node-cap edge-survival filter), so the counts
        // describe what is connected to the root, not just what survived the cap.
        let mut counted: std::collections::HashSet<(GraphNode, GraphNode, EdgeKind)> =
            std::collections::HashSet::new();
        let mut by_kind: BTreeMap<&'static str, usize> = BTreeMap::new();
        let mut by_provenance: BTreeMap<&'static str, usize> = BTreeMap::new();
        let mut connectors_dropped = false;
        for e in &out_edges {
            if !counted.insert((e.from.clone(), e.to.clone(), e.kind)) {
                continue;
            }
            *by_kind.entry(edge_kind_label(e.kind)).or_default() += 1;
            *by_provenance.entry(provenance_label(e)).or_default() += 1;
            let survives = (e.from == root || kept.contains(&e.from))
                && (e.to == root || kept.contains(&e.to));
            if !survives {
                connectors_dropped = true;
            }
        }

        let returned = discovered.len();
        let confidence = (!by_provenance.is_empty()).then(|| confidence_label(&by_provenance));
        Ok(NeighborsResult {
            root: self.node_ref(root, params.detail),
            nodes,
            edges,
            total,
            returned,
            dropped_count: total - returned,
            dropped,
            by_kind,
            by_provenance,
            confidence,
            connectors_dropped,
            out_total,
            in_total,
        })
    }

    fn directed_edges(&self, node: &GraphNode, dir: Direction) -> Vec<&WorkspaceCallEdge> {
        match dir {
            Direction::Out => self.graph.callees(node).iter().collect(),
            Direction::In => self.graph.callers(node).iter().collect(),
            Direction::Both => {
                self.graph.callees(node).iter().chain(self.graph.callers(node).iter()).collect()
            }
        }
    }

    fn provenance_allowed(&self, edge: &WorkspaceCallEdge, filter: &[String]) -> bool {
        filter.is_empty() || filter.iter().any(|p| p == provenance_label(edge))
    }

    /// Project an edge for the agent. An endpoint equal to `root` is omitted
    /// (`None`), since the neighbours response already carries the root node once.
    ///
    /// `sites` are the call sites of every row this edge was grouped from, in projection
    /// order. This path holds no root table, so a recorded site can never be addressed here
    /// — it names `roots_unavailable`, exactly as the artefact-backed projection does when it
    /// serves without one. That agreement is what the parity gate compares.
    fn edge_ref(
        &self,
        edge: &WorkspaceCallEdge,
        root: &GraphNode,
        sites: &[hir::CallSite],
        params: &NeighborsParams<'_>,
    ) -> EdgeRef {
        let endpoint = |n: &GraphNode| (n != root).then(|| self.encode_node(n).0);
        let call_sites_unavailable =
            params.call_sites.then(|| call_site_absence_reason(sites).unwrap_or(ROOTS_UNAVAILABLE));
        EdgeRef {
            from: endpoint(&edge.from),
            to: endpoint(&edge.to),
            kind: edge_kind_label(edge.kind),
            provenance: provenance_label(edge),
            call_sites: None,
            call_sites_total: None,
            call_sites_truncated: false,
            call_sites_unavailable,
            crosses_client_to_server: edge.crosses_client_to_server,
        }
    }
}

/// The workspace root a path-fallback rel is stripped against, in the spelling the row
/// encoder uses.
///
/// A rel is only worth minting if it names the node the graph actually holds, and the encoder
/// derives its rel one way: the WALK's spelling of the file — every link resolved — stripped
/// against this root. So this type answers that one question, and the root it keeps is the
/// resolved one for that reason.
///
/// Resolving belongs HERE and not inside [`workspace_rel_path`], which is deliberately a
/// string operation so a caller reproduces the encoder's exact rel rather than the real path.
/// What was wrong was never the strip; it was the spellings handed to it.
///
/// **The base belongs to a GENERATION, not to the current disk.** A workspace reached through
/// a link names one directory while that generation's files were walked and possibly another
/// by the time an answer about them is rendered; a base read at answer time would then strip
/// the ids of one tree against the root of the next and mint none. So it is read off the disk
/// exactly once — when the generation that walked those files is built, by
/// [`StripRoot::resolve`] — and every consumer of that generation's paths carries it forward
/// with [`StripRoot::pinned`] instead of resolving again. Resolving once is also what keeps a
/// `canonicalize` of the same root off every node.
///
/// A root that cannot be resolved at all — deleted, or never on disk, as a stated root in a
/// test is — keeps its declared spelling, so such a caller strips exactly as it always did.
#[derive(Clone, Debug)]
pub struct StripRoot {
    /// The declared spelling only when the disk could not improve on it.
    resolved: PathBuf,
}

impl StripRoot {
    /// Read the declared root through the disk, at the moment a generation is built.
    ///
    /// The one call that may touch the filesystem for a root, and only whoever establishes a
    /// file universe may make it: the value it returns is what the universe's ids mean. An
    /// answer ABOUT that universe takes the base it produced, through [`Self::pinned`].
    pub fn resolve(root: &Path) -> StripRoot {
        StripRoot { resolved: std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf()) }
    }

    /// The base a generation already resolved, carried to whoever decodes its ids.
    pub fn pinned(resolved: &Path) -> StripRoot {
        StripRoot { resolved: resolved.to_path_buf() }
    }

    /// The rel the encoder would have minted for `abs`, whichever spelling of the file `abs` is.
    ///
    /// The disk is asked FIRST, because the string strip cannot tell a wrong placement from a
    /// right one. A link INSIDE the root — what a `source.root` that is itself a link produces
    /// — leaves the declared spelling lexically under the root, so stripping
    /// `ws/src/Documents/…` against `ws` yields `src/…` where the walk recorded `actual/…`: a
    /// well-formed id under a directory the graph has no node beneath, which is worse than no
    /// id at all. That the strip placed a path is therefore no evidence it placed it where the
    /// encoder did, and only the filesystem can say two spellings are one file.
    ///
    /// The strip remains for the paths the disk cannot answer for — a deleted file, or a root
    /// that was never on disk, as a stated root in a test is. There it strips exactly as it
    /// always did.
    ///
    /// What is asked of the disk here is which FILE `abs` names, never which directory the
    /// root names: the base was fixed when the generation was built and is not re-read. So the
    /// generation's own spelling of a file — the walk's, links already resolved — mints the
    /// generation's own rel for as long as that generation serves, whether or not the workspace
    /// has been respelled since. Only a caller handing a spelling of its own (a declared path
    /// through a link) still depends on where that link points now, which is what naming a file
    /// through a link means.
    fn rel(&self, abs: &str) -> Option<String> {
        match std::fs::canonicalize(abs) {
            Ok(canonical) => workspace_rel_path(canonical.to_str()?, &self.resolved),
            Err(_) => workspace_rel_path(abs, &self.resolved),
        }
    }

    /// The single spelling to hand a consumer that takes one — the row encoder, whose strip
    /// lives in another crate and stays a plain string operation.
    fn resolved(&self) -> &Path {
        &self.resolved
    }
}

/// Derive the durable method id for `method_name` in the module at `path`,
/// without a database. Returns `None` when `path` is not an indexable user
/// module (forms, commands, non-module files). Best-effort: the id is not
/// verified to resolve, but it round-trips through [`Analysis::graph_node`] when
/// the method exists. Used to bridge code-search hits into the graph.
pub fn method_id_for_path(path: &str, method_name: &str) -> Option<String> {
    let key = module_key_for_path(path)?;
    Some(format!("method/{}/{method_name}", encode_scope(&key)))
}

/// Workspace-relative form of `abs` under `root`, matching the id encoder's rel
/// ([`GraphRowEncoder::rel_path`]): `\` → `/`, leading `/` trimmed. Returns `None`
/// when `abs` is not under `root`. STRING-level strip (no filesystem canonicalization)
/// so a caller reproduces the encoder's exact rel rather than the real path — but with a
/// component-boundary check so a sibling like `/ws/project` is not mistaken for being
/// under `/ws/proj` (which would mint a garbled, non-resolving rel).
pub(crate) fn workspace_rel_path(abs: &str, root: &Path) -> Option<String> {
    let root_str = root.to_str()?.replace('\\', "/");
    let abs = abs.replace('\\', "/");
    let root_str = root_str.trim_end_matches('/');
    let stripped = abs.strip_prefix(root_str)?;
    // `abs` must be `root` itself or continue at a path boundary (`root/<rel>`), never a
    // longer-named sibling that merely shares the prefix string.
    if !stripped.is_empty() && !stripped.starts_with('/') {
        return None;
    }
    let rel = stripped.trim_start_matches('/').to_string();
    if rel.is_empty() {
        return None;
    }
    Some(rel)
}

/// Durable id for `method_name` at `path`, module-keyed when possible, otherwise the
/// `method/file/<rel>::<name>` path-fallback the graph encoder also mints (see
/// [`GraphRowEncoder::encode_method`]) so form/command/file-module methods stay addressable.
///
/// `workspace_root` is the root the graph was built against; it is needed only to strip an
/// absolute `path` down to the encoder's rel. A relative `path` (already root-relative, as
/// produced by the search overlay) is used directly and resolves even without a root. Returns
/// `None` when neither form yields a resolvable id (e.g. an absolute path with no root, or a
/// path not under the root) — a wrong, non-resolving id is worse than no decoration.
///
/// The root arrives ALREADY RESOLVED, as a [`StripRoot`], and it is the base of the GENERATION
/// whose paths `path` came from — the walk that established those ids, not the disk as it
/// stands now. Taking it rather than deriving it is what keeps the two halves of one id from
/// coming from two states of the tree, and it also keeps a `canonicalize` of the same root off
/// every one of the nodes, hits and findings this is called for.
///
/// Distinct from [`method_id_for_path`], which stays module-keyed-only for the graph-enriched
/// embedding path that deliberately does not enrich path-fallback methods.
pub fn method_graph_id(
    path: &str,
    method_name: &str,
    workspace_root: Option<&StripRoot>,
) -> Option<String> {
    if let Some(key) = module_key_for_path(path) {
        return Some(format!("method/{}/{method_name}", encode_scope(&key)));
    }
    let rel = if Path::new(path).is_absolute() {
        workspace_root?.rel(path)?
    } else {
        path.replace('\\', "/").trim_start_matches('/').to_string()
    };
    if rel.is_empty() {
        return None;
    }
    Some(format!("method/file/{rel}::{method_name}"))
}

/// The durable graph id of a method declared in `file`, for every caller that holds a file
/// and a name.
///
/// One computation, deliberately. There used to be three — a card by name, a card by
/// position, and a reference anchor — and only the first of them minted the
/// `method/file/<rel>::<name>` fallback, so a form event handler had an id when asked for by
/// name and none when reached by position. An id that appears and disappears with the way a
/// client spelled its request is worse than none: it makes two tools disagree about a symbol
/// they both resolved.
///
/// Which is also why the base arrives as a [`StripRoot`] rather than a path to resolve here: a
/// card and a finding about one method are answered by different tools over the same generation,
/// and a base each of them read off the disk for itself would let them disagree exactly when the
/// workspace is spelled through a link that has moved.
pub fn graph_id_of_method(
    db: &crate::RootDatabaseImpl,
    file: vfs::FileId,
    method_name: &str,
    workspace_root: Option<&StripRoot>,
) -> Option<String> {
    // The file's OWN root, not the source root assumed by name: a body may be registered
    // under a root other than the configuration's, and asking the wrong root yields no path
    // at all — which would silently drop the id instead of encoding it.
    let path = crate::name_lookup::workspace_path(db, file)?;
    method_graph_id(&path, method_name, workspace_root)
}

/// The durable `module/<scope>` id that owns a `method/<scope>/<name>` (or
/// `method/file/<rel>::<name>`) id. The inverse of [`method_id_range`]'s lower bound:
/// it strips the trailing method segment, keeping the same `::`/`/` member grammar.
/// `None` for any id that is not a method id.
pub fn module_id_of_method(method_id: &str) -> Option<String> {
    let rest = method_id.strip_prefix("method/")?;
    if let Some(rel) = rest.strip_prefix("file/") {
        // `file/<rel>::<name>` — the file module is everything before the `::`. The `::` is
        // required: a `file/<rel>` with no member separator is not a method id.
        let (rel, _name) = rel.split_once("::")?;
        if rel.is_empty() {
            return None;
        }
        return Some(format!("module/file/{rel}"));
    }
    // `<scope>/<name>` — the module scope is everything before the last `/`.
    let scope = rest.rsplit_once('/')?.0;
    if scope.is_empty() {
        return None;
    }
    Some(format!("module/{scope}"))
}

/// The trailing name segment of a durable id: the part after the last `::` (a file
/// module's member separator) or, failing that, the last `/`. Used by
/// [`rank_resolve_candidates`] to match a bare method/object name against full ids.
pub fn resolve_name_segment(id: &str) -> &str {
    if let Some(pos) = id.rfind("::") {
        return &id[pos + 2..];
    }
    id.rsplit_once('/').map_or(id, |(_, tail)| tail)
}

/// A single near-miss candidate for [`ResolveResult`]: a durable id that an agent's
/// `query` could have meant, with the match strength that surfaced it.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ResolveCandidate {
    pub id: String,
    pub kind: &'static str,
    /// How `query` matched: `exact` | `case_insensitive` | `name` | `substring`
    /// (strongest first).
    #[serde(rename = "match")]
    pub match_kind: &'static str,
}

/// The result of `graph(action=resolve)`: the candidate durable ids an imprecise
/// `query` (wrong casing, bare name, or partial id) could resolve to, so an agent can
/// recover from a `not_found` without guessing.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ResolveResult {
    pub query: String,
    pub candidates: Vec<ResolveCandidate>,
    /// Total distinct candidates matched before the `limit` cap. `candidates` is the
    /// top-`limit` slice; `total` lets an agent see that a frequent name has many more
    /// matches than shown, instead of treating the capped list as exhaustive.
    pub total: usize,
    /// `true` when the cap dropped candidates (`total` exceeds `candidates.len()`), so the
    /// shown list is a partial view — refine the query or use `search_code`.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

impl ResolveResult {
    /// Assemble a result from the ranker output (`candidates` already capped, `total` the
    /// pre-cap match count), deriving `truncated` from the two.
    pub fn new(query: &str, candidates: Vec<ResolveCandidate>, total: usize) -> Self {
        let truncated = total > candidates.len();
        Self { query: query.to_string(), candidates, total, truncated }
    }
}

/// Rank durable ids against an imprecise `query`, strongest match first then id-ascending,
/// capped at `limit`. Both serve paths (in-memory [`Analysis::graph_resolve`] and SQLite)
/// feed their full `(id, kind)` node set through this one ranker, so the candidate lists
/// stay byte-identical regardless of scan order. An empty `query` matches nothing.
///
/// Returns the top-`limit` candidates together with the total number of distinct matches
/// before the cap, so a frequent name (thousands of `ПриСозданииНаСервере`) is not silently
/// presented as if the top 20 were the whole set.
pub fn rank_resolve_candidates(
    nodes: impl Iterator<Item = (String, &'static str)>,
    query: &str,
    limit: usize,
) -> (Vec<ResolveCandidate>, usize) {
    if query.is_empty() {
        return (Vec::new(), 0);
    }
    let q_lower = query.fold_lower();
    let mut ranked: Vec<(u8, ResolveCandidate)> = nodes
        .filter_map(|(id, kind)| {
            let (rank, match_kind) = if id == query {
                (0, "exact")
            } else if id.fold_lower() == q_lower {
                (1, "case_insensitive")
            } else if resolve_name_segment(&id).fold_lower() == q_lower {
                (2, "name")
            } else if id.fold_lower().contains(&q_lower) {
                (3, "substring")
            } else {
                return None;
            };
            Some((rank, ResolveCandidate { id, kind, match_kind }))
        })
        .collect();
    let total = ranked.len();
    ranked.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.id.cmp(&b.1.id)));
    ranked.truncate(limit);
    (ranked.into_iter().map(|(_, c)| c).collect(), total)
}

/// The agent-facing kind label for a [`GraphNode`], matching the SQLite serve path's
/// `node_kind`. A tabular-section attribute is reported as a plain `attribute`.
fn graph_node_kind(node: &GraphNode) -> &'static str {
    match node {
        GraphNode::Method(_) => "method",
        GraphNode::ModuleCode(_) => "module",
        GraphNode::Mdo { .. } => "mdo",
        GraphNode::Attribute { .. } | GraphNode::TabularSectionAttribute { .. } => "attribute",
        GraphNode::TabularSection { .. } => "tabular_section",
        GraphNode::Form { .. } => "form",
        GraphNode::FormItem { .. } => "form_item",
        GraphNode::FormAttribute { .. } => "form_attribute",
    }
}

/// The parsed shape of a durable graph id, independent of any database. Drives
/// both the in-memory [`Analysis::graph_node`] resolver and the SQLite serving
/// path, so the id grammar — and its [`GraphError::BadId`] rules — live in one
/// place. Resolving a kind to an actual node needs the graph; that is the caller's
/// job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GraphIdKind {
    /// `method/<scope>/<name>` — a method addressed by module scope.
    Method { scope: ModuleKey, name: String },
    /// `module/<scope>` — a module body addressed by scope.
    Module { scope: ModuleKey },
    /// `method/file/<rel>::<name>` — a method in a non-standard file path.
    MethodFile { rel: String, name: String },
    /// `module/file/<rel>` — a module body in a non-standard file path.
    ModuleFile { rel: String },
    /// `mdo/<MdoEnglish>/<Object>`.
    Mdo { mdo_type: MdoType, object: String },
    /// `attribute/<MdoEnglish>/<Object>/<Attr>`.
    Attribute { mdo_type: MdoType, object: String, attr: String },
    /// `form/<MdoEnglish>/<Object>/<Form>` or `form/common/<Form>`.
    Form { owner: Option<(MdoType, String)>, form_name: String },
    /// `form_item/<MdoEnglish>/<Object>/<Form>/<Item>` or
    /// `form_item/common/<Form>/<Item>`.
    FormItem { owner: Option<(MdoType, String)>, form_name: String, item_name: String },
    /// `form_attr/<MdoEnglish>/<Object>/<Form>/<Attr>` or
    /// `form_attr/common/<Form>/<Attr>`.
    FormAttribute { owner: Option<(MdoType, String)>, form_name: String, attr_name: String },
    /// `tabular_section/<MdoEnglish>/<Object>/<Section>`.
    TabularSection { mdo_type: MdoType, object: String, section: String },
    /// `ts_attr/<MdoEnglish>/<Object>/<Section>/<Attr>`.
    TabularSectionAttribute { mdo_type: MdoType, object: String, section: String, attr: String },
}

/// A form's owner: `None` for a common form, `Some((type, object))` for an
/// object-owned form.
type FormOwner = Option<(MdoType, String)>;

/// Split a form id's segments (after the `form/`/`form_item/` prefix) into the owner
/// — `None` for a `common/…` form, `Some((type, object))` otherwise — and the
/// trailing segments (form name, then item name for a form item).
fn split_form_owner<'a>(parts: &'a [&'a str]) -> Option<(FormOwner, &'a [&'a str])> {
    match parts.first().copied()? {
        "common" => Some((None, &parts[1..])),
        eng => {
            let mdo_type = eng.parse().ok()?;
            let object = (*parts.get(1)?).to_string();
            Some((Some((mdo_type, object)), &parts[2..]))
        }
    }
}

/// Parse a durable graph id into its [`GraphIdKind`] without touching a database,
/// returning [`GraphError::BadId`] for a malformed id (unknown prefix, missing
/// `::<method>`, unknown metadata type, malformed scope).
pub fn classify_graph_id(id: &str) -> Result<GraphIdKind, GraphError> {
    let bad = |reason: &str| GraphError::BadId { id: id.to_string(), reason: reason.to_string() };

    if let Some(rest) = id.strip_prefix("method/file/") {
        let (rel, name) = rest
            .rsplit_once("::")
            .ok_or_else(|| bad("path method id must contain '::<method>'"))?;
        return Ok(GraphIdKind::MethodFile { rel: rel.to_string(), name: name.to_string() });
    }
    if let Some(rel) = id.strip_prefix("module/file/") {
        return Ok(GraphIdKind::ModuleFile { rel: rel.to_string() });
    }
    if let Some(rest) = id.strip_prefix("mdo/") {
        let (mdo_eng, object) =
            rest.split_once('/').ok_or_else(|| bad("mdo id must be 'mdo/<MdoType>/<Object>'"))?;
        let mdo_type =
            mdo_eng.parse().map_err(|_| bad(&format!("unknown metadata type '{mdo_eng}'")))?;
        return Ok(GraphIdKind::Mdo { mdo_type, object: object.to_string() });
    }
    if let Some(rest) = id.strip_prefix("attribute/") {
        let mut parts = rest.splitn(3, '/');
        let structure = || bad("attribute id must be 'attribute/<MdoType>/<Object>/<Attr>'");
        let mdo_eng = parts.next().ok_or_else(structure)?;
        let object = parts.next().ok_or_else(structure)?;
        let attr = parts.next().ok_or_else(structure)?;
        let mdo_type =
            mdo_eng.parse().map_err(|_| bad(&format!("unknown metadata type '{mdo_eng}'")))?;
        return Ok(GraphIdKind::Attribute {
            mdo_type,
            object: object.to_string(),
            attr: attr.to_string(),
        });
    }
    // `ts_attr/` before `tabular_section/`: distinct prefixes, but keep the more
    // specific column id alongside its section id for readability.
    if let Some(rest) = id.strip_prefix("ts_attr/") {
        let mut parts = rest.splitn(4, '/');
        let structure =
            || bad("ts attribute id must be 'ts_attr/<MdoType>/<Object>/<Section>/<Attr>'");
        let mdo_eng = parts.next().ok_or_else(structure)?;
        let object = parts.next().ok_or_else(structure)?;
        let section = parts.next().ok_or_else(structure)?;
        let attr = parts.next().ok_or_else(structure)?;
        let mdo_type =
            mdo_eng.parse().map_err(|_| bad(&format!("unknown metadata type '{mdo_eng}'")))?;
        return Ok(GraphIdKind::TabularSectionAttribute {
            mdo_type,
            object: object.to_string(),
            section: section.to_string(),
            attr: attr.to_string(),
        });
    }
    if let Some(rest) = id.strip_prefix("tabular_section/") {
        let mut parts = rest.splitn(3, '/');
        let structure =
            || bad("tabular section id must be 'tabular_section/<MdoType>/<Object>/<Section>'");
        let mdo_eng = parts.next().ok_or_else(structure)?;
        let object = parts.next().ok_or_else(structure)?;
        let section = parts.next().ok_or_else(structure)?;
        let mdo_type =
            mdo_eng.parse().map_err(|_| bad(&format!("unknown metadata type '{mdo_eng}'")))?;
        return Ok(GraphIdKind::TabularSection {
            mdo_type,
            object: object.to_string(),
            section: section.to_string(),
        });
    }
    if let Some(rest) = id.strip_prefix("form_attr/") {
        let parts: Vec<&str> = rest.split('/').collect();
        let structure = || {
            bad("form attribute id must be 'form_attr/<scope>/<Form>/<Attr>' (scope = <MdoType>/<Object> or 'common')")
        };
        let (owner, tail) = split_form_owner(&parts).ok_or_else(structure)?;
        let [form_name, attr_name] = tail else { return Err(structure()) };
        return Ok(GraphIdKind::FormAttribute {
            owner,
            form_name: form_name.to_string(),
            attr_name: attr_name.to_string(),
        });
    }
    // `form_item/` must be tested before `form/` (the latter is a prefix of the former).
    if let Some(rest) = id.strip_prefix("form_item/") {
        let parts: Vec<&str> = rest.split('/').collect();
        let structure = || {
            bad("form item id must be 'form_item/<scope>/<Form>/<Item>' (scope = <MdoType>/<Object> or 'common')")
        };
        let (owner, tail) = split_form_owner(&parts).ok_or_else(structure)?;
        let [form_name, item_name] = tail else { return Err(structure()) };
        return Ok(GraphIdKind::FormItem {
            owner,
            form_name: form_name.to_string(),
            item_name: item_name.to_string(),
        });
    }
    if let Some(rest) = id.strip_prefix("form/") {
        let parts: Vec<&str> = rest.split('/').collect();
        let structure = || {
            bad("form id must be 'form/<scope>/<Form>' (scope = <MdoType>/<Object> or 'common')")
        };
        let (owner, tail) = split_form_owner(&parts).ok_or_else(structure)?;
        let [form_name] = tail else { return Err(structure()) };
        return Ok(GraphIdKind::Form { owner, form_name: form_name.to_string() });
    }

    let parts: Vec<&str> = id.split('/').collect();
    let (is_method, rest) = match parts.first().copied() {
        Some("method") => (true, &parts[1..]),
        Some("module") => (false, &parts[1..]),
        _ => {
            return Err(bad(
                "unknown id prefix; expected one of: method/, module/, mdo/, attribute/, \
                 ts_attr/, tabular_section/, form/, form_attr/, form_item/",
            ))
        }
    };
    let (scope, method) = decode_scope(rest, is_method).ok_or_else(|| bad("malformed scope"))?;
    Ok(match method {
        Some(name) => GraphIdKind::Method { scope, name },
        None => GraphIdKind::Module { scope },
    })
}

fn decode_scope(rest: &[&str], is_method: bool) -> Option<(ModuleKey, Option<String>)> {
    // For a method id the trailing segment is the method name; module ids end at
    // the scope.
    let (scope, method) = if is_method {
        let (method, scope) = rest.split_last()?;
        (scope, Some((*method).to_string()))
    } else {
        (rest, None)
    };

    let key = match scope {
        ["common", name] => ModuleKey::Common { name: name.to_string() },
        ["manager", mdo, name] => {
            ModuleKey::Manager { mdo_type: parse_mdo(mdo)?, name: name.to_string() }
        }
        ["object", mdo, name] => {
            ModuleKey::Object { mdo_type: parse_mdo(mdo)?, name: name.to_string() }
        }
        ["recordset", mdo, name] => {
            ModuleKey::RecordSet { mdo_type: parse_mdo(mdo)?, name: name.to_string() }
        }
        _ => return None,
    };
    Some((key, method))
}

fn parse_mdo(s: &str) -> Option<MdoType> {
    // Bilingual `MdoType: FromStr` accepts the English folder names we encode.
    s.parse().ok()
}

/// The full declaration header — from the keyword line through the closing `)` /
/// export keyword — with wrapped parameter lines collapsed to one. Char-boundary
/// safe; `None` if the offsets fall outside the (current) file text.
fn signature_line(
    db: &RootDatabaseImpl,
    file_id: FileId,
    name_offset: syntax::TextSize,
    sig_end: syntax::TextSize,
) -> Option<String> {
    let text = db.file_text(file_id);
    let name = (u32::from(name_offset) as usize).min(text.len());
    let end = (u32::from(sig_end) as usize).min(text.len());
    if name > end || !text.is_char_boundary(name) || !text.is_char_boundary(end) {
        return None;
    }
    let start = text[..name].rfind('\n').map_or(0, |i| i + 1);
    Some(text.get(start..end)?.split_whitespace().collect::<Vec<_>>().join(" "))
}

fn dispatch_labels(d: MethodDispatch) -> Vec<&'static str> {
    let mut labels = Vec::new();
    if d.can_run_on_client {
        labels.push("client");
    }
    if d.can_run_on_server {
        labels.push("server");
    }
    labels
}

/// Reduce a provenance histogram to a one-glance trust label for the *shown* edges:
/// `resolved_only` when every edge is a direct static resolution, else
/// `contains_inferred` (at least one edge is metadata-inferred or string-dispatched —
/// a concrete target, but lower trust than a direct call). Edges that could not be
/// resolved are dropped from the graph entirely (see `project_module_call_edges`), so
/// this describes the trust of the edges that are shown, not graph recall. Any
/// non-`resolved` label therefore counts as `contains_inferred`. Caller passes only
/// non-empty maps. Shared with the SQLite serve path so both graphs report identically.
pub fn confidence_label(by_provenance: &BTreeMap<&'static str, usize>) -> &'static str {
    let total: usize = by_provenance.values().copied().sum();
    let resolved = by_provenance.get("resolved").copied().unwrap_or(0);
    if resolved == total {
        "resolved_only"
    } else {
        "contains_inferred"
    }
}

fn provenance_label(edge: &WorkspaceCallEdge) -> &'static str {
    use hir::EdgeProvenance::*;
    match edge.provenance {
        Resolved => "resolved",
        Inferred => "inferred",
        VisibilityBlocked => "visibility_blocked",
        Unresolved => "unresolved",
        StringResolved => "string_resolved",
    }
}

fn edge_kind_label(kind: EdgeKind) -> &'static str {
    // Both direct-call variants project to the same agent-facing edge kind; the
    // local-vs-qualified distinction is an internal resolution detail.
    match kind {
        EdgeKind::DirectLocal | EdgeKind::DirectQualifiedModule => "call",
        EdgeKind::ManagerCreates => "manager_creates",
        EdgeKind::ManagerAccess => "manager_access",
        EdgeKind::QueryRef => "query_ref",
        EdgeKind::Contains => "contains",
        EdgeKind::DataBinding => "data_binding",
        EdgeKind::NotifyRef => "notify_ref",
        EdgeKind::IdleHandler => "idle_handler",
        EdgeKind::EventSubscriptionRef => "event_subscription",
        EdgeKind::RegisterMovement => "register_movement",
        EdgeKind::SubsystemMembership => "subsystem_membership",
        EdgeKind::RoleReference => "role_reference",
        EdgeKind::RegisterRecords => "register_records",
        EdgeKind::RegisterRecordSet => "register_record_set",
    }
}

/// Whether an edge of `kind` passes an `edge_kinds` filter: an empty filter admits all,
/// otherwise the edge's agent-facing label must be listed. Shared shape with the SQLite
/// serve path, which filters on the stored kind string.
pub(crate) fn edge_kind_allowed(kind: EdgeKind, filter: &[String]) -> bool {
    filter.is_empty() || filter.iter().any(|k| k == edge_kind_label(kind))
}

/// Whether a stored form node's owner (`node`) equals a parsed-id owner (`query`),
/// comparing the object name case-insensitively. `None` (common form) matches `None`.
fn form_owner_matches(node: Option<(MdoType, &str)>, query: &Option<(MdoType, String)>) -> bool {
    match (node, query) {
        (None, None) => true,
        (Some((nt, nn)), Some((qt, qn))) => nt == *qt && nn.fold_lower() == qn.fold_lower(),
        _ => false,
    }
}

fn basename(path: &str) -> Option<&str> {
    path.rsplit('/').next()
}

/// Truncate `src` to at most `max_chars` bytes on a char boundary, returning the
/// (possibly shortened) text and whether it was cut.
fn clamp_source(src: String, max_chars: usize) -> (String, bool) {
    if src.len() <= max_chars {
        return (src, false);
    }
    let mut end = max_chars;
    while end > 0 && !src.is_char_boundary(end) {
        end -= 1;
    }
    (src[..end].to_string(), true)
}

#[cfg(test)]
mod ticker_tests {
    use super::{estimate_graph_build_eta_seconds, GraphBuildTicker};
    use std::time::Duration;

    #[test]
    fn eta_requires_completed_measured_intervals_and_never_rounds_to_zero() {
        assert_eq!(estimate_graph_build_eta_seconds(10, 0, Duration::from_secs(4)), None);
        assert_eq!(estimate_graph_build_eta_seconds(10, 1, Duration::ZERO), None);
        assert_eq!(estimate_graph_build_eta_seconds(0, 0, Duration::from_secs(4)), None);
        assert_eq!(estimate_graph_build_eta_seconds(10, 10, Duration::from_secs(4)), None);
        assert_eq!(estimate_graph_build_eta_seconds(10, 3, Duration::from_millis(2500)), Some(6));
        assert_eq!(estimate_graph_build_eta_seconds(2, 1, Duration::from_nanos(1)), Some(1));
    }

    #[test]
    fn note_stamps_position_and_resets_stall_clock() {
        let ticker = GraphBuildTicker::default();
        std::thread::sleep(std::time::Duration::from_millis(30));
        ticker.note("call_edges", 4, 59, "src/cfe/Ext/CommonModules/М/Ext/Module.bsl");
        assert!(ticker.ms_since_progress() < 25);
        let position = ticker.position();
        assert!(position.contains("call_edges batch 5/59"), "{position}");
        assert!(position.contains("Module.bsl"), "{position}");
    }

    #[test]
    fn phase_without_batches_keeps_bare_label() {
        let ticker = GraphBuildTicker::default();
        ticker.note("catalog_edges", 0, 0, "");
        assert_eq!(ticker.position(), "catalog_edges");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Analysis, RootDatabaseImpl};
    use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
    use vfs::{file_set::FileSet, FileId, VfsPath};

    pub(super) const ROOT: SourceRootId = SourceRootId(0);

    pub(super) fn workspace(files: &[(&str, &str)]) -> Analysis {
        let mut db = RootDatabaseImpl::new();
        let mut file_set = FileSet::new();
        for (i, (path, _)) in files.iter().enumerate() {
            file_set.insert(FileId(i as u32), VfsPath::new(*path));
        }
        db.set_source_root(ROOT, SourceRoot::new_local(file_set));
        for (i, (_, text)) in files.iter().enumerate() {
            let fid = FileId(i as u32);
            db.set_file_source_root(fid, ROOT);
            db.set_file_text(fid, text);
        }
        Analysis::from_database(db)
    }

    /// Two common modules: a client-dispatched caller invoking a server-only
    /// exported function in another module.
    fn client_server_workspace() -> Analysis {
        workspace(&[
            (
                "/src/CommonModules/Клиент/Ext/Module.bsl",
                "&НаКлиенте\n\
                 Процедура Главная() Экспорт\n\
                 Сервер.Считать();\n\
                 КонецПроцедуры",
            ),
            (
                "/src/CommonModules/Сервер/Ext/Module.bsl",
                "&НаСервере\n\
                 Функция Считать() Экспорт КонецФункции",
            ),
        ])
    }

    #[test]
    fn node_id_round_trips_for_common_module_method() {
        let a = client_server_workspace();
        let result = a
            .graph_node(ROOT, None, "method/common/Сервер/Считать", GraphDetail::Signatures)
            .expect("server method resolves by durable id");
        let node = result.node;
        assert_eq!(node.id, "method/common/Сервер/Считать");
        assert_eq!(node.kind, "method");
        assert_eq!(node.name, "Считать");
        assert_eq!(node.qualified, None, "code nodes do not serve qualified");
        assert_eq!(node.is_export, Some(true));
        assert_eq!(node.dispatch, vec!["server"]);
        // Full header through the export keyword, not just the name.
        assert_eq!(node.signature.as_deref(), Some("Функция Считать() Экспорт"));
        assert!(node.addressable);
    }

    #[test]
    fn signature_collapses_a_wrapped_parameter_list() {
        let a = workspace(&[(
            "/src/CommonModules/Утилиты/Ext/Module.bsl",
            "Функция Сложить(Знач Первое,\n\
             \tВторое,\n\
             \tТретье = 0) Экспорт\n\
             Возврат Первое;\n\
             КонецФункции",
        )]);
        let node = a
            .graph_node(ROOT, None, "method/common/Утилиты/Сложить", GraphDetail::Signatures)
            .expect("method resolves")
            .node;
        // The wrapped parameter lines collapse to one, ending at the export keyword.
        assert_eq!(
            node.signature.as_deref(),
            Some("Функция Сложить(Знач Первое, Второе, Третье = 0) Экспорт")
        );
    }

    #[test]
    fn unknown_id_reports_not_found() {
        let a = client_server_workspace();
        let err = a
            .graph_node(ROOT, None, "method/common/Сервер/НетТакого", GraphDetail::Names)
            .unwrap_err();
        assert_eq!(
            err,
            GraphError::NotFound {
                id: "method/common/Сервер/НетТакого".to_string()
            }
        );
    }

    #[test]
    fn malformed_id_reports_bad_id() {
        let a = client_server_workspace();
        let err = a.graph_node(ROOT, None, "Сервер.Считать", GraphDetail::Names).unwrap_err();
        assert!(matches!(err, GraphError::BadId { .. }));
    }

    #[test]
    fn classify_graph_id_grammar() {
        use super::{classify_graph_id, GraphIdKind};

        // Well-formed ids of every kind.
        assert!(matches!(
            classify_graph_id("method/common/Сервер/Считать"),
            Ok(GraphIdKind::Method { name, .. }) if name == "Считать"
        ));
        assert!(matches!(
            classify_graph_id("module/common/Сервер"),
            Ok(GraphIdKind::Module { .. })
        ));
        assert!(matches!(
            classify_graph_id("method/file/src/a.bsl::M"),
            Ok(GraphIdKind::MethodFile { rel, name }) if rel == "src/a.bsl" && name == "M"
        ));
        assert!(matches!(
            classify_graph_id("module/file/src/a.bsl"),
            Ok(GraphIdKind::ModuleFile { rel }) if rel == "src/a.bsl"
        ));
        assert!(matches!(
            classify_graph_id("mdo/Catalog/Контрагенты"),
            Ok(GraphIdKind::Mdo { object, .. }) if object == "Контрагенты"
        ));
        // A localized type spelling parses to the same variant.
        assert!(matches!(
            classify_graph_id("mdo/Справочник/Контрагенты"),
            Ok(GraphIdKind::Mdo { .. })
        ));
        assert!(matches!(
            classify_graph_id("attribute/Catalog/Контрагенты/Наименование"),
            Ok(GraphIdKind::Attribute { object, attr, .. })
                if object == "Контрагенты" && attr == "Наименование"
        ));
        // Forms: object-owned and common, plus their items.
        assert!(matches!(
            classify_graph_id("form/Catalog/Контрагенты/ФормаЭлемента"),
            Ok(GraphIdKind::Form { owner: Some((_, object)), form_name })
                if object == "Контрагенты" && form_name == "ФормаЭлемента"
        ));
        assert!(matches!(
            classify_graph_id("form/common/НастройкиПрограммы"),
            Ok(GraphIdKind::Form { owner: None, form_name }) if form_name == "НастройкиПрограммы"
        ));
        assert!(matches!(
            classify_graph_id("form_item/Catalog/Контрагенты/ФормаЭлемента/ПолеКод"),
            Ok(GraphIdKind::FormItem { owner: Some(_), form_name, item_name })
                if form_name == "ФормаЭлемента" && item_name == "ПолеКод"
        ));
        assert!(matches!(
            classify_graph_id("form_item/common/Ф/Кнопка"),
            Ok(GraphIdKind::FormItem { owner: None, form_name, item_name })
                if form_name == "Ф" && item_name == "Кнопка"
        ));
        // Form attributes: object-owned and common.
        assert!(matches!(
            classify_graph_id("form_attr/Catalog/Контрагенты/ФормаЭлемента/Объект"),
            Ok(GraphIdKind::FormAttribute { owner: Some(_), form_name, attr_name })
                if form_name == "ФормаЭлемента" && attr_name == "Объект"
        ));
        assert!(matches!(
            classify_graph_id("form_attr/common/Ф/Список"),
            Ok(GraphIdKind::FormAttribute { owner: None, form_name, attr_name })
                if form_name == "Ф" && attr_name == "Список"
        ));
        // Tabular sections and their columns.
        assert!(matches!(
            classify_graph_id("tabular_section/Catalog/Контрагенты/Товары"),
            Ok(GraphIdKind::TabularSection { object, section, .. })
                if object == "Контрагенты" && section == "Товары"
        ));
        assert!(matches!(
            classify_graph_id("ts_attr/Catalog/Контрагенты/Товары/Цена"),
            Ok(GraphIdKind::TabularSectionAttribute { object, section, attr, .. })
                if object == "Контрагенты" && section == "Товары" && attr == "Цена"
        ));

        // Malformed ids are BadId.
        for bad in [
            "garbage",
            "Сервер.Считать",
            "method/file/no-method-separator",
            "mdo/onlytype",
            "mdo/NoSuchType/X",
            "attribute/Catalog/OnlyObject",
            "method/bogusscope/M",
            "form/Catalog/OnlyObjectNoForm",
            "form/NoSuchType/X/Ф",
            "form_item/common/OnlyForm",
            "form_attr/common/OnlyForm",
            "form_attr/NoSuchType/X/Ф/А",
            "tabular_section/Catalog/OnlyObject",
            "tabular_section/NoSuchType/X/TС",
            "ts_attr/Catalog/Obj/OnlySection",
            "ts_attr/NoSuchType/X/TС/К",
        ] {
            assert!(
                matches!(classify_graph_id(bad), Err(GraphError::BadId { .. })),
                "{bad} must be BadId"
            );
        }

        // An unknown top-level prefix must not steer the caller toward only
        // method/module — every accepted prefix is listed so the agent learns the
        // full vocabulary.
        let Err(GraphError::BadId { reason, .. }) = classify_graph_id("widget/Catalog/X") else {
            panic!("unknown prefix must be BadId");
        };
        for kind in ["method/", "module/", "mdo/", "attribute/", "form/"] {
            assert!(reason.contains(kind), "reason must mention {kind}; got: {reason}");
        }
    }

    #[test]
    fn neighbors_in_lists_caller_and_flags_client_server_crossing() {
        let a = client_server_workspace();
        let params = NeighborsParams {
            id: "method/common/Сервер/Считать",
            dir: Direction::In,
            depth: 1,
            max_nodes: 50,
            detail: GraphDetail::Names,
            provenance_filter: Vec::new(),
            edge_kind_filter: Vec::new(),
            call_sites: false,
            max_call_sites: 0,
        };
        let res = a.graph_neighbors(ROOT, None, &params).expect("neighbors resolve");
        assert_eq!(res.root.id, "method/common/Сервер/Считать");
        assert!(res.nodes.iter().any(|n| n.id == "method/common/Клиент/Главная"));
        // One caller discovered, none dropped under a generous cap.
        assert_eq!(res.total, 1);
        assert_eq!(res.total, res.nodes.len());
        assert!(res.dropped.is_empty());
        let edge = res
            .edges
            .iter()
            .find(|e| e.to.is_none())
            .expect("an incoming edge to the root, its endpoint elided");
        assert_eq!(edge.from.as_deref(), Some("method/common/Клиент/Главная"));
        assert_eq!(edge.kind, "call");
        assert_eq!(edge.provenance, "resolved");
        assert!(edge.crosses_client_to_server);
    }

    #[test]
    fn neighbors_total_counts_beyond_the_max_nodes_cap() {
        let a = client_server_workspace();
        let params = NeighborsParams {
            id: "method/common/Сервер/Считать",
            dir: Direction::In,
            depth: 1,
            max_nodes: 0,
            detail: GraphDetail::Names,
            provenance_filter: Vec::new(),
            edge_kind_filter: Vec::new(),
            call_sites: false,
            max_call_sites: 0,
        };
        let res = a.graph_neighbors(ROOT, None, &params).expect("neighbors resolve");
        // The cap drops the sole caller, but `total` still reflects the real fan-out.
        assert_eq!(res.total, 1);
        assert!(res.nodes.is_empty());
        assert_eq!(res.dropped, vec!["method/common/Клиент/Главная".to_string()]);
    }

    #[test]
    fn overview_counts_edges_and_ranks_called_method() {
        let a = client_server_workspace();
        let ov = a.graph_overview(ROOT, None, 10);
        assert!(ov.methods >= 2);
        assert_eq!(ov.edges, 1);
        assert_eq!(ov.client_to_server_edges, 1);
        assert_eq!(ov.edge_provenance.get("resolved"), Some(&1));
        assert_eq!(
            ov.top_by_centrality.first().map(|n| n.id.as_str()),
            Some("method/common/Сервер/Считать")
        );
    }

    #[test]
    fn manager_module_method_id_round_trips() {
        let a = workspace(&[
            (
                "/src/CommonModules/Вызыватель/Ext/Module.bsl",
                "Процедура Делать() Экспорт\n\
                 Справочники.Контрагенты.НайтиПоИНН();\n\
                 КонецПроцедуры",
            ),
            (
                "/src/Catalogs/Контрагенты/Ext/ManagerModule.bsl",
                "Функция НайтиПоИНН() Экспорт КонецФункции",
            ),
        ]);
        let id = "method/manager/Catalog/Контрагенты/НайтиПоИНН";
        let node =
            a.graph_node(ROOT, None, id, GraphDetail::Names).expect("manager method resolves");
        assert_eq!(node.node.id, id);
        assert_eq!(node.node.qualified, None, "code nodes do not serve qualified");

        // And the caller reaches it via a resolved edge (literal manager path, the
        // manager module is uniquely determined).
        let params = NeighborsParams {
            id,
            dir: Direction::In,
            depth: 1,
            max_nodes: 50,
            detail: GraphDetail::Names,
            provenance_filter: Vec::new(),
            edge_kind_filter: Vec::new(),
            call_sites: false,
            max_call_sites: 0,
        };
        let res = a.graph_neighbors(ROOT, None, &params).unwrap();
        assert!(res.nodes.iter().any(|n| n.id == "method/common/Вызыватель/Делать"));
        assert!(res.edges.iter().any(|e| e.provenance == "resolved"));
        assert_eq!(
            res.confidence,
            Some("resolved_only"),
            "a literal manager-method neighbourhood is fully resolved"
        );
    }

    #[test]
    fn platform_manager_calls_link_to_mdo_node() {
        // No manager module for Контрагенты, so СоздатьЭлемент/НайтиПоКоду are
        // platform methods that touch the metadata object rather than a user node.
        let a = workspace(&[(
            "/src/CommonModules/Вызыватель/Ext/Module.bsl",
            "Процедура Делать() Экспорт\n\
             Справочники.Контрагенты.СоздатьЭлемент();\n\
             Справочники.Контрагенты.НайтиПоКоду();\n\
             КонецПроцедуры",
        )]);

        let id = "mdo/Catalog/Контрагенты";
        let node = a.graph_node(ROOT, None, id, GraphDetail::Names).expect("mdo node resolves");
        assert_eq!(node.node.id, id);
        assert_eq!(node.node.kind, "mdo");
        assert_eq!(node.node.name, "Контрагенты");
        assert_eq!(node.node.qualified.as_deref(), Some("Справочник.Контрагенты"));
        assert!(node.node.addressable);

        let ov = a.graph_overview(ROOT, None, 10);
        assert_eq!(ov.mdos, 1, "one metadata object node");

        // The Mdo node's callers carry both edge kinds (create + access), deduped
        // by kind rather than collapsed to one.
        let params = NeighborsParams {
            id,
            dir: Direction::In,
            depth: 1,
            max_nodes: 50,
            detail: GraphDetail::Names,
            provenance_filter: Vec::new(),
            edge_kind_filter: Vec::new(),
            call_sites: false,
            max_call_sites: 0,
        };
        let res = a.graph_neighbors(ROOT, None, &params).unwrap();
        assert!(res.nodes.iter().any(|n| n.id == "method/common/Вызыватель/Делать"));
        assert!(res
            .edges
            .iter()
            .any(|e| e.kind == "manager_creates" && e.provenance == "inferred"));
        assert!(res.edges.iter().any(|e| e.kind == "manager_access"));
        assert_eq!(
            res.confidence,
            Some("contains_inferred"),
            "platform manager touches resolve to Mdo nodes → inferred, not fully resolved"
        );
    }

    #[test]
    fn graph_context_renders_calls_signature_dispatch() {
        let a = client_server_workspace();
        // Клиент.Главная is &НаКлиенте and calls the server-only Сервер.Считать.
        let ctx = a.graph_context_for_method(FileId(0), "Главная").expect("method resolves");
        assert_eq!(ctx.dispatch, vec!["client"]);
        assert_eq!(ctx.signature.as_deref(), Some("Процедура Главная() Экспорт"));
        assert_eq!(ctx.calls, vec!["Считать".to_string()]);
        assert!(ctx.reads.is_empty());

        let rendered = ctx.render();
        assert!(rendered.contains("Dispatch: client | клиент"), "{rendered}");
        assert!(rendered.contains("Signature: Процедура Главная() Экспорт"), "{rendered}");
        assert!(rendered.contains("Calls: Считать"), "{rendered}");
    }

    #[test]
    fn graph_context_leaf_keeps_signature_and_dispatch() {
        let a = client_server_workspace();
        // Сервер.Считать calls nothing — a leaf — but is still worth embedding by its
        // intrinsic signature + dispatch (not collapsed to an empty context).
        let ctx = a.graph_context_for_method(FileId(1), "Считать").expect("method resolves");
        assert_eq!(ctx.dispatch, vec!["server"]);
        assert_eq!(ctx.signature.as_deref(), Some("Функция Считать() Экспорт"));
        assert!(ctx.calls.is_empty());
        assert!(ctx.reads.is_empty());
        assert!(!ctx.is_empty());
    }

    #[test]
    fn graph_context_is_none_for_unresolved_method() {
        let a = client_server_workspace();
        assert!(a.graph_context_for_method(FileId(1), "НетТакого").is_none());
    }

    #[test]
    fn graph_context_reads_dedup_manager_touched_metadata() {
        // Two manager touches of the same object (create + access) collapse to one
        // `Reads` entry, in Russian spelling.
        let a = workspace(&[(
            "/src/CommonModules/Вызыватель/Ext/Module.bsl",
            "Процедура Делать() Экспорт\n\
             Справочники.Контрагенты.СоздатьЭлемент();\n\
             Справочники.Контрагенты.НайтиПоКоду();\n\
             КонецПроцедуры",
        )]);
        let ctx = a.graph_context_for_method(FileId(0), "Делать").expect("method resolves");
        assert_eq!(
            ctx.reads.iter().filter(|r| *r == "Справочник.Контрагенты").count(),
            1,
            "create + access collapse to one read entry: {:?}",
            ctx.reads
        );
        assert!(ctx.render().contains("Reads: Справочник.Контрагенты"));
    }

    #[test]
    fn source_returns_method_body_and_reports_bad_ids() {
        let a = client_server_workspace();
        let ids = vec![
            "method/common/Сервер/Считать".to_string(),
            "method/common/Сервер/НетТакого".to_string(),
        ];
        let result = a.graph_source(ROOT, None, &ids, 4000);
        assert_eq!(result.items.len(), 2);
        assert!(result.items[0].source.as_deref().unwrap().contains("Функция Считать"));
        assert!(result.items[0].error.is_none());
        assert!(result.items[1].source.is_none());
        assert!(matches!(result.items[1].error, Some(GraphError::NotFound { .. })));
    }

    #[test]
    fn source_honors_token_budget() {
        let a = client_server_workspace();
        let ids = vec![
            "method/common/Сервер/Считать".to_string(),
            "method/common/Клиент/Главная".to_string(),
        ];
        // 1 token ≈ 4 chars: the bodies far exceed it, so output is capped.
        let result = a.graph_source(ROOT, None, &ids, 1);
        assert!(result.budget_exhausted);
        let emitted: usize =
            result.items.iter().filter_map(|i| i.source.as_ref()).map(String::len).sum();
        assert!(emitted <= 4, "emitted {emitted} bytes must stay within the 4-byte budget");
        assert!(result.items.iter().all(|i| i.truncated || i.source.is_none()));
    }

    #[test]
    fn source_item_skipped_budget_flag_serializes_only_when_set() {
        // A budget-skipped item carries the flag so a client does not misread its absent
        // source as a method with no body; an ordinary item omits the flag entirely.
        let skipped = SourceItem {
            id: "method/common/М/А".to_string(),
            source: None,
            error: None,
            truncated: true,
            skipped_budget_exhausted: true,
        };
        let v = serde_json::to_value(&skipped).unwrap();
        assert_eq!(v["skipped_budget_exhausted"], serde_json::json!(true));

        let served = SourceItem {
            id: "method/common/М/Б".to_string(),
            source: Some("Процедура Б() КонецПроцедуры".to_string()),
            error: None,
            truncated: false,
            skipped_budget_exhausted: false,
        };
        let v = serde_json::to_value(&served).unwrap();
        assert!(v.get("skipped_budget_exhausted").is_none(), "flag must be omitted when false");
    }

    #[test]
    fn provenance_filter_excludes_non_matching_edges() {
        let a = client_server_workspace();
        let params = NeighborsParams {
            id: "method/common/Сервер/Считать",
            dir: Direction::In,
            depth: 1,
            max_nodes: 50,
            detail: GraphDetail::Names,
            provenance_filter: vec!["inferred".to_string()],
            edge_kind_filter: Vec::new(),
            call_sites: false,
            max_call_sites: 0,
        };
        let res = a.graph_neighbors(ROOT, None, &params).unwrap();
        // The only incoming edge is `resolved`, so the inferred-only filter drops it.
        assert!(res.edges.is_empty());
        assert!(res.nodes.is_empty());
    }

    /// The build-time `GraphRowEncoder` (used by the SQLite graph build) must
    /// produce byte-identical durable ids and matching node/edge fields to this
    /// module's serve-time encoder, so ids an agent holds survive the in-memory →
    /// SQLite switch.
    #[test]
    fn build_time_encoder_matches_serve_time_ids() {
        use hir::graph_index::{GraphIndex, GraphRowEncoder};
        use hir::ConfigsDatabase;

        let a = workspace(&[
            (
                "/src/CommonModules/Вызов/Ext/Module.bsl",
                "Утил.Делать();\n\
                 Процедура СоздатьКонтрагента() Экспорт\n\
                 Справочники.Контрагенты.СоздатьЭлемент();\n\
                 КонецПроцедуры",
            ),
            ("/src/CommonModules/Утил/Ext/Module.bsl", "Процедура Делать() Экспорт КонецПроцедуры"),
        ]);
        let db = a.database();
        let graph = db.workspace_call_graph(ROOT);

        let source_root = db.source_root_input(ROOT).root(db);
        let file_set = source_root.file_set();
        let modules: Vec<hir::ModuleId> = source_root
            .iter()
            .filter(|&f| hir::is_bsl_source(file_set, f))
            .map(hir::ModuleId::new)
            .collect();
        let index = GraphIndex::build(db, &modules);

        let mut paths: std::collections::HashMap<FileId, String> = std::collections::HashMap::new();
        for f in source_root.iter() {
            if let Some(p) = file_set.path_for_file(&f) {
                if let Some(s) = p.as_path().to_str() {
                    paths.insert(f, s.to_string());
                }
            }
        }
        let paths: rustc_hash::FxHashMap<FileId, String> = paths.into_iter().collect();
        let no_objects = hir::graph_index::MdoFiles::default();
        let encoder = GraphRowEncoder::new(&index, &paths, None, &no_objects);

        let ctx = GraphCtx::new(db, ROOT, None);

        let mut nodes = 0;
        let mut kinds = std::collections::HashSet::new();
        for node in graph.nodes() {
            let (build_id, build_addr) = encoder.encode(&node);
            let (serve_id, serve_addr) = ctx.encode_node(&node);
            assert_eq!(build_id, serve_id, "durable id mismatch for {node:?}");
            assert_eq!(build_addr, serve_addr, "addressable mismatch for {node:?}");

            let row = encoder.node_row(&node);
            let serve = ctx.node_ref(node.clone(), GraphDetail::Names);
            assert_eq!(row.kind, serve.kind, "kind for {node:?}");
            assert_eq!(row.name, serve.name, "name for {node:?}");
            let expected_qualified =
                (!matches!(serve.kind, "method" | "module")).then(|| row.qualified.clone());
            assert_eq!(expected_qualified, serve.qualified, "qualified for {node:?}");
            assert_eq!(row.module, serve.module, "module for {node:?}");
            assert_eq!(row.dispatch, serve.dispatch, "dispatch for {node:?}");
            assert_eq!(row.is_export, serve.is_export, "is_export for {node:?}");
            assert_eq!(row.addressable, serve.addressable, "addressable for {node:?}");
            kinds.insert(row.kind);
            nodes += 1;
        }
        assert!(nodes >= 4, "fixture should yield several nodes, got {nodes}");
        assert!(kinds.contains("method") && kinds.contains("module") && kinds.contains("mdo"));

        for edge in graph.edges() {
            let row = encoder.edge_row(edge);
            assert_eq!(row.from_id, ctx.encode_node(&edge.from).0);
            assert_eq!(row.to_id, ctx.encode_node(&edge.to).0);
            assert_eq!(row.kind, edge_kind_label(edge.kind));
            assert_eq!(row.provenance, provenance_label(edge));
            assert_eq!(row.crosses, edge.crosses_client_to_server);
        }
    }

    /// The same parity, over a root that is a LINK to the tree the paths were walked from.
    ///
    /// The sibling gate below states its root as `/ws/proj`, a path that is on no disk. Nothing
    /// resolves it, so declared and canonical coincide there and the two spellings never come
    /// apart — which is precisely the case this one has to make real. Both encoders take the
    /// same resolved root, so both mint the rel; either taking the declared one alone would
    /// mint a basename and the two would disagree about a node they both hold.
    #[cfg(unix)]
    #[test]
    fn build_time_encoder_matches_serve_time_through_a_linked_root() {
        use hir::graph_index::{GraphIndex, GraphRowEncoder};
        use hir::ConfigsDatabase;

        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("ws");
        std::fs::create_dir_all(real.join("scripts")).unwrap();
        let linked = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &linked).unwrap();
        // The file side is the walk's spelling — links resolved — as it is in production.
        let walked = real.canonicalize().unwrap().join("scripts/loose.bsl");
        let walked = walked.to_str().unwrap().to_owned();

        let a = workspace(&[(walked.as_str(), "Процедура Свободный() Экспорт КонецПроцедуры")]);
        let db = a.database();
        let graph = db.workspace_call_graph(ROOT);
        let source_root = db.source_root_input(ROOT).root(db);
        let file_set = source_root.file_set();
        let modules: Vec<hir::ModuleId> = source_root
            .iter()
            .filter(|&f| hir::is_bsl_source(file_set, f))
            .map(hir::ModuleId::new)
            .collect();
        let index = GraphIndex::build(db, &modules);
        let paths: rustc_hash::FxHashMap<FileId, String> = source_root
            .iter()
            .filter_map(|f| {
                file_set
                    .path_for_file(&f)
                    .and_then(|p| p.as_path().to_str().map(|s| (f, s.to_string())))
            })
            .collect();

        let no_objects = hir::graph_index::MdoFiles::default();
        let strip_root = StripRoot::resolve(&linked);
        let encoder =
            GraphRowEncoder::new(&index, &paths, Some(strip_root.resolved()), &no_objects);
        let ctx = GraphCtx::new(db, ROOT, Some(&strip_root));
        let mut seen = 0;
        for node in graph.nodes() {
            let (build_id, build_addr) = encoder.encode(&node);
            let (serve_id, serve_addr) = ctx.encode_node(&node);
            assert_eq!(build_id, serve_id, "id mismatch through the link for {node:?}");
            assert_eq!(build_addr, serve_addr, "addressable mismatch for {node:?}");
            if build_id.starts_with("method/") {
                assert_eq!(
                    build_id, "method/file/scripts/loose.bsl::Свободный",
                    "and the id is the rel form, not the basename a declared link would yield",
                );
                assert!(build_addr, "which is the addressable one");
                seen += 1;
            }
        }
        assert!(seen >= 1, "the loose-path method must surface as a node");
    }

    /// Parity for the path-fallback id forms: a file outside the recognised module
    /// layout (`module_key_for_path` → None) encodes to `method/file/<rel>::name`
    /// with a workspace root, or `method/file/<basename>::name` (addressable=false)
    /// without one. Build-time and serve-time encoders must agree in both.
    ///
    /// Its root is stated, not staged, so declared and canonical coincide — the case where
    /// they come apart is [`build_time_encoder_matches_serve_time_through_a_linked_root`].
    #[test]
    fn build_time_encoder_matches_serve_time_path_fallback() {
        use hir::graph_index::{GraphIndex, GraphRowEncoder};
        use hir::ConfigsDatabase;
        use std::path::Path;

        let a = workspace(&[(
            "/ws/proj/scripts/loose.bsl",
            "Процедура Свободный() Экспорт КонецПроцедуры",
        )]);
        let db = a.database();
        let graph = db.workspace_call_graph(ROOT);
        let source_root = db.source_root_input(ROOT).root(db);
        let file_set = source_root.file_set();
        let modules: Vec<hir::ModuleId> = source_root
            .iter()
            .filter(|&f| hir::is_bsl_source(file_set, f))
            .map(hir::ModuleId::new)
            .collect();
        let index = GraphIndex::build(db, &modules);
        let paths: rustc_hash::FxHashMap<FileId, String> = source_root
            .iter()
            .filter_map(|f| {
                file_set
                    .path_for_file(&f)
                    .and_then(|p| p.as_path().to_str().map(|s| (f, s.to_string())))
            })
            .collect();

        // With a workspace root → rel_path form (addressable); without → basename
        // fallback (not addressable). Both must match the serve-time encoder.
        for workspace_root in [Some(Path::new("/ws/proj")), None] {
            let no_objects = hir::graph_index::MdoFiles::default();
            let strip_root = workspace_root.map(StripRoot::resolve);
            let encoder = GraphRowEncoder::new(&index, &paths, workspace_root, &no_objects);
            let ctx = GraphCtx::new(db, ROOT, strip_root.as_ref());
            let mut seen = 0;
            for node in graph.nodes() {
                let (build_id, build_addr) = encoder.encode(&node);
                let (serve_id, serve_addr) = ctx.encode_node(&node);
                assert_eq!(build_id, serve_id, "id mismatch (ws={workspace_root:?}) for {node:?}");
                assert_eq!(
                    build_addr, serve_addr,
                    "addressable mismatch (ws={workspace_root:?}) for {node:?}"
                );
                seen += 1;
            }
            assert!(seen >= 1, "the loose-path method must surface as a node");
        }
    }

    /// The real gate for the search/diagnostics graph_id bridge: `method_graph_id` must mint
    /// the SAME path-fallback id the encoder stored for a loose-path method, and that id must
    /// resolve back to the method node — both for an absolute path (stripped by the root) and
    /// for the already-relative form the search overlay stores.
    #[test]
    fn method_graph_id_matches_encoder_and_round_trips_for_path_fallback() {
        use std::path::Path;

        let a = workspace(&[(
            "/ws/proj/scripts/loose.bsl",
            "Процедура Свободный() Экспорт КонецПроцедуры",
        )]);
        let root = Path::new("/ws/proj");
        let strip_root = StripRoot::resolve(root);

        // The durable id the encoder stores for the loose method.
        let db = a.database();
        let graph = db.workspace_call_graph(ROOT);
        let ctx = GraphCtx::new(db, ROOT, Some(&strip_root));
        let encoder_id = graph
            .nodes()
            .find(|n| matches!(n, GraphNode::Method(_)))
            .map(|n| ctx.encode_node(&n).0)
            .expect("loose method node");
        assert_eq!(encoder_id, "method/file/scripts/loose.bsl::Свободный");

        // Absolute path → stripped by the root to the encoder's rel.
        assert_eq!(
            method_graph_id("/ws/proj/scripts/loose.bsl", "Свободный", Some(&strip_root))
                .as_deref(),
            Some(encoder_id.as_str()),
        );
        // Already-relative path (search-overlay form) → used directly; root unused.
        assert_eq!(
            method_graph_id("scripts/loose.bsl", "Свободный", None).as_deref(),
            Some(encoder_id.as_str()),
        );
        // The minted id resolves back to the node (round-trip, not just a string match).
        assert!(
            a.graph_node(ROOT, Some(&strip_root), &encoder_id, GraphDetail::Names).is_ok(),
            "minted path-fallback id must resolve"
        );
    }

    #[test]
    fn method_graph_id_module_keyed_is_root_independent() {
        use std::path::Path;
        // A recognised module path is prefix-independent: same id with or without a root,
        // absolute or relative.
        for path in [
            "/anything/CommonModules/Утилиты/Ext/Module.bsl",
            "CommonModules/Утилиты/Ext/Module.bsl",
        ] {
            for root in [Some(Path::new("/ws")), None] {
                assert_eq!(
                    method_graph_id(path, "Сложить", root.map(StripRoot::resolve).as_ref())
                        .as_deref(),
                    Some("method/common/Утилиты/Сложить"),
                );
            }
        }
    }

    #[test]
    fn method_graph_id_file_fallback_normalization() {
        use std::path::Path;
        let root = Path::new("/ws/proj");

        // Absolute path under the root → rel form.
        assert_eq!(
            method_graph_id("/ws/proj/a/b/Module.bsl", "M", Some(&StripRoot::resolve(root)))
                .as_deref(),
            Some("method/file/a/b/Module.bsl::M"),
        );
        // Trailing slash on the root is tolerated (matches the encoder's rel).
        assert_eq!(
            method_graph_id(
                "/ws/proj/a/b/Module.bsl",
                "M",
                Some(&StripRoot::resolve(Path::new("/ws/proj/"))),
            )
            .as_deref(),
            Some("method/file/a/b/Module.bsl::M"),
        );
        // Backslash path is normalised to forward slashes.
        assert_eq!(
            method_graph_id(r"a\b\Module.bsl", "M", None).as_deref(),
            Some("method/file/a/b/Module.bsl::M"),
        );
        // Absolute path NOT under the root → None (never emit a non-resolving id).
        assert_eq!(
            method_graph_id("/elsewhere/Module.bsl", "M", Some(&StripRoot::resolve(root))),
            None
        );
        // A longer-named sibling that merely shares the prefix string is NOT under the root.
        assert_eq!(
            method_graph_id("/ws/project/a/Module.bsl", "M", Some(&StripRoot::resolve(root))),
            None
        );
        // Absolute path with no root → None.
        assert_eq!(method_graph_id("/ws/proj/a/Module.bsl", "M", None), None);
    }

    #[test]
    fn workspace_rel_path_strips_and_normalizes() {
        use std::path::Path;
        assert_eq!(
            workspace_rel_path("/ws/proj/a/b.bsl", Path::new("/ws/proj")).as_deref(),
            Some("a/b.bsl"),
        );
        assert_eq!(
            workspace_rel_path("/ws/proj/a/b.bsl", Path::new("/ws/proj/")).as_deref(),
            Some("a/b.bsl"),
        );
        assert_eq!(workspace_rel_path("/other/a.bsl", Path::new("/ws/proj")), None);
        // Sibling sharing the prefix string but not a path component → not under the root.
        assert_eq!(workspace_rel_path("/ws/project/a.bsl", Path::new("/ws/proj")), None);
        // The root itself (no rel remainder) → None.
        assert_eq!(workspace_rel_path("/ws/proj", Path::new("/ws/proj")), None);
    }

    /// A root reached through a link mints the id its canonical spelling mints.
    ///
    /// The rel in a path-fallback id is stripped off the walk's own path, which has every link
    /// resolved. A root declared through a link is therefore a prefix of nothing, the strip
    /// finds no rel, and the id disappears — leaving a form handler addressable or not
    /// depending on how the workspace happened to be spelled when the server was started. One
    /// workspace, one id, whichever spelling names it.
    #[cfg(unix)]
    #[test]
    fn a_root_declared_through_a_link_mints_the_id_its_real_path_mints() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("ws");
        std::fs::create_dir_all(real.join("Documents/Д/Forms/Ф/Ext/Form")).unwrap();
        let linked = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &linked).unwrap();

        // The path side is never in doubt: it is the walk's spelling, links already resolved.
        let root = real.canonicalize().expect("the workspace exists");
        let module =
            root.join("Documents/Д/Forms/Ф/Ext/Form/Module.bsl").to_string_lossy().into_owned();

        let direct = method_graph_id(&module, "ПриЗаписи", Some(&StripRoot::resolve(&root)));
        assert_eq!(
            direct.as_deref(),
            Some("method/file/Documents/Д/Forms/Ф/Ext/Form/Module.bsl::ПриЗаписи"),
            "control: the real path has always minted the fallback id",
        );
        assert_eq!(
            method_graph_id(&module, "ПриЗаписи", Some(&StripRoot::resolve(&linked))),
            direct,
            "and the link names the same workspace, so it names the same method",
        );
    }

    /// One file, two ways to spell it, one id — including when a link lies INSIDE the root.
    ///
    /// A `source.root` may itself be a link (`ws/src -> ws/actual`), and then the two spellings
    /// of one file diverge below the root, not at it: the walk records `actual/…` while the
    /// search overlay hands back the declared `src/…`. Stripping each against the root it looks
    /// like a prefix of yields two rels for one file, and the graph holds a node under only one
    /// of them — so the other is a well-formed id that resolves to nothing, which is worse than
    /// no id at all.
    #[cfg(unix)]
    #[test]
    fn one_file_reached_by_two_spellings_has_one_id() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        let form = "Documents/Д/Forms/Ф/Ext/Form/Module.bsl";
        std::fs::create_dir_all(real.join("actual/Documents/Д/Forms/Ф/Ext/Form")).unwrap();
        std::fs::write(real.join("actual").join(form), "").unwrap();
        // A link at the root AND a link inside it: `source.root = "src"` is how the second
        // arrives in a real project.
        std::os::unix::fs::symlink(real.join("actual"), real.join("src")).unwrap();
        let ws = dir.path().join("ws");
        std::os::unix::fs::symlink(&real, &ws).unwrap();

        let root = StripRoot::resolve(&ws);
        // What the walk records, and therefore what the encoder minted its rel from.
        let walked = real.canonicalize().unwrap().join("actual").join(form);
        // What the search overlay resolves a stored key to: the DECLARED spelling.
        let declared = ws.join("src").join(form);

        let from_walk = method_graph_id(walked.to_str().unwrap(), "ПриЗаписи", Some(&root));
        assert_eq!(
            from_walk.as_deref(),
            Some("method/file/actual/Documents/Д/Forms/Ф/Ext/Form/Module.bsl::ПриЗаписи"),
            "control: the walked spelling mints the rel the graph holds a node under",
        );
        assert_eq!(
            method_graph_id(declared.to_str().unwrap(), "ПриЗаписи", Some(&root)),
            from_walk,
            "and the declared spelling of the same file must name that same node",
        );

        // A root spelled without a link of its OWN is the case a string strip appears to
        // handle: `real/src/…` is then lexically under the root, so the strip places it —
        // under `src/`, where the graph holds nothing. The link that makes the two spellings
        // differ sits inside the root, and it sits there however the root is spelled.
        let plain = real.canonicalize().expect("the workspace exists");
        assert_eq!(
            method_graph_id(
                plain.join("src").join(form).to_str().unwrap(),
                "ПриЗаписи",
                Some(&StripRoot::resolve(&plain)),
            ),
            from_walk,
            "the link inside the root does not stop being one when the root is spelled plainly",
        );
    }

    #[test]
    fn module_id_of_method_inverts_each_member_separator() {
        // Keyed scope: strip the trailing `/<method>`.
        assert_eq!(
            module_id_of_method("method/common/Сервер/Считать").as_deref(),
            Some("module/common/Сервер"),
        );
        assert_eq!(
            module_id_of_method("method/manager/Catalog/Товары/Найти").as_deref(),
            Some("module/manager/Catalog/Товары"),
        );
        // File module: keep `file/<rel>`, drop the `::<method>` member.
        assert_eq!(
            module_id_of_method("method/file/src/cf/Forms/A/Module.bsl::ПриОткрытии").as_deref(),
            Some("module/file/src/cf/Forms/A/Module.bsl"),
        );
        // Not a method id, or no member segment → None.
        assert_eq!(module_id_of_method("module/common/Сервер"), None);
        assert_eq!(module_id_of_method("mdo/Catalog/Товары"), None);
        assert_eq!(module_id_of_method("method/file/::M"), None);
        // A `file/<rel>` with no `::` member separator is malformed, not a module bucket.
        assert_eq!(module_id_of_method("method/file/src/Module.bsl"), None);
    }

    #[test]
    fn rank_resolve_orders_by_match_strength_then_id() {
        let nodes = || {
            [
                ("method/common/Сервер/Считать".to_string(), "method"),
                ("method/common/Клиент/считать".to_string(), "method"),
                ("method/common/Прочее/СчитатьВсё".to_string(), "method"),
                ("module/common/Сервер".to_string(), "module"),
            ]
            .into_iter()
        };

        // Exact id wins outright.
        let (exact, _) = rank_resolve_candidates(nodes(), "method/common/Сервер/Считать", 10);
        assert_eq!(exact[0].id, "method/common/Сервер/Считать");
        assert_eq!(exact[0].match_kind, "exact");

        // A bare name matches both case-spellings as `name` (id-ascending), ahead of the
        // `substring`-only `СчитатьВсё`.
        let (by_name, _) = rank_resolve_candidates(nodes(), "Считать", 10);
        let labels: Vec<_> =
            by_name.iter().map(|c| (c.id.as_str(), c.match_kind, c.kind)).collect();
        assert_eq!(
            labels,
            vec![
                ("method/common/Клиент/считать", "name", "method"),
                ("method/common/Сервер/Считать", "name", "method"),
                ("method/common/Прочее/СчитатьВсё", "substring", "method"),
            ],
        );

        // The cap is honoured, and `total` reports the pre-cap match count so the caller
        // can tell the shown list is partial.
        let (capped, total) = rank_resolve_candidates(nodes(), "common", 2);
        assert_eq!(capped.len(), 2);
        assert_eq!(total, 4, "all four ids contain 'common'");
        let result = ResolveResult::new("common", capped, total);
        assert!(result.truncated, "two of four shown must flag truncation");

        // Under the cap, nothing is truncated.
        let (all, total) = rank_resolve_candidates(nodes(), "common", 10);
        assert_eq!(total, 4);
        assert!(!ResolveResult::new("common", all, total).truncated);

        // Exactly at the cap (limit == total) is the boundary: all shown, not truncated.
        let (exact_cap, total) = rank_resolve_candidates(nodes(), "common", 4);
        assert_eq!(exact_cap.len(), 4);
        assert_eq!(total, 4);
        assert!(!ResolveResult::new("common", exact_cap, total).truncated);

        // Empty query matches nothing and is never truncated.
        let (none, total) = rank_resolve_candidates(nodes(), "", 10);
        assert!(none.is_empty());
        assert_eq!(total, 0);
        assert!(!ResolveResult::new("", none, total).truncated);
    }

    /// Forms never enter the in-memory `workspace_call_graph` (they are emitted only
    /// by the SQLite build pass), so the loops above never see a `Form`/`FormItem`
    /// node. This pins the build-time vs serve-time encoder parity for those kinds —
    /// and the `contains` edge label — on synthetic nodes, guarding the shared
    /// `form_scope`/`form_qualified_prefix` helpers against drift.
    #[test]
    fn form_node_ids_match_between_encoders() {
        use hir::call_graph::{EdgeProvenance, WorkspaceCallEdge};
        use hir::graph_index::{GraphIndex, GraphRowEncoder};
        use hir::Name;

        let a = workspace(&[(
            "/src/CommonModules/М/Ext/Module.bsl",
            "Процедура П() Экспорт КонецПроцедуры",
        )]);
        let db = a.database();
        let source_root = db.source_root_input(ROOT).root(db);
        let file_set = source_root.file_set();
        let modules: Vec<hir::ModuleId> = source_root
            .iter()
            .filter(|&f| hir::is_bsl_source(file_set, f))
            .map(hir::ModuleId::new)
            .collect();
        let index = GraphIndex::build(db, &modules);
        let paths: rustc_hash::FxHashMap<FileId, String> = FxHashMap::default();
        let no_objects = hir::graph_index::MdoFiles::default();
        let encoder = GraphRowEncoder::new(&index, &paths, None, &no_objects);
        let ctx = GraphCtx::new(db, ROOT, None);

        let owner = Some((MdoType::Catalog, Name::new("Контрагенты")));
        let object_form = GraphNode::Form {
            owner: owner.clone(),
            form_name: Name::new("ФормаЭлемента"),
        };
        let common_form =
            GraphNode::Form { owner: None, form_name: Name::new("ОбщаяФорма1") };
        let object_item = GraphNode::FormItem {
            owner: owner.clone(),
            form_name: Name::new("ФормаЭлемента"),
            item_name: Name::new("ПолеКод"),
        };
        let common_item = GraphNode::FormItem {
            owner: None,
            form_name: Name::new("ОбщаяФорма1"),
            item_name: Name::new("Кнопка"),
        };
        let object_attr = GraphNode::FormAttribute {
            owner: owner.clone(),
            form_name: Name::new("ФормаЭлемента"),
            attr_name: Name::new("Объект"),
        };
        let common_attr = GraphNode::FormAttribute {
            owner: None,
            form_name: Name::new("ОбщаяФорма1"),
            attr_name: Name::new("Список"),
        };
        let section = GraphNode::TabularSection {
            mdo_type: MdoType::Catalog,
            object_name: Name::new("Контрагенты"),
            section_name: Name::new("Товары"),
        };
        let section_attr = GraphNode::TabularSectionAttribute {
            mdo_type: MdoType::Catalog,
            object_name: Name::new("Контрагенты"),
            section_name: Name::new("Товары"),
            attr_name: Name::new("Цена"),
        };

        for node in [
            &object_form,
            &common_form,
            &object_item,
            &common_item,
            &object_attr,
            &common_attr,
            &section,
            &section_attr,
        ] {
            assert_eq!(encoder.encode(node), ctx.encode_node(node), "encode mismatch for {node:?}");
            let row = encoder.node_row(node);
            let serve = ctx.node_ref(node.clone(), GraphDetail::Names);
            assert_eq!(row.kind, serve.kind, "kind for {node:?}");
            assert_eq!(row.name, serve.name, "name for {node:?}");
            assert_eq!(Some(row.qualified.clone()), serve.qualified, "qualified for {node:?}");
            assert_eq!(row.addressable, serve.addressable, "addressable for {node:?}");
        }

        // The exact durable id strings (object + common scopes).
        assert_eq!(encoder.encode(&object_form).0, "form/Catalog/Контрагенты/ФормаЭлемента");
        assert_eq!(encoder.encode(&common_form).0, "form/common/ОбщаяФорма1");
        assert_eq!(
            encoder.encode(&object_item).0,
            "form_item/Catalog/Контрагенты/ФормаЭлемента/ПолеКод"
        );
        assert_eq!(encoder.encode(&common_item).0, "form_item/common/ОбщаяФорма1/Кнопка");
        assert_eq!(
            encoder.encode(&object_attr).0,
            "form_attr/Catalog/Контрагенты/ФормаЭлемента/Объект"
        );
        assert_eq!(encoder.encode(&common_attr).0, "form_attr/common/ОбщаяФорма1/Список");
        assert_eq!(encoder.encode(&section).0, "tabular_section/Catalog/Контрагенты/Товары");
        assert_eq!(encoder.encode(&section_attr).0, "ts_attr/Catalog/Контрагенты/Товары/Цена");

        let edge = WorkspaceCallEdge {
            from: object_form.clone(),
            to: object_item.clone(),
            kind: EdgeKind::Contains,
            provenance: EdgeProvenance::Resolved,
            call_site: hir::CallSite::Structural,
            crosses_client_to_server: false,
        };
        let row = encoder.edge_row(&edge);
        assert_eq!(row.kind, "contains");
        assert_eq!(row.kind, edge_kind_label(edge.kind));
        assert_eq!(row.from_id, ctx.encode_node(&edge.from).0);
        assert_eq!(row.to_id, ctx.encode_node(&edge.to).0);
        assert_eq!(row.provenance, "resolved");
        // A form containing an item is not a call: the row says so with the code, not with
        // an absent span a consumer would read as "somewhere, unknown".
        assert_eq!((row.call_start, row.call_end), (None, None));
        assert_eq!(row.call_site_absent, Some(NO_CALL_SITE));
    }
}

#[cfg(test)]
mod mdo_row_tests {
    //! Where an object's row gets its file.

    use super::tests::{workspace, ROOT};
    use bsl_metadata::MdoType;
    use hir::graph_index::{mdo_files_key, GraphIndex, GraphRowEncoder, MdoFiles};
    use hir::{ConfigsDatabase, GraphNode};
    use ide_db::base_db::SourceDatabase;
    use rustc_hash::FxHashMap;
    use vfs::FileId;

    const CATALOG_XML: &str = "/src/Catalogs/Контрагенты.xml";

    fn row_for_the_catalog(mdo_files: &MdoFiles) -> hir::graph_index::NodeRow {
        // The code names the catalog, so the build sees THAT spelling first and
        // the node carries it — not the file tree's.
        let a = workspace(&[(
            "/src/CommonModules/Вызов/Ext/Module.bsl",
            "Процедура Создать() Экспорт\n\
             Справочники.Контрагенты.СоздатьЭлемент();\n\
             КонецПроцедуры",
        )]);
        let db = a.database();
        let graph = db.workspace_call_graph(ROOT);

        let source_root = db.source_root_input(ROOT).root(db);
        let file_set = source_root.file_set();
        let modules: Vec<hir::ModuleId> = source_root
            .iter()
            .filter(|&f| hir::is_bsl_source(file_set, f))
            .map(hir::ModuleId::new)
            .collect();
        let index = GraphIndex::build(db, &modules);

        let paths: FxHashMap<FileId, String> = source_root
            .iter()
            .filter_map(|f| {
                let path = file_set.path_for_file(&f)?;
                Some((f, path.as_path().to_str()?.to_string()))
            })
            .collect();

        let encoder = GraphRowEncoder::new(&index, &paths, None, mdo_files);
        let node = graph
            .nodes()
            .find(|node| matches!(node, GraphNode::Mdo { mdo_type, .. } if *mdo_type == MdoType::Catalog))
            .expect("the call names a catalog, so the graph holds a node for it");
        encoder.node_row(&node)
    }

    /// The map is keyed off the file tree; the node carries the first spelling
    /// the build saw, which is the code's whenever code mentions the object.
    /// They meet on the folded key alone — an exact-spelling lookup would place
    /// only the objects nobody refers to.
    #[test]
    fn an_object_is_placed_through_a_differently_cased_key() {
        let mut mdo_files = MdoFiles::default();
        mdo_files.insert(mdo_files_key(MdoType::Catalog, "КОНТРАГЕНТЫ"), CATALOG_XML.to_string());

        let row = row_for_the_catalog(&mdo_files);
        assert_eq!(row.name, "Контрагенты", "the node carries the code's spelling");
        assert_eq!(row.file.as_deref(), Some(CATALOG_XML));
    }

    /// A kind nothing discovers has no file to give, and the row stays
    /// addressable by its durable id all the same.
    #[test]
    fn an_object_the_map_does_not_know_keeps_no_file_and_stays_addressable() {
        let row = row_for_the_catalog(&MdoFiles::default());
        assert_eq!(row.file, None);
        assert!(row.addressable, "an object without a file is still a node one can open");
    }
}
