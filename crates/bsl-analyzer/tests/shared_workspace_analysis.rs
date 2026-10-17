mod common;
#[path = "shared_workspace_analysis/mcp_metrics.rs"]
mod mcp_metrics;

use std::path::Path;
use std::time::Duration;

use common::Lsp;
use mcp_server::{serve_stream, McpProfile, McpServer, SharedState};
use rmcp::model::CallToolRequestParams;
use rmcp::service::RunningService;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{json, Map, Value};

type Client = RunningService<RoleClient, ()>;

/// Bound of a wait for a state that a regression would never reach; a hang watchdog, not a
/// latency measure.
const HANG_WATCHDOG: Duration = test_utils::wait::ceiling(Duration::from_secs(60));

const CALLER: &str =
    include_str!("fixtures/shared_workspace_analysis/CommonModules/Потребитель/Ext/Module.bsl");
const STRING_TARGET: &str =
    "Функция ЧисловоеЗначение() Экспорт\n    Возврат \"buffer-A\";\nКонецФункции\n";

fn fixture() -> tempfile::TempDir {
    let root = tempfile::tempdir().expect("fixture workspace");
    let fixture_root =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/shared_workspace_analysis");
    for entry in walkdir::WalkDir::new(&fixture_root)
        .into_iter()
        .filter_entry(|entry| entry.path() != fixture_root.join("perf-shadow"))
    {
        let entry = entry.expect("fixture entry");
        let relative = entry.path().strip_prefix(&fixture_root).expect("fixture-relative path");
        let target = root.path().join(relative);
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&target).expect("fixture directory");
        } else {
            std::fs::create_dir_all(target.parent().expect("fixture parent"))
                .expect("fixture directory");
            std::fs::copy(entry.path(), target).expect("fixture file");
        }
    }
    root
}

async fn mcp_client(root: &Path) -> Client {
    let server = McpServer::new(
        McpProfile::Workspace,
        SharedState::workspace(root.to_path_buf()).expect("valid fixture workspace"),
    );
    let (client_io, server_io) = tokio::io::duplex(4 * 1024 * 1024);
    tokio::spawn(serve_stream(server, server_io));
    ().serve(client_io).await.expect("real rmcp handshake")
}

async fn poll_tool(client: &Client, tool: &'static str, args: Map<String, Value>) -> Value {
    let deadline =
        tokio::time::Instant::now() + test_utils::wait::ceiling(Duration::from_secs(120));
    loop {
        let result = client
            .call_tool(CallToolRequestParams::new(tool).with_arguments(args.clone()))
            .await
            .expect("MCP transport call");
        if let Some(value) = result.structured_content {
            if value["status"] != "loading" {
                return value;
            }
        }
        assert!(tokio::time::Instant::now() < deadline, "{tool} stayed loading");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn wait_symbol_status(client: &Client, expected: &str) -> Value {
    let deadline = tokio::time::Instant::now() + test_utils::wait::ceiling(Duration::from_secs(30));
    loop {
        let card = poll_tool(
            client,
            "symbol_info",
            args(&[("symbol", json!("ОбщийМодуль.ЧисловоеЗначение"))]),
        )
        .await;
        if card["status"] == expected {
            return card;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "expected symbol status {expected}: {card}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn wait_symbol_return_type(client: &Client, expected: &str, stale: Option<&str>) -> Value {
    let deadline = tokio::time::Instant::now() + test_utils::wait::ceiling(Duration::from_secs(30));
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(!remaining.is_zero(), "symbol_info never returned type {expected}");
        let result = tokio::time::timeout(
            remaining,
            client.call_tool(
                CallToolRequestParams::new("symbol_info")
                    .with_arguments(args(&[("symbol", json!("ОбщийМодуль.ЧисловоеЗначение"))])),
            ),
        )
        .await
        .expect("symbol_info request deadline")
        .expect("MCP transport call");
        let card = result.structured_content.expect("symbol_info structured result");
        match card["status"].as_str() {
            Some("loading") => {}
            Some("ok") => {
                let actual = card["return_type"].as_str().expect("symbol_info return_type string");
                if actual == expected {
                    return card;
                }
                assert_eq!(
                    Some(actual),
                    stale,
                    "unexpected symbol_info type while waiting for {expected}: {card}"
                );
            }
            _ => panic!("symbol_info did not succeed while waiting for {expected}: {card}"),
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "symbol_info never returned type {expected}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn wait_hover_type(
    lsp: &mut Lsp,
    first_id: u64,
    params: Value,
    expected: &str,
    stale: Option<&str>,
) -> Value {
    let deadline = std::time::Instant::now() + test_utils::wait::ceiling(Duration::from_secs(30));
    let mut id = first_id;
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        assert!(!remaining.is_zero(), "LSP hover never returned type {expected}");
        lsp.send(
            json!({"jsonrpc": "2.0", "id": id, "method": "textDocument/hover", "params": params}),
        );
        let response = lsp
            .wait_for_within(remaining, |message| {
                message["id"] == id
                    && message["method"].is_null()
                    && (message.get("result").is_some() || message.get("error").is_some())
            })
            .unwrap_or_else(|| panic!("LSP hover request {id} exceeded its deadline"));
        assert!(response["error"].is_null(), "LSP hover failed: {response}");
        let result = response["result"].to_string();
        if result.contains(expected) {
            return response;
        }
        let stale_type = stale.expect("unexpected LSP hover type");
        assert!(
            result.contains(stale_type),
            "unexpected LSP hover result while waiting for {expected}: {response}"
        );
        assert!(std::time::Instant::now() < deadline, "LSP hover never returned type {expected}");
        id += 1;
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn args(pairs: &[(&str, Value)]) -> Map<String, Value> {
    pairs.iter().map(|(key, value)| ((*key).to_owned(), value.clone())).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lsp_and_mcp_return_the_same_nonempty_diagnostic_and_semantic_target() {
    let root = fixture();
    let caller_path = root.path().join("CommonModules/Потребитель/Ext/Module.bsl");
    let target_path = root.path().join("CommonModules/ОбщийМодуль/Ext/Module.bsl");

    let mut lsp = Lsp::start(root.path());
    let published = lsp.open(&caller_path, CALLER);
    let diagnostics = published["params"]["diagnostics"].as_array().expect("diagnostic list");
    let unresolved = diagnostics
        .iter()
        .find(|diagnostic| diagnostic["code"] == "UnresolvedBareCall")
        .expect("fixture has a known unresolved call");
    let diagnostic_prefix = "    Значение = \"🧪\"; ";
    let utf16_column = diagnostic_prefix.encode_utf16().count() as u64;
    let codepoint_column = diagnostic_prefix.chars().count() as u64;
    assert_ne!(utf16_column, codepoint_column, "fixture distinguishes UTF-16 columns");
    assert_eq!(unresolved["severity"], 1, "known diagnostic is an LSP error: {unresolved}");
    assert_eq!(unresolved["range"]["start"]["line"], 4);
    assert_eq!(unresolved["range"]["start"]["character"], utf16_column);
    assert_eq!(
        unresolved["message"],
        "Процедура или функция с указанным именем не определена: 'НетТакойПроцедуры'"
    );

    let caller_uri = lsp_types::Url::from_file_path(&caller_path).expect("caller file URI");
    let definition = lsp.request(
        2,
        "textDocument/definition",
        json!({
            "textDocument": {"uri": caller_uri.clone()},
            "position": {"line": 2, "character": 27}
        }),
    );
    assert!(definition["error"].is_null(), "definition request: {definition}");
    let location = if definition["result"].is_array() {
        &definition["result"][0]
    } else {
        &definition["result"]
    };
    assert_eq!(location["uri"], lsp_types::Url::from_file_path(&target_path).unwrap().as_str());
    assert_eq!(location["range"]["start"]["line"], 0);

    let hover = lsp.request(
        3,
        "textDocument/hover",
        json!({
            "textDocument": {"uri": caller_uri.clone()},
            "position": {"line": 2, "character": 8}
        }),
    );
    assert!(hover["error"].is_null(), "hover request: {hover}");
    assert!(hover["result"].to_string().contains("Число"), "expected inferred Число: {hover}");

    let client = mcp_client(root.path()).await;
    let diagnostic = poll_tool(
        &client,
        "diagnostics",
        args(&[
            ("action", json!("file")),
            ("path", json!("CommonModules/Потребитель/Ext/Module.bsl")),
            ("min_severity", json!("hint")),
            ("detail", json!("detailed")),
        ]),
    )
    .await;
    let findings = diagnostic["result"]["findings"].as_array().expect("MCP findings");
    let mcp_unresolved = findings
        .iter()
        .find(|finding| finding["code"] == "UnresolvedBareCall")
        .expect("MCP carries the known diagnostic");
    assert_eq!(mcp_unresolved["internal_severity"], "Major");
    assert_eq!(mcp_unresolved["range"]["start_line"], 4);
    assert_eq!(mcp_unresolved["range"]["start_column"], codepoint_column);
    let mcp_location = &mcp_unresolved["location"];
    assert_eq!(mcp_location["position_encoding"], "utf-16");
    assert_eq!(mcp_location["range"]["start_line"], 4);
    assert_eq!(mcp_location["range"]["start_character"], utf16_column);
    assert_eq!(
        unresolved["range"]["start"]["character"], mcp_location["range"]["start_character"],
        "LSP and MCP location contract normalize to the same UTF-16 position"
    );

    let symbol = poll_tool(
        &client,
        "symbol_info",
        args(&[("symbol", json!("ОбщийМодуль.ЧисловоеЗначение"))]),
    )
    .await;
    assert_eq!(symbol["status"], "ok", "symbol_info result: {symbol}");
    assert!(symbol["return_type"] == "Число", "expected exact Число return_type: {symbol}");
    let definition = symbol["definitions"].as_array().unwrap().first().unwrap();
    assert!(
        definition["location"]["path"].as_str().unwrap().ends_with("ОбщийМодуль/Ext/Module.bsl"),
        "MCP definition target: {symbol}"
    );

    // LSP owns the unsaved editor view; MCP keeps the workspace's saved source view.
    lsp.open(&target_path, STRING_TARGET);
    let buffer_hover = lsp.request(
        4,
        "textDocument/hover",
        json!({
            "textDocument": {"uri": caller_uri},
            "position": {"line": 2, "character": 8}
        }),
    );
    assert!(
        buffer_hover["result"].to_string().contains("Строка"),
        "LSP reads buffer A's inferred string type: {buffer_hover}"
    );

    std::fs::write(
        &target_path,
        "Функция ЧисловоеЗначение() Экспорт\n    Возврат 84;\nКонецФункции\n",
    )
    .expect("write saved disk view B");
    lsp.send(json!({
        "jsonrpc": "2.0", "method": "workspace/didChangeWatchedFiles",
        "params": {"changes": [{
            "uri": lsp_types::Url::from_file_path(&target_path).unwrap(), "type": 2
        }]}
    }));
    let after_disk_event = lsp.request(
        5,
        "textDocument/hover",
        json!({
            "textDocument": {"uri": lsp_types::Url::from_file_path(&caller_path).unwrap()},
            "position": {"line": 2, "character": 8}
        }),
    );
    assert!(
        after_disk_event["result"].to_string().contains("Строка"),
        "disk B must not replace open buffer A: {after_disk_event}"
    );
    let disk_card = wait_symbol_return_type(&client, "Число", None).await;
    assert!(
        disk_card["return_type"] == "Число",
        "MCP reads the saved numeric disk view B: {disk_card}"
    );

    client.cancel().await.ok();
    assert!(lsp.shutdown(6).success(), "LSP process exits cleanly");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unsaved_lsp_overlay_remains_distinct_from_mcp_disk_until_save_and_close() {
    let root = fixture();
    let caller_path = root.path().join("CommonModules/Потребитель/Ext/Module.bsl");
    let target_path = root.path().join("CommonModules/ОбщийМодуль/Ext/Module.bsl");
    let caller_uri = lsp_types::Url::from_file_path(&caller_path).expect("caller URI");
    let target_uri = lsp_types::Url::from_file_path(&target_path).expect("target URI");
    let original_target =
        include_str!("fixtures/shared_workspace_analysis/CommonModules/ОбщийМодуль/Ext/Module.bsl");

    let mut lsp = Lsp::start(root.path());
    let published = lsp.open(&caller_path, CALLER);
    let diagnostics = published["params"]["diagnostics"].as_array().expect("diagnostics");
    let unresolved = diagnostics
        .iter()
        .find(|diagnostic| diagnostic["code"] == "UnresolvedBareCall")
        .expect("fixture's positive diagnostic control");
    let diagnostic_prefix = "    Значение = \"🧪\"; ";
    let utf16_column = diagnostic_prefix.encode_utf16().count() as u64;
    assert_eq!(unresolved["severity"], 1, "known diagnostic is an LSP error");
    assert_eq!(unresolved["range"]["start"]["line"], 4);
    assert_eq!(unresolved["range"]["start"]["character"], utf16_column);
    assert_eq!(
        unresolved["message"],
        "Процедура или функция с указанным именем не определена: 'НетТакойПроцедуры'"
    );

    let client = mcp_client(root.path()).await;
    let disk_number = wait_symbol_return_type(&client, "Число", None).await;
    assert_eq!(disk_number["status"], "ok", "MCP disk baseline: {disk_number}");
    assert!(
        disk_number["return_type"] == "Число",
        "saved fixture has the numeric return_type: {disk_number}"
    );

    let hover_params = json!({
        "textDocument": {"uri": caller_uri.clone()},
        "position": {"line": 2, "character": 8}
    });
    wait_hover_type(&mut lsp, 2, hover_params.clone(), "Число", None);

    lsp.open(&target_path, original_target);
    lsp.send(json!({
        "jsonrpc": "2.0", "method": "textDocument/didChange",
        "params": {"textDocument": {"uri": target_uri.clone(), "version": 2},
            "contentChanges": [{"text": STRING_TARGET}]}
    }));
    let disk_number_during_overlay = wait_symbol_return_type(&client, "Число", None).await;
    let unsaved_string_hover =
        wait_hover_type(&mut lsp, 3, hover_params.clone(), "Строка", Some("Число"));
    assert!(disk_number_during_overlay["return_type"] == "Число");
    assert_ne!(
        unsaved_string_hover["result"].to_string().contains("Строка"),
        disk_number_during_overlay["return_type"] == "Строка",
        "positive type mismatch: LSP buffer is string, MCP disk is numeric"
    );

    let disk_replacement = "Функция ЧисловоеЗначение() Экспорт\n    Возврат 84;\nКонецФункции\n";
    std::fs::write(&target_path, disk_replacement).expect("external numeric disk update");
    lsp.send(json!({
        "jsonrpc": "2.0", "method": "workspace/didChangeWatchedFiles",
        "params": {"changes": [{"uri": target_uri.clone(), "type": 2}]}
    }));
    wait_hover_type(&mut lsp, 4, hover_params.clone(), "Строка", Some("Число"));
    let disk_still_number = wait_symbol_return_type(&client, "Число", None).await;
    assert!(
        disk_still_number["return_type"] == "Число",
        "MCP continues to read the numeric disk version: {disk_still_number}"
    );

    std::fs::write(&target_path, STRING_TARGET).expect("save string source to disk");
    lsp.send(json!({
        "jsonrpc": "2.0", "method": "workspace/didChangeWatchedFiles",
        "params": {"changes": [{"uri": target_uri.clone(), "type": 2}]}
    }));
    lsp.send(json!({
        "jsonrpc": "2.0", "method": "textDocument/didSave",
        "params": {"textDocument": {"uri": target_uri.clone()}}
    }));
    lsp.send(json!({
        "jsonrpc": "2.0", "method": "textDocument/didClose",
        "params": {"textDocument": {"uri": target_uri}}
    }));

    wait_hover_type(&mut lsp, 5, hover_params, "Строка", Some("Число"));
    let disk_string = wait_symbol_return_type(&client, "Строка", Some("Число")).await;
    assert!(
        disk_string["return_type"] == "Строка",
        "after save/close MCP converges to the saved string source: {disk_string}"
    );

    std::fs::write(&target_path, original_target).expect("restore owned fixture contents");
    client.cancel().await.ok();
    assert!(lsp.shutdown(6).success(), "LSP process exits cleanly");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn watched_bsl_remove_and_recreate_updates_both_protocols() {
    let root = fixture();
    let caller_path = root.path().join("CommonModules/Потребитель/Ext/Module.bsl");
    let target_path = root.path().join("CommonModules/ОбщийМодуль/Ext/Module.bsl");
    let caller_uri = lsp_types::Url::from_file_path(&caller_path).unwrap();
    let target_uri = lsp_types::Url::from_file_path(&target_path).unwrap();
    let mut lsp = Lsp::start(root.path());
    lsp.open(&caller_path, CALLER);
    let client = mcp_client(root.path()).await;
    let initial_card = wait_symbol_status(&client, "ok").await;
    assert_eq!(initial_card["status"], "ok");

    let before = lsp.request(
        2,
        "textDocument/definition",
        json!({"textDocument": {"uri": caller_uri.clone()}, "position": {"line": 2, "character": 27}}),
    );
    assert!(before["error"].is_null() && !before["result"].is_null(), "initial target: {before}");

    std::fs::remove_file(&target_path).expect("remove target module");
    lsp.send(json!({
        "jsonrpc": "2.0", "method": "workspace/didChangeWatchedFiles",
        "params": {"changes": [{"uri": target_uri.clone(), "type": 3}]}
    }));
    let mut next_id = 3;
    let deadline = std::time::Instant::now() + HANG_WATCHDOG;
    let removed = loop {
        let response = lsp.request(
            next_id,
            "textDocument/definition",
            json!({"textDocument": {"uri": caller_uri.clone()}, "position": {"line": 2, "character": 27}}),
        );
        next_id += 1;
        assert!(response["error"].is_null(), "missing target is a semantic miss: {response}");
        if response["result"].is_null() {
            break response;
        }
        assert!(std::time::Instant::now() < deadline, "the removed target never disappeared");
        std::thread::sleep(Duration::from_millis(25));
    };
    assert!(removed["result"].is_null(), "removed target has no definition: {removed}");
    let missing_card = wait_symbol_status(&client, "not_found").await;
    assert_eq!(missing_card["status"], "not_found");

    std::fs::write(
        &target_path,
        include_str!("fixtures/shared_workspace_analysis/CommonModules/ОбщийМодуль/Ext/Module.bsl"),
    )
    .expect("recreate target module");
    lsp.send(json!({
        "jsonrpc": "2.0", "method": "workspace/didChangeWatchedFiles",
        "params": {"changes": [{"uri": target_uri.clone(), "type": 1}]}
    }));
    let deadline = std::time::Instant::now() + HANG_WATCHDOG;
    let recreated = loop {
        let response = lsp.request(
            next_id,
            "textDocument/definition",
            json!({"textDocument": {"uri": caller_uri.clone()}, "position": {"line": 2, "character": 27}}),
        );
        next_id += 1;
        assert!(response["error"].is_null(), "recreated target request: {response}");
        if !response["result"].is_null() {
            break response;
        }
        assert!(std::time::Instant::now() < deadline, "the recreated target never returned");
        std::thread::sleep(Duration::from_millis(25));
    };
    assert!(recreated["error"].is_null(), "recreated target request: {recreated}");
    assert!(!recreated["result"].is_null(), "recreated target is resolvable: {recreated}");
    let location =
        if recreated["result"].is_array() { &recreated["result"][0] } else { &recreated["result"] };
    assert_eq!(location["uri"], target_uri.as_str());
    let recreated_card = wait_symbol_status(&client, "ok").await;
    assert_eq!(recreated_card["status"], "ok");
    client.cancel().await.ok();
    assert!(lsp.shutdown(next_id).success(), "LSP process exits cleanly");
}
