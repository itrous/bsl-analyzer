use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

use rmcp::ServiceExt;
use serde_json::{json, Value};

use crate::{McpProfile, McpServer, SharedState, WorkspaceCacheLayout};

const SOURCE: &str = include_str!("fixtures/occurrence_identity.bsl");
const GUARD: Duration = Duration::from_secs(15);

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sdk_diagnostics_preserves_occurrence_identity_and_consumer_collisions() {
    let parsed = parser::parse(SOURCE);
    assert!(parsed.errors().is_empty(), "{:?}", parsed.errors());
    let literals = parsed
        .syntax_node()
        .descendants_with_tokens()
        .filter_map(|element| element.into_token())
        .filter(|token| token.text() == "25")
        .collect::<Vec<_>>();
    assert_eq!(literals.len(), 4);
    let in_calls = literals
        .iter()
        .map(|token| {
            let ancestors = token.parent().unwrap().ancestors().collect::<Vec<_>>();
            assert!(ancestors.iter().any(|node| node.kind() == syntax::SyntaxKind::BINARY_EXPR));
            assert!(ancestors.iter().any(|node| node.kind() == syntax::SyntaxKind::IF_STMT));
            ancestors.iter().any(|node| node.kind() == syntax::SyntaxKind::CALL_EXPR)
        })
        .collect::<Vec<_>>();
    assert_eq!(in_calls, [true, false, true, false]);

    let directory = tempfile::tempdir().unwrap();
    let cache_directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let relative = "cf/CommonModules/Occurrence/Ext/Module.bsl";
    let file = root.join(relative);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, SOURCE).unwrap();
    std::fs::write(
        root.join("bsl-analyzer.toml"),
        "[source]\nroot = 'cf'\nextensions = []\nexternals = []\n",
    )
    .unwrap();
    std::fs::write(root.join("cf/Configuration.xml"), "<MetaDataObject><Configuration uuid='00000000-0000-0000-0000-000000000001'><Properties><Name>Fixture</Name></Properties></Configuration></MetaDataObject>").unwrap();
    std::fs::write(root.join("cf/CommonModules/Occurrence.xml"), "<MetaDataObject><CommonModule uuid='00000000-0000-0000-0000-000000000002'><Properties><Name>Occurrence</Name><Global>false</Global><Server>true</Server><ClientManagedApplication>false</ClientManagedApplication><ClientOrdinaryApplication>false</ClientOrdinaryApplication><ExternalConnection>false</ExternalConnection><ServerCall>false</ServerCall><Privileged>false</Privileged><ReturnValuesReuse>DontUse</ReturnValuesReuse></Properties></CommonModule></MetaDataObject>").unwrap();
    let project = crate::project::at(root).unwrap();
    let cache =
        WorkspaceCacheLayout::for_project(&project, Some(cache_directory.path()), root, None)
            .unwrap();
    let state =
        SharedState::shared_workspace_with_cache_and_prefixes(root.to_path_buf(), cache, None)
            .unwrap();
    state.diagnostics().ensure_loading();
    crate::diagnostics_state::test_support::wait_ready(state.diagnostics());
    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    let stop = tokio_util::sync::CancellationToken::new();
    let _stop_guard = stop.clone().drop_guard();
    let server =
        McpServer::new(McpProfile::Workspace, state.clone()).for_session(Default::default());
    let server_task =
        tokio::spawn(crate::serve_stream_with_shutdown(server, server_io, stop.clone()));
    let client = tokio::time::timeout(GUARD, ().serve(client_io)).await.unwrap().unwrap();
    tokio::time::timeout(GUARD, async {
        while state.graph().is_reloading() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let response = tokio::time::timeout(
        GUARD,
        client.call_tool(rmcp::model::CallToolRequestParams::new("diagnostics").with_arguments(
            json!({"action":"file","path":file,"min_severity":"hint","codes":["MagicNumber","SelfAssign"],"max_findings":100,"max_output_tokens":16000})
                .as_object().unwrap().clone(),
        )),
    )
    .await
    .unwrap()
    .unwrap();
    if let Some(output) = std::env::var_os("BSL_OCCURRENCE_EVIDENCE_DIR") {
        let output = std::path::PathBuf::from(output);
        std::fs::create_dir_all(&output).unwrap();
        std::fs::write(output.join("raw-mcp.json"), serde_json::to_vec_pretty(&response).unwrap())
            .unwrap();
    }
    assert!(!response.is_error.unwrap_or(false));
    let body = response.structured_content.as_ref().expect("structured diagnostics");
    let text: Value = serde_json::from_str(&response.content[0].as_text().unwrap().text).unwrap();
    assert_eq!(&text, body);
    assert!(body["revision"].is_number());
    assert_eq!(body["stale"], false);
    let result = &body["result"];
    assert!(result.get("error").is_none(), "{body:#}");
    assert_eq!(result["truncated"], false);
    let findings = result["findings"].as_array().unwrap();
    assert_eq!(findings.len(), 5, "{body:#}");
    let counts: u64 = ["error", "warning", "info", "hint"]
        .iter()
        .map(|severity| result["counts"][*severity].as_u64().unwrap_or(0))
        .sum();
    assert_eq!(counts, findings.len() as u64);
    let mut identities = HashSet::new();
    let mut consumer_fingerprints = HashSet::new();
    let mut positions = Vec::new();
    for finding in findings {
        let range = &finding["range"];
        let identity = json!([
            relative,
            body["revision"],
            body["freshness"],
            range,
            finding["code"],
            finding["message"]
        ]);
        assert!(identities.insert(identity.to_string()), "duplicate raw identity: {finding}");
        let line = range["start_line"].as_u64().unwrap();
        let consumer = BTreeMap::from([
            ("path", json!(relative)),
            ("line", json!(line + 1)),
            ("rule", finding["code"].clone()),
            ("message", finding["message"].clone()),
        ]);
        consumer_fingerprints.insert(bsl_metadata::rename_xml::metadata_source_fingerprint(
            &serde_json::to_vec(&consumer).unwrap(),
        ));
        if finding["code"] == "MagicNumber" {
            positions.push((
                line,
                range["start_column"].as_u64().unwrap(),
                range["end_line"].as_u64().unwrap(),
                range["end_column"].as_u64().unwrap(),
            ));
        } else {
            assert_eq!(finding["code"], "SelfAssign");
            assert_eq!(line, 9);
        }
    }
    positions.sort();
    assert_eq!(positions, [(2, 37, 2, 39), (2, 43, 2, 45), (6, 37, 6, 39), (6, 43, 6, 45)]);
    assert_eq!(identities.len(), 5);
    assert_eq!(consumer_fingerprints.len(), 3);
    client.cancel().await.unwrap();
    stop.cancel();
    tokio::time::timeout(GUARD, server_task).await.unwrap().unwrap().unwrap();
    state.shutdown();
}
