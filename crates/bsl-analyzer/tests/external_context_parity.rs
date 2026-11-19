mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use rmcp::ServiceExt;
use serde_json::{json, Value};

const COMPATIBILITY: &str = "PlatformMemberHiddenByCompatibilityMode";
const ENVIRONMENT: &str = "UnavailableInEnvironment";
const CODE: &str = "Процедура Проверить()\n Части = СтрРазделить(\"a,b\", \",\");\n Параметр = ПараметрЗапуска;\n ЗавершитьРаботуСистемы();\nКонецПроцедуры\n";
const GUARD: Duration = Duration::from_secs(60);
type Findings = Vec<(String, String)>;

struct IsolatedEnvironment(Vec<(std::ffi::OsString, std::ffi::OsString)>);

impl IsolatedEnvironment {
    fn new(cache: &Path) -> Self {
        // This executable has one test; isolate its in-process MCP and both child transports.
        let saved = std::env::vars_os()
            .filter(|(key, _)| {
                let key = key.to_string_lossy().to_ascii_uppercase();
                key.starts_with("PG")
                    || key.starts_with("EMBEDDING_")
                    || key.starts_with("BSL_SEARCH_")
                    || key.starts_with("BSL_MCP_EFFECTIVE_EMBEDDING_")
                    || key.starts_with("BSL_ONEC_")
                    || (key.starts_with("BSL_PLATFORM_HELP_")
                        && !(cfg!(corpus_contract) && key == bsl_platform::CORPUS_ENV))
                    || matches!(
                        key.as_str(),
                        "DATABASE_URL"
                            | "ONEC_CONFIGURATIONS_ROOT"
                            | "BSL_TEST_EMBEDDING"
                            | "BSL_CACHE_DIR"
                    )
            })
            .collect::<Vec<_>>();
        for (key, _) in &saved {
            std::env::remove_var(key);
        }
        std::env::set_var("BSL_CACHE_DIR", cache);
        Self(saved)
    }
}

impl Drop for IsolatedEnvironment {
    fn drop(&mut self) {
        std::env::remove_var("BSL_CACHE_DIR");
        for (key, value) in &self.0 {
            std::env::set_var(key, value);
        }
    }
}

fn write(root: &Path, relative: &str, content: &str) {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let content = if relative == "bsl-analyzer.toml" {
        format!("{content}\n[search.baseline]\nbackend='sqlite'\n")
    } else {
        content.to_owned()
    };
    std::fs::write(path, content).unwrap();
}

fn configuration(root: &Path, mode: &str, compatibility: &str) {
    write(root, "Configuration.xml", &format!("<MetaDataObject><Configuration uuid='00000000-0000-0000-0000-000000000001'><Properties><Name>Main</Name><DefaultRunMode>{mode}</DefaultRunMode><CompatibilityMode>{compatibility}</CompatibilityMode><UseOrdinaryFormInManagedApplication>false</UseOrdinaryFormInManagedApplication></Properties></Configuration></MetaDataObject>"));
    write(root, "Catalogs/Common.xml", "<MetaDataObject><Catalog uuid='00000000-0000-0000-0000-000000000002'><Properties><Name>Common</Name></Properties></Catalog></MetaDataObject>");
}

fn external(root: &Path, directory: &str, content: &str) -> PathBuf {
    write(root, &format!("{directory}/Shared.xml"), "<MetaDataObject><ExternalDataProcessor uuid='00000000-0000-0000-0000-000000000003'><Properties><Name>Shared</Name></Properties></ExternalDataProcessor></MetaDataObject>");
    let path = format!("{directory}/Shared/Ext/ObjectModule.bsl");
    write(root, &path, content);
    root.join(path)
}

fn findings(values: &[Value]) -> Findings {
    let mut result = values
        .iter()
        .filter_map(|value| {
            let code = value["code"].as_str()?;
            matches!(code, COMPATIBILITY | ENVIRONMENT | "ParseError")
                .then(|| (code.to_owned(), value["message"].as_str().unwrap().to_owned()))
        })
        .collect::<Vec<_>>();
    result.sort();
    result
}

fn cli(root: &Path, subjects: &[PathBuf]) -> BTreeMap<PathBuf, Findings> {
    let output = Command::new(common::qualified_app())
        .current_dir(root)
        .args(["analyze", "-s", ".", "--format", "jsonl", "-q"])
        .env_remove("ONEC_CONFIGURATIONS_ROOT")
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let mut files = BTreeMap::new();
    for row in String::from_utf8(output.stdout).unwrap().lines() {
        let event: Value = serde_json::from_str(row).unwrap();
        if event["type"] != "file" {
            continue;
        }
        let path = PathBuf::from(event["path"].as_str().unwrap());
        let path = if path.is_absolute() { path } else { root.join(path) };
        let path = path.canonicalize().unwrap();
        if !subjects.iter().any(|subject| subject.canonicalize().unwrap() == path) {
            continue;
        }
        assert!(event["error"].is_null(), "{event}");
        assert!(
            files.insert(path, findings(event["diagnostics"].as_array().unwrap())).is_none(),
            "a subject was analyzed twice"
        );
    }
    assert_eq!(files.len(), subjects.len(), "every external subject is analyzed");
    files
}

async fn parity(root: &Path, subjects: &[PathBuf]) -> BTreeMap<PathBuf, Findings> {
    let expected = cli(root, subjects);
    let state = mcp_server::SharedState::workspace(root.to_path_buf()).unwrap();
    let server = mcp_server::McpServer::new(mcp_server::McpProfile::Workspace, state.clone());
    let (client_io, server_io) = tokio::io::duplex(1024 * 1024);
    let task = tokio::spawn(mcp_server::serve_stream(server, server_io));
    let client = tokio::time::timeout(GUARD, ().serve(client_io)).await.unwrap().unwrap();
    let mut command = Command::new(common::qualified_app());
    command.args(["lsp", "--mode", "stdio"]);
    let mut lsp = common::Lsp::start_command(command, root, json!({}), common::SILENCE);
    lsp.wait_for(|message| {
        message["method"] == "$/progress"
            && message["params"]["token"].as_str().is_some_and(|token| token.ends_with("/Loading"))
            && message["params"]["value"]["kind"] == "end"
    });
    for (index, subject) in subjects.iter().enumerate() {
        let deadline = tokio::time::Instant::now() + GUARD;
        let mcp = loop {
            let result = tokio::time::timeout_at(
                deadline,
                client.call_tool(
                    rmcp::model::CallToolRequestParams::new("diagnostics").with_arguments(
                        json!({"action":"file","path":subject,"min_severity":"hint"})
                            .as_object()
                            .unwrap()
                            .clone(),
                    ),
                ),
            )
            .await
            .unwrap()
            .unwrap()
            .structured_content
            .unwrap();
            if result["status"] != "loading" {
                break result;
            }
            assert!(tokio::time::Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        let actual = findings(mcp["result"]["findings"].as_array().expect("MCP file findings"));
        let expected_file = &expected[&subject.canonicalize().unwrap()];
        assert_eq!(&actual, expected_file, "MCP must use the same owning context: {subject:?}");
        let text = std::fs::read_to_string(subject).unwrap();
        lsp.open(subject, &text);
        let uri = lsp_types::Url::from_file_path(subject).unwrap();
        let deadline = Instant::now() + GUARD;
        let mut id = 100 + index as u64 * 100;
        let actual = loop {
            id += 1;
            lsp.send(json!({"jsonrpc":"2.0","id":id,"method":"textDocument/diagnostic","params":{"textDocument":{"uri":uri}}}));
            let response = lsp
                .wait_for_within(deadline.saturating_duration_since(Instant::now()), |message| {
                    message["id"] == id && message["method"].is_null()
                })
                .expect("LSP context diagnostics must finish");
            if response["error"]["code"] == -32801 {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(20));
                continue;
            }
            assert!(response["error"].is_null(), "{response}");
            break findings(
                response["result"]["items"].as_array().expect("full diagnostic report"),
            );
        };
        assert_eq!(&actual, expected_file, "LSP must use the same owning context: {subject:?}");
    }
    assert!(lsp.shutdown(9000).success());
    client.cancel().await.unwrap();
    tokio::time::timeout(GUARD, task).await.unwrap().unwrap().unwrap();
    state.shutdown();
    expected
}

fn count(findings: &Findings, code: &str) -> usize {
    findings.iter().filter(|finding| finding.0 == code).count()
}

#[test]
fn explicit_legacy_default_and_unknown_external_owners_have_cli_lsp_mcp_parity() {
    #[cfg(corpus_contract)]
    {
        let path = std::env::var_os(bsl_platform::CORPUS_ENV)
            .expect("corpus_contract requires the platform help corpus path");
        let metadata = std::fs::metadata(PathBuf::from(path))
            .expect("the platform help corpus must be readable");
        assert!(metadata.is_file() && metadata.len() > 0, "the corpus must be a nonempty file");
    }
    common::qualified_app();
    let cache = tempfile::tempdir().unwrap();
    let _environment = IsolatedEnvironment::new(cache.path());
    // Reverse drop order also stops runtime workers before restoring the environment on panic.
    let runtime =
        tokio::runtime::Builder::new_multi_thread().worker_threads(4).enable_all().build().unwrap();
    runtime.block_on(external_context_parity());
    drop(runtime);
}

async fn external_context_parity() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    configuration(&root.join("modern"), "ManagedApplication", "Version8_3_27");
    configuration(&root.join("legacy"), "OrdinaryApplication", "Version8_2_16");
    let legacy = external(root, "legacy-external", CODE);
    let modern = external(root, "modern-external", CODE);
    write(root, "bsl-analyzer.toml", "[source]\nroot='modern'\nexternals=[{name='Legacy',path='legacy-external',configurationRoot='legacy'},{name='Modern',path='modern-external',configuration_root='modern'}]\n");
    let results = parity(root, &[legacy.clone(), modern.clone()]).await;
    let old = &results[&legacy.canonicalize().unwrap()];
    let new = &results[&modern.canonicalize().unwrap()];
    assert_eq!(count(old, COMPATIBILITY), 1);
    assert!(old.iter().any(|finding| finding.0 == COMPATIBILITY && finding.1.contains("8.2.16")));
    assert_eq!(count(new, COMPATIBILITY), 0);
    assert_eq!(count(old, ENVIRONMENT), 0);
    assert!(count(new, ENVIRONMENT) > 0);

    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    configuration(&root.join("main"), "OrdinaryApplication", "Version8_2_16");
    let legacy_default = external(root, "external", CODE);
    write(
        root,
        "bsl-analyzer.toml",
        "[source]\nroot='main'\nexternals=[{name='Default',path='external'}]\n",
    );
    let results = parity(root, std::slice::from_ref(&legacy_default)).await;
    let result = &results[&legacy_default.canonicalize().unwrap()];
    assert_eq!(count(result, COMPATIBILITY), 1, "old configurations retain their default");
    assert_eq!(count(result, ENVIRONMENT), 0);

    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let unknown = external(root, "external", &format!("{CODE}Процедура Ошибка(\n"));
    write(root, "bsl-analyzer.toml", "[source]\nexternals=[{name='Unknown',path='external'}]\n");
    let results = parity(root, std::slice::from_ref(&unknown)).await;
    let result = &results[&unknown.canonicalize().unwrap()];
    assert_eq!(count(result, COMPATIBILITY), 0);
    assert_eq!(count(result, ENVIRONMENT), 0, "unknown owner is not evidence of unavailable APIs");
    assert!(count(result, "ParseError") > 0, "unknown context still receives syntax diagnostics");
}
