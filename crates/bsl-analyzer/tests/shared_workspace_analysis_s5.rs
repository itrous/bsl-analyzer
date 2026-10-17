mod common;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use common::Lsp;
use mcp_server::{serve_stream, McpProfile, McpServer, SharedState, WorkspaceCacheLayout};
use rmcp::model::CallToolRequestParams;
use rmcp::service::RunningService;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{json, Map, Value};

type Client = RunningService<RoleClient, ()>;

const CONSUMER: &str = include_str!(
    "fixtures/shared_workspace_analysis_s5/extensions/consumer/CommonModules/Потребитель/Ext/Module.bsl"
);

fn fixture() -> tempfile::TempDir {
    let root = tempfile::tempdir().expect("fixture workspace");
    let fixture_root =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/shared_workspace_analysis_s5");
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

async fn mcp_client(root: &Path) -> Client {
    let server = McpServer::new(
        McpProfile::Workspace,
        SharedState::workspace(root.to_path_buf()).expect("valid fixture workspace"),
    );
    let (client_io, server_io) = tokio::io::duplex(4 * 1024 * 1024);
    tokio::spawn(serve_stream(server, server_io));
    ().serve(client_io).await.expect("real rmcp handshake")
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
    let column = source_line[..byte_column].encode_utf16().count();
    (line as u32, column as u32)
}

fn last_range_positions(text: &str, range_text: &str) -> ((u32, u32), (u32, u32)) {
    let start = text.rfind(range_text).expect("diagnostic range exists");
    let end = start + range_text.len();
    let position_at = |offset: usize| {
        let prefix = &text[..offset];
        let line = prefix.bytes().filter(|byte| *byte == b'\n').count() as u32;
        let source_line = prefix.rsplit('\n').next().expect("diagnostic source line");
        (line, source_line.encode_utf16().count() as u32)
    };
    (position_at(start), position_at(end))
}

fn source_section(text: &str) -> &str {
    let tail = text.split_once("[source]").expect("source section").1;
    tail.split_once("[diagnostics.parameters]").map_or(tail, |(source, _)| source).trim()
}

fn analysis_config(
    base: &str,
    compatibility_mode: Option<&str>,
    unresolved_bare_call: bool,
) -> String {
    assert_eq!(base.matches("[source]").count(), 1, "fixture has one source section");
    let mut config = base.to_owned();
    if let Some(mode) = compatibility_mode {
        config =
            config.replacen("[source]", &format!("compatibility_mode = \"{mode}\"\n\n[source]"), 1);
    }
    config.push_str(&format!(
        "\n[diagnostics.parameters]\nUnresolvedBareCall = {unresolved_bare_call}\n"
    ));
    config
}

fn replace_config(path: &Path, content: &str) {
    // Truncating a live config can expose a different source scope and retire MCP.
    let mut staged = tempfile::NamedTempFile::new_in(path.parent().expect("config directory"))
        .expect("stage complete config beside the watched file");
    staged.write_all(content.as_bytes()).expect("write complete staged config");
    staged.persist(path).expect("atomically replace watched config");
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

fn lsp_diagnostic_state(
    lsp: &mut Lsp,
    uri: &lsp_types::Url,
    expected_compatibility: bool,
    expected_unresolved: bool,
) -> Value {
    let deadline = std::time::Instant::now() + Duration::from_secs(45);
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        assert!(!remaining.is_zero(), "LSP did not publish the expected settings state");
        let published = lsp
            .wait_for_within(remaining, |message| {
                message["method"] == "textDocument/publishDiagnostics"
                    && message["params"]["uri"] == uri.as_str()
            })
            .expect("LSP diagnostics notification deadline");
        let diagnostics =
            published["params"]["diagnostics"].as_array().expect("LSP diagnostics array");
        let compatibility = diagnostics
            .iter()
            .any(|diagnostic| diagnostic["code"] == "PlatformMemberHiddenByCompatibilityMode");
        let unresolved =
            diagnostics.iter().any(|diagnostic| diagnostic["code"] == "UnresolvedBareCall");
        if compatibility == expected_compatibility && unresolved == expected_unresolved {
            return published;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "LSP settings state did not converge: {published}"
        );
    }
}

async fn mcp_diagnostic_state(
    client: &Client,
    relative_path: &str,
    expected_compatibility: bool,
    expected_unresolved: bool,
) -> Value {
    let describe = |body: Option<&Value>| {
        let state = match body {
            Some(body) => match body["result"]["findings"].as_array() {
                Some(findings) => {
                    let compatibility = findings.iter().any(|finding| {
                        finding["code"] == "PlatformMemberHiddenByCompatibilityMode"
                    });
                    let unresolved =
                        findings.iter().any(|finding| finding["code"] == "UnresolvedBareCall");
                    format!(
                        "status={}, compatibility={compatibility}, unresolved={unresolved}",
                        body["status"].as_str().unwrap_or("<missing>")
                    )
                }
                None => format!(
                    "status={}, findings=<missing>",
                    body["status"].as_str().unwrap_or("<missing>")
                ),
            },
            None => "no MCP body observed".to_owned(),
        };
        format!(
            "expected compatibility={expected_compatibility}, unresolved={expected_unresolved}; observed {state}; last body={body:?}"
        )
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(45);
    let mut last_body = None;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(
            !remaining.is_zero(),
            "MCP diagnostics did not reach the expected settings state: {}",
            describe(last_body.as_ref())
        );
        let response = match tokio::time::timeout(
            remaining,
            client.call_tool(CallToolRequestParams::new("diagnostics").with_arguments(args(&[
                ("action", json!("file")),
                ("path", json!(relative_path)),
                ("min_severity", json!("hint")),
                ("detail", json!("detailed")),
            ]))),
        )
        .await
        {
            Ok(Ok(response)) => response,
            Ok(Err(error)) => panic!(
                "MCP diagnostics transport failed ({error:?}): {}",
                describe(last_body.as_ref())
            ),
            Err(_) => panic!("MCP diagnostics request timed out: {}", describe(last_body.as_ref())),
        };
        assert_ne!(
            response.is_error,
            Some(true),
            "MCP diagnostics returned an error {response:?}: {}",
            describe(last_body.as_ref())
        );
        let response_debug = format!("{response:?}");
        let body = response.structured_content.unwrap_or_else(|| {
            panic!(
                "MCP diagnostics response has no structured content {response_debug}: {}",
                describe(last_body.as_ref())
            )
        });
        last_body = Some(body.clone());
        if body["status"] == "loading" {
            tokio::time::sleep(Duration::from_millis(50)).await;
            continue;
        }
        let findings = body["result"]["findings"].as_array().unwrap_or_else(|| {
            panic!("MCP diagnostics has no findings array: {}", describe(Some(&body)))
        });
        let compatibility = findings
            .iter()
            .any(|finding| finding["code"] == "PlatformMemberHiddenByCompatibilityMode");
        let unresolved = findings.iter().any(|finding| finding["code"] == "UnresolvedBareCall");
        if compatibility == expected_compatibility && unresolved == expected_unresolved {
            return body;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "MCP settings state did not converge: {}",
            describe(Some(&body))
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn assert_lsp_finding(
    published: &Value,
    source: &str,
    code: &str,
    range_text: &str,
    severity: u64,
    expected_message: &str,
) {
    let diagnostics = published["params"]["diagnostics"].as_array().expect("LSP diagnostics");
    let matching: Vec<_> =
        diagnostics.iter().filter(|diagnostic| diagnostic["code"] == code).collect();
    assert_eq!(matching.len(), 1, "expected one {code} diagnostic: {published}");
    let ((line, character), (end_line, end_character)) = last_range_positions(source, range_text);
    assert_eq!(matching[0]["severity"], severity, "{code} severity: {published}");
    assert_eq!(matching[0]["message"], expected_message, "{code} identity: {published}");
    assert_eq!(matching[0]["range"]["start"]["line"], line, "{code} range: {published}");
    assert_eq!(matching[0]["range"]["start"]["character"], character, "{code} range: {published}");
    assert_eq!(matching[0]["range"]["end"]["line"], end_line, "{code} range: {published}");
    assert_eq!(
        matching[0]["range"]["end"]["character"], end_character,
        "{code} range: {published}"
    );
}

fn assert_mcp_finding(
    body: &Value,
    source: &str,
    code: &str,
    range_text: &str,
    expected_message: &str,
) {
    let findings = body["result"]["findings"].as_array().expect("MCP findings");
    let matching: Vec<_> = findings.iter().filter(|finding| finding["code"] == code).collect();
    assert_eq!(matching.len(), 1, "expected one {code} finding: {body}");
    let finding = matching[0];
    let ((line, utf16_column), (end_line, end_utf16_column)) =
        last_range_positions(source, range_text);
    assert_eq!(finding["internal_severity"], "Major", "{code} severity: {body}");
    assert_eq!(finding["message"], expected_message, "{code} identity: {body}");
    if code == "ModuleAccessibility" {
        assert_eq!(
            finding["standards"],
            json!([{ "id": 469, "url": "https://v8std.ru/std/469/" }]),
            "{code} standard identity: {body}"
        );
    }
    assert_eq!(finding["range"]["start_line"], line, "{code} range: {body}");
    assert_eq!(finding["range"]["start_column"], utf16_column, "{code} range: {body}");
    assert_eq!(finding["range"]["end_line"], end_line, "{code} range: {body}");
    assert_eq!(finding["range"]["end_column"], end_utf16_column, "{code} range: {body}");
    assert_eq!(finding["location"]["position_encoding"], "utf-16", "{code} location: {body}");
    assert_eq!(finding["location"]["range"]["start_line"], line, "{code} location: {body}");
    assert_eq!(
        finding["location"]["range"]["start_character"], utf16_column,
        "{code} location: {body}"
    );
    assert_eq!(finding["location"]["range"]["end_line"], end_line, "{code} location: {body}");
    assert_eq!(
        finding["location"]["range"]["end_character"], end_utf16_column,
        "{code} location: {body}"
    );
}

async fn mcp_call_at(
    client: &Client,
    source_path: &Path,
    needle: &str,
    allow_loading: bool,
) -> Value {
    let (line, column) = position(CONSUMER, needle);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(45);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(!remaining.is_zero(), "MCP symbol_info never returned a card for {needle}");
        let call = tokio::time::timeout(
            remaining,
            client.call_tool(CallToolRequestParams::new("symbol_info").with_arguments(args(&[
                ("path", json!(source_path.to_string_lossy())),
                ("line", json!(line)),
                ("column", json!(column)),
            ]))),
        )
        .await
        .expect("MCP symbol_info deadline")
        .expect("MCP symbol_info transport");
        let card = call.structured_content.expect("symbol_info structured result");
        match card["status"].as_str() {
            Some("loading") => assert!(allow_loading, "ready MCP workspace regressed to loading"),
            Some("ok" | "not_found") => return card,
            _ => panic!("symbol_info returned an invalid result for {needle}: {card}"),
        }
        assert!(tokio::time::Instant::now() < deadline, "MCP symbol_info timeout");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn wait_mcp_call_type(
    client: &Client,
    source_path: &Path,
    needle: &str,
    expected: &str,
    allow_loading: bool,
) -> Value {
    let card = mcp_call_at(client, source_path, needle, allow_loading).await;
    assert_eq!(card["status"], "ok", "symbol_info did not resolve {needle}: {card}");
    assert_eq!(card["return_type"], expected, "semantic type for {needle}: {card}");
    card
}

async fn symbol_status(client: &Client, symbol: &str) -> Value {
    let result = tokio::time::timeout(
        Duration::from_secs(45),
        client.call_tool(
            CallToolRequestParams::new("symbol_info")
                .with_arguments(args(&[("symbol", json!(symbol))])),
        ),
    )
    .await
    .expect("MCP symbol_info deadline")
    .expect("MCP symbol_info transport");
    let card = result.structured_content.expect("symbol_info structured result");
    assert_ne!(card["status"], "loading", "ready MCP workspace regressed to loading");
    card
}

fn hover(lsp: &mut Lsp, id: u64, uri: &lsp_types::Url, needle: &str) -> Value {
    let (line, character) = position(CONSUMER, needle);
    lsp.request(
        id,
        "textDocument/hover",
        json!({
            "textDocument": {"uri": uri},
            "position": {"line": line, "character": character}
        }),
    )
}

fn definition(lsp: &mut Lsp, id: u64, uri: &lsp_types::Url, needle: &str) -> Value {
    let (line, character) = position(CONSUMER, needle);
    lsp.request(
        id,
        "textDocument/definition",
        json!({
            "textDocument": {"uri": uri},
            "position": {"line": line, "character": character}
        }),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn extension_dependency_exclusion_and_duplicate_names_match_across_protocols() {
    let root = fixture();
    let consumer_relative =
        PathBuf::from("extensions/consumer/CommonModules/Потребитель/Ext/Module.bsl");
    let consumer_path = root.path().join(&consumer_relative);
    let provider_path = root.path().join("extensions/provider/CommonModules/Общий/Ext/Module.bsl");
    let base_path = root.path().join("CommonModules/Общий/Ext/Module.bsl");
    let consumer_uri = lsp_types::Url::from_file_path(&consumer_path).expect("consumer URI");
    let provider_uri = lsp_types::Url::from_file_path(&provider_path).expect("provider URI");
    let base_uri = lsp_types::Url::from_file_path(&base_path).expect("base URI");

    let mut lsp = Lsp::start(root.path());
    lsp.open(&consumer_path, CONSUMER);
    let client = mcp_client(root.path()).await;

    let provider_call =
        wait_mcp_call_type(&client, &consumer_path, "ДатьСтроку", "Строка", true).await;
    let provider_definition = provider_call["definitions"]
        .as_array()
        .expect("provider definition list")
        .first()
        .expect("provider definition");
    assert!(
        provider_definition["source_extension"] == "Поставщик",
        "the declaration must identify its source extension: {provider_call}"
    );
    assert_eq!(
        provider_definition["location"]["path"],
        "extensions/provider/CommonModules/Общий/Ext/Module.bsl",
        "the dependency call must resolve into the provider extension: {provider_call}"
    );
    let provider_hover = hover(&mut lsp, 2, &consumer_uri, "ОтветПоставщика");
    assert!(provider_hover["error"].is_null(), "LSP hover failed: {provider_hover}");
    assert!(
        provider_hover["result"].to_string().contains("Строка"),
        "LSP sees the provider's String result: {provider_hover}"
    );
    let provider_target = definition(&mut lsp, 3, &consumer_uri, "ДатьСтроку");
    assert!(provider_target["error"].is_null(), "LSP definition failed: {provider_target}");
    assert!(
        provider_target["result"].to_string().contains(provider_uri.as_str()),
        "LSP definition comes from the declared dependency: {provider_target}"
    );

    let base_call = wait_mcp_call_type(&client, &consumer_path, "Спорный", "Число", false).await;
    let base_definition = base_call["definitions"]
        .as_array()
        .expect("base definition list")
        .first()
        .expect("base definition");
    assert_eq!(base_definition["location"]["root_id"], "", "configuration wins duplicate name");
    assert_eq!(
        base_definition["location"]["path"], "CommonModules/Общий/Ext/Module.bsl",
        "configuration duplicate has the canonical base path: {base_call}"
    );
    let base_hover = hover(&mut lsp, 4, &consumer_uri, "ОтветКонфигурации");
    assert!(base_hover["error"].is_null(), "LSP hover failed: {base_hover}");
    assert!(
        base_hover["result"].to_string().contains("Число"),
        "LSP applies configuration precedence to the duplicate: {base_hover}"
    );
    let base_target = definition(&mut lsp, 5, &consumer_uri, "Спорный");
    assert!(base_target["error"].is_null(), "LSP definition failed: {base_target}");
    assert!(
        base_target["result"].to_string().contains(base_uri.as_str()),
        "LSP definition uses the base configuration body: {base_target}"
    );

    let excluded = symbol_status(&client, "СкрытыйМодуль.СекретныйМетод").await;
    assert_eq!(excluded["status"], "not_found", "excluded extension is absent: {excluded}");
    let excluded_target = definition(&mut lsp, 6, &consumer_uri, "СкрытыйМодуль");
    assert!(excluded_target["error"].is_null(), "LSP definition failed: {excluded_target}");
    assert!(
        excluded_target["result"].is_null()
            || excluded_target["result"].as_array().is_some_and(Vec::is_empty),
        "LSP cannot navigate into the excluded extension: {excluded_target}"
    );

    client.cancel().await.ok();
    assert!(lsp.shutdown(7).success(), "LSP exits cleanly");

    let without_dependency = fixture();
    let config_path = without_dependency.path().join("bsl-analyzer.toml");
    let config = std::fs::read_to_string(&config_path).expect("read fixture config");
    let independent_config = config.replacen(", dependsOn = [\"Поставщик\"]", "", 1);
    assert_ne!(independent_config, config, "fixture has exactly one declared dependency");
    std::fs::write(config_path, independent_config).expect("remove only the dependency edge");
    let isolated_consumer = without_dependency
        .path()
        .join("extensions/consumer/CommonModules/Потребитель/Ext/Module.bsl");
    let isolated_uri =
        lsp_types::Url::from_file_path(&isolated_consumer).expect("isolated consumer URI");
    let mut isolated_lsp = Lsp::start(without_dependency.path());
    isolated_lsp.open(&isolated_consumer, CONSUMER);
    let isolated_client = mcp_client(without_dependency.path()).await;

    let hidden_provider =
        mcp_call_at(&isolated_client, &isolated_consumer, "ДатьСтроку", true).await;
    assert!(
        hidden_provider["status"] == "not_found"
            || (hidden_provider["status"] == "ok"
                && hidden_provider["kind"] == "unresolved"
                && hidden_provider["return_type"].is_null()),
        "without dependsOn the provider-only call has no semantic target: {hidden_provider}"
    );
    assert!(
        hidden_provider["definitions"].is_null()
            || hidden_provider["definitions"].as_array().is_some_and(Vec::is_empty),
        "without dependsOn MCP publishes no semantic definition: {hidden_provider}"
    );
    let hidden_target = definition(&mut isolated_lsp, 2, &isolated_uri, "ДатьСтроку");
    assert!(hidden_target["error"].is_null(), "LSP definition failed: {hidden_target}");
    assert!(
        hidden_target["result"].is_null()
            || hidden_target["result"].as_array().is_some_and(Vec::is_empty),
        "without dependsOn LSP has no provider definition: {hidden_target}"
    );
    let isolated_base =
        wait_mcp_call_type(&isolated_client, &isolated_consumer, "Спорный", "Число", false).await;
    assert!(isolated_base["definitions"][0]["location"]["root_id"] == "");
    let isolated_base_hover = hover(&mut isolated_lsp, 3, &isolated_uri, "ОтветКонфигурации");
    assert!(isolated_base_hover["error"].is_null(), "LSP hover failed: {isolated_base_hover}");
    assert!(
        isolated_base_hover["result"].to_string().contains("Число"),
        "dependency removal leaves base visibility intact: {isolated_base_hover}"
    );

    isolated_client.cancel().await.ok();
    assert!(isolated_lsp.shutdown(4).success(), "isolated LSP exits cleanly");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn effective_settings_compatibility_and_execution_context_match_live_adapters() {
    const COMPATIBILITY_CODE: &str = "PlatformMemberHiddenByCompatibilityMode";
    const SETTINGS_CODE: &str = "UnresolvedBareCall";
    const CONTEXT_CODE: &str = "ModuleAccessibility";
    const CONTEXT_RANGE: &str = "Общий.ДатьСтроку";
    const COMPATIBILITY_MESSAGE: &str =
        "'СтрРазделить' недоступен в режиме совместимости 8.2.13: виден с режима 8.3.6";
    const SETTINGS_MESSAGE: &str =
        "Процедура или функция с указанным именем не определена: 'НетТакойПроцедуры'";
    const MCP_CONTEXT_MESSAGE: &str = "Модуль 'Общий' недоступен [Тонкий клиент, Веб-клиент, Толстый клиент (управляемое приложение)]";
    const CONTEXT_MESSAGE: &str = "Модуль 'Общий' недоступен [Тонкий клиент, Веб-клиент, Толстый клиент (управляемое приложение)] (Стандарт 469: https://v8std.ru/std/469/)";

    let root = fixture();
    let consumer_relative =
        PathBuf::from("extensions/consumer/CommonModules/Потребитель/Ext/Module.bsl");
    let consumer_path = root.path().join(&consumer_relative);
    let provider_path = root.path().join("extensions/provider/CommonModules/Общий/Ext/Module.bsl");
    let provider_uri = lsp_types::Url::from_file_path(&provider_path).expect("provider URI");
    let consumer_uri = lsp_types::Url::from_file_path(&consumer_path).expect("consumer URI");
    let consumer_xml_path = root.path().join("extensions/consumer/CommonModules/Потребитель.xml");
    let config_path = root.path().join("bsl-analyzer.toml");
    let xml_path = root.path().join("Configuration.xml");
    let original_config = std::fs::read_to_string(&config_path).expect("read original config");
    let original_xml = std::fs::read_to_string(&xml_path).expect("read original Configuration.xml");
    let original_consumer_xml =
        std::fs::read_to_string(&consumer_xml_path).expect("read consumer module metadata");

    // Runtime-only controls cover analysis context without changing source composition
    // during the settings changes below.
    let mut source = CONSUMER.to_owned();
    source.push_str(concat!(
        "\n&НаСервере\n",
        "Процедура ПроверитьСовместимость() Экспорт\n",
        "    Части = СтрРазделить(\"a,b\", \",\");\n",
        "КонецПроцедуры\n\n",
        "&НаСервере\n",
        "Процедура ПроверитьНастройки() Экспорт\n",
        "    НетТакойПроцедуры();\n",
        "КонецПроцедуры\n\n",
        "&НаКлиенте\n",
        "Процедура ПроверитьКонтекстКлиента() Экспорт\n",
        "    Ответ = Общий.ДатьСтроку();\n",
        "КонецПроцедуры\n",
    ));
    std::fs::write(&consumer_path, &source).expect("write disposable S5 control source");
    let client_enabled_consumer_xml = original_consumer_xml.replace(
        "<ClientManagedApplication>false</ClientManagedApplication>",
        "<ClientManagedApplication>true</ClientManagedApplication>",
    );
    assert_ne!(
        client_enabled_consumer_xml, original_consumer_xml,
        "consumer metadata has client support disabled"
    );
    std::fs::write(&consumer_xml_path, &client_enabled_consumer_xml)
        .expect("enable client context in disposable module metadata");
    let xml_with_later_mode = original_xml.replace(
        "<Properties><Name>БазоваяКонфигурация</Name></Properties>",
        "<Properties><Name>БазоваяКонфигурация</Name><CompatibilityMode>Version8_3_17</CompatibilityMode></Properties>",
    );
    assert_ne!(xml_with_later_mode, original_xml, "fixture XML has the expected properties");
    std::fs::write(&xml_path, &xml_with_later_mode).expect("set project XML compatibility mode");
    let initial_config = analysis_config(&original_config, Some("Version8_2_13"), true);
    std::fs::write(&config_path, &initial_config).expect("set compatibility override and rule");

    let mut lsp = Lsp::start(root.path());
    let initial_lsp = lsp.open(&consumer_path, &source);
    // Give MCP its own disposable cache so this acceptance scenario cannot observe or
    // modify the user's workspace cache.
    let cache_parent = tempfile::tempdir().expect("owned MCP cache parent");
    let project = mcp_server::project::at(root.path()).expect("fixture project");
    let cache =
        WorkspaceCacheLayout::for_project(&project, Some(cache_parent.path()), root.path(), None)
            .expect("owned workspace cache");
    let state = SharedState::workspace_with_cache(root.path().to_path_buf(), cache)
        .expect("MCP workspace from fixture settings");
    let server = McpServer::new(McpProfile::Workspace, state.clone());
    let (client_io, server_io) = tokio::io::duplex(4 * 1024 * 1024);
    let server_task = tokio::spawn(serve_stream(server, server_io));
    let client = ().serve(client_io).await.expect("real rmcp handshake");
    let relative = consumer_relative.to_str().expect("UTF-8 fixture path");

    assert_eq!(
        initial_lsp["params"]["uri"],
        consumer_uri.as_str(),
        "LSP opened the controlled source"
    );
    assert_lsp_finding(
        &initial_lsp,
        &source,
        COMPATIBILITY_CODE,
        "СтрРазделить",
        1,
        COMPATIBILITY_MESSAGE,
    );
    assert_lsp_finding(
        &initial_lsp,
        &source,
        SETTINGS_CODE,
        "НетТакойПроцедуры",
        1,
        SETTINGS_MESSAGE,
    );
    assert_lsp_finding(&initial_lsp, &source, CONTEXT_CODE, CONTEXT_RANGE, 1, CONTEXT_MESSAGE);
    let initial_mcp = mcp_diagnostic_state(&client, relative, true, true).await;
    assert_mcp_finding(
        &initial_mcp,
        &source,
        COMPATIBILITY_CODE,
        "СтрРазделить",
        COMPATIBILITY_MESSAGE,
    );
    assert_mcp_finding(&initial_mcp, &source, SETTINGS_CODE, "НетТакойПроцедуры", SETTINGS_MESSAGE);
    assert_mcp_finding(&initial_mcp, &source, CONTEXT_CODE, CONTEXT_RANGE, MCP_CONTEXT_MESSAGE);

    let provider_call =
        wait_mcp_call_type(&client, &consumer_path, "ДатьСтроку", "Строка", false).await;
    assert_eq!(
        provider_call["definitions"][0]["location"]["path"],
        "extensions/provider/CommonModules/Общий/Ext/Module.bsl",
        "active provider target survives the effective settings"
    );
    let provider_hover = hover(&mut lsp, 2, &consumer_uri, "ОтветПоставщика");
    assert!(provider_hover["error"].is_null(), "LSP hover failed: {provider_hover}");
    assert!(
        provider_hover["result"].to_string().contains("Строка"),
        "provider type: {provider_hover}"
    );
    let provider_definition = definition(&mut lsp, 3, &consumer_uri, "ДатьСтроку");
    assert!(provider_definition["error"].is_null(), "LSP definition failed: {provider_definition}");
    assert!(
        provider_definition["result"].to_string().contains(provider_uri.as_str()),
        "provider target: {provider_definition}"
    );

    let disabled_config = analysis_config(&original_config, Some("Version8_2_13"), false);
    replace_config(&config_path, &disabled_config);
    send_config_change(&mut lsp, &config_path);
    let disabled_lsp = lsp_diagnostic_state(&mut lsp, &consumer_uri, true, false);
    assert_lsp_finding(
        &disabled_lsp,
        &source,
        COMPATIBILITY_CODE,
        "СтрРазделить",
        1,
        COMPATIBILITY_MESSAGE,
    );
    assert_lsp_finding(&disabled_lsp, &source, CONTEXT_CODE, CONTEXT_RANGE, 1, CONTEXT_MESSAGE);
    let disabled_mcp = mcp_diagnostic_state(&client, relative, true, false).await;
    assert_mcp_finding(
        &disabled_mcp,
        &source,
        COMPATIBILITY_CODE,
        "СтрРазделить",
        COMPATIBILITY_MESSAGE,
    );
    assert_mcp_finding(&disabled_mcp, &source, CONTEXT_CODE, CONTEXT_RANGE, MCP_CONTEXT_MESSAGE);
    let retained_provider =
        wait_mcp_call_type(&client, &consumer_path, "ДатьСтроку", "Строка", false).await;
    assert_eq!(
        retained_provider["definitions"][0]["location"]["path"],
        provider_call["definitions"][0]["location"]["path"],
        "rule toggle does not change source target"
    );
    let retained_hover = hover(&mut lsp, 4, &consumer_uri, "ОтветПоставщика");
    assert!(
        retained_hover["error"].is_null(),
        "LSP hover failed after rule toggle: {retained_hover}"
    );
    assert!(
        retained_hover["result"].to_string().contains("Строка"),
        "provider type after toggle: {retained_hover}"
    );
    let retained_definition = definition(&mut lsp, 5, &consumer_uri, "ДатьСтроку");
    assert!(
        retained_definition["error"].is_null(),
        "LSP definition failed after rule toggle: {retained_definition}"
    );
    assert!(
        retained_definition["result"].to_string().contains(provider_uri.as_str()),
        "provider target survives the rule toggle: {retained_definition}"
    );

    let restored_config = analysis_config(&original_config, Some("Version8_2_13"), true);
    replace_config(&config_path, &restored_config);
    send_config_change(&mut lsp, &config_path);
    let restored_lsp = lsp_diagnostic_state(&mut lsp, &consumer_uri, true, true);
    assert_lsp_finding(
        &restored_lsp,
        &source,
        SETTINGS_CODE,
        "НетТакойПроцедуры",
        1,
        SETTINGS_MESSAGE,
    );
    let restored_mcp = mcp_diagnostic_state(&client, relative, true, true).await;
    assert_mcp_finding(
        &restored_mcp,
        &source,
        SETTINGS_CODE,
        "НетТакойПроцедуры",
        SETTINGS_MESSAGE,
    );

    // Removing the TOML override exposes the fixed XML 8.3.17 mode: the platform
    // diagnostic disappears while the independent setting and execution context stay.
    let xml_fallback = analysis_config(&original_config, None, true);
    replace_config(&config_path, &xml_fallback);
    send_config_change(&mut lsp, &config_path);
    let fallback_lsp = lsp_diagnostic_state(&mut lsp, &consumer_uri, false, true);
    assert_lsp_finding(
        &fallback_lsp,
        &source,
        SETTINGS_CODE,
        "НетТакойПроцедуры",
        1,
        SETTINGS_MESSAGE,
    );
    let fallback_mcp = mcp_diagnostic_state(&client, relative, false, true).await;
    assert_mcp_finding(
        &fallback_mcp,
        &source,
        SETTINGS_CODE,
        "НетТакойПроцедуры",
        SETTINGS_MESSAGE,
    );
    assert_lsp_finding(&fallback_lsp, &source, CONTEXT_CODE, CONTEXT_RANGE, 1, CONTEXT_MESSAGE);
    assert_mcp_finding(&fallback_mcp, &source, CONTEXT_CODE, CONTEXT_RANGE, MCP_CONTEXT_MESSAGE);
    let fallback_provider =
        wait_mcp_call_type(&client, &consumer_path, "ДатьСтроку", "Строка", false).await;
    assert_eq!(
        fallback_provider["definitions"][0]["location"]["path"],
        provider_call["definitions"][0]["location"]["path"],
        "XML compatibility fallback preserves MCP provider target"
    );
    let fallback_hover = hover(&mut lsp, 6, &consumer_uri, "ОтветПоставщика");
    assert!(
        fallback_hover["error"].is_null(),
        "LSP hover failed after XML fallback: {fallback_hover}"
    );
    assert!(
        fallback_hover["result"].to_string().contains("Строка"),
        "provider type after XML fallback: {fallback_hover}"
    );
    let fallback_definition = definition(&mut lsp, 7, &consumer_uri, "ДатьСтроку");
    assert!(
        fallback_definition["error"].is_null(),
        "LSP definition failed after XML fallback: {fallback_definition}"
    );
    assert!(
        fallback_definition["result"].to_string().contains(provider_uri.as_str()),
        "XML compatibility fallback preserves LSP provider target: {fallback_definition}"
    );

    assert_eq!(source_section(&xml_fallback), source_section(&original_config));
    assert_eq!(
        std::fs::read_to_string(&consumer_path).unwrap(),
        source,
        "source scope stays fixed"
    );
    assert!(lsp.shutdown(8).success(), "LSP exits cleanly after compatibility fallback");
    client.cancel().await.ok();
    tokio::time::timeout(Duration::from_secs(15), server_task)
        .await
        .expect("MCP server task joins after transport cancellation")
        .expect("MCP server task joins cleanly")
        .expect("MCP server exits cleanly");
    state.shutdown();
    std::fs::write(&consumer_path, CONSUMER).expect("restore disposable BSL fixture");
    std::fs::write(&consumer_xml_path, &original_consumer_xml)
        .expect("restore disposable consumer metadata");
    std::fs::write(&config_path, &original_config).expect("restore disposable project config");
    std::fs::write(&xml_path, &original_xml).expect("restore disposable Configuration.xml");
    assert_eq!(std::fs::read_to_string(&consumer_path).unwrap(), CONSUMER);
    assert_eq!(std::fs::read_to_string(&consumer_xml_path).unwrap(), original_consumer_xml);
    assert_eq!(std::fs::read_to_string(&config_path).unwrap(), original_config);
    assert_eq!(std::fs::read_to_string(&xml_path).unwrap(), original_xml);
}
