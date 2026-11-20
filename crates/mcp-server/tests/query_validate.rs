//! `query(validate)` over a real MCP session.
//!
//! The things worth pinning here cannot be seen from inside the tool: whether the answer
//! actually reaches `workspace_semantics` once the resident is up, whether the degraded answer
//! still honours the project's own configuration, and whether the tool leaves the resident in
//! a state it can come back from. An in-process call skips the lifecycle that decides all
//! three.

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

/// The query names a field the catalog does not have. Without metadata nothing can say so;
/// with metadata `UnknownFieldInQuery` must appear. That difference is the whole point of the
/// task, so it is what the tests measure — a query both paths treat identically would make
/// every assertion below pass on a tool that never loaded metadata at all.
const UNKNOWN_FIELD: &str = "ВЫБРАТЬ Т.НетТакогоПоля КАК П ИЗ Справочник.Товары КАК Т";

/// Structural, resolver-free. Present under both backends, so it proves the degraded answer is
/// not simply empty.
const JOIN_WITH_SUBQUERY: &str = "ВЫБРАТЬ Т.Наименование КАК П ИЗ Справочник.Товары КАК Т \
     ВНУТРЕННЕЕ СОЕДИНЕНИЕ (ВЫБРАТЬ 1 КАК Ч) КАК В ПО ИСТИНА";

fn write(root: &Path, relative: &str, contents: &str) {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

fn stage(root: &Path) {
    write(
        root,
        "Configuration.xml",
        r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
  <Configuration uuid="00000000-0000-0000-0000-0000000000aa">
    <Properties><Name>ТестоваяКонфигурация</Name></Properties>
  </Configuration>
</MetaDataObject>"#,
    );
    write(
        root,
        "Catalogs/Товары.xml",
        r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
  <Catalog uuid="00000000-0000-0000-0000-0000000000bb">
    <Properties><Name>Товары</Name></Properties>
  </Catalog>
</MetaDataObject>"#,
    );
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

async fn validate(client: &Client, arguments: Map<String, Value>) -> Value {
    client
        .call_tool(CallToolRequestParams::new("query").with_arguments(arguments))
        .await
        .expect("query call")
        .structured_content
        .expect("validate answers with a structured envelope")
}

fn args(query: &str) -> Map<String, Value> {
    json!({ "action": "validate", "query": query }).as_object().cloned().expect("object")
}

fn local(envelope: &Value) -> &Value {
    &envelope["results"][0]
}

fn codes(envelope: &Value) -> Vec<String> {
    local(envelope)["diagnostics"]
        .as_array()
        .expect("diagnostics array")
        .iter()
        .map(|d| d["code"].as_str().unwrap_or_default().to_string())
        .collect()
}

/// Keeps asking until the answer stops being parse-only. Written as a poll rather than a
/// single call because the tool deliberately never waits for the resident.
async fn validate_until_semantic(client: &Client, query: &str) -> Value {
    let mut deadline = common::Patience::new(
        std::time::Duration::from_secs(90),
        std::time::Duration::from_millis(200),
    );
    loop {
        let envelope = validate(client, args(query)).await;
        if local(&envelope)["backend"] == "workspace_semantics" {
            return envelope;
        }
        assert!(deadline.allows(), "never reached workspace_semantics: {envelope}",);
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_ready_workspace_answers_with_metadata_semantics() {
    let dir = tempfile::tempdir().expect("tempdir");
    stage(dir.path());
    let client = client(dir.path()).await;

    let envelope = validate_until_semantic(&client, UNKNOWN_FIELD).await;

    assert!(
        codes(&envelope).iter().any(|c| c == "UnknownFieldInQuery"),
        "a ready workspace must catch the unknown field: {envelope}",
    );
    assert!(
        local(&envelope)["degraded_reason"].is_null(),
        "a complete answer states no reason to be incomplete: {envelope}",
    );
    assert_eq!(local(&envelope)["metadata_checked"], true, "{envelope}");
    assert_eq!(local(&envelope)["status"], "checked_against_metadata", "{envelope}");
}

/// The first call happens while the resident is still cold, so it is the degraded answer —
/// and it must be honest about that AND still carry the rules that need no metadata. A test
/// asserting only "no UnknownFieldInQuery" would pass on a tool that returned nothing at all.
#[tokio::test(flavor = "multi_thread")]
async fn the_first_call_degrades_without_claiming_completeness() {
    let dir = tempfile::tempdir().expect("tempdir");
    stage(dir.path());
    let client = client(dir.path()).await;

    let first = validate(&client, args(JOIN_WITH_SUBQUERY)).await;

    if local(&first)["backend"] == "parser" {
        assert!(
            local(&first)["degraded_reason"].is_string(),
            "a parser-backed answer must say why: {first}",
        );
        assert!(
            !codes(&first).iter().any(|c| c == "UnknownFieldInQuery"),
            "a metadata-dependent code cannot appear without metadata: {first}",
        );
        assert!(
            codes(&first).iter().any(|c| c == "JoinWithSubQuery"),
            "the structural rules still apply while metadata loads: {first}",
        );
        // Said in words as well as by `backend`: a caller that only looks for an empty list of
        // findings must not take this answer for a check against metadata.
        assert_eq!(local(&first)["metadata_checked"], false, "{first}");
        assert_eq!(local(&first)["status"], "not_checked_against_metadata", "{first}");
    }

    // Whatever the first call saw, the tool must have kicked the build — otherwise a workspace
    // that went idle would never serve semantics again.
    let ready = validate_until_semantic(&client, JOIN_WITH_SUBQUERY).await;
    assert!(
        codes(&ready).iter().any(|c| c == "JoinWithSubQuery"),
        "the structural rule survives the move to full semantics: {ready}",
    );
}

/// The project's configuration is the tool's configuration, in BOTH completeness states.
/// Reading it from the resident would resurrect a disabled rule exactly on the degraded path,
/// where no resident exists — so the disabled rule here is a structural one, reachable
/// without metadata, and the assertion runs on the very first (cold) call as well as after.
#[tokio::test(flavor = "multi_thread")]
async fn a_rule_disabled_in_the_project_stays_disabled_in_both_states() {
    let dir = tempfile::tempdir().expect("tempdir");
    stage(dir.path());
    write(dir.path(), "bsl-analyzer.toml", "[diagnostics.parameters]\nJoinWithSubQuery = false\n");
    let client = client(dir.path()).await;

    let cold = validate(&client, args(JOIN_WITH_SUBQUERY)).await;
    assert!(
        !codes(&cold).iter().any(|c| c == "JoinWithSubQuery"),
        "[{}] a rule the project turned off must stay off: {cold}",
        local(&cold)["backend"],
    );

    let warm = validate_until_semantic(&client, JOIN_WITH_SUBQUERY).await;
    assert!(
        !codes(&warm).iter().any(|c| c == "JoinWithSubQuery"),
        "the same rule must stay off once metadata is ready: {warm}",
    );
}

/// The control for the test above: without it, a tool that reported nothing at all would look
/// like a tool that honours the configuration.
#[tokio::test(flavor = "multi_thread")]
async fn the_same_rule_fires_when_the_project_leaves_it_on() {
    let dir = tempfile::tempdir().expect("tempdir");
    stage(dir.path());
    let client = client(dir.path()).await;

    let envelope = validate(&client, args(JOIN_WITH_SUBQUERY)).await;
    assert!(
        codes(&envelope).iter().any(|c| c == "JoinWithSubQuery"),
        "with no override the rule must fire — otherwise the disable test is vacuous: {envelope}",
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unregistered_root_id_fails_the_call_instead_of_degrading() {
    let dir = tempfile::tempdir().expect("tempdir");
    stage(dir.path());
    let client = client(dir.path()).await;

    let mut arguments = args(UNKNOWN_FIELD);
    arguments.insert("root_id".into(), json!("нет-такого-корня"));

    let result =
        client.call_tool(CallToolRequestParams::new("query").with_arguments(arguments)).await;

    assert!(result.is_err(), "an unknown root_id is a bad request, not a reason to degrade");
}

/// The two queries of issue #82, byte for byte: D, where the derived table in `ИЗ` names the
/// alias of the query around it, and C, the same name inside a correlated `ГДЕ ... В (...)`.
const DERIVED_TABLE_NAMES_OUTER_ALIAS: &str =
    include_str!("../../ide-diagnostics/tests/fixtures/derived_table_outer_alias/d.sdbl");
const CORRELATED_IN_SUBQUERY: &str =
    include_str!("../../ide-diagnostics/tests/fixtures/derived_table_outer_alias/c.sdbl");

const UNAVAILABLE_ALIAS: &str = "UnavailableAliasInQuery";

fn diagnostics_of<'a>(envelope: &'a Value, code: &str) -> Vec<&'a Value> {
    local(envelope)["diagnostics"]
        .as_array()
        .expect("diagnostics array")
        .iter()
        .filter(|d| d["code"] == code)
        .collect()
}

/// Zero-based line and character column of the first `needle` in `text`.
fn position_of(text: &str, needle: &str) -> (usize, usize) {
    let before = &text[..text.find(needle).expect("the needle is in the text")];
    (before.matches('\n').count(), before.rsplit('\n').next().unwrap_or("").chars().count())
}

/// One answer per query, whatever backend gave it: the rule reads the query's own text, so the
/// degraded answer of a cold resident and the one with metadata must agree. No waiting for the
/// resident — a test that needed it would pass on a tool that never reached it.
///
/// C is asked in the same session as the control: a tool that lost the rule answers both
/// queries without it.
#[tokio::test(flavor = "multi_thread")]
async fn a_derived_table_naming_an_outer_alias_is_reported_and_a_correlated_subquery_is_not() {
    let dir = tempfile::tempdir().expect("tempdir");
    stage(dir.path());
    let client = client(dir.path()).await;

    let d = validate(&client, args(DERIVED_TABLE_NAMES_OUTER_ALIAS)).await;
    let c = validate(&client, args(CORRELATED_IN_SUBQUERY)).await;

    let found = diagnostics_of(&d, UNAVAILABLE_ALIAS);
    assert_eq!(found.len(), 1, "D must be reported exactly once: {d}");
    let (line, column) = position_of(DERIVED_TABLE_NAMES_OUTER_ALIAS, "Внеш.Ссылка");
    assert_eq!(
        found[0]["message"],
        "Псевдоним \"Внеш\" внешнего запроса недоступен внутри производной таблицы",
        "{d}",
    );
    assert_eq!(
        (
            found[0]["start_line"].as_u64(),
            found[0]["start_column"].as_u64(),
            found[0]["end_line"].as_u64(),
            found[0]["end_column"].as_u64(),
        ),
        (
            Some(line as u64),
            Some(column as u64),
            Some(line as u64),
            Some((column + "Внеш".chars().count()) as u64),
        ),
        "the range is the alias token of the inner reference, in the query's coordinates: {d}",
    );
    assert!(
        codes(&d).iter().any(|c| c == "JoinWithSubQuery"),
        "the performance warning stays beside the new error: {d}",
    );

    assert!(
        diagnostics_of(&c, UNAVAILABLE_ALIAS).is_empty(),
        "a correlated subquery in a condition is legal: {c}",
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_unavailable_alias_follows_the_projects_configuration() {
    let dir = tempfile::tempdir().expect("tempdir");
    stage(dir.path());
    write(
        dir.path(),
        "bsl-analyzer.toml",
        "[diagnostics.parameters]\nUnavailableAliasInQuery = false\n",
    );
    let client = client(dir.path()).await;

    let d = validate(&client, args(DERIVED_TABLE_NAMES_OUTER_ALIAS)).await;

    assert!(
        diagnostics_of(&d, UNAVAILABLE_ALIAS).is_empty(),
        "a rule the project turned off must stay off: {d}",
    );
    // The control: only the named rule went silent, so an empty list is not a dead tool.
    assert!(codes(&d).iter().any(|c| c == "JoinWithSubQuery"), "{d}");
}
