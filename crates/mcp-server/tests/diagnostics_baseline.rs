use std::path::Path;
use std::time::Duration;

use ide::diagnostics_baseline::{
    diagnostic_fingerprint, diagnostics_baseline_json, normalize_diagnostic_snippet,
    DiagnosticsBaseline, DiagnosticsBaselineEntry, DiagnosticsBaselineRange,
    DiagnosticsBaselineScope, DIAGNOSTICS_BASELINE_SCHEMA_VERSION,
};
use mcp_server::{serve_stream, McpProfile, McpServer, SharedState};
use rmcp::model::{CallToolRequest, CallToolRequestParams, ClientRequest};
use rmcp::service::{PeerRequestOptions, RunningService};
use rmcp::{RoleClient, ServiceExt};
use serde_json::{json, Map, Value};

#[path = "../../bsl-analyzer/tests/fixtures/diagnostic_rules/mod.rs"]
mod diagnostic_rules;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn diagnostic_rules_resident_preserves_occurrence_ranges_and_reloads_parameters() {
    let dir = tempfile::tempdir().unwrap();
    let source = diagnostic_rules::project(dir.path());
    let client = client(dir.path()).await;
    let mut arguments = args("file");
    arguments.insert("path".to_owned(), json!("src/Module.bsl"));
    arguments.insert("min_severity".to_owned(), json!("hint"));
    let mut revision = 0;
    for _ in 0..2 {
        let body = diagnostics(&client, arguments.clone()).await;
        assert_eq!(
            diagnostic_rules::legacy_ranges(body["result"]["findings"].as_array().unwrap()),
            diagnostic_rules::expected(&source, false, true)
        );
        revision = body["revision"].as_u64().expect("revision");
    }
    diagnostic_rules::replace_config(dir.path(), 7);
    // A config load rebuilds the resident and so raises its revision: the reloaded answer is
    // the first one that is both newer than every answer before the change and carries the
    // changed parameter.
    let mut patience = common::Patience::new(common::STATE_CEILING, Duration::from_millis(20));
    let reloaded = loop {
        let body = diagnostics(&client, arguments.clone()).await;
        if body["revision"].as_u64().expect("revision") > revision
            && diagnostic_rules::legacy_ranges(body["result"]["findings"].as_array().unwrap())
                == diagnostic_rules::expected(&source, false, false)
        {
            break body["revision"].as_u64().unwrap();
        }
        assert!(patience.allows(), "resident config was not reloaded: {body}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    // A request that reloaded the config would raise the revision by one each time, so this
    // many outgrow the rebuilds still settling after the first reloaded answer.
    let mut last = reloaded;
    for _ in 0..diagnostic_rules::PULLS_PER_LOAD_BOUND {
        let body = diagnostics(&client, arguments.clone()).await;
        assert_eq!(
            diagnostic_rules::legacy_ranges(body["result"]["findings"].as_array().unwrap()),
            diagnostic_rules::expected(&source, false, false)
        );
        last = body["revision"].as_u64().unwrap();
    }
    assert!(
        last - reloaded < diagnostic_rules::PULLS_PER_LOAD_BOUND as u64,
        "requests reloaded the changed config: revision {reloaded} -> {last}"
    );
    client.cancel().await.unwrap();
}

// Shared helpers for the gates in this binary; not every binary uses every one.
#[allow(dead_code)]
mod common;

type Client = RunningService<RoleClient, ()>;

fn write(root: &Path, relative: &str, contents: &str) {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

fn write_module(root: &Path, name: &str, body: &str) {
    write(
        root,
        &format!("CommonModules/{name}.xml"),
        &format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses">
  <CommonModule uuid="00000000-0000-0000-0000-0000000000{id:02}">
    <Properties><Name>{name}</Name><Server>true</Server></Properties>
  </CommonModule>
</MetaDataObject>"#,
            id = name.len()
        ),
    );
    write(root, &format!("CommonModules/{name}/Ext/Module.bsl"), body);
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

/// A workspace answer that has caught up with the unreadable file: polls until the tree is
/// stale and the unreadable file is a hole in the baseline, or gives back the last answer for
/// the assertion to fail on.
///
/// Staleness alone is not that fact. The edit that precedes the permission change is seen as
/// drift first, and the answer is stale with the baseline still `full` until the read failure
/// is observed as a hole; a hole, once it exists, keeps the answer `partial` until it heals.
///
/// Gated like its only caller: the unreadable-file case it serves needs a permission bit
/// Windows does not have, and an ungated helper is dead code there — which the mirror's
/// Windows job, unlike ours, builds with warnings denied and so reports as an error.
#[cfg(unix)]
async fn workspace_until_partial_stale(client: &Client) -> Value {
    let mut deadline =
        common::Patience::new(common::STATE_CEILING, std::time::Duration::from_millis(20));
    loop {
        let body = diagnostics(client, args("workspace")).await;
        let settled = body["stale"] == true && body["result"]["baseline"]["state"] == "partial";
        if settled || !deadline.allows() {
            return body;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn diagnostics(client: &Client, arguments: Map<String, Value>) -> Value {
    let mut deadline =
        common::Patience::new(common::STATE_CEILING, std::time::Duration::from_millis(20));
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

/// Asks the workspace until it counts both modules of the stand.
async fn workspace_until_both_modules(client: &Client) {
    let mut patience = common::Patience::new(common::STATE_CEILING, Duration::from_millis(20));
    loop {
        let body = diagnostics(client, args("workspace")).await;
        if body["result"]["files_total"] == 2 {
            return;
        }
        assert!(patience.allows(), "the workspace never counted both modules: {body}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn args(action: &str) -> Map<String, Value> {
    Map::from_iter([("action".to_owned(), json!(action))])
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn baseline_parity_partial_stale_error_recovery_and_cancellation() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let first_rel = "CommonModules/Первый/Ext/Module.bsl";
    let first_body = "&НаСервере\nФункция Считать() Экспорт Возврат 1; КонецФункции\n";
    write_module(root, "Первый", first_body);
    write_module(root, "Второй", "Процедура Выполнить() Экспорт\n    А = 1;\nКонецПроцедуры\n");
    write(root, "bsl-analyzer.toml", "[diagnostics.baseline]\npath = \"baseline.json\"\n");
    let baseline_path = root.join("baseline.json");
    let mut baseline = DiagnosticsBaseline {
        schema_version: DIAGNOSTICS_BASELINE_SCHEMA_VERSION,
        scope: DiagnosticsBaselineScope { source_root: None, extensions: vec![] },
        diagnostics: vec![],
    };
    std::fs::write(&baseline_path, diagnostics_baseline_json(&baseline).unwrap()).unwrap();

    let client = client(root).await;
    let mut file_args = args("file");
    file_args.insert("path".to_owned(), json!(first_rel));
    file_args.insert("min_severity".to_owned(), json!("hint"));
    file_args.insert("detail".to_owned(), json!("detailed"));
    let initial = diagnostics(&client, file_args.clone()).await;
    let finding = initial["result"]["findings"].as_array().unwrap().first().unwrap();
    let line = finding["range"]["start_line"].as_u64().unwrap() as usize;
    let snippet = normalize_diagnostic_snippet(first_body.lines().nth(line).unwrap());
    let code = finding["code"].as_str().unwrap().to_owned();
    baseline.diagnostics.push(DiagnosticsBaselineEntry {
        fingerprint: diagnostic_fingerprint(first_rel, &code, &snippet, 0),
        path: first_rel.to_owned(),
        code,
        snippet,
        occurrence: 0,
        message: finding["message"].as_str().unwrap().to_owned(),
        severity: finding["internal_severity"].as_str().unwrap().to_owned(),
        range: DiagnosticsBaselineRange {
            start_line: finding["range"]["start_line"].as_u64().unwrap() as u32,
            start_column: finding["range"]["start_column"].as_u64().unwrap() as u32,
            end_line: finding["range"]["end_line"].as_u64().unwrap() as u32,
            end_column: finding["range"]["end_column"].as_u64().unwrap() as u32,
        },
    });
    let valid = diagnostics_baseline_json(&baseline).unwrap();
    std::fs::write(&baseline_path, &valid).unwrap();
    let classified = diagnostics(&client, file_args.clone()).await;
    assert_eq!(classified["result"]["baseline"]["known"], 1);
    assert_eq!(
        classified["result"]["findings"].as_array().unwrap().len() + 1,
        initial["result"]["findings"].as_array().unwrap().len()
    );

    // `partial` is the answer for a sweep cut short by the file cap, which needs a second file
    // to cut. The resident can report ready before it has counted both modules, so the cap is
    // applied only once the workspace has said it sees both.
    workspace_until_both_modules(&client).await;
    let mut workspace_args = args("workspace");
    workspace_args.insert("min_severity".to_owned(), json!("hint"));
    workspace_args.insert("max_files".to_owned(), json!(1));
    let limited = diagnostics(&client, workspace_args.clone()).await;
    assert_eq!(limited["result"]["baseline"]["state"], "partial");
    assert_eq!(limited["result"]["baseline"]["complete"], false);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let source = root.join(first_rel);
        let original = std::fs::metadata(&source).unwrap().permissions();
        std::fs::write(&source, format!("{first_body}// drift\n")).unwrap();
        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o000)).unwrap();
        // Mode bits do not stop a privileged user, and the suite does run as root in
        // CI. Without this check the assertions below would pass vacuously there —
        // the file stays readable, the resident reconciles, and nothing is asserted
        // about an unreadable file at all.
        if std::fs::read(&source).is_err() {
            // The edit above is announced by the watcher, not by the write returning, so
            // the first answer can legitimately predate it: the resident is still at the
            // pre-edit generation with nothing drifted and no hole. Wait for the fact
            // instead of racing it — once the unreadable file IS observed it becomes a
            // hole, and a hole keeps the answer stale until it heals, so this settles in
            // one direction only.
            let stale = workspace_until_partial_stale(&client).await;
            assert_eq!(
                stale["stale"], true,
                "the unreadable file has to make the answer stale: {stale}"
            );
            assert_eq!(stale["result"]["baseline"]["state"], "partial", "{stale}");
            assert_eq!(stale["result"]["baseline"]["resolved"], 0);
        } else {
            eprintln!("skipping the unreadable-file leg: mode 0o000 is not an obstacle here");
        }
        std::fs::set_permissions(&source, original).unwrap();
        std::fs::write(&source, first_body).unwrap();
    }

    std::fs::write(&baseline_path, b"{broken").unwrap();
    let broken = diagnostics(&client, file_args.clone()).await;
    assert_eq!(broken["result"]["baseline"]["state"], "error");
    assert!(broken["result"].get("findings").is_none());
    std::fs::write(&baseline_path, &valid).unwrap();
    let recovered = diagnostics(&client, file_args.clone()).await;
    assert_eq!(recovered["result"]["baseline"]["known"], 1);

    let request = ClientRequest::CallToolRequest(CallToolRequest::new(
        CallToolRequestParams::new("diagnostics").with_arguments(args("workspace")),
    ));
    client
        .peer()
        .send_cancellable_request(request, PeerRequestOptions::no_options())
        .await
        .expect("cancellable request")
        .cancel(Some("integration cancellation".to_owned()))
        .await
        .expect("cancellation notification");
    let after_cancel = diagnostics(&client, file_args).await;
    assert_eq!(after_cancel["result"]["baseline"]["known"], 1);

    client.cancel().await.expect("session closed");
}
