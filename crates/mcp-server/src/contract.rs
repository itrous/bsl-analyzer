//! Machine-readable declaration of what this build accepts.
//!
//! Consumers need to know which tools, actions and parameters a given build supports
//! before they call it. Without a declaration the only observable surface is the text of
//! `--help` and the prose in tool descriptions, so downstream CI ends up asserting on
//! substrings: a reworded help line breaks it, while a renamed parameter does not. This
//! module turns the surface into data.
//!
//! Two things are declared here by hand — the action names each tool accepts and the
//! parameters each action needs — because neither is expressible in the JSON schema
//! `tools/list` already publishes (`action` is a plain string, and requiredness varies per
//! action). Everything else is derived: parameter names, types and schema-level
//! requiredness come from the tool router's own schemas, so a renamed parameter cannot
//! silently diverge from the declaration — [`tests::declared_params_exist_in_schema`]
//! rejects it. The unknown-action error messages are generated from the same tables
//! ([`unknown_action`]), so a renamed action cannot diverge either.
//!
//! The CLI surface is declared by the binary crate, which owns the clap definition, and
//! handed over at startup through [`register_cli_surface`].

use std::collections::BTreeSet;
use std::sync::OnceLock;

use rmcp::ErrorData as McpError;
use serde_json::{json, Map, Value};

use crate::{McpProfile, McpServer};

/// Version of the *contract*, deliberately separate from the analyzer's build version: a
/// patch release must not look like a contract change, and a renamed parameter must.
///
/// - **major** — a tool, action or parameter is removed or renamed, a parameter becomes
///   required, or the meaning of an existing field changes;
/// - **minor** — a tool, action, parameter, accepted value or structured output is added.
///
/// Consumers should require an exact major and a minimum minor. Bump this by hand in the
/// same commit that changes the surface; the snapshot test over [`document`] puts the
/// version field next to the change in the diff.
pub const CONTRACT_VERSION: &str = "3.4";

/// URI of the MCP resource carrying [`document`].
pub const CONTRACT_URI: &str = "bsl-analyzer://contract";

/// One tool's hand-declared part of the contract.
pub struct ToolDecl {
    pub name: &'static str,
    /// Values accepted in the tool's `action` parameter, in the order the tool reports
    /// them. Empty for single-purpose tools that have no `action` parameter.
    pub actions: &'static [ActionDecl],
    /// A requirement the JSON schema cannot express (e.g. "one of these two").
    pub note: Option<&'static str>,
    /// Version carried by this tool's `structuredContent`, when it has a stable output schema.
    pub output_schema_version: Option<&'static str>,
    /// Whether the profile serves this tool without being asked. A `false` here is the
    /// only thing that makes a tool opt-in: it is declared, the build can serve it, and
    /// `tools/list` omits it until a launch names it.
    pub default_enabled: bool,
}

/// One action of a tool, with the parameters it needs beyond the schema-level required ones.
pub struct ActionDecl {
    pub name: &'static str,
    pub required: &'static [&'static str],
    /// A requirement that holds only in a specific mode.
    pub note: Option<&'static str>,
}

const fn action(name: &'static str, required: &'static [&'static str]) -> ActionDecl {
    ActionDecl { name, required, note: None }
}

const fn tool(name: &'static str, actions: &'static [ActionDecl]) -> ToolDecl {
    ToolDecl { name, actions, note: None, output_schema_version: None, default_enabled: true }
}

const METADATA_ACTIONS: &[ActionDecl] = &[
    action("info", &[]),
    ActionDecl {
        name: "tree",
        required: &[],
        note: Some(
            "in infobase mode (`mode=infobase`, or `connection` set under `mode=auto`) \
             `meta_type` is required and only `tree`/`object` are available",
        ),
    },
    action("object", &["object_type", "object_name"]),
    action("form", &["object_type"]),
    action("status", &[]),
];

const WORKSPACE_SEARCH_ACTIONS: &[ActionDecl] = &[
    action("search_code", &["query"]),
    action("status", &[]),
    action("list_platform", &[]),
    action("find_docs", &["query"]),
    action("search_docs", &["query"]),
];

const QUERY_ACTIONS: &[ActionDecl] =
    &[action("validate", &["query"]), action("execute", &["query"]), action("schema", &[])];

const EXECUTE_ACTIONS: &[ActionDecl] =
    &[action("check", &[]), action("run", &[]), action("eval", &[])];

const DEBUG_ACTIONS: &[ActionDecl] = &[
    action("attach", &["host", "infobase"]),
    action("disconnect", &[]),
    action("set_breakpoint", &["module", "line"]),
    action("remove_breakpoint", &["module", "line"]),
    action("continue", &[]),
    action("step", &["direction"]),
    action("wait_stop", &[]),
    action("stack_trace", &[]),
    action("locals", &[]),
    action("eval", &["expression"]),
];

const GRAPH_ACTIONS: &[ActionDecl] = &[
    action("overview", &[]),
    action("schema", &[]),
    action("status", &[]),
    action("node", &["id"]),
    action("source", &["ids"]),
    action("neighbors", &["id"]),
    action("callers", &["id"]),
    action("callees", &["id"]),
    action("resolve", &["query"]),
];

const DIAGNOSTICS_ACTIONS: &[ActionDecl] = &[
    action("catalog", &[]),
    action("schema", &[]),
    action("status", &[]),
    action("file", &["path"]),
    action("workspace", &[]),
];

const REFERENCE_SEARCH_ACTIONS: &[ActionDecl] = &[
    action("find_docs", &["query"]),
    action("search_docs", &["query"]),
    action("status", &[]),
    action("list_platform", &[]),
];

const SYNTAX_HELP: ToolDecl = ToolDecl {
    name: "syntax_help",
    actions: &[],
    note: Some("exactly one of `name` or `reference_id` is required"),
    output_schema_version: Some("2"),
    default_enabled: true,
};

const WORKSPACE_TOOLS: &[ToolDecl] = &[
    tool("metadata", METADATA_ACTIONS),
    ToolDecl {
        name: "search",
        actions: WORKSPACE_SEARCH_ACTIONS,
        note: None,
        output_schema_version: Some("9"),
        default_enabled: true,
    },
    tool("query", QUERY_ACTIONS),
    tool("execute", EXECUTE_ACTIONS),
    tool("event_log", &[]),
    tool("debug", DEBUG_ACTIONS),
    ToolDecl {
        name: "graph",
        actions: GRAPH_ACTIONS,
        note: None,
        output_schema_version: Some("36"),
        default_enabled: true,
    },
    ToolDecl {
        name: "symbol_info",
        actions: &[],
        note: Some("one of `symbol` or `path`+`line` is required"),
        output_schema_version: Some("1"),
        default_enabled: true,
    },
    ToolDecl {
        name: "references",
        actions: &[],
        note: Some(
            "one of `symbol` or `path` with `line` and/or `line_content` is required; \
             opt-in — a launch has to name it with `--enable-tool references`",
        ),
        output_schema_version: Some("1"),
        // Opt-in: the full occurrence list of a popular name is a large answer, and a
        // profile that served it unasked would spend an agent's budget on a question it
        // did not ask.
        default_enabled: false,
    },
    ToolDecl {
        name: "diagnostics",
        actions: DIAGNOSTICS_ACTIONS,
        note: None,
        output_schema_version: Some("16"),
        default_enabled: true,
    },
    tool("outline", &[]),
    SYNTAX_HELP,
];

const REFERENCE_TOOLS: &[ToolDecl] = &[
    ToolDecl {
        name: "search",
        actions: REFERENCE_SEARCH_ACTIONS,
        note: None,
        output_schema_version: Some("7"),
        default_enabled: true,
    },
    SYNTAX_HELP,
    tool("its_help", &[]),
];

fn tools_of(profile: McpProfile) -> &'static [ToolDecl] {
    match profile {
        McpProfile::Workspace => WORKSPACE_TOOLS,
        McpProfile::Reference => REFERENCE_TOOLS,
    }
}

fn decl_of(profile: McpProfile, tool: &str) -> Option<&'static ToolDecl> {
    tools_of(profile).iter().find(|d| d.name == tool)
}

/// Every tool name the profile declares, in declaration order — both default and opt-in.
///
/// This is the set `--enable-tool` accepts, and it is wider than [`opt_in_tools`] on
/// purpose: naming a tool that is already served is an identity operation, so a client
/// config carrying the flag keeps starting after that tool graduates to the default
/// surface. Rejecting it would break every installed config on such a release.
pub fn declared_tools(profile: McpProfile) -> impl Iterator<Item = &'static str> {
    tools_of(profile).iter().map(|decl| decl.name)
}

/// Tools this profile can serve but does not serve unless a launch asks for them.
pub fn opt_in_tools(profile: McpProfile) -> impl Iterator<Item = &'static str> {
    tools_of(profile).iter().filter(|decl| !decl.default_enabled).map(|decl| decl.name)
}

/// The requested names that actually change the served surface: `requested ∩ opt_in`.
///
/// Names outside the opt-in set change nothing, so they must not reach the backend
/// identity: hashing raw flags would give a client that passes `--enable-tool <default>`
/// a different daemon than one that does not, though both are served the same tools.
pub fn effective_opt_in(profile: McpProfile, requested: &[String]) -> BTreeSet<String> {
    let opt_in: BTreeSet<&str> = opt_in_tools(profile).collect();
    requested.iter().filter(|name| opt_in.contains(name.as_str())).cloned().collect()
}

/// Reject a name this profile does not declare, naming the ones it does.
///
/// The accepted list comes from the same declaration the contract publishes, so a build
/// cannot advertise one set and accept another. It is never empty: every profile declares
/// at least one tool, whether or not the build has any opt-in tools yet.
pub fn validate_enabled_tools(profile: McpProfile, requested: &[String]) -> Result<(), String> {
    let known: Vec<&str> = declared_tools(profile).collect();
    for name in requested {
        if !known.contains(&name.as_str()) {
            return Err(format!(
                "unknown tool '{name}' for profile '{}'. Known tools: {}",
                profile.as_str(),
                known.join(", ")
            ));
        }
    }
    Ok(())
}

/// The `Unknown action` error every action-dispatching tool returns, generated from the
/// declaration so the accepted list an agent is told about and the one a consumer reads
/// from the contract are the same string by construction.
pub(crate) fn unknown_action(profile: McpProfile, tool: &str, got: &str) -> McpError {
    let expected: Vec<&str> = decl_of(profile, tool)
        .map(|d| d.actions.iter().map(|a| a.name).collect())
        .unwrap_or_default();
    McpError::invalid_params(
        format!("Unknown action '{got}'. Expected: {}", expected.join(", ")),
        None,
    )
}

/// The full declaration: contract version, build version, and every surface this process
/// exposes. The CLI surface is present once the binary has registered it
/// ([`register_cli_surface`]); a library embedder that only serves MCP gets the MCP part.
pub fn document() -> Value {
    let mut doc = Map::new();
    doc.insert("contract_version".into(), json!(CONTRACT_VERSION));
    doc.insert("build_version".into(), json!(env!("CARGO_PKG_VERSION")));
    doc.insert("mcp".into(), mcp_surface());
    // `platforms` is the same list for every build, not the running one: a consumer picks a
    // transport while deciding what to deploy where, so the declaration has to name the whole
    // supported set rather than the host that happened to answer. It is the very list the CLI
    // gate reads, so the declaration cannot outlive the gate's truth.
    doc.insert(
        "transports".into(),
        json!({
            "workspace": {
                "broker-required": {
                    "backend_pid_required": true,
                    "backend_pid_source": "supervisor launching bsl-analyzer-app directly",
                    "auto_launch": false,
                    "stdio_fallback": false,
                    "peer_identity": "supervised-pid+platform-trust",
                    "platforms": crate::broker::SUPERVISED_PID_PLATFORMS
                }
            }
        }),
    );
    if let Some(cli) = CLI_SURFACE.get() {
        doc.insert("cli".into(), cli.clone());
    }
    Value::Object(doc)
}

/// The MCP surface of both profiles: which tools each serves, which actions each tool
/// accepts, and every parameter with its type and requiredness.
pub fn mcp_surface() -> Value {
    json!({
        "profiles": {
            McpProfile::Workspace.as_str(): profile_surface(McpProfile::Workspace),
            McpProfile::Reference.as_str(): profile_surface(McpProfile::Reference),
        }
    })
}

/// Both halves of a profile's surface: `tools` is what `tools/list` serves by default,
/// `opt_in_tools` is what the same build can serve once a launch enables it. The split is
/// additive on purpose — folding opt-in tools into `tools` with a flag would change the
/// meaning of a field consumers already read as "what I will get", which is a major bump.
fn profile_surface(profile: McpProfile) -> Value {
    let router = McpServer::profile_router(profile);
    let listed = router.list_all();
    let entries = |default_enabled: bool| -> Vec<Value> {
        tools_of(profile)
            .iter()
            .filter(|decl| decl.default_enabled == default_enabled)
            .map(|decl| {
                let listed_tool = listed.iter().find(|t| t.name == decl.name);
                let schema = listed_tool.map(|t| &*t.input_schema);
                let mut entry = Map::new();
                entry.insert("name".into(), json!(decl.name));
                if let Some(note) = decl.note {
                    entry.insert("note".into(), json!(note));
                }
                if let Some(version) = decl.output_schema_version {
                    entry.insert("output_schema_version".into(), json!(version));
                    let output = listed_tool
                        .and_then(|tool| tool.output_schema.as_deref())
                        .expect("declared structured output must publish outputSchema");
                    let encoded =
                        serde_json::to_vec(output).expect("outputSchema must be serializable");
                    entry.insert(
                        "output_schema_fingerprint".into(),
                        json!(format!("blake3:{}", blake3::hash(&encoded).to_hex())),
                    );
                }
                entry.insert(
                    "actions".into(),
                    Value::Array(decl.actions.iter().map(action_surface).collect()),
                );
                entry.insert(
                    "params".into(),
                    schema.map(params_surface).unwrap_or_else(|| Value::Array(Vec::new())),
                );
                Value::Object(entry)
            })
            .collect()
    };
    json!({ "tools": entries(true), "opt_in_tools": entries(false) })
}

fn action_surface(decl: &ActionDecl) -> Value {
    let mut entry = Map::new();
    entry.insert("name".into(), json!(decl.name));
    entry.insert("required".into(), json!(decl.required));
    if let Some(note) = decl.note {
        entry.insert("note".into(), json!(note));
    }
    Value::Object(entry)
}

/// Parameters lifted straight out of the tool's published JSON schema, so names, types and
/// schema-level requiredness cannot drift from what the server actually accepts.
/// Descriptions are deliberately left out: they are prose for agents, they live in
/// `tools/list`, and carrying them here would make every reworded sentence read as a
/// contract change — exactly the failure mode this declaration exists to end.
fn params_surface(schema: &Map<String, Value>) -> Value {
    let shape = schema_shape(schema);
    let props = &shape.properties;
    let required = &shape.required;
    let mut names: Vec<&String> = props.keys().collect();
    names.sort();
    Value::Array(
        names
            .into_iter()
            .map(|name| {
                let mut param = Map::new();
                param.insert("name".into(), json!(name));
                param.insert("type".into(), json!(type_of(&props[name])));
                param.insert("required".into(), json!(required.contains(name.as_str())));
                // Absent and explicitly `null` are different inputs: an optional parameter
                // may be omitted, and a nullable one additionally accepts `null` in place
                // of a value. `required: false` alone would under-declare the second.
                if accepts_null(&props[name]) {
                    param.insert("nullable".into(), json!(true));
                }
                Value::Object(param)
            })
            .collect(),
    )
}

#[derive(Default)]
struct SchemaShape {
    properties: Map<String, Value>,
    required: BTreeSet<String>,
}

#[cfg(test)]
pub(crate) fn schema_properties(schema: &Map<String, Value>) -> Map<String, Value> {
    schema_shape(schema).properties
}

/// Give a published schema the root `type: "object"` the MCP specification requires.
///
/// A union of object branches is generated as a bare `oneOf`/`anyOf`, and a schema whose
/// root says nothing about its type is rejected by a spec-compliant client — and by
/// `rmcp`'s own `schema_for_output`. The branches stay as they are: the keyword only
/// states what every branch already is.
pub(crate) fn ensure_object_root(schema: &mut Map<String, Value>) {
    if !schema.contains_key("type") {
        schema.insert("type".to_owned(), Value::String("object".to_owned()));
    }
}

fn schema_shape(schema: &Map<String, Value>) -> SchemaShape {
    shape_from_value(schema, &Value::Object(schema.clone()), 0)
}

fn shape_from_value(root: &Map<String, Value>, schema: &Value, depth: usize) -> SchemaShape {
    if depth > 16 {
        return SchemaShape::default();
    }
    let Some(object) = schema.as_object() else { return SchemaShape::default() };
    let mut shape = SchemaShape::default();
    // A `$ref` keeps its neighbours: schemars writes an internally tagged variant as the
    // tag's `properties`/`required` NEXT TO a `$ref` at the variant's payload. Returning
    // the target alone drops the discriminator, and the published surface then calls a
    // required parameter optional.
    if let Some(reference) = object.get("$ref").and_then(Value::as_str) {
        if let Some(name) = reference.strip_prefix("#/$defs/") {
            if let Some(target) =
                root.get("$defs").and_then(Value::as_object).and_then(|defs| defs.get(name))
            {
                shape = shape_from_value(root, target, depth + 1);
            }
        }
    }

    if let Some(properties) = object.get("properties").and_then(Value::as_object) {
        shape.properties.extend(properties.clone());
    }
    if let Some(required) = object.get("required").and_then(Value::as_array) {
        shape.required.extend(required.iter().filter_map(Value::as_str).map(str::to_owned));
    }
    if let Some(all_of) = object.get("allOf").and_then(Value::as_array) {
        for branch in all_of {
            let branch = shape_from_value(root, branch, depth + 1);
            shape.properties.extend(branch.properties);
            shape.required.extend(branch.required);
        }
    }
    for keyword in ["oneOf", "anyOf"] {
        if let Some(union) = object.get(keyword).and_then(Value::as_array) {
            let branches: Vec<_> =
                union.iter().map(|branch| shape_from_value(root, branch, depth + 1)).collect();
            for branch in &branches {
                shape.properties.extend(branch.properties.clone());
            }
            if let Some(first) = branches.first() {
                let mut common = first.required.clone();
                for branch in &branches[1..] {
                    common.retain(|name| branch.required.contains(name));
                }
                shape.required.extend(common);
            }
        }
    }
    shape
}

fn accepts_null(schema: &Value) -> bool {
    schema
        .get("type")
        .and_then(Value::as_array)
        .is_some_and(|names| names.iter().any(|name| name == "null"))
}

/// Collapse a property schema to one type name. `Option<T>` widens the schema to
/// `["T", "null"]`; the null arm is reported separately by `nullable`, so it is dropped
/// here rather than turning every optional parameter's type into a union.
fn type_of(schema: &Value) -> String {
    let base = match schema.get("type") {
        Some(Value::String(name)) => name.clone(),
        Some(Value::Array(names)) => names
            .iter()
            .filter_map(Value::as_str)
            .find(|name| *name != "null")
            .unwrap_or("any")
            .to_string(),
        _ => "any".to_string(),
    };
    if base == "array" {
        if let Some(item) = schema.get("items") {
            return format!("array<{}>", type_of(item));
        }
    }
    base
}

static CLI_SURFACE: OnceLock<Value> = OnceLock::new();

/// Hand the process's CLI surface to the contract document.
///
/// The clap definition lives in the binary crate, which this crate cannot see, yet the
/// point of the declaration is that one read answers "what does this build accept" for
/// every surface at once. The binary introspects its own commands at startup and registers
/// the result here; the first registration wins, later ones are ignored.
pub fn register_cli_surface(surface: Value) {
    let _ = CLI_SURFACE.set(surface);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ToolGate;
    use expect_test::expect;

    #[test]
    fn indexing_discovery_contract() {
        use crate::indexing::{Indexing, Kind, State, Target};
        assert_eq!(CONTRACT_VERSION, "3.4");
        let indexing = serde_json::to_value(Indexing::single(Target::new(
            Kind::Reference,
            State::Ready,
            None,
        )))
        .unwrap();
        for profile in [McpProfile::Workspace, McpProfile::Reference] {
            let schema = output_schema(profile, "search").unwrap();
            let validator = jsonschema::validator_for(&schema).unwrap();
            if profile == McpProfile::Workspace {
                let mut body = json!({
                    "action":"search_code", "schema_version":"8", "indexing":indexing,
                    "hits":[{"symbol":"Тест", "source_span": {
                        "byte_start":0, "byte_end":10, "parent_byte_start":0,
                        "parent_byte_end":20, "part_index":1, "part_count":2
                    }}], "shown":1, "total":1
                });
                assert!(validator.is_valid(&body));
                body["hits"][0]["source_span"]["part_index"] = json!(0);
                assert!(!validator.is_valid(&body));
                body["hits"][0]["source_span"]["part_index"] = json!(1);
                body["hits"][0]["source_span"]["parent_symbol"] = json!("internal");
                assert!(!validator.is_valid(&body));
            }
            let mut actual = crate::tools::search::docs_not_ready("find_docs");
            Indexing::single(Target::new(
                Kind::Reference,
                State::Waiting,
                Some(crate::indexing::Reason::Initializing),
            ))
            .attach(&mut actual);
            assert!(validator.is_valid(actual.structured_content.as_ref().unwrap()));
            for action in ["search_code", "find_docs", "search_docs"] {
                let version = if action == "search_code" && profile == McpProfile::Workspace {
                    "8"
                } else {
                    "6"
                };
                for mut body in [
                    json!({"action":action,"schema_version":version,"hits":[],"shown":0,"total":0,"indexing":indexing}),
                    json!({"action":action,"schema_version":version,"status":"not_ready","retry_after_ms":100,"indexing":indexing}),
                ] {
                    assert!(validator.is_valid(&body), "{body}");
                    body.as_object_mut().unwrap().remove("indexing");
                    assert!(!validator.is_valid(&body));
                }
            }
            let mut status = json!({"action":"status","schema_version":"4","profile":"reference","state":"ready","indexing":indexing});
            assert!(validator.is_valid(&status));
            status["embedding_profile"] = json!({
                "wire_model": "served-alias",
                "storage_identity": "profile-v1:0123",
                "dimension": 768,
            });
            assert!(validator.is_valid(&status));
            status["embedding_profile"]["query_prefix"] = json!("must stay undisclosed");
            assert!(!validator.is_valid(&status));
            status.as_object_mut().unwrap().remove("embedding_profile");
            status.as_object_mut().unwrap().remove("indexing");
            assert!(!validator.is_valid(&status));
            assert!(validator.is_valid(&json!({"action":"list_platform","schema_version":"1","items":[],"shown":0,"total":0,"budget_exhausted":false})));
        }
        let schema = output_schema(McpProfile::Workspace, "graph").unwrap();
        let validator = jsonschema::validator_for(&schema).unwrap();
        for mut body in [
            json!({"state":"ready","indexing":indexing}),
            json!({"status":"loading","indexing":indexing}),
        ] {
            assert!(validator.is_valid(&body));
            body.as_object_mut().unwrap().remove("indexing");
            assert!(!validator.is_valid(&body), "alternative branch must not bypass indexing");
        }
        let graph_schema = crate::tools::graph::schema().structured_content.unwrap();
        assert_eq!(graph_schema["schema_version"], "36");
        assert!(validator.is_valid(&graph_schema));
    }

    fn schema_props(profile: McpProfile, tool: &str) -> Map<String, Value> {
        let router = McpServer::profile_router(profile);
        let listed = router.list_all();
        let found = listed.iter().find(|t| t.name == tool).unwrap_or_else(|| {
            panic!("tool '{tool}' is declared but the router does not serve it")
        });
        schema_properties(&found.input_schema)
    }

    fn output_schema(profile: McpProfile, tool: &str) -> Option<Value> {
        let router = McpServer::profile_router(profile);
        router
            .list_all()
            .iter()
            .find(|candidate| candidate.name == tool)
            .and_then(|found| found.output_schema.as_deref().cloned())
            .map(Value::Object)
    }

    /// The declaration names parameters that the tool really accepts. A renamed parameter
    /// fails here instead of shipping a contract that points at a field nobody reads.
    #[test]
    fn declared_params_exist_in_schema() {
        for profile in [McpProfile::Workspace, McpProfile::Reference] {
            for decl in tools_of(profile) {
                let props = schema_props(profile, decl.name);
                if !decl.actions.is_empty() {
                    assert!(
                        props.contains_key("action"),
                        "{}/{} declares actions but has no `action` parameter",
                        profile.as_str(),
                        decl.name
                    );
                }
                for action in decl.actions {
                    for param in action.required {
                        assert!(
                            props.contains_key(*param),
                            "{}/{} action '{}' requires '{param}', which is not a parameter of \
                             the tool",
                            profile.as_str(),
                            decl.name,
                            action.name
                        );
                    }
                }
            }
        }
    }

    /// The `tools` half of the declaration says what a plain launch serves — the meaning
    /// consumers already read it with. Comparing it against the router a default gate
    /// actually produces keeps that promise from drifting, and catches a gate that hides
    /// more than the opt-in set: an over-wide difference empties the served list here.
    #[test]
    fn declared_default_tools_match_default_router() {
        for profile in [McpProfile::Workspace, McpProfile::Reference] {
            let router = McpServer::gated_router(profile, &ToolGate::for_launch(profile, &[]));
            let mut served: Vec<String> =
                router.list_all().iter().map(|t| t.name.to_string()).collect();
            served.sort();
            let mut declared: Vec<String> = tools_of(profile)
                .iter()
                .filter(|decl| decl.default_enabled)
                .map(|decl| decl.name.to_string())
                .collect();
            declared.sort();
            assert_eq!(served, declared, "profile {}", profile.as_str());
        }
    }

    /// The two halves partition the declaration: a tool is served by default or it is
    /// opt-in, never both and never neither. A consumer reads "what the build can do" as
    /// their union, so an overlap would double-count and a gap would hide a tool from
    /// feature detection entirely.
    #[test]
    fn opt_in_and_default_partition_the_declaration() {
        for profile in [McpProfile::Workspace, McpProfile::Reference] {
            let declared: BTreeSet<&str> = declared_tools(profile).collect();
            let opt_in: BTreeSet<&str> = opt_in_tools(profile).collect();
            let default: BTreeSet<&str> = tools_of(profile)
                .iter()
                .filter(|decl| decl.default_enabled)
                .map(|decl| decl.name)
                .collect();
            assert!(
                default.is_disjoint(&opt_in),
                "{}: a tool is both default and opt-in",
                profile.as_str()
            );
            assert_eq!(
                declared,
                default.union(&opt_in).copied().collect::<BTreeSet<_>>(),
                "profile {}",
                profile.as_str()
            );
        }
    }

    /// Only names that change the served surface reach the backend identity. Asking for a
    /// tool the profile already serves is an identity operation, so it must project to the
    /// empty set — otherwise one client passing the flag and one omitting it would rendezvous
    /// at different daemons while being served exactly the same tools.
    #[test]
    fn effective_opt_in_drops_default_names() {
        for profile in [McpProfile::Workspace, McpProfile::Reference] {
            let every_default: Vec<String> = tools_of(profile)
                .iter()
                .filter(|decl| decl.default_enabled)
                .map(|decl| decl.name.to_owned())
                .collect();
            assert!(
                !every_default.is_empty(),
                "profile {} declares no default tool",
                profile.as_str()
            );
            assert_eq!(
                effective_opt_in(profile, &every_default),
                BTreeSet::new(),
                "profile {}",
                profile.as_str()
            );
        }
    }

    /// The accepted list is generated from the declaration, so it cannot advertise a name
    /// the build does not serve, and it is non-empty even in a build without a single
    /// opt-in tool — an error that named nothing would leave the caller no way forward.
    #[test]
    fn validation_accepts_every_declared_name_and_names_them_on_refusal() {
        for profile in [McpProfile::Workspace, McpProfile::Reference] {
            let declared: Vec<String> = declared_tools(profile).map(str::to_owned).collect();
            assert_eq!(validate_enabled_tools(profile, &declared), Ok(()));
            let refusal = validate_enabled_tools(profile, &["no_such_tool".to_owned()])
                .expect_err("an undeclared name must be refused");
            for name in &declared {
                assert!(refusal.contains(name), "refusal omits '{name}': {refusal}");
            }
        }
    }

    /// Every tool the router serves is declared, and nothing is declared that is not
    /// served. A new tool that forgets its declaration fails here.
    #[test]
    fn declaration_covers_every_served_tool() {
        for profile in [McpProfile::Workspace, McpProfile::Reference] {
            let router = McpServer::profile_router(profile);
            let mut served: Vec<String> =
                router.list_all().iter().map(|t| t.name.to_string()).collect();
            served.sort();
            let mut declared: Vec<String> =
                tools_of(profile).iter().map(|d| d.name.to_string()).collect();
            declared.sort();
            assert_eq!(served, declared, "profile {}", profile.as_str());
        }
    }

    #[test]
    fn declared_structured_outputs_have_published_schemas() {
        for profile in [McpProfile::Workspace, McpProfile::Reference] {
            for decl in tools_of(profile) {
                if decl.output_schema_version.is_some() {
                    let schema = output_schema(profile, decl.name).unwrap_or_else(|| {
                        panic!("{}/{} has no outputSchema", profile.as_str(), decl.name)
                    });
                    assert_eq!(
                        schema["type"],
                        "object",
                        "{}/{} outputSchema must describe object responses",
                        profile.as_str(),
                        decl.name
                    );
                    assert!(
                        schema
                            .as_object()
                            .is_some_and(|schema| !schema_properties(schema).is_empty()),
                        "{}/{} outputSchema has no object-shaped branch",
                        profile.as_str(),
                        decl.name
                    );
                    assert!(
                        schema.to_string().contains("schema_version"),
                        "{}/{} outputSchema has no schema_version",
                        profile.as_str(),
                        decl.name
                    );
                }
            }
        }
    }

    /// Every served tool's inputSchema says its root is an object.
    ///
    /// The output side is gated above; the input side is the half a client reads FIRST, and a
    /// tool whose parameters are a union is generated as a bare `oneOf` — a shape a
    /// spec-compliant client rejects along with the whole listing.
    #[test]
    fn every_published_input_schema_is_object_rooted() {
        for profile in [McpProfile::Workspace, McpProfile::Reference] {
            let router = McpServer::profile_router(profile);
            for tool in router.list_all() {
                assert_eq!(
                    tool.input_schema.get("type").and_then(Value::as_str),
                    Some("object"),
                    "{}/{} inputSchema must describe object parameters",
                    profile.as_str(),
                    tool.name
                );
            }
        }
    }

    /// A published schema is what a machine consumer validates against, so a field the schema
    /// requires and the card omits is not an edge case: it fails every ordinary response of that
    /// shape. Each of the four card kinds is checked, root fields and the kind's own branch.
    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn syntax_help_cards_carry_every_field_the_schema_requires() {
        let schema = output_schema(McpProfile::Reference, "syntax_help").expect("outputSchema");
        let platform = bsl_platform::PlatformDataInner::instance();
        let method = platform.all_methods()[0].clone();
        let lookups: [(&str, Option<&str>); 4] = [
            ("Массив", None),
            (method.name.as_str(), Some(method.type_name.as_str())),
            ("Сообщить", None),
            ("Если", None),
        ];

        for (name, type_name) in lookups {
            let result = crate::tools::platform::bsl_syntax_help(name, type_name, 6000).unwrap();
            let card = result.structured_content.expect("structuredContent");
            for key in required_keys(&schema) {
                assert!(card.get(&key).is_some(), "{name}: card omits required `{key}`");
            }
            let branch = schema["oneOf"]
                .as_array()
                .expect("the card is a tagged union")
                .iter()
                .find(|variant| variant["properties"]["kind"]["const"] == card["kind"])
                .unwrap_or_else(|| panic!("{name}: no schema branch for kind {}", card["kind"]));
            for key in required_keys(branch) {
                assert!(card.get(&key).is_some(), "{name}: card omits required `{key}`");
            }
        }
    }

    fn required_keys(schema: &Value) -> Vec<String> {
        schema["required"]
            .as_array()
            .map(|keys| keys.iter().filter_map(|k| k.as_str().map(str::to_owned)).collect())
            .unwrap_or_default()
    }

    /// The declaration tells a consumer where the supervised transport can be deployed; the same
    /// list decides where the CLI accepts it. Published as a copy it would drift for every target
    /// nobody runs the suite on, so what is asserted here is that it is not a copy.
    #[test]
    fn declared_transport_platforms_are_the_supervised_pid_gate() {
        let doc = document();
        let platforms = doc["transports"]["workspace"]["broker-required"]["platforms"]
            .as_array()
            .expect("the supervised transport declares its platforms")
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>();

        assert_eq!(platforms, crate::broker::SUPERVISED_PID_PLATFORMS);
        assert_eq!(platforms.contains(&std::env::consts::OS), crate::broker::peer_pid_available());
    }

    #[test]
    fn unknown_action_lists_the_declared_actions() {
        let err = unknown_action(McpProfile::Workspace, "query", "validte");
        assert_eq!(err.message, "Unknown action 'validte'. Expected: validate, execute, schema");
    }

    /// A tool with no declared actions must not produce an empty `Expected:` list — that
    /// would mean an action-dispatching tool lost its declaration.
    #[test]
    fn unknown_action_never_reports_an_empty_list() {
        for profile in [McpProfile::Workspace, McpProfile::Reference] {
            for decl in tools_of(profile) {
                if decl.actions.is_empty() {
                    continue;
                }
                let err = unknown_action(profile, decl.name, "nope");
                assert!(
                    !err.message.ends_with("Expected: "),
                    "{}/{} produced an empty expected-action list",
                    profile.as_str(),
                    decl.name
                );
            }
        }
    }

    /// The declaration in full. Any change to a tool, action, parameter name, parameter
    /// type or requiredness lands in this diff next to `contract_version` — that adjacency
    /// is the reminder to bump it. `build_version` is left out on purpose: it moves every
    /// release and would drown the changes worth noticing. Rebase with
    /// `UPDATE_EXPECT=1 cargo test -p mcp-server contract`.
    #[test]
    fn mcp_surface_snapshot() {
        let mut doc = Map::new();
        doc.insert("contract_version".into(), json!(CONTRACT_VERSION));
        doc.insert("mcp".into(), mcp_surface());
        expect![[r#"
            {
              "contract_version": "3.4",
              "mcp": {
                "profiles": {
                  "reference": {
                    "opt_in_tools": [],
                    "tools": [
                      {
                        "actions": [
                          {
                            "name": "find_docs",
                            "required": [
                              "query"
                            ]
                          },
                          {
                            "name": "search_docs",
                            "required": [
                              "query"
                            ]
                          },
                          {
                            "name": "status",
                            "required": []
                          },
                          {
                            "name": "list_platform",
                            "required": []
                          }
                        ],
                        "name": "search",
                        "output_schema_fingerprint": "blake3:16ac68a2b10e4a20342f41a6fc435182996213e917a75a8a25077d3021f3d4c5",
                        "output_schema_version": "7",
                        "params": [
                          {
                            "name": "action",
                            "required": true,
                            "type": "string"
                          },
                          {
                            "name": "kind",
                            "required": false,
                            "type": "any"
                          },
                          {
                            "name": "limit",
                            "nullable": true,
                            "required": false,
                            "type": "integer"
                          },
                          {
                            "name": "max_output_tokens",
                            "nullable": true,
                            "required": false,
                            "type": "integer"
                          },
                          {
                            "name": "name",
                            "nullable": true,
                            "required": false,
                            "type": "string"
                          },
                          {
                            "name": "query",
                            "required": false,
                            "type": "string"
                          }
                        ]
                      },
                      {
                        "actions": [],
                        "name": "syntax_help",
                        "note": "exactly one of `name` or `reference_id` is required",
                        "output_schema_fingerprint": "blake3:219fdb853fb5b1c5a40de0010463c820ba26d85b01d9ee1ad8766ff45b848465",
                        "output_schema_version": "2",
                        "params": [
                          {
                            "name": "max_output_tokens",
                            "nullable": true,
                            "required": false,
                            "type": "integer"
                          },
                          {
                            "name": "name",
                            "required": false,
                            "type": "string"
                          },
                          {
                            "name": "reference_id",
                            "required": false,
                            "type": "string"
                          },
                          {
                            "name": "type_name",
                            "nullable": true,
                            "required": false,
                            "type": "string"
                          }
                        ]
                      },
                      {
                        "actions": [],
                        "name": "its_help",
                        "params": [
                          {
                            "name": "max_output_tokens",
                            "nullable": true,
                            "required": false,
                            "type": "integer"
                          },
                          {
                            "name": "question",
                            "required": true,
                            "type": "string"
                          }
                        ]
                      }
                    ]
                  },
                  "workspace": {
                    "opt_in_tools": [
                      {
                        "actions": [],
                        "name": "references",
                        "note": "one of `symbol` or `path` with `line` and/or `line_content` is required; opt-in — a launch has to name it with `--enable-tool references`",
                        "output_schema_fingerprint": "blake3:859cbcac6a3e708b603de266d134c2a0782de818e4a220144fa6fc4bedd1519a",
                        "output_schema_version": "1",
                        "params": [
                          {
                            "name": "anchor_root_id",
                            "nullable": true,
                            "required": false,
                            "type": "string"
                          },
                          {
                            "name": "area_path_prefix",
                            "nullable": true,
                            "required": false,
                            "type": "string"
                          },
                          {
                            "name": "area_root_id",
                            "nullable": true,
                            "required": false,
                            "type": "string"
                          },
                          {
                            "name": "column",
                            "nullable": true,
                            "required": false,
                            "type": "integer"
                          },
                          {
                            "name": "include_declaration",
                            "nullable": true,
                            "required": false,
                            "type": "boolean"
                          },
                          {
                            "name": "include_preview",
                            "nullable": true,
                            "required": false,
                            "type": "boolean"
                          },
                          {
                            "name": "kinds",
                            "required": false,
                            "type": "array<string>"
                          },
                          {
                            "name": "limit",
                            "nullable": true,
                            "required": false,
                            "type": "integer"
                          },
                          {
                            "name": "line",
                            "nullable": true,
                            "required": false,
                            "type": "integer"
                          },
                          {
                            "name": "line_content",
                            "nullable": true,
                            "required": false,
                            "type": "string"
                          },
                          {
                            "name": "max_files",
                            "nullable": true,
                            "required": false,
                            "type": "integer"
                          },
                          {
                            "name": "max_output_tokens",
                            "nullable": true,
                            "required": false,
                            "type": "integer"
                          },
                          {
                            "name": "path",
                            "nullable": true,
                            "required": false,
                            "type": "string"
                          },
                          {
                            "name": "root_id",
                            "nullable": true,
                            "required": false,
                            "type": "string"
                          },
                          {
                            "name": "symbol",
                            "nullable": true,
                            "required": false,
                            "type": "string"
                          }
                        ]
                      }
                    ],
                    "tools": [
                      {
                        "actions": [
                          {
                            "name": "info",
                            "required": []
                          },
                          {
                            "name": "tree",
                            "note": "in infobase mode (`mode=infobase`, or `connection` set under `mode=auto`) `meta_type` is required and only `tree`/`object` are available",
                            "required": []
                          },
                          {
                            "name": "object",
                            "required": [
                              "object_type",
                              "object_name"
                            ]
                          },
                          {
                            "name": "form",
                            "required": [
                              "object_type"
                            ]
                          },
                          {
                            "name": "status",
                            "required": []
                          }
                        ],
                        "name": "metadata",
                        "params": [
                          {
                            "name": "action",
                            "required": true,
                            "type": "string"
                          },
                          {
                            "name": "connection",
                            "nullable": true,
                            "required": false,
                            "type": "string"
                          },
                          {
                            "name": "filter",
                            "nullable": true,
                            "required": false,
                            "type": "string"
                          },
                          {
                            "name": "form_name",
                            "nullable": true,
                            "required": false,
                            "type": "string"
                          },
                          {
                            "name": "max_items",
                            "nullable": true,
                            "required": false,
                            "type": "integer"
                          },
                          {
                            "name": "max_output_tokens",
                            "nullable": true,
                            "required": false,
                            "type": "integer"
                          },
                          {
                            "name": "meta_type",
                            "nullable": true,
                            "required": false,
                            "type": "string"
                          },
                          {
                            "name": "mode",
                            "nullable": true,
                            "required": false,
                            "type": "string"
                          },
                          {
                            "name": "name_mask",
                            "nullable": true,
                            "required": false,
                            "type": "string"
                          },
                          {
                            "name": "object_name",
                            "nullable": true,
                            "required": false,
                            "type": "string"
                          },
                          {
                            "name": "object_type",
                            "nullable": true,
                            "required": false,
                            "type": "string"
                          }
                        ]
                      },
                      {
                        "actions": [
                          {
                            "name": "search_code",
                            "required": [
                              "query"
                            ]
                          },
                          {
                            "name": "status",
                            "required": []
                          },
                          {
                            "name": "list_platform",
                            "required": []
                          },
                          {
                            "name": "find_docs",
                            "required": [
                              "query"
                            ]
                          },
                          {
                            "name": "search_docs",
                            "required": [
                              "query"
                            ]
                          }
                        ],
                        "name": "search",
                        "output_schema_fingerprint": "blake3:253ed1cc1c5757ff39f5de35cdd81d724ab9509c93e6730f60c8302a4c11a553",
                        "output_schema_version": "9",
                        "params": [
                          {
                            "name": "action",
                            "required": true,
                            "type": "string"
                          },
                          {
                            "name": "kind",
                            "required": false,
                            "type": "any"
                          },
                          {
                            "name": "limit",
                            "nullable": true,
                            "required": false,
                            "type": "integer"
                          },
                          {
                            "name": "max_output_tokens",
                            "nullable": true,
                            "required": false,
                            "type": "integer"
                          },
                          {
                            "name": "name",
                            "nullable": true,
                            "required": false,
                            "type": "string"
                          },
                          {
                            "name": "query",
                            "required": false,
                            "type": "string"
                          }
                        ]
                      },
                      {
                        "actions": [
                          {
                            "name": "validate",
                            "required": [
                              "query"
                            ]
                          },
                          {
                            "name": "execute",
                            "required": [
                              "query"
                            ]
                          },
                          {
                            "name": "schema",
                            "required": []
                          }
                        ],
                        "name": "query",
                        "params": [
                          {
                            "name": "action",
                            "required": true,
                            "type": "string"
                          },
                          {
                            "name": "connection",
                            "nullable": true,
                            "required": false,
                            "type": "string"
                          },
                          {
                            "name": "limit",
                            "nullable": true,
                            "required": false,
                            "type": "integer"
                          },
                          {
                            "name": "max_output_tokens",
                            "nullable": true,
                            "required": false,
                            "type": "integer"
                          },
                          {
                            "name": "parameters",
                            "nullable": true,
                            "required": false,
                            "type": "object"
                          },
                          {
                            "name": "query",
                            "nullable": true,
                            "required": false,
                            "type": "string"
                          },
                          {
                            "name": "root_id",
                            "nullable": true,
                            "required": false,
                            "type": "string"
                          }
                        ]
                      },
                      {
                        "actions": [
                          {
                            "name": "check",
                            "required": []
                          },
                          {
                            "name": "run",
                            "required": []
                          },
                          {
                            "name": "eval",
                            "required": []
                          }
                        ],
                        "name": "execute",
                        "params": [
                          {
                            "name": "action",
                            "required": true,
                            "type": "string"
                          },
                          {
                            "name": "code",
                            "required": true,
                            "type": "string"
                          },
                          {
                            "name": "connection",
                            "nullable": true,
                            "required": false,
                            "type": "string"
                          },
                          {
                            "name": "max_output_tokens",
                            "nullable": true,
                            "required": false,
                            "type": "integer"
                          }
                        ]
                      },
                      {
                        "actions": [],
                        "name": "event_log",
                        "params": [
                          {
                            "name": "connection",
                            "nullable": true,
                            "required": false,
                            "type": "string"
                          },
                          {
                            "name": "contains",
                            "nullable": true,
                            "required": false,
                            "type": "string"
                          },
                          {
                            "name": "date_from",
                            "nullable": true,
                            "required": false,
                            "type": "string"
                          },
                          {
                            "name": "date_to",
                            "nullable": true,
                            "required": false,
                            "type": "string"
                          },
                          {
                            "name": "event",
                            "nullable": true,
                            "required": false,
                            "type": "string"
                          },
                          {
                            "name": "level",
                            "nullable": true,
                            "required": false,
                            "type": "string"
                          },
                          {
                            "name": "limit",
                            "nullable": true,
                            "required": false,
                            "type": "integer"
                          },
                          {
                            "name": "max_output_tokens",
                            "nullable": true,
                            "required": false,
                            "type": "integer"
                          },
                          {
                            "name": "metadata",
                            "nullable": true,
                            "required": false,
                            "type": "string"
                          },
                          {
                            "name": "user",
                            "nullable": true,
                            "required": false,
                            "type": "string"
                          }
                        ]
                      },
                      {
                        "actions": [
                          {
                            "name": "attach",
                            "required": [
                              "host",
                              "infobase"
                            ]
                          },
                          {
                            "name": "disconnect",
                            "required": []
                          },
                          {
                            "name": "set_breakpoint",
                            "required": [
                              "module",
                              "line"
                            ]
                          },
                          {
                            "name": "remove_breakpoint",
                            "required": [
                              "module",
                              "line"
                            ]
                          },
                          {
                            "name": "continue",
                            "required": []
                          },
                          {
                            "name": "step",
                            "required": [
                              "direction"
                            ]
                          },
                          {
                            "name": "wait_stop",
                            "required": []
                          },
                          {
                            "name": "stack_trace",
                            "required": []
                          },
                          {
                            "name": "locals",
                            "required": []
                          },
                          {
                            "name": "eval",
                            "required": [
                              "expression"
                            ]
                          }
                        ],
                        "name": "debug",
                        "params": [
                          {
                            "name": "action",
                            "required": true,
                            "type": "string"
                          },
                          {
                            "name": "auto_attach",
                            "required": false,
                            "type": "array<string>"
                          },
                          {
                            "name": "condition",
                            "nullable": true,
                            "required": false,
                            "type": "string"
                          },
                          {
                            "name": "config_root",
                            "nullable": true,
                            "required": false,
                            "type": "string"
                          },
                          {
                            "name": "direction",
                            "nullable": true,
                            "required": false,
                            "type": "string"
                          },
                          {
                            "name": "expression",
                            "nullable": true,
                            "required": false,
                            "type": "string"
                          },
                          {
                            "name": "extensions",
                            "required": false,
                            "type": "array<array<string>>"
                          },
                          {
                            "name": "host",
                            "nullable": true,
                            "required": false,
                            "type": "string"
                          },
                          {
                            "name": "infobase",
                            "nullable": true,
                            "required": false,
                            "type": "string"
                          },
                          {
                            "name": "line",
                            "nullable": true,
                            "required": false,
                            "type": "integer"
                          },
                          {
                            "name": "max_output_tokens",
                            "nullable": true,
                            "required": false,
                            "type": "integer"
                          },
                          {
                            "name": "module",
                            "nullable": true,
                            "required": false,
                            "type": "string"
                          },
                          {
                            "name": "port",
                            "nullable": true,
                            "required": false,
                            "type": "integer"
                          },
                          {
                            "name": "stack_level",
                            "nullable": true,
                            "required": false,
                            "type": "integer"
                          },
                          {
                            "name": "timeout_secs",
                            "nullable": true,
                            "required": false,
                            "type": "integer"
                          }
                        ]
                      },
                      {
                        "actions": [
                          {
                            "name": "overview",
                            "required": []
                          },
                          {
                            "name": "schema",
                            "required": []
                          },
                          {
                            "name": "status",
                            "required": []
                          },
                          {
                            "name": "node",
                            "required": [
                              "id"
                            ]
                          },
                          {
                            "name": "source",
                            "required": [
                              "ids"
                            ]
                          },
                          {
                            "name": "neighbors",
                            "required": [
                              "id"
                            ]
                          },
                          {
                            "name": "callers",
                            "required": [
                              "id"
                            ]
                          },
                          {
                            "name": "callees",
                            "required": [
                              "id"
                            ]
                          },
                          {
                            "name": "resolve",
                            "required": [
                              "query"
                            ]
                          }
                        ],
                        "name": "graph",
                        "output_schema_fingerprint": "blake3:4d966d91520f2b5fc5b52b7a030dc11f6b0796bca427688d904199b098fa1184",
                        "output_schema_version": "36",
                        "params": [
                          {
                            "name": "action",
                            "required": true,
                            "type": "string"
                          },
                          {
                            "name": "call_sites",
                            "nullable": true,
                            "required": false,
                            "type": "boolean"
                          },
                          {
                            "name": "depth",
                            "nullable": true,
                            "required": false,
                            "type": "integer"
                          },
                          {
                            "name": "detail",
                            "nullable": true,
                            "required": false,
                            "type": "string"
                          },
                          {
                            "name": "dir",
                            "nullable": true,
                            "required": false,
                            "type": "string"
                          },
                          {
                            "name": "edge_kinds",
                            "required": false,
                            "type": "array<string>"
                          },
                          {
                            "name": "id",
                            "nullable": true,
                            "required": false,
                            "type": "string"
                          },
                          {
                            "name": "ids",
                            "required": false,
                            "type": "array<string>"
                          },
                          {
                            "name": "max_call_sites",
                            "nullable": true,
                            "required": false,
                            "type": "integer"
                          },
                          {
                            "name": "max_nodes",
                            "nullable": true,
                            "required": false,
                            "type": "integer"
                          },
                          {
                            "name": "max_output_tokens",
                            "nullable": true,
                            "required": false,
                            "type": "integer"
                          },
                          {
                            "name": "provenance",
                            "required": false,
                            "type": "array<string>"
                          },
                          {
                            "name": "query",
                            "nullable": true,
                            "required": false,
                            "type": "string"
                          },
                          {
                            "name": "top",
                            "nullable": true,
                            "required": false,
                            "type": "integer"
                          }
                        ]
                      },
                      {
                        "actions": [],
                        "name": "symbol_info",
                        "note": "one of `symbol` or `path`+`line` is required",
                        "output_schema_fingerprint": "blake3:117d545855a5f8370e8835fb9ec1903166654396c7fd3e74282439f3c1dff9cf",
                        "output_schema_version": "1",
                        "params": [
                          {
                            "name": "column",
                            "nullable": true,
                            "required": false,
                            "type": "integer"
                          },
                          {
                            "name": "include",
                            "required": false,
                            "type": "array<string>"
                          },
                          {
                            "name": "line",
                            "nullable": true,
                            "required": false,
                            "type": "integer"
                          },
                          {
                            "name": "locale",
                            "nullable": true,
                            "required": false,
                            "type": "string"
                          },
                          {
                            "name": "max_output_tokens",
                            "nullable": true,
                            "required": false,
                            "type": "integer"
                          },
                          {
                            "name": "member_kind",
                            "required": false,
                            "type": "any"
                          },
                          {
                            "name": "member_name",
                            "nullable": true,
                            "required": false,
                            "type": "string"
                          },
                          {
                            "name": "path",
                            "nullable": true,
                            "required": false,
                            "type": "string"
                          },
                          {
                            "name": "root_id",
                            "nullable": true,
                            "required": false,
                            "type": "string"
                          },
                          {
                            "name": "symbol",
                            "nullable": true,
                            "required": false,
                            "type": "string"
                          }
                        ]
                      },
                      {
                        "actions": [
                          {
                            "name": "catalog",
                            "required": []
                          },
                          {
                            "name": "schema",
                            "required": []
                          },
                          {
                            "name": "status",
                            "required": []
                          },
                          {
                            "name": "file",
                            "required": [
                              "path"
                            ]
                          },
                          {
                            "name": "workspace",
                            "required": []
                          }
                        ],
                        "name": "diagnostics",
                        "output_schema_fingerprint": "blake3:2de7d4aa984c1a12c46e892e51dd64097a7087221b705850d7ebd90b609e1031",
                        "output_schema_version": "16",
                        "params": [
                          {
                            "name": "action",
                            "required": true,
                            "type": "string"
                          },
                          {
                            "name": "codes",
                            "required": false,
                            "type": "array<string>"
                          },
                          {
                            "name": "detail",
                            "nullable": true,
                            "required": false,
                            "type": "string"
                          },
                          {
                            "name": "locale",
                            "nullable": true,
                            "required": false,
                            "type": "string"
                          },
                          {
                            "name": "max_files",
                            "nullable": true,
                            "required": false,
                            "type": "integer"
                          },
                          {
                            "name": "max_findings",
                            "nullable": true,
                            "required": false,
                            "type": "integer"
                          },
                          {
                            "name": "max_output_tokens",
                            "nullable": true,
                            "required": false,
                            "type": "integer"
                          },
                          {
                            "name": "min_severity",
                            "nullable": true,
                            "required": false,
                            "type": "string"
                          },
                          {
                            "name": "path",
                            "nullable": true,
                            "required": false,
                            "type": "string"
                          },
                          {
                            "name": "range_end",
                            "nullable": true,
                            "required": false,
                            "type": "integer"
                          },
                          {
                            "name": "range_start",
                            "nullable": true,
                            "required": false,
                            "type": "integer"
                          },
                          {
                            "name": "root_id",
                            "nullable": true,
                            "required": false,
                            "type": "string"
                          }
                        ]
                      },
                      {
                        "actions": [],
                        "name": "outline",
                        "params": [
                          {
                            "name": "max_output_tokens",
                            "nullable": true,
                            "required": false,
                            "type": "integer"
                          },
                          {
                            "name": "mode",
                            "nullable": true,
                            "required": false,
                            "type": "string"
                          },
                          {
                            "name": "path",
                            "required": true,
                            "type": "string"
                          },
                          {
                            "name": "root_id",
                            "nullable": true,
                            "required": false,
                            "type": "string"
                          }
                        ]
                      },
                      {
                        "actions": [],
                        "name": "syntax_help",
                        "note": "exactly one of `name` or `reference_id` is required",
                        "output_schema_fingerprint": "blake3:219fdb853fb5b1c5a40de0010463c820ba26d85b01d9ee1ad8766ff45b848465",
                        "output_schema_version": "2",
                        "params": [
                          {
                            "name": "max_output_tokens",
                            "nullable": true,
                            "required": false,
                            "type": "integer"
                          },
                          {
                            "name": "name",
                            "required": false,
                            "type": "string"
                          },
                          {
                            "name": "reference_id",
                            "required": false,
                            "type": "string"
                          },
                          {
                            "name": "type_name",
                            "nullable": true,
                            "required": false,
                            "type": "string"
                          }
                        ]
                      }
                    ]
                  }
                }
              }
            }"#]].assert_eq(&serde_json::to_string_pretty(&Value::Object(doc)).unwrap());
    }
}
