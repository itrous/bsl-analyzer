//! The contract declaration over a real MCP session: a consumer must be able to discover
//! and read it with the standard `resources/list` + `resources/read` calls, without
//! knowing anything build-specific beforehand.

use mcp_server::{contract, serve_stream, McpProfile, McpServer, SharedState};
use rmcp::model::{CallToolRequestParams, ReadResourceRequestParams, ResourceContents};
use rmcp::ServiceExt;
use serde_json::{json, Map, Value};
use tempfile::TempDir;

/// The reference profile needs no workspace build, and the resource is profile-independent.
fn reference_server() -> McpServer {
    McpServer::new(McpProfile::Reference, SharedState::reference(None))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
async fn contract_is_discoverable_and_readable_over_a_session() {
    let (client_io, server_io) = tokio::io::duplex(1024 * 1024);
    tokio::spawn(serve_stream(reference_server(), server_io));
    let client = ().serve(client_io).await.expect("session initialized");

    let info = client.peer_info().expect("server info after initialize");
    assert!(info.capabilities.resources.is_some(), "resources capability not advertised");
    // A consumer reads `serverInfo` to learn which analyzer build it is talking to, so it
    // must name the analyzer rather than the transport library underneath it. The field is
    // optional on a negotiated peer because discovery responses need not carry it; an
    // absent identity is as much a contract break here as a wrong one.
    let server_info = info.server_info.as_ref().expect("server identity after initialize");
    assert_eq!(server_info.name, "bsl-analyzer");

    let listed = client.list_resources(None).await.expect("resources/list");
    assert!(
        listed.resources.iter().any(|r| r.uri == contract::CONTRACT_URI),
        "contract resource is not listed: {listed:?}"
    );

    let read = client
        .read_resource(ReadResourceRequestParams::new(contract::CONTRACT_URI))
        .await
        .expect("resources/read");
    let [ResourceContents::TextResourceContents { text, .. }] = &read.contents[..] else {
        panic!("expected one text content, got {:?}", read.contents);
    };
    let doc: serde_json::Value = serde_json::from_str(text).expect("contract is valid JSON");

    assert_eq!(doc["contract_version"], contract::CONTRACT_VERSION);
    assert_eq!(doc["build_version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(
        doc["transports"]["workspace"]["broker-required"],
        serde_json::json!({
            "backend_pid_required": true,
            "backend_pid_source": "supervisor launching bsl-analyzer-app directly",
            "auto_launch": false,
            "stdio_fallback": false,
            "peer_identity": "supervised-pid+platform-trust",
            "platforms": mcp_server::broker::SUPERVISED_PID_PLATFORMS
        })
    );

    // Both profiles are declared regardless of which one is serving: a consumer choosing a
    // profile must be able to see what it would get before starting that server.
    let workspace = &doc["mcp"]["profiles"]["workspace"]["tools"];
    let names: Vec<&str> =
        workspace.as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"diagnostics"), "{names:?}");

    let diagnostics =
        workspace.as_array().unwrap().iter().find(|t| t["name"] == "diagnostics").unwrap();
    let actions: Vec<&str> = diagnostics["actions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["name"].as_str().unwrap())
        .collect();
    assert_eq!(actions, ["catalog", "schema", "status", "file", "workspace"]);

    let file_action =
        diagnostics["actions"].as_array().unwrap().iter().find(|a| a["name"] == "file").unwrap();
    assert_eq!(file_action["required"], serde_json::json!(["path"]));

    let tools = client.list_tools(Default::default()).await.expect("tools/list");
    let syntax_help =
        tools.tools.iter().find(|tool| tool.name == "syntax_help").expect("syntax_help is listed");
    assert!(
        syntax_help.output_schema.as_ref().is_some_and(|schema| {
            serde_json::to_string(schema.as_ref())
                .is_ok_and(|schema| schema.contains("schema_version"))
        }),
        "syntax_help publishes a versioned outputSchema"
    );
    let result = client
        .call_tool(
            CallToolRequestParams::new("syntax_help")
                .with_arguments(Map::from_iter([("name".to_owned(), json!("Массив"))])),
        )
        .await
        .expect("syntax_help call");
    let body = result.structured_content.expect("syntax_help structuredContent");
    assert_eq!(body["schema_version"], "2");
    assert_eq!(body["kind"], "type");
    assert_eq!(body["name"], "Массив");
    assert!(
        result.content.first().and_then(|content| content.as_text()).is_some(),
        "legacy text content remains available"
    );

    client.cancel().await.expect("session closed");
}

/// Nothing else is served under the contract scheme; a typo must fail loudly rather than
/// return some other resource.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_resource_uri_is_rejected() {
    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    tokio::spawn(serve_stream(reference_server(), server_io));
    let client = ().serve(client_io).await.expect("session initialized");

    let err = client
        .read_resource(ReadResourceRequestParams::new("bsl-analyzer://nope"))
        .await
        .expect_err("unknown resource must be an error");
    assert!(err.to_string().contains("nope"), "{err}");

    client.cancel().await.expect("session closed");
}

/// A parameter the declaration says an action needs, with the other declared parameters
/// present so the probe fails on this one alone.
struct Probe {
    tool: String,
    action: String,
    omitted: String,
    arguments: Map<String, Value>,
}

fn dummy_for(ty: &str) -> Value {
    match ty {
        "integer" | "number" => json!(1),
        "boolean" => json!(true),
        "object" => json!({}),
        ty if ty.starts_with("array<") => json!(["x"]),
        _ => json!("x"),
    }
}

/// `require()` runs before the readiness gate for most tools, so a missing parameter comes
/// back as a parameter error even against an empty workspace. These two resolve their
/// arguments inside the gate instead, where an unbuilt project answers "still loading"
/// before it ever looks at them — probing them would assert on the gate, not the contract.
fn resolves_arguments_behind_the_readiness_gate(tool: &str, action: &str) -> bool {
    tool == "graph" || (tool == "metadata" && action == "object")
}

fn probes(profile: &Value) -> Vec<Probe> {
    let mut probes = Vec::new();
    for tool in profile["tools"].as_array().unwrap() {
        let name = tool["name"].as_str().unwrap();
        let types: std::collections::HashMap<&str, &str> = tool["params"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| (p["name"].as_str().unwrap(), p["type"].as_str().unwrap()))
            .collect();
        for action in tool["actions"].as_array().unwrap() {
            let action_name = action["name"].as_str().unwrap();
            if resolves_arguments_behind_the_readiness_gate(name, action_name) {
                continue;
            }
            let required: Vec<&str> = action["required"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| r.as_str().unwrap())
                .collect();
            for omitted in &required {
                let mut arguments = Map::new();
                arguments.insert("action".into(), json!(action_name));
                for other in required.iter().filter(|r| *r != omitted) {
                    arguments.insert((*other).into(), dummy_for(types[*other]));
                }
                // Schema-level required parameters must be present or the call fails on
                // deserialization instead of reaching the action's own check.
                for param in tool["params"].as_array().unwrap() {
                    let param_name = param["name"].as_str().unwrap();
                    if param["required"] == json!(true)
                        && param_name != "action"
                        && param_name != *omitted
                    {
                        arguments.insert(param_name.into(), dummy_for(types[param_name]));
                    }
                }
                probes.push(Probe {
                    tool: name.to_string(),
                    action: action_name.to_string(),
                    omitted: (*omitted).to_string(),
                    arguments,
                });
            }
        }
    }
    probes
}

/// The declaration's required-parameter lists are hand-written, and the unknown-action
/// messages are generated from the same tables — so a table that drifted from the dispatch
/// code would drift consistently and every string comparison would still pass. This asks
/// the running server instead: omit a parameter the contract calls required, and the tool
/// must reject the call for exactly that parameter.
async fn assert_required_params_are_enforced(server: McpServer, profile_key: &str) {
    let (client_io, server_io) = tokio::io::duplex(1024 * 1024);
    tokio::spawn(serve_stream(server, server_io));
    let client = ().serve(client_io).await.expect("session initialized");

    let doc = contract::document();
    let probes = probes(&doc["mcp"]["profiles"][profile_key]);
    assert!(!probes.is_empty(), "no required parameters declared for profile {profile_key}");

    for probe in probes {
        let call = client.call_tool(
            CallToolRequestParams::new(probe.tool.clone()).with_arguments(probe.arguments.clone()),
        );
        // A rejection reaches the caller in one of two shapes. Our own guards raise a
        // JSON-RPC error; a parameter that serde rejects before the tool body runs comes
        // back as a result flagged `isError`, carrying the same reason as text. Accepting
        // only the first shape would make this gate pass vacuously on every tool whose
        // required parameter is enforced by deserialization rather than by a hand-written
        // check.
        let rejection = match call.await {
            Err(err) => err.to_string(),
            Ok(result) => {
                assert_eq!(
                    result.is_error,
                    Some(true),
                    "{}/{} accepted a call without the required '{}': {:?}",
                    probe.tool,
                    probe.action,
                    probe.omitted,
                    probe.arguments
                );
                result
                    .content
                    .iter()
                    .filter_map(|block| block.as_text())
                    .map(|text| text.text.as_str())
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        };
        let explicit = format!("'{}' is required", probe.omitted);
        let serde = format!("missing field `{}`", probe.omitted);
        assert!(
            rejection.contains(&explicit) || rejection.contains(&serde),
            "{}/{} rejected a call missing '{}' for another reason: {rejection}",
            probe.tool,
            probe.action,
            probe.omitted
        );
    }

    client.cancel().await.expect("session closed");
}

/// The full call the declaration says is complete: the action plus every parameter it
/// declares required.
fn complete_calls(profile: &Value) -> Vec<Probe> {
    let mut calls = Vec::new();
    for tool in profile["tools"].as_array().unwrap() {
        let name = tool["name"].as_str().unwrap();
        let types: std::collections::HashMap<&str, &str> = tool["params"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| (p["name"].as_str().unwrap(), p["type"].as_str().unwrap()))
            .collect();
        for action in tool["actions"].as_array().unwrap() {
            let action_name = action["name"].as_str().unwrap();
            if resolves_arguments_behind_the_readiness_gate(name, action_name)
                || reaches_out_of_process(name, action_name)
            {
                continue;
            }
            let mut arguments = Map::new();
            arguments.insert("action".into(), json!(action_name));
            for required in action["required"].as_array().unwrap() {
                let required = required.as_str().unwrap();
                arguments.insert(required.into(), dummy_for(types[required]));
            }
            for param in tool["params"].as_array().unwrap() {
                let param_name = param["name"].as_str().unwrap();
                if param["required"] == json!(true) && param_name != "action" {
                    arguments.insert(param_name.into(), dummy_for(types[param_name]));
                }
            }
            calls.push(Probe {
                tool: name.to_string(),
                action: action_name.to_string(),
                omitted: String::new(),
                arguments,
            });
        }
    }
    calls
}

/// `debug attach` opens a TCP connection to the address it is handed, so a probe with a
/// placeholder host would leave the process. Nothing else here talks to the outside world:
/// the other actions stop at "no 1C connection configured" or at an empty workspace.
fn reaches_out_of_process(tool: &str, action: &str) -> bool {
    tool == "debug" && action == "attach"
}

/// The mirror of [`assert_required_params_are_enforced`]: that one catches a requirement
/// the declaration invented, this one catches a requirement it forgot. Hand the tool
/// everything the contract says an action needs, and no answer may still be "'x' is
/// required" — that would be a `require()` in the dispatch code with no counterpart in the
/// declaration, which is how a consumer ends up with a call the contract promised.
async fn assert_no_undeclared_requirements(server: McpServer, profile_key: &str) {
    let (client_io, server_io) = tokio::io::duplex(1024 * 1024);
    tokio::spawn(serve_stream(server, server_io));
    let client = ().serve(client_io).await.expect("session initialized");

    let doc = contract::document();
    let calls = complete_calls(&doc["mcp"]["profiles"][profile_key]);
    assert!(!calls.is_empty(), "no actions probed for profile {profile_key}");

    for call in calls {
        let answer = client
            .call_tool(
                CallToolRequestParams::new(call.tool.clone())
                    .with_arguments(call.arguments.clone()),
            )
            .await;
        if let Err(err) = answer {
            let err = err.to_string();
            assert!(
                !err.contains("is required"),
                "{}/{} needs a parameter the contract does not declare: {err}",
                call.tool,
                call.action
            );
        }
    }

    client.cancel().await.expect("session closed");
}

fn workspace_server(root: &TempDir) -> McpServer {
    McpServer::new(
        McpProfile::Workspace,
        SharedState::workspace(root.path().to_path_buf()).expect("valid workspace project"),
    )
}

async fn budgeted_platform_list(server: McpServer) -> (Value, String) {
    let (client_io, server_io) = tokio::io::duplex(1024 * 1024);
    tokio::spawn(serve_stream(server, server_io));
    let client = ().serve(client_io).await.expect("session initialized");
    let result = client
        .call_tool(CallToolRequestParams::new("search").with_arguments(Map::from_iter([
            ("action".to_owned(), json!("list_platform")),
            ("max_output_tokens".to_owned(), json!(1_000)),
        ])))
        .await
        .expect("list_platform call");
    let body = result.structured_content.expect("list_platform structuredContent");
    let text = result.content[0].as_text().expect("list_platform text mirror").text.clone();
    client.cancel().await.expect("session closed");
    (body, text)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
async fn shared_platform_listing_is_identical_in_workspace_and_reference_profiles() {
    let ws = TempDir::new().unwrap();
    let workspace = budgeted_platform_list(workspace_server(&ws)).await;
    let reference = budgeted_platform_list(reference_server()).await;

    assert_eq!(workspace, reference);
    assert_eq!(workspace.0["action"], "list_platform");
    assert_eq!(workspace.0["schema_version"], "1");
    assert_eq!(workspace.0["budget_exhausted"], true);
    assert!(workspace.0["shown"].as_u64().unwrap() < workspace.0["total"].as_u64().unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workspace_tools_enforce_their_declared_required_params() {
    let ws = TempDir::new().unwrap();
    assert_required_params_are_enforced(workspace_server(&ws), "workspace").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workspace_tools_need_nothing_the_contract_omits() {
    let ws = TempDir::new().unwrap();
    assert_no_undeclared_requirements(workspace_server(&ws), "workspace").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reference_tools_need_nothing_the_contract_omits() {
    assert_no_undeclared_requirements(reference_server(), "reference").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reference_tools_enforce_their_declared_required_params() {
    assert_required_params_are_enforced(reference_server(), "reference").await;
}
