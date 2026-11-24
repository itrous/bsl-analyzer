//! The `snippet` action over a real MCP session: BSL text that is not a file, analysed
//! inside a host module of the workspace and answered with ranges relative to the text.

use std::path::Path;
use std::time::Duration;

use mcp_server::{serve_stream, McpProfile, McpServer, SharedState};
use rmcp::model::CallToolRequestParams;
use rmcp::service::RunningService;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{json, Map, Value};

// Shared helpers for the gates in this binary; not every binary uses every one.
#[allow(dead_code)]
mod common;

type Client = RunningService<RoleClient, ()>;
static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const HOST: &str = "CommonModules/Хозяин/Ext/Module.bsl";
const HOST_TEXT: &str = "\
Процедура Публичная() Экспорт
    Служебная();
КонецПроцедуры

Процедура Служебная()
    НеизвестнаяПроцедураХозяина();
КонецПроцедуры
";

fn write(root: &Path, relative: &str, contents: &str) {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

fn write_module(root: &Path, name: &str, uuid_tail: u32, body: &str) {
    write(
        root,
        &format!("CommonModules/{name}.xml"),
        &format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject>
  <CommonModule uuid="00000000-0000-0000-0000-0000000000{uuid_tail:02}">
    <Properties><Name>{name}</Name><Server>true</Server></Properties>
  </CommonModule>
</MetaDataObject>"#
        ),
    );
    write(root, &format!("CommonModules/{name}/Ext/Module.bsl"), body);
}

fn project(root: &Path) {
    write_module(root, "Хозяин", 1, HOST_TEXT);
    write_module(root, "Сосед", 2, "Процедура Пусто() Экспорт\nКонецПроцедуры\n");
}

async fn client(root: &Path) -> Client {
    let server = McpServer::new(
        McpProfile::Workspace,
        SharedState::workspace(root.to_path_buf()).expect("valid project"),
    );
    let (client_io, server_io) = tokio::io::duplex(4 * 1024 * 1024);
    tokio::spawn(serve_stream(server, server_io));
    ().serve(client_io).await.expect("session initialized")
}

async fn diagnostics(client: &Client, arguments: Value) -> Value {
    let arguments: Map<String, Value> = arguments.as_object().unwrap().clone();
    let mut deadline = common::Patience::new(common::STATE_CEILING, Duration::from_millis(20));
    loop {
        let result = client
            .call_tool(CallToolRequestParams::new("diagnostics").with_arguments(arguments.clone()))
            .await
            .expect("diagnostics call");
        let body = result.structured_content.expect("structured response");
        if body["status"] != "loading" {
            return body;
        }
        assert!(deadline.allows(), "resident did not become ready");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn codes(body: &Value) -> Vec<String> {
    body["result"]["findings"]
        .as_array()
        .unwrap_or_else(|| panic!("findings in {body}"))
        .iter()
        .map(|finding| finding["code"].as_str().unwrap().to_owned())
        .collect()
}

// The authored module has a known exported method and a completely read interface.
// A miss is provable without relying on platform-help provenance.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_method_the_module_lacks_is_reported_relative_to_the_snippet() {
    let _guard = TEST_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    project(dir.path());
    let client = client(dir.path()).await;

    let body = diagnostics(
        &client,
        json!({
            "action": "snippet",
            "code": "Сосед.Пусто();\nСосед.МетодКоторогоНет();",
        }),
    )
    .await;

    assert_eq!(body["action"], "snippet", "{body}");
    let finding = body["result"]["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|finding| finding["code"] == "UnresolvedMethodCall")
        .unwrap_or_else(|| panic!("no UnresolvedMethodCall in {body}"));
    assert_eq!(finding["range"]["start_line"], 1, "{body}");
    assert!(finding.get("location").is_none(), "a snippet has no file location: {body}");
    assert_eq!(body["result"]["context"]["code_kind"], "statements", "{body}");
    assert_eq!(body["result"]["context"]["chosen"], "default", "{body}");
    client.cancel().await.expect("session closed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_correct_snippet_has_no_findings() {
    let _guard = TEST_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    project(dir.path());
    let client = client(dir.path()).await;

    let body = diagnostics(
        &client,
        json!({
            "action": "snippet",
            "code": "Таблица = Новый ТаблицаЗначений;\nТаблица.Колонки.Добавить(\"Имя\");\nРезультат = Таблица.Количество();",
            "known_variables": ["Результат"],
            "min_severity": "hint",
        }),
    )
    .await;

    assert_eq!(codes(&body), Vec::<String>::new(), "{body}");
    client.cancel().await.expect("session closed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_host_module_resolves_its_own_procedures() {
    let _guard = TEST_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    project(dir.path());
    let client = client(dir.path()).await;
    let code = "Служебная();";

    let hosted =
        diagnostics(&client, json!({"action": "snippet", "code": code, "path": HOST})).await;
    assert!(!codes(&hosted).contains(&"UnresolvedBareCall".to_owned()), "{hosted}");
    assert_eq!(hosted["result"]["context"]["chosen"], "path", "{hosted}");

    // Control: the same text in another module does not see a non-exported procedure.
    let elsewhere = diagnostics(
        &client,
        json!({"action": "snippet", "code": code, "path": "CommonModules/Сосед/Ext/Module.bsl"}),
    )
    .await;
    assert!(codes(&elsewhere).contains(&"UnresolvedBareCall".to_owned()), "{elsewhere}");
    client.cancel().await.expect("session closed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_snippet_leaves_the_host_file_diagnostics_unchanged() {
    let _guard = TEST_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    project(dir.path());
    let client = client(dir.path()).await;
    let file = json!({"action": "file", "path": HOST, "min_severity": "hint"});

    let before = diagnostics(&client, file.clone()).await;
    assert!(codes(&before).contains(&"UnresolvedBareCall".to_owned()), "{before}");

    let snippet = diagnostics(
        &client,
        json!({
            "action": "snippet",
            "code": "Процедура Добавленная()\n    НеизвестнаяИзФрагмента();\nКонецПроцедуры",
            "path": HOST,
            "min_severity": "hint",
        }),
    )
    .await;
    assert_eq!(snippet["result"]["context"]["code_kind"], "module", "{snippet}");
    assert!(codes(&snippet).contains(&"UnresolvedBareCall".to_owned()), "{snippet}");

    let after = diagnostics(&client, file).await;
    assert_eq!(before["result"]["findings"], after["result"]["findings"], "{before}\n{after}");
    assert_eq!(std::fs::read_to_string(dir.path().join(HOST)).unwrap(), HOST_TEXT);
    client.cancel().await.expect("session closed");
}

// Module text has no wrapper line to surface its unclosed block on: the parser reports the
// missing closer at the end of the host, past the snippet, and that error is still the
// snippet's. The host tail is clean here, so nothing there is the host's own.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unclosed_method_in_module_code_is_a_parse_error_of_the_snippet() {
    let _guard = TEST_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    project(dir.path());
    let client = client(dir.path()).await;
    let code = "Процедура Незакрытая()\n    А = 1;";

    let body = diagnostics(&client, json!({"action": "snippet", "code": code, "path": HOST})).await;

    assert_eq!(body["result"]["context"]["code_kind"], "module", "{body}");
    let finding = body["result"]["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|finding| finding["code"] == "ParseError")
        .unwrap_or_else(|| panic!("no ParseError in {body}"));
    assert_eq!(finding["range"]["start_line"], 1, "pinned to the end of code: {body}");
    client.cancel().await.expect("session closed");
}

// Control: a host whose own text after its last method does not parse keeps that error to
// itself, so a closed snippet placed before it gets no parse error.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_parse_error_of_the_host_tail_is_not_the_snippets() {
    let _guard = TEST_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    project(dir.path());
    let broken = "CommonModules/Сосед/Ext/Module.bsl";
    write(dir.path(), broken, "Процедура Пусто() Экспорт\nКонецПроцедуры\n\nА = ;\n");
    let client = client(dir.path()).await;

    let file = diagnostics(&client, json!({"action": "file", "path": broken})).await;
    assert!(codes(&file).contains(&"ParseError".to_owned()), "{file}");

    let body = diagnostics(
        &client,
        json!({
            "action": "snippet",
            "code": "Процедура Закрытая()\nКонецПроцедуры",
            "path": broken,
            "codes": ["ParseError"],
        }),
    )
    .await;
    assert_eq!(codes(&body), Vec::<String>::new(), "{body}");
    client.cancel().await.expect("session closed");
}

// A host tail with its own parse error does not hide the snippet's: only the errors the host
// has by itself there are the host's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unclosed_method_is_reported_in_a_host_whose_tail_does_not_parse() {
    let _guard = TEST_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    project(dir.path());
    let broken = "CommonModules/Сосед/Ext/Module.bsl";
    write(dir.path(), broken, "Процедура Пусто() Экспорт\nКонецПроцедуры\n\nА = ;\n");
    let client = client(dir.path()).await;

    let body = diagnostics(
        &client,
        json!({
            "action": "snippet",
            "code": "Процедура Незакрытая()\n    А = 1;",
            "path": broken,
            "codes": ["ParseError"],
        }),
    )
    .await;
    assert!(codes(&body).contains(&"ParseError".to_owned()), "{body}");
    client.cancel().await.expect("session closed");
}

// A rule about a whole module's layout is dropped from a snippet's findings; the answer must
// still name it, or "no finding" and "finding discarded" are indistinguishable.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dropped_module_structure_rule_is_named_in_the_answer() {
    let _guard = TEST_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    project(dir.path());
    let client = client(dir.path()).await;

    let body = diagnostics(
        &client,
        json!({
            "action": "snippet",
            "code": "Процедура НиктоНеЗовет()\n    А = 1;\n    Сообщить(А);\nКонецПроцедуры\n",
        }),
    )
    .await;
    let result = &body["result"];
    assert!(
        result["module_structure_omitted"].as_u64().unwrap_or(0) > 0,
        "expected a dropped module-structure finding: {body}"
    );
    let named: Vec<String> = result["module_structure_omitted_codes"]
        .as_array()
        .unwrap_or_else(|| panic!("module_structure_omitted_codes in {body}"))
        .iter()
        .map(|code| code.as_str().unwrap().to_owned())
        .collect();
    assert!(
        named.contains(&"UnusedLocalMethod".to_owned()),
        "dropped codes must name the rule: {named:?} in {body}"
    );
    client.cancel().await.expect("session closed");
}

// Two findings of one answer, each carrying a fix: removing an empty statement is
// deterministic, removing a self-assignment is not something to do blind. `has_fix` renders
// them identically, so the verdict has to be published beside it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_answer_says_which_fix_may_be_applied_unattended() {
    let _guard = TEST_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    project(dir.path());
    let client = client(dir.path()).await;

    let body = diagnostics(
        &client,
        json!({
            "action": "snippet",
            "code": "Процедура Проба()\n\tА = 1;\n\tА = А;\n\tБ = 2;;\n\tСообщить(А + Б);\nКонецПроцедуры\n",
            "min_severity": "hint",
            "detail": "detailed",
        }),
    )
    .await;
    let findings = body["result"]["findings"]
        .as_array()
        .unwrap_or_else(|| panic!("findings in {body}"))
        .clone();
    let by_code = |code: &str| -> serde_json::Value {
        findings
            .iter()
            .find(|f| f["code"] == code)
            .unwrap_or_else(|| panic!("a {code} finding in {body}"))
            .clone()
    };

    let empty_statement = by_code("EmptyStatement");
    assert_eq!(empty_statement["has_fix"], json!(true), "{body}");
    assert_eq!(empty_statement["fix_auto_applicable"], json!(true), "{body}");
    assert_eq!(empty_statement["fix"]["auto_applicable"], json!(true), "{body}");

    let self_assign = by_code("SelfAssign");
    assert_eq!(self_assign["has_fix"], json!(true), "{body}");
    assert_eq!(self_assign["fix_auto_applicable"], json!(false), "{body}");
    assert_eq!(self_assign["fix"]["auto_applicable"], json!(false), "{body}");

    client.cancel().await.expect("session closed");
}
