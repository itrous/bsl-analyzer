mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::Lsp;
use mcp_server::{serve_stream, McpProfile, McpServer, SharedState, WorkspaceCacheLayout};
use rmcp::model::{CallToolRequestParams, ClientRequest, PingRequest};
use rmcp::service::RunningService;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{json, Map, Value};
use tokio::task::JoinHandle;

type Client = RunningService<RoleClient, ()>;
type ServerTask = JoinHandle<anyhow::Result<()>>;

/// Bound of a wait for an event that a regression would never deliver; a hang watchdog, not a
/// latency measure.
const HANG_WATCHDOG: Duration = test_utils::wait::ceiling(Duration::from_secs(60));

const CONSUMER: &str = include_str!(
    "fixtures/shared_workspace_analysis_s6/extensions/consumer/CommonModules/Потребитель/Ext/Module.bsl"
);
const CONFIG_A: &str = include_str!("fixtures/shared_workspace_analysis_s6/bsl-analyzer.toml");
const CONFIG_B: &str = r#"[source]
root = "base"
extensions = [
  { name = "Поставщик", path = "extensions/provider-b", scope = "InfoBase" },
  { name = "Потребитель", path = "extensions/consumer", scope = "InfoBase", dependsOn = ["Поставщик"] },
]
"#;

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

async fn mcp_session(
    root: &Path,
    cache: WorkspaceCacheLayout,
) -> (Client, ServerTask, SharedState) {
    let state = SharedState::workspace_with_cache(root.to_path_buf(), cache)
        .expect("workspace scope boots from the current project config");
    let server = McpServer::new(McpProfile::Workspace, state.clone());
    let (client_io, server_io) = tokio::io::duplex(4 * 1024 * 1024);
    let task = tokio::spawn(serve_stream(server, server_io));
    let client = ().serve(client_io).await.expect("real rmcp handshake");
    (client, task, state)
}

async fn symbol_info(client: &Client, path: &Path, needle: &str, timeout: Duration) -> Value {
    let (line, column) = position(CONSUMER, needle);
    let call = tokio::time::timeout(
        timeout,
        client.call_tool(CallToolRequestParams::new("symbol_info").with_arguments(args(&[
            ("path", json!(path.to_string_lossy())),
            ("line", json!(line)),
            ("column", json!(column)),
        ]))),
    )
    .await
    .expect("MCP symbol_info deadline")
    .expect("MCP symbol_info transport");
    call.structured_content.expect("symbol_info structured result")
}

async fn wait_mcp_target(
    client: &Client,
    path: &Path,
    needle: &str,
    expected_type: &str,
    expected_target: &str,
) -> Value {
    let (expected_root_id, expected_path_suffix) = expected_target
        .split_once("/CommonModules/")
        .expect("expected target includes source root and module path");
    let expected_root_id = expected_root_id.split('/').collect::<PathBuf>();
    let expected_path = format!("CommonModules/{expected_path_suffix}");
    let deadline = tokio::time::Instant::now() + test_utils::wait::ceiling(Duration::from_secs(45));
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(!remaining.is_zero(), "MCP never resolved {needle}");
        let card = symbol_info(client, path, needle, remaining).await;
        match card["status"].as_str() {
            Some("loading") => {}
            Some("ok") => {
                assert_eq!(
                    card["return_type"], expected_type,
                    "semantic type for {needle}: {card}"
                );
                let definitions = card["definitions"].as_array().expect("definition list");
                assert!(
                    definitions.iter().any(|definition| {
                        definition["location"]["root_id"]
                            == expected_root_id.to_string_lossy().as_ref()
                            && definition["location"]["path"] == expected_path
                    }),
                    "active composition resolves {needle} into {expected_target}: {card}"
                );
                return card;
            }
            other => panic!("MCP returned {other:?} for active method {needle}: {card}"),
        }
        assert!(tokio::time::Instant::now() < deadline, "MCP never resolved {needle}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn assert_mcp_missing(client: &Client, path: &Path, needle: &str) {
    let card =
        symbol_info(client, path, needle, test_utils::wait::ceiling(Duration::from_secs(45))).await;
    let no_target =
        card["definitions"].is_null() || card["definitions"].as_array().is_some_and(Vec::is_empty);
    assert!(
        card["status"] == "not_found"
            || (card["status"] == "ok"
                && card["kind"] == "unresolved"
                && card["return_type"].is_null()),
        "inactive source-set method is not presented as resolved: {card}"
    );
    assert!(no_target, "inactive source-set method has no definition target: {card}");
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
    let (line, character) = position(CONSUMER, needle);
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

fn assert_lsp_missing(lsp: &mut Lsp, id: u64, uri: &lsp_types::Url, needle: &str) {
    let deadline = std::time::Instant::now() + test_utils::wait::ceiling(Duration::from_secs(45));
    let mut request_id = id;
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        assert!(!remaining.is_zero(), "LSP definition stayed busy for {needle}");
        let response = lsp_definition(lsp, request_id, uri, needle, remaining);
        if response["error"]["code"] == -32801 {
            assert!(
                std::time::Instant::now() < deadline,
                "LSP definition remained ContentModified for {needle}: {response}"
            );
            request_id += 1;
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }
        assert!(response["error"].is_null(), "LSP definition failed: {response}");
        assert!(
            response["result"].is_null()
                || response["result"].as_array().is_some_and(Vec::is_empty),
            "inactive source-set method has no definition: {response}"
        );
        return;
    }
}

fn assert_diagnostic(published: &Value) {
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
async fn source_set_change_retires_mcp_scope_and_reconnects_to_new_semantics() {
    eprintln!("S6 phase=source-set/setup");
    let root = fixture();
    assert_eq!(std::fs::read_to_string(root.path().join("bsl-analyzer.toml")).unwrap(), CONFIG_A);
    let consumer_relative =
        PathBuf::from("extensions/consumer/CommonModules/Потребитель/Ext/Module.bsl");
    let consumer_path = root.path().join(&consumer_relative);
    let config_path = root.path().join("bsl-analyzer.toml");
    let consumer_uri = lsp_types::Url::from_file_path(&consumer_path).expect("consumer URI");
    let provider_a_uri = lsp_types::Url::from_file_path(
        root.path().join("extensions/provider-a/CommonModules/Общий/Ext/Module.bsl"),
    )
    .expect("provider A URI");
    let provider_b_uri = lsp_types::Url::from_file_path(
        root.path().join("extensions/provider-b/CommonModules/Общий/Ext/Module.bsl"),
    )
    .expect("provider B URI");

    let cache_parent = tempfile::tempdir().expect("owned MCP cache parent");
    let old_project = mcp_server::project::at(root.path()).expect("initial source-set project");
    let old_cache = WorkspaceCacheLayout::for_project(
        &old_project,
        Some(cache_parent.path()),
        cache_parent.path(),
        None,
    )
    .expect("initial cache scope");
    let old_cache_root = old_cache.root().to_path_buf();
    let old_scope_stamp = old_cache.scope_stamp();
    let (old_client, old_task, old_state) = mcp_session(root.path(), old_cache).await;

    eprintln!("S6 phase=source-set/lsp-initialize");
    let mut lsp = Lsp::start(root.path());
    eprintln!("S6 phase=source-set/open-consumer");
    let initial_diagnostics = lsp.open(&consumer_path, CONSUMER);
    assert_diagnostic(&initial_diagnostics);
    let initial_mcp = wait_mcp_target(
        &old_client,
        &consumer_path,
        "ДатьСтроку",
        "Строка",
        "extensions/provider-a/CommonModules/Общий/Ext/Module.bsl",
    )
    .await;
    assert_eq!(initial_mcp["return_type"], "Строка");
    wait_lsp_target(&mut lsp, 2, &consumer_uri, "ДатьСтроку", &provider_a_uri);
    assert_lsp_missing(&mut lsp, 3, &consumer_uri, "ДатьЧисло");
    assert_mcp_missing(&old_client, &consumer_path, "ДатьЧисло").await;

    eprintln!("S6 phase=source-set/change-config");
    std::fs::write(&config_path, CONFIG_B).expect("switch the configured provider source root");
    send_config_change(&mut lsp, &config_path);
    let updated_diagnostics = lsp
        .wait_for_within(test_utils::wait::ceiling(Duration::from_secs(45)), |message| {
            message["method"] == "textDocument/publishDiagnostics"
                && message["params"]["uri"] == consumer_uri.as_str()
        })
        .expect("LSP republishes diagnostics after source-set reload");
    assert_diagnostic(&updated_diagnostics);
    wait_lsp_target(&mut lsp, 4, &consumer_uri, "ДатьЧисло", &provider_b_uri);
    assert_lsp_missing(&mut lsp, 5, &consumer_uri, "ДатьСтроку");

    eprintln!("S6 phase=source-set/retire-old-mcp");
    let new_project = mcp_server::project::at(root.path()).expect("updated source-set project");
    let new_cache = WorkspaceCacheLayout::for_project(
        &new_project,
        Some(cache_parent.path()),
        cache_parent.path(),
        None,
    )
    .expect("updated cache scope");
    assert_ne!(new_cache.root(), old_cache_root, "source-set identity selects a new cache leaf");
    assert_ne!(new_cache.scope_stamp(), old_scope_stamp, "source-set identity changes scope stamp");
    assert!(
        !new_cache.root().exists(),
        "the retired owner has not created or published into the new cache scope"
    );
    tokio::time::timeout(test_utils::wait::ceiling(Duration::from_secs(15)), old_task)
        .await
        .expect("source-set drift retires the old MCP transport")
        .expect("old MCP task joins")
        .expect("retired MCP session closes cleanly");
    assert!(
        old_client.send_request(ClientRequest::PingRequest(PingRequest::default())).await.is_err(),
        "retired MCP scope does not answer requests"
    );
    old_state.shutdown();

    let new_cache_root = new_cache.root().to_path_buf();
    let (new_client, new_task, new_state) = mcp_session(root.path(), new_cache).await;
    assert!(
        old_cache_root.exists() && new_cache_root != old_cache_root && new_cache_root.exists(),
        "reconnection serves a distinct validated scope"
    );
    let new_mcp = wait_mcp_target(
        &new_client,
        &consumer_path,
        "ДатьЧисло",
        "Число",
        "extensions/provider-b/CommonModules/Общий/Ext/Module.bsl",
    )
    .await;
    assert_eq!(new_mcp["return_type"], "Число");
    assert_mcp_missing(&new_client, &consumer_path, "ДатьСтроку").await;

    eprintln!("S6 phase=source-set/lsp-shutdown");
    new_client.cancel().await.expect("new MCP client disconnects");
    tokio::time::timeout(HANG_WATCHDOG, new_task)
        .await
        .expect("reconnected MCP session exits")
        .expect("new MCP task joins")
        .expect("new MCP session closes cleanly");
    new_state.shutdown();
    assert!(lsp.shutdown(6).success(), "LSP exits cleanly");
    eprintln!("S6 phase=source-set/complete");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn excluding_and_readmitting_a_provider_tombstones_and_restores_its_definition() {
    eprintln!("S6 phase=exclude/setup");
    let root = fixture();
    let consumer_path =
        root.path().join("extensions/consumer/CommonModules/Потребитель/Ext/Module.bsl");
    let consumer_uri = lsp_types::Url::from_file_path(&consumer_path).expect("consumer URI");
    let provider_uri = lsp_types::Url::from_file_path(
        root.path().join("extensions/provider-a/CommonModules/Общий/Ext/Module.bsl"),
    )
    .expect("provider URI");
    let config_path = root.path().join("bsl-analyzer.toml");
    eprintln!("S6 phase=exclude/lsp-initialize");
    let mut lsp = Lsp::start(root.path());

    eprintln!("S6 phase=exclude/open-consumer");
    let initial_diagnostics = lsp.open(&consumer_path, CONSUMER);
    assert_diagnostic(&initial_diagnostics);
    wait_lsp_target(&mut lsp, 2, &consumer_uri, "ДатьСтроку", &provider_uri);

    let excluded_config = CONFIG_A
        .replace("root = \"base\"", "root = \"base\"\nexclude = [\"extensions/provider-a\"]");
    assert_ne!(excluded_config, CONFIG_A, "the exclusion edit was applied");
    eprintln!("S6 phase=exclude/write-and-notify");
    std::fs::write(&config_path, excluded_config).expect("exclude provider source root");
    send_config_change(&mut lsp, &config_path);
    let excluded_diagnostics = lsp
        .wait_for_within(test_utils::wait::ceiling(Duration::from_secs(45)), |message| {
            message["method"] == "textDocument/publishDiagnostics"
                && message["params"]["uri"] == consumer_uri.as_str()
        })
        .expect("LSP republishes diagnostics after source exclusion");
    assert_diagnostic(&excluded_diagnostics);
    assert_lsp_missing(&mut lsp, 3, &consumer_uri, "ДатьСтроку");

    eprintln!("S6 phase=exclude/restore-config");
    std::fs::write(&config_path, CONFIG_A).expect("restore provider source root");
    send_config_change(&mut lsp, &config_path);
    let restored_diagnostics = lsp
        .wait_for_within(test_utils::wait::ceiling(Duration::from_secs(45)), |message| {
            message["method"] == "textDocument/publishDiagnostics"
                && message["params"]["uri"] == consumer_uri.as_str()
        })
        .expect("LSP republishes diagnostics after provider readmission");
    assert_diagnostic(&restored_diagnostics);
    wait_lsp_target(&mut lsp, 4, &consumer_uri, "ДатьСтроку", &provider_uri);

    eprintln!("S6 phase=exclude/lsp-shutdown");
    assert!(lsp.shutdown(6).success(), "LSP exits cleanly");
    eprintln!("S6 phase=exclude/complete");
}
