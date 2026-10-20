//! Every JSON example in the tool document is a body the tool could actually serve.
//!
//! A document example is the first thing a client reads and the last thing anyone reruns.
//! Two of them here were older than the location contract and showed a shape no build had
//! served for months — plausible, self-consistent, and wrong in the one field a consumer
//! addresses with. Nothing failed, because nothing was checking.
//!
//! What this gate does NOT check is written down beside what it does: an example of a tool
//! that publishes no schema is not validated at all, and the blocks a schema declares as a
//! bare object are validated only as objects. Both limits are enforced rather than trusted —
//! a tool that gains a schema drops out of the exemption list by failing here.
//! `tool=search:error` marks the embedding RPC error, validated separately from tool results.

use std::collections::BTreeMap;
use std::path::Path;

use mcp_server::{serve_stream, McpProfile, McpServer, SharedState, ToolGate};
use rmcp::service::RunningService;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{json, Value};
use tempfile::TempDir;

type Client = RunningService<RoleClient, ()>;

const DOC: &str = "../../docs/mcp/TOOLS_AND_EXTENSION.md";

/// Tools whose examples this gate cannot validate, because they publish no output schema.
///
/// A list, not a silent skip: an example under one of these names is announced as unchecked,
/// and the moment the tool starts publishing a schema the gate fails on its own exemption
/// instead of quietly keeping it forever.
const WITHOUT_SCHEMA: &[&str] = &["outline"];

/// One fenced block: the tool it claims to be an answer of, and its parsed body.
struct Example {
    line: usize,
    tool: String,
    body: Value,
}

/// Read the fenced JSON blocks and the tool each one names.
///
/// The marker is part of the fence info string (```json tool=references), so a block cannot
/// be added without answering the question "whose answer is this?". `tool=none` is the
/// answer for a payload that is not an MCP tool body at all — the 1C HTTP service's version
/// reply — and it is spelled out rather than left to a missing marker, which would make
/// "forgot to mark it" and "deliberately not a tool body" the same thing.
fn examples(document: &str) -> Vec<Example> {
    let mut found = Vec::new();
    let mut lines = document.lines().enumerate();
    while let Some((index, line)) = lines.next() {
        let Some(info) = line.strip_prefix("```json") else {
            continue;
        };
        let tool = info
            .split_whitespace()
            .find_map(|word| word.strip_prefix("tool="))
            .unwrap_or_else(|| {
                panic!(
                    "docs/mcp/TOOLS_AND_EXTENSION.md:{}: a JSON example must name the tool it \
                     is an answer of — ```json tool=<name>, or tool=none when it is not a \
                     tool body",
                    index + 1
                )
            })
            .to_owned();
        let mut text = String::new();
        for (_, line) in lines.by_ref() {
            if line.starts_with("```") {
                break;
            }
            text.push_str(line);
            text.push('\n');
        }
        let body = serde_json::from_str(&text).unwrap_or_else(|error| {
            panic!("docs/mcp/TOOLS_AND_EXTENSION.md:{}: not JSON: {error}", index + 1)
        });
        found.push(Example { line: index + 1, tool, body });
    }
    found
}

/// The fixture workspace, so a workspace-profile session starts at all.
fn stage_fixture() -> TempDir {
    let src = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-metadata/fixtures/designer"));
    let dst = TempDir::new().expect("scratch workspace");
    for entry in walkdir::WalkDir::new(src) {
        let entry = entry.expect("walk fixture");
        let rel = entry.path().strip_prefix(src).expect("path under fixture root");
        let target = dst.path().join(rel);
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&target).expect("mkdir");
        } else {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).expect("mkdir parent");
            }
            std::fs::copy(entry.path(), &target).expect("copy fixture file");
        }
    }
    dst
}

async fn session(server: McpServer) -> Client {
    let (client_io, server_io) = tokio::io::duplex(1024 * 1024);
    tokio::spawn(serve_stream(server, server_io));
    ().serve(client_io).await.expect("session initialized")
}

/// The schemas as a CLIENT sees them: read off `tools/list`, not off an internal helper.
///
/// Both profiles, because the surface is split between them — `syntax_help` is served by the
/// reference profile and `references` only by a workspace launch that opted in — and a
/// document example does not say which launch it came from.
async fn published_schemas() -> BTreeMap<String, Value> {
    let ws = stage_fixture();
    let state = SharedState::workspace(ws.path().to_path_buf()).expect("valid workspace project");
    let gate = ToolGate::for_launch(McpProfile::Workspace, &["references".to_owned()]);
    let workspace = McpServer::with_gate(McpProfile::Workspace, state, &gate);
    let reference = McpServer::new(McpProfile::Reference, SharedState::reference(None));

    let mut schemas = BTreeMap::new();
    for server in [workspace, reference] {
        let client = session(server).await;
        let listed = client.list_tools(Default::default()).await.expect("tools/list");
        for tool in listed.tools {
            if let Some(schema) = tool.output_schema.as_ref() {
                schemas
                    .entry(tool.name.to_string())
                    .or_insert_with(|| Value::Object((**schema).clone()));
            }
        }
        client.cancel().await.ok();
    }
    schemas
}

/// RPC errors have no successful outputSchema; reuse only its closed failure definition.
fn embedding_error_schema(search_schema: &Value) -> Value {
    assert!(search_schema["$defs"]["SemanticFailureSchema"].is_object());
    json!({
        "$defs": search_schema["$defs"],
        "type": "object",
        "additionalProperties": false,
        "required": ["code", "message", "data"],
        "properties": {
            "code": {"const": -32603},
            "message": {"const": "Semantic embedding failed; use lexical search or retry."},
            "data": {
                "type": "object",
                "additionalProperties": false,
                "required": ["semantic_failure"],
                "properties": {"semantic_failure": {"$ref": "#/$defs/SemanticFailureSchema"}}
            }
        }
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_documented_example_validates_against_the_schema_its_tool_publishes() {
    let document = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(DOC))
        .expect("the tool document is checked in beside the crate");
    let examples = examples(&document);
    assert!(!examples.is_empty(), "a gate over no examples is green whatever the examples say");
    let schemas = published_schemas().await;
    let error_schema = embedding_error_schema(schemas.get("search").expect("search outputSchema"));
    assert!(examples.iter().any(|example| example.tool == "search:error"));

    let mut unchecked = Vec::new();
    for example in &examples {
        if example.tool == "none" {
            continue;
        }
        if WITHOUT_SCHEMA.contains(&example.tool.as_str()) {
            assert!(
                !schemas.contains_key(&example.tool),
                "`{}` publishes an output schema now, so its example at \
                 docs/mcp/TOOLS_AND_EXTENSION.md:{} can be validated — drop it from \
                 WITHOUT_SCHEMA",
                example.tool,
                example.line,
            );
            unchecked.push(format!("{} (line {})", example.tool, example.line));
            continue;
        }
        let schema = if example.tool == "search:error" {
            &error_schema
        } else {
            schemas.get(&example.tool).unwrap_or_else(|| {
                panic!(
                    "docs/mcp/TOOLS_AND_EXTENSION.md:{}: no tool named `{}` publishes a schema; \
                 either the name is wrong or it belongs in WITHOUT_SCHEMA",
                    example.line, example.tool,
                )
            })
        };
        let validator = jsonschema::validator_for(schema).unwrap_or_else(|error| {
            panic!("`{}` publishes an unusable schema: {error}", example.tool)
        });
        if let Err(error) = validator.validate(&example.body) {
            panic!(
                "docs/mcp/TOOLS_AND_EXTENSION.md:{}: the `{}` example is not a body that tool \
                 can serve: {error}\n{}",
                example.line,
                example.tool,
                serde_json::to_string_pretty(&example.body).unwrap_or_default(),
            );
        }
        if example.tool == "search:error" {
            let mut unsafe_example = example.body.clone();
            unsafe_example["data"]["semantic_failure"]["provider_body"] = json!("private");
            assert!(!validator.is_valid(&unsafe_example), "RPC diagnostics must stay closed");
        }
    }

    // Said out loud rather than left to whoever reads a green run: these examples were seen
    // and not checked.
    if !unchecked.is_empty() {
        println!("examples left unvalidated (their tools publish no schema): {unchecked:?}");
    }
}
