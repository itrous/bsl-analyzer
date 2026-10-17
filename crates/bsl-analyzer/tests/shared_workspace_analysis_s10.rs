mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::Lsp;
use mcp_server::{
    serve_stream, McpProfile, McpServer, SharedState, WorkspaceCacheLayout, WorkspaceInitError,
};
use rmcp::model::{CallToolRequestParams, ClientRequest, PingRequest};
use rmcp::service::RunningService;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{json, Map, Value};
use tokio::task::JoinHandle;

/// Bound of a wait for an event that a regression would never deliver; a hang watchdog, not a
/// latency measure.
const HANG_WATCHDOG: Duration = test_utils::wait::ceiling(Duration::from_secs(60));

const CONSUMER: &str = include_str!(
    "fixtures/shared_workspace_analysis_s6/extensions/consumer/CommonModules/Потребитель/Ext/Module.bsl"
);
const PROVIDER_A: &str = include_str!(
    "fixtures/shared_workspace_analysis_s6/extensions/provider-a/CommonModules/Общий/Ext/Module.bsl"
);
const CONFIG: &str = include_str!("fixtures/shared_workspace_analysis_s6/bsl-analyzer.toml");
const INVALID_CONFIG: &str = "[source\nroot = \"base\"\n";

type Client = RunningService<RoleClient, ()>;
type ServerTask = JoinHandle<anyhow::Result<()>>;

fn fixture() -> tempfile::TempDir {
    let root = tempfile::tempdir().expect("fixture workspace");
    let fixture_root =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/shared_workspace_analysis_s6");
    for entry in walkdir::WalkDir::new(&fixture_root) {
        let entry = entry.expect("fixture entry");
        let relative = entry.path().strip_prefix(&fixture_root).expect("fixture-relative path");
        let target = root.path().join(relative);
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&target).expect("fixture directory");
        } else {
            std::fs::create_dir_all(target.parent().expect("fixture parent"))
                .expect("fixture parent directory");
            std::fs::copy(entry.path(), target).expect("fixture file");
        }
    }
    root
}

fn args(pairs: &[(&str, Value)]) -> Map<String, Value> {
    pairs.iter().map(|(key, value)| ((*key).to_owned(), value.clone())).collect()
}

fn position(text: &str, needle: &str) -> (u32, u32) {
    let (line, source_line) = text
        .lines()
        .enumerate()
        .find(|(_, line)| line.contains(needle))
        .expect("position token exists");
    let byte_column = source_line.find(needle).expect("position token on line");
    (line as u32, source_line[..byte_column].encode_utf16().count() as u32)
}

fn non_config_files(root: &Path, config_path: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut files: Vec<_> = walkdir::WalkDir::new(root)
        .into_iter()
        .map(|entry| entry.expect("fixture inventory entry"))
        .filter(|entry| entry.file_type().is_file() && entry.path() != config_path)
        .map(|entry| {
            (
                entry.path().strip_prefix(root).unwrap().to_path_buf(),
                std::fs::read(entry.path()).expect("fixture inventory contents"),
            )
        })
        .collect();
    files.sort_by(|left, right| left.0.cmp(&right.0));
    files
}

async fn mcp_session(
    root: &Path,
    cache: WorkspaceCacheLayout,
) -> (Client, ServerTask, SharedState) {
    let state = SharedState::workspace_with_cache(root.to_path_buf(), cache)
        .expect("workspace scope boots from valid project config");
    let server = McpServer::new(McpProfile::Workspace, state.clone());
    let (client_io, server_io) = tokio::io::duplex(4 * 1024 * 1024);
    let task = tokio::spawn(serve_stream(server, server_io));
    let client = ().serve(client_io).await.expect("real rmcp handshake");
    (client, task, state)
}

async fn symbol_info(client: &Client, path: &Path, needle: &str) -> Value {
    let (line, column) = position(CONSUMER, needle);
    let deadline = tokio::time::Instant::now() + test_utils::wait::ceiling(Duration::from_secs(45));
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(!remaining.is_zero(), "MCP symbol_info never settled for {needle}");
        let result = tokio::time::timeout(
            remaining,
            client.call_tool(CallToolRequestParams::new("symbol_info").with_arguments(args(&[
                ("path", json!(path.to_string_lossy())),
                ("line", json!(line)),
                ("column", json!(column)),
            ]))),
        )
        .await
        .expect("MCP symbol_info deadline")
        .expect("MCP symbol_info transport");
        let card = result.structured_content.expect("symbol_info structured result");
        if card["status"] != "loading" {
            return card;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn rooted_symbol_info(
    client: &Client,
    root_id: &str,
    relative_path: &Path,
    source_text: &str,
    needle: &str,
) -> Value {
    let (line, column) = position(source_text, needle);
    let deadline = tokio::time::Instant::now() + test_utils::wait::ceiling(Duration::from_secs(45));
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(!remaining.is_zero(), "rooted MCP symbol_info never settled for {needle}");
        let result = tokio::time::timeout(
            remaining,
            client.call_tool(CallToolRequestParams::new("symbol_info").with_arguments(args(&[
                ("root_id", json!(root_id)),
                ("path", json!(relative_path.to_string_lossy())),
                ("line", json!(line)),
                ("column", json!(column)),
            ]))),
        )
        .await
        .expect("rooted MCP symbol_info deadline")
        .expect("rooted MCP symbol_info transport");
        let card = result.structured_content.expect("rooted symbol_info structured result");
        if card["status"] != "loading" {
            return card;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn wait_mcp_target(client: &Client, source_path: &Path) -> Value {
    let provider_a_root_id = Path::new("extensions").join("provider-a");
    let card = symbol_info(client, source_path, "ДатьСтроку").await;
    assert_eq!(card["status"], "ok", "provider A symbol resolves: {card}");
    assert_eq!(card["return_type"], "Строка", "provider A semantic type: {card}");
    let definitions = card["definitions"].as_array().expect("definition list");
    assert!(
        definitions.iter().any(|definition| {
            definition["location"]["root_id"] == provider_a_root_id.to_string_lossy().as_ref()
                && definition["location"]["path"] == "CommonModules/Общий/Ext/Module.bsl"
        }),
        "provider A resolves through the expected source root and path: {card}"
    );
    card
}

async fn assert_mcp_missing(client: &Client, source_path: &Path) {
    let card = symbol_info(client, source_path, "ДатьЧисло").await;
    let no_target =
        card["definitions"].is_null() || card["definitions"].as_array().is_some_and(Vec::is_empty);
    assert!(
        card["status"] == "not_found"
            || (card["status"] == "ok"
                && card["kind"] == "unresolved"
                && card["return_type"].is_null()),
        "provider B is not active in this project: {card}"
    );
    assert!(no_target, "excluded provider B has no definition target: {card}");
}

async fn mcp_diagnostic_body(client: &Client, source_path: &Path) -> Value {
    mcp_rooted_diagnostic_body(client, None, source_path).await
}

async fn mcp_rooted_diagnostic_body(
    client: &Client,
    root_id: Option<&str>,
    source_path: &Path,
) -> Value {
    let deadline = tokio::time::Instant::now() + test_utils::wait::ceiling(Duration::from_secs(45));
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(!remaining.is_zero(), "MCP diagnostics never settled");
        let mut arguments = args(&[
            ("action", json!("file")),
            ("path", json!(source_path.to_string_lossy())),
            ("min_severity", json!("hint")),
            ("detail", json!("detailed")),
        ]);
        if let Some(root_id) = root_id {
            arguments.insert("root_id".to_owned(), json!(root_id));
        }
        let result = tokio::time::timeout(
            remaining,
            client.call_tool(CallToolRequestParams::new("diagnostics").with_arguments(arguments)),
        )
        .await
        .expect("MCP diagnostics deadline")
        .expect("MCP diagnostics transport");
        assert!(
            result.is_error != Some(true),
            "MCP diagnostics returned an error: {:?}",
            result.structured_content
        );
        let body = result.structured_content.expect("diagnostics structured result");
        if body["status"] != "loading" {
            return body;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn mcp_diagnostic(client: &Client, source_path: &Path) -> Value {
    let body = mcp_diagnostic_body(client, source_path).await;
    assert!(body["result"].is_object(), "MCP diagnostics response has no result object: {body}");
    body["result"].clone()
}

fn assert_mcp_diagnostic(body: &Value) {
    let findings = body["findings"]
        .as_array()
        .unwrap_or_else(|| panic!("diagnostics result has no findings array: {body}"));
    let (line, character) = position(CONSUMER, "НетТакойПроцедуры");
    let finding = findings
        .iter()
        .find(|finding| finding["code"] == "UnresolvedBareCall")
        .expect("fixture has a known unresolved-call diagnostic");
    assert_eq!(finding["severity"], "error", "diagnostic severity: {finding}");
    assert_eq!(finding["location"]["range"]["start_line"], line);
    assert_eq!(finding["location"]["range"]["start_character"], character);
}

fn send_config_change(lsp: &mut Lsp, config_path: &Path) {
    lsp.send(json!({
        "jsonrpc": "2.0", "method": "workspace/didChangeWatchedFiles",
        "params": {"changes": [{
            "uri": lsp_types::Url::from_file_path(config_path).expect("config file URI"),
            "type": 2
        }]}
    }));
}

fn lsp_definition(
    lsp: &mut Lsp,
    id: u64,
    uri: &lsp_types::Url,
    needle: &str,
    timeout: Duration,
) -> Value {
    lsp_definition_at(lsp, id, uri, CONSUMER, needle, timeout)
}

fn lsp_definition_at(
    lsp: &mut Lsp,
    id: u64,
    uri: &lsp_types::Url,
    text: &str,
    needle: &str,
    timeout: Duration,
) -> Value {
    let (line, character) = position(text, needle);
    lsp.send(json!({
        "jsonrpc": "2.0", "id": id, "method": "textDocument/definition",
        "params": {
            "textDocument": {"uri": uri},
            "position": {"line": line, "character": character}
        }
    }));
    lsp.wait_for_within(timeout, |message| {
        message["id"] == id
            && message["method"].is_null()
            && (message.get("result").is_some() || message.get("error").is_some())
    })
    .unwrap_or_else(|| panic!("LSP definition {needle} exceeded {timeout:?}"))
}

fn lsp_hover(lsp: &mut Lsp, id: u64, uri: &lsp_types::Url, text: &str, needle: &str) -> Value {
    let (line, character) = position(text, needle);
    lsp.request(
        id,
        "textDocument/hover",
        json!({
            "textDocument": {"uri": uri},
            "position": {"line": line, "character": character}
        }),
    )
}

fn wait_lsp_target(
    lsp: &mut Lsp,
    first_id: u64,
    uri: &lsp_types::Url,
    needle: &str,
    target_uri: &lsp_types::Url,
) {
    let deadline = std::time::Instant::now() + test_utils::wait::ceiling(Duration::from_secs(45));
    let mut id = first_id;
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        assert!(!remaining.is_zero(), "LSP never resolved {needle}");
        let response = lsp_definition(lsp, id, uri, needle, remaining);
        if response["error"]["code"] == -32801 {
            assert!(
                std::time::Instant::now() < deadline,
                "LSP definition remained ContentModified for {needle}: {response}"
            );
            id += 1;
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }
        assert!(response["error"].is_null(), "LSP definition failed: {response}");
        if response["result"].to_string().contains(target_uri.as_str()) {
            return;
        }
        assert!(
            response["result"].is_null()
                || response["result"].as_array().is_some_and(Vec::is_empty),
            "definition resolved to an unexpected source-set target: {response}"
        );
        assert!(std::time::Instant::now() < deadline, "LSP never resolved {needle}");
        id += 1;
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn assert_lsp_missing(lsp: &mut Lsp, id: u64, uri: &lsp_types::Url) {
    let response = lsp_definition(
        lsp,
        id,
        uri,
        "ДатьЧисло",
        test_utils::wait::ceiling(Duration::from_secs(45)),
    );
    assert!(response["error"].is_null(), "LSP negative control failed: {response}");
    assert!(
        response["result"].is_null() || response["result"].as_array().is_some_and(Vec::is_empty),
        "unconfigured provider B has no definition target: {response}"
    );
}

fn assert_lsp_diagnostic(published: &Value) {
    let diagnostics = published["params"]["diagnostics"].as_array().expect("diagnostic list");
    let (line, character) = position(CONSUMER, "НетТакойПроцедуры");
    let unresolved = diagnostics
        .iter()
        .find(|diagnostic| diagnostic["code"] == "UnresolvedBareCall")
        .expect("fixture retains a known unresolved-call diagnostic");
    assert_eq!(unresolved["severity"], 1, "known diagnostic severity: {unresolved}");
    assert_eq!(unresolved["range"]["start"]["line"], line);
    assert_eq!(unresolved["range"]["start"]["character"], character);
    assert_eq!(
        unresolved["message"],
        "Процедура или функция с указанным именем не определена: 'НетТакойПроцедуры'"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn invalid_project_config_preserves_lsp_and_rejects_mcp_until_repair() {
    let root = fixture();
    let config_path = root.path().join("bsl-analyzer.toml");
    assert_eq!(std::fs::read_to_string(&config_path).unwrap(), CONFIG);

    let consumer_relative =
        PathBuf::from("extensions/consumer/CommonModules/Потребитель/Ext/Module.bsl");
    let consumer_path = root.path().join(&consumer_relative);
    let consumer_uri = lsp_types::Url::from_file_path(&consumer_path).expect("consumer URI");
    let provider_a_uri = lsp_types::Url::from_file_path(
        root.path().join("extensions/provider-a/CommonModules/Общий/Ext/Module.bsl"),
    )
    .expect("provider A URI");
    let source_files_before = non_config_files(root.path(), &config_path);

    let cache_parent = tempfile::tempdir().expect("owned MCP cache parent");
    let initial_project = mcp_server::project::at(root.path()).expect("initial project is valid");
    let cache = WorkspaceCacheLayout::for_project(
        &initial_project,
        Some(cache_parent.path()),
        root.path(),
        None,
    )
    .expect("initial explicit cache scope");
    let cache_root = cache.root().to_path_buf();
    let cache_scope = cache.scope_stamp();
    let (old_client, old_task, old_state) = mcp_session(root.path(), cache.clone()).await;

    let mut lsp = Lsp::start(root.path());
    let initial_diagnostics = lsp.open(&consumer_path, CONSUMER);
    assert_lsp_diagnostic(&initial_diagnostics);
    wait_lsp_target(&mut lsp, 2, &consumer_uri, "ДатьСтроку", &provider_a_uri);
    assert_lsp_missing(&mut lsp, 3, &consumer_uri);
    let initial_symbol = wait_mcp_target(&old_client, &consumer_path).await;
    assert_eq!(initial_symbol["return_type"], "Строка");
    assert_mcp_missing(&old_client, &consumer_path).await;
    assert_mcp_diagnostic(&mcp_diagnostic(&old_client, &consumer_path).await);

    std::fs::write(&config_path, INVALID_CONFIG).expect("write only invalid TOML config");
    send_config_change(&mut lsp, &config_path);
    let error = lsp
        .wait_for_within(test_utils::wait::ceiling(Duration::from_secs(45)), |message| {
            message["method"] == "window/showMessage"
                && message["params"]["message"]
                    .as_str()
                    .is_some_and(|text| text.contains("project config reload failed"))
        })
        .expect("LSP reports invalid project config reload");
    assert_eq!(error["params"]["type"], 1, "invalid config is an LSP error: {error}");
    assert!(
        error["params"]["message"].as_str().unwrap().contains("bsl-analyzer.toml"),
        "LSP identifies the invalid project input: {error}"
    );
    wait_lsp_target(&mut lsp, 4, &consumer_uri, "ДатьСтроку", &provider_a_uri);
    assert_lsp_missing(&mut lsp, 5, &consumer_uri);

    let rejected_cache_parent = tempfile::tempdir().expect("owned rejected-start cache parent");
    let rejected_cache = WorkspaceCacheLayout::for_project(
        &initial_project,
        Some(rejected_cache_parent.path()),
        root.path(),
        None,
    )
    .expect("candidate cache path from last valid project");
    assert!(!rejected_cache.root().exists(), "rejected-start cache starts absent");
    let invalid_start =
        SharedState::workspace_with_cache(root.path().to_path_buf(), rejected_cache.clone());
    assert!(
        matches!(invalid_start, Err(WorkspaceInitError::Project(_))),
        "new MCP workspace construction rejects invalid project config before storage"
    );
    assert!(
        !rejected_cache.root().exists(),
        "invalid MCP construction creates no cache storage or fallback workspace"
    );

    tokio::time::timeout(test_utils::wait::ceiling(Duration::from_secs(15)), old_task)
        .await
        .expect("invalid project input retires the active MCP scope")
        .expect("retired MCP task joins")
        .expect("retired MCP stream closes cleanly");
    assert!(
        old_client.send_request(ClientRequest::PingRequest(PingRequest::default())).await.is_err(),
        "the old MCP transport no longer serves a retired project scope"
    );
    old_state.shutdown();

    std::fs::write(&config_path, CONFIG).expect("restore exact known-good TOML config");
    send_config_change(&mut lsp, &config_path);
    let restored_diagnostics = lsp
        .wait_for_within(test_utils::wait::ceiling(Duration::from_secs(45)), |message| {
            message["method"] == "textDocument/publishDiagnostics"
                && message["params"]["uri"] == consumer_uri.as_str()
        })
        .expect("LSP republishes diagnostics after project repair");
    assert_lsp_diagnostic(&restored_diagnostics);
    wait_lsp_target(&mut lsp, 6, &consumer_uri, "ДатьСтроку", &provider_a_uri);
    assert_lsp_missing(&mut lsp, 7, &consumer_uri);

    let restored_project = mcp_server::project::at(root.path()).expect("restored project parses");
    let restored_cache = WorkspaceCacheLayout::for_project(
        &restored_project,
        Some(cache_parent.path()),
        root.path(),
        None,
    )
    .expect("restored original cache scope");
    assert_eq!(restored_cache.root(), cache_root, "repair reconnects to the original cache leaf");
    assert_eq!(
        restored_cache.scope_stamp(),
        cache_scope,
        "repair restores the original scope identity"
    );
    let (new_client, new_task, new_state) = mcp_session(root.path(), restored_cache).await;
    assert!(cache_root.exists(), "the restored valid scope owns its original cache");
    let restored_symbol = wait_mcp_target(&new_client, &consumer_path).await;
    assert_eq!(restored_symbol["return_type"], "Строка");
    assert_mcp_missing(&new_client, &consumer_path).await;
    assert_mcp_diagnostic(&mcp_diagnostic(&new_client, &consumer_path).await);

    new_client.cancel().await.expect("restored MCP client disconnects");
    tokio::time::timeout(HANG_WATCHDOG, new_task)
        .await
        .expect("restored MCP session exits")
        .expect("restored MCP task joins")
        .expect("restored MCP stream closes cleanly");
    new_state.shutdown();
    assert!(lsp.shutdown(8).success(), "LSP exits cleanly");
    assert_eq!(std::fs::read_to_string(&config_path).unwrap(), CONFIG);
    let source_files_after = non_config_files(root.path(), &config_path);
    assert_eq!(source_files_after, source_files_before, "only TOML changed during this scenario");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unknown_and_excluded_rooted_paths_do_not_collide() {
    let root = fixture();
    let config_path = root.path().join("bsl-analyzer.toml");
    let excluded_relative = Path::new("CommonModules/Общий/Ext/Module.bsl");
    let excluded_path = root.path().join("base").join(excluded_relative);
    std::fs::create_dir_all(excluded_path.parent().expect("excluded module parent"))
        .expect("create excluded source directory");
    std::fs::write(
        &excluded_path,
        "&НаСервере\nФункция ДатьСтроку() Экспорт\n    Возврат \"excluded base\";\nКонецФункции\n",
    )
    .expect("create an excluded base copy with the provider's relative path");
    let excluded_config = CONFIG.replace(
        "[source]\nroot = \"base\"\n",
        "[source]\nroot = \"base\"\nexclude = [\"base/CommonModules/Общий\"]\n",
    );
    assert_ne!(excluded_config, CONFIG, "the exclusion was added to the project config");
    std::fs::write(&config_path, &excluded_config).expect("write explicit source exclusion");

    let consumer_relative =
        PathBuf::from("extensions/consumer/CommonModules/Потребитель/Ext/Module.bsl");
    let consumer_path = root.path().join(&consumer_relative);
    let consumer_uri = lsp_types::Url::from_file_path(&consumer_path).expect("consumer URI");
    let provider_a_uri = lsp_types::Url::from_file_path(
        root.path().join("extensions/provider-a/CommonModules/Общий/Ext/Module.bsl"),
    )
    .expect("provider A URI");
    let mut lsp = Lsp::start(root.path());
    let consumer_diagnostics = lsp.open(&consumer_path, CONSUMER);
    assert_lsp_diagnostic(&consumer_diagnostics);
    wait_lsp_target(&mut lsp, 2, &consumer_uri, "ДатьСтроку", &provider_a_uri);
    let consumer_hover = lsp_hover(&mut lsp, 3, &consumer_uri, CONSUMER, "ОтветСтроки");
    assert!(consumer_hover["error"].is_null(), "consumer hover failed: {consumer_hover}");
    assert!(
        consumer_hover["result"].to_string().contains("Строка"),
        "consumer sees provider A's String result: {consumer_hover}"
    );

    let excluded_text = std::fs::read_to_string(&excluded_path).expect("excluded source text");
    let excluded_uri = lsp_types::Url::from_file_path(&excluded_path).expect("excluded URI");
    lsp.send(json!({
        "jsonrpc": "2.0", "method": "textDocument/didOpen",
        "params": {"textDocument": {
            "uri": excluded_uri,
            "languageId": "bsl",
            "version": 1,
            "text": excluded_text
        }}
    }));
    let excluded_definition = lsp_definition_at(
        &mut lsp,
        4,
        &excluded_uri,
        &excluded_text,
        "ДатьСтроку",
        test_utils::wait::ceiling(Duration::from_secs(45)),
    );
    assert!(
        excluded_definition["error"].is_object() && excluded_definition["result"].is_null(),
        "excluded root-relative duplicate must be rejected before resolving provider A: {excluded_definition}"
    );
    assert!(
        excluded_definition["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("File not in frozen VFS snapshot")),
        "excluded didOpen is retained only in the editor buffer, not the analyzed VFS: {excluded_definition}"
    );
    assert_eq!(
        excluded_definition["error"]["code"],
        json!(lsp_server::ErrorCode::InternalError as i32),
        "frontend admission errors retain the existing JSON-RPC mapping: {excluded_definition}"
    );

    let unknown_path = root.path().join("not-present/CommonModules/Неизвестный/Ext/Module.bsl");
    let unknown_uri = lsp_types::Url::from_file_path(&unknown_path).expect("unknown URI");
    let unknown_definition = lsp_definition_at(
        &mut lsp,
        5,
        &unknown_uri,
        &excluded_text,
        "ДатьСтроку",
        test_utils::wait::ceiling(Duration::from_secs(45)),
    );
    assert!(
        unknown_definition["error"].is_object() && unknown_definition["result"].is_null(),
        "unknown path cannot resolve through another root with the same relative path: {unknown_definition}"
    );
    assert!(
        unknown_definition["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("File not in frozen VFS snapshot")),
        "a document without didOpen is absent from the request's frozen VFS paths: {unknown_definition}"
    );
    assert_eq!(
        unknown_definition["error"]["code"],
        json!(lsp_server::ErrorCode::InternalError as i32),
        "frontend admission errors retain the existing JSON-RPC mapping: {unknown_definition}"
    );

    wait_lsp_target(&mut lsp, 6, &consumer_uri, "ДатьСтроку", &provider_a_uri);
    let consumer_hover_after_rejections =
        lsp_hover(&mut lsp, 7, &consumer_uri, CONSUMER, "ОтветСтроки");
    assert!(
        consumer_hover_after_rejections["error"].is_null(),
        "consumer hover remains available after the rejected requests: {consumer_hover_after_rejections}"
    );
    assert!(
        consumer_hover_after_rejections["result"].to_string().contains("Строка"),
        "active provider-A type remains available after the rejected requests: {consumer_hover_after_rejections}"
    );

    let project = mcp_server::project::at(root.path()).expect("excluded fixture project parses");
    let cache_parent = tempfile::tempdir().expect("owned MCP cache parent");
    let cache =
        WorkspaceCacheLayout::for_project(&project, Some(cache_parent.path()), root.path(), None)
            .expect("explicit cache scope for excluded fixture");
    let (client, task, state) = mcp_session(root.path(), cache).await;

    let provider_a_relative = Path::new("CommonModules/Общий/Ext/Module.bsl");
    let provider_a_root_id = Path::new("extensions").join("provider-a");
    let active_provider = rooted_symbol_info(
        &client,
        provider_a_root_id.to_str().expect("UTF-8 fixture root id"),
        provider_a_relative,
        PROVIDER_A,
        "ДатьСтроку",
    )
    .await;
    assert_eq!(active_provider["status"], "ok", "the selected extension root remains active");
    assert_eq!(active_provider["return_type"], "Строка");
    assert!(
        active_provider["definitions"].as_array().is_some_and(|definitions| {
            definitions.iter().any(|definition| {
                definition["location"]["root_id"] == provider_a_root_id.to_string_lossy().as_ref()
                    && definition["location"]["path"] == provider_a_relative.to_string_lossy().as_ref()
            })
        }),
        "the same root-relative path resolves to provider A, not the excluded base copy: {active_provider}"
    );

    let excluded = mcp_rooted_diagnostic_body(&client, Some(""), excluded_relative).await;
    let excluded_result = excluded["result"]
        .as_object()
        .expect("excluded-path diagnostics envelope has a result object");
    assert_eq!(
        excluded_result["error"],
        "not_in_workspace",
        "configuration-root path is excluded even though provider A has the same relative path: {excluded}"
    );

    let unknown_path = root.path().join("not-present/CommonModules/Неизвестный/Ext/Module.bsl");
    let unknown = mcp_diagnostic_body(&client, &unknown_path).await;
    let unknown_result = unknown["result"]
        .as_object()
        .expect("unknown-path diagnostics envelope has a result object");
    assert_eq!(unknown_result["error"], "not_in_workspace", "unknown file path: {unknown}");

    client.cancel().await.expect("MCP client disconnects");
    tokio::time::timeout(HANG_WATCHDOG, task)
        .await
        .expect("MCP task exits")
        .expect("MCP task joins")
        .expect("MCP stream closes cleanly");
    state.shutdown();
    assert!(lsp.shutdown(8).success(), "LSP exits cleanly");
}
