//! End-to-end `symbol_info` over the real MCP transport: a workspace-profile server
//! answers a qualified-name lookup with a consolidated semantic card through an rmcp
//! client, and honours the tool's documented contracts — param validation, output
//! budget, and workspace-only availability — across serialization and tool dispatch.
//!
//! The card-resolution logic itself is unit-tested in `ide::symbol_info` (see
//! `crates/ide/tests/ide_suite/symbol_info.rs`); this suite covers the MCP layer those tests do
//! not exercise: the resident lifecycle (loading envelope → ready), the JSON shaping,
//! and the profile gating, driven through a genuine `serve_stream` handshake.

use std::path::{Path, PathBuf};
use std::time::Duration;

use mcp_server::{serve_stream, McpProfile, McpServer, SharedState};
use rmcp::model::CallToolRequestParams;
use rmcp::service::RunningService;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{Map, Value};
use tempfile::TempDir;

// Shared helpers for the gates in this binary; not every binary uses every one.
#[allow(dead_code)]
mod common;

type Client = RunningService<RoleClient, ()>;

fn designer_fixture() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-metadata/fixtures/designer"))
}

/// Copy the checked-in metadata fixture into a scratch dir so the resident host's on-disk
/// caches (graph/search) never land in the repo tree, and each run starts cold.
fn stage_workspace() -> TempDir {
    let src = designer_fixture();
    let dst = TempDir::new().expect("scratch workspace");
    for entry in walkdir::WalkDir::new(&src) {
        let entry = entry.expect("walk fixture");
        let rel = entry.path().strip_prefix(&src).expect("path under fixture root");
        let target = dst.path().join(rel);
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&target).expect("mkdir");
        } else {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).expect("mkdir parent");
            }
            std::fs::copy(entry.path(), &target).expect("copy fixture file");
        }
    }
    dst
}

/// Copy the checked-in metadata fixture into an existing directory.
fn copy_fixture_into(dst: &Path) {
    let src = designer_fixture();
    for entry in walkdir::WalkDir::new(&src) {
        let entry = entry.expect("walk fixture");
        let rel = entry.path().strip_prefix(&src).expect("path under fixture root");
        let target = dst.join(rel);
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&target).expect("mkdir");
        } else {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).expect("mkdir parent");
            }
            std::fs::copy(entry.path(), &target).expect("copy fixture file");
        }
    }
}

/// A workspace-profile server reachable over an in-memory duplex, exactly how the daemon
/// serves a proxy from one `SharedState` (mirrors the broker concurrency test).
async fn workspace_client(root: &Path) -> Client {
    let server = McpServer::new(
        McpProfile::Workspace,
        SharedState::workspace(root.to_path_buf()).expect("valid workspace project"),
    );
    // The buffer must exceed the largest single response: an in-process duplex has no kernel
    // backpressure, so an oversized frame would wedge the pipe (a harness artifact, not the
    // socket transport the daemon uses).
    let (client_io, server_io) = tokio::io::duplex(4 * 1024 * 1024);
    tokio::spawn(serve_stream(server, server_io));
    ().serve(client_io).await.expect("session initialized")
}

fn args(pairs: &[(&str, Value)]) -> Map<String, Value> {
    pairs.iter().map(|(k, v)| ((*k).to_string(), v.clone())).collect()
}

/// Call `symbol_info` and return the parsed card, retrying past the "still loading"
/// envelope until the resident is ready or the budget runs out. The envelope is told apart
/// by its `status: "loading"` field — the way a consumer is meant to read it. Matching the
/// human sentence, or inferring "not ready" from an absent envelope, is what this suite
/// must not model: a resolved card, a resident miss AND the retry envelope all carry
/// structured content.
async fn poll_card(client: &Client, call_args: Map<String, Value>) -> Value {
    let mut deadline =
        common::Patience::new(common::STATE_CEILING, std::time::Duration::from_millis(100));
    loop {
        let call = CallToolRequestParams::new("symbol_info").with_arguments(call_args.clone());
        let result = client.call_tool(call).await.expect("transport ok");
        if let Some(structured) = result.structured_content {
            if structured["status"] != "loading" {
                return structured;
            }
            assert_eq!(structured["schema_version"], "1");
            assert!(structured["detail"].is_string());
            assert!(structured["state"].is_string());
            assert!(structured["generation"].is_u64());
        }
        assert!(deadline.allows(), "resident never became ready for {call_args:?}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// A card the resident answered with every name source consulted.
///
/// [`poll_card`] returns the first answer that is not the loading envelope, and that answer
/// may legitimately be `not_found` while the index is still enrolling files — it says so in
/// `freshness.completeness`. A gate asking whether a symbol EXISTS has to wait for that to
/// clear, or it reads "not indexed yet" as "not there".
async fn poll_complete_card(client: &Client, call_args: Map<String, Value>) -> Value {
    let mut deadline =
        common::Patience::new(common::STATE_CEILING, std::time::Duration::from_millis(100));
    loop {
        let card = poll_card(client, call_args.clone()).await;
        if card["freshness"]["completeness"]["status"] == "complete" {
            return card;
        }
        assert!(
            deadline.allows(),
            "the name sources never all reported in for {call_args:?}: {card}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn symbol_info_serves_semantic_cards_over_the_transport() {
    let ws = stage_workspace();
    let client = workspace_client(ws.path()).await;

    let listed = client.list_tools(Default::default()).await.expect("tools/list");
    let tool = listed.tools.iter().find(|tool| tool.name == "symbol_info").unwrap();
    let schema = tool.output_schema.as_ref().expect("symbol_info outputSchema");
    let encoded = serde_json::to_string(schema).unwrap();
    for value in
        ["ok", "not_found", "ambiguous", "loading", "availability", "type_variants", "signature"]
    {
        assert!(encoded.contains(value), "missing {value} in tools/list outputSchema");
    }

    // A qualified metadata-object name resolves to a full card end-to-end: kind, container,
    // and the object's members straight from the metadata substrate.
    let object =
        poll_card(&client, args(&[("symbol", Value::from("Справочник.Справочник1"))])).await;
    assert_eq!(object["schema_version"], "1");
    assert_eq!(object["status"], "ok");
    assert_eq!(object["symbol"], "Справочник.Справочник1");
    assert_eq!(object["kind"], "metadata object");
    assert_eq!(object["container"]["kind"], "Справочник");
    let members = object["members"].as_array().expect("object lists members");
    assert!(
        members.iter().any(|m| m["name"] == "Реквизит1" && m["kind"] == "Реквизит"),
        "members were {members:?}"
    );
    let attribute_member = members.iter().find(|m| m["name"] == "Реквизит1").unwrap();
    assert_eq!(attribute_member["member_kind"], "attribute");
    assert_eq!(attribute_member["origin"], "metadata");
    assert_eq!(attribute_member["availability"]["context_status"], "not_evaluated");
    assert!(attribute_member["availability"]["contexts"].is_null());
    let variants = attribute_member["type_variants"].as_array().unwrap();
    assert!(!variants.is_empty());
    assert!(variants.iter().all(|variant| variant["resolution"] == "static"));
    assert!(variants.iter().all(|variant| variant["technical_name"].is_string()));
    assert!(attribute_member.get("signature").is_none());

    for symbol in [
        "СправочникОбъект.Справочник1",
        "СправочникСсылка.Справочник1",
        "СправочникМенеджер.Справочник1",
        "ОбработкаОбъект.ТестоваяОбработка",
    ] {
        let facet = poll_card(&client, args(&[("symbol", Value::from(symbol))])).await;
        assert_eq!(facet["symbol"], symbol, "facet card: {facet:?}");
    }

    // An attribute of that object carries its type and ownership from the substrate — the
    // metadata-member path the issue calls out explicitly.
    let attribute =
        poll_card(&client, args(&[("symbol", Value::from("Справочник.Справочник1.Реквизит1"))]))
            .await;
    assert_eq!(attribute["kind"], "attribute");
    assert!(attribute.get("return_type").is_some(), "attribute carries its type: {attribute:?}");
    assert_eq!(attribute["container"]["kind"], "Справочник");
    assert_eq!(attribute["container"]["name"], "Справочник1");

    // The output budget is honoured over the wire: a tiny budget trims the member list and
    // stamps the card `truncated`.
    let trimmed = poll_card(
        &client,
        args(&[
            ("symbol", Value::from("Справочник.Справочник1")),
            ("max_output_tokens", Value::from(1)),
        ]),
    )
    .await;
    assert_eq!(trimmed["truncated"], true, "tiny budget trims the card: {trimmed:?}");

    let form_args = args(&[
        ("symbol", Value::from("Документ.Документ1.Форма.ФормаДокумента")),
        ("max_output_tokens", Value::from(1_000)),
    ]);
    let first_form = poll_card(&client, form_args.clone()).await;
    let second_form = poll_card(&client, form_args).await;
    assert_eq!(first_form["truncated"], true, "managed form exceeds the test budget");
    let members = first_form["members"].as_array().expect("budget keeps a member prefix");
    assert!(!members.is_empty());
    assert_eq!(members, second_form["members"].as_array().unwrap());

    // An imprecise name that no resident symbol matches is a structured "not resolved"
    // envelope, never a transport error.
    let miss =
        poll_card(&client, args(&[("symbol", Value::from("НетТакогоМодуля.НетМетода"))])).await;
    assert_eq!(miss["schema_version"], "1", "versioned miss: {miss:?}");
    assert_eq!(miss["status"], "not_found", "versioned miss: {miss:?}");
    assert_eq!(miss["resolved"], false, "unknown name is a structured miss: {miss:?}");

    for symbol in ["НеизвестнаяГрань.Справочник1", "СправочникОбъект.НетТакого"]
    {
        let miss = poll_card(&client, args(&[("symbol", Value::from(symbol))])).await;
        assert_eq!(miss["schema_version"], "1");
        assert_eq!(miss["status"], "not_found");
        assert_eq!(miss["symbol"], symbol);
    }

    let malformed = client
        .call_tool(
            CallToolRequestParams::new("symbol_info")
                .with_arguments(args(&[("symbol", Value::from("СправочникОбъект..Справочник1"))])),
        )
        .await;
    assert!(malformed.is_err(), "malformed symbol must be invalid params: {malformed:?}");

    let path = "Catalogs/Справочник1/Ext/ObjectModule.bsl";
    for positional in [
        args(&[("path", Value::from(path)), ("line", Value::from(0))]),
        args(&[
            ("root_id", Value::from("")),
            ("path", Value::from(path)),
            ("line", Value::from(0)),
        ]),
    ] {
        let _ = poll_card(&client, positional).await;
    }
    let object_facet = poll_card(
        &client,
        args(&[
            ("symbol", Value::from("СправочникОбъект.Справочник1")),
            ("max_output_tokens", Value::from(60_000)),
        ]),
    )
    .await;
    let facet_members = object_facet["members"].as_array().unwrap();
    let sample_name = facet_members[0]["name"].as_str().unwrap();
    let sample_kind = facet_members[0]["member_kind"].as_str().unwrap();
    let by_kind = poll_card(
        &client,
        args(&[
            ("symbol", Value::from("СправочникОбъект.Справочник1")),
            ("member_kind", Value::from(sample_kind)),
        ]),
    )
    .await;
    assert!(by_kind["members"]
        .as_array()
        .unwrap()
        .iter()
        .all(|member| member["member_kind"] == sample_kind));
    let by_name = poll_card(
        &client,
        args(&[
            ("symbol", Value::from("СправочникОбъект.Справочник1")),
            ("member_name", Value::from(sample_name.to_lowercase())),
        ]),
    )
    .await;
    let named = by_name["members"].as_array().unwrap();
    assert!(!named.is_empty());
    assert!(named.iter().all(|member| {
        member["name"]
            .as_str()
            .is_some_and(|name| name.to_lowercase() == sample_name.to_lowercase())
    }));
    let legacy_include = poll_card(
        &client,
        args(&[
            ("symbol", Value::from("СправочникОбъект.Справочник1")),
            ("include", Value::Array(vec!["definition".into(), "type".into(), "doc".into()])),
            ("max_output_tokens", Value::from(60_000)),
        ]),
    )
    .await;
    assert_eq!(legacy_include["members"].as_array().unwrap().len(), facet_members.len());
    let contextual = poll_card(
        &client,
        args(&[
            ("symbol", Value::from("СправочникОбъект.Справочник1")),
            ("path", Value::from(path)),
            ("line", Value::from(0)),
            ("column", Value::from(0)),
            ("max_output_tokens", Value::from(60_000)),
        ]),
    )
    .await;
    let contextual_members = contextual["members"].as_array().unwrap();
    assert_eq!(
        contextual_members.len(),
        object_facet["members"].as_array().unwrap().len(),
        "position must not filter members"
    );
    assert!(contextual_members.iter().all(|member| {
        matches!(
            member["availability"]["context_status"].as_str(),
            Some("available" | "unavailable" | "unknown")
        )
    }));

    for incomplete in [
        args(&[("path", Value::from(path))]),
        args(&[("line", Value::from(0))]),
        args(&[
            ("symbol", Value::from("СправочникОбъект.Справочник1")),
            ("root_id", Value::from("")),
        ]),
    ] {
        let result = client
            .call_tool(CallToolRequestParams::new("symbol_info").with_arguments(incomplete))
            .await;
        assert!(result.is_err(), "incomplete positional context must be rejected: {result:?}");
    }

    // A call with neither `symbol` nor `path` is a parameter error, surfaced as a
    // JSON-RPC error through the transport.
    let bad = client
        .call_tool(CallToolRequestParams::new("symbol_info").with_arguments(Map::new()))
        .await;
    assert!(bad.is_err(), "missing symbol/path must be a param error, got {bad:?}");

    client.cancel().await.ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn symbol_info_is_workspace_only() {
    // The reference profile has no resident analysis host, so `symbol_info` is not part of
    // its tool surface — a call must be rejected, not served.
    let server = McpServer::new(McpProfile::Reference, SharedState::reference(None));
    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    tokio::spawn(serve_stream(server, server_io));
    let client = ().serve(client_io).await.expect("reference session initialized");

    let call = CallToolRequestParams::new("symbol_info")
        .with_arguments(args(&[("symbol", Value::from("Справочник.Справочник1"))]));
    let result = client.call_tool(call).await;
    assert!(result.is_err(), "reference profile must not serve symbol_info, got {result:?}");

    client.cancel().await.ok();
}

/// The relative path a common module keeps in every root that declares it.
const WOVEN_MODULE_REL: &str = "CommonModules/Общий";
/// The method the extension weaves onto, and one it leaves alone.
const WOVEN_METHOD: &str = "Обернутый";
const UNTOUCHED_METHOD: &str = "Нетронутый";
/// Declared, under this same name and exported, by BOTH bodies — that is the whole point of
/// it. For every other name only one body declares it, so the walk finds that body whatever
/// order it walks in and a broken ranking stays invisible. Here the order decides the answer.
const CONTESTED_METHOD: &str = "Спорный";

fn write_common_module(root: &Path, name: &str, body: &str) {
    write_common_module_with_flags(root, name, body, "<Server>true</Server>")
}

/// The same writer, with the execution flags spelled by the caller.
///
/// Two bodies of one module may declare DIFFERENT environments, and that difference is the
/// only thing that can tell a card taking its context from the declaring body apart from one
/// taking it from whichever body the path-derived index answered with.
fn write_common_module_with_flags(root: &Path, name: &str, body: &str, flags: &str) {
    let configuration = root.join("Configuration.xml");
    let text = std::fs::read_to_string(&configuration).expect("read Configuration.xml");
    let at = text.find("<CommonModule>").expect("the fixture lists common modules");
    let mut listed = text.clone();
    listed.insert_str(at, &format!("<CommonModule>{name}</CommonModule>\n\t\t\t"));
    std::fs::write(&configuration, listed).expect("register the module");

    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:v8="http://v8.1c.ru/8.1/data/core">
	<CommonModule uuid="00000000-0000-0000-0000-0000000000{id:02}">
		<Properties>
			<Name>{name}</Name>
			<Global>false</Global>
			{flags}
			<ExternalConnection>false</ExternalConnection>
			<ClientOrdinaryApplication>false</ClientOrdinaryApplication>
			<ServerCall>false</ServerCall>
			<Privileged>false</Privileged>
			<ReturnValuesReuse>DontUse</ReturnValuesReuse>
		</Properties>
	</CommonModule>
</MetaDataObject>"#,
        id = name.len(),
        flags = flags,
    );
    std::fs::create_dir_all(root.join("CommonModules")).expect("mkdir CommonModules");
    std::fs::write(root.join(format!("CommonModules/{name}.xml")), xml).expect("write descriptor");
    // The body goes where its NAME says, not to a fixed path: a module whose descriptor and
    // whose file disagree is not found at all, and a stand built on it fails for a reason that
    // has nothing to do with what it meant to test.
    let dir = root.join(format!("CommonModules/{name}")).join("Ext");
    std::fs::create_dir_all(&dir).expect("mkdir module");
    std::fs::write(dir.join("Module.bsl"), body).expect("write module");
}

/// A configuration whose common module an extension weaves onto: one method is wrapped by
/// `&Перед`, another is left alone as the control.
///
/// The two roots are named, not left to chance. The extension's directory sorts BEFORE the
/// configuration's on purpose: resolution that ranks bodies by path rather than by root
/// topology picks the extension here and loses the base declaration outright, so this stand
/// is an input on which that mistake is visible. Random temporary names would make the same
/// gate pass or fail by luck of the draw.
fn stage_woven_workspace() -> (TempDir, PathBuf) {
    let dir = TempDir::new().expect("scratch workspace");
    let ws = dir.path().join("configuration");
    let ext = dir.path().join("00-extension");
    for root in [&ws, &ext] {
        std::fs::create_dir_all(root).expect("mkdir root");
    }
    copy_fixture_into(&ws);
    std::fs::copy(ws.join("Configuration.xml"), ext.join("Configuration.xml"))
        .expect("the extension needs a configuration file to be one");

    write_common_module(
        &ws,
        "Общий",
        &format!(
            "&НаСервере\nПроцедура {WOVEN_METHOD}() Экспорт\nКонецПроцедуры\n\n\
             &НаСервере\nПроцедура {UNTOUCHED_METHOD}() Экспорт\nКонецПроцедуры\n\n\
             &НаСервере\nФункция {CONTESTED_METHOD}() Экспорт\n    Возврат \"конфигурация\";\nКонецФункции\n"
        ),
    );
    write_common_module(
        &ext,
        "Общий",
        &format!(
            "&Перед(\"{WOVEN_METHOD}\")\nПроцедура РасширениеПеред() Экспорт\nКонецПроцедуры\n\n\
             &НаСервере\nФункция {CONTESTED_METHOD}() Экспорт\n    Возврат \"расширение\";\nКонецФункции\n\n\
             &НаСервере\nПроцедура ТолькоВРасширении() Экспорт\nКонецПроцедуры\n"
        ),
    );

    std::fs::write(
        ws.join("bsl-analyzer.toml"),
        format!("[source]\nroot = \".\"\nextensions = [{{ name = \"расш\", path = {ext:?}, scope = \"InfoBase\" }}]\n"),
    )
    .expect("declare the extension");
    (dir, ws)
}

/// A method an extension weaves onto has MORE THAN ONE declaration, and a card that
/// publishes one of them tells a client the base body is what runs — false for `&Вместо`,
/// incomplete for `&Перед` and `&После`. Each site names the part it plays, so a client can
/// tell the body that runs from the ones that run around it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_woven_method_publishes_every_declaration_site() {
    let (_dir, ws) = stage_woven_workspace();
    let client = workspace_client(&ws).await;

    let woven = poll_complete_card(
        &client,
        args(&[("symbol", Value::from(format!("Общий.{WOVEN_METHOD}")))]),
    )
    .await;
    assert_eq!(woven["status"], "ok", "the stand must resolve the woven method: {woven}");
    let sites = woven["definitions"].as_array().expect("definitions is a list");

    let roles: Vec<&str> =
        sites.iter().map(|site| site["role"].as_str().unwrap_or_default()).collect();
    assert_eq!(
        roles,
        vec!["base", "before"],
        "the base declaration and the interceptor, each naming its part: {woven}",
    );

    let roots: Vec<&str> =
        sites.iter().map(|site| site["location"]["root_id"].as_str().unwrap_or_default()).collect();
    assert_ne!(
        roots[0], roots[1],
        "the two sites live in two roots, and the pair is what says so: {woven}",
    );

    // Control in the same stand: a method nothing weaves onto keeps exactly one site, so the
    // assertion above is about weaving and not about the list having grown for everyone.
    let untouched = poll_complete_card(
        &client,
        args(&[("symbol", Value::from(format!("Общий.{UNTOUCHED_METHOD}")))]),
    )
    .await;
    let untouched_sites = untouched["definitions"].as_array().expect("definitions is a list");
    assert_eq!(untouched_sites.len(), 1, "nothing weaves onto this one: {untouched}");
    assert_eq!(untouched_sites[0]["role"], "base", "{untouched}");

    // The configuration outranks the extension, and only a name BOTH bodies declare can tell
    // a correct ranking from no ranking at all: for every other name the walk finds the one
    // body that declares it whatever order it walks in.
    let contested = poll_complete_card(
        &client,
        args(&[("symbol", Value::from(format!("Общий.{CONTESTED_METHOD}")))]),
    )
    .await;
    assert_eq!(contested["status"], "ok", "{contested}");
    assert_eq!(
        contested["definitions"][0]["location"]["root_id"], "",
        "the configuration declares this name too, and it outranks the extension: {contested}",
    );
    assert_eq!(contested["definitions"][0]["role"], "base", "{contested}");

    client.cancel().await.ok();
}

/// Gate — the `column` this tool accepts is counted in the unit its answers publish.
///
/// `references` has this gate; without the same one here the pair is covered by one of its
/// two members, and the two tools read the same parameter through the same helper only for
/// as long as nobody changes one of them.
///
/// The input has to be one where the unit systems disagree, and a Cyrillic name is not that
/// input: every BSL identifier is one UTF-16 unit per character, so a column counted in
/// characters lands on the same token and a broken build stays green. Astral characters are
/// the only text that separates UTF-16 from code points, and there have to be enough of them
/// that the drift carries the column clear off the name rather than into its middle.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_column_is_read_in_the_unit_the_answers_are_published_in() {
    let ws = stage_workspace();
    let astral = "😀".repeat(25);
    let line = format!("    Текст = \"{astral}\"; ПервыйОбщийМодуль.НеУстаревшаяФункция();");
    // Appended to a module the fixture's configuration already lists: a module the metadata
    // does not name is invisible to the resident, and the answer would be `not_found` for a
    // reason that has nothing to do with columns.
    let path = "CommonModules/ПервыйОбщийМодуль/Ext/Module.bsl";
    let module = ws.path().join(path);
    let existing = std::fs::read_to_string(&module).expect("the fixture module is there");
    let extended = format!("{existing}\nПроцедура Астральная() Экспорт\n{line}\nКонецПроцедуры\n");
    // Counted out of the text that was written, not from the length of what preceded it: an
    // off-by-one there aims the anchor at a line with no name on it, and the gate then fails
    // for a reason that has nothing to do with the unit columns are counted in.
    let anchor_line =
        extended.lines().position(|text| text == line).expect("the appended line is in the file")
            as u32;
    std::fs::write(&module, &extended).expect("extend the module");
    let client = workspace_client(ws.path()).await;

    let name_at =
        line.find("НеУстаревшаяФункция").expect("the stand puts the name after the literal");
    let utf16_column = line[..name_at].encode_utf16().count() as u32;
    let code_point_column = line[..name_at].chars().count() as u32;
    assert_ne!(
        utf16_column, code_point_column,
        "the stand must be an input the two unit systems disagree on, or this gate is vacuous",
    );

    // A positional входа refuses a path the resident has not enrolled yet, and that refusal is
    // a transport error rather than an answer — so wait for the module by name first.
    let enrolled = poll_complete_card(
        &client,
        args(&[("symbol", Value::from("ПервыйОбщийМодуль.Астральная"))]),
    )
    .await;
    assert_eq!(enrolled["status"], "ok", "the stand module must be resident: {enrolled}");

    let card = poll_card(
        &client,
        args(&[
            ("path", Value::from(path)),
            ("line", Value::from(anchor_line)),
            ("column", Value::from(utf16_column)),
        ]),
    )
    .await;
    assert_eq!(
        card["symbol"], "НеУстаревшаяФункция",
        "a UTF-16 column did not address the name it measures to: {card}",
    );

    // The control: the same column counted the other way lands somewhere else entirely.
    // Without it a build reading columns as code points would satisfy the assertion above
    // whenever both numbers happened to fall on the same token.
    let elsewhere = poll_card(
        &client,
        args(&[
            ("path", Value::from(path)),
            ("line", Value::from(anchor_line)),
            ("column", Value::from(code_point_column)),
        ]),
    )
    .await;
    assert_ne!(
        elsewhere["symbol"], "НеУстаревшаяФункция",
        "the two unit systems must not address the same token, or the gate proves nothing: \
         {elsewhere}",
    );

    client.cancel().await.ok();
}

/// An annotation 1C does not allow is not published as a site that runs.
///
/// `&Перед` / `&После` cannot extend a FUNCTION — only `&Вместо` can — and the effective
/// exports calculation drops such an interceptor. A card that keeps it tells a client code
/// runs around this function when none does. The control is the same annotation on a
/// PROCEDURE in the same stand, which must still be published: a collector that simply
/// stopped publishing `before` sites would pass the first half of this gate.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_inapplicable_interception_is_not_published_as_a_site() {
    let dir = TempDir::new().expect("scratch workspace");
    let ws = dir.path().join("configuration");
    let ext = dir.path().join("00-extension");
    for root in [&ws, &ext] {
        std::fs::create_dir_all(root).expect("mkdir root");
    }
    copy_fixture_into(&ws);
    std::fs::copy(ws.join("Configuration.xml"), ext.join("Configuration.xml"))
        .expect("the extension needs a configuration file to be one");

    write_common_module(
        &ws,
        "Общий",
        "&НаСервере\nФункция ЭтоФункция() Экспорт\n    Возврат 1;\nКонецФункции\n\n\
         &НаСервере\nПроцедура ЭтоПроцедура() Экспорт\nКонецПроцедуры\n",
    );
    write_common_module(
        &ext,
        "Общий",
        "&Перед(\"ЭтоФункция\")\nПроцедура ПередФункцией() Экспорт\nКонецПроцедуры\n\n\
         &Перед(\"ЭтоПроцедура\")\nПроцедура ПередПроцедурой() Экспорт\nКонецПроцедуры\n",
    );
    std::fs::write(
        ws.join("bsl-analyzer.toml"),
        format!("[source]\nroot = \".\"\nextensions = [{{ name = \"расш\", path = {ext:?}, scope = \"InfoBase\" }}]\n"),
    )
    .expect("declare the extension");

    let client = workspace_client(&ws).await;

    let function =
        poll_complete_card(&client, args(&[("symbol", Value::from("Общий.ЭтоФункция"))])).await;
    let roles: Vec<&str> = function["definitions"]
        .as_array()
        .expect("definitions is a list")
        .iter()
        .map(|site| site["role"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(
        roles,
        vec!["base"],
        "`&Перед` cannot extend a function, so nothing runs before it: {function}",
    );

    let procedure =
        poll_complete_card(&client, args(&[("symbol", Value::from("Общий.ЭтоПроцедура"))])).await;
    let roles: Vec<&str> = procedure["definitions"]
        .as_array()
        .expect("definitions is a list")
        .iter()
        .map(|site| site["role"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(
        roles,
        vec!["base", "before"],
        "control: the same annotation on a procedure DOES run, and is published: {procedure}",
    );

    client.cancel().await.ok();
}

/// The same method, addressed by position instead of by name, gets the same sites.
///
/// Which sentence a client used to reach a symbol is not a property of the symbol. A card
/// that lists interceptors only for a named request hands two different answers about one
/// method depending on how it was asked for.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_positional_card_lists_the_same_sites_as_a_named_one() {
    let (_dir, ws) = stage_woven_workspace();
    let client = workspace_client(&ws).await;

    let named = poll_complete_card(
        &client,
        args(&[("symbol", Value::from(format!("Общий.{WOVEN_METHOD}")))]),
    )
    .await;
    let named_roles: Vec<&str> = named["definitions"]
        .as_array()
        .expect("definitions is a list")
        .iter()
        .map(|site| site["role"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(named_roles, vec!["base", "before"], "premise: the named card is whole: {named}");

    // The declaration's own coordinates, taken out of the answer rather than counted by hand.
    let base = &named["definitions"][0]["location"];
    let positional = poll_card(
        &client,
        args(&[
            ("path", base["path"].clone()),
            ("root_id", base["root_id"].clone()),
            ("line", base["range"]["start_line"].clone()),
            ("column", base["range"]["start_character"].clone()),
        ]),
    )
    .await;
    let positional_roles: Vec<&str> = positional["definitions"]
        .as_array()
        .expect("definitions is a list")
        .iter()
        .map(|site| site["role"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(
        positional_roles, named_roles,
        "one method, one set of declaration sites, whichever way it was addressed: {positional}",
    );
    // The node id travels with the symbol, not with the sentence used to reach it. `references`
    // publishes it for a positional anchor, so a card withholding it here would make the two
    // tools disagree about a symbol both of them resolved.
    assert_eq!(
        positional["graph_id"], named["graph_id"],
        "the positional card names the same graph node: {positional}",
    );
    assert!(positional["graph_id"].is_string(), "and names one at all: {positional}");

    client.cancel().await.ok();
}

/// A method an extension DECLARES — not weaves onto — says which extension it came from.
///
/// `source_extension` follows the ROOT a site lives in, not the role it plays. An extension
/// may add an exported method the base configuration never had; that site is the only one
/// there is, so its role is `base`, and a card that omits the label there tells a client the
/// configuration declares it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_base_site_from_an_extension_names_its_extension() {
    let (_dir, ws) = stage_woven_workspace();
    let client = workspace_client(&ws).await;

    let card =
        poll_complete_card(&client, args(&[("symbol", Value::from("Общий.ТолькоВРасширении"))]))
            .await;
    assert_eq!(card["status"], "ok", "the extension's own method must resolve: {card}");
    let site = &card["definitions"][0];
    assert_eq!(site["role"], "base", "nothing weaves onto it, so it is the base site: {card}");
    assert_ne!(
        site["location"]["root_id"], "",
        "premise: the site really does live in an extension root: {card}",
    );
    assert_eq!(
        site["source_extension"], "расш",
        "a base site from an extension still names the extension: {card}",
    );

    client.cancel().await.ok();
}

/// Weaving is not a property of COMMON modules. An extension weaves onto an object module
/// exactly as it weaves onto a common one, and a card that lists interceptors for one kind
/// only answers a catalog's own method with the base body alone — calling that the whole
/// truth.
///
/// The control is in the same stand: a second method of the same object module that nothing
/// weaves onto keeps exactly one site, so the assertion is about weaving rather than about
/// every list having grown.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_object_module_method_publishes_its_interceptors_too() {
    let dir = TempDir::new().expect("scratch workspace");
    let ws = dir.path().join("configuration");
    let ext = dir.path().join("00-extension");
    for root in [&ws, &ext] {
        std::fs::create_dir_all(root).expect("mkdir root");
    }
    copy_fixture_into(&ws);
    std::fs::copy(ws.join("Configuration.xml"), ext.join("Configuration.xml"))
        .expect("the extension needs a configuration file to be one");

    // The catalog and its object module are already in the checked-in fixture, so the stand
    // adds the two methods rather than a whole metadata object.
    let module_rel = "Catalogs/Справочник1/Ext/ObjectModule.bsl";
    let base = ws.join(module_rel);
    let existing = std::fs::read_to_string(&base).expect("the fixture module is there");
    std::fs::write(
        &base,
        format!(
            "{existing}\n\
             Процедура ПередЗаписью(Отказ) Экспорт\nКонецПроцедуры\n\n\
             Процедура НикемНеТронутая() Экспорт\nКонецПроцедуры\n"
        ),
    )
    .expect("extend the object module");

    let woven = ext.join(module_rel);
    std::fs::create_dir_all(woven.parent().expect("module dir")).expect("mkdir module dir");
    std::fs::write(
        &woven,
        "&Перед(\"ПередЗаписью\")\nПроцедура РасшПередЗаписью(Отказ) Экспорт\nКонецПроцедуры\n",
    )
    .expect("write the extension body");
    std::fs::write(
        ws.join("bsl-analyzer.toml"),
        format!("[source]\nroot = \".\"\nextensions = [{{ name = \"расш\", path = {ext:?}, scope = \"InfoBase\" }}]\n"),
    )
    .expect("declare the extension");

    let client = workspace_client(&ws).await;

    let card = poll_complete_card(
        &client,
        args(&[("symbol", Value::from("Справочник.Справочник1.ПередЗаписью"))]),
    )
    .await;
    assert_eq!(card["status"], "ok", "the object-module method must resolve: {card}");
    let roles: Vec<&str> = card["definitions"]
        .as_array()
        .expect("definitions is a list")
        .iter()
        .map(|site| site["role"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(
        roles,
        vec!["base", "before"],
        "an object module is woven onto like any other: {card}",
    );

    let untouched = poll_complete_card(
        &client,
        args(&[("symbol", Value::from("Справочник.Справочник1.НикемНеТронутая"))]),
    )
    .await;
    let sites = untouched["definitions"].as_array().expect("definitions is a list");
    assert_eq!(sites.len(), 1, "control: nothing weaves onto this one: {untouched}");

    client.cancel().await.ok();
}

/// The container's execution context comes from the body that DECLARES the method.
///
/// The declaration, the signature and the graph id are already taken from the body ranked by
/// config root. The context was the one field still read off the path-derived winner, and the
/// two disagree exactly when an extension's directory sorts first — so the card described an
/// environment the declaration it published does not run in.
///
/// The stand only means something because the two bodies declare DIFFERENT environments: with
/// matching flags every implementation prints the same label and the gate is vacuous.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_container_context_comes_from_the_declaring_body() {
    let dir = TempDir::new().expect("scratch workspace");
    let ws = dir.path().join("configuration");
    let ext = dir.path().join("00-extension");
    for root in [&ws, &ext] {
        std::fs::create_dir_all(root).expect("mkdir root");
    }
    copy_fixture_into(&ws);
    std::fs::copy(ws.join("Configuration.xml"), ext.join("Configuration.xml"))
        .expect("the extension needs a configuration file to be one");

    write_common_module_with_flags(
        &ws,
        "Общий",
        "&НаСервере\nПроцедура Общая() Экспорт\nКонецПроцедуры\n",
        "<Server>true</Server>",
    );
    write_common_module_with_flags(
        &ext,
        "Общий",
        "&НаКлиенте\nПроцедура Общая() Экспорт\nКонецПроцедуры\n",
        "<ClientManagedApplication>true</ClientManagedApplication>\n\t\t\t<Server>false</Server>",
    );
    std::fs::write(
        ws.join("bsl-analyzer.toml"),
        format!("[source]\nroot = \".\"\nextensions = [{{ name = \"расш\", path = {ext:?}, scope = \"InfoBase\" }}]\n"),
    )
    .expect("declare the extension");

    let client = workspace_client(&ws).await;
    let card = poll_complete_card(&client, args(&[("symbol", Value::from("Общий.Общая"))])).await;
    assert_eq!(card["status"], "ok", "{card}");
    assert_eq!(
        card["definitions"][0]["location"]["root_id"], "",
        "premise: the configuration outranks the extension and declares this site: {card}",
    );
    assert_eq!(
        card["container"]["context"], "Сервер",
        "the context belongs to the body the card published, not to the path-order winner: \
         {card}",
    );

    client.cancel().await.ok();
}

/// A manager module obeys root rank exactly as an object module does.
///
/// The ranking is computed from the candidates, but a resolver that then looks the module up
/// by NAME again gets the path-order winner and throws that ranking away. With an extension
/// sorting first, a method only the configuration declares then reads as absent.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_manager_module_method_follows_root_rank() {
    let dir = TempDir::new().expect("scratch workspace");
    let ws = dir.path().join("configuration");
    let ext = dir.path().join("00-extension");
    for root in [&ws, &ext] {
        std::fs::create_dir_all(root).expect("mkdir root");
    }
    copy_fixture_into(&ws);
    std::fs::copy(ws.join("Configuration.xml"), ext.join("Configuration.xml"))
        .expect("the extension needs a configuration file to be one");

    let module_rel = "Catalogs/СправочникСМенеджером/Ext/ManagerModule.bsl";
    let base = ws.join(module_rel);
    let existing = std::fs::read_to_string(&base).expect("the fixture manager module is there");
    std::fs::write(
        &base,
        format!("{existing}\nПроцедура ТолькоВКонфигурации() Экспорт\nКонецПроцедуры\n"),
    )
    .expect("extend the manager module");

    // The extension adopts the module but does NOT declare this method — the case where a
    // path-order winner answers about a method it never had.
    let woven = ext.join(module_rel);
    std::fs::create_dir_all(woven.parent().expect("module dir")).expect("mkdir module dir");
    std::fs::write(&woven, "Процедура ЧтоТоСвоё() Экспорт\nКонецПроцедуры\n")
        .expect("write the extension body");
    std::fs::write(
        ws.join("bsl-analyzer.toml"),
        format!("[source]\nroot = \".\"\nextensions = [{{ name = \"расш\", path = {ext:?}, scope = \"InfoBase\" }}]\n"),
    )
    .expect("declare the extension");

    let client = workspace_client(&ws).await;
    let card = poll_complete_card(
        &client,
        args(&[("symbol", Value::from("Справочник.СправочникСМенеджером.ТолькоВКонфигурации"))]),
    )
    .await;
    assert_eq!(
        card["status"], "ok",
        "the configuration declares this manager method, extension or not: {card}",
    );
    assert_eq!(
        card["definitions"][0]["location"]["root_id"], "",
        "and the site published is the configuration's: {card}",
    );

    client.cancel().await.ok();
}

/// A private method of the base body does not hide an exported one in an extension.
///
/// Asked from outside, a name means the EXPORTED declaration. Ranking bodies by root and then
/// taking the first that merely declares the name puts a private base method in front of a
/// usable extension export — and the card then says the method does not exist.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_private_base_method_does_not_hide_an_exported_one() {
    let dir = TempDir::new().expect("scratch workspace");
    let ws = dir.path().join("configuration");
    let ext = dir.path().join("00-extension");
    for root in [&ws, &ext] {
        std::fs::create_dir_all(root).expect("mkdir root");
    }
    copy_fixture_into(&ws);
    std::fs::copy(ws.join("Configuration.xml"), ext.join("Configuration.xml"))
        .expect("the extension needs a configuration file to be one");

    let module_rel = "Catalogs/Справочник1/Ext/ObjectModule.bsl";
    let base = ws.join(module_rel);
    let existing = std::fs::read_to_string(&base).expect("the fixture module is there");
    std::fs::write(&base, format!("{existing}\nПроцедура Скрытая()\nКонецПроцедуры\n"))
        .expect("extend the object module");

    let woven = ext.join(module_rel);
    std::fs::create_dir_all(woven.parent().expect("module dir")).expect("mkdir module dir");
    std::fs::write(&woven, "Процедура Скрытая() Экспорт\nКонецПроцедуры\n")
        .expect("write the extension body");

    // The same shape on a MANAGER module. That branch has no export check of its own after
    // the body is chosen, so the choice itself is the only thing standing between a private
    // base declaration and the answer — and without this half the gate stays green when that
    // filter is removed.
    let manager_rel = "Catalogs/СправочникСМенеджером/Ext/ManagerModule.bsl";
    let manager_base = ws.join(manager_rel);
    let manager_text =
        std::fs::read_to_string(&manager_base).expect("the fixture manager module is there");
    std::fs::write(
        &manager_base,
        format!("{manager_text}\nПроцедура СкрытаяУМенеджера()\nКонецПроцедуры\n"),
    )
    .expect("extend the manager module");
    let manager_woven = ext.join(manager_rel);
    std::fs::create_dir_all(manager_woven.parent().expect("module dir"))
        .expect("mkdir manager dir");
    std::fs::write(&manager_woven, "Процедура СкрытаяУМенеджера() Экспорт\nКонецПроцедуры\n")
        .expect("write the extension manager body");

    std::fs::write(
        ws.join("bsl-analyzer.toml"),
        format!("[source]\nroot = \".\"\nextensions = [{{ name = \"расш\", path = {ext:?}, scope = \"InfoBase\" }}]\n"),
    )
    .expect("declare the extension");

    let client = workspace_client(&ws).await;
    let card = poll_complete_card(
        &client,
        args(&[("symbol", Value::from("Справочник.Справочник1.Скрытая"))]),
    )
    .await;
    assert_eq!(
        card["status"], "ok",
        "the extension exports this method, so the name resolves: {card}",
    );
    assert_ne!(
        card["definitions"][0]["location"]["root_id"], "",
        "and the site published is the extension's, since the base body keeps it private: \
         {card}",
    );

    let manager = poll_complete_card(
        &client,
        args(&[("symbol", Value::from("Справочник.СправочникСМенеджером.СкрытаяУМенеджера"))]),
    )
    .await;
    assert_eq!(
        manager["status"], "ok",
        "a manager module obeys the same rule: the exported body answers: {manager}",
    );
    assert_ne!(
        manager["definitions"][0]["location"]["root_id"], "",
        "and it is the extension's body, not the private base one: {manager}",
    );

    client.cancel().await.ok();
}

/// The node id survives a narrowed `include`, and survives it the same way on both paths.
///
/// The id is a property of the symbol, not of the sections a caller asked for: the named path
/// publishes it whatever `include` says. Computing it inside the definition section on the
/// positional path made a section filter decide whether two tools name the same node — an
/// answer depending on a knob that has nothing to do with identity.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_narrowed_include_does_not_take_the_node_id_away() {
    let (_dir, ws) = stage_woven_workspace();
    let client = workspace_client(&ws).await;

    let named = poll_complete_card(
        &client,
        args(&[
            ("symbol", Value::from(format!("Общий.{UNTOUCHED_METHOD}"))),
            ("include", Value::from(vec![Value::from("doc"), Value::from("type")])),
        ]),
    )
    .await;
    let expected = named["graph_id"]
        .as_str()
        .unwrap_or_else(|| panic!("the named card names its node whatever include says: {named}"))
        .to_owned();

    // The declaration's own coordinates, so the two calls address one method.
    let whole = poll_complete_card(
        &client,
        args(&[("symbol", Value::from(format!("Общий.{UNTOUCHED_METHOD}")))]),
    )
    .await;
    let base = &whole["definitions"][0]["location"];

    let positional = poll_card(
        &client,
        args(&[
            ("path", base["path"].clone()),
            ("root_id", base["root_id"].clone()),
            ("line", base["range"]["start_line"].clone()),
            ("column", base["range"]["start_character"].clone()),
            ("include", Value::from(vec![Value::from("doc"), Value::from("type")])),
        ]),
    )
    .await;
    assert_eq!(
        positional["graph_id"], expected,
        "a narrowed include must not decide whether the positional card names its node: \
         {positional}",
    );

    client.cancel().await.ok();
}

/// A whole-module card names the body ranked by config root.
///
/// The card has no declaration node of its own, so it is anchored at the file start — but it
/// still names a FILE, and taking that file from an earlier-sorting extension makes the card
/// report the module as declared by the extension, in `root_id` and in `source_extension`
/// both.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_module_card_names_the_configuration_body() {
    let (_dir, ws) = stage_woven_workspace();
    let client = workspace_client(&ws).await;

    let card = poll_complete_card(&client, args(&[("symbol", Value::from("Общий"))])).await;
    assert_eq!(card["status"], "ok", "{card}");
    assert_eq!(
        card["definitions"][0]["location"]["root_id"], "",
        "the configuration declares this module and outranks the extension: {card}",
    );
    assert!(
        card["definitions"][0]["source_extension"].is_null(),
        "so no extension is named as its source: {card}",
    );

    client.cancel().await.ok();
}

/// An interceptor of a PRIVATE method is not published as a site that runs.
///
/// An extension cannot weave onto a method the base body does not export, and the effective
/// exports calculation drops such an interceptor. The card has to ask the same question, or
/// it publishes a handler that never executes. The control is the exported method in the same
/// stand, whose interceptor must still appear.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_interceptor_of_a_private_method_is_not_a_site() {
    let dir = TempDir::new().expect("scratch workspace");
    let ws = dir.path().join("configuration");
    let ext = dir.path().join("00-extension");
    for root in [&ws, &ext] {
        std::fs::create_dir_all(root).expect("mkdir root");
    }
    copy_fixture_into(&ws);
    std::fs::copy(ws.join("Configuration.xml"), ext.join("Configuration.xml"))
        .expect("the extension needs a configuration file to be one");

    write_common_module(
        &ws,
        "Общий",
        "&НаСервере\nПроцедура Приватная()\nКонецПроцедуры\n\n\
         &НаСервере\nПроцедура Открытая() Экспорт\nКонецПроцедуры\n",
    );
    write_common_module(
        &ext,
        "Общий",
        "&Перед(\"Приватная\")\nПроцедура ПередПриватной() Экспорт\nКонецПроцедуры\n\n\
         &Перед(\"Открытая\")\nПроцедура ПередОткрытой() Экспорт\nКонецПроцедуры\n",
    );
    std::fs::write(
        ws.join("bsl-analyzer.toml"),
        format!("[source]\nroot = \".\"\nextensions = [{{ name = \"расш\", path = {ext:?}, scope = \"InfoBase\" }}]\n"),
    )
    .expect("declare the extension");

    let client = workspace_client(&ws).await;

    // The control first, which also waits for the module to be enrolled: a positional call on
    // a path the resident has not seen is refused before any card is built.
    let exported =
        poll_complete_card(&client, args(&[("symbol", Value::from("Общий.Открытая"))])).await;
    let roles: Vec<&str> = exported["definitions"]
        .as_array()
        .expect("definitions is a list")
        .iter()
        .map(|site| site["role"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(
        roles,
        vec!["base", "before"],
        "control: the exported method IS woven onto, and its site is published: {exported}",
    );

    // Reached by position: a private method has no qualified name to ask for.
    let module_file = ws.join(WOVEN_MODULE_REL).join("Ext/Module.bsl");
    let text = std::fs::read_to_string(&module_file).expect("the stand module");
    let line = text.lines().position(|l| l.contains("Приватная")).expect("the private line") as u32;
    // UTF-16 units, not bytes: `find` answers in bytes, and every Cyrillic character before
    // the name counts twice there — the column would point past the name into the parentheses.
    let column = text
        .lines()
        .nth(line as usize)
        .and_then(|l| l.find("Приватная").map(|at| l[..at].encode_utf16().count() as u32))
        .expect("the name on that line");

    let private = poll_card(
        &client,
        args(&[
            ("path", Value::from(format!("{WOVEN_MODULE_REL}/Ext/Module.bsl"))),
            ("root_id", Value::from("")),
            ("line", Value::from(line)),
            ("column", Value::from(column)),
        ]),
    )
    .await;
    assert_eq!(
        private["status"],
        "ok",
        "the private method resolves by position (line {line}, column {column} of {:?}): \
         {private}",
        text.lines().nth(line as usize),
    );
    let roles: Vec<&str> = private["definitions"]
        .as_array()
        .unwrap_or_else(|| panic!("a resolved card lists its sites: {private}"))
        .iter()
        .map(|site| site["role"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(
        roles,
        vec!["base"],
        "an extension cannot weave onto a private method, so nothing runs before it: {private}",
    );

    client.cancel().await.ok();
}

/// Two extensions, one relative path each, and a `dependsOn` between them: a name from
/// another extension resolves ONLY when the dependency is declared.
///
/// This is the axis the acceptance criterion is about, and it is the one an ordinary stand
/// cannot show: the base configuration is visible to every extension, so a gate built on
/// "extension sees the configuration" passes whether the topology is consulted or not. Here
/// the same source, the same call and the same two roots are analysed twice — the ONLY
/// difference being the declared dependency — so a build that ignores the topology fails the
/// first half, and one that hides everything fails the second.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_name_from_another_extension_needs_a_declared_dependency() {
    async fn resolve_with(depends_on: Option<&str>) -> Value {
        let dir = TempDir::new().expect("scratch workspace");
        let ws = dir.path().join("configuration");
        let provider = dir.path().join("provider");
        let consumer = dir.path().join("consumer");
        for root in [&ws, &provider, &consumer] {
            std::fs::create_dir_all(root).expect("mkdir root");
        }
        copy_fixture_into(&ws);
        for root in [&provider, &consumer] {
            std::fs::copy(ws.join("Configuration.xml"), root.join("Configuration.xml"))
                .expect("an extension needs a configuration file to be one");
        }

        write_common_module(
            &provider,
            "Поставщик",
            "&НаСервере\nПроцедура ОтдатьДанные() Экспорт\nКонецПроцедуры\n",
        );
        write_common_module(
            &consumer,
            "Потребитель",
            "&НаСервере\nПроцедура Использовать() Экспорт\n    Поставщик.ОтдатьДанные();\nКонецПроцедуры\n",
        );

        let dependency =
            depends_on.map(|name| format!(", dependsOn = [\"{name}\"]")).unwrap_or_default();
        std::fs::write(
            ws.join("bsl-analyzer.toml"),
            format!(
                "[source]\nroot = \".\"\nextensions = [\n  {{ name = \"Поставщик\", path = {provider:?}, scope = \"InfoBase\" }},\n  \
                 {{ name = \"Потребитель\", path = {consumer:?}, scope = \"InfoBase\"{dependency} }},\n]\n"
            ),
        )
        .expect("declare the extensions");

        let client = workspace_client(&ws).await;
        // Positional, not by name: a name lookup answers about the workspace as a whole, while
        // visibility is a property of the FILE doing the asking — the distinction this gate is
        // about.
        let own = poll_complete_card(
            &client,
            args(&[("symbol", Value::from("Потребитель.Использовать"))]),
        )
        .await;
        assert_eq!(own["status"], "ok", "the calling module itself must resolve: {own}");
        let place = own["definitions"][0]["location"].clone();
        let call_line = place["range"]["start_line"].as_u64().expect("a line") as u32 + 1;

        let card = poll_card(
            &client,
            args(&[
                ("path", place["path"].clone()),
                ("root_id", place["root_id"].clone()),
                ("line", Value::from(call_line)),
                ("column", Value::from(14u32)),
            ]),
        )
        .await;
        client.cancel().await.ok();
        card
    }

    let without = resolve_with(None).await;
    assert_eq!(
        without["status"], "not_found",
        "independent extensions do not see each other: {without}",
    );

    let with = resolve_with(Some("Поставщик")).await;
    assert_eq!(
        with["status"], "ok",
        "and a declared dependency is what makes the name visible: {with}",
    );
}
