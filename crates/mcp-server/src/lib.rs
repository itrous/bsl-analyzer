mod baseline;
pub mod broker;
mod cache;
mod cancel;
mod change_hub;
pub mod contract;
mod diagnostics_state;
mod drift_classify;
mod graph;
mod graph_db;
mod graph_query;
mod http;
#[cfg(test)]
mod inventory;
pub mod project;
mod state;
mod tasks;
mod tools;
#[cfg(test)]
mod walk_probe;
mod workspace_lease;

pub use baseline::{
    resolve_project_baseline_diagnostics, BaselineConfigDiagnostics, BaselineResolutionSummary,
};
pub use cache::WorkspaceCacheLayout;
pub use graph_db::{
    read_source_root_scoped_sqlite_method_call_digest, read_sqlite_method_call_digest,
};
pub use graph_query::{GraphDb, GraphDbContextProvider};
pub use http::{serve_http, wildcard_allowed_hosts, MAX_HTTP_REQUEST_BODY_BYTES};
pub use state::WorkspaceInitError;
use state::WorkspaceSearchMode;
pub use state::{OnecConnection, SharedState};
pub use tools::platform::{
    build_reference_documents, reference_documents_fingerprint, REFERENCE_DOCUMENT_SCHEMA_VERSION,
};

pub async fn serve_stdio(server: McpServer) -> anyhow::Result<()> {
    serve_stream(server, rmcp::transport::stdio()).await
}

/// Serve one MCP session over an arbitrary bidirectional transport. The transport
/// carries framed JSON-RPC; MCP handling is identical whether the bytes come from
/// stdio (`serve_stdio`) or a local socket (the broker). This is the single seam
/// stdio and socket serving share.
pub async fn serve_stream<T, A>(server: McpServer, transport: T) -> anyhow::Result<()>
where
    T: rmcp::transport::IntoTransport<rmcp::RoleServer, std::io::Error, A>,
{
    use rmcp::ServiceExt;
    let session = server.serve(transport).await.map_err(|e| anyhow::anyhow!("{e}"))?;
    session.waiting().await.map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(())
}

use crate::graph::GraphStatus;
use std::collections::BTreeSet;

use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResponse, CallToolResult, ListResourcesResult, PaginatedRequestParams,
    ReadResourceRequestParams, ReadResourceResponse, ReadResourceResult, Resource,
    ResourceContents, ServerCapabilities, ServerInfo,
};
use rmcp::service::RequestContext;
use rmcp::{tool, tool_handler, tool_router, ErrorData as McpError, RoleServer, ServerHandler};
use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpProfile {
    Workspace,
    Reference,
}

impl McpProfile {
    /// Stable lowercase tag for the profile. Used as part of the broker backend
    /// identity, so it must stay byte-stable across releases.
    pub fn as_str(self) -> &'static str {
        match self {
            McpProfile::Workspace => "workspace",
            McpProfile::Reference => "reference",
        }
    }
}

#[derive(Deserialize, JsonSchema)]
struct MetadataParams {
    /// info | tree | object | form | status.
    action: String,
    /// `tree`: case-insensitive substring to narrow the returned tree (optional).
    filter: Option<String>,
    /// `tree` in infobase mode: metadata collection, e.g. `Справочники`/`Documents`.
    meta_type: Option<String>,
    /// `tree` in infobase mode: case-insensitive object name/synonym substring.
    name_mask: Option<String>,
    /// `tree` in infobase mode: maximum returned objects (default 100, max 1000).
    max_items: Option<u32>,
    /// Mode-dependent metadata object type. Use a singular analyzer type for `mode=source`
    /// and `mode=auto` without `connection`, e.g. `Документ`/`Справочник`/`ОбщийМодуль`.
    /// Use a plural live-service collection for `mode=infobase` and `mode=auto` with
    /// `connection`, e.g. `Документы`/`Справочники`. Forms are source-only. Required for
    /// `object` and `form`; values are passed through without singular/plural conversion.
    object_type: Option<String>,
    /// Metadata object name, e.g. `ЗаказКлиента`. Required for `object`; for `form` it
    /// selects the owner object (omit for a configuration-level common form).
    object_name: Option<String>,
    /// `form`: managed-form name (optional; omit for the object's default form).
    form_name: Option<String>,
    /// `tree` (filtered listing): output budget in tokens (~4 chars each); an over-budget
    /// listing is truncated at a line boundary with a continuation note (default 6000).
    max_output_tokens: Option<usize>,
    /// Named live 1C connection for `mode=infobase` (optional).
    connection: Option<String>,
    /// auto | source | infobase (default auto).
    mode: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct LegacySearchQueryParams {
    /// Free-text query.
    #[schemars(length(min = 1))]
    #[serde(deserialize_with = "deserialize_non_empty_string")]
    query: String,
    /// Cap on returned hits (default 10, max 50).
    limit: Option<usize>,
    /// Output budget in tokens (~4 chars each) for the text listing and the structured hits
    /// together; over-budget results are truncated at a hit boundary with a note telling you to
    /// raise `limit` or narrow the query, and `budget_exhausted: true` (default 6000).
    max_output_tokens: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
struct LegacySearchStatusParams {}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct DocsSearchParams {
    /// Free-text query for the selected search action.
    #[schemars(length(min = 1))]
    #[serde(deserialize_with = "deserialize_non_empty_string")]
    query: String,
    /// Cap on returned platform-reference hits (default 10, max 50).
    limit: Option<usize>,
    /// Output budget in tokens (~4 chars each) for text and structured content together.
    max_output_tokens: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ListPlatformParams {
    /// Optional platform entity kind: type, method, property, constructor, or global_function.
    kind: Option<tools::platform::PlatformReferenceKind>,
    /// Optional case-insensitive substring of the Russian or English platform entity name.
    name: Option<String>,
    /// Output budget in tokens (~4 chars each) for text and structured content together.
    max_output_tokens: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case")]
#[schemars(transform = document_search_action)]
enum WorkspaceSearchParams {
    SearchCode(LegacySearchQueryParams),
    Status(LegacySearchStatusParams),
    ListPlatform(ListPlatformParams),
    FindDocs(DocsSearchParams),
    SearchDocs(DocsSearchParams),
}

#[derive(Deserialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case")]
#[schemars(transform = document_search_action)]
enum ReferenceSearchParams {
    FindDocs(LegacySearchQueryParams),
    SearchDocs(LegacySearchQueryParams),
    Status(LegacySearchStatusParams),
    ListPlatform(ListPlatformParams),
}

fn document_search_action(schema: &mut schemars::Schema) {
    contract::ensure_object_root(schema.ensure_object());

    fn visit(object: &mut serde_json::Map<String, serde_json::Value>) {
        if let Some(properties) =
            object.get_mut("properties").and_then(serde_json::Value::as_object_mut)
        {
            if let Some(action) =
                properties.get_mut("action").and_then(serde_json::Value::as_object_mut)
            {
                action.insert(
                    "description".to_string(),
                    serde_json::Value::String(
                        "Requested search, platform-listing, or lifecycle action.".to_string(),
                    ),
                );
            }
        }
        for value in object.values_mut() {
            match value {
                serde_json::Value::Object(child) => visit(child),
                serde_json::Value::Array(values) => {
                    for value in values {
                        if let serde_json::Value::Object(child) = value {
                            visit(child);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    visit(schema.ensure_object());
}

enum SearchCommand {
    SearchCode(LegacySearchQueryParams),
    Status,
    ListPlatform(ListPlatformParams),
    FindDocs { query: String, limit: Option<usize>, max_output_tokens: Option<usize> },
    SearchDocs { query: String, limit: Option<usize>, max_output_tokens: Option<usize> },
}

impl From<WorkspaceSearchParams> for SearchCommand {
    fn from(params: WorkspaceSearchParams) -> Self {
        match params {
            WorkspaceSearchParams::SearchCode(params) => Self::SearchCode(params),
            WorkspaceSearchParams::Status(_) => Self::Status,
            WorkspaceSearchParams::ListPlatform(params) => Self::ListPlatform(params),
            WorkspaceSearchParams::FindDocs(params) => Self::FindDocs {
                query: params.query,
                limit: params.limit,
                max_output_tokens: params.max_output_tokens,
            },
            WorkspaceSearchParams::SearchDocs(params) => Self::SearchDocs {
                query: params.query,
                limit: params.limit,
                max_output_tokens: params.max_output_tokens,
            },
        }
    }
}

impl From<ReferenceSearchParams> for SearchCommand {
    fn from(params: ReferenceSearchParams) -> Self {
        match params {
            ReferenceSearchParams::FindDocs(params) => Self::FindDocs {
                query: params.query,
                limit: params.limit,
                max_output_tokens: params.max_output_tokens,
            },
            ReferenceSearchParams::SearchDocs(params) => Self::SearchDocs {
                query: params.query,
                limit: params.limit,
                max_output_tokens: params.max_output_tokens,
            },
            ReferenceSearchParams::Status(_) => Self::Status,
            ReferenceSearchParams::ListPlatform(params) => Self::ListPlatform(params),
        }
    }
}

fn deserialize_non_empty_string<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    if value.is_empty() {
        return Err(serde::de::Error::custom("must not be empty"));
    }
    Ok(value)
}

#[derive(Deserialize, JsonSchema)]
struct SyntaxHelpByNameParams {
    /// Platform member name to look up, e.g. `СтрНайти` or a type method.
    name: String,
    /// Owning platform type when `name` is a member of a specific type (optional).
    type_name: Option<String>,
    /// Output budget in tokens (~4 chars each) covering the compatibility Markdown and the
    /// structured card together: the Markdown is truncated at a line boundary with a note
    /// pointing at the single-member lookup, and the card's listings take what is left
    /// (`budget_exhausted` says when they were cut). Default 6000.
    max_output_tokens: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SyntaxHelpByReferenceIdParams {
    /// Exact stable identifier returned by `search(action="list_platform")`.
    #[schemars(length(min = 1))]
    #[serde(deserialize_with = "deserialize_non_empty_string")]
    reference_id: String,
    /// Output budget in tokens (~4 chars each) for Markdown and structured content together.
    max_output_tokens: Option<usize>,
}

#[derive(JsonSchema)]
#[serde(untagged)]
#[schemars(transform = syntax_help_xor_schema)]
#[allow(dead_code)] // schema-only mirror; runtime deserialization enforces the same XOR below
enum SyntaxHelpParamsSchema {
    ByName(SyntaxHelpByNameParams),
    ByReferenceId(SyntaxHelpByReferenceIdParams),
}

enum SyntaxHelpParams {
    ByName(SyntaxHelpByNameParams),
    ByReferenceId(SyntaxHelpByReferenceIdParams),
}

impl<'de> Deserialize<'de> for SyntaxHelpParams {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        let object = value
            .as_object()
            .ok_or_else(|| serde::de::Error::custom("syntax_help params must be an object"))?;
        if object.contains_key("reference_id") {
            if object.contains_key("name") || object.contains_key("type_name") {
                return Err(serde::de::Error::custom(
                    "reference_id cannot be combined with name or type_name",
                ));
            }
            serde_json::from_value(value).map(Self::ByReferenceId).map_err(serde::de::Error::custom)
        } else {
            serde_json::from_value(value).map(Self::ByName).map_err(serde::de::Error::custom)
        }
    }
}

impl JsonSchema for SyntaxHelpParams {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "SyntaxHelpParams".into()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        // Inlined, not referenced: this schema IS the published inputSchema, and a root
        // consisting of a `$ref` says nothing about itself — the specification types the
        // root as an object, and a client is not required to follow the reference to
        // learn that.
        <SyntaxHelpParamsSchema as JsonSchema>::json_schema(generator)
    }
}

fn syntax_help_xor_schema(schema: &mut schemars::Schema) {
    let object = schema.ensure_object();
    let Some(mut branches) = object
        .remove("anyOf")
        .or_else(|| object.remove("oneOf"))
        .and_then(|value| value.as_array().cloned())
    else {
        return;
    };
    if let Some(legacy_branch) = branches.first_mut().and_then(serde_json::Value::as_object_mut) {
        legacy_branch.insert("not".to_owned(), serde_json::json!({"required": ["reference_id"]}));
    }
    object.insert("oneOf".to_owned(), serde_json::Value::Array(branches));
    contract::ensure_object_root(object);
}

#[derive(Deserialize, JsonSchema)]
struct QueryParams {
    /// validate | execute | schema.
    action: String,
    /// SDBL text — required for `validate`/`execute`, omitted for `schema`.
    query: Option<String>,
    /// `execute`: cap on returned rows (optional).
    limit: Option<u32>,
    /// `execute`: named SDBL query parameters (`&Param` → value) (optional).
    parameters: Option<std::collections::HashMap<String, serde_json::Value>>,
    /// Named live 1C connection (optional when only one/default connection exists).
    connection: Option<String>,
    /// `validate`: the configuration root the query is meant for — `""` for the configuration,
    /// an extension's id otherwise. An assertion about context, not a narrowing knob: an id
    /// that is not registered fails the call, a registered one is echoed back in `context`.
    root_id: Option<String>,
    /// `execute`: output budget in tokens (~4 chars each) on top of the `limit` row cap —
    /// `limit` bounds how many rows come back, nothing bounds how wide they are. An
    /// over-budget table is truncated at a row boundary with a note (default 6000); when the
    /// row cap fired too, the note says raising the budget alone will not help.
    max_output_tokens: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
struct ExecuteParams {
    /// check | run | eval.
    action: String,
    /// BSL source to `check`/`run`, or the single expression to `eval`.
    code: String,
    /// Named live 1C connection (optional when only one/default connection exists).
    connection: Option<String>,
    /// Output budget in tokens (~4 chars each); over-budget output (a `run` context block, an
    /// evaluated value, a long syntax-error listing) is truncated with a note (default 6000).
    max_output_tokens: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
struct EventLogParams {
    /// Lower time bound (inclusive), ISO-8601, e.g. `2026-07-05T00:00:00` or `2026-07-05`.
    date_from: Option<String>,
    /// Upper time bound (inclusive), ISO-8601.
    date_to: Option<String>,
    /// Severity: Информация/Предупреждение/Ошибка/Примечание or Information/Warning/Error/Note.
    level: Option<String>,
    /// Infobase user name (deleted users can only be matched by name).
    user: Option<String>,
    /// Event name, e.g. `_$Session$_.Authentication` or a metadata event like `_$Data$_.Post`.
    event: Option<String>,
    /// Full metadata name to filter by, e.g. `Документ.ЗаказКлиента`.
    metadata: Option<String>,
    /// Case-insensitive substring filter over the comment/data columns, applied AFTER the
    /// platform read — so it narrows the already-`limit`-capped newest window, it does not
    /// scan the whole log. Widen `limit` if a match may lie deeper.
    contains: Option<String>,
    /// Max records (newest first), default 100, capped at 1000.
    limit: Option<u32>,
    /// Named live 1C connection (optional when only one/default connection exists).
    connection: Option<String>,
    /// Output budget in tokens (~4 chars each) on top of the `limit` record cap — `limit`
    /// counts records, it does not bound their size. An over-budget read drops the oldest
    /// records, flags `budget_exhausted: true` and carries a `budget_hint` (default 6000);
    /// when the record cap fired too, the hint says raising the budget alone will not help.
    /// In the response, `returned` counts the records actually delivered and `total` the ones
    /// the platform read for this `limit` window — neither is the whole matching population,
    /// which the platform never reports.
    max_output_tokens: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
struct GraphParams {
    /// overview | schema | status | node | source | neighbors | callers | callees | resolve
    action: String,
    /// Durable node id (required for node/neighbors/callers/callees).
    id: Option<String>,
    /// Imprecise lookup string (required for `resolve`): wrong casing, a bare method/object
    /// name, or a partial id.
    query: Option<String>,
    /// Durable node ids (required for `source`).
    #[serde(default)]
    ids: Vec<String>,
    /// Output budget in tokens (~4 chars each) for source-bearing actions: `source`
    /// (default 4000) and `node`/`neighbors` at `detail=bodies` (default 6000). When the
    /// body output is truncated the response carries `budget_exhausted: true`.
    max_output_tokens: Option<usize>,
    /// names | signatures | bodies (default: signatures).
    detail: Option<String>,
    /// in | out | both — only for `neighbors` (default: in).
    dir: Option<String>,
    /// Traversal depth for neighbors (default: 1).
    depth: Option<usize>,
    /// Server-side cap on returned neighbour nodes (default: 50).
    max_nodes: Option<usize>,
    /// Keep only edges with these provenances (resolved/inferred/visibility_blocked/unresolved).
    #[serde(default)]
    provenance: Vec<String>,
    /// Keep only edges of these kinds (call/manager_creates/manager_access/query_ref/
    /// contains/data_binding) — lets metadata-impact queries isolate e.g. only `query_ref`.
    #[serde(default)]
    edge_kinds: Vec<String>,
    /// How many top-centrality methods to include in `overview` (default: 20).
    top: Option<usize>,
    /// Ask each edge where its call is written — for `neighbors`/`callers`/`callees`
    /// (default: false). Off, an edge carries no `call_site*` key at all, which is how an
    /// edge nobody asked about is told apart from one that has no place.
    call_sites: Option<bool>,
    /// Cap on places per edge when `call_sites` is on (default: 20, max: 200). What the cap
    /// cuts is declared: `call_sites_total` counts the places before any are shown.
    max_call_sites: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
#[schemars(transform = symbol_info_input_schema)]
struct SymbolInfoParams {
    /// Qualified name of the symbol (primary input): a common-module method
    /// (`ОбщегоНазначения.ЗначениеРеквизитаОбъекта`), a metadata object (`Справочник.Товары`)
    /// or its attribute (`Справочник.Товары.Артикул`), an object/manager module method
    /// (`Документ.ЗаказКлиента.Провести`), or a platform member (`СтрНайти`, `Массив.Добавить`).
    /// Case-insensitive; the MdoType keyword accepts singular or plural, RU or EN.
    symbol: Option<String>,
    /// `path`: the source root that path is spelled against, as carried by every `search`
    /// code hit. Omit for a path already spelled against the workspace; `""` names the
    /// configuration. An extension repeats the configuration's layout, so the same relative
    /// path exists under several roots and the pair is what names one file.
    root_id: Option<String>,
    /// Positional fallback for locals/parameters that have no qualified name: absolute or
    /// workspace-relative `.bsl` path, or a path relative to `root_id`. Requires `line`.
    path: Option<String>,
    /// `path`: 0-based line of the symbol occurrence.
    line: Option<u32>,
    /// `path`: 0-based offset within the line of the symbol occurrence, counted in UTF-16
    /// units — the unit every column this server publishes is counted in, so a
    /// `range.start_character` out of any answer goes straight back in (default 0).
    column: Option<u32>,
    /// Card sections to include: any of `definition` | `type` | `doc`. Empty = all. `usages`
    /// is always a summary and is added when the call graph is ready.
    #[serde(default)]
    include: Vec<String>,
    /// Keep only members of this machine kind; does not affect symbol resolution or `include`.
    member_kind: Option<SymbolMemberKindFilter>,
    /// Keep only members with this exact case-insensitive name.
    member_name: Option<String>,
    /// Type/label language: `ru` (default) or `en`.
    locale: Option<String>,
    /// Output budget in tokens (~4 chars each); an over-budget member list is trimmed and the
    /// response carries `truncated: true` with a `budget_hint` (default 6000).
    max_output_tokens: Option<usize>,
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum SymbolMemberKindFilter {
    Attribute,
    TabularSection,
    Method,
    Variable,
    Property,
    FormAttribute,
    FormElement,
    Handler,
}

impl SymbolMemberKindFilter {
    fn as_str(self) -> &'static str {
        match self {
            Self::Attribute => "attribute",
            Self::TabularSection => "tabular_section",
            Self::Method => "method",
            Self::Variable => "variable",
            Self::Property => "property",
            Self::FormAttribute => "form_attribute",
            Self::FormElement => "form_element",
            Self::Handler => "handler",
        }
    }
}

fn symbol_info_input_schema(schema: &mut schemars::Schema) {
    schema.ensure_object().insert(
        "oneOf".to_owned(),
        serde_json::json!([
            {
                "required": ["symbol"],
                "not": {"anyOf": [
                    {"required": ["root_id"]},
                    {"required": ["path"]},
                    {"required": ["line"]},
                    {"required": ["column"]}
                ]}
            },
            {"required": ["path", "line"]}
        ]),
    );
}

#[derive(Deserialize, JsonSchema)]
struct ReferencesParams {
    /// Qualified name of the symbol whose references you want: a common-module method
    /// (`ОбщегоНазначения.ЗначениеРеквизитаОбъекта`), an object/manager module method
    /// (`Справочник.Товары.ОбновитьКэш`), a form-module method
    /// (`Документ.Заказ.Форма.ФормаДокумента.ПриОткрытии` — those need no `Экспорт`, since
    /// handlers never have it), or a short unique name. Case-insensitive. Any OTHER member
    /// declared without `Экспорт` has no qualified name — `symbol_info` reads the same
    /// spelling the same way — so reach it by `path` with `line_content`.
    symbol: Option<String>,
    /// `symbol`: which root to look for the DECLARATION in. The way out of
    /// `outcome: "ambiguous"`, when a configuration and an extension declare the same name.
    /// Not a filter on the answer — that is `area_root_id`.
    anchor_root_id: Option<String>,
    /// `path`: the source root that path is spelled against, as carried by every `search`
    /// code hit. Omit for a path already spelled against the workspace; `""` names the
    /// configuration.
    root_id: Option<String>,
    /// Anchor on a file for what no name addresses (a local, a parameter, a non-exported
    /// member): absolute or workspace-relative `.bsl` path, or a path relative to `root_id`.
    /// Needs `line_content` (the text of the line, checked against the file) or `line` (bare
    /// coordinates, taken on trust) — either alone will do, and together they narrow.
    path: Option<String>,
    /// `path`: 0-based line of the occurrence to anchor on.
    line: Option<u32>,
    /// `path`: 0-based offset within the line, counted in UTF-16 units — the unit every
    /// column this server publishes is counted in, so a `range.start_character` out of any
    /// answer goes straight back in. Defaults to 0 for a `path`+`line`
    /// anchor; beside `line_content` it has NO default and narrows only when you pass it —
    /// sending one picks the token under it and refuses when that token is not in your quote,
    /// which is the opposite of what the quote is for.
    column: Option<u32>,
    /// `path`: the text of the line the occurrence is on, as you read it — any substring that
    /// carries at least one whole identifier (a quote cutting a name in half certifies
    /// nothing and is refused by name). Checked against the file before anything is counted,
    /// so a file edited since you read it answers `outcome: "anchor_stale"` instead of a
    /// confident list about whatever token now stands at your coordinates. Makes `line`
    /// optional — and `line` beside it never CHOOSES: when the quote names one symbol the
    /// answer is that symbol wherever it moved (`anchor.relocated_from_line`), and when it
    /// names several the answer is `ambiguous` with `pointed_by_line` on the place your line
    /// stood at.
    line_content: Option<String>,
    /// Show only references from this root. Narrows the ANSWER, not the anchor: a symbol
    /// declared in the configuration and used from an extension takes `anchor_root_id: ""`
    /// with `area_root_id: "<extension>"`.
    area_root_id: Option<String>,
    /// Show only references under this root-relative directory prefix (e.g.
    /// `CommonModules/Продажи`). Combine with `area_root_id`: one relative path lives in
    /// every root that repeats the configuration's layout.
    area_path_prefix: Option<String>,
    /// Keep only these kinds: any of `declaration` | `call` | `write` | `read`. Empty = all.
    #[serde(default)]
    kinds: Vec<String>,
    /// Whether the declaration itself is one of the references (default: true).
    include_declaration: Option<bool>,
    /// Cap on returned references (default 50, max 500). `total` is counted before it, and
    /// the per-file histogram covers everything the limit hid.
    limit: Option<usize>,
    /// Cap on candidate files walked (default 2000, max 10000). Reaching it makes `total` a
    /// lower bound and sets `narrowing_comparable: false`.
    max_files: Option<usize>,
    /// Add a one-line `snippet` of source to every reference (default false). Decoration
    /// only: it is paid for out of whatever budget the answer leaves, so the references, the
    /// `total` and the per-file histogram are the same either way, and previews that did not
    /// fit are counted in `previews_omitted`.
    include_preview: Option<bool>,
    /// Output budget in tokens (~4 chars each), default 6000; a trimmed response says so in
    /// `freshness.completeness`.
    max_output_tokens: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
struct DiagnosticsParams {
    /// catalog | schema | status | file | workspace.
    action: String,
    /// `file`: absolute or workspace-relative `.bsl` path, or a path relative to `root_id`.
    path: Option<String>,
    /// `file`: the source root `path` is spelled against, as carried by every `search` code
    /// hit. Omit for a path already spelled against the workspace; `""` names the
    /// configuration.
    root_id: Option<String>,
    /// `catalog`: narrow to these codes. `file`: keep only these codes.
    #[serde(default)]
    codes: Vec<String>,
    /// `catalog`: ru | en (default ru) — title language.
    locale: Option<String>,
    /// `file`: inclusive severity floor error|warning|info|hint (default warning).
    min_severity: Option<String>,
    /// `file`: 0-based first line to include (optional).
    range_start: Option<usize>,
    /// `file`: 0-based last line to include (optional).
    range_end: Option<usize>,
    /// `file`: concise | detailed (default concise).
    detail: Option<String>,
    /// `file`: cap on returned findings (default 200).
    max_findings: Option<usize>,
    /// `workspace`: cap on files swept (default 1000).
    max_files: Option<usize>,
    /// `catalog`/`file`/`workspace`: output budget in tokens (~4 chars each), minimum 256;
    /// a truncated response carries `budget_exhausted: true` and a `budget_hint` on how to narrow it
    /// (tighten `codes`/`min_severity`/range or raise the budget). When omitted, no token
    /// budget applies — only the action's own count caps (`max_findings`/`max_files`).
    #[schemars(range(min = 256))]
    max_output_tokens: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
struct OutlineParams {
    /// Absolute or workspace-relative `.bsl` path, or a path relative to `root_id`.
    path: String,
    /// The source root `path` is spelled against, as carried by every `search` code hit. Omit
    /// for a path already spelled against the workspace; `""` names the configuration. An
    /// extension repeats the configuration's layout, so the same relative path exists under
    /// several roots and the pair is what names one file.
    root_id: Option<String>,
    /// `full` (default) — every region and declaration; `regions` — the region skeleton alone,
    /// for a module too big to read method by method.
    mode: Option<String>,
    /// Output budget in tokens (~4 chars each); an over-budget map stops partway through the
    /// file and the response carries `truncated: true` with a `budget_hint` (default 6000).
    max_output_tokens: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
struct ItsHelpParams {
    /// Natural-language question for the ITS expert help.
    question: String,
    /// Output budget in tokens (~4 chars each); a long answer is truncated at a line boundary
    /// with a continuation note (default 6000).
    max_output_tokens: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
struct DebugParams {
    /// attach | disconnect | set_breakpoint | remove_breakpoint | continue | step |
    /// wait_stop | stack_trace | locals | eval.
    action: String,
    /// `attach`: debugger host (required).
    host: Option<String>,
    /// `attach`: debugger port (default 1550).
    port: Option<u16>,
    /// `attach`: infobase name (required).
    infobase: Option<String>,
    /// `attach`: configuration source root (optional).
    config_root: Option<String>,
    /// `attach`: `[name, root]` pairs for loaded extensions (optional).
    #[serde(default)]
    extensions: Vec<[String; 2]>,
    /// `attach`: object-name patterns to auto-attach on connect (optional).
    #[serde(default)]
    auto_attach: Vec<String>,
    /// `set_breakpoint`/`remove_breakpoint`: target module id (required).
    module: Option<String>,
    /// `set_breakpoint`/`remove_breakpoint`: 1-based line (required).
    line: Option<u32>,
    /// `set_breakpoint`: conditional-breakpoint expression (optional).
    condition: Option<String>,
    /// `step`: `over`/`next`, `in`/`into`, or `out` (required).
    direction: Option<String>,
    /// `wait_stop`: max seconds to wait for a stop event (optional).
    timeout_secs: Option<u64>,
    /// `locals`/`eval`: stack frame level to evaluate in (optional, default top frame).
    stack_level: Option<u32>,
    /// `eval`: BSL expression to evaluate in the current stop (required).
    expression: Option<String>,
    /// Output budget in tokens (~4 chars each) for the state-reading actions `stack_trace`,
    /// `locals`, `wait_stop` and `eval`: a deep stack or a wide frame is truncated at a line
    /// boundary with a continuation note (default 6000).
    max_output_tokens: Option<usize>,
}

fn default_debug_port() -> u16 {
    1550
}

fn require<T>(val: Option<T>, field: &str, action: &str) -> Result<T, McpError> {
    val.ok_or_else(|| {
        McpError::invalid_params(format!("'{field}' is required for action '{action}'"), None)
    })
}

/// How the graph's lifecycle reads as a name-provider verdict.
///
/// Both entries to the name dictionary need it, and they must agree: an agent
/// told `not_ready` by one and `failed` by the other would draw opposite
/// conclusions about whether waiting helps.
fn graph_provider_state(status: &GraphStatus, has_snapshot: bool) -> ide::ProviderState {
    match status {
        GraphStatus::Ready { .. } if has_snapshot => ide::ProviderState::Answered,
        // Ready with no snapshot is a race, not a build failure: the answer is the
        // same as still-building — ask again.
        GraphStatus::Ready { .. } | GraphStatus::Idle | GraphStatus::Loading => {
            ide::ProviderState::NotReady
        }
        GraphStatus::Failed(_) => ide::ProviderState::Failed,
        GraphStatus::Disabled => ide::ProviderState::Unavailable,
    }
}

/// The verdict that travels with the EMPTY database when the resident did not
/// hand itself over.
///
/// Both inputs are here on purpose. The status seen before the read is the only
/// evidence when no read was attempted; the moment one was, it is stale — the
/// idle sweeper can evict the base between the two, and then the earlier
/// `answered` would describe tables that were never consulted as exhaustively
/// searched.
fn fallback_workspace_state<R>(
    before_read: ide::ProviderState,
    read: Option<&crate::diagnostics_state::ResidentOutcome<R>>,
) -> ide::ProviderState {
    match read {
        None => before_read,
        Some(outcome) => unread_resident_state(outcome),
    }
}

/// What a read that did NOT hand over the resident says about it.
///
/// Separate from the status taken before the read, and that separation is the
/// point: the idle sweeper can evict the base between the two, and reusing the
/// earlier `answered` would serve an EMPTY database under a verdict that says
/// every table was consulted — a proven zero over nothing at all.
fn unread_resident_state<R>(
    outcome: &crate::diagnostics_state::ResidentOutcome<R>,
) -> ide::ProviderState {
    use crate::diagnostics_state::ResidentOutcome;
    match outcome {
        // A read that succeeded is not this function's business; answering
        // `answered` here is what the caller must never be able to do.
        ResidentOutcome::Ready(..) | ResidentOutcome::Loading => ide::ProviderState::NotReady,
        ResidentOutcome::Disabled => ide::ProviderState::Unavailable,
        ResidentOutcome::Failed(_) => ide::ProviderState::Failed,
    }
}

#[cfg(test)]
mod resident_state_tests {
    use super::*;
    use crate::diagnostics_state::ResidentOutcome;

    /// The empty database served when a read hands nothing over must never be
    /// described as consulted. `answered` over empty tables is a proven zero
    /// about a workspace nobody looked at — the one verdict `providers` exists
    /// to make impossible.
    #[test]
    fn an_unread_resident_is_never_answered() {
        let outcomes: [ResidentOutcome<()>; 4] = [
            ResidentOutcome::Loading,
            ResidentOutcome::Disabled,
            ResidentOutcome::Failed("сборка упала".into()),
            // Ready reaches this path only when the caller misroutes it, and
            // even then the answer must not claim the tables were read.
            ResidentOutcome::Ready(
                (),
                crate::diagnostics_state::Freshness {
                    revision: 0,
                    stale: false,
                    reload: "",
                    topology: 0,
                },
            ),
        ];
        for outcome in &outcomes {
            assert_ne!(unread_resident_state(outcome), ide::ProviderState::Answered);
        }
    }

    /// The regression this decision exists to prevent: the status said `ready`,
    /// the read then found nothing to hand over, and the empty database went out
    /// under `answered`. The state before the read is evidence only while no
    /// read has happened.
    #[test]
    fn a_read_that_handed_nothing_over_overrides_the_status_before_it() {
        assert_eq!(
            fallback_workspace_state(
                ide::ProviderState::Answered,
                Some(&ResidentOutcome::<()>::Loading)
            ),
            ide::ProviderState::NotReady,
        );
        // And the other direction: with no read attempted the status IS the
        // evidence, so a decision that always looked at the outcome would be
        // just as wrong.
        assert_eq!(
            fallback_workspace_state(ide::ProviderState::Failed, None::<&ResidentOutcome<()>>),
            ide::ProviderState::Failed,
        );
    }

    /// And each non-ready outcome keeps its own advice: waiting helps, waiting
    /// is pointless, or there is nothing to wait for.
    #[test]
    fn each_unread_outcome_keeps_its_own_advice() {
        assert_eq!(
            unread_resident_state(&ResidentOutcome::<()>::Loading),
            ide::ProviderState::NotReady
        );
        assert_eq!(
            unread_resident_state(&ResidentOutcome::<()>::Disabled),
            ide::ProviderState::Unavailable
        );
        assert_eq!(
            unread_resident_state(&ResidentOutcome::<()>::Failed("нет".into())),
            ide::ProviderState::Failed
        );
    }
}

/// The same reading for the resident database, which backs three of the five
/// providers at once.
///
/// It has to be a state and not "ready or not". A resident whose build failed
/// and one still building are the same emptiness and opposite advice, and a
/// reference-profile server has no resident to wait for at all — three answers
/// a boolean cannot hold, which is how a failed build came to be reported as
/// one still in progress.
fn resident_provider_state(
    status: &crate::diagnostics_state::DiagnosticsStatus,
) -> ide::ProviderState {
    use crate::diagnostics_state::DiagnosticsStatus;
    match status {
        DiagnosticsStatus::Ready { .. } => ide::ProviderState::Answered,
        DiagnosticsStatus::Idle | DiagnosticsStatus::Loading => ide::ProviderState::NotReady,
        DiagnosticsStatus::Failed(_) => ide::ProviderState::Failed,
        DiagnosticsStatus::Disabled => ide::ProviderState::Unavailable,
    }
}

/// Which of a profile's tools this process serves.
///
/// Composition is decided once, at construction, and the router is never mutated
/// afterwards: a session's `tools/list` cannot change under the client, and no
/// `notifications/tools/list_changed` is ever sent. A hidden tool is hidden by rmcp's own
/// mechanism, so calling it fails with the error an unknown name gets — the build does not
/// leak that it could have served it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolGate {
    hidden: BTreeSet<String>,
}

impl ToolGate {
    /// Hide exactly these names. This is the primitive; launch policy is
    /// [`ToolGate::for_launch`].
    pub fn hiding(names: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self { hidden: names.into_iter().map(Into::into).collect() }
    }

    /// The gate one launch asks for: `opt_in_tools(profile) \ enabled`.
    ///
    /// The difference is taken over the opt-in set, never over everything the profile
    /// declares. Hiding "everything not named" would empty the surface on a plain launch
    /// and would make naming an already-served tool a destructive act, while both the
    /// contract and the backend identity treat it as an identity operation.
    pub fn for_launch(profile: McpProfile, enabled: &[String]) -> Self {
        Self {
            hidden: contract::opt_in_tools(profile)
                .filter(|name| !enabled.iter().any(|enabled| enabled == name))
                .map(str::to_owned)
                .collect(),
        }
    }
}

#[derive(Clone)]
pub struct McpServer {
    profile: McpProfile,
    state: SharedState,
    tool_router: ToolRouter<Self>,
}

#[tool_router(router = workspace_tool_router)]
impl McpServer {
    /// A server serving the profile's default surface.
    pub fn new(profile: McpProfile, state: SharedState) -> Self {
        Self::with_gate(profile, state, &ToolGate::for_launch(profile, &[]))
    }

    /// A server serving what `gate` leaves visible.
    pub fn with_gate(profile: McpProfile, state: SharedState, gate: &ToolGate) -> Self {
        Self { profile, state, tool_router: Self::gated_router(profile, gate) }
    }

    /// The router a profile serves under `gate`. Exposed without state so the declaration
    /// can be compared against the composition a launch actually produces.
    pub fn gated_router(profile: McpProfile, gate: &ToolGate) -> ToolRouter<Self> {
        let mut router = Self::profile_router(profile);
        for name in &gate.hidden {
            router.disable_route(name.clone());
        }
        router
    }

    pub(crate) fn profile_router(profile: McpProfile) -> ToolRouter<Self> {
        let mut router = match profile {
            McpProfile::Workspace => Self::workspace_tool_router(),
            McpProfile::Reference => Self::reference_tool_router(),
        };
        router.merge(Self::shared_tool_router());
        router
    }

    pub fn shutdown(&self) {
        self.state.shutdown();
    }

    /// Whether a newer daemon generation owns this workspace's derived caches. The broker
    /// backend consults it when it falls idle: staying warm buys a reconnecting client a
    /// resident state that can no longer maintain itself, while the memory it holds is the
    /// same multi-gigabyte footprint as a working backend's.
    pub fn superseded(&self) -> bool {
        self.state.superseded()
    }

    pub(crate) fn background_work_active(&self) -> bool {
        self.state.background_work_active()
    }

    /// Browse the configuration's metadata: objects, their structure, and managed forms.
    /// Use to answer "what objects exist / what does object X contain / what is on form Y" —
    /// attributes, tabular sections, forms, types — straight from the metadata substrate. Not
    /// for call relationships (use `graph`) and not for finding code by meaning (use `search`).
    /// Actions: `info` — configuration summary; `tree` — the metadata object tree (filterable);
    /// `object` — one object's structure (needs `object_type` + `object_name`); `form` — a
    /// source-only managed form layout (needs `object_type`); `status` — resident readiness.
    /// For `object`, use a singular analyzer type (`Документ`) in source mode or auto without
    /// `connection`, and a plural live-service collection (`Документы`) in infobase mode or auto
    /// with `connection`; the server does not convert between the forms. Reads the
    /// resident analysis host; while it builds it returns a retry envelope, not an error —
    /// `structuredContent.status == "loading"`, same field `diagnostics`/`graph` set, so retry
    /// shortly instead of reading the answer as "no such object".
    #[tool(name = "metadata", annotations(read_only_hint = true))]
    async fn metadata(
        &self,
        params: Parameters<MetadataParams>,
        ct: tokio_util::sync::CancellationToken,
        caller: tasks::TaskCapable,
    ) -> Result<CallToolResponse, McpError> {
        use crate::diagnostics_state::ResidentOutcome;

        let p = params.0;
        let mode = p.mode.as_deref().unwrap_or("auto");
        if !matches!(mode, "auto" | "source" | "infobase") {
            return Err(McpError::invalid_params(
                format!("Unknown metadata mode '{mode}'. Expected: auto, source, infobase"),
                None,
            ));
        }
        // `status` reports the resident lifecycle (and kicks the lazy build), so an agent can
        // poll readiness here instead of firing `info` just to read its `loading` envelope.
        // Answered ahead of every mode branch: readiness is a property of this server, not of
        // the requested mode, and a client that passes `connection` on every metadata call
        // would otherwise be told the action does not exist. Rendered by the shared renderer,
        // so it is byte-identical to `diagnostics status`: one resident, one status shape.
        if p.action == "status" {
            let diag = self.state.diagnostics();
            diag.ensure_loading();
            return Ok(tools::resident::status(
                &diag.status_report(),
                self.state.owns_caches_cached(),
                self.state.standalone_notice().as_deref(),
            )
            .into());
        }

        let live = mode == "infobase" || (mode == "auto" && p.connection.is_some());
        if live {
            return match p.action.as_str() {
                "tree" => {
                    let meta_type = require(p.meta_type, "meta_type", "tree in infobase mode")?;
                    tools::metadata::get_live_metadata_tree(
                        &self.state,
                        p.connection.as_deref(),
                        &meta_type,
                        p.name_mask,
                        p.max_items.unwrap_or(100),
                    )
                    .await
                }
                "object" => {
                    let object_type = require(p.object_type, "object_type", "object")?;
                    let object_name = require(p.object_name, "object_name", "object")?;
                    tools::metadata::get_live_metadata_object(
                        &self.state,
                        p.connection.as_deref(),
                        &object_type,
                        &object_name,
                    )
                    .await
                }
                other => Err(McpError::invalid_params(
                    format!(
                        "Metadata action '{other}' is unavailable in infobase mode. Expected: tree, object"
                    ),
                    None,
                )),
            }
            .map(CallToolResponse::from);
        }

        // `form` reads managed-form XML straight off the configuration source root — data
        // the metadata substrate does not carry — so it needs neither the resident db nor
        // a loaded configuration, and stays available while the resident is building or
        // evicted. `source_root` survives the `MetadataCache` retirement for exactly this.
        if p.action == "form" {
            let object_type = require(p.object_type, "object_type", "form")?;
            return tools::metadata::get_form_structure(
                self.state.source_root().map(|p| p.as_path()),
                &object_type,
                p.object_name.as_deref(),
                p.form_name.as_deref(),
            )
            .map(CallToolResponse::from);
        }

        // `info`/`tree`/`object` read the resident analysis host. Trigger the build if idle
        // or idle-evicted and, while it is not ready, return a "loading, retry" envelope —
        // never a hard "not loaded" error, so an evicted resident degrades to slow, not
        // wrong. Reference/shared profiles have no resident and stay "not configured".
        let diag = self.state.diagnostics().clone();
        // Kicks the lazy build only: every outcome, `loading` included, is rendered from
        // the read, so an already-cancelled request is never handed a body.
        diag.ensure_loading();

        let action = p.action.clone();
        let filter = p.filter.clone();
        let object_type = p.object_type.clone();
        let object_name = p.object_name.clone();
        let max_output_tokens =
            p.max_output_tokens.unwrap_or(tools::response::DEFAULT_OUTPUT_BUDGET_TOKENS);

        let retry_diag = diag.clone();
        tasks::resident_response(
            self,
            caller,
            "metadata",
            ct,
            move |session| {
                let read = || {
                    session.read(|resident, analysis, _generation| {
                        let _ = resident;
                        let db = analysis.database();
                        match action.as_str() {
                            "info" => {
                                let (config, extensions) = tools::metadata::configs_from_db(db);
                                tools::metadata::get_configuration_info(&config, &extensions)
                            }
                            "tree" => {
                                let (config, extensions) = tools::metadata::configs_from_db(db);
                                tools::metadata::get_metadata_tree(
                                    db,
                                    &config,
                                    &extensions,
                                    filter.clone(),
                                    max_output_tokens,
                                )
                            }
                            "object" => {
                                let object_type =
                                    require(object_type.clone(), "object_type", "object")?;
                                let object_name =
                                    require(object_name.clone(), "object_name", "object")?;
                                tools::metadata::object_from_db(db, &object_type, &object_name)
                            }
                            other => Err(contract::unknown_action(
                                McpProfile::Workspace,
                                "metadata",
                                other,
                            )),
                        }
                    })
                };

                // This tool's miss is an error from an object lookup whose type CAN resolve.
                // A bad object type is returned as-is: no re-scan can turn it into a type,
                // and asking for one would let a typo walk the tree.
                let outcome = session.read_retrying_a_stale_miss(read, |answer| {
                    action == "object"
                        && answer.is_err()
                        && object_type
                            .as_deref()
                            .is_some_and(tools::metadata::is_resolvable_object_type)
                });

                match outcome {
                    ResidentOutcome::Ready(result, _freshness) => result,
                    ResidentOutcome::Loading => {
                        Ok(tools::metadata::loading(&session.status_report()))
                    }
                    ResidentOutcome::Disabled => Err(McpError::invalid_params(
                        "metadata is only available in the workspace profile",
                        None,
                    )),
                    ResidentOutcome::Failed(msg) => {
                        Err(McpError::internal_error(format!("metadata database: {msg}"), None))
                    }
                }
            },
            move || tools::metadata::loading(&retry_diag.status_report()),
        )
        .await
    }

    /// Search project code or the built-in platform reference from the workspace profile.
    /// Actions: `search_code` searches project source; `find_docs`/`search_docs` search platform
    /// reference text; `list_platform` lists exact platform entities; `status` reports project
    /// code-index readiness. Not for walking call relationships — use `graph` — or analyzer
    /// findings — use `diagnostics`. While an index warms up its search action returns a retry
    /// envelope; retry shortly. Hits arrive twice: a listing for people in the text block, and
    /// the same hits in `structuredContent` — `{schema_version, hits: [{rank, modality, root_id, path,
    /// line_start, line_end, location, symbol, kind, graph_id, snippet, snippet_truncated_lines}],
    /// shown, total, budget_exhausted?, degraded?, freshness}`. `location` is the shared location
    /// contract (0-based, end-exclusive, UTF-16 columns, `(root_id, path)` as the file key) and
    /// `freshness` carries machine-readable completeness; the 1-based `line_start`/`line_end`
    /// stay as they were. Read the structured form: it is the versioned
    /// contract, whereas the text layout may be reformatted in any release. Absent fields mean
    /// absent facts — no `symbol` is a file/header chunk, no `graph_id` means the hit has no
    /// durable id to pass to `graph`. `total` is the ranked list before the output budget cut
    /// it (already bounded by `limit`), not the configuration-wide match count.
    #[tool(
        name = "search",
        output_schema = tools::search::search_output_schema(),
        annotations(read_only_hint = true)
    )]
    async fn workspace_search(
        &self,
        params: Parameters<WorkspaceSearchParams>,
        ct: tokio_util::sync::CancellationToken,
    ) -> Result<CallToolResult, McpError> {
        let started = std::time::Instant::now();
        match SearchCommand::from(params.0) {
            SearchCommand::Status => {
                let engine = self.state.search_engine().clone();
                let progress = self.state.index_progress().clone();
                let semantic_runtime = self.state.semantic_runtime();
                let workspace_search_mode = self.state.workspace_search_mode();
                let overlay_warmup = self
                    .state
                    .overlay_warmup()
                    .lock()
                    .map(|guard| guard.clone())
                    .unwrap_or(crate::state::OverlayWarmupState::Pending);
                let baseline = self.state.baseline_view();
                tokio::task::spawn_blocking(move || {
                    tools::search::search_status(
                        McpProfile::Workspace,
                        &engine,
                        &progress,
                        &semantic_runtime,
                        workspace_search_mode,
                        overlay_warmup,
                        baseline.configured,
                        baseline.external,
                        baseline.pending,
                    )
                })
                .await
                .map_err(|e| McpError::internal_error(format!("Task error: {e}"), None))?
            }
            // `search_code` is the unified lexical+semantic code search (smart-fused: exact-symbol
            // tier then semantic tail).
            SearchCommand::SearchCode(params) => {
                let query = params.query;
                let limit = params.limit.unwrap_or(10).min(50);
                let max_output_tokens = params
                    .max_output_tokens
                    .unwrap_or(tools::response::DEFAULT_OUTPUT_BUDGET_TOKENS);
                let engine = self.state.search_engine().clone();
                let semantic_runtime = self.state.semantic_runtime();
                let workspace_search_mode = self.state.workspace_search_mode();
                // A query landing during the deferred baseline connect gets the retry
                // envelope, not the gates' "fix config / restart MCP" errors — those are
                // for resolved-and-broken, this is merely not-resolved-yet. One snapshot
                // feeds both the pending check and the gates, so a publish in between
                // cannot produce a torn pending/configured/external mix.
                let baseline = self.state.baseline_view();
                if matches!(
                    workspace_search_mode,
                    crate::state::WorkspaceSearchMode::PostgresRemoteOverlay
                ) && baseline.pending
                {
                    return Ok(tools::search::baseline_warming_not_ready(
                        self.state.index_progress(),
                    ));
                }
                let configured_baseline = baseline.configured;
                let external_baseline = baseline.external;
                // The graph keys file ids against the repo (workspace) root; pass it so search
                // can mint form/file `graph_id`s with the same `src/cf/…` prefix the graph uses.
                let graph_root = self.state.workspace_root().cloned();
                let index_progress = self.state.index_progress().clone();
                self.state.request_overlay_refresh();
                let retry_progress = index_progress.clone();
                let outcome = tools::search::search_call(ct, move |cancel| {
                    tools::search::hybrid_code_cancellable(
                        &engine,
                        cancel,
                        &semantic_runtime,
                        workspace_search_mode,
                        configured_baseline.as_ref(),
                        external_baseline,
                        graph_root.as_deref(),
                        &index_progress,
                        &query,
                        limit,
                        max_output_tokens,
                    )
                })
                .await;
                cancellable_answer(outcome, "search", started, || {
                    tools::search::search_not_ready(
                        "Search was cut short by an index rebuild; please retry.",
                        &retry_progress,
                        "search_code",
                    )
                })
            }
            SearchCommand::ListPlatform(params) => tools::platform::list_platform(
                params.kind,
                params.name.as_deref(),
                params.max_output_tokens.unwrap_or(tools::response::DEFAULT_OUTPUT_BUDGET_TOKENS),
            ),
            command @ (SearchCommand::FindDocs { .. } | SearchCommand::SearchDocs { .. }) => {
                self.state.ensure_reference_loading();
                if let crate::state::ReferenceSearchLifecycle::Failed { message, reason_code } =
                    self.state.reference_lifecycle()
                {
                    return Err(McpError::internal_error(
                        format!("reference search initialization failed: {message}"),
                        Some(serde_json::json!({"reasonCode": reason_code})),
                    ));
                }
                let semantic = matches!(&command, SearchCommand::SearchDocs { .. });
                let (query, limit, max_output_tokens) = match command {
                    SearchCommand::FindDocs { query, limit, max_output_tokens }
                    | SearchCommand::SearchDocs { query, limit, max_output_tokens } => {
                        (query, limit, max_output_tokens)
                    }
                    _ => unreachable!(),
                };
                let engine = self.state.reference_search_engine();
                let baseline = self.state.reference_baseline_view();
                let configured = baseline.configured;
                let external = baseline.external;
                let limit = limit.unwrap_or(10).min(50);
                let max_output_tokens =
                    max_output_tokens.unwrap_or(tools::response::DEFAULT_OUTPUT_BUDGET_TOKENS);
                let outcome = tools::search::search_call(ct, move |cancel| {
                    if semantic {
                        tools::search::search_docs(
                            &engine,
                            cancel,
                            configured.as_ref(),
                            external,
                            &query,
                            limit,
                            max_output_tokens,
                        )
                    } else {
                        tools::search::find_docs(
                            &engine,
                            cancel,
                            configured.as_ref(),
                            external,
                            &query,
                            limit,
                            max_output_tokens,
                        )
                    }
                })
                .await;
                let action = if semantic { "search_docs" } else { "find_docs" };
                cancellable_answer(outcome, "search", started, || {
                    tools::search::docs_not_ready(action)
                })
            }
        }
    }

    /// Validate or execute SDBL (the 1C query language) against the configuration schema. Use
    /// to check a query for errors before shipping it, to run a read-only query, or to fetch
    /// the query-language schema. Not for browsing metadata structure (use `metadata`) and not
    /// for BSL code (use `execute`). Actions: `validate` — parse and type-check a query (`query`
    /// required); `execute` — run it (`query` required; optional `limit`, `parameters`);
    /// `schema` — the SDBL schema reference. `execute` output is bounded by `max_output_tokens`
    /// on top of `limit` and appends a truncation note.
    #[tool(name = "query", annotations(read_only_hint = true))]
    async fn query(
        &self,
        params: Parameters<QueryParams>,
        ct: tokio_util::sync::CancellationToken,
    ) -> Result<CallToolResult, McpError> {
        let p = params.0;
        let max_output_tokens =
            p.max_output_tokens.unwrap_or(tools::response::DEFAULT_OUTPUT_BUDGET_TOKENS);
        match p.action.as_str() {
            "schema" => Ok(tools::query::schema()),
            "validate" => {
                let query = require(p.query, "query", "validate")?;
                tools::query::validate_query(
                    &self.state,
                    &query,
                    p.root_id.as_deref(),
                    p.connection.as_deref(),
                    ct,
                    max_output_tokens,
                )
                .await
            }
            "execute" => {
                let query = require(p.query, "query", "execute")?;
                tools::query::execute_query(
                    &self.state,
                    &query,
                    p.limit,
                    p.parameters,
                    p.connection.as_deref(),
                    max_output_tokens,
                )
                .await
            }
            other => Err(contract::unknown_action(McpProfile::Workspace, "query", other)),
        }
    }

    /// Run or syntax-check BSL code in an embedded interpreter. Use to confirm a snippet
    /// compiles, run a small script, or evaluate a single expression. Not for querying the
    /// database (use `query` for SDBL) and not for analyzer findings (use `diagnostics`).
    /// Actions: `check` — syntax-check `code`; `run` — execute `code`; `eval` — evaluate the
    /// single expression in `code`. `run`/`eval` execute code, so this tool is not read-only.
    /// Output is bounded by `max_output_tokens` and appends a truncation note.
    #[tool(name = "execute")]
    async fn execute(&self, params: Parameters<ExecuteParams>) -> Result<CallToolResult, McpError> {
        let p = params.0;
        let budget = p.max_output_tokens.unwrap_or(tools::response::DEFAULT_OUTPUT_BUDGET_TOKENS);
        match p.action.as_str() {
            "check" => {
                tools::execution::check_syntax(
                    &self.state,
                    &p.code,
                    p.connection.as_deref(),
                    budget,
                )
                .await
            }
            "run" => {
                tools::execution::execute_code(
                    &self.state,
                    &p.code,
                    p.connection.as_deref(),
                    budget,
                )
                .await
            }
            "eval" => {
                tools::execution::eval_expression(
                    &self.state,
                    &p.code,
                    p.connection.as_deref(),
                    budget,
                )
                .await
            }
            other => Err(contract::unknown_action(McpProfile::Workspace, "execute", other)),
        }
    }

    /// Read the 1C infobase event log (журнал регистрации) through the deployed BSL_Analyzer
    /// extension. Use to inspect runtime events — errors, authentications, data changes —
    /// filtered by time, user, event, metadata object, or severity. Not for static analysis of
    /// source (use `diagnostics`): this reads live runtime records from a running infobase.
    /// Filters: `date_from`/`date_to`, `level`, `user`, `event`, `metadata`, and `contains`
    /// (post-read substring over the newest `limit` window). `limit` is newest-first (default
    /// 100, max 1000) and bounds the record COUNT; `max_output_tokens` bounds the response
    /// SIZE and flags `budget_exhausted`. Requires the extension deployed with event-log read
    /// rights.
    #[tool(name = "event_log", annotations(read_only_hint = true))]
    async fn event_log(
        &self,
        params: Parameters<EventLogParams>,
    ) -> Result<CallToolResult, McpError> {
        let p = params.0;
        tools::event_log::event_log(
            &self.state,
            tools::event_log::EventLogQuery {
                date_from: p.date_from,
                date_to: p.date_to,
                level: p.level,
                user: p.user,
                event: p.event,
                metadata: p.metadata,
                contains: p.contains,
                limit: p.limit,
                connection: p.connection,
            },
            p.max_output_tokens.unwrap_or(tools::response::DEFAULT_OUTPUT_BUDGET_TOKENS),
        )
        .await
    }

    /// Drive a live 1C debugger session: attach, set breakpoints, step, and inspect state. Use
    /// to debug a running infobase — attach, break, then step and read locals/eval. Not for
    /// static analysis (use `diagnostics`) and not for running standalone code (use `execute`).
    /// Actions: `attach`/`disconnect`; `set_breakpoint`/`remove_breakpoint`; `continue`/`step`;
    /// `wait_stop`; `stack_trace`; `locals`; `eval`. State-reading actions are bounded by
    /// `max_output_tokens` and append a truncation note. Requires a reachable debug endpoint
    /// (`host` + `infobase`, default port 1550).
    #[tool(name = "debug")]
    async fn debug(&self, params: Parameters<DebugParams>) -> Result<CallToolResult, McpError> {
        let p = params.0;
        let session = self.state.debug_session().clone();
        let budget = p.max_output_tokens.unwrap_or(tools::response::DEFAULT_OUTPUT_BUDGET_TOKENS);

        match p.action.as_str() {
            "attach" => {
                let host = require(p.host, "host", "attach")?;
                let infobase = require(p.infobase, "infobase", "attach")?;
                let port = p.port.unwrap_or_else(default_debug_port);
                let workspace_root = self.state.workspace_root().cloned();
                let config_root = p.config_root;
                let extensions = p.extensions;
                let auto_attach = p.auto_attach;
                tokio::task::spawn_blocking(move || {
                    tools::debug::debug_attach(
                        &session,
                        tools::debug::AttachParams {
                            host: &host,
                            port,
                            infobase: &infobase,
                            config_root: config_root.as_deref(),
                            workspace_root: workspace_root.as_deref(),
                            extensions: &extensions,
                            auto_attach: &auto_attach,
                        },
                        budget,
                    )
                })
                .await
                .map_err(|e| McpError::internal_error(format!("Task error: {e}"), None))?
            }
            "disconnect" => {
                tokio::task::spawn_blocking(move || tools::debug::debug_disconnect(&session))
                    .await
                    .map_err(|e| McpError::internal_error(format!("Task error: {e}"), None))?
            }
            "set_breakpoint" => {
                let module = require(p.module, "module", "set_breakpoint")?;
                let line = require(p.line, "line", "set_breakpoint")?;
                let condition = p.condition;
                tokio::task::spawn_blocking(move || {
                    tools::debug::debug_set_breakpoint(
                        &session,
                        &module,
                        line,
                        condition.as_deref(),
                    )
                })
                .await
                .map_err(|e| McpError::internal_error(format!("Task error: {e}"), None))?
            }
            "remove_breakpoint" => {
                let module = require(p.module, "module", "remove_breakpoint")?;
                let line = require(p.line, "line", "remove_breakpoint")?;
                tokio::task::spawn_blocking(move || {
                    tools::debug::debug_remove_breakpoint(&session, &module, line)
                })
                .await
                .map_err(|e| McpError::internal_error(format!("Task error: {e}"), None))?
            }
            "continue" => {
                tokio::task::spawn_blocking(move || tools::debug::debug_continue(&session))
                    .await
                    .map_err(|e| McpError::internal_error(format!("Task error: {e}"), None))?
            }
            "step" => {
                let direction = require(p.direction, "direction", "step")?;
                tokio::task::spawn_blocking(move || tools::debug::debug_step(&session, &direction))
                    .await
                    .map_err(|e| McpError::internal_error(format!("Task error: {e}"), None))?
            }
            "wait_stop" => {
                let timeout_secs = p.timeout_secs;
                tokio::task::spawn_blocking(move || {
                    tools::debug::debug_wait_stop(&session, timeout_secs, budget)
                })
                .await
                .map_err(|e| McpError::internal_error(format!("Task error: {e}"), None))?
            }
            "stack_trace" => tokio::task::spawn_blocking(move || {
                tools::debug::debug_stack_trace(&session, budget)
            })
            .await
            .map_err(|e| McpError::internal_error(format!("Task error: {e}"), None))?,
            "locals" => {
                let stack_level = p.stack_level;
                tokio::task::spawn_blocking(move || {
                    tools::debug::debug_locals(&session, stack_level, budget)
                })
                .await
                .map_err(|e| McpError::internal_error(format!("Task error: {e}"), None))?
            }
            "eval" => {
                let expression = require(p.expression, "expression", "eval")?;
                let stack_level = p.stack_level;
                tokio::task::spawn_blocking(move || {
                    tools::debug::debug_eval(&session, &expression, stack_level, budget)
                })
                .await
                .map_err(|e| McpError::internal_error(format!("Task error: {e}"), None))?
            }
            other => Err(contract::unknown_action(McpProfile::Workspace, "debug", other)),
        }
    }

    /// The name dictionary behind `graph action=resolve`.
    ///
    /// Five providers, each of which may be missing for its own reason, and the
    /// answer says which were. Neither the graph nor the resident is required:
    /// a host with neither still answers from the platform, and each of the two
    /// says what it is actually doing — building, failed, or absent from this
    /// profile — instead of returning an empty list that would read as a proven
    /// zero.
    async fn resolve_names(
        &self,
        query: String,
        limit: usize,
        ct: tokio_util::sync::CancellationToken,
    ) -> Result<CallToolResult, McpError> {
        use crate::diagnostics_state::{resident_call, ResidentOutcome};

        let graph = self.state.graph().clone();
        let snapshot = graph.snapshot();
        let graph_state = graph_provider_state(&graph.status(), snapshot.is_some());

        let diag = self.state.diagnostics().clone();
        diag.ensure_loading();
        let resident_state = resident_provider_state(&diag.status());
        let resident_ready = resident_state == ide::ProviderState::Answered;

        let started = std::time::Instant::now();
        let retry_diag = diag.clone();
        let outcome = resident_call(diag, ct, move |session| {
            let source = match (&snapshot, graph_state) {
                (Some(snapshot), ide::ProviderState::Answered) => {
                    crate::graph_query::GraphNameSource::answering(&snapshot.graph)
                }
                (_, state) => crate::graph_query::GraphNameSource::absent(state),
            };

            let served = |db: &ide::RootDatabaseImpl,
                          workspace: ide::ProviderState,
                          roots: Option<&bsl_search::WorkspaceRoots>| {
                tools::graph::resolve(db, workspace, &source, roots, &query, limit)
            };

            let outcome = resident_ready.then(|| {
                session.read(|resident, analysis, _generation| {
                    let (value, completeness) = served(
                        analysis.database(),
                        ide::ProviderState::Answered,
                        Some(resident.workspace_roots()),
                    );
                    (value, completeness, resident.unread_count())
                })
            });

            // Decided from the read that actually happened, while the outcome is
            // still in hand.
            let workspace = fallback_workspace_state(resident_state, outcome.as_ref());

            let answer = match outcome {
                Some(ResidentOutcome::Ready((value, completeness, unread), _)) => {
                    // Modules that could not be read hold members this search
                    // would have matched.
                    Some((
                        value,
                        completeness.when(
                            unread > 0,
                            tools::location::ReasonCode::UnreadableFiles,
                            "some workspace files could not be read, so the search was \
                             not exhaustive",
                        ),
                    ))
                }
                _ => None,
            };

            // No resident: an empty database under the verdict decided above, so
            // its three providers report what is actually true of it — building,
            // failed, or absent from this profile. The platform still answers,
            // which is the whole point of serving this action ahead of the gate.
            let (value, completeness) = answer.unwrap_or_else(|| {
                session.read_detached(|empty| served(empty.database(), workspace, None))
            });

            // Not `graph::envelope`: that one stamps the graph's revision and
            // drift verdict, and this answer is not the graph's.
            Ok(tools::response::structured(serde_json::json!({
                "freshness": tools::name_answer::NameAnswer::freshness(completeness).to_value(),
                "result": value,
            })))
        })
        .await;
        cancellable_answer(outcome, "graph resolve", started, || {
            tools::metadata::loading(&retry_diag.status_report())
        })
    }

    /// Whole-config semantic call graph: traverse who-calls-whom and object/metadata usage by
    /// durable node id. Use to understand call relationships and change impact — start with
    /// `overview` on an unfamiliar project, then `node`/`callers`/`callees`/`neighbors` on the
    /// ids it returns. Not for finding code by meaning (use `search`) and not for analyzer
    /// findings (use `diagnostics`). Actions: `overview`, `schema`, `status`, `resolve`
    /// (imprecise name → candidates, each with every address that works), `node`, `source`,
    /// `neighbors`, `callers`, `callees`. Source-bearing actions honour `max_output_tokens`
    /// and flag `budget_exhausted` on truncation. Lazily indexes on first use; while it
    /// builds, traversal returns a retry envelope — but `resolve` answers anyway, from the
    /// name dictionary, and names every source it could not consult.
    #[tool(name = "graph", annotations(read_only_hint = true))]
    async fn graph(
        &self,
        params: Parameters<GraphParams>,
        ct: tokio_util::sync::CancellationToken,
    ) -> Result<CallToolResult, McpError> {
        let p = params.0;
        let graph = self.state.graph().clone();

        // `schema` is static and needs no loaded graph.
        if p.action == "schema" {
            return Ok(tools::graph::schema());
        }

        // `status` reports the graph lifecycle (and kicks the lazy build) so an agent can start
        // it and poll progress instead of reading a flat `loading` envelope from a data action.
        if p.action == "status" {
            graph.ensure_loading();
            let report = graph.status_report();
            return Ok(tools::graph::status(&report));
        }

        // Lazily trigger the background load on first use.
        graph.ensure_loading();
        let superseded = graph.superseded_latched();

        // `resolve` is a name lookup, not a graph traversal: the platform answers it
        // with no index at all and the resident's tables answer it without the graph,
        // so it is served BEFORE the readiness gate rather than behind a `loading`
        // envelope that carried no candidates. The `ensure_loading` above still runs —
        // without it the `graph: not_ready` this action reports would never converge,
        // because nothing else would start the build.
        if p.action == "resolve" {
            let query = require(p.query, "query", "resolve")?;
            let limit = p.top.unwrap_or(tools::graph::DEFAULT_RESOLVE_LIMIT);
            return self.resolve_names(query, limit, ct).await;
        }

        match graph.status() {
            _ if superseded => {
                return Err(McpError::internal_error(crate::graph::SUPERSEDED_GRAPH_ERROR, None))
            }
            GraphStatus::Disabled => {
                return Err(McpError::invalid_params(
                    "graph is only available in the workspace profile",
                    None,
                ))
            }
            GraphStatus::Idle | GraphStatus::Loading => {
                return Ok(tools::graph::loading(Some(
                    "call graph is still indexing; retry shortly",
                )))
            }
            GraphStatus::Failed(msg) => {
                return Err(McpError::internal_error(format!("graph load failed: {msg}"), None))
            }
            GraphStatus::Ready { .. } => {}
        }

        let Some(snapshot) = graph.snapshot() else {
            if graph.superseded_latched() {
                return Err(McpError::internal_error(crate::graph::SUPERSEDED_GRAPH_ERROR, None));
            }
            return Ok(tools::graph::loading(None));
        };

        tokio::task::spawn_blocking(move || {
            let gdb = &snapshot.graph;
            // The table of the publication that is answering — every node this call serves
            // is placed against it, or says why it could not be.
            let roots = snapshot.workspace_roots();
            let (value, completeness) = match p.action.as_str() {
                "overview" => tools::graph::overview(gdb, p.top.unwrap_or(20), roots),
                "node" => {
                    let id = require(p.id, "id", "node")?;
                    let detail = tools::graph::detail_from(p.detail.as_deref())
                        .map_err(|e| McpError::invalid_params(e, None))?;
                    let budget =
                        p.max_output_tokens.unwrap_or(tools::graph::DEFAULT_BODY_BUDGET_TOKENS);
                    tools::graph::node(gdb, &id, detail, budget, roots)
                }
                "source" => {
                    if p.ids.is_empty() {
                        return Err(McpError::invalid_params(
                            "'ids' is required (non-empty) for action 'source'",
                            None,
                        ));
                    }
                    let budget = p.max_output_tokens.unwrap_or(4000);
                    tools::graph::source(gdb, &p.ids, budget)
                }
                action @ ("neighbors" | "callers" | "callees") => {
                    let id = require(p.id, "id", action)?;
                    let dir = match action {
                        "callers" => ide::Direction::In,
                        "callees" => ide::Direction::Out,
                        _ => tools::graph::direction_from(p.dir.as_deref())
                            .map_err(|e| McpError::invalid_params(e, None))?,
                    };
                    let detail = tools::graph::detail_from(p.detail.as_deref())
                        .map_err(|e| McpError::invalid_params(e, None))?;
                    tools::graph::validate_edge_kinds(&p.edge_kinds)
                        .map_err(|e| McpError::invalid_params(e, None))?;
                    let max_call_sites = tools::graph::call_site_cap(p.max_call_sites)
                        .map_err(|e| McpError::invalid_params(e, None))?;
                    let neighbors = ide::NeighborsParams {
                        id: &id,
                        dir,
                        depth: p.depth.unwrap_or(1),
                        max_nodes: p.max_nodes.unwrap_or(50),
                        detail,
                        provenance_filter: p.provenance.clone(),
                        edge_kind_filter: p.edge_kinds.clone(),
                        call_sites: p.call_sites.unwrap_or(false),
                        max_call_sites,
                    };
                    let budget =
                        p.max_output_tokens.unwrap_or(tools::graph::DEFAULT_BODY_BUDGET_TOKENS);
                    tools::graph::neighbors(gdb, &neighbors, budget, roots)
                }
                other => {
                    return Err(contract::unknown_action(McpProfile::Workspace, "graph", other))
                }
            };
            // Request reads report the publication already paired with this descriptor;
            // background owners detect drift and schedule reloads.
            let freshness = graph.cached_freshness(&snapshot);
            // Modules the artefact could not read are missing nodes and edges: that is
            // incompleteness of the answer, not merely drift.
            let completeness = completeness.when(
                snapshot.unread_files() > 0,
                tools::location::ReasonCode::UnreadableFiles,
                "some workspace modules could not be read when the graph was built",
            );
            Ok(tools::graph::envelope(freshness, completeness, value))
        })
        .await
        .map_err(|e| McpError::internal_error(format!("Task error: {e}"), None))?
    }

    /// One symbol's consolidated card: kind, signature, type, doc, definition site, and a
    /// usages summary — by qualified name. Use to answer "what is X / where is it defined / what
    /// does it return / who calls it" for a single symbol in ONE call, instead of chaining
    /// hover + definition + references. Pass `symbol` (a common-module method
    /// `ОбщегоНазначения.ЗначениеРеквизитаОбъекта`, a metadata object `Справочник.Товары` or its
    /// attribute `Справочник.Товары.Артикул`, an object/manager method
    /// `Документ.ЗаказКлиента.Провести`, or a platform member `СтрНайти`); for a local/parameter
    /// with no qualified name pass `path`+`line` instead. An imprecise `symbol` returns candidate
    /// ids (not an error) — resolve one, or open it in `graph`. Not for finding code by meaning
    /// (use `search`), whole-object browsing (use `metadata`), or the full caller list (use
    /// `graph` with the returned `graph_id`). Reads the resident host; while it builds it returns
    /// a retry envelope. The `usages` summary needs the call graph; if it is still indexing the
    /// core card is still served with `usages_unavailable`.
    #[tool(
        name = "symbol_info",
        output_schema = tools::symbol_info::symbol_info_output_schema(),
        annotations(read_only_hint = true)
    )]
    async fn symbol_info(
        &self,
        params: Parameters<SymbolInfoParams>,
        ct: tokio_util::sync::CancellationToken,
        caller: tasks::TaskCapable,
    ) -> Result<CallToolResponse, McpError> {
        use crate::diagnostics_state::ResidentOutcome;

        let p = params.0;
        if p.symbol.as_deref().is_some_and(|symbol| !ide::is_well_formed_symbol(symbol)) {
            return Err(McpError::invalid_params(
                "'symbol' must be a dot-separated sequence of identifiers",
                None,
            ));
        }
        let has_position_field =
            p.root_id.is_some() || p.path.is_some() || p.line.is_some() || p.column.is_some();
        let has_complete_position = p.path.is_some() && p.line.is_some();
        if has_position_field && !has_complete_position {
            return Err(McpError::invalid_params(
                "positional context requires both 'path' and 'line'",
                None,
            ));
        }
        if p.symbol.is_none() && !has_complete_position {
            return Err(McpError::invalid_params(
                "one of 'symbol' or 'path'+'line' is required",
                None,
            ));
        }

        // The core card resolves on the resident host. Kicks the lazy build and nothing
        // more: every outcome, the retry envelope included, is rendered from the read, so
        // an already-cancelled request is never handed a body.
        let diag = self.state.diagnostics().clone();
        diag.ensure_loading();

        let sections = tools::symbol_info::sections_from(&p.include);
        let locale = tools::symbol_info::locale_from(p.locale.as_deref())?;
        let max_output_tokens =
            p.max_output_tokens.unwrap_or(tools::response::DEFAULT_OUTPUT_BUDGET_TOKENS);
        let member_kind = p.member_kind.map(SymbolMemberKindFilter::as_str);
        let member_name = p.member_name.clone();
        let symbol = p.symbol.clone();
        let root_id = p.root_id.clone();
        let path = p.path.clone();
        let line = p.line;
        let column = p.column;

        // The graph is enrichment only (usages + fuzzy candidates on a resident miss). Take a
        // best-effort snapshot: `None` when it is not `Ready`, in which case the core card is
        // still served (with `usages_unavailable`).
        let graph = self.state.graph().clone();
        graph.ensure_loading();

        let retry_diag = diag.clone();
        tasks::resident_response(
            self,
            caller,
            "symbol_info",
            ct,
            move |session| {
                let read = || {
                    session.read(|resident, analysis, _generation| {
                        // The root table and the unread count are read UNDER the same lock as
                        // the card, so the envelope describes the resident that answered rather
                        // than whatever it became afterwards.
                        let card = tools::symbol_info::resolve_card(
                            resident,
                            analysis.database(),
                            symbol.as_deref(),
                            root_id.as_deref(),
                            path.as_deref(),
                            line,
                            column,
                            sections,
                            locale,
                        );
                        card.map(|card| {
                            (card, resident.workspace_roots().clone(), resident.unread_count())
                        })
                    })
                };

                // This tool's miss is an absent card for a request made BY NAME. A positional
                // request that resolved nothing describes a place, not a name, and a re-scan
                // cannot put a symbol where the caller pointed.
                let outcome = session.read_retrying_a_stale_miss(read, |answer| {
                    symbol.is_some() && matches!(answer, Ok((None, _, _)))
                });

                let (card, roots, unread_files, freshness) = match outcome {
                    ResidentOutcome::Ready(result, freshness) => {
                        let (card, roots, unread_files) = result?;
                        (card, roots, unread_files, freshness)
                    }
                    ResidentOutcome::Loading => {
                        return Ok(tools::symbol_info::loading(&session.status_report()))
                    }
                    ResidentOutcome::Disabled => {
                        return Err(McpError::invalid_params(
                            "symbol_info is only available in the workspace profile",
                            None,
                        ))
                    }
                    ResidentOutcome::Failed(msg) => {
                        return Err(tools::symbol_info::database_error(msg))
                    }
                };

                let snapshot = graph.snapshot();
                let graph_state = graph_provider_state(&graph.status(), snapshot.is_some());
                let gdb = snapshot.as_ref().map(|s| &*s.graph);

                let stamp = tools::symbol_info::ResidentStamp {
                    roots: Some(&roots),
                    revision: freshness.revision,
                    topology: freshness.topology,
                    stale: freshness.stale,
                    unread_files,
                };

                match card {
                    Some(mut card) => {
                        tools::symbol_info::filter_members(
                            &mut card,
                            member_kind,
                            member_name.as_deref(),
                        );
                        Ok(tools::symbol_info::render_card(
                            &card,
                            gdb,
                            tools::symbol_info::DEFAULT_TOP_MODULES,
                            max_output_tokens,
                            &stamp,
                        ))
                    }
                    None => {
                        // Resident miss: offer the name dictionary's candidates for an
                        // imprecise name. The lookup runs under the resident lock — it
                        // reads that database's tables, and placing a candidate needs
                        // the same file set that produced it.
                        let symbol = symbol.as_deref().unwrap_or_default();
                        let source = match (&snapshot, graph_state) {
                            (Some(snapshot), ide::ProviderState::Answered) => {
                                crate::graph_query::GraphNameSource::answering(&snapshot.graph)
                            }
                            (_, state) => crate::graph_query::GraphNameSource::absent(state),
                        };
                        let answer = session.read(|resident, analysis, _generation| {
                            let db = analysis.database();
                            let query = ide::NameQuery::new(
                                symbol,
                                tools::symbol_info::DEFAULT_CANDIDATE_LIMIT,
                            );
                            let found = ide::lookup_names(db, &query, &[&source]);
                            tools::name_answer::NameAnswer::render(
                                db,
                                Some(resident.workspace_roots()),
                                &found,
                            )
                        });
                        match answer {
                            ResidentOutcome::Ready(answer, _) => {
                                Ok(tools::symbol_info::render_not_found(symbol, answer, &stamp))
                            }
                            // The resident was evicted between the card read and this
                            // one; a retry envelope is the honest answer, not a miss
                            // with no candidates.
                            _ => Ok(tools::symbol_info::loading(&session.status_report())),
                        }
                    }
                }
            },
            move || tools::symbol_info::loading(&retry_diag.status_report()),
        )
        .await
    }

    /// Every occurrence of ONE symbol across the workspace, each labelled with what it does
    /// at its site (`declaration` | `call` | `write` | `read`). Use to answer "who uses X",
    /// "is this method called anywhere", "where is this variable written" before renaming or
    /// deleting. Pass `symbol` (a qualified name — `ОбщегоНазначения.Метод`,
    /// `Справочник.Товары.ОбновитьКэш` — exported members only, plus form-module methods as
    /// `Документ.Заказ.Форма.ФормаДокумента.ПриОткрытии` — or a short unique name); for a
    /// local, a parameter or a non-exported member pass `path` with `line_content` — the text
    /// of the line as you read it, which is checked against the file before anything is
    /// counted — or `path`+`line` when you trust the coordinates. The answer always says which of five things happened in
    /// `outcome`: `resolved` (the list IS the answer, and an empty list is a proven zero unless
    /// `total_is_lower_bound` or `freshness.completeness` says the walk was cut short or
    /// could not read everything),
    /// `ambiguous` (several declarations answer to that name — the answer's
    /// `resolution_hint` names the axis that separates THESE ones: a root, a qualified
    /// `symbol`, or a `root_id`+`path` pair with the text of a line), `not_found` (nothing matched exactly), `unsupported_symbol` (the
    /// name resolves to something no reference walk enumerates — a metadata object, a
    /// platform member, a module as a whole), `anchor_stale` (the quoted line does not
    /// describe that file any more — `anchor_stale.reason` says how, and the freshness
    /// envelope says which revision you diverged from). Narrow a large answer with `area_root_id`,
    /// `area_path_prefix` or `kinds` and walk the per-file `files` histogram; there is no
    /// cursor. Not for the caller graph of a method (use `graph` with its `graph_id`) or for
    /// finding code by meaning (use `search`). Reads the resident host; while it builds it
    /// returns a retry envelope.
    #[tool(
        name = "references",
        output_schema = tools::references::references_output_schema(),
        annotations(read_only_hint = true)
    )]
    async fn references(
        &self,
        params: Parameters<ReferencesParams>,
        ct: tokio_util::sync::CancellationToken,
        caller: tasks::TaskCapable,
    ) -> Result<CallToolResponse, McpError> {
        use crate::diagnostics_state::ResidentOutcome;

        let p = params.0;
        if p.symbol.is_none() && p.path.is_none() {
            return Err(McpError::invalid_params(
                if p.line_content.is_some() {
                    tools::references::CONTENT_NEEDS_PATH
                } else {
                    tools::references::NO_ANCHOR
                },
                None,
            ));
        }

        // The call graph anchors names of its own, and its state is what `providers`
        // reports: passing no source at all would report it as never consulted while the
        // answer quietly depended on the resident alone.
        let graph = self.state.graph().clone();

        let diag = self.state.diagnostics().clone();
        // Kicks the lazy build, and nothing more. The lifecycle is NOT branched on here:
        // an answer decided before `resident_call` is an answer decided before the
        // cancellation token is ever consulted, so an already-cancelled request would be
        // handed a `loading` body — the one shape the contract says a cancelled call
        // never produces. Every outcome, `loading` included, is rendered from the read.
        diag.ensure_loading();

        let max_output_tokens = p.max_output_tokens.unwrap_or(tools::references::DEFAULT_BUDGET);

        let retry_diag = diag.clone();
        tasks::resident_response(
            self,
            caller,
            "references",
            ct,
            move |session| {
                let snapshot = graph.snapshot();
                let graph_state = graph_provider_state(&graph.status(), snapshot.is_some());
                let graph_source = match (&snapshot, graph_state) {
                    (Some(snapshot), ide::ProviderState::Answered) => {
                        crate::graph_query::GraphNameSource::answering(&snapshot.graph)
                    }
                    (_, state) => crate::graph_query::GraphNameSource::absent(state),
                };

                // The whole answer is assembled under the resident read lock: the hits, the
                // paths they are published under and the root table that names them all
                // describe ONE revision, and taking any of them afterwards would stamp the
                // envelope of a resident that no longer produced the body.
                let read = || {
                    session.read(|resident, analysis, _generation| {
                        let params = tools::references::Params {
                            symbol: p.symbol.as_deref(),
                            anchor_root_id: p.anchor_root_id.as_deref(),
                            root_id: p.root_id.as_deref(),
                            path: p.path.as_deref(),
                            line: p.line,
                            column: p.column,
                            line_content: p.line_content.as_deref(),
                            area_root_id: p.area_root_id.as_deref(),
                            area_path_prefix: p.area_path_prefix.as_deref(),
                            kinds: &p.kinds,
                            include_declaration: p.include_declaration,
                            limit: p.limit,
                            max_files: p.max_files,
                            include_preview: p.include_preview,
                        };
                        tools::references::answer(
                            resident,
                            analysis.database(),
                            &params,
                            max_output_tokens,
                            &[&graph_source],
                        )
                    })
                };

                // This tool's miss is decided by the answer itself, not by the shape of the
                // request — see `warrants_rescan`.
                let outcome = session.read_retrying_a_stale_miss(read, |answer| {
                    matches!(answer, Ok(answer) if tools::references::warrants_rescan(answer))
                });

                match outcome {
                    ResidentOutcome::Ready(answer, freshness) => Ok(tools::references::finish(
                        answer?,
                        freshness.revision,
                        freshness.topology,
                        freshness.stale,
                    )),
                    ResidentOutcome::Loading => {
                        Ok(tools::references::loading(&session.status_report()))
                    }
                    ResidentOutcome::Disabled => Err(McpError::invalid_params(
                        "references is only available in the workspace profile",
                        None,
                    )),
                    ResidentOutcome::Failed(msg) => {
                        Err(McpError::internal_error(format!("references database: {msg}"), None))
                    }
                }
            },
            move || tools::references::loading(&retry_diag.status_report()),
        )
        .await
    }

    /// Semantic analyzer findings the compiler and grep cannot give you — unreachable code,
    /// type mismatch, unresolved calls, and 180+ other rules. Use to check a file or the whole
    /// config for issues, or to discover which rules exist. Not for finding code (use `search`)
    /// and not for call relationships (use `graph`). Actions: `catalog` — list rules (start here
    /// to learn the codes); `schema` — response shape; `status` — analysis readiness; `file` —
    /// per-finding results for one `.bsl` path; `workspace` — a bounded per-code aggregate sweep
    /// of the whole config. Honours `max_output_tokens`/`max_findings` and flags truncation.
    /// Reads the resident host; while it builds it returns a retry envelope.
    #[tool(
        name = "diagnostics",
        output_schema = rmcp::handler::server::tool::schema_for_type::<
            tools::diagnostics::DiagnosticsResponseSchema,
        >(),
        annotations(read_only_hint = true)
    )]
    async fn diagnostics(
        &self,
        params: Parameters<DiagnosticsParams>,
        ct: tokio_util::sync::CancellationToken,
        caller: tasks::TaskCapable,
    ) -> Result<CallToolResponse, McpError> {
        let p = params.0;
        // Only the actions that honour a budget are held to its minimum: `schema` and
        // `status` ignore `max_output_tokens` entirely, and refusing them for a value
        // they never read would be a refusal about nothing.
        let budgeted = matches!(p.action.as_str(), "catalog" | "file" | "workspace");
        if budgeted
            && p.max_output_tokens
                .is_some_and(|tokens| tokens < tools::diagnostics::MIN_OUTPUT_TOKENS)
        {
            return Err(McpError::invalid_params(
                format!(
                    "max_output_tokens must be at least {} for diagnostics",
                    tools::diagnostics::MIN_OUTPUT_TOKENS
                ),
                None,
            ));
        }
        match p.action.as_str() {
            // `catalog` and `schema` are static (compile-time metadata), so they need
            // no resident analysis database and answer in either profile.
            "schema" => Ok(tools::diagnostics::schema().into()),
            "catalog" => {
                let locale = match p.locale.as_deref() {
                    Some(s) => ide::Locale::from_config_str(s)
                        .map_err(|e| McpError::invalid_params(e.to_string(), None))?,
                    None => ide::Locale::default(),
                };
                Ok(tools::diagnostics::catalog(locale, &p.codes, p.max_output_tokens).into())
            }
            // `status` reports the resident lifecycle (and kicks the lazy build) so an
            // agent can start it and poll progress instead of a flat `loading`.
            "status" => {
                let diag = self.state.diagnostics();
                diag.ensure_loading();
                Ok(tools::resident::status(
                    &diag.status_report(),
                    self.state.owns_caches_cached(),
                    self.state.standalone_notice().as_deref(),
                )
                .into())
            }
            "file" => self.diagnostics_file(p, ct, caller).await,
            "workspace" => self.diagnostics_workspace(p, ct, caller).await,
            other => Err(contract::unknown_action(McpProfile::Workspace, "diagnostics", other)),
        }
    }

    /// The `diagnostics file` action: build/serve per-file findings from the resident
    /// analysis database, behind the lazy-load lifecycle and freshness envelope.
    async fn diagnostics_file(
        &self,
        p: DiagnosticsParams,
        ct: tokio_util::sync::CancellationToken,
        caller: tasks::TaskCapable,
    ) -> Result<CallToolResponse, McpError> {
        use tools::diagnostics::{parse_detail, parse_min_severity, FileFilters};

        let diag = self.state.diagnostics().clone();
        let path = require(p.path, "path", "file")?;
        let path = std::path::PathBuf::from(path);
        let root_id = p.root_id;

        // Kicks the lazy build only: every outcome, `loading` included, is rendered from
        // the read, so an already-cancelled request is never handed a body.
        diag.ensure_loading();

        let min_severity = parse_min_severity(p.min_severity.as_deref())
            .map_err(|e| McpError::invalid_params(e, None))?;
        let detailed =
            parse_detail(p.detail.as_deref()).map_err(|e| McpError::invalid_params(e, None))?;
        let range = match (p.range_start, p.range_end) {
            (Some(s), Some(e)) => Some((s, e)),
            (Some(s), None) => Some((s, usize::MAX)),
            (None, Some(e)) => Some((0, e)),
            (None, None) => None,
        };
        let filters = FileFilters {
            min_severity,
            codes: p.codes,
            range,
            max_findings: p.max_findings.unwrap_or(tools::diagnostics::DEFAULT_MAX_FINDINGS),
            max_output_tokens: p.max_output_tokens,
            detailed,
        };

        let retry_diag = diag.clone();
        tasks::resident_response(
            self,
            caller,
            "diagnostics file",
            ct,
            move |session| {
                // `generation` is supplied by `read` under the lock (so `result_id` describes
                // the exact resident state queried), and the freshness verdict is computed
                // under that same lock and returned alongside — the envelope is atomic.
                let outcome = session.read(|resident, analysis, generation| {
                    tools::diagnostics::file_findings(
                        resident,
                        analysis,
                        root_id.as_deref(),
                        &path,
                        &filters,
                        generation,
                    )
                });
                use crate::diagnostics_state::ResidentOutcome;
                match outcome {
                    ResidentOutcome::Ready((result, completeness), freshness) => {
                        Ok(tools::diagnostics::envelope(freshness, completeness, result))
                    }
                    ResidentOutcome::Loading => {
                        Ok(tools::diagnostics::loading(&session.status_report()))
                    }
                    ResidentOutcome::Disabled => Err(McpError::invalid_params(
                        "diagnostics 'file' is only available in the workspace profile",
                        None,
                    )),
                    ResidentOutcome::Failed(msg) => {
                        Err(McpError::internal_error(format!("diagnostics database: {msg}"), None))
                    }
                }
            },
            move || tools::diagnostics::loading(&retry_diag.status_report()),
        )
        .await
    }

    /// The `diagnostics workspace` action: an opt-in, bounded whole-config sweep that
    /// returns per-code aggregates only (no per-finding detail). The rayon sweep runs
    /// under the resident lock (so no reload mutates the db mid-sweep), which serialises
    /// other diagnostics calls for its duration — acceptable for a capped, opt-in pass.
    ///
    /// `ct` is the rmcp per-request token, cancelled on MCP `notifications/cancelled`
    /// and on transport shutdown; on cancel the call answers immediately with an
    /// error while the sweep's per-worker salsa tokens unwind it early, releasing
    /// the resident lock instead of silently running to completion for minutes.
    async fn diagnostics_workspace(
        &self,
        p: DiagnosticsParams,
        ct: tokio_util::sync::CancellationToken,
        caller: tasks::TaskCapable,
    ) -> Result<CallToolResponse, McpError> {
        use crate::diagnostics_state::{ResidentOutcome, SweepOptions};
        use tools::diagnostics::{
            parse_min_severity, DEFAULT_MAX_SWEEP_FILES, MAX_SWEEP_FILES_CEILING,
        };

        let diag = self.state.diagnostics().clone();
        // Kicks the lazy build only: branching on the lifecycle here would answer before
        // the cancellation token is consulted, and a cancelled call would be handed a
        // `loading` body. The read renders every outcome, `loading` included.
        diag.ensure_loading();

        let min_severity = parse_min_severity(p.min_severity.as_deref())
            .map_err(|e| McpError::invalid_params(e, None))?;
        let max_output_tokens = p.max_output_tokens;
        let opts = SweepOptions {
            min_severity,
            codes: p.codes,
            // Clamp to the ceiling: the sweep holds the resident lock throughout, so an
            // unbounded request would stall every other diagnostics call. A larger
            // config surfaces as `truncated` with the true `files_total`.
            max_files: p.max_files.unwrap_or(DEFAULT_MAX_SWEEP_FILES).min(MAX_SWEEP_FILES_CEILING),
        };

        // The sweep fans out to rayon, and each worker queries its OWN db clone with its
        // own salsa token. Those tokens register in the request's registry — the same one
        // the door uses for the request's handle — so one cancel reaches the request and
        // every worker it spawned. Only clones are ever cancelled: the master handle and
        // concurrent calls stay untouched.
        let started = std::time::Instant::now();
        let retry_diag = diag.clone();
        tasks::resident_response(
            self,
            caller,
            "diagnostics workspace",
            ct,
            move |session| {
                let sweep_cancel = std::sync::Arc::clone(session.cancel());
                let outcome = session.read_fanout(|resident, generation| {
                    let sweep =
                        resident.workspace_aggregates(resident.config(), &opts, &sweep_cancel);
                    if sweep.cancelled {
                        tracing::info!(
                            tool = "diagnostics",
                            action = "workspace",
                            elapsed_ms = started.elapsed().as_millis() as u64,
                            files_processed = sweep.files_swept,
                            files_total = sweep.files_total,
                            "MCP call cancelled, sweep unwound early"
                        );
                    }
                    tools::diagnostics::workspace_findings(&sweep, generation, max_output_tokens)
                });
                match outcome {
                    ResidentOutcome::Ready((result, completeness), freshness) => {
                        Ok(tools::diagnostics::envelope(freshness, completeness, result))
                    }
                    ResidentOutcome::Loading => {
                        Ok(tools::diagnostics::loading(&session.status_report()))
                    }
                    ResidentOutcome::Disabled => Err(McpError::invalid_params(
                        "diagnostics 'workspace' is only available in the workspace profile",
                        None,
                    )),
                    ResidentOutcome::Failed(msg) => {
                        Err(McpError::internal_error(format!("diagnostics database: {msg}"), None))
                    }
                }
            },
            move || tools::diagnostics::loading(&retry_diag.status_report()),
        )
        .await
    }

    /// The map of ONE `.bsl` file: its `#Область` tree, the procedures, functions and module
    /// variables it declares, each with its export flag, compilation directives, parameters
    /// (with `Знач` and default values) and 0-based UTF-16 ranges. Method BODIES are never
    /// returned — read the file for those, or ask `search`. Use it to orient yourself in an
    /// unfamiliar module before reading it, or to find where a declaration sits. Params: `path`
    /// (required) with optional `root_id` naming the source root it is spelled against, `mode`
    /// (`full` | `regions` — the region skeleton alone, for a big module), `max_output_tokens`.
    /// Answers from one parse of that file: no index is built, nothing is ever `loading`, and
    /// the answer is the same whether or not the workspace has been analysed.
    #[tool(name = "outline", annotations(read_only_hint = true))]
    async fn outline(&self, params: Parameters<OutlineParams>) -> Result<CallToolResult, McpError> {
        let p = params.0;
        let Some(workspace_root) = self.state.workspace_root().cloned() else {
            return Err(McpError::invalid_params(
                "`outline` is only available in the workspace profile",
                None,
            ));
        };
        let mode = tools::outline::parse_mode(p.mode.as_deref())
            .map_err(|e| McpError::invalid_params(e, None))?;
        let max_output_tokens =
            p.max_output_tokens.unwrap_or(tools::response::DEFAULT_OUTPUT_BUDGET_TOKENS);
        let path = std::path::PathBuf::from(p.path);

        // Off the async executor: parsing a 1 MB module takes tens of milliseconds, and the
        // whole answer is one such parse.
        tokio::task::spawn_blocking(move || {
            // Built per call, from the project alone. Milliseconds on real configurations, and
            // caching it would mean a second lifecycle to invalidate beside the resident's.
            let project = crate::project::at(&workspace_root).map_err(|e| {
                McpError::internal_error(format!("workspace project failed to load: {e}"), None)
            })?;
            let (roots, _rejected) = crate::project::workspace_roots(&project, &[]);
            tools::outline::answer(
                &roots,
                &workspace_root,
                p.root_id.as_deref(),
                &path,
                mode,
                max_output_tokens,
            )
        })
        .await
        .map_err(|e| McpError::internal_error(format!("Task error: {e}"), None))?
    }
}

/// One rendering of a resident call's outcome, so no tool invents its own.
///
/// `retry` builds that tool's own retry envelope: it is what a caller gets when a WRITER
/// cut the call short, which is not the client's cancellation and must not be reported as
/// one. Everything else is the same sentence for every tool — the contract says the server
/// has one behaviour at cancellation, and one behaviour is easiest to keep by having one
/// place that spells it.
fn cancellable_answer(
    outcome: crate::diagnostics_state::CallOutcome<Result<CallToolResult, McpError>>,
    tool: &'static str,
    started: std::time::Instant,
    retry: impl FnOnce() -> CallToolResult,
) -> Result<CallToolResult, McpError> {
    use crate::diagnostics_state::CallOutcome;
    match outcome {
        CallOutcome::Ready(answer) => answer,
        CallOutcome::Cancelled => {
            tracing::info!(
                tool,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "MCP call cancelled, answer abandoned"
            );
            Err(McpError::internal_error("request cancelled", None))
        }
        CallOutcome::Superseded => Ok(retry()),
        CallOutcome::Panicked => Err(McpError::internal_error("internal handler panic", None)),
    }
}

#[tool_router(router = reference_tool_router)]
impl McpServer {
    /// Search the platform reference documentation index (not project code). Use to find
    /// platform API documentation by keyword or meaning. For project code search use the
    /// workspace profile's `search`; for one platform member's signature use `syntax_help`.
    /// Actions: `find_docs` / `search_docs` — doc search (`query` required; `limit` default 10,
    /// max 50); `list_platform` — exact built-in entity listing; `status` — index readiness.
    /// While the index warms up a doc search returns a retry
    /// envelope. Hits arrive twice: a listing for people in the text block, and the same hits
    /// in `structuredContent` — `{schema_version, hits: [{rank, score, path, line_start,
    /// line_end, symbol, kind, snippet, snippet_truncated_lines}], shown, total,
    /// budget_exhausted?}`. Read the structured form: it is the versioned contract, whereas the
    /// text layout may be reformatted in any release. `score` is the ranker's own number —
    /// comparable within one response, meaningless across searches or backends.
    #[tool(
        name = "search",
        output_schema = tools::search::search_output_schema(),
        annotations(read_only_hint = true)
    )]
    async fn reference_search(
        &self,
        params: Parameters<ReferenceSearchParams>,
        ct: tokio_util::sync::CancellationToken,
    ) -> Result<CallToolResult, McpError> {
        let started = std::time::Instant::now();
        match SearchCommand::from(params.0) {
            SearchCommand::Status => {
                let engine = self.state.search_engine().clone();
                let progress = self.state.index_progress().clone();
                let semantic_runtime = self.state.semantic_runtime();
                let baseline = self.state.baseline_view();
                // The reference/shared path runs no overlay warmup, so its state is always
                // `Pending`; the Summary block words this profile as a reference docs index.
                let overlay_warmup = crate::state::OverlayWarmupState::Pending;
                tokio::task::spawn_blocking(move || {
                    tools::search::search_status(
                        McpProfile::Reference,
                        &engine,
                        &progress,
                        &semantic_runtime,
                        WorkspaceSearchMode::SqliteLocal,
                        overlay_warmup,
                        baseline.configured,
                        baseline.external,
                        baseline.pending,
                    )
                })
                .await
                .map_err(|e| McpError::internal_error(format!("Task error: {e}"), None))?
            }
            command @ (SearchCommand::FindDocs { .. } | SearchCommand::SearchDocs { .. }) => {
                if let crate::state::ReferenceSearchLifecycle::Failed { message, reason_code } =
                    self.state.reference_lifecycle()
                {
                    return Err(McpError::internal_error(
                        format!("reference search initialization failed: {message}"),
                        Some(serde_json::json!({"reasonCode": reason_code})),
                    ));
                }
                let semantic = matches!(&command, SearchCommand::SearchDocs { .. });
                let (query, limit, max_output_tokens) = match command {
                    SearchCommand::FindDocs { query, limit, max_output_tokens }
                    | SearchCommand::SearchDocs { query, limit, max_output_tokens } => {
                        (query, limit, max_output_tokens)
                    }
                    _ => unreachable!(),
                };
                let limit = limit.unwrap_or(10).min(50);
                let max_output_tokens =
                    max_output_tokens.unwrap_or(tools::response::DEFAULT_OUTPUT_BUDGET_TOKENS);
                let engine = self.state.search_engine().clone();
                let baseline = self.state.baseline_view();
                let configured_baseline = baseline.configured;
                let external_baseline = baseline.external;
                let outcome = tools::search::search_call(ct, move |cancel| {
                    if semantic {
                        tools::search::search_docs(
                            &engine,
                            cancel,
                            configured_baseline.as_ref(),
                            external_baseline,
                            &query,
                            limit,
                            max_output_tokens,
                        )
                    } else {
                        tools::search::find_docs(
                            &engine,
                            cancel,
                            configured_baseline.as_ref(),
                            external_baseline,
                            &query,
                            limit,
                            max_output_tokens,
                        )
                    }
                })
                .await;
                let action = if semantic { "search_docs" } else { "find_docs" };
                cancellable_answer(outcome, "search", started, || {
                    tools::search::docs_not_ready(action)
                })
            }
            SearchCommand::ListPlatform(params) => tools::platform::list_platform(
                params.kind,
                params.name.as_deref(),
                params.max_output_tokens.unwrap_or(tools::response::DEFAULT_OUTPUT_BUDGET_TOKENS),
            ),
            SearchCommand::SearchCode(_) => unreachable!("reference schema excludes search_code"),
        }
    }

    /// Ask the ITS expert-help knowledge base a natural-language question about the 1C platform
    /// and development standards. Use for conceptual "how / why" questions. For one member's
    /// signature use `syntax_help`; for doc keyword search use `search`. Params: `question`
    /// (required), optional `max_output_tokens` bounding a long answer.
    #[tool(name = "its_help", annotations(read_only_hint = true))]
    async fn its_help(
        &self,
        params: Parameters<ItsHelpParams>,
    ) -> Result<CallToolResult, McpError> {
        let p = params.0;
        tools::its_help::its_help(
            &p.question,
            p.max_output_tokens.unwrap_or(tools::response::DEFAULT_OUTPUT_BUDGET_TOKENS),
        )
        .await
    }
}

#[tool_router(router = shared_tool_router)]
impl McpServer {
    /// Look up one platform member's reference card — signature, parameters, and description —
    /// from the built-in platform data. Available in both profiles. Use `name` with optional
    /// `type_name` for the legacy lookup, or the mutually exclusive `reference_id` returned by
    /// `search(action="list_platform")` for an exact entity.
    #[tool(
        name = "syntax_help",
        output_schema = tools::platform::syntax_help_output_schema(),
        annotations(read_only_hint = true)
    )]
    async fn syntax_help(
        &self,
        params: Parameters<SyntaxHelpParams>,
    ) -> Result<CallToolResult, McpError> {
        match params.0 {
            SyntaxHelpParams::ByName(p) => tools::platform::bsl_syntax_help(
                &p.name,
                p.type_name.as_deref(),
                p.max_output_tokens.unwrap_or(tools::response::DEFAULT_OUTPUT_BUDGET_TOKENS),
            ),
            SyntaxHelpParams::ByReferenceId(p) => tools::platform::bsl_syntax_help_by_reference_id(
                &p.reference_id,
                p.max_output_tokens.unwrap_or(tools::response::DEFAULT_OUTPUT_BUDGET_TOKENS),
            ),
        }
    }
}

/// The routing prose a client is handed at `initialize`. It names the tools of the
/// profile's default surface; an opt-in tool must never appear here, or a client would be
/// told to reach for something this process does not serve.
fn profile_instructions(profile: McpProfile) -> &'static str {
    match profile {
        McpProfile::Workspace => {
            "BSL Analyzer workspace MCP server for a 1C:Enterprise (BSL) configuration. \
             Route by task (each tool's own description carries the full contract):\n\
             - find code by meaning or unknown name → `search`;\n\
             - who-calls-whom / change impact → `graph` (durable ids; start at overview);\n\
             - one symbol's kind/signature/type/doc/definition/usages by name → `symbol_info`;\n\
             - one file's map (regions, declarations, parameters) → `outline`;\n\
             - analyzer findings (unreachable, type mismatch, unresolved) → `diagnostics` \
             (start at catalog to learn the codes);\n\
             - metadata objects / structure / forms → `metadata`;\n\
             - SDBL query validate/run → `query`; run/check BSL code → `execute`;\n\
             - live infobase runtime events → `event_log`; live debugger session → `debug`.\n\
             Tools whose data is built lazily (metadata, graph, diagnostics, search) return a \
             retry envelope while indexing rather than an error; every response is bounded \
             by `max_output_tokens` (and, where one exists, the action's own count cap) — \
             JSON tools (graph, diagnostics, event_log) flag `budget_exhausted` with a \
             `budget_hint`, text tools (search, metadata, query, execute, debug) append a \
             truncation note. When a count cap fired too, the hint says so: raising \
             `max_output_tokens` alone will not return more."
        }
        McpProfile::Reference => {
            "BSL Analyzer reference MCP server for the 1C platform (no project code). \
             Route by task (each tool's own description carries the full contract):\n\
             - one platform member's signature by name → `syntax_help`;\n\
             - platform docs by keyword or meaning → `search`;\n\
             - conceptual how/why question on the platform or standards → `its_help`.\n\
             Tools: search, syntax_help, its_help. Every response is bounded by \
             `max_output_tokens`; a truncated one appends a continuation note."
        }
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for McpServer {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        info.instructions = Some(profile_instructions(self.profile).into());
        let mut capabilities = ServerCapabilities::builder().enable_tools().enable_resources();
        // Declared only where it is served: without the declaration the dispatcher answers
        // `tasks/get` with `-32601`, and with it a client would read a promise of a branch
        // this build does not take. The handshake is thus the one place a caller learns
        // which of the two answers it can expect.
        if tasks::enabled() {
            capabilities = capabilities.enable_tasks();
        }
        info.capabilities = capabilities.build();
        // NOT `Implementation::from_build_env()`: that macro expands inside rmcp, so it
        // reports rmcp's own name and version — a consumer reading `serverInfo` to learn
        // which analyzer build it is talking to gets the transport library instead.
        info.server_info =
            rmcp::model::Implementation::new("bsl-analyzer", env!("CARGO_PKG_VERSION"));
        info
    }

    /// The contract declaration is served as a resource rather than a tool on purpose: it
    /// is for feature detection by consumers, not work an agent does, and a tool would
    /// spend description tokens in every session to say so.
    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        let resource = Resource::new(contract::CONTRACT_URI, "contract")
            .with_title("Tool and CLI contract")
            .with_description(
                "Machine-readable declaration of this build's surfaces: MCP tools with their \
                 actions and parameters, the CLI commands and flags, and a contract version \
                 separate from the build version.",
            )
            .with_mime_type("application/json");
        Ok(ListResourcesResult::with_all_items(vec![resource]))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, McpError> {
        if request.uri != contract::CONTRACT_URI {
            return Err(McpError::resource_not_found(
                format!("Unknown resource '{}'", request.uri),
                None,
            ));
        }
        let body = serde_json::to_string_pretty(&contract::document())
            .map_err(|e| McpError::internal_error(format!("contract serialization: {e}"), None))?;
        Ok(ReadResourceResult::new(vec![
            ResourceContents::text(body, contract::CONTRACT_URI).with_mime_type("application/json")
        ])
        .into())
    }

    /// The three task methods are pass-throughs on purpose: the registry is the only owner
    /// of a handle's state, and a second place deciding what a status means is a second
    /// place to get it wrong. The dispatcher has already refused a caller that did not
    /// declare the extension before any of them is reached.
    async fn get_task(
        &self,
        request: rmcp::model::GetTaskParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<rmcp::model::GetTaskResult, McpError> {
        Ok(rmcp::model::GetTaskResult::new(self.state.tasks().get_task(&request.task_id)?))
    }

    async fn update_task(
        &self,
        request: rmcp::model::UpdateTaskParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<(), McpError> {
        self.state.tasks().update_task(&request.task_id, request.input_responses)
    }

    async fn cancel_task(
        &self,
        request: rmcp::model::CancelTaskParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<(), McpError> {
        self.state.tasks().cancel_task(&request.task_id)
    }
}

#[cfg(test)]
mod surface_guards {
    use super::*;

    fn tool_input_schema(profile: McpProfile, name: &str) -> serde_json::Value {
        let router = McpServer::gated_router(profile, &ToolGate::for_launch(profile, &[]));
        let tools = router.list_all();
        let tool = tools
            .iter()
            .find(|tool| tool.name == name)
            .unwrap_or_else(|| panic!("{name} is not served by {}", profile.as_str()));
        serde_json::Value::Object(tool.input_schema.as_ref().clone())
    }

    fn tool_output_schema(profile: McpProfile, name: &str) -> serde_json::Value {
        let router = McpServer::gated_router(profile, &ToolGate::for_launch(profile, &[]));
        let tool = router
            .list_all()
            .iter()
            .find(|tool| tool.name == name)
            .unwrap_or_else(|| panic!("{name} is not served by {}", profile.as_str()))
            .clone();
        serde_json::Value::Object(
            tool.output_schema.expect("published outputSchema").as_ref().clone(),
        )
    }

    fn string_values<'a>(value: &'a serde_json::Value, key: &str, out: &mut Vec<&'a str>) {
        match value {
            serde_json::Value::Object(object) => {
                if let Some(value) = object.get(key).and_then(serde_json::Value::as_str) {
                    out.push(value);
                }
                for value in object.values() {
                    string_values(value, key, out);
                }
            }
            serde_json::Value::Array(values) => {
                for value in values {
                    string_values(value, key, out);
                }
            }
            _ => {}
        }
    }

    fn array_for_key<'a>(
        value: &'a serde_json::Value,
        key: &str,
    ) -> Option<&'a Vec<serde_json::Value>> {
        match value {
            serde_json::Value::Object(object) => object
                .get(key)
                .and_then(serde_json::Value::as_array)
                .or_else(|| object.values().find_map(|value| array_for_key(value, key))),
            serde_json::Value::Array(values) => {
                values.iter().find_map(|value| array_for_key(value, key))
            }
            _ => None,
        }
    }

    fn required_sets<'a>(value: &'a serde_json::Value, out: &mut Vec<Vec<&'a str>>) {
        match value {
            serde_json::Value::Object(object) => {
                if let Some(required) = object.get("required").and_then(serde_json::Value::as_array)
                {
                    out.push(required.iter().filter_map(serde_json::Value::as_str).collect());
                }
                for value in object.values() {
                    required_sets(value, out);
                }
            }
            serde_json::Value::Array(values) => {
                for value in values {
                    required_sets(value, out);
                }
            }
            _ => {}
        }
    }

    #[test]
    fn profile_search_inputs_are_tagged_and_legacy_branches_stay_permissive() {
        let workspace_schema = tool_input_schema(McpProfile::Workspace, "search");
        let reference_schema = tool_input_schema(McpProfile::Reference, "search");
        assert_eq!(workspace_schema["oneOf"].as_array().map(Vec::len), Some(5));
        assert_eq!(reference_schema["oneOf"].as_array().map(Vec::len), Some(4));

        let mut workspace_actions = Vec::new();
        string_values(&workspace_schema, "const", &mut workspace_actions);
        for action in ["search_code", "status", "list_platform", "find_docs", "search_docs"] {
            assert!(workspace_actions.contains(&action), "missing {action}: {workspace_schema}");
        }
        let mut reference_actions = Vec::new();
        string_values(&reference_schema, "const", &mut reference_actions);
        assert!(!reference_actions.contains(&"search_code"));
        for action in ["status", "list_platform", "find_docs", "search_docs"] {
            assert!(reference_actions.contains(&action), "missing {action}: {reference_schema}");
        }
        assert!(workspace_schema.to_string().contains("\"minLength\":1"));
        for kind in ["type", "method", "property", "constructor", "global_function"] {
            assert!(workspace_schema.to_string().contains(&format!("\"{kind}\"")));
        }

        assert!(serde_json::from_value::<WorkspaceSearchParams>(serde_json::json!({
            "action": "search_code", "query": "x", "future": true
        }))
        .is_ok());
        assert!(serde_json::from_value::<ReferenceSearchParams>(serde_json::json!({
            "action": "find_docs", "query": "x", "future": true
        }))
        .is_ok());
        assert!(serde_json::from_value::<WorkspaceSearchParams>(serde_json::json!({
            "action": "list_platform", "future": true
        }))
        .is_err());
        assert!(serde_json::from_value::<WorkspaceSearchParams>(serde_json::json!({
            "action": "find_docs", "query": "x", "future": true
        }))
        .is_err());
        assert!(serde_json::from_value::<WorkspaceSearchParams>(serde_json::json!({
            "action": "search_code", "query": ""
        }))
        .is_err());
        assert!(serde_json::from_value::<ReferenceSearchParams>(serde_json::json!({
            "action": "search_code", "query": "x"
        }))
        .is_err());
    }

    #[test]
    fn profile_search_outputs_publish_every_discriminated_branch() {
        let workspace = tool_output_schema(McpProfile::Workspace, "search");
        let reference = tool_output_schema(McpProfile::Reference, "search");
        assert_eq!(workspace, reference);
        let encoded = workspace.to_string();
        for value in [
            "search_code",
            "find_docs",
            "search_docs",
            "list_platform",
            "status",
            "not_ready",
            "ready",
            "loading",
            "busy",
            "failed",
        ] {
            assert!(encoded.contains(&format!("\"{value}\"")), "missing {value}: {encoded}");
        }
        assert!(encoded.contains("\"schema_version\""));
    }

    #[test]
    fn symbol_info_output_schema_requires_discriminators_and_member_branches() {
        let schema = tool_output_schema(McpProfile::Workspace, "symbol_info");
        assert_eq!(array_for_key(&schema, "oneOf").map(Vec::len), Some(4), "{schema}");

        let mut constants = Vec::new();
        string_values(&schema, "const", &mut constants);
        for value in ["1", "ok", "not_found", "ambiguous", "loading"] {
            assert!(constants.contains(&value), "missing const {value}: {schema}");
        }

        let mut required = Vec::new();
        required_sets(&schema, &mut required);
        assert!(required.iter().any(|keys| {
            ["availability", "type", "type_variants"].iter().all(|key| keys.contains(key))
        }));
        assert!(required
            .iter()
            .any(|keys| ["availability", "signature"].iter().all(|key| keys.contains(key))));
    }

    #[test]
    fn syntax_help_input_is_an_xor_with_a_closed_reference_branch() {
        for profile in [McpProfile::Workspace, McpProfile::Reference] {
            let schema = tool_input_schema(profile, "syntax_help");
            assert_eq!(array_for_key(&schema, "oneOf").map(Vec::len), Some(2), "{schema}");
            assert!(schema.to_string().contains("reference_id"));
            assert!(schema.to_string().contains("additionalProperties"));
            assert!(schema.to_string().contains("max_output_tokens"));
        }

        assert!(serde_json::from_value::<SyntaxHelpParams>(serde_json::json!({
            "name": "Массив", "future": true
        }))
        .is_ok());
        assert!(serde_json::from_value::<SyntaxHelpParams>(serde_json::json!({
            "reference_id": "type::x~digest", "max_output_tokens": 100
        }))
        .is_ok());
        assert!(serde_json::from_value::<SyntaxHelpParams>(serde_json::json!({
            "reference_id": "type::x~digest", "future": true
        }))
        .is_err());
        assert!(serde_json::from_value::<SyntaxHelpParams>(serde_json::json!({
            "name": "Массив", "reference_id": "type::x~digest"
        }))
        .is_err());
    }

    #[test]
    fn symbol_info_input_schema_accepts_name_with_optional_complete_position() {
        let schema = tool_input_schema(McpProfile::Workspace, "symbol_info");
        let branches = schema["oneOf"].as_array().expect("symbol_info oneOf");
        assert_eq!(branches.len(), 2);
        assert_eq!(branches[0]["required"], serde_json::json!(["symbol"]));
        assert_eq!(branches[1]["required"], serde_json::json!(["path", "line"]));
        assert!(schema["properties"].get("root_id").is_some());
        assert!(schema["properties"].get("column").is_some());
        let encoded = schema.to_string();
        for kind in [
            "attribute",
            "tabular_section",
            "method",
            "variable",
            "property",
            "form_attribute",
            "form_element",
            "handler",
        ] {
            assert!(encoded.contains(&format!("\"{kind}\"")), "missing {kind}: {schema}");
        }
        assert!(schema["properties"].get("member_name").is_some());
    }

    /// The routing prose must not send a client to a tool this process may not be serving.
    ///
    /// Forward guard: the build declares no opt-in tool yet, so the loop body does not run
    /// and this cannot fail today. It gains teeth with the first opt-in tool, whose author
    /// would otherwise have to remember that the reference profile's prose ends with a
    /// literal `Tools: search, syntax_help, its_help`.
    #[test]
    fn instructions_do_not_advertise_opt_in_tools() {
        for profile in [McpProfile::Workspace, McpProfile::Reference] {
            let prose = profile_instructions(profile);
            for name in contract::opt_in_tools(profile) {
                assert!(
                    !prose.contains(name),
                    "{}: instructions name the opt-in tool `{name}`",
                    profile.as_str()
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn all_status_requests_are_cached_under_held_lease() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        crate::graph::test_support::sample_workspace(root);
        std::fs::write(root.join("Configuration.xml"), "<Configuration/>").unwrap();
        let cache = crate::cache::WorkspaceCacheLayout::for_workspace(root);
        let lease_path = cache.lease_path();
        let state = SharedState::workspace_with_cache(root.to_path_buf(), cache).unwrap();
        let lease = state.workspace_lease().clone();
        let server = McpServer::new(McpProfile::Workspace, state);
        let held_lease = lease.hold_file_lock_for_test();
        std::fs::remove_file(lease_path).unwrap();
        lease.invalidate_verdict_for_test();
        let token = || tokio_util::sync::CancellationToken::new();

        tokio::time::timeout(
            Duration::from_millis(500),
            server.metadata(
                Parameters(MetadataParams {
                    action: "status".to_owned(),
                    filter: None,
                    meta_type: None,
                    name_mask: None,
                    max_items: None,
                    object_type: None,
                    object_name: None,
                    form_name: None,
                    max_output_tokens: None,
                    connection: None,
                    mode: None,
                }),
                token(),
                tasks::TaskCapable(false),
            ),
        )
        .await
        .expect("metadata status must not wait for the lease lock")
        .expect("metadata status response");

        tokio::time::timeout(
            Duration::from_millis(500),
            server.diagnostics(
                Parameters(DiagnosticsParams {
                    action: "status".to_owned(),
                    path: None,
                    root_id: None,
                    codes: Vec::new(),
                    locale: None,
                    min_severity: None,
                    range_start: None,
                    range_end: None,
                    detail: None,
                    max_findings: None,
                    max_files: None,
                    max_output_tokens: None,
                }),
                token(),
                tasks::TaskCapable(false),
            ),
        )
        .await
        .expect("diagnostics status must not wait for the lease lock")
        .expect("diagnostics status response");

        tokio::time::timeout(
            Duration::from_millis(500),
            server.graph(
                Parameters(GraphParams {
                    action: "status".to_owned(),
                    id: None,
                    query: None,
                    ids: Vec::new(),
                    max_output_tokens: None,
                    detail: None,
                    dir: None,
                    depth: None,
                    max_nodes: None,
                    provenance: Vec::new(),
                    edge_kinds: Vec::new(),
                    top: None,
                    call_sites: None,
                    max_call_sites: None,
                }),
                token(),
            ),
        )
        .await
        .expect("graph status must not wait for the lease lock")
        .expect("graph status response");

        drop(held_lease);
        server.shutdown();
    }
}

#[cfg(test)]
mod graph_supersession_contract {
    use super::*;
    use std::time::Duration;

    fn params(action: &str, id: Option<&str>) -> Parameters<GraphParams> {
        Parameters(GraphParams {
            action: action.to_owned(),
            id: id.map(str::to_owned),
            query: None,
            ids: Vec::new(),
            max_output_tokens: None,
            detail: None,
            dir: None,
            depth: None,
            max_nodes: None,
            provenance: Vec::new(),
            edge_kinds: Vec::new(),
            top: None,
            call_sites: None,
            max_call_sites: None,
        })
    }

    fn resolve_params(query: &str) -> Parameters<GraphParams> {
        let mut params = params("resolve", None).0;
        params.query = Some(query.to_owned());
        Parameters(params)
    }

    fn symbol_params(symbol: &str) -> Parameters<SymbolInfoParams> {
        Parameters(SymbolInfoParams {
            symbol: Some(symbol.to_owned()),
            root_id: None,
            path: None,
            line: None,
            column: None,
            include: Vec::new(),
            member_kind: None,
            member_name: None,
            locale: None,
            max_output_tokens: None,
        })
    }

    fn references_params(symbol: &str) -> Parameters<ReferencesParams> {
        Parameters(ReferencesParams {
            symbol: Some(symbol.to_owned()),
            anchor_root_id: None,
            root_id: None,
            path: None,
            line: None,
            column: None,
            line_content: None,
            area_root_id: None,
            area_path_prefix: None,
            kinds: Vec::new(),
            include_declaration: None,
            limit: None,
            max_files: None,
            include_preview: None,
            max_output_tokens: None,
        })
    }

    enum BusyGraphHandler {
        Graph,
        ResolveNames,
        SymbolInfo,
        References,
    }

    async fn assert_busy_graph_handler_returns_immediately(handler: BusyGraphHandler) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        crate::graph::test_support::sample_workspace(root);
        std::fs::write(root.join("Configuration.xml"), "<Configuration/>").unwrap();
        let cache = crate::cache::WorkspaceCacheLayout::for_workspace(root);
        let state = SharedState::workspace_with_cache(root.to_path_buf(), cache.clone()).unwrap();
        state.diagnostics().ensure_loading();
        crate::diagnostics_state::test_support::wait_ready(state.diagnostics());
        let graph = state.graph().clone();
        graph.ensure_loading();
        crate::graph::test_support::wait_ready(&graph);
        let held: Vec<_> = (0..crate::graph::SNAPSHOT_POOL_CAP)
            .map(|_| graph.snapshot().expect("the published pool has four handles"))
            .collect();
        let lock = crate::workspace_lease::WorkspaceLease::hold_cache_lock_for(
            &cache,
            Duration::from_secs(3),
        );
        let server = McpServer::new(McpProfile::Workspace, state);
        let token = || tokio_util::sync::CancellationToken::new();

        let answer = match handler {
            BusyGraphHandler::Graph => tokio::time::timeout(
                Duration::from_millis(500),
                server.graph(params("overview", None), token()),
            )
            .await
            .expect("graph handler must not wait for the lease lock")
            .expect("an exhausted pool is a temporary loading answer"),
            BusyGraphHandler::ResolveNames => tokio::time::timeout(
                Duration::from_millis(500),
                server.graph(resolve_params("Считать"), token()),
            )
            .await
            .expect("resolve_names must not wait for the lease lock")
            .expect("resolve still answers from its other providers"),
            BusyGraphHandler::SymbolInfo => {
                let response = tokio::time::timeout(
                    Duration::from_millis(500),
                    server.symbol_info(
                        symbol_params("Сервер.Считать"),
                        token(),
                        tasks::TaskCapable(false),
                    ),
                )
                .await
                .expect("symbol_info must not wait for the lease lock")
                .expect("the resident card survives unavailable graph enrichment");
                match response {
                    rmcp::model::CallToolResponse::Complete(result) => result,
                    _ => panic!("a caller that declared no task extension is answered inline"),
                }
            }
            BusyGraphHandler::References => {
                let response = tokio::time::timeout(
                    Duration::from_millis(500),
                    server.references(
                        references_params("Сервер.Считать"),
                        token(),
                        tasks::TaskCapable(false),
                    ),
                )
                .await
                .expect("references must not wait for the lease lock")
                .expect("resident references survive unavailable graph enrichment");
                match response {
                    rmcp::model::CallToolResponse::Complete(result) => result,
                    _ => panic!("a caller that declared no task extension is answered inline"),
                }
            }
        };
        assert!(answer.structured_content.is_some());

        drop(held);
        lock.join().unwrap();
        server.shutdown();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn graph_handler_misses_immediately_when_preopened_handles_are_busy() {
        assert_busy_graph_handler_returns_immediately(BusyGraphHandler::Graph).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn resolve_names_misses_immediately_when_preopened_handles_are_busy() {
        assert_busy_graph_handler_returns_immediately(BusyGraphHandler::ResolveNames).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn symbol_info_misses_immediately_when_preopened_handles_are_busy() {
        assert_busy_graph_handler_returns_immediately(BusyGraphHandler::SymbolInfo).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn references_misses_immediately_when_preopened_handles_are_busy() {
        assert_busy_graph_handler_returns_immediately(BusyGraphHandler::References).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn graph_actions_fail_when_superseded_snapshot_is_gone() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        crate::graph::test_support::sample_workspace(root);
        std::fs::write(root.join("Configuration.xml"), "<Configuration/>").unwrap();
        let cache = crate::cache::WorkspaceCacheLayout::for_workspace(root);
        let state = SharedState::workspace_with_cache(root.to_path_buf(), cache.clone()).unwrap();
        let graph = state.graph().clone();
        graph.ensure_loading();
        crate::graph::test_support::wait_ready(&graph);
        let held = graph.snapshot().expect("the old daemon owns one open descriptor");
        let server = McpServer::new(McpProfile::Workspace, state);
        let newer = crate::workspace_lease::WorkspaceLease::claim_cache(&cache);
        assert!(graph.is_superseded(), "background ownership probing latches takeover");
        std::fs::write(cache.graph_db_path(), b"replacement").unwrap();

        let schema = server
            .graph(params("schema", None), tokio_util::sync::CancellationToken::new())
            .await
            .expect("schema stays static");
        assert!(schema.structured_content.is_some());

        for (action, id) in
            [("overview", None), ("node", Some("method/CommonModule.X.Y")), ("overview", None)]
        {
            let error = server
                .graph(params(action, id), tokio_util::sync::CancellationToken::new())
                .await
                .expect_err(action);
            assert_eq!(error.message, crate::graph::SUPERSEDED_GRAPH_ERROR, "{action}");
        }

        assert!(graph.is_superseded());
        drop(held);
        newer.release();
        server.shutdown();
    }
}

#[cfg(test)]
mod tool_descriptions {
    use super::*;
    use expect_test::expect;
    use std::fmt::Write;

    /// Render a profile's `tools/list` into a stable text contract: every tool (sorted by
    /// name) with its description and each parameter field's description. A refactor that
    /// drops a tool description or a field doc changes this snapshot loudly, instead of
    /// silently shipping an empty contract to agents. The machine-readable declaration
    /// consumers read lives in [`crate::contract`]; this guards the prose agents read.
    /// Rebase with `UPDATE_EXPECT=1 cargo test -p mcp-server tool_descriptions`.
    fn render(tools: &[rmcp::model::Tool]) -> String {
        let mut tools: Vec<&rmcp::model::Tool> = tools.iter().collect();
        tools.sort_by(|a, b| a.name.cmp(&b.name));
        let mut out = String::new();
        for tool in tools {
            let _ = writeln!(out, "## {}", tool.name);
            let _ =
                writeln!(out, "{}", tool.description.as_deref().unwrap_or("<MISSING DESCRIPTION>"));
            let props = contract::schema_properties(&tool.input_schema);
            if !props.is_empty() {
                let mut keys: Vec<&String> = props.keys().collect();
                keys.sort();
                for key in keys {
                    let desc = props[key]
                        .get("description")
                        .and_then(|d| d.as_str())
                        .unwrap_or("<no doc>");
                    let _ = writeln!(out, "  - {key}: {desc}");
                }
            }
            out.push('\n');
        }
        out
    }

    /// The description is what an agent reads BEFORE any outcome, so it must not name an
    /// axis the answer may contradict: an object module and its manager module are one
    /// `ambiguous` in one root, where `anchor_root_id` separates nothing. The clause points
    /// at the field that names the axis per answer, and this gate reads that clause alone —
    /// `anchor_root_id` is a legitimate word elsewhere in the same text.
    #[test]
    fn the_references_description_defers_to_the_answers_own_hint() {
        let enabled = vec!["references".to_string()];
        let router = McpServer::gated_router(
            McpProfile::Workspace,
            &ToolGate::for_launch(McpProfile::Workspace, &enabled),
        );
        let tools = router.list_all();
        let references = tools
            .iter()
            .find(|tool| tool.name == "references")
            .expect("the workspace router declares `references`");
        let description = references.description.as_deref().expect("a description");
        let start = description.find("`ambiguous`").expect("the description names `ambiguous`");
        let end = description[start..]
            .find("`not_found`")
            .map(|offset| start + offset)
            .expect("the description names `not_found`");
        let clause = &description[start..end];

        let zero = description
            .find("proven zero")
            .map(|at| &description[at..])
            .expect("the description promises a proven zero");
        assert!(
            zero.starts_with("proven zero unless")
                || zero[..80.min(zero.len())].contains("total_is_lower_bound"),
            "a walk cut short by `max_files` counts a floor, so the zero it reports proves \
             nothing: {zero}",
        );

        assert!(
            clause.contains("resolution_hint"),
            "the ambiguity clause must send the agent to the field that names the axis: {clause}",
        );
        assert!(
            !clause.contains("anchor_root_id"),
            "one axis cannot be promised for every ambiguity — same-root declarations have \
             none: {clause}",
        );
    }

    #[test]
    fn workspace_tools_contract() {
        let router = McpServer::gated_router(
            McpProfile::Workspace,
            &ToolGate::for_launch(McpProfile::Workspace, &[]),
        );
        let rendered = render(&router.list_all());
        assert!(!rendered.contains("<no doc>"), "every published parameter needs prose docs");
        expect![[r###"
            ## debug
            Drive a live 1C debugger session: attach, set breakpoints, step, and inspect state. Use
            to debug a running infobase — attach, break, then step and read locals/eval. Not for
            static analysis (use `diagnostics`) and not for running standalone code (use `execute`).
            Actions: `attach`/`disconnect`; `set_breakpoint`/`remove_breakpoint`; `continue`/`step`;
            `wait_stop`; `stack_trace`; `locals`; `eval`. State-reading actions are bounded by
            `max_output_tokens` and append a truncation note. Requires a reachable debug endpoint
            (`host` + `infobase`, default port 1550).
              - action: attach | disconnect | set_breakpoint | remove_breakpoint | continue | step |
            wait_stop | stack_trace | locals | eval.
              - auto_attach: `attach`: object-name patterns to auto-attach on connect (optional).
              - condition: `set_breakpoint`: conditional-breakpoint expression (optional).
              - config_root: `attach`: configuration source root (optional).
              - direction: `step`: `over`/`next`, `in`/`into`, or `out` (required).
              - expression: `eval`: BSL expression to evaluate in the current stop (required).
              - extensions: `attach`: `[name, root]` pairs for loaded extensions (optional).
              - host: `attach`: debugger host (required).
              - infobase: `attach`: infobase name (required).
              - line: `set_breakpoint`/`remove_breakpoint`: 1-based line (required).
              - max_output_tokens: Output budget in tokens (~4 chars each) for the state-reading actions `stack_trace`,
            `locals`, `wait_stop` and `eval`: a deep stack or a wide frame is truncated at a line
            boundary with a continuation note (default 6000).
              - module: `set_breakpoint`/`remove_breakpoint`: target module id (required).
              - port: `attach`: debugger port (default 1550).
              - stack_level: `locals`/`eval`: stack frame level to evaluate in (optional, default top frame).
              - timeout_secs: `wait_stop`: max seconds to wait for a stop event (optional).

            ## diagnostics
            Semantic analyzer findings the compiler and grep cannot give you — unreachable code,
            type mismatch, unresolved calls, and 180+ other rules. Use to check a file or the whole
            config for issues, or to discover which rules exist. Not for finding code (use `search`)
            and not for call relationships (use `graph`). Actions: `catalog` — list rules (start here
            to learn the codes); `schema` — response shape; `status` — analysis readiness; `file` —
            per-finding results for one `.bsl` path; `workspace` — a bounded per-code aggregate sweep
            of the whole config. Honours `max_output_tokens`/`max_findings` and flags truncation.
            Reads the resident host; while it builds it returns a retry envelope.
              - action: catalog | schema | status | file | workspace.
              - codes: `catalog`: narrow to these codes. `file`: keep only these codes.
              - detail: `file`: concise | detailed (default concise).
              - locale: `catalog`: ru | en (default ru) — title language.
              - max_files: `workspace`: cap on files swept (default 1000).
              - max_findings: `file`: cap on returned findings (default 200).
              - max_output_tokens: `catalog`/`file`/`workspace`: output budget in tokens (~4 chars each), minimum 256;
            a truncated response carries `budget_exhausted: true` and a `budget_hint` on how to narrow it
            (tighten `codes`/`min_severity`/range or raise the budget). When omitted, no token
            budget applies — only the action's own count caps (`max_findings`/`max_files`).
              - min_severity: `file`: inclusive severity floor error|warning|info|hint (default warning).
              - path: `file`: absolute or workspace-relative `.bsl` path, or a path relative to `root_id`.
              - range_end: `file`: 0-based last line to include (optional).
              - range_start: `file`: 0-based first line to include (optional).
              - root_id: `file`: the source root `path` is spelled against, as carried by every `search` code
            hit. Omit for a path already spelled against the workspace; `""` names the
            configuration.

            ## event_log
            Read the 1C infobase event log (журнал регистрации) through the deployed BSL_Analyzer
            extension. Use to inspect runtime events — errors, authentications, data changes —
            filtered by time, user, event, metadata object, or severity. Not for static analysis of
            source (use `diagnostics`): this reads live runtime records from a running infobase.
            Filters: `date_from`/`date_to`, `level`, `user`, `event`, `metadata`, and `contains`
            (post-read substring over the newest `limit` window). `limit` is newest-first (default
            100, max 1000) and bounds the record COUNT; `max_output_tokens` bounds the response
            SIZE and flags `budget_exhausted`. Requires the extension deployed with event-log read
            rights.
              - connection: Named live 1C connection (optional when only one/default connection exists).
              - contains: Case-insensitive substring filter over the comment/data columns, applied AFTER the
            platform read — so it narrows the already-`limit`-capped newest window, it does not
            scan the whole log. Widen `limit` if a match may lie deeper.
              - date_from: Lower time bound (inclusive), ISO-8601, e.g. `2026-07-05T00:00:00` or `2026-07-05`.
              - date_to: Upper time bound (inclusive), ISO-8601.
              - event: Event name, e.g. `_$Session$_.Authentication` or a metadata event like `_$Data$_.Post`.
              - level: Severity: Информация/Предупреждение/Ошибка/Примечание or Information/Warning/Error/Note.
              - limit: Max records (newest first), default 100, capped at 1000.
              - max_output_tokens: Output budget in tokens (~4 chars each) on top of the `limit` record cap — `limit`
            counts records, it does not bound their size. An over-budget read drops the oldest
            records, flags `budget_exhausted: true` and carries a `budget_hint` (default 6000);
            when the record cap fired too, the hint says raising the budget alone will not help.
            In the response, `returned` counts the records actually delivered and `total` the ones
            the platform read for this `limit` window — neither is the whole matching population,
            which the platform never reports.
              - metadata: Full metadata name to filter by, e.g. `Документ.ЗаказКлиента`.
              - user: Infobase user name (deleted users can only be matched by name).

            ## execute
            Run or syntax-check BSL code in an embedded interpreter. Use to confirm a snippet
            compiles, run a small script, or evaluate a single expression. Not for querying the
            database (use `query` for SDBL) and not for analyzer findings (use `diagnostics`).
            Actions: `check` — syntax-check `code`; `run` — execute `code`; `eval` — evaluate the
            single expression in `code`. `run`/`eval` execute code, so this tool is not read-only.
            Output is bounded by `max_output_tokens` and appends a truncation note.
              - action: check | run | eval.
              - code: BSL source to `check`/`run`, or the single expression to `eval`.
              - connection: Named live 1C connection (optional when only one/default connection exists).
              - max_output_tokens: Output budget in tokens (~4 chars each); over-budget output (a `run` context block, an
            evaluated value, a long syntax-error listing) is truncated with a note (default 6000).

            ## graph
            Whole-config semantic call graph: traverse who-calls-whom and object/metadata usage by
            durable node id. Use to understand call relationships and change impact — start with
            `overview` on an unfamiliar project, then `node`/`callers`/`callees`/`neighbors` on the
            ids it returns. Not for finding code by meaning (use `search`) and not for analyzer
            findings (use `diagnostics`). Actions: `overview`, `schema`, `status`, `resolve`
            (imprecise name → candidates, each with every address that works), `node`, `source`,
            `neighbors`, `callers`, `callees`. Source-bearing actions honour `max_output_tokens`
            and flag `budget_exhausted` on truncation. Lazily indexes on first use; while it
            builds, traversal returns a retry envelope — but `resolve` answers anyway, from the
            name dictionary, and names every source it could not consult.
              - action: overview | schema | status | node | source | neighbors | callers | callees | resolve
              - call_sites: Ask each edge where its call is written — for `neighbors`/`callers`/`callees`
            (default: false). Off, an edge carries no `call_site*` key at all, which is how an
            edge nobody asked about is told apart from one that has no place.
              - depth: Traversal depth for neighbors (default: 1).
              - detail: names | signatures | bodies (default: signatures).
              - dir: in | out | both — only for `neighbors` (default: in).
              - edge_kinds: Keep only edges of these kinds (call/manager_creates/manager_access/query_ref/
            contains/data_binding) — lets metadata-impact queries isolate e.g. only `query_ref`.
              - id: Durable node id (required for node/neighbors/callers/callees).
              - ids: Durable node ids (required for `source`).
              - max_call_sites: Cap on places per edge when `call_sites` is on (default: 20, max: 200). What the cap
            cuts is declared: `call_sites_total` counts the places before any are shown.
              - max_nodes: Server-side cap on returned neighbour nodes (default: 50).
              - max_output_tokens: Output budget in tokens (~4 chars each) for source-bearing actions: `source`
            (default 4000) and `node`/`neighbors` at `detail=bodies` (default 6000). When the
            body output is truncated the response carries `budget_exhausted: true`.
              - provenance: Keep only edges with these provenances (resolved/inferred/visibility_blocked/unresolved).
              - query: Imprecise lookup string (required for `resolve`): wrong casing, a bare method/object
            name, or a partial id.
              - top: How many top-centrality methods to include in `overview` (default: 20).

            ## metadata
            Browse the configuration's metadata: objects, their structure, and managed forms.
            Use to answer "what objects exist / what does object X contain / what is on form Y" —
            attributes, tabular sections, forms, types — straight from the metadata substrate. Not
            for call relationships (use `graph`) and not for finding code by meaning (use `search`).
            Actions: `info` — configuration summary; `tree` — the metadata object tree (filterable);
            `object` — one object's structure (needs `object_type` + `object_name`); `form` — a
            source-only managed form layout (needs `object_type`); `status` — resident readiness.
            For `object`, use a singular analyzer type (`Документ`) in source mode or auto without
            `connection`, and a plural live-service collection (`Документы`) in infobase mode or auto
            with `connection`; the server does not convert between the forms. Reads the
            resident analysis host; while it builds it returns a retry envelope, not an error —
            `structuredContent.status == "loading"`, same field `diagnostics`/`graph` set, so retry
            shortly instead of reading the answer as "no such object".
              - action: info | tree | object | form | status.
              - connection: Named live 1C connection for `mode=infobase` (optional).
              - filter: `tree`: case-insensitive substring to narrow the returned tree (optional).
              - form_name: `form`: managed-form name (optional; omit for the object's default form).
              - max_items: `tree` in infobase mode: maximum returned objects (default 100, max 1000).
              - max_output_tokens: `tree` (filtered listing): output budget in tokens (~4 chars each); an over-budget
            listing is truncated at a line boundary with a continuation note (default 6000).
              - meta_type: `tree` in infobase mode: metadata collection, e.g. `Справочники`/`Documents`.
              - mode: auto | source | infobase (default auto).
              - name_mask: `tree` in infobase mode: case-insensitive object name/synonym substring.
              - object_name: Metadata object name, e.g. `ЗаказКлиента`. Required for `object`; for `form` it
            selects the owner object (omit for a configuration-level common form).
              - object_type: Mode-dependent metadata object type. Use a singular analyzer type for `mode=source`
            and `mode=auto` without `connection`, e.g. `Документ`/`Справочник`/`ОбщийМодуль`.
            Use a plural live-service collection for `mode=infobase` and `mode=auto` with
            `connection`, e.g. `Документы`/`Справочники`. Forms are source-only. Required for
            `object` and `form`; values are passed through without singular/plural conversion.

            ## outline
            The map of ONE `.bsl` file: its `#Область` tree, the procedures, functions and module
            variables it declares, each with its export flag, compilation directives, parameters
            (with `Знач` and default values) and 0-based UTF-16 ranges. Method BODIES are never
            returned — read the file for those, or ask `search`. Use it to orient yourself in an
            unfamiliar module before reading it, or to find where a declaration sits. Params: `path`
            (required) with optional `root_id` naming the source root it is spelled against, `mode`
            (`full` | `regions` — the region skeleton alone, for a big module), `max_output_tokens`.
            Answers from one parse of that file: no index is built, nothing is ever `loading`, and
            the answer is the same whether or not the workspace has been analysed.
              - max_output_tokens: Output budget in tokens (~4 chars each); an over-budget map stops partway through the
            file and the response carries `truncated: true` with a `budget_hint` (default 6000).
              - mode: `full` (default) — every region and declaration; `regions` — the region skeleton alone,
            for a module too big to read method by method.
              - path: Absolute or workspace-relative `.bsl` path, or a path relative to `root_id`.
              - root_id: The source root `path` is spelled against, as carried by every `search` code hit. Omit
            for a path already spelled against the workspace; `""` names the configuration. An
            extension repeats the configuration's layout, so the same relative path exists under
            several roots and the pair is what names one file.

            ## query
            Validate or execute SDBL (the 1C query language) against the configuration schema. Use
            to check a query for errors before shipping it, to run a read-only query, or to fetch
            the query-language schema. Not for browsing metadata structure (use `metadata`) and not
            for BSL code (use `execute`). Actions: `validate` — parse and type-check a query (`query`
            required); `execute` — run it (`query` required; optional `limit`, `parameters`);
            `schema` — the SDBL schema reference. `execute` output is bounded by `max_output_tokens`
            on top of `limit` and appends a truncation note.
              - action: validate | execute | schema.
              - connection: Named live 1C connection (optional when only one/default connection exists).
              - limit: `execute`: cap on returned rows (optional).
              - max_output_tokens: `execute`: output budget in tokens (~4 chars each) on top of the `limit` row cap —
            `limit` bounds how many rows come back, nothing bounds how wide they are. An
            over-budget table is truncated at a row boundary with a note (default 6000); when the
            row cap fired too, the note says raising the budget alone will not help.
              - parameters: `execute`: named SDBL query parameters (`&Param` → value) (optional).
              - query: SDBL text — required for `validate`/`execute`, omitted for `schema`.
              - root_id: `validate`: the configuration root the query is meant for — `""` for the configuration,
            an extension's id otherwise. An assertion about context, not a narrowing knob: an id
            that is not registered fails the call, a registered one is echoed back in `context`.

            ## search
            Search project code or the built-in platform reference from the workspace profile.
            Actions: `search_code` searches project source; `find_docs`/`search_docs` search platform
            reference text; `list_platform` lists exact platform entities; `status` reports project
            code-index readiness. Not for walking call relationships — use `graph` — or analyzer
            findings — use `diagnostics`. While an index warms up its search action returns a retry
            envelope; retry shortly. Hits arrive twice: a listing for people in the text block, and
            the same hits in `structuredContent` — `{schema_version, hits: [{rank, modality, root_id, path,
            line_start, line_end, location, symbol, kind, graph_id, snippet, snippet_truncated_lines}],
            shown, total, budget_exhausted?, degraded?, freshness}`. `location` is the shared location
            contract (0-based, end-exclusive, UTF-16 columns, `(root_id, path)` as the file key) and
            `freshness` carries machine-readable completeness; the 1-based `line_start`/`line_end`
            stay as they were. Read the structured form: it is the versioned
            contract, whereas the text layout may be reformatted in any release. Absent fields mean
            absent facts — no `symbol` is a file/header chunk, no `graph_id` means the hit has no
            durable id to pass to `graph`. `total` is the ranked list before the output budget cut
            it (already bounded by `limit`), not the configuration-wide match count.
              - action: Requested search, platform-listing, or lifecycle action.
              - kind: Optional platform entity kind: type, method, property, constructor, or global_function.
              - limit: Cap on returned platform-reference hits (default 10, max 50).
              - max_output_tokens: Output budget in tokens (~4 chars each) for text and structured content together.
              - name: Optional case-insensitive substring of the Russian or English platform entity name.
              - query: Free-text query for the selected search action.

            ## symbol_info
            One symbol's consolidated card: kind, signature, type, doc, definition site, and a
            usages summary — by qualified name. Use to answer "what is X / where is it defined / what
            does it return / who calls it" for a single symbol in ONE call, instead of chaining
            hover + definition + references. Pass `symbol` (a common-module method
            `ОбщегоНазначения.ЗначениеРеквизитаОбъекта`, a metadata object `Справочник.Товары` or its
            attribute `Справочник.Товары.Артикул`, an object/manager method
            `Документ.ЗаказКлиента.Провести`, or a platform member `СтрНайти`); for a local/parameter
            with no qualified name pass `path`+`line` instead. An imprecise `symbol` returns candidate
            ids (not an error) — resolve one, or open it in `graph`. Not for finding code by meaning
            (use `search`), whole-object browsing (use `metadata`), or the full caller list (use
            `graph` with the returned `graph_id`). Reads the resident host; while it builds it returns
            a retry envelope. The `usages` summary needs the call graph; if it is still indexing the
            core card is still served with `usages_unavailable`.
              - column: `path`: 0-based offset within the line of the symbol occurrence, counted in UTF-16
            units — the unit every column this server publishes is counted in, so a
            `range.start_character` out of any answer goes straight back in (default 0).
              - include: Card sections to include: any of `definition` | `type` | `doc`. Empty = all. `usages`
            is always a summary and is added when the call graph is ready.
              - line: `path`: 0-based line of the symbol occurrence.
              - locale: Type/label language: `ru` (default) or `en`.
              - max_output_tokens: Output budget in tokens (~4 chars each); an over-budget member list is trimmed and the
            response carries `truncated: true` with a `budget_hint` (default 6000).
              - member_kind: Keep only members of this machine kind; does not affect symbol resolution or `include`.
              - member_name: Keep only members with this exact case-insensitive name.
              - path: Positional fallback for locals/parameters that have no qualified name: absolute or
            workspace-relative `.bsl` path, or a path relative to `root_id`. Requires `line`.
              - root_id: `path`: the source root that path is spelled against, as carried by every `search`
            code hit. Omit for a path already spelled against the workspace; `""` names the
            configuration. An extension repeats the configuration's layout, so the same relative
            path exists under several roots and the pair is what names one file.
              - symbol: Qualified name of the symbol (primary input): a common-module method
            (`ОбщегоНазначения.ЗначениеРеквизитаОбъекта`), a metadata object (`Справочник.Товары`)
            or its attribute (`Справочник.Товары.Артикул`), an object/manager module method
            (`Документ.ЗаказКлиента.Провести`), or a platform member (`СтрНайти`, `Массив.Добавить`).
            Case-insensitive; the MdoType keyword accepts singular or plural, RU or EN.

            ## syntax_help
            Look up one platform member's reference card — signature, parameters, and description —
            from the built-in platform data. Available in both profiles. Use `name` with optional
            `type_name` for the legacy lookup, or the mutually exclusive `reference_id` returned by
            `search(action="list_platform")` for an exact entity.
              - max_output_tokens: Output budget in tokens (~4 chars each) for Markdown and structured content together.
              - name: Platform member name to look up, e.g. `СтрНайти` or a type method.
              - reference_id: Exact stable identifier returned by `search(action="list_platform")`.
              - type_name: Owning platform type when `name` is a member of a specific type (optional).

        "###]].assert_eq(&rendered);
    }

    #[test]
    fn reference_tools_contract() {
        let router = McpServer::gated_router(
            McpProfile::Reference,
            &ToolGate::for_launch(McpProfile::Reference, &[]),
        );
        let rendered = render(&router.list_all());
        assert!(!rendered.contains("<no doc>"), "every published parameter needs prose docs");
        expect![[r###"
            ## its_help
            Ask the ITS expert-help knowledge base a natural-language question about the 1C platform
            and development standards. Use for conceptual "how / why" questions. For one member's
            signature use `syntax_help`; for doc keyword search use `search`. Params: `question`
            (required), optional `max_output_tokens` bounding a long answer.
              - max_output_tokens: Output budget in tokens (~4 chars each); a long answer is truncated at a line boundary
            with a continuation note (default 6000).
              - question: Natural-language question for the ITS expert help.

            ## search
            Search the platform reference documentation index (not project code). Use to find
            platform API documentation by keyword or meaning. For project code search use the
            workspace profile's `search`; for one platform member's signature use `syntax_help`.
            Actions: `find_docs` / `search_docs` — doc search (`query` required; `limit` default 10,
            max 50); `list_platform` — exact built-in entity listing; `status` — index readiness.
            While the index warms up a doc search returns a retry
            envelope. Hits arrive twice: a listing for people in the text block, and the same hits
            in `structuredContent` — `{schema_version, hits: [{rank, score, path, line_start,
            line_end, symbol, kind, snippet, snippet_truncated_lines}], shown, total,
            budget_exhausted?}`. Read the structured form: it is the versioned contract, whereas the
            text layout may be reformatted in any release. `score` is the ranker's own number —
            comparable within one response, meaningless across searches or backends.
              - action: Requested search, platform-listing, or lifecycle action.
              - kind: Optional platform entity kind: type, method, property, constructor, or global_function.
              - limit: Cap on returned hits (default 10, max 50).
              - max_output_tokens: Output budget in tokens (~4 chars each) for text and structured content together.
              - name: Optional case-insensitive substring of the Russian or English platform entity name.
              - query: Free-text query.

            ## syntax_help
            Look up one platform member's reference card — signature, parameters, and description —
            from the built-in platform data. Available in both profiles. Use `name` with optional
            `type_name` for the legacy lookup, or the mutually exclusive `reference_id` returned by
            `search(action="list_platform")` for an exact entity.
              - max_output_tokens: Output budget in tokens (~4 chars each) for Markdown and structured content together.
              - name: Platform member name to look up, e.g. `СтрНайти` or a type method.
              - reference_id: Exact stable identifier returned by `search(action="list_platform")`.
              - type_name: Owning platform type when `name` is a member of a specific type (optional).

        "###]].assert_eq(&rendered);
    }
}

/// Each tool consults the stale-miss rescan hatch on ITS OWN miss and on nothing else.
///
/// What the consultation is worth is settled elsewhere, on stands where the answer itself
/// changes; what is settled here is that the tool asks — and that it does not ask for an
/// answer nobody doubted, which is what a policy applied unconditionally would do.
#[cfg(test)]
mod rescan_hatch_consultations {
    use super::*;
    use crate::diagnostics_state::DiagnosticsState;

    const DECLARED: &str = "Сервер.Считать";
    const ABSENT: &str = "Сервер.НетТакогоИмени";

    fn stand() -> (tempfile::TempDir, McpServer, DiagnosticsState) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        crate::graph::test_support::sample_workspace(root);
        std::fs::write(root.join("Configuration.xml"), "<Configuration/>").unwrap();
        let state = SharedState::workspace(root.to_path_buf()).expect("valid workspace project");
        state.diagnostics().ensure_loading();
        crate::diagnostics_state::test_support::wait_ready(state.diagnostics());
        let diag = state.diagnostics().clone();
        (dir, McpServer::new(McpProfile::Workspace, state), diag)
    }

    fn token() -> tokio_util::sync::CancellationToken {
        tokio_util::sync::CancellationToken::new()
    }

    fn metadata_params(object_type: &str, object_name: &str) -> Parameters<MetadataParams> {
        Parameters(MetadataParams {
            action: "object".to_owned(),
            filter: None,
            meta_type: None,
            name_mask: None,
            max_items: None,
            object_type: Some(object_type.to_owned()),
            object_name: Some(object_name.to_owned()),
            form_name: None,
            max_output_tokens: None,
            connection: None,
            mode: None,
        })
    }

    fn symbol_params(symbol: &str) -> Parameters<SymbolInfoParams> {
        Parameters(SymbolInfoParams {
            symbol: Some(symbol.to_owned()),
            root_id: None,
            path: None,
            line: None,
            column: None,
            include: Vec::new(),
            member_kind: None,
            member_name: None,
            locale: None,
            max_output_tokens: None,
        })
    }

    fn references_params(symbol: &str) -> Parameters<ReferencesParams> {
        Parameters(ReferencesParams {
            symbol: Some(symbol.to_owned()),
            anchor_root_id: None,
            root_id: None,
            path: None,
            line: None,
            column: None,
            line_content: None,
            area_root_id: None,
            area_path_prefix: None,
            kinds: Vec::new(),
            include_declaration: None,
            limit: None,
            max_files: None,
            include_preview: None,
            max_output_tokens: None,
        })
    }

    /// A miss for a type that CAN resolve is a candidate for an object added since the
    /// last walk. A type that cannot resolve is not: no walk turns a typo into a type,
    /// and asking for one would let a bad parameter stat the workspace.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn metadata_object_consults_the_rescan_hatch_on_a_resolvable_type_miss() {
        let (_dir, server, diag) = stand();

        let before = diag.forced_rescans();
        let _ = server
            .metadata(
                metadata_params("Справочник", "НетТакогоОбъекта"),
                token(),
                tasks::TaskCapable(false),
            )
            .await;
        assert_eq!(diag.forced_rescans(), before + 1, "a resolvable type miss consults the hatch");

        let before = diag.forced_rescans();
        let _ = server
            .metadata(
                metadata_params("НеизвестныйТип", "НетТакогоОбъекта"),
                token(),
                tasks::TaskCapable(false),
            )
            .await;
        assert_eq!(diag.forced_rescans(), before, "control: an unresolvable type does not");

        server.shutdown();
    }

    /// An absent card for a request made BY NAME is a candidate; a name that resolved is
    /// not.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn symbol_info_consults_the_rescan_hatch_on_a_named_symbol_miss() {
        let (_dir, server, diag) = stand();

        let before = diag.forced_rescans();
        let _ = server.symbol_info(symbol_params(ABSENT), token(), tasks::TaskCapable(false)).await;
        assert_eq!(diag.forced_rescans(), before + 1, "a name that missed consults the hatch");

        let before = diag.forced_rescans();
        let _ =
            server.symbol_info(symbol_params(DECLARED), token(), tasks::TaskCapable(false)).await;
        assert_eq!(diag.forced_rescans(), before, "control: a name that resolved does not");

        server.shutdown();
    }

    /// The same for the answer-shaped miss this tool decides by its outcome.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn references_consults_the_rescan_hatch_on_a_named_symbol_miss() {
        let (_dir, server, diag) = stand();

        let before = diag.forced_rescans();
        let _ =
            server.references(references_params(ABSENT), token(), tasks::TaskCapable(false)).await;
        assert_eq!(diag.forced_rescans(), before + 1, "a name that missed consults the hatch");

        let before = diag.forced_rescans();
        let _ = server
            .references(references_params(DECLARED), token(), tasks::TaskCapable(false))
            .await;
        assert_eq!(diag.forced_rescans(), before, "control: an answer that resolved does not");

        server.shutdown();
    }
}

/// The request's token reaches the search door from every handler cell.
///
/// The gates below the handlers drive the search functions directly and are green whatever
/// the handler does with `ct` — a bare `spawn_blocking` left in one branch, or a fresh token
/// handed to the door instead of the request's own, would pass them all. This is the one
/// gate at the handler layer: the engine lock is held from outside, the request is already
/// cancelled, and the answer must be the cancellation BEFORE the lock frees.
#[cfg(test)]
mod search_cancellation_matrix {
    use super::*;
    use serde_json::json;
    use std::time::{Duration, Instant};

    const BOUND: Duration = Duration::from_millis(500);

    fn cancelled_token() -> tokio_util::sync::CancellationToken {
        let token = tokio_util::sync::CancellationToken::new();
        token.cancel();
        token
    }

    /// Hold `engine`'s lock from a thread of its own until the returned handle is dropped.
    /// A guard held in the async test itself would sit across the `await` (which clippy
    /// rightly refuses); a holder thread is also the production shape — some other caller.
    fn hold_lock(engine: &crate::state::SharedSearchEngine) -> std::sync::mpsc::Sender<()> {
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let (held_tx, held_rx) = std::sync::mpsc::channel::<()>();
        let engine = engine.clone();
        std::thread::spawn(move || {
            let _guard = engine.lock().unwrap();
            let _ = held_tx.send(());
            let _ = release_rx.recv();
        });
        held_rx.recv().expect("the holder took the lock");
        release_tx
    }

    fn assert_cancelled(answer: Result<CallToolResult, McpError>, elapsed: Duration, cell: &str) {
        let error =
            answer.err().unwrap_or_else(|| panic!("{cell}: a cancelled call produced a body"));
        assert_eq!(error.message, "request cancelled", "{cell}: {error:?}");
        assert!(
            elapsed < BOUND,
            "{cell}: answered only after {elapsed:?}, i.e. after the held lock"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn every_workspace_search_cell_answers_a_cancelled_request_before_the_lock_frees() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        crate::graph::test_support::sample_workspace(root);
        std::fs::write(root.join("Configuration.xml"), "<Configuration/>").unwrap();
        let state = SharedState::workspace(root.to_path_buf()).expect("valid workspace project");
        let code_engine = state.search_engine().clone();
        let docs_engine = state.reference_search_engine();
        let server = McpServer::new(McpProfile::Workspace, state);

        let cells = [
            ("search_code", json!({"action": "search_code", "query": "Процедура"}), &code_engine),
            ("find_docs", json!({"action": "find_docs", "query": "Массив"}), &docs_engine),
            ("search_docs", json!({"action": "search_docs", "query": "Массив"}), &docs_engine),
        ];
        for (cell, arguments, engine) in cells {
            let held = hold_lock(engine);
            let params =
                Parameters(serde_json::from_value::<WorkspaceSearchParams>(arguments).unwrap());
            let started = Instant::now();
            let answer = server.workspace_search(params, cancelled_token()).await;
            assert_cancelled(answer, started.elapsed(), cell);
            drop(held);
        }
        server.shutdown();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn every_reference_search_cell_answers_a_cancelled_request_before_the_lock_frees() {
        let state = SharedState::shared();
        let engine = state.search_engine().clone();
        let server = McpServer::new(McpProfile::Reference, state);

        for action in ["find_docs", "search_docs"] {
            let held = hold_lock(&engine);
            let params = Parameters(
                serde_json::from_value::<ReferenceSearchParams>(
                    json!({"action": action, "query": "Массив"}),
                )
                .unwrap(),
            );
            let started = Instant::now();
            let answer = server.reference_search(params, cancelled_token()).await;
            assert_cancelled(answer, started.elapsed(), action);
            drop(held);
        }
        server.shutdown();
    }
}
