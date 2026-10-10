//! Agent-facing `symbol_info` tool: a consolidated semantic card for ONE symbol.
//!
//! Thin projection over [`ide::symbol_info`]. The resident host owns all BSL semantics
//! (kind, signature, type, doc, definition); this adapter bridges the resolved symbol to the
//! call graph for the `usages` summary and, on a resident miss, offers graph-derived
//! candidates. The core card is served whenever the resident is `Ready`, independent of the
//! graph's readiness.

use std::{path::Path, sync::Arc};

use ide::{Locale, SymbolInfoCard, SymbolInfoRequest, SymbolInfoSections, SymbolPosition};
use rmcp::model::CallToolResult;
use rmcp::ErrorData as McpError;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::diagnostics_state::DiagnosticsResident;
use crate::graph_query::GraphDb;
use crate::tools::location::{
    Completeness, Freshness, FreshnessSource, Location, LocationUnavailable, PositionRange,
    ReasonCode,
};
use crate::tools::response::{serialized_bytes, structured};

/// Default number of top calling modules included in the `usages` summary.
pub(crate) const DEFAULT_TOP_MODULES: usize = 5;

/// Default cap on graph candidates offered on a resident miss.
pub(crate) const DEFAULT_CANDIDATE_LIMIT: usize = 20;

#[derive(Deserialize, JsonSchema, Serialize)]
#[serde(untagged)]
#[schemars(transform = union_as_one_of)]
#[allow(
    dead_code,
    clippy::large_enum_variant,
    reason = "schema-only union published by tools/list is never instantiated"
)]
enum SymbolInfoOutput {
    Ok(SymbolInfoOk),
    NotFound(SymbolInfoCandidates<NotFoundStatus>),
    Ambiguous(SymbolInfoCandidates<AmbiguousStatus>),
    Loading(SymbolInfoLoading),
}

#[derive(Deserialize, JsonSchema, Serialize)]
struct SymbolInfoLoading {
    schema_version: SchemaVersion,
    status: LoadingStatus,
    detail: String,
    state: String,
    generation: u64,
    elapsed_ms: Option<u64>,
    error: Option<String>,
}

#[derive(Deserialize, JsonSchema, Serialize)]
struct SymbolInfoOk {
    schema_version: SchemaVersion,
    status: OkStatus,
    symbol: String,
    kind: Option<String>,
    container: Option<Value>,
    signature: Option<String>,
    doc: Option<String>,
    return_type: Option<String>,
    definition: Option<Value>,
    definitions: Option<Vec<Value>>,
    members: Option<Vec<SymbolMemberOutput>>,
    usages: Option<Value>,
    usages_unavailable: Option<String>,
    freshness: Option<Value>,
    truncated: Option<bool>,
    budget_hint: Option<String>,
    semantics_unavailable: Option<bool>,
}

#[derive(Deserialize, JsonSchema, Serialize)]
struct SymbolInfoCandidates<S> {
    schema_version: SchemaVersion,
    status: S,
    resolved: bool,
    symbol: String,
    candidates: Vec<Value>,
    total: usize,
    total_exact: bool,
    truncated: bool,
    providers: Vec<Value>,
    hint: String,
    freshness: Value,
}

#[derive(Deserialize, JsonSchema, Serialize)]
#[serde(untagged)]
#[schemars(transform = overlapping_union)]
#[allow(dead_code, reason = "schema-only member union published by tools/list")]
enum SymbolMemberOutput {
    Typed(TypedMemberOutput),
    Callable(CallableMemberOutput),
    Plain(PlainMemberOutput),
}

#[derive(Deserialize, JsonSchema, Serialize)]
struct TypedMemberOutput {
    name: String,
    kind: String,
    member_kind: MemberKind,
    origin: MemberOrigin,
    availability: MemberAvailabilityOutput,
    source_extension: Option<String>,
    #[serde(rename = "type")]
    type_name: String,
    type_variants: Vec<TypeVariantOutput>,
}

#[derive(Deserialize, JsonSchema, Serialize)]
struct CallableMemberOutput {
    name: String,
    kind: String,
    member_kind: MemberKind,
    origin: MemberOrigin,
    availability: MemberAvailabilityOutput,
    source_extension: Option<String>,
    signature: MemberSignatureOutput,
}

/// A member the analysis names by kind alone: a tabular section, or an attribute or form
/// element whose type did not resolve. Neither `type` nor `signature` describes it, and a
/// union without this branch calls a routine catalog card malformed.
#[derive(Deserialize, JsonSchema, Serialize)]
struct PlainMemberOutput {
    name: String,
    kind: String,
    member_kind: MemberKind,
    origin: MemberOrigin,
    availability: MemberAvailabilityOutput,
    source_extension: Option<String>,
}

#[derive(Deserialize, JsonSchema, Serialize)]
struct MemberAvailabilityOutput {
    contexts: Option<Vec<MemberContext>>,
    context_status: ContextStatus,
    reason: Option<String>,
}

#[derive(Deserialize, JsonSchema, Serialize)]
struct TypeVariantOutput {
    presentation: String,
    technical_name: Option<String>,
    resolution: TypeResolution,
    reason: Option<String>,
}

#[derive(Deserialize, JsonSchema, Serialize)]
struct MemberSignatureOutput {
    presentation: String,
}

#[macro_export]
macro_rules! wire_enum {
    ($name:ident { $($variant:ident => $wire:literal),+ $(,)? }) => {
        #[derive(Deserialize, JsonSchema, Serialize)]
        #[allow(dead_code, reason = "schema-only wire values published by tools/list")]
        enum $name {
            $(#[serde(rename = $wire)] $variant),+
        }
    };
}

wire_enum!(SchemaVersion { V1 => "1" });
wire_enum!(OkStatus { Ok => "ok" });
wire_enum!(NotFoundStatus { NotFound => "not_found" });
wire_enum!(AmbiguousStatus { Ambiguous => "ambiguous" });
wire_enum!(LoadingStatus { Loading => "loading" });
wire_enum!(MemberKind {
    Attribute => "attribute",
    TabularSection => "tabular_section",
    Method => "method",
    Variable => "variable",
    Property => "property",
    FormAttribute => "form_attribute",
    FormElement => "form_element",
    Handler => "handler",
});
wire_enum!(MemberOrigin {
    Metadata => "metadata",
    Module => "module",
    Platform => "platform",
});
wire_enum!(MemberContext {
    ThinClient => "thin_client",
    WebClient => "web_client",
    ThickClientManaged => "thick_client_managed",
    ThickClientOrdinary => "thick_client_ordinary",
    Server => "server",
    MobileClient => "mobile_client",
    ExternalConnection => "external_connection",
});
wire_enum!(ContextStatus {
    Available => "available",
    Unavailable => "unavailable",
    Unknown => "unknown",
    NotEvaluated => "not_evaluated",
});
wire_enum!(TypeResolution {
    Static => "static",
    Source => "source",
    Unresolved => "unresolved",
});

/// Publish a union whose branches OVERLAP as `anyOf`.
///
/// A typed member satisfies the typed branch and the plain one alike — the plain branch names
/// the fields every member carries and forbids nothing beyond them. Under `oneOf` a validator
/// must find exactly one match and rejects the card; `anyOf` states what is true: the member
/// has at least one of these shapes.
fn overlapping_union(schema: &mut schemars::Schema) {
    let object = schema.ensure_object();
    if let Some(branches) = object.remove("oneOf") {
        object.insert("anyOf".to_string(), branches);
    }
    crate::contract::ensure_object_root(object);
}

pub(crate) fn union_as_one_of(schema: &mut schemars::Schema) {
    let object = schema.ensure_object();
    if let Some(branches) = object.remove("anyOf") {
        object.insert("oneOf".to_string(), branches);
    }
    crate::contract::ensure_object_root(object);
}

pub(crate) fn symbol_info_output_schema() -> Arc<serde_json::Map<String, Value>> {
    let mut schema = (*rmcp::handler::server::tool::schema_for_type::<SymbolInfoOutput>()).clone();
    single_value_enums_to_consts(&mut schema);
    crate::contract::ensure_object_root(&mut schema);
    Arc::new(schema)
}

fn single_value_enums_to_consts(object: &mut Map<String, Value>) {
    if let Some(constant) = object
        .get("enum")
        .and_then(Value::as_array)
        .filter(|values| values.len() == 1)
        .and_then(|values| values.first())
        .cloned()
    {
        object.remove("enum");
        object.insert("const".into(), constant);
    }
    for value in object.values_mut() {
        match value {
            Value::Object(object) => single_value_enums_to_consts(object),
            Value::Array(values) => {
                for value in values {
                    if let Value::Object(object) = value {
                        single_value_enums_to_consts(object);
                    }
                }
            }
            _ => {}
        }
    }
}

pub(crate) fn database_error(message: impl std::fmt::Display) -> McpError {
    McpError::internal_error(format!("symbol_info database: {message}"), None)
}

/// Preserve the resident loading envelope used by sibling tools while making the
/// successful `symbol_info` variant conform to this tool's versioned output schema.
pub(crate) fn loading(report: &crate::diagnostics_state::StatusReport) -> CallToolResult {
    let mut result = crate::tools::metadata::loading(report);
    result.structured_content.as_mut().expect("metadata loading is structured")["schema_version"] =
        json!("1");
    result
}

/// Interpret the `include` section filter. An empty filter means "all sections"; unknown
/// entries are ignored (a superset request never narrows below the named sections).
pub(crate) fn sections_from(include: &[String]) -> SymbolInfoSections {
    if include.is_empty() {
        return SymbolInfoSections::all();
    }
    let has = |name: &str| include.iter().any(|s| s.eq_ignore_ascii_case(name));
    SymbolInfoSections { definition: has("definition"), type_: has("type"), doc: has("doc") }
}

/// Parse the optional locale (`ru`/`en`), defaulting to the project default.
pub(crate) fn locale_from(raw: Option<&str>) -> Result<Locale, McpError> {
    match raw {
        Some(s) => {
            Locale::from_config_str(s).map_err(|e| McpError::invalid_params(e.to_string(), None))
        }
        None => Ok(Locale::default()),
    }
}

pub(crate) fn filter_members(
    card: &mut SymbolInfoCard,
    member_kind: Option<&str>,
    member_name: Option<&str>,
) {
    card.members.retain(|member| {
        member_kind.is_none_or(|kind| member.member_kind.eq_ignore_ascii_case(kind))
            && member_name.is_none_or(|name| member.name.to_lowercase() == name.to_lowercase())
    });
}

/// Whether this answer is worth one forced rescan and a retry.
///
/// The decision is read off the anchor the caller used and the card it produced, and
/// nowhere else. A request made BY NAME that resolved nothing carries evidence a second
/// look can check: something on disk may declare that name since the last walk. A
/// POSITIONAL request carries none — it says only "no symbol stands at these coordinates",
/// and re-reading the same coordinates says it again. An error is not a miss at all: no
/// walk turns a malformed request into a card.
///
/// Beside the card rather than in the handler, for the same reason as its `references`
/// namesake: `read()` polls for drift before it computes anything, so an end-to-end answer
/// may already be a hit with no rescan behind it, and a rule kept where nothing can observe
/// it is a rule that drifts.
pub(crate) fn warrants_rescan(
    symbol: Option<&str>,
    card: Result<Option<&SymbolInfoCard>, &McpError>,
) -> bool {
    symbol.is_some() && matches!(card, Ok(None))
}

/// Resolve the semantic card on the resident host. Runs inside the resident read lock.
/// `Ok(None)` means the resident could not resolve the request — the caller then offers graph
/// candidates rather than a hard error. A malformed positional request is a param error.
#[allow(
    clippy::too_many_arguments,
    reason = "one argument per declared tool parameter; a grouping struct here would have to be \
              built at the single call site and taken apart again in the first statement"
)]
pub(crate) fn resolve_card(
    resident: &DiagnosticsResident,
    db: &ide::RootDatabaseImpl,
    symbol: Option<&str>,
    root_id: Option<&str>,
    path: Option<&str>,
    line: Option<u32>,
    column: Option<u32>,
    sections: SymbolInfoSections,
    locale: Locale,
) -> Result<Option<SymbolInfoCard>, McpError> {
    let position = match path {
        Some(path) => {
            // Resolved ONCE and bound to the same name, so the unreadable branch below reads
            // the same file this resolves to rather than the same spelling the caller sent.
            let path = resident
                .resolve_rooted_path(root_id, Path::new(path))
                .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
            let path = path.as_path();
            let file_id = resident.file_id_for(path).ok_or_else(|| {
                // Same split as `diagnostics file`: an existing but unreadable file
                // gets its own answer rather than being called a non-workspace path.
                if resident.is_unread(path) {
                    McpError::invalid_params(
                        format!(
                            "'{}' is a workspace .bsl file whose bytes could not be read; \
                             it is held out of service and re-read every drift window",
                            path.display()
                        ),
                        None,
                    )
                } else {
                    McpError::invalid_params(
                        format!("'{}' is not a resident workspace .bsl file", path.display()),
                        None,
                    )
                }
            })?;
            let line = line
                .ok_or_else(|| McpError::invalid_params("'line' is required with 'path'", None))?;
            let column = column.unwrap_or(0);
            Some(SymbolPosition { file_id, line, column })
        }
        None => {
            // `root_id` qualifies `path` and nothing else, so any root beside a symbol is a
            // request that cannot be honoured: a symbol is resolved by NAME, across the whole
            // resident, and the card comes from whichever root owns that name. The empty id is
            // no exception — it asserts the configuration, and a module that exists only in an
            // extension would answer it anyway. Silence there would be the very substitution
            // this node exists to stop, and the same value would be a hard error in one shape
            // and a no-op in the other.
            if root_id.is_some() {
                return Err(McpError::invalid_params(
                    "'root_id' spells out which root 'path' is relative to, so it needs a \
                     'path'; a 'symbol' is resolved by name and belongs to no root"
                        .to_owned(),
                    None,
                ));
            }
            None
        }
    };

    if symbol.is_none() && position.is_none() {
        return Err(McpError::invalid_params("one of 'symbol' or 'path'+'line' is required", None));
    }

    let req = SymbolInfoRequest {
        symbol: symbol.map(str::to_string),
        position,
        locale,
        sections,
        // The graph was built against the resident's workspace root; a form handler's path-fallback
        // graph id must be encoded relative to it (NOT the config root) to resolve its usages.
        // Taken from the resident's own root table, so a card names a method exactly as a finding
        // about that method does: one generation, one base, whatever the workspace is spelled
        // through by the time the two requests arrive.
        workspace_root: Some(ide::StripRoot::pinned(
            resident.workspace_roots().workspace_canonical(),
        )),
    };
    Ok(ide::symbol_info(db, &req))
}

/// Serialize a resolved card, enriching it with a graph-derived `usages` summary when the
/// graph is available. `graph` is `None` when the call graph is not `Ready` — the core card is
/// still returned, stamped with `usages_unavailable`. The symbol is bridged to the graph by the
/// durable id the resident resolution already produced (`card.graph_id`), so no fuzzy
/// re-resolution is needed and a dotted `Module.Method` name resolves its fan-in reliably.
pub(crate) fn render_card(
    card: &SymbolInfoCard,
    graph: Option<&GraphDb>,
    top_modules: usize,
    max_output_tokens: usize,
    stamp: &ResidentStamp<'_>,
) -> CallToolResult {
    let mut body = card_to_value(card);
    // Three states, not one flag: the resident lost the symbol's semantics, the graph is
    // still indexing, or the graph answered and does not know this symbol. They call for
    // different reactions — retry, retry later, don't retry — so they get different codes.
    let mut degraded = card.semantics_unavailable;
    let mut indexing = false;

    // The node's durable id, published whether or not the graph is up. It is derived from the
    // declaration, so it is knowable while the graph still indexes — and `references` prints
    // the same id for the same anchor. Serving it only beside `usages` made the two tools
    // disagree on a cold graph: one named the node, the other did not.
    if let Some(graph_id) = card.graph_id.as_deref() {
        body["graph_id"] = json!(graph_id);
    }

    if !card.semantics_unavailable {
        match (card.graph_id.as_deref(), graph) {
            (Some(graph_id), Some(graph)) => match usages(graph, graph_id, top_modules) {
                Some(u) => {
                    body["usages"] = u;
                }
                None => {
                    body["usages_unavailable"] = json!("symbol not in call graph");
                    degraded = true;
                }
            },
            // Cards without a graph id (metadata objects/attributes, platform members) carry no
            // usages summary; only note the graph being unavailable for graph-addressable symbols.
            (Some(_), None) => {
                body["usages_unavailable"] = json!("graph indexing");
                indexing = true;
            }
            (None, _) => {}
        }
    }

    // The definition under the location contract, beside the legacy `definition` key. A list
    // because an extension may override a method (`&Вместо`, `&До`, `&После`) and the shape
    // must not have to change when the second target starts being reported.
    body["definitions"] = json!(definitions_value(card, stamp));

    let completeness = |budget_exhausted| {
        Completeness::complete()
            .when(
                budget_exhausted,
                ReasonCode::OutputBudget,
                "card trimmed to fit max_output_tokens",
            )
            .when(
                degraded,
                ReasonCode::ModalityDegraded,
                "the call graph could not answer for this symbol",
            )
            // The same condition the miss shape reports, under the same code: the graph is
            // building, and a consumer driving retries off the code must not get two answers
            // for one state depending on whether the symbol happened to resolve.
            .when(
                indexing,
                ReasonCode::IndexBuilding,
                "the call graph is still indexing, so usages are not counted yet",
            )
        // Deliberately NOT stamped here: the workspace-wide unread counter. This answer is
        // one resolved symbol, and a hole in an unrelated module does not make it less
        // whole. The miss below is the shape where a hole CAN hide the answer, and that is
        // where the counter is read.
    };
    body["freshness"] = stamp.freshness(completeness(false)).to_value();

    let full = structured(body.clone());
    if serialized_bytes(&full) <= max_output_tokens.saturating_mul(4) {
        return full;
    }
    body["freshness"] = stamp.freshness(completeness(true)).to_value();
    fit_card_to_budget(body, max_output_tokens)
}

/// What the caller knows about the resident that answered: its root table (for locations)
/// and its freshness. Carried as one value so a new envelope field cannot be forgotten at
/// one of the two response shapes.
pub(crate) struct ResidentStamp<'a> {
    pub roots: Option<&'a bsl_search::WorkspaceRoots>,
    pub revision: u64,
    pub topology: u64,
    pub stale: bool,
    pub unread_files: usize,
}

impl ResidentStamp<'_> {
    fn freshness(&self, completeness: Completeness) -> Freshness {
        Freshness::new(FreshnessSource::Resident, completeness)
            .with_revision(self.revision)
            .with_topology(self.topology)
            .with_stale(self.stale)
    }
}

/// The definition sites under the location contract. Empty when the card has no source
/// (platform members, metadata objects): an empty list says "no definition site", where an
/// invented location would say something false.
fn definitions_value(card: &SymbolInfoCard, stamp: &ResidentStamp<'_>) -> Vec<Value> {
    card.definitions.iter().map(|site| definition_site_value(card, site, stamp)).collect()
}

/// One declaration site: where it is, and the part it plays for the requested name.
///
/// The part matters as much as the place. An extension weaving `&Вместо` onto a method means
/// the base body no longer runs on its own, and a list of bare locations cannot say that.
fn definition_site_value(
    card: &SymbolInfoCard,
    site: &ide::SymbolDefinitionSite,
    stamp: &ResidentStamp<'_>,
) -> Value {
    let def = &site.definition;
    let mut entry = Map::new();
    entry.insert("role".into(), json!(site.role.as_str()));
    if let Some(extension) = &site.source_extension {
        entry.insert("source_extension".into(), json!(extension));
    }

    match (&def.path, stamp.roots) {
        (Some(path), Some(roots)) => match Location::from_path(roots, Path::new(path)) {
            Ok(location) => {
                let location = location
                    .with_range(def.name_range.map(PositionRange::from))
                    .with_enclosing_range(def.enclosing_range.map(PositionRange::from))
                    .with_module(module_ref(card));
                entry.insert("location".into(), location.to_value());
            }
            Err(reason) => {
                entry.insert("location_unavailable".into(), json!(reason.code()));
            }
        },
        // The two remaining cases are different facts and must not share a code: no table
        // at all, versus a table that was there and a path that could not be named.
        (Some(_), None) => {
            entry.insert(
                "location_unavailable".into(),
                json!(LocationUnavailable::RootsUnavailable.code()),
            );
        }
        (None, _) => {
            entry.insert(
                "location_unavailable".into(),
                json!(LocationUnavailable::SourcePathUnavailable.code()),
            );
        }
    }

    // No snippet here on purpose: the card already carries it in `definition`, and a second
    // copy would double the very payload the budget just trimmed. The location says WHERE;
    // the text stays where it was.
    Value::Object(entry)
}

/// The owning module, when the card already holds both halves — never parsed back out of a
/// display string.
fn module_ref(card: &SymbolInfoCard) -> Option<crate::tools::location::ModuleRef> {
    let container = card.container.as_ref()?;
    // Only a common module's container IS the module. For a method of an object or manager
    // module the container describes the OWNING OBJECT (`Документ`, `Справочник`, …), and
    // publishing that under a key named `module` would make one key mean two things —
    // exactly the ambiguity this contract removes.
    if container.kind != "ОбщийМодуль" {
        return None;
    }
    Some(crate::tools::location::ModuleRef {
        kind: container.kind.clone(),
        name: container.name.clone(),
    })
}

/// The resident-miss response: the name dictionary's candidates.
///
/// It used to be a projection of the call graph, which made the miss useless in
/// the two cases it mattered most — a platform member, which the graph does not
/// hold at all, and a workspace with no graph yet, where the answer was an empty
/// list dressed as an answer. The dictionary asks the platform and the
/// resident's own tables too, and names whichever source could not be asked.
pub(crate) fn render_not_found(
    symbol: &str,
    answer: crate::tools::name_answer::NameAnswer,
    stamp: &ResidentStamp<'_>,
) -> CallToolResult {
    let status = if answer.total == 0 { "not_found" } else { "ambiguous" };
    let mut body = serde_json::Map::new();
    body.insert("schema_version".into(), json!("1"));
    body.insert("status".into(), json!(status));
    body.insert("resolved".into(), json!(false));
    body.insert("symbol".into(), json!(symbol));
    let completeness = answer.insert_into(&mut body);
    // Here the counter matters: the symbol was not found, and an unread module is
    // a place it could have been.
    let completeness = completeness.when(
        stamp.unread_files > 0,
        ReasonCode::UnreadableFiles,
        "some workspace files could not be read, so the search was not exhaustive",
    );
    body.insert(
        "hint".into(),
        json!(
            "no exact resident match; feed a candidate's `address.symbol` back to \
             `symbol_info`, open its `address.graph_id` in `graph`, or read its \
             `address.syntax_help` in `syntax_help`"
        ),
    );
    body.insert("freshness".into(), stamp.freshness(completeness).to_value());
    structured(Value::Object(body))
}

fn usages(graph: &GraphDb, graph_id: &str, top_modules: usize) -> Option<Value> {
    let summary = graph.usages(graph_id, top_modules).ok()??;
    let top: Vec<Value> = summary
        .top_modules
        .iter()
        .map(|(module, count)| json!({ "module": module, "count": count }))
        .collect();
    let mut value = json!({
        "count": summary.count,
        "top_modules": top,
        "graph_id": graph_id,
    });
    // `count` is the full fan-in; `top_modules` is aggregated from a capped caller sample. Flag
    // the discrepancy so an agent knows the per-module breakdown is partial and can walk the
    // full caller list via `graph` with the returned `graph_id`.
    if summary.top_modules_sampled {
        value["top_modules_sampled"] = json!(true);
    }
    Some(value)
}

fn card_to_value(card: &SymbolInfoCard) -> Value {
    let mut body = Map::new();
    body.insert("schema_version".into(), json!("1"));
    body.insert("status".into(), json!("ok"));
    body.insert("symbol".into(), json!(card.symbol));
    body.insert("kind".into(), json!(card.kind));

    if card.semantics_unavailable {
        body.insert("semantics_unavailable".into(), json!(true));
    }
    if let Some(container) = &card.container {
        let mut c = json!({ "kind": container.kind, "name": container.name });
        if let Some(context) = &container.context {
            c["context"] = json!(context);
        }
        body.insert("container".into(), c);
    }
    if let Some(signature) = &card.signature {
        body.insert("signature".into(), json!(signature));
    }
    if let Some(doc) = &card.doc {
        body.insert("doc".into(), json!(doc));
    }
    if let Some(return_type) = &card.return_type {
        body.insert("return_type".into(), json!(return_type));
    }
    if let Some(def) = &card.definition {
        let mut d = json!({ "line": def.line });
        if let Some(path) = &def.path {
            d["path"] = json!(path);
        }
        if let Some(snippet) = &def.snippet {
            d["snippet"] = json!(snippet);
        }
        body.insert("definition".into(), d);
    }

    if !card.members.is_empty() {
        let mut members: Vec<Value> = card
            .members
            .iter()
            .map(|m| {
                let mut v = json!({
                    "name": m.name,
                    "kind": m.kind,
                    "member_kind": m.member_kind,
                    "origin": m.origin.as_str(),
                    "availability": {
                        "contexts": m.availability.contexts,
                        "context_status": m.availability.context_status.as_str(),
                    },
                });
                if let Some(source_extension) = &m.source_extension {
                    v["source_extension"] = json!(source_extension);
                }
                if let Some(reason) = &m.availability.reason {
                    v["availability"]["reason"] = json!(reason);
                }
                if let Some(ty) = &m.ty {
                    v["type"] = json!(ty);
                    v["type_variants"] = json!(m
                        .type_variants
                        .iter()
                        .map(|variant| {
                            let mut value = json!({
                                "presentation": variant.presentation,
                                "technical_name": variant.technical_name,
                                "resolution": variant.resolution,
                            });
                            if let Some(reason) = &variant.reason {
                                value["reason"] = json!(reason);
                            }
                            value
                        })
                        .collect::<Vec<_>>());
                } else if let Some(signature) = &m.signature {
                    v["signature"] = json!({ "presentation": signature.presentation });
                }
                v
            })
            .collect();
        members.sort_by_cached_key(|member| {
            (
                member["member_kind"].as_str().unwrap_or_default().to_owned(),
                member["name"].as_str().unwrap_or_default().to_lowercase(),
                member["origin"].as_str().unwrap_or_default().to_owned(),
                member["source_extension"].as_str().unwrap_or_default().to_owned(),
            )
        });
        body.insert("members".into(), json!(members));
    }

    Value::Object(body)
}

fn fit_card_to_budget(mut body: Value, max_output_tokens: usize) -> CallToolResult {
    let ceiling = max_output_tokens.saturating_mul(4);
    let symbol = body["symbol"].clone();
    body["truncated"] = json!(true);
    body["budget_hint"] = json!("increase max_output_tokens or narrow member_kind/member_name");

    let fitted = |body: &Value| {
        let result = structured(body.clone());
        (serialized_bytes(&result) <= ceiling).then_some(result)
    };
    if let Some(result) = fitted(&body) {
        return result;
    }

    for field in ["doc", "usages"] {
        if body.as_object_mut().is_some_and(|body| body.remove(field).is_some()) {
            if let Some(result) = fitted(&body) {
                return result;
            }
        }
    }
    if body
        .get_mut("definition")
        .and_then(Value::as_object_mut)
        .is_some_and(|definition| definition.remove("snippet").is_some())
    {
        if let Some(result) = fitted(&body) {
            return result;
        }
    }
    while body
        .get_mut("members")
        .and_then(Value::as_array_mut)
        .is_some_and(|members| members.pop().is_some())
    {
        if let Some(result) = fitted(&body) {
            return result;
        }
    }
    if body.as_object_mut().expect("card body is an object").remove("members").is_some() {
        if let Some(result) = fitted(&body) {
            return result;
        }
    }
    for field in [
        "definitions",
        "freshness",
        "definition",
        "kind",
        "container",
        "signature",
        "return_type",
        "usages_unavailable",
        "semantics_unavailable",
    ] {
        if body.as_object_mut().is_some_and(|body| body.remove(field).is_some()) {
            if let Some(result) = fitted(&body) {
                return result;
            }
        }
    }

    structured(json!({
        "schema_version": "1",
        "status": "ok",
        "symbol": symbol,
        "truncated": true,
        "budget_hint": "increase max_output_tokens or narrow member_kind/member_name",
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ide::{
        SymbolContainer, SymbolDefinition, SymbolMember, SymbolMemberAvailability,
        SymbolMemberContextStatus, SymbolMemberOrigin, SymbolTypeVariant,
    };
    use line_index::LineColRange;

    /// A workspace whose configuration root owns the module the card below points at.
    fn stand_roots() -> bsl_search::WorkspaceRoots {
        let (roots, _rejected) =
            bsl_search::WorkspaceRoots::build(Path::new("/ws"), Path::new("/ws/src/cf"), &[]);
        roots
    }

    fn stamp(roots: &bsl_search::WorkspaceRoots) -> ResidentStamp<'_> {
        ResidentStamp {
            roots: Some(roots),
            revision: 7,
            topology: 0x0a1b_2c3d_4e5f_6071,
            stale: false,
            unread_files: 0,
        }
    }

    fn method_card() -> SymbolInfoCard {
        let definition = SymbolDefinition {
            path: Some("/ws/src/cf/CommonModules/МойМодуль/Ext/Module.bsl".to_string()),
            line: 3,
            snippet: Some("Функция Сложить(А, Б) Экспорт".to_string()),
            name_range: Some(LineColRange {
                start_line: 2,
                start_character: 8,
                end_line: 2,
                end_character: 15,
            }),
            enclosing_range: Some(LineColRange {
                start_line: 2,
                start_character: 0,
                end_line: 6,
                end_character: 15,
            }),
        };
        SymbolInfoCard {
            symbol: "МойМодуль.Сложить".to_string(),
            kind: "function",
            container: Some(SymbolContainer {
                kind: "ОбщийМодуль".to_string(),
                name: "МойМодуль".to_string(),
                context: Some("Сервер".to_string()),
            }),
            signature: Some("Функция Сложить(А, Б) Экспорт".to_string()),
            doc: Some("Складывает".to_string()),
            return_type: Some("Число".to_string()),
            definition: Some(definition.clone()),
            definitions: vec![ide::SymbolDefinitionSite {
                definition,
                role: ide::DefinitionRole::Base,
                source_extension: None,
            }],
            members: Vec::new(),
            graph_id: Some("method/common/МойМодуль/Сложить".to_string()),
            semantics_unavailable: false,
        }
    }

    /// Which answers earn one forced rescan is decided by the anchor and the card, and the
    /// list is exhaustive over what this surface serves.
    ///
    /// The cell that matters is the positional miss. `Ok(None)` is common to both anchor
    /// forms, so a predicate written as "any miss" would switch a rescan on for
    /// `path`+`line`, where a second look at the same coordinates can only repeat the first
    /// — at the price of a walk of the whole tree.
    #[test]
    fn a_rescan_is_earned_by_a_named_miss_and_by_no_other_answer() {
        let card = method_card();
        let refused = McpError::invalid_params("'line' is required with 'path'", None);

        assert!(
            warrants_rescan(Some("МойМодуль.Сложить"), Ok(None)),
            "a name nothing declares may be a name declared since the last scan",
        );
        assert!(
            !warrants_rescan(None, Ok(None)),
            "a positional miss says only that no symbol stands at those coordinates, and a \
             second look at the same coordinates says it again",
        );
        assert!(
            !warrants_rescan(Some("МойМодуль.Сложить"), Ok(Some(&card))),
            "a card is an answer, not a miss",
        );
        assert!(
            !warrants_rescan(Some("МойМодуль.Сложить"), Err(&refused)),
            "a refused request is not a resident that fell behind the disk",
        );
        assert!(
            !warrants_rescan(None, Ok(Some(&card))),
            "a positional request that resolved is an answer too",
        );
        assert!(
            !warrants_rescan(None, Err(&refused)),
            "and a refused positional request is neither an answer nor a miss",
        );
    }

    /// The positional request is the one place `symbol_info` takes a path, and a path from a
    /// `search` hit is spelled against the root that owns it. Answered without the root, this
    /// lands on the configuration's module of the same name and describes a symbol the caller
    /// never asked about — which is why the two modules declare differently named functions.
    #[test]
    fn a_rooted_position_describes_the_symbol_of_that_root() {
        use crate::diagnostics_state::test_support::{
            extension_root_id, wait_ready, workspace_with_an_outside_extension,
            CONFIGURATION_SYMBOL, EXTENSION_SYMBOL, SHARED_MODULE_REL,
        };
        use crate::diagnostics_state::DiagnosticsState;

        let (_dir, workspace, extension) = workspace_with_an_outside_extension();
        let root_id = extension_root_id(&workspace, &extension);
        let state = DiagnosticsState::for_workspace(workspace.clone());
        state.ensure_loading();
        wait_ready(&state);

        let card = match state.read(|resident, _| {
            // Line 1, past `Функция `, is the function's own name in both modules.
            resolve_card(
                resident,
                resident.db(),
                None,
                Some(root_id.as_str()),
                Some(SHARED_MODULE_REL),
                Some(1),
                Some(8),
                sections_from(&[]),
                Locale::default(),
            )
        }) {
            crate::diagnostics_state::ResidentOutcome::Ready(card, _) => {
                card.expect("the request resolves").expect("the position names a symbol")
            }
            _ => panic!("the resident is ready in this stand"),
        };

        assert!(
            card.symbol.contains(EXTENSION_SYMBOL),
            "the card describes the extension's function: {}",
            card.symbol,
        );
        assert!(
            !card.symbol.contains(CONFIGURATION_SYMBOL),
            "and not its configuration namesake: {}",
            card.symbol,
        );
    }

    #[test]
    fn empty_include_means_all_sections() {
        let s = sections_from(&[]);
        assert!(s.definition && s.type_ && s.doc);
    }

    #[test]
    fn include_narrows_to_named_sections() {
        let s = sections_from(&["doc".to_string(), "type".to_string()]);
        assert!(s.doc && s.type_);
        assert!(!s.definition);
    }

    #[test]
    fn member_filters_are_exact_and_do_not_change_card_sections() {
        let mut card = method_card();
        card.members = vec![
            SymbolMember::metadata("Артикул", "Реквизит", "attribute", Some("Строка".into())),
            SymbolMember::callable(
                "артикул",
                "Метод",
                "method",
                "артикул()",
                SymbolMemberOrigin::Module,
            ),
            SymbolMember::metadata("Цена", "Реквизит", "attribute", Some("Число".into())),
        ];

        filter_members(&mut card, Some("method"), Some("АРТИКУЛ"));
        assert_eq!(card.members.len(), 1);
        assert_eq!(card.members[0].member_kind, "method");
        assert_eq!(card.signature.as_deref(), Some("Функция Сложить(А, Б) Экспорт"));
        assert!(card.doc.is_some(), "member filters must not change legacy include sections");
    }

    /// Ветви члена пересекаются, поэтому объединение публикуется как `anyOf`.
    ///
    /// Положительный контроль встроен в саму проверку: типизированный член обязан
    /// удовлетворять И типизированной ветви, И «простой» — именно поэтому `oneOf`, требующий
    /// ровно одного совпадения, отверг бы карточку целиком.
    #[test]
    fn the_member_union_is_published_as_any_of_because_its_branches_overlap() {
        let schema = symbol_info_output_schema();
        let member = serde_json::to_value(&*schema)
            .expect("schema serializes")
            .to_string()
            .contains("\"member_kind\"");
        assert!(member, "в схеме нет членов — проверять нечего");

        let defs = schema["$defs"].as_object().expect("$defs");
        let union = defs
            .values()
            .find(|value| {
                value
                    .get("anyOf")
                    .or_else(|| value.get("oneOf"))
                    .and_then(Value::as_array)
                    .is_some_and(|branches| {
                        branches.iter().any(|branch| {
                            required_keys(branch, defs).iter().any(|key| key == "type_variants")
                        })
                    })
            })
            .expect("объединение членов опубликовано");

        assert!(union.get("anyOf").is_some(), "ветви пересекаются, oneOf отверг бы ответ");

        let branches = union["anyOf"].as_array().expect("ветви");
        let typed = json!({
            "name": "Реквизит1",
            "kind": "Реквизит",
            "member_kind": "attribute",
            "origin": "metadata",
            "availability": {"context_status": "not_evaluated"},
            "type": "Строка",
            "type_variants": [],
        });
        let matching = branches
            .iter()
            .filter(|branch| required_keys(branch, defs).iter().all(|key| typed.get(key).is_some()))
            .count();
        assert!(matching >= 2, "типизированный член обязан подходить и к простой ветви");
    }

    /// Имена обязательных полей ветви, с раскрытием `$ref`.
    fn required_keys(branch: &Value, defs: &Map<String, Value>) -> Vec<String> {
        let branch = branch
            .get("$ref")
            .and_then(Value::as_str)
            .and_then(|reference| reference.strip_prefix("#/$defs/"))
            .and_then(|name| defs.get(name))
            .unwrap_or(branch);
        branch
            .get("required")
            .and_then(Value::as_array)
            .map(|keys| keys.iter().filter_map(Value::as_str).map(str::to_owned).collect())
            .unwrap_or_default()
    }

    /// Член, названный только видом, — законная ветвь опубликованной схемы.
    ///
    /// Табличную часть не описывает ни тип, ни сигнатура, и такой член приходит в
    /// карточке обычного справочника. Схема, перечисляющая только типизированную и
    /// вызываемую ветви, заставляет валидирующего клиента отвергнуть ВЕСЬ ответ.
    #[test]
    fn a_member_named_only_by_its_kind_is_a_published_branch() {
        let mut card = method_card();
        card.members =
            vec![SymbolMember::metadata("Товары", "ТабличнаяЧасть", "tabular_section", None)];
        let roots = stand_roots();
        let body = render_card(&card, None, DEFAULT_TOP_MODULES, 6000, &stamp(&roots))
            .structured_content
            .expect("card structuredContent");

        let member = &body["members"][0];
        assert!(
            member.get("type").is_none() && member.get("signature").is_none(),
            "член без типа и без сигнатуры — тот самый вход, на котором ветвь нужна: {member}"
        );
        serde_json::from_value::<SymbolInfoOutput>(body.clone()).expect("card output schema shape");
    }

    #[test]
    fn loading_is_a_versioned_symbol_info_output_branch() {
        let report = crate::diagnostics_state::StatusReport {
            state: "loading",
            generation: 2,
            files: None,
            unread_files: None,
            reload: "none",
            error: None,
            elapsed_ms: Some(20),
            watch: None,
        };
        let body = loading(&report).structured_content.expect("loading structuredContent");

        serde_json::from_value::<SymbolInfoOutput>(body.clone())
            .expect("loading output schema shape");
        assert_eq!(body["schema_version"], "1");
        assert_eq!(body["status"], "loading");
    }

    #[test]
    fn render_card_without_graph_stamps_usages_unavailable() {
        let card = method_card();
        let roots = stand_roots();
        let result = render_card(&card, None, DEFAULT_TOP_MODULES, 6000, &stamp(&roots));
        let body = result.structured_content.unwrap();
        serde_json::from_value::<SymbolInfoOutput>(body.clone()).expect("ok output schema shape");
        assert_eq!(body["kind"], "function");
        assert_eq!(body["container"]["context"], "Сервер");
        assert_eq!(body["usages_unavailable"], "graph indexing");
        assert!(body.get("usages").is_none());
        // A graph that has not finished indexing is `index_building` — the SAME code the
        // miss shape reports for the same condition, so a consumer driving retries off the
        // code is not told two different things about one state.
        assert_eq!(body["freshness"]["completeness"]["status"], "partial");
        assert_eq!(body["freshness"]["completeness"]["reasons"][0]["code"], "index_building");
    }

    /// The card keeps its legacy 1-based `definition.line` AND gains the contract location:
    /// both are asserted together, because dropping either is the failure this step exists
    /// to prevent.
    #[test]
    fn a_card_carries_both_the_old_line_and_the_new_location() {
        let card = method_card();
        let roots = stand_roots();
        let body = render_card(&card, None, DEFAULT_TOP_MODULES, 6000, &stamp(&roots))
            .structured_content
            .unwrap();

        assert_eq!(body["definition"]["line"], 3);
        assert_eq!(body["definition"]["path"], "/ws/src/cf/CommonModules/МойМодуль/Ext/Module.bsl");

        // The tool envelope and the location slab have independent versions.
        assert_eq!(body["schema_version"], "1");
        assert_eq!(body["status"], "ok");
        let location = &body["definitions"][0]["location"];
        assert_eq!(location["root_id"], "");
        assert_eq!(location["path"], "CommonModules/МойМодуль/Ext/Module.bsl");
        assert_eq!(location["position_encoding"], "utf-16");
        // 0-based against the 1-based `definition.line` above: the same declaration.
        assert_eq!(location["range"]["start_line"], 2);
        assert_eq!(location["enclosing_range"]["end_line"], 6);
        assert_eq!(location["module"]["name"], "МойМодуль");
        assert_eq!(body["freshness"]["source"], "resident");
        assert_eq!(body["freshness"]["revision"], 7);
        assert_eq!(body["freshness"]["topology_fingerprint"], "0a1b2c3d4e5f6071");
    }

    /// Without the root table there is no pair to publish — and the answer says which,
    /// instead of omitting the location and letting it read as "no definition".
    #[test]
    fn a_card_without_roots_names_the_reason() {
        let card = method_card();
        let stamp =
            ResidentStamp { roots: None, revision: 7, topology: 1, stale: false, unread_files: 0 };
        let body =
            render_card(&card, None, DEFAULT_TOP_MODULES, 6000, &stamp).structured_content.unwrap();

        assert_eq!(body["definitions"][0]["location_unavailable"], "roots_unavailable");
        assert!(body["definitions"][0].get("location").is_none());
    }

    #[test]
    fn card_members_truncate_under_budget() {
        let mut card = method_card();
        card.kind = "metadata object";
        card.members = (0..200)
            .rev()
            .map(|i| {
                SymbolMember::metadata(
                    format!("Реквизит{i:03}"),
                    "Реквизит",
                    "attribute",
                    Some("Строка".to_string()),
                )
            })
            .collect();
        card.doc = Some("Документация".repeat(2_000));
        card.definition.as_mut().unwrap().snippet = Some("Фрагмент".repeat(2_000));
        let roots = stand_roots();
        let first = render_card(&card, None, DEFAULT_TOP_MODULES, 1_000, &stamp(&roots));
        let second = render_card(&card, None, DEFAULT_TOP_MODULES, 1_000, &stamp(&roots));
        assert!(serialized_bytes(&first) <= 4_000);
        let body = first.structured_content.unwrap();
        serde_json::from_value::<SymbolInfoOutput>(body.clone())
            .expect("truncated output schema shape");
        assert_eq!(body["truncated"], true);
        let members = body["members"].as_array().unwrap();
        assert!(members.len() < 200 && !members.is_empty(), "{body}");
        assert_eq!(body, second.structured_content.unwrap(), "same input keeps the same prefix");
        assert!(members
            .windows(2)
            .all(|pair| { pair[0]["name"].as_str().unwrap() < pair[1]["name"].as_str().unwrap() }));

        let text = first.content[0].as_text().expect("JSON text mirror").text.as_str();
        assert_eq!(serde_json::from_str::<Value>(text).unwrap(), body);

        let tiny = render_card(&card, None, DEFAULT_TOP_MODULES, 1, &stamp(&roots));
        let tiny_body = tiny.structured_content.unwrap();
        assert_eq!(
            tiny_body
                .as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect::<std::collections::BTreeSet<_>>(),
            ["budget_hint", "schema_version", "status", "symbol", "truncated"]
                .into_iter()
                .map(str::to_owned)
                .collect()
        );
        serde_json::from_value::<SymbolInfoOutput>(tiny_body).expect("minimum output schema shape");
    }

    #[test]
    fn member_contract_keeps_sources_and_type_signature_branches_separate() {
        let mut card = method_card();
        card.kind = "metadata object";
        let mut metadata = SymbolMember::metadata(
            "Состояние",
            "Реквизит",
            "attribute",
            Some("Строка".to_string()),
        );
        metadata.type_variants = vec![
            SymbolTypeVariant {
                presentation: "Одинаковое представление".to_string(),
                technical_name: Some("СправочникСсылка.А".to_string()),
                resolution: "static",
                reason: None,
            },
            SymbolTypeVariant {
                presentation: "Одинаковое представление".to_string(),
                technical_name: Some("СправочникСсылка.Б".to_string()),
                resolution: "static",
                reason: None,
            },
        ];
        let mut module = SymbolMember::metadata(
            "Состояние",
            "Переменная",
            "property",
            Some("Неизвестно".to_string()),
        );
        module.origin = SymbolMemberOrigin::Module;
        module.source_extension = Some("РасширениеА".to_string());
        module.availability = SymbolMemberAvailability {
            contexts: None,
            context_status: SymbolMemberContextStatus::Unknown,
            reason: Some("module_context_unknown".to_string()),
        };
        let callable = SymbolMember::callable(
            "Состояние",
            "Метод",
            "method",
            "Состояние()",
            SymbolMemberOrigin::Platform,
        );
        card.members = vec![metadata, module, callable];

        let body = card_to_value(&card);
        let members = body["members"].as_array().unwrap();
        assert_eq!(members.len(), 3, "same-name source candidates must not collapse");
        let metadata = members.iter().find(|member| member["origin"] == "metadata").unwrap();
        let module = members.iter().find(|member| member["origin"] == "module").unwrap();
        let callable = members.iter().find(|member| member["origin"] == "platform").unwrap();
        assert_eq!(metadata["availability"]["context_status"], "not_evaluated");
        assert!(metadata["type_variants"].is_array());
        assert_eq!(metadata["type_variants"].as_array().unwrap().len(), 2);
        assert_ne!(
            metadata["type_variants"][0]["technical_name"],
            metadata["type_variants"][1]["technical_name"]
        );
        assert_eq!(module["member_kind"], "property");
        assert_eq!(module["source_extension"], "РасширениеА");
        assert!(module["availability"]["contexts"].is_null());
        assert_eq!(module["availability"]["context_status"], "unknown");
        assert_eq!(module["availability"]["reason"], "module_context_unknown");
        assert_eq!(callable["signature"]["presentation"], "Состояние()");
        assert!(callable.get("type").is_none());
        assert!(callable.get("type_variants").is_none());
    }

    /// A miss carries the envelope, and an empty list says which source could
    /// not be consulted.
    ///
    /// The test this replaces asked for a name nothing could ever hold, so it
    /// stayed green whether the miss consulted five sources or none — the shape
    /// it checked was the shape of "no candidates", not of "no candidates and
    /// here is why".
    #[test]
    fn a_miss_names_the_source_it_could_not_consult() {
        let roots = stand_roots();
        let analysis = ide::Analysis::new();
        let found = ide::NameLookupResult {
            candidates: Vec::new(),
            total: 0,
            total_exact: true,
            truncated: false,
            providers: vec![
                ide::ProviderReport {
                    provider: ide::ProviderId::Platform,
                    state: ide::ProviderState::Answered,
                },
                ide::ProviderReport {
                    provider: ide::ProviderId::Graph,
                    state: ide::ProviderState::NotReady,
                },
            ],
        };
        let answer = crate::tools::name_answer::NameAnswer::render(
            analysis.database(),
            Some(&roots),
            &found,
        );

        let body =
            render_not_found("НетТакого.Метод", answer, &stamp(&roots)).structured_content.unwrap();

        serde_json::from_value::<SymbolInfoOutput>(body.clone())
            .expect("not_found output schema shape");
        assert_eq!(body["resolved"], false);
        assert_eq!(body["symbol"], "НетТакого.Метод");
        assert!(body["candidates"].as_array().unwrap().is_empty());
        assert_eq!(body["providers"][1]["provider"], "graph");
        assert_eq!(body["providers"][1]["state"], "not_ready");
        // The miss shape is assembled by its own function, so it is the one that would
        // silently ship without an envelope.
        assert_eq!(body["freshness"]["source"], "resident");
        assert_eq!(body["freshness"]["completeness"]["reasons"][0]["code"], "index_building");
    }

    /// И16 where the graph's verdict is an INPUT: a resident miss is answered by the
    /// platform while the graph has not answered.
    ///
    /// The claim used to be gated end to end, by waiting for the resident and hoping the
    /// graph's own background build had not published yet — two builds racing, and the
    /// answer decided by how fast the machine was. Here the one thing the claim is ABOUT is
    /// stated, exactly as the handler states it: whether the graph can answer is the host's
    /// fact, so it hands `lookup_names` a source that says so. Everything else is live —
    /// the platform candidate below comes from the real dictionary run over a real
    /// database, not from a value put there by this test.
    ///
    /// Its neighbour above renders a lookup assembled by hand and so pins the ENVELOPE; this
    /// one pins that the platform still answers when the graph does not, which is the
    /// regression И16 was written for: `symbol_info` used to answer a resident miss from the
    /// graph alone and returned an empty list for a platform member the graph never held.
    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn a_resident_miss_is_answered_by_the_platform_while_the_graph_is_not_ready() {
        let roots = stand_roots();
        // A real workspace behind a real database: the dictionary runs its module and
        // metadata sources over it, so the platform's answer below is one it had to compete
        // for rather than the only one on offer.
        let dir = tempfile::tempdir().unwrap();
        crate::graph::test_support::sample_workspace(dir.path());
        let project = crate::graph::ProjectSnapshot::load(dir.path());
        let files = crate::graph::input::enumerate_bsl_files(&project);
        let source_root = crate::graph::build_source_root(&files);
        let loaded = crate::graph::db_for_files(&source_root, &files, &project.configs, None);
        let analysis = ide::Analysis::from_database(loaded.db);
        let db = analysis.database();

        // One letter short of `СтрНайти`: a name the platform answers by prefix and nothing
        // else can hold, so an empty list would mean the platform was not consulted.
        let source = crate::graph_query::GraphNameSource::absent(ide::ProviderState::NotReady);
        let query = ide::NameQuery::new("СтрНайт", DEFAULT_CANDIDATE_LIMIT);
        let found = ide::lookup_names(db, &query, &[&source]);
        let answer = crate::tools::name_answer::NameAnswer::render(db, Some(&roots), &found);
        let body = render_not_found("СтрНайт", answer, &stamp(&roots)).structured_content.unwrap();

        assert_eq!(body["resolved"], false, "{body}");
        let categories: Vec<&str> = body["candidates"]
            .as_array()
            .unwrap_or_else(|| panic!("the miss carries candidates: {body}"))
            .iter()
            .filter_map(|c| c["category"].as_str())
            .collect();
        assert!(
            categories.contains(&"platform_member"),
            "the platform answered nothing on a miss: {body}",
        );

        let graph = body["providers"]
            .as_array()
            .unwrap_or_else(|| panic!("the miss names its providers: {body}"))
            .iter()
            .find(|p| p["provider"] == "graph")
            .unwrap_or_else(|| panic!("`graph` is named: {body}"))
            .clone();
        assert_eq!(graph["state"], "not_ready", "{body}");
        let reasons: Vec<&str> = body["freshness"]["completeness"]["reasons"]
            .as_array()
            .map(|r| r.iter().filter_map(|r| r["code"].as_str()).collect())
            .unwrap_or_default();
        assert!(
            reasons.contains(&"index_building"),
            "an unconsulted source makes the emptiness incomplete: {body}",
        );
    }

    #[test]
    fn candidates_are_ambiguous_and_database_failures_are_mcp_errors() {
        let roots = stand_roots();
        let answer = crate::tools::name_answer::NameAnswer {
            candidates: vec![json!({"display": "Кандидат"})],
            providers: Vec::new(),
            total: 1,
            total_exact: true,
            truncated: false,
            completeness: Completeness::complete(),
        };
        let body = render_not_found("Кандидат", answer, &stamp(&roots)).structured_content.unwrap();
        serde_json::from_value::<SymbolInfoOutput>(body.clone())
            .expect("ambiguous output schema shape");
        assert_eq!(body["status"], "ambiguous");

        let error = database_error("broken cache");
        assert_eq!(error.code.0, -32603);
        assert!(error.message.contains("broken cache"));
    }
}
