//! End-to-end `references` over the real MCP transport.
//!
//! What the `ide` suite cannot cover is here: that the tool is served only when a launch
//! names it, that every place it hands out is a pair another tool accepts, that the
//! envelope names whoever composed the body, and that narrowing and the caps behave the
//! way the answer says they do. Each check names the input on which it must fail.

use std::path::Path;
use std::time::Duration;

use mcp_server::{serve_stream, McpProfile, McpServer, SharedState, ToolGate};
use rmcp::model::CallToolRequestParams;
use rmcp::service::RunningService;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{json, Map, Value};
use tempfile::TempDir;

mod common;

type Client = RunningService<RoleClient, ()>;

const TOOL: &str = "references";

/// The declaration under test. It is a method of a module the fixture's configuration
/// actually declares: a module the metadata does not list is invisible to a caller, so a
/// call to it resolves to something else and no reference walk would find it — which is
/// the visibility rule working, not a defect of this tool.
///
/// Its own module already holds a qualified call and a bare one; the stand adds three
/// callers, so the answer spans four files and a per-file histogram has something to be
/// wrong about.
const STAND_METHOD: &str = "ПервыйОбщийМодуль.НеУстаревшаяФункция";

/// Copy the checked-in metadata fixture into a scratch dir and add the stand's own modules,
/// so derived caches never land in the repo tree and each run starts cold.
fn stage_workspace() -> TempDir {
    let dst = stage_fixture();
    for caller in ["Первый", "Второй", "Третий"] {
        // Distinct method names on purpose: one of them has to be a name NO other module
        // declares, or the "short unique name" input below would be ambiguous and would
        // stop testing what it is there for.
        write_module(
            dst.path(),
            caller,
            &format!(
                "Процедура Вызвать{caller}() Экспорт\n    \
                 ПервыйОбщийМодуль.НеУстаревшаяФункция();\nКонецПроцедуры\n"
            ),
        );
    }
    dst
}

/// The checked-in metadata fixture alone, for stands that bring their own modules.
fn stage_fixture() -> TempDir {
    let src = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-metadata/fixtures/designer"));
    let dst = TempDir::new().expect("scratch workspace");
    for entry in walkdir::WalkDir::new(src) {
        let entry = entry.expect("walk fixture");
        let rel = entry.path().strip_prefix(src).expect("path under fixture root");
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

fn write_module(root: &Path, name: &str, body: &str) {
    let dir = root.join("CommonModules").join(name).join("Ext");
    std::fs::create_dir_all(&dir).expect("mkdir module");
    std::fs::write(dir.join("Module.bsl"), body).expect("write module");
}

/// A workspace server with the opt-in tool enabled, as `--enable-tool references` does.
async fn client_with_references(root: &Path) -> Client {
    let state = SharedState::workspace(root.to_path_buf()).expect("valid workspace project");
    let gate = ToolGate::for_launch(McpProfile::Workspace, &[TOOL.to_owned()]);
    let server = McpServer::with_gate(McpProfile::Workspace, state, &gate);
    let (client_io, server_io) = tokio::io::duplex(4 * 1024 * 1024);
    tokio::spawn(serve_stream(server, server_io));
    ().serve(client_io).await.expect("session initialized")
}

fn args(pairs: &[(&str, Value)]) -> Map<String, Value> {
    pairs.iter().map(|(k, v)| ((*k).to_string(), v.clone())).collect()
}

/// Call a tool, retrying past the "still building" envelope. The envelope is told apart by
/// its `status: "loading"` field — how a consumer is meant to read it — never by matching
/// the human sentence beside it.
async fn poll(client: &Client, tool: &'static str, call_args: Map<String, Value>) -> Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    loop {
        let call = CallToolRequestParams::new(tool).with_arguments(call_args.clone());
        let result = client.call_tool(call).await.expect("transport ok");
        if let Some(structured) = result.structured_content {
            if structured["status"] != "loading" {
                return structured;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{tool} never became ready for {call_args:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn references(client: &Client, pairs: &[(&str, Value)]) -> Value {
    poll(client, TOOL, args(pairs)).await
}

fn kinds_of(answer: &Value) -> Vec<String> {
    answer["references"]
        .as_array()
        .expect("a resolved answer carries the list")
        .iter()
        .map(|entry| entry["kind"].as_str().expect("a kind").to_owned())
        .collect()
}

/// The whole surface in one pass over a stand with three call sites: the outcome, the
/// kinds, the per-file histogram that replaces a cursor, and the envelope's identity.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_qualified_name_is_answered_with_kinds_and_a_histogram() {
    let ws = stage_workspace();
    let client = client_with_references(ws.path()).await;

    let answer = references(&client, &[("symbol", Value::from(STAND_METHOD))]).await;

    assert_eq!(answer["outcome"], "resolved", "{answer}");
    assert_eq!(answer["schema_version"], "1");
    assert_eq!(answer["total"], 6, "declaration + five calls: {answer}");
    assert_eq!(answer["total_is_lower_bound"], false);
    assert_eq!(answer["narrowing_comparable"], true);

    let mut kinds = kinds_of(&answer);
    kinds.sort();
    assert_eq!(kinds, ["call", "call", "call", "call", "call", "declaration"], "{answer}");

    // The histogram is what a caller walks when `limit` hides part of the list, so its
    // counts must add up to the total — over more than one file, or any summing mistake
    // would still add up.
    let buckets = answer["files"].as_array().expect("a per-file histogram");
    assert!(buckets.len() >= 4, "one bucket per file with a hit: {buckets:?}");
    let sum: u64 = buckets.iter().map(|b| b["count"].as_u64().expect("a count")).sum();
    assert_eq!(sum, answer["total"].as_u64().unwrap(), "the histogram must cover the total");
    assert_eq!(answer["histogram_truncated"], false);

    // The body is the resident's own walk, so it carries the resident's identity.
    let freshness = &answer["freshness"];
    assert_eq!(freshness["source"], "resident");
    assert!(!freshness["revision"].is_null(), "{freshness}");
    assert!(!freshness["topology_fingerprint"].is_null(), "{freshness}");
    assert_eq!(freshness["completeness"]["status"], "complete", "{freshness}");

    client.cancel().await.ok();
}

/// Gate L — every pair this tool hands out is one another tool accepts. An address a
/// consumer cannot use is decoration.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_published_pair_addresses_a_file_diagnostics_serves() {
    let ws = stage_workspace();
    let client = client_with_references(ws.path()).await;

    let answer = references(&client, &[("symbol", Value::from(STAND_METHOD))]).await;
    let entries = answer["references"].as_array().expect("the list").clone();
    assert!(!entries.is_empty(), "an empty list would make this check vacuous");

    for entry in entries {
        let location = entry.get("location").unwrap_or_else(|| panic!("a place: {entry}"));
        assert_eq!(location["position_encoding"], "utf-16");
        assert_eq!(location["schema_version"], "1");
        let diagnostics = poll(
            &client,
            "diagnostics",
            args(&[
                ("action", Value::from("file")),
                ("root_id", location["root_id"].clone()),
                ("path", location["path"].clone()),
            ]),
        )
        .await;
        assert!(
            diagnostics["result"].get("error").is_none(),
            "the pair {location} was refused: {}",
            diagnostics["result"],
        );
    }

    client.cancel().await.ok();
}

/// Gate F — the envelope names whoever COMPOSED the body, not whoever was asked along the
/// way. The pair is the point: one input whose body the resident walked, one whose body is
/// dictionary candidates. A single input would pass against an implementation that stamps
/// the same source on everything.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn freshness_names_whoever_composed_the_body() {
    let ws = stage_workspace();
    let client = client_with_references(ws.path()).await;

    // Composed by the resident, though the anchor came from the dictionary: a short unique
    // name, answered with the resident's own reference walk.
    let short = references(&client, &[("symbol", Value::from("ВызватьПервый"))]).await;
    assert_eq!(short["outcome"], "resolved", "{short}");
    assert_eq!(short["freshness"]["source"], "resident", "{short}");
    assert!(!short["freshness"]["topology_fingerprint"].is_null(), "{short}");

    // Composed by the dictionary: nothing anchored, and the body IS its candidate list.
    let missing = references(&client, &[("symbol", Value::from("СовершенноНеизвестноеИмя"))]).await;
    assert_eq!(missing["outcome"], "not_found", "{missing}");
    assert_eq!(missing["freshness"]["source"], "name-dictionary", "{missing}");
    assert!(missing["freshness"]["revision"].is_null(), "{missing}");
    assert!(missing["freshness"]["topology_fingerprint"].is_null(), "{missing}");

    client.cancel().await.ok();
}

/// Gate O — a name that resolves to something with no reference walk is `unsupported_symbol`,
/// which is not `not_found` and not an empty list. The third input is the control: an
/// implementation answering `unsupported_symbol` for everything would pass the first two.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_symbol_without_a_reference_walk_says_so() {
    let ws = stage_workspace();
    let client = client_with_references(ws.path()).await;

    let object = references(&client, &[("symbol", Value::from("Справочник.Справочник1"))]).await;
    assert_eq!(object["outcome"], "unsupported_symbol", "{object}");
    assert!(object.get("references").is_none(), "no list at all, not an empty one: {object}");
    assert!(!object["unsupported"]["category"].as_str().unwrap().is_empty(), "{object}");

    let module = references(&client, &[("symbol", Value::from("ПервыйОбщийМодуль"))]).await;
    assert_eq!(module["outcome"], "unsupported_symbol", "a module as a whole: {module}");

    // The control: an exported method nobody calls is `resolved` with an empty list —
    // a proven zero, and the answer says so with a different word.
    let unused = references(
        &client,
        &[
            ("symbol", Value::from(STAND_METHOD)),
            ("include_declaration", Value::from(false)),
            ("kinds", Value::from(vec!["write"])),
        ],
    )
    .await;
    assert_eq!(unused["outcome"], "resolved", "{unused}");
    assert_eq!(unused["total"], 0, "nobody writes to a procedure: {unused}");
    assert_eq!(unused["references"].as_array().map(Vec::len), Some(0), "{unused}");

    client.cancel().await.ok();
}

/// Gate N — narrowing by area is a subset, and it selects by file identity rather than by
/// comparing path strings. The control is the wide answer: a filter that matched nothing
/// would leave both at zero and pass.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_area_narrows_the_answer_and_says_what_it_selected() {
    let ws = stage_workspace();
    let client = client_with_references(ws.path()).await;

    let wide = references(&client, &[("symbol", Value::from(STAND_METHOD))]).await;
    let wide_total = wide["total"].as_u64().expect("a total");

    let narrowed = references(
        &client,
        &[
            ("symbol", Value::from(STAND_METHOD)),
            ("area_path_prefix", Value::from("CommonModules/Первый")),
        ],
    )
    .await;

    assert_eq!(narrowed["outcome"], "resolved", "{narrowed}");
    // Exactly one, and the input is chosen so a plain `starts_with` would answer two:
    // `CommonModules/ПервыйОбщийМодуль` also begins with this prefix, and only a
    // segment-wise comparison keeps it out.
    assert_eq!(narrowed["total"], 1, "only the one caller: {narrowed}");
    assert!(narrowed["total"].as_u64().unwrap() < wide_total, "the filter must actually cut");
    // A prefix that names nothing would be indistinguishable from "no references" without
    // this count.
    assert!(narrowed["area"]["files_in_area"].as_u64().unwrap() >= 1, "{narrowed}");
    for entry in narrowed["references"].as_array().expect("the list") {
        let path = entry["location"]["path"].as_str().expect("a path");
        assert!(path.starts_with("CommonModules/Первый"), "{path} escaped the area");
    }

    client.cancel().await.ok();
}

/// Gate T — a display cap is declared as a reason, and the reason is not stuck on. The
/// control is the same query with room to spare.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_display_cap_is_declared_and_the_total_is_not_capped() {
    let ws = stage_workspace();
    let client = client_with_references(ws.path()).await;

    let capped =
        references(&client, &[("symbol", Value::from(STAND_METHOD)), ("limit", Value::from(1))])
            .await;
    assert_eq!(capped["references"].as_array().map(Vec::len), Some(1), "{capped}");
    assert_eq!(capped["total"], 6, "`total` is counted before `limit`: {capped}");
    let reasons = capped["freshness"]["completeness"]["reasons"]
        .as_array()
        .expect("reasons")
        .iter()
        .map(|reason| reason["code"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert!(reasons.contains(&"result_cap".to_owned()), "{capped}");
    // The histogram still covers everything the limit hid — that is what makes it a
    // replacement for a cursor.
    let sum: u64 = capped["files"]
        .as_array()
        .expect("a histogram")
        .iter()
        .map(|b| b["count"].as_u64().unwrap())
        .sum();
    assert_eq!(sum, 6, "{capped}");

    let roomy =
        references(&client, &[("symbol", Value::from(STAND_METHOD)), ("limit", Value::from(50))])
            .await;
    assert_eq!(roomy["freshness"]["completeness"]["status"], "complete", "{roomy}");

    client.cancel().await.ok();
}

/// An unregistered root is refused. The control is the call just above it: the
/// configuration's own id is accepted, so the refusal is about the unknown root and not
/// about the parameter existing at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unknown_root_is_refused_rather_than_silently_ignored() {
    let ws = stage_workspace();
    let client = client_with_references(ws.path()).await;

    // Reach a ready resident first: while it builds, every call is answered with the retry
    // envelope, and a refusal that never happened would look like one that did.
    let served = references(
        &client,
        &[("symbol", Value::from(STAND_METHOD)), ("area_root_id", Value::from(""))],
    )
    .await;
    assert_eq!(served["outcome"], "resolved", "{served}");

    let call = CallToolRequestParams::new(TOOL).with_arguments(args(&[
        ("symbol", Value::from(STAND_METHOD)),
        ("area_root_id", Value::from("no-such-root")),
    ]));
    let result = client.call_tool(call).await;
    assert!(
        result.is_err(),
        "an unregistered root must be refused, not answered with an empty list: {result:?}",
    );

    client.cancel().await.ok();
}

/// A short name two modules declare is `ambiguous`, and the answer carries the qualified
/// names that end the ambiguity. The control is feeding one of them back: it resolves, so
/// `ambiguous` is a step in a workflow and not a dead end.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_ambiguous_short_name_offers_the_names_that_resolve_it() {
    let ws = stage_workspace();
    let client = client_with_references(ws.path()).await;

    let ambiguous = references(&client, &[("symbol", Value::from("НеУстаревшаяФункция"))]).await;

    assert_eq!(ambiguous["outcome"], "ambiguous", "{ambiguous}");
    assert!(ambiguous.get("references").is_none(), "an ambiguous anchor counts nothing");
    // The body is the dictionary's candidate list, and it says so rather than borrowing the
    // resident's revision.
    assert_eq!(ambiguous["freshness"]["source"], "name-dictionary", "{ambiguous}");
    // `total` at the top level counts occurrences and nothing else. The dictionary's own
    // count lives under its own key, so a consumer reading `total` after branching on
    // `outcome` cannot pick up a number that means something different.
    assert!(ambiguous.get("total").is_none(), "top-level total is occurrences-only: {ambiguous}");
    assert!(ambiguous["lookup"]["total"].as_u64().unwrap() >= 2, "{ambiguous}");

    let candidates = ambiguous["lookup"]["candidates"].as_array().expect("candidates");
    assert!(candidates.len() >= 2, "two modules declare it: {candidates:?}");
    let qualified = candidates
        .iter()
        .filter_map(|c| c["address"]["symbol"].as_str())
        .find(|symbol| symbol.contains('.'))
        .expect("a candidate carries a qualified name")
        .to_owned();

    let resolved = references(&client, &[("symbol", Value::from(qualified.clone()))]).await;
    assert_eq!(resolved["outcome"], "resolved", "{qualified} did not resolve: {resolved}");

    client.cancel().await.ok();
}

/// Gate H — the histogram is what replaces a cursor, so its own truncation is named apart
/// from the body's: a shortened histogram loses whole files, and a caller walking it would
/// never learn of them from the reference list. The control is the same query with the
/// default budget, where the sum covers the total exactly.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_truncated_histogram_is_named_apart_from_a_truncated_list() {
    let ws = stage_workspace();
    let client = client_with_references(ws.path()).await;

    let squeezed = references(
        &client,
        &[("symbol", Value::from(STAND_METHOD)), ("max_output_tokens", Value::from(40))],
    )
    .await;

    assert_eq!(squeezed["outcome"], "resolved", "{squeezed}");
    assert_eq!(squeezed["total"], 6, "the count survives the budget: {squeezed}");
    assert_eq!(squeezed["histogram_truncated"], true, "{squeezed}");
    let sum: u64 = squeezed["files"]
        .as_array()
        .expect("a histogram")
        .iter()
        .map(|bucket| bucket["count"].as_u64().unwrap())
        .sum();
    assert!(sum < 6, "a cut histogram must not still add up to the total: {squeezed}");
    let reasons = reason_codes(&squeezed);
    assert!(reasons.contains(&"output_budget".to_owned()), "{squeezed}");

    let whole = references(&client, &[("symbol", Value::from(STAND_METHOD))]).await;
    assert_eq!(whole["histogram_truncated"], false, "control: {whole}");
    let whole_sum: u64 = whole["files"]
        .as_array()
        .expect("a histogram")
        .iter()
        .map(|bucket| bucket["count"].as_u64().unwrap())
        .sum();
    assert_eq!(whole_sum, 6, "control: an untruncated histogram covers the total: {whole}");

    client.cancel().await.ok();
}

/// Gate M — a walk stopped by `max_files` says so twice: `total` becomes a lower bound and
/// the answer declares itself incomparable with a narrower one. The control is the same
/// query with room to finish, where both flags clear.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_truncated_walk_declares_that_its_total_is_a_floor() {
    let ws = stage_workspace();
    let client = client_with_references(ws.path()).await;

    let capped = references(
        &client,
        &[("symbol", Value::from(STAND_METHOD)), ("max_files", Value::from(1))],
    )
    .await;

    assert_eq!(capped["outcome"], "resolved", "{capped}");
    assert_eq!(capped["files_scanned"], 1, "{capped}");
    assert_eq!(capped["total_is_lower_bound"], true, "{capped}");
    assert_eq!(capped["narrowing_comparable"], false, "{capped}");
    assert!(capped["total"].as_u64().unwrap() < 6, "a partial walk cannot see everything");
    assert!(reason_codes(&capped).contains(&"result_cap".to_owned()), "{capped}");

    let full = references(&client, &[("symbol", Value::from(STAND_METHOD))]).await;
    assert_eq!(full["total_is_lower_bound"], false, "control: {full}");
    assert_eq!(full["narrowing_comparable"], true, "control: {full}");
    assert_eq!(full["total"], 6, "control: {full}");

    client.cancel().await.ok();
}

fn reason_codes(answer: &Value) -> Vec<String> {
    answer["freshness"]["completeness"]["reasons"]
        .as_array()
        .expect("reasons")
        .iter()
        .map(|reason| reason["code"].as_str().expect("a code").to_owned())
        .collect()
}

/// Gate D — narrowing survives a root whose DECLARED spelling differs from the one the
/// files are indexed under. A filter built by joining the declared root onto a prefix and
/// comparing strings matches nothing here, and answers `resolved` with an empty list: the
/// silent zero this whole tool exists to abolish, returned through its own filter.
///
/// The control is the second half: the same stand opened by its real path, where the two
/// spellings coincide and any implementation passes.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn narrowing_survives_a_root_declared_through_a_symlink() {
    let ws = stage_workspace();
    let link_home = TempDir::new().expect("scratch dir for the link");
    let link = link_home.path().join("проект");
    std::os::unix::fs::symlink(ws.path(), &link).expect("symlink the workspace");

    let through_link = client_with_references(&link).await;
    let narrowed = references(
        &through_link,
        &[
            ("symbol", Value::from(STAND_METHOD)),
            ("area_root_id", Value::from("")),
            ("area_path_prefix", Value::from("CommonModules")),
        ],
    )
    .await;
    assert_eq!(narrowed["outcome"], "resolved", "{narrowed}");
    assert_eq!(
        narrowed["total"], 6,
        "every hit lives under CommonModules, so the filter must keep them all: {narrowed}",
    );
    assert!(narrowed["area"]["files_in_area"].as_u64().unwrap() > 1, "{narrowed}");
    through_link.cancel().await.ok();

    let direct = client_with_references(ws.path()).await;
    let control = references(
        &direct,
        &[
            ("symbol", Value::from(STAND_METHOD)),
            ("area_root_id", Value::from("")),
            ("area_path_prefix", Value::from("CommonModules")),
        ],
    )
    .await;
    assert_eq!(control["total"], narrowed["total"], "the link must not change the answer");
    direct.cancel().await.ok();
}

/// Does this volume keep two names that differ only in case apart?
///
/// Asked of the volume the stand actually sits on, because that is the fact the stand needs:
/// a case-sensitive APFS volume answers yes on macOS, and a folding volume answers no
/// wherever it is mounted.
#[cfg(unix)]
fn fs_keeps_case_distinct(dir: &Path) -> bool {
    let probe = dir.join("bsl_case_distinction_probe");
    if std::fs::write(&probe, "").is_err() {
        return false;
    }
    let distinct = std::fs::metadata(dir.join("BSL_CASE_DISTINCTION_PROBE")).is_err();
    let _ = std::fs::remove_file(&probe);
    distinct
}

/// Two declarations in ONE root cannot be told apart by a root filter, and the answer must
/// not send an agent down that road. The control is the two-root case, where the root
/// filter is exactly the right advice.
///
/// Staging it needs a case-sensitive filesystem: the two spellings are the same directory on
/// a folding volume, and the stand would quietly become a single module answering
/// `resolved` — a gate that cannot fail. The VOLUME is asked rather than the target OS: a
/// case-sensitive APFS volume is a configuration chosen at format time, and there this stand
/// is staged exactly as it is on Linux, so gating by `target_os` would drop the coverage on
/// a machine that can carry it. Left declared rather than degraded, and the stand still
/// asserts its own precondition before it asserts anything about the hint.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_resolution_hint_names_an_axis_that_can_separate_these_declarations() {
    let ws = stage_workspace();
    if !fs_keeps_case_distinct(ws.path()) {
        return;
    }
    // One root, two directories whose names differ only in case: a module path is folded to
    // its key, so both files answer to `Стенд.ОбщийМетод`, and no root filter stands between
    // them. This is the spelling divergence a case-sensitive filesystem makes real, not an
    // invented stand.
    for spelling in ["Стенд", "СТЕНД"] {
        write_module(ws.path(), spelling, "Процедура ОбщийМетод() Экспорт\nКонецПроцедуры\n");
    }
    let staged = std::fs::read_dir(ws.path().join("CommonModules"))
        .expect("the stand's modules")
        .filter_map(Result::ok)
        // Cyrillic, so `eq_ignore_ascii_case` would fold nothing and count zero.
        .filter(|entry| entry.file_name().to_string_lossy().to_lowercase() == "стенд")
        .count();
    assert_eq!(
        staged, 2,
        "this filesystem folded the two spellings into one directory, so the ambiguity this \
         gate is about was never staged",
    );

    let client = client_with_references(ws.path()).await;
    let answer = references(&client, &[("symbol", Value::from("Стенд.ОбщийМетод"))]).await;

    assert_eq!(answer["outcome"], "ambiguous", "{answer}");
    let declarations = answer["declarations"].as_array().expect("declarations");
    assert_eq!(declarations.len(), 2, "{answer}");
    let roots: Vec<&str> =
        declarations.iter().filter_map(|d| d["location"]["root_id"].as_str()).collect();
    assert_eq!(roots, ["", ""], "the stand must put both declarations in one root: {answer}");
    let hint = answer["resolution_hint"].as_str().expect("a hint");
    // A published machine field, so its text is checked as a value: an alignment run left
    // inside the literal travels to the agent and into the tool's own documentation.
    assert!(
        !hint.contains("  "),
        "a run of spaces from a wrapped literal leaked into the hint: {hint:?}",
    );
    assert!(
        !hint.contains("anchor_root_id"),
        "a root filter cannot separate declarations sharing a root: {hint}",
    );
    assert!(hint.contains("path"), "the hint must name an axis that works: {hint}");

    client.cancel().await.ok();
}

/// Gate F, the pair the first edition of it lacked: an `unsupported_symbol` composed by the
/// dictionary carries the dictionary's own answer, and one composed by the resident does
/// not pretend to. An envelope that names `name-dictionary` and shows none of what it could
/// consult is an identity without its evidence.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
async fn an_unsupported_answer_carries_the_sources_that_composed_it() {
    let ws = stage_workspace();
    let client = client_with_references(ws.path()).await;

    // Composed by the dictionary: a short exact name of a platform member.
    let platform = references(&client, &[("symbol", Value::from("Добавить"))]).await;
    assert_eq!(platform["outcome"], "unsupported_symbol", "{platform}");
    assert_eq!(platform["freshness"]["source"], "name-dictionary", "{platform}");
    let providers = platform["lookup"]["providers"].as_array().unwrap_or_else(|| {
        panic!("a name-dictionary envelope must show its providers: {platform}")
    });
    assert!(!providers.is_empty(), "{platform}");

    // Composed by the resident: the category came from the resident's own resolution, and
    // there is no dictionary answer to show.
    let object = references(&client, &[("symbol", Value::from("Справочник.Справочник1"))]).await;
    assert_eq!(object["outcome"], "unsupported_symbol", "{object}");
    assert_eq!(object["freshness"]["source"], "resident", "{object}");
    assert!(object.get("lookup").is_none(), "nothing was asked of the dictionary: {object}");

    client.cancel().await.ok();
}

/// `histogram_truncated` says whole files are missing from the histogram. A single bucket
/// that is itself larger than what the budget left is not that: nothing was lost, and the
/// counts still add up. The control is the multi-file stand where buckets really are
/// dropped — there the flag must fire.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_kept_oversized_bucket_is_not_a_truncated_histogram() {
    let ws = stage_workspace();
    let client = client_with_references(ws.path()).await;

    let one_file = references(
        &client,
        &[("symbol", Value::from("Первый.ВызватьПервый")), ("max_output_tokens", Value::from(1))],
    )
    .await;

    assert_eq!(one_file["outcome"], "resolved", "{one_file}");
    let buckets = one_file["files"].as_array().expect("a histogram");
    assert_eq!(buckets.len(), 1, "the stand must have exactly one file with hits: {one_file}");
    let sum: u64 = buckets.iter().map(|b| b["count"].as_u64().unwrap()).sum();
    assert_eq!(sum, one_file["total"].as_u64().unwrap(), "nothing was dropped: {one_file}");
    assert_eq!(
        one_file["histogram_truncated"], false,
        "no file is missing from this histogram: {one_file}",
    );
    // The overflow itself is still declared — as a budget reason, which is what it is.
    assert!(reason_codes(&one_file).contains(&"output_budget".to_owned()), "{one_file}");

    client.cancel().await.ok();
}

/// The output budget is a promise of the parameter, not a property of one branch: an answer
/// that resolved nothing still has to fit it, and to say when it did not. The control is the
/// same query with room to spare.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_budget_applies_to_an_answer_that_resolved_nothing() {
    let ws = stage_workspace();
    let client = client_with_references(ws.path()).await;

    let squeezed = references(
        &client,
        &[("symbol", Value::from("НеУстаревшаяФункция")), ("max_output_tokens", Value::from(20))],
    )
    .await;
    assert_eq!(squeezed["outcome"], "ambiguous", "{squeezed}");
    assert!(
        reason_codes(&squeezed).contains(&"output_budget".to_owned()),
        "an over-budget candidate list must say so: {squeezed}",
    );

    let roomy = references(&client, &[("symbol", Value::from("НеУстаревшаяФункция"))]).await;
    assert_eq!(roomy["outcome"], "ambiguous", "{roomy}");
    assert!(
        !reason_codes(&roomy).contains(&"output_budget".to_owned()),
        "control: with room, no budget reason: {roomy}",
    );

    client.cancel().await.ok();
}

/// A variable a body never declares is still one variable. Every occurrence must answer
/// with the same complete list — an answer that carries `resolved`, a `total` and
/// `total_is_lower_bound: false` claims a proven zero for everything it leaves out, so
/// half the occurrences returned that way is a wrong answer, not a partial one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_implicit_local_answers_the_same_from_every_occurrence() {
    let ws = stage_fixture();
    write_module(
        ws.path(),
        "Первый",
        "Процедура Тест() Экспорт\n    \
         Набор = 1;\n    Сообщить(Набор);\n    Набор = 2;\n    Сообщить(Набор);\n\
         КонецПроцедуры\n",
    );
    let client = client_with_references(ws.path()).await;

    let path = "CommonModules/Первый/Ext/Module.bsl";
    // The two writes and the two reads, 0-based: the name starts at column 4 on a write
    // line and at column 13 on a `Сообщить(` line.
    for (line, column) in [(1, 4), (2, 13), (3, 4), (4, 13)] {
        let answer = references(
            &client,
            &[
                ("root_id", Value::from("")),
                ("path", Value::from(path)),
                ("line", Value::from(line)),
                ("column", Value::from(column)),
            ],
        )
        .await;

        assert_eq!(answer["outcome"], "resolved", "at {line}:{column}: {answer}");
        assert_eq!(answer["total"], 4, "at {line}:{column}: {answer}");
        assert_eq!(answer["total_is_lower_bound"], false, "at {line}:{column}: {answer}");
        assert_eq!(
            answer["freshness"]["completeness"]["status"], "complete",
            "at {line}:{column}: {answer}"
        );
    }

    client.cancel().await.ok();
}

/// A parameter that is validated and then ignored is worse than one that is refused: the
/// caller learns its root is unknown but never that its narrowing did nothing. The control
/// is the same positional call without it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_anchor_root_with_a_positional_anchor_is_refused() {
    let ws = stage_workspace();
    let client = client_with_references(ws.path()).await;

    let path = "CommonModules/Первый/Ext/Module.bsl";
    let control = references(
        &client,
        &[
            ("root_id", Value::from("")),
            ("path", Value::from(path)),
            ("line", Value::from(0)),
            // Column 10: the declared name, past the `Процедура` keyword.
            ("column", Value::from(10)),
        ],
    )
    .await;
    assert_eq!(control["outcome"], "resolved", "control: the positional anchor works: {control}");

    let call = CallToolRequestParams::new(TOOL).with_arguments(args(&[
        ("root_id", Value::from("")),
        ("path", Value::from(path)),
        ("line", Value::from(0)),
        ("column", Value::from(10)),
        ("anchor_root_id", Value::from("")),
    ]));
    let refused = client.call_tool(call).await;
    assert!(
        refused.is_err(),
        "a position already names one file; a declaration root cannot also apply: {refused:?}",
    );

    client.cancel().await.ok();
}

/// A candidate list the budget shortened must say so where the promise lives. `truncated`
/// and `total` under `lookup` are the dictionary's own contract — "this list is complete
/// against that count" — and trimming the list from outside without touching the flag
/// leaves the two numbers to betray it. The control is the same query with room, where the
/// list is whole and the flag is false.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_budget_trimmed_candidate_list_declares_itself_incomplete() {
    let ws = stage_workspace();
    let client = client_with_references(ws.path()).await;

    let squeezed = references(
        &client,
        &[("symbol", Value::from("НеУстаревшаяФункция")), ("max_output_tokens", Value::from(20))],
    )
    .await;
    assert_eq!(squeezed["outcome"], "ambiguous", "{squeezed}");
    let shown = squeezed["lookup"]["candidates"].as_array().expect("candidates").len() as u64;
    let total = squeezed["lookup"]["total"].as_u64().expect("a dictionary total");
    assert!(
        shown < total,
        "the input must actually lose a candidate, or this gate passes on any code: {squeezed}",
    );
    assert_eq!(
        squeezed["lookup"]["truncated"], true,
        "a list shorter than its own total is truncated, whoever shortened it: {squeezed}",
    );

    let roomy = references(&client, &[("symbol", Value::from("НеУстаревшаяФункция"))]).await;
    let whole = roomy["lookup"]["candidates"].as_array().expect("candidates").len() as u64;
    assert_eq!(whole, roomy["lookup"]["total"].as_u64().unwrap(), "control: {roomy}");
    assert_eq!(roomy["lookup"]["truncated"], false, "control: nothing was cut: {roomy}");

    client.cancel().await.ok();
}

/// An ambiguity the name dictionary composed has an axis of its own — the qualified name of
/// a candidate, fed back as `symbol` — and it is not a root: the candidates are spread over
/// kinds, not over roots. Without the field the agent falls back to the tool description,
/// which cannot know which ambiguity it got. The control is the declaration-based ambiguity
/// next door, whose hint names a different axis for the same field.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dictionary_ambiguity_carries_its_own_resolution_hint() {
    let ws = stage_workspace();
    let client = client_with_references(ws.path()).await;

    let ambiguous = references(&client, &[("symbol", Value::from("НеУстаревшаяФункция"))]).await;
    assert_eq!(ambiguous["outcome"], "ambiguous", "{ambiguous}");
    assert_eq!(ambiguous["freshness"]["source"], "name-dictionary", "{ambiguous}");
    let hint = ambiguous["resolution_hint"].as_str().expect("a hint: {ambiguous}");
    assert!(
        hint.contains("symbol"),
        "the axis out of a dictionary ambiguity is a qualified name: {hint}",
    );
    assert!(
        !hint.contains("anchor_root_id"),
        "these candidates are not separated by a root: {hint}",
    );

    client.cancel().await.ok();
}

/// `root_id` spells out which root a `path` is relative to, and beside a `symbol` it has
/// nothing to qualify: a name is resolved across every root. Accepting it there would report
/// a narrowing that never happened — and `symbol_info` refuses the same pair for the same
/// reason. The controls are both halves apart: the symbol alone, and the root beside a path.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_root_beside_a_symbol_is_refused_rather_than_dropped() {
    let ws = stage_workspace();
    let client = client_with_references(ws.path()).await;

    let control = references(&client, &[("symbol", Value::from(STAND_METHOD))]).await;
    assert_eq!(control["outcome"], "resolved", "control: the symbol alone answers: {control}");

    let call = CallToolRequestParams::new(TOOL).with_arguments(args(&[
        ("symbol", Value::from(STAND_METHOD)),
        ("root_id", Value::from("")),
    ]));
    assert!(
        client.call_tool(call).await.is_err(),
        "a root that qualifies nothing must be refused, not silently dropped",
    );

    let positional = references(
        &client,
        &[
            ("root_id", Value::from("")),
            ("path", Value::from("CommonModules/Первый/Ext/Module.bsl")),
            ("line", Value::from(0)),
            ("column", Value::from(10)),
        ],
    )
    .await;
    assert!(
        positional.get("outcome").is_some(),
        "control: the same root beside a path is what the parameter is for: {positional}",
    );

    // The same rule for the other half of a positional anchor: `line` narrows where the
    // walk starts, and beside a name there is nothing for it to narrow.
    let with_line = CallToolRequestParams::new(TOOL)
        .with_arguments(args(&[("symbol", Value::from(STAND_METHOD)), ("line", Value::from(0))]));
    assert!(
        client.call_tool(with_line).await.is_err(),
        "a line that positions nothing must be refused, not silently dropped",
    );

    client.cancel().await.ok();
}

/// The stand the root filter needs: two roots, each holding a caller of ONE declaration.
/// A single-root stand cannot tell a working root filter from one that never runs, because
/// `""` selects everything either way. The extension lives in its own scratch dir — a
/// subdirectory of the configuration would be walked by the configuration root too, and
/// then no file would be exclusive to either root.
fn stage_two_roots() -> (TempDir, TempDir) {
    let ws = stage_workspace();
    let ext = TempDir::new().expect("scratch extension");
    // A declared extension is recognised by its `Configuration.xml`, and without one the
    // topology refuses the declaration outright.
    std::fs::copy(ws.path().join("Configuration.xml"), ext.path().join("Configuration.xml"))
        .expect("the extension needs a configuration file to be one");
    write_module(
        ext.path(),
        "Расширение",
        "Процедура ВызватьИзРасширения() Экспорт\n    \
         ПервыйОбщийМодуль.НеУстаревшаяФункция();\nКонецПроцедуры\n",
    );
    let path = ext.path();
    std::fs::write(
        ws.path().join("bsl-analyzer.toml"),
        format!("[source]\nroot = \".\"\nextensions = [{{ name = \"расш\", path = {path:?} }}]\n"),
    )
    .expect("declare the extension");
    (ws, ext)
}

/// Gate N — `area_root_id` names a root and the answer holds references from that root
/// alone. Every other root check in this file passes `""`, which selects the whole
/// workspace: a `select` that validated the id against the root table and then filtered
/// nothing would pass all of them. The control is the same query against the other root,
/// whose hits are disjoint from these.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_area_root_selects_one_root_and_excludes_the_other() {
    let (ws, _ext) = stage_two_roots();
    let client = client_with_references(ws.path()).await;

    let everywhere = references(&client, &[("symbol", Value::from(STAND_METHOD))]).await;
    assert_eq!(everywhere["outcome"], "resolved", "{everywhere}");
    let roots: Vec<String> = everywhere["files"]
        .as_array()
        .expect("a histogram")
        .iter()
        .map(|bucket| bucket["location"]["root_id"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert!(
        roots.iter().any(String::is_empty),
        "the configuration must contribute hits: {roots:?}",
    );
    // Taken from the answer, not spelled here: a `root_id` a location publishes is the one
    // the tools accept back, and that is the pair this gate is about.
    let extension_root = roots
        .iter()
        .find(|root| !root.is_empty())
        .expect("the stand must span both roots, or the filter has nothing to exclude")
        .to_owned();

    let extension = references(
        &client,
        &[
            ("symbol", Value::from(STAND_METHOD)),
            ("area_root_id", Value::from(extension_root.clone())),
        ],
    )
    .await;
    let configuration = references(
        &client,
        &[("symbol", Value::from(STAND_METHOD)), ("area_root_id", Value::from(""))],
    )
    .await;

    let files_of = |answer: &Value| -> Vec<String> {
        answer["files"]
            .as_array()
            .expect("a histogram")
            .iter()
            .map(|bucket| {
                format!(
                    "{}:{}",
                    bucket["location"]["root_id"].as_str().unwrap_or_default(),
                    bucket["location"]["path"].as_str().unwrap_or_default(),
                )
            })
            .collect()
    };
    let from_extension = files_of(&extension);
    let from_configuration = files_of(&configuration);

    assert!(!from_extension.is_empty(), "the extension calls it: {extension}");
    assert!(!from_configuration.is_empty(), "so does the configuration: {configuration}");
    assert!(
        from_extension.iter().all(|file| file.starts_with(&format!("{extension_root}:"))),
        "a root filter that let another root through selected nothing: {from_extension:?}",
    );
    assert!(
        from_configuration.iter().all(|file| file.starts_with(':')),
        "the control must be just as exclusive: {from_configuration:?}",
    );
    assert_eq!(
        extension["total"].as_u64().unwrap() + configuration["total"].as_u64().unwrap(),
        everywhere["total"].as_u64().unwrap(),
        "the two roots partition the answer: {extension} / {configuration}",
    );

    client.cancel().await.ok();
}

/// A name declared after the resident was built must not come back `not_found`: a caller
/// reads that outcome as final. Two mechanisms can keep the promise — the change hub, and
/// the forced re-scan this tool does on a miss the way `symbol_info` does — and this gate
/// holds whichever of them ran; it does not single out the second. It does not have to:
/// the re-scan and the read behind it are coloured on their own by
/// `references_resolves_a_late_declaration_only_behind_the_forced_retry`, on a stand with
/// no hub to deliver anything, and the floor the re-scan waits out by
/// `a_force_the_storm_guard_declines_is_waited_out_and_asked_again` beside it. What is
/// measured HERE is the promise, not the mechanism — asked until it is kept, because both
/// mechanisms run on clocks this test does not hold.
/// The control is a name nothing ever declared, which stays `not_found` however many
/// re-scans it takes — and it is asked ONCE, because there is nothing for a second ask to
/// wait for.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_name_declared_after_the_last_scan_is_found_on_a_forced_retry() {
    let ws = stage_workspace();
    let client = client_with_references(ws.path()).await;

    // Reach a ready resident first, so what follows is a stale scan and not a cold start.
    let ready = references(&client, &[("symbol", Value::from(STAND_METHOD))]).await;
    assert_eq!(ready["outcome"], "resolved", "{ready}");

    let module = ws.path().join("CommonModules").join("Первый").join("Ext").join("Module.bsl");
    let text = std::fs::read_to_string(&module).expect("the stand module");
    std::fs::write(
        &module,
        format!("{text}\nПроцедура ДобавленнаяПозже() Экспорт\nКонецПроцедуры\n"),
    )
    .expect("declare a method after the resident was built");

    let declared = [("symbol", Value::from("Первый.ДобавленнаяПозже"))];
    common::settled(
        "a method that exists on disk must not be reported missing",
        |answer| answer["outcome"] == "resolved",
        || references(&client, &declared),
    )
    .await;

    let never = references(&client, &[("symbol", Value::from("Первый.НикогдаНеБыло"))]).await;
    assert_eq!(
        never["outcome"], "not_found",
        "control: a re-scan finds nothing that is not there: {never}"
    );

    client.cancel().await.ok();
}

// --- the quoted-line anchor ---------------------------------------------------------------

/// The stand the quoted line is exercised on. Two methods declare a parameter of the same
/// name, so one quote matches two DIFFERENT symbols; the third holds a call, so a quote can
/// also resolve to something with references across the workspace.
const ANCHOR_MODULE: &str = "\
Процедура Первая(Значение) Экспорт
    Значение = 1;
КонецПроцедуры

Процедура Вторая(Значение) Экспорт
    Значение = 2;
КонецПроцедуры

Процедура Третья() Экспорт
    ПервыйОбщийМодуль.НеУстаревшаяФункция();
КонецПроцедуры
";

const ANCHOR_PATH: &str = "CommonModules/Якорь/Ext/Module.bsl";

/// 0-based line of the call inside [`ANCHOR_MODULE`].
const ANCHOR_CALL_LINE: u32 = 9;

const ANCHOR_CALL_QUOTE: &str = "ПервыйОбщийМодуль.НеУстаревшаяФункция()";

fn stage_anchor_workspace() -> TempDir {
    let dst = stage_fixture();
    write_module(dst.path(), "Якорь", ANCHOR_MODULE);
    dst
}

fn anchor_args(pairs: &[(&str, Value)]) -> Vec<(&'static str, Value)> {
    let mut all: Vec<(&'static str, Value)> =
        vec![("root_id", Value::from("")), ("path", Value::from(ANCHOR_PATH))];
    for (key, value) in pairs {
        let key: &'static str = Box::leak((*key).to_owned().into_boxed_str());
        all.push((key, value.clone()));
    }
    all
}

/// Gate I7 (wire) — a quote found somewhere other than the line it came with still answers,
/// and the answer names the line it moved from. Without the field the relocation is silent
/// again, one level up from the symbol.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_quote_found_on_another_line_says_where_it_moved_from() {
    let ws = stage_anchor_workspace();
    let client = client_with_references(ws.path()).await;

    let relocated = references(
        &client,
        &anchor_args(&[
            // The line the caller remembers, three methods above where the quote now lives.
            ("line", Value::from(0)),
            ("line_content", Value::from(ANCHOR_CALL_QUOTE)),
        ]),
    )
    .await;
    assert_eq!(relocated["outcome"], "resolved", "{relocated}");
    assert_eq!(relocated["anchor"]["mode"], "line_content");
    assert_eq!(relocated["anchor"]["line"], ANCHOR_CALL_LINE);
    assert_eq!(
        relocated["anchor"]["relocated_from_line"], 0,
        "an anchor that stood elsewhere has to say so: {relocated}",
    );

    // The control: the same call with the line the quote is actually on. Same answer, and
    // nothing moved, so the field is absent rather than equal to the line.
    let exact = references(
        &client,
        &anchor_args(&[
            ("line", Value::from(ANCHOR_CALL_LINE)),
            ("line_content", Value::from(ANCHOR_CALL_QUOTE)),
        ]),
    )
    .await;
    assert_eq!(exact["outcome"], "resolved", "{exact}");
    assert_eq!(exact["anchor"]["line"], ANCHOR_CALL_LINE);
    assert!(
        exact["anchor"].get("relocated_from_line").is_none(),
        "nothing moved, so nothing is reported: {exact}",
    );
    assert_eq!(exact["total"], relocated["total"], "the relocation changed no count");

    client.cancel().await.ok();
}

/// Gate I8 (wire) — a quote that names two symbols is an ambiguity WITH the places to choose
/// from. An ambiguity without them is the same silence the tool exists to end, and the two
/// lists this tool already had (`declarations`, `lookup`) are both empty on this path.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_ambiguous_quote_carries_the_places_to_choose_from() {
    let ws = stage_anchor_workspace();
    let client = client_with_references(ws.path()).await;

    let ambiguous =
        references(&client, &anchor_args(&[("line_content", Value::from("Значение = "))])).await;

    assert_eq!(ambiguous["outcome"], "ambiguous", "{ambiguous}");
    let sites = ambiguous["anchor_sites"].as_array().expect("the places to choose from");
    assert_eq!(sites.len(), 2, "two methods, two parameters, two symbols: {ambiguous}");
    for site in sites {
        assert!(site["location"].is_object(), "each place is addressable: {site}");
        assert!(site["snippet"].is_string(), "each place quotes its own line: {site}");
    }
    assert!(
        ambiguous["resolution_hint"].as_str().is_some_and(|hint| hint.contains("line_content")),
        "the hint names the axis that separates THESE: {ambiguous}",
    );

    // The control: a quote over the same file that names one symbol carries no list.
    let resolved =
        references(&client, &anchor_args(&[("line_content", Value::from(ANCHOR_CALL_QUOTE))]))
            .await;
    assert_eq!(resolved["outcome"], "resolved", "{resolved}");
    assert!(resolved.get("anchor_sites").is_none(), "nothing to choose from: {resolved}");

    client.cancel().await.ok();
}

/// Gate I8 (wire) — the main input of the promise: a caller that read the file and does not
/// trust its own line numbers sends the text alone. Before this stage `build_anchor` refused
/// it outright, so a contract naming the input would have promised an `invalid_params`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_quote_without_a_line_is_a_working_anchor() {
    let ws = stage_anchor_workspace();
    let client = client_with_references(ws.path()).await;

    let answer =
        references(&client, &anchor_args(&[("line_content", Value::from(ANCHOR_CALL_QUOTE))]))
            .await;
    assert_eq!(answer["outcome"], "resolved", "{answer}");
    assert_eq!(answer["anchor"]["mode"], "line_content");
    assert_eq!(answer["anchor"]["line"], ANCHOR_CALL_LINE);

    // The control: a path with neither a line nor a quote addresses nothing, and is still
    // refused. The requirement was lifted for the quote, not dropped.
    let bare = CallToolRequestParams::new(TOOL)
        .with_arguments(args(&[("root_id", Value::from("")), ("path", Value::from(ANCHOR_PATH))]));
    assert!(
        client.call_tool(bare).await.is_err(),
        "a path alone names a file, not an occurrence in it",
    );

    client.cancel().await.ok();
}

/// Gate I10 — the positional anchor is untouched: it still resolves, and it now says which
/// anchor answered. The control is the same occurrence reached by its quote.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_positional_anchor_answers_as_it_did_and_calls_itself_unverified() {
    let ws = stage_anchor_workspace();
    let client = client_with_references(ws.path()).await;

    let positional = references(
        &client,
        &anchor_args(&[("line", Value::from(ANCHOR_CALL_LINE)), ("column", Value::from(22))]),
    )
    .await;
    assert_eq!(positional["outcome"], "resolved", "{positional}");
    assert_eq!(positional["anchor"]["mode"], "position");
    assert_eq!(positional["anchor"]["line"], ANCHOR_CALL_LINE);
    assert!(
        positional["anchor"].get("relocated_from_line").is_none(),
        "a position stands where it was told to: {positional}",
    );

    let quoted =
        references(&client, &anchor_args(&[("line_content", Value::from(ANCHOR_CALL_QUOTE))]))
            .await;
    assert_eq!(quoted["anchor"]["mode"], "line_content");
    assert_eq!(
        positional["references"], quoted["references"],
        "the two anchors reach the same occurrence and answer the same list",
    );

    client.cancel().await.ok();
}

/// Gate I12 — `anchor_root_id` is refused beside EVERY anchor that names a file, not just
/// beside the positional one. A condition written against one variant lets the next one
/// through, and a root validated and then dropped tells the caller its narrowing was
/// understood while nothing narrowed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_declaration_root_beside_a_quoted_line_is_refused() {
    let ws = stage_anchor_workspace();
    let client = client_with_references(ws.path()).await;

    let control =
        references(&client, &anchor_args(&[("line_content", Value::from(ANCHOR_CALL_QUOTE))]))
            .await;
    assert_eq!(control["outcome"], "resolved", "control: the quote alone works: {control}");

    let refused = CallToolRequestParams::new(TOOL).with_arguments(args(&anchor_args(&[
        ("line_content", Value::from(ANCHOR_CALL_QUOTE)),
        ("anchor_root_id", Value::from("")),
    ])));
    assert!(
        client.call_tool(refused).await.is_err(),
        "a quoted line already names one file; a declaration root cannot also apply",
    );

    // The control for the other half: the refusal the positional anchor already had is
    // still there, so the condition was widened and not moved.
    let positional = CallToolRequestParams::new(TOOL).with_arguments(args(&anchor_args(&[
        ("line", Value::from(ANCHOR_CALL_LINE)),
        ("anchor_root_id", Value::from("")),
    ])));
    assert!(client.call_tool(positional).await.is_err(), "the old refusal is unchanged");

    client.cancel().await.ok();
}

/// Gate I9/I6 (wire) — a quote the file does not carry is `anchor_stale`, with the code, the
/// count of lines that still carry it, and what stands at the line that was asked for.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_quote_the_file_lost_is_answered_as_a_stale_anchor() {
    let ws = stage_anchor_workspace();
    let client = client_with_references(ws.path()).await;

    let stale = references(
        &client,
        &anchor_args(&[
            ("line", Value::from(ANCHOR_CALL_LINE)),
            ("line_content", Value::from("ПервыйОбщийМодуль.ФункцииТакойНет()")),
        ]),
    )
    .await;

    assert_eq!(stale["outcome"], "anchor_stale", "{stale}");
    assert_eq!(stale["anchor_stale"]["reason"], "not_in_file");
    assert_eq!(stale["anchor_stale"]["line_content_matches"], 0);
    assert_eq!(
        stale["anchor_stale"]["actual_line"], "    ПервыйОбщийМодуль.НеУстаревшаяФункция();",
        "what stands there now, so the caller sees which picture moved: {stale}",
    );
    assert!(stale.get("references").is_none(), "a stale anchor counts nothing: {stale}");
    // The envelope still names the revision the caller diverged from — the whole reason
    // this is an outcome and not a transport error.
    assert!(stale["freshness"]["revision"].is_number(), "{stale}");

    // The control: the same call with the text the file does carry.
    let resolved = references(
        &client,
        &anchor_args(&[
            ("line", Value::from(ANCHOR_CALL_LINE)),
            ("line_content", Value::from(ANCHOR_CALL_QUOTE)),
        ]),
    )
    .await;
    assert_eq!(resolved["outcome"], "resolved", "{resolved}");

    client.cancel().await.ok();
}

// --- the preview ---------------------------------------------------------------------------

/// How many callers the budget gate needs. The number is not decoration: the whole point is
/// an answer that fills `limit` AND leaves the histogram fighting for what is left, because
/// at six occurrences and a 6000-token budget every possible order of assembly produces the
/// same six records and the gate would be green on a wrong implementation.
const CROWD: usize = 60;

fn stage_crowded_workspace() -> TempDir {
    let dst = stage_fixture();
    for index in 0..CROWD {
        write_module(
            dst.path(),
            &format!("Толпа{index:02}"),
            &format!(
                "Процедура Вызвать{index:02}() Экспорт\n    \
                 ПервыйОбщийМодуль.НеУстаревшаяФункция();\nКонецПроцедуры\n"
            ),
        );
    }
    dst
}

/// The `location` + `kind` of every record, which is what a reference IS. Previews are a
/// caption on top and must not move any of it.
fn skeleton(answer: &Value) -> Vec<Value> {
    answer["references"]
        .as_array()
        .expect("a resolved answer carries the list")
        .iter()
        .map(|entry| serde_json::json!({ "location": entry["location"], "kind": entry["kind"] }))
        .collect()
}

/// Gate I1 — switching previews on changes nothing about WHAT the answer contains.
///
/// The stand has to press against the budget for this to mean anything: with `limit: 50`
/// over sixty-odd occurrences the histogram lives on whatever the list leaves, so an
/// implementation that hangs previews on the records before the histogram is built pays for
/// a decoration with the only way past `limit` there is.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_preview_changes_no_part_of_the_answer_it_decorates() {
    let ws = stage_crowded_workspace();
    let client = client_with_references(ws.path()).await;

    let plain =
        references(&client, &[("symbol", Value::from(STAND_METHOD)), ("limit", Value::from(50))])
            .await;
    let decorated = references(
        &client,
        &[
            ("symbol", Value::from(STAND_METHOD)),
            ("limit", Value::from(50)),
            ("include_preview", Value::from(true)),
        ],
    )
    .await;

    assert_eq!(plain["outcome"], "resolved", "{plain}");
    assert!(
        plain["total"].as_u64().expect("a total") > 50,
        "the stand has to overflow `limit`, or the order of assembly cannot matter: {plain}",
    );
    // The precondition, read off the answer rather than trusted from the stand's constants:
    // the histogram must actually be losing buckets, or the order of assembly could not
    // matter and the comparison below would be green whatever the implementation did.
    assert_eq!(plain["histogram_truncated"], true, "the histogram has to be fighting for room");
    assert!(
        plain["files"].as_array().expect("the histogram").len() < CROWD,
        "buckets were lost, not merely counted: {plain}",
    );

    for field in ["outcome", "total", "total_is_lower_bound", "files", "histogram_truncated"] {
        assert_eq!(plain[field], decorated[field], "`{field}` moved when previews came on");
    }
    assert_eq!(skeleton(&plain), skeleton(&decorated), "the records themselves moved");

    client.cancel().await.ok();
}

/// Gate I1 — when the budget runs out it is the previews that go, never the references.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_exhausted_budget_drops_previews_and_keeps_every_reference() {
    let ws = stage_workspace();
    let client = client_with_references(ws.path()).await;

    let control = references(
        &client,
        &[("symbol", Value::from(STAND_METHOD)), ("max_output_tokens", Value::from(700))],
    )
    .await;
    let squeezed = references(
        &client,
        &[
            ("symbol", Value::from(STAND_METHOD)),
            ("max_output_tokens", Value::from(700)),
            ("include_preview", Value::from(true)),
        ],
    )
    .await;

    assert_eq!(skeleton(&control), skeleton(&squeezed), "a preview cost a reference its place");
    assert!(
        squeezed["previews_omitted"].as_u64().is_some_and(|omitted| omitted > 0),
        "the budget was too small for every preview, and the answer has to count what it \
         could not send: {squeezed}",
    );
    assert!(
        reason_codes(&squeezed).contains(&"output_budget".to_owned()),
        "an incomplete decoration is still incomplete: {squeezed}",
    );

    client.cancel().await.ok();
}

/// A module with a secret on one reference's line and nothing sensitive on the next one's.
const SECRET_MODULE: &str = "\
Процедура Секретная() Экспорт
    Пароль = \"тайна\"; ПервыйОбщийМодуль.НеУстаревшаяФункция();
    ПервыйОбщийМодуль.НеУстаревшаяФункция();
КонецПроцедуры
";

/// Gate I4 — a preview is sanitized, and exactly as far as the filter reaches on ONE line.
/// The pair is the point: the second occurrence sits on a line with no marker and comes out
/// byte for byte, so the masking above is the filter working and not the reader mangling.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_secret_on_a_previewed_line_is_masked() {
    let ws = stage_fixture();
    write_module(ws.path(), "Секрет", SECRET_MODULE);
    let client = client_with_references(ws.path()).await;

    let answer = references(
        &client,
        &[("symbol", Value::from(STAND_METHOD)), ("include_preview", Value::from(true))],
    )
    .await;

    let previews: Vec<&Value> = answer["references"]
        .as_array()
        .expect("the list")
        .iter()
        .filter(|entry| {
            entry["location"]["path"].as_str().is_some_and(|path| path.contains("Секрет"))
        })
        .collect();
    assert_eq!(previews.len(), 2, "both occurrences of the stand module: {answer}");

    let masked = previews
        .iter()
        .find(|entry| entry["snippet"].as_str().is_some_and(|line| line.contains("Пароль")))
        .expect("the line carrying the secret");
    let snippet = masked["snippet"].as_str().expect("a snippet");
    assert!(!snippet.contains("тайна"), "the secret value left the server: {snippet}");
    assert!(snippet.contains("***"), "the value is masked, not dropped: {snippet}");
    assert_eq!(masked["snippet_redacted"], true, "a masked line no longer matches its columns");

    let plain = previews
        .iter()
        .find(|entry| !entry["snippet"].as_str().is_some_and(|line| line.contains("Пароль")))
        .expect("the line with nothing sensitive on it");
    assert_eq!(
        plain["snippet"], "    ПервыйОбщийМодуль.НеУстаревшаяФункция();",
        "a line with no marker travels byte for byte",
    );
    assert!(plain.get("snippet_redacted").is_none(), "nothing was masked: {plain}");

    client.cancel().await.ok();
}

/// What stands in a record's snippet at the column the record's own `location` names.
///
/// This is the whole contract of `snippet_start_character`: `range.start_character` minus it
/// is an offset INTO the snippet, in the UTF-16 units both are counted in. A snippet that
/// cannot be indexed this way is a caption that describes some other line.
fn snippet_at_the_occurrence(entry: &Value) -> String {
    let snippet = entry["snippet"].as_str().expect("a snippet");
    let start = entry["snippet_start_character"].as_u64().expect("a start column");
    let column = entry["location"]["range"]["start_character"].as_u64().expect("a column");
    let units: Vec<u16> = snippet.encode_utf16().collect();
    let offset = (column - start) as usize;
    String::from_utf16(&units[offset.min(units.len())..])
        .expect("the offset falls between characters")
}

/// Gate I3 — on a line too long to show, the window follows the occurrence.
///
/// A preview cut from the head of the line would not contain the symbol the answer is about,
/// which is the plausible-looking wrong answer this tool exists to end.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_long_line_is_windowed_around_the_occurrence() {
    let ws = stage_fixture();
    let filler = "я".repeat(3900);
    write_module(
        ws.path(),
        "Длинный",
        &format!(
            "Процедура Длинная() Экспорт\n    \
             Текст = \"{filler}\"; ПервыйОбщийМодуль.НеУстаревшаяФункция();\n\
             КонецПроцедуры\n"
        ),
    );
    write_module(
        ws.path(),
        "Короткий",
        "Процедура Короткая() Экспорт\n    \
         ПервыйОбщийМодуль.НеУстаревшаяФункция();\nКонецПроцедуры\n",
    );
    let client = client_with_references(ws.path()).await;

    let answer = references(
        &client,
        &[("symbol", Value::from(STAND_METHOD)), ("include_preview", Value::from(true))],
    )
    .await;
    let by_module = |name: &str| {
        answer["references"]
            .as_array()
            .expect("the list")
            .iter()
            .find(|entry| {
                entry["location"]["path"].as_str().is_some_and(|path| path.contains(name))
            })
            .unwrap_or_else(|| panic!("an occurrence in {name}: {answer}"))
            .clone()
    };

    let long = by_module("Длинный");
    let snippet = long["snippet"].as_str().expect("a snippet");
    assert_eq!(long["snippet_truncated"], true, "a 4000-character line does not fit: {long}");
    assert!(
        snippet.contains("НеУстаревшаяФункция"),
        "a preview without the occurrence in it is worse than none: {snippet}",
    );
    assert!(
        long["snippet_start_character"].as_u64().expect("a start column") > 0,
        "the window moved off the head of the line: {long}",
    );
    assert!(
        snippet_at_the_occurrence(&long).starts_with("НеУстаревшаяФункция"),
        "the published columns have to index the snippet they travel with: {long}",
    );

    // The control: a line short enough to show whole carries no truncation flag and starts
    // at column zero.
    let short = by_module("Короткий");
    assert!(short.get("snippet_truncated").is_none(), "nothing was cut: {short}");
    assert_eq!(short["snippet_start_character"], 0);

    client.cancel().await.ok();
}

/// Gate I5 — with previews off, not one preview key reaches the wire. A `snippet: null` is
/// a field a consumer has to learn to ignore.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn previews_off_writes_no_preview_key_at_all() {
    let ws = stage_workspace();
    let client = client_with_references(ws.path()).await;

    let plain = references(&client, &[("symbol", Value::from(STAND_METHOD))]).await;
    assert!(
        !plain.to_string().contains("snippet"),
        "an unasked-for decoration left a trace: {plain}",
    );

    // The control: the same request with previews on does carry them, so the assertion
    // above is about the switch and not about a surface that never previews anything.
    let decorated = references(
        &client,
        &[("symbol", Value::from(STAND_METHOD)), ("include_preview", Value::from(true))],
    )
    .await;
    assert!(decorated.to_string().contains("\"snippet\""), "{decorated}");

    client.cancel().await.ok();

    // The one exception, gated so it stays deliberate: a place a quote could not choose
    // between carries its line WITHOUT anyone asking for previews. That line is not a caption
    // on a finding — it is the evidence for a refusal, and evidence the caller has to ask for
    // twice is evidence withheld.
    let ws = stage_anchor_workspace();
    let client = client_with_references(ws.path()).await;
    let ambiguous =
        references(&client, &anchor_args(&[("line_content", Value::from("Значение = "))])).await;
    assert_eq!(ambiguous["outcome"], "ambiguous", "{ambiguous}");
    for site in ambiguous["anchor_sites"].as_array().expect("the places") {
        assert!(site["snippet"].is_string(), "a place without its evidence: {site}");
    }

    client.cancel().await.ok();
}

/// Gate I2 — neither the terminator nor the whitespace before it travels with the preview.
///
/// Two spellings of the same end of line, and the input carries both halves of the promise:
/// the `\r` a CRLF file keeps inside the line (the shared reader drops it, and its own gate
/// in `ide` holds that), and a run of trailing spaces that only the preview trims. Without
/// the second half this check would be vacuous the moment the reader learned about `\r` —
/// green whatever the preview did. The control is the LF twin: same bytes out.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_crlf_file_previews_the_same_bytes_as_an_lf_one() {
    let ws = stage_fixture();
    let body = "Процедура Вызвать() Экспорт\n    \
                ПервыйОбщийМодуль.НеУстаревшаяФункция();   \nКонецПроцедуры\n";
    write_module(ws.path(), "СЛФ", body);
    write_module(ws.path(), "СКрЛф", &body.replace('\n', "\r\n"));
    let client = client_with_references(ws.path()).await;

    let answer = references(
        &client,
        &[("symbol", Value::from(STAND_METHOD)), ("include_preview", Value::from(true))],
    )
    .await;
    let snippet_of = |name: &str| {
        answer["references"]
            .as_array()
            .expect("the list")
            .iter()
            .find(|entry| {
                entry["location"]["path"].as_str().is_some_and(|path| path.contains(name))
            })
            .unwrap_or_else(|| panic!("an occurrence in {name}: {answer}"))["snippet"]
            .as_str()
            .expect("a snippet")
            .to_owned()
    };

    let crlf = snippet_of("СКрЛф");
    assert_eq!(
        crlf, "    ПервыйОбщийМодуль.НеУстаревшаяФункция();",
        "the end of the line reached the wire: {crlf:?}",
    );
    assert_eq!(crlf, snippet_of("СЛФ"), "one line, two spellings of its end");

    client.cancel().await.ok();
}

/// The module the disk gate rewrites. The trailing comment is there to be swapped for
/// another of exactly the same byte length.
const DRAFT_MODULE: &str = "\
Процедура Черновик() Экспорт
    ПервыйОбщийМодуль.НеУстаревшаяФункция(); // мет
КонецПроцедуры
";

/// Rewrite a file and put its modification time back, so the change is invisible to drift.
///
/// The drift fingerprint is `(mtime, len)` and nothing else, so a rewrite of the same length
/// under the same timestamp is, to every classifier in the server, no change at all. That is
/// what makes the disk and the resident hold different text at the same instant — the only
/// state in which a preview read from disk can be told from one read from the database.
fn rewrite_invisibly(path: &Path, contents: &str) {
    let before = std::fs::metadata(path).expect("the file is there");
    assert_eq!(
        before.len() as usize,
        contents.len(),
        "an invisible rewrite has to keep the length: the fingerprint would see it otherwise",
    );
    std::fs::write(path, contents).expect("rewrite");
    let file = std::fs::File::options().write(true).open(path).expect("reopen to set times");
    file.set_times(
        std::fs::FileTimes::new().set_modified(before.modified().expect("a modification time")),
    )
    .expect("restore the modification time");
}

/// Gate I2 — the preview is a slice of the text the OFFSETS were counted against, never of
/// whatever is on disk when the answer is assembled.
///
/// A same-length, same-timestamp rewrite must still move the content fingerprint. The preview
/// and the revision in its envelope must describe the same observed text.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_preview_quotes_the_revision_the_answer_is_signed_with() {
    let ws = stage_fixture();
    write_module(ws.path(), "Черновик", DRAFT_MODULE);
    let module = ws.path().join("CommonModules/Черновик/Ext/Module.bsl");
    let client = client_with_references(ws.path()).await;

    let draft_snippet = |answer: &Value| {
        answer["references"]
            .as_array()
            .expect("the list")
            .iter()
            .find(|entry| {
                entry["location"]["path"].as_str().is_some_and(|path| path.contains("Черновик"))
            })
            .unwrap_or_else(|| panic!("an occurrence in Черновик: {answer}"))["snippet"]
            .as_str()
            .expect("a snippet")
            .to_owned()
    };
    async fn ask(client: &Client) -> Value {
        references(
            client,
            &[("symbol", Value::from(STAND_METHOD)), ("include_preview", Value::from(true))],
        )
        .await
    }

    let before = ask(&client).await;
    assert!(draft_snippet(&before).ends_with("// мет"), "the stand starts where it says: {before}");

    rewrite_invisibly(&module, &DRAFT_MODULE.replace("// мет", "// про"));
    let updated = common::settled(
        "the same-stat edit reaches the preview",
        |answer| draft_snippet(answer).ends_with("// про"),
        || ask(&client),
    )
    .await;
    assert_ne!(
        updated["freshness"]["revision"], before["freshness"]["revision"],
        "the updated preview must name its new revision: {updated}",
    );

    // Control: a second edit also moves the text and its revision.
    std::fs::write(&module, DRAFT_MODULE.replace("// мет", "// метка")).expect("visible rewrite");
    common::settled(
        "the visible edit reaches the preview",
        |answer| draft_snippet(answer).ends_with("// метка"),
        || ask(&client),
    )
    .await;

    client.cancel().await.ok();
}

/// A line written after the resident was built must not come back `anchor_stale`: a caller
/// reads that outcome as "one of us is out of date" and acts on it. Two mechanisms can keep
/// the promise — the change hub's drain, and the forced re-scan a stale anchor now earns the
/// way a name miss does — and this gate holds whichever of them ran. It does not single out
/// the second, and cannot: `read()` polls for drift before it computes anything, so the very
/// first answer may already be `resolved`. What DOES pin the rule down is the unit gate on
/// `warrants_rescan` beside the predicate. The control is a quote nothing ever carried,
/// which stays stale however many re-scans it takes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_line_written_after_the_last_scan_is_found_on_a_forced_retry() {
    let ws = stage_anchor_workspace();
    let client = client_with_references(ws.path()).await;

    let ready =
        references(&client, &anchor_args(&[("line_content", Value::from(ANCHOR_CALL_QUOTE))]))
            .await;
    assert_eq!(ready["outcome"], "resolved", "{ready}");

    let module = ws.path().join("CommonModules").join("Якорь").join("Ext").join("Module.bsl");
    std::fs::write(
        &module,
        format!(
            "{ANCHOR_MODULE}\nПроцедура Четвёртая() Экспорт\n    \
             ПервыйОбщийМодуль.НеУстаревшаяФункция(); // добавлено позже\nКонецПроцедуры\n"
        ),
    )
    .expect("write a line after the resident was built");

    // Asked until answered, not asked once: both mechanisms that could deliver the line run
    // on clocks this test does not hold — see `common::settled`.
    let quoted =
        anchor_args(&[("line_content", Value::from("НеУстаревшаяФункция(); // добавлено позже"))]);
    let fresh = common::settled(
        "a line that exists on disk must not be called stale",
        |answer| answer["outcome"] == "resolved",
        || references(&client, &quoted),
    )
    .await;
    assert_eq!(fresh["anchor"]["mode"], "line_content", "{fresh}");

    let never =
        references(&client, &anchor_args(&[("line_content", Value::from("// никогда не было"))]))
            .await;
    assert_eq!(
        never["outcome"], "anchor_stale",
        "control: a re-scan finds no line that was never written: {never}",
    );

    client.cancel().await.ok();
}

/// Gate — the marker that arms redaction may stand outside any window drawn around the
/// occurrence, and the secret must not travel because of it.
///
/// The input is the one that separates the two possible orders: a line longer than the cap
/// whose `Пароль =` sits at the head and whose occurrence sits at the tail, so a window taken
/// BEFORE masking carries the literal's tail with no marker beside it to arm the filter. The
/// control is the same shape with no marker at all, which must still come through untouched.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_secret_whose_marker_falls_outside_the_window_is_still_masked() {
    let ws = stage_fixture();
    let filler = "x".repeat(1800);
    write_module(
        ws.path(),
        "ДлинныйСекрет",
        &format!(
            "Процедура Секретная() Экспорт\n    \
             Пароль = \"{filler}\"; ПервыйОбщийМодуль.НеУстаревшаяФункция();\n\
             КонецПроцедуры\n"
        ),
    );
    write_module(
        ws.path(),
        "ДлинныйОткрытый",
        &format!(
            "Процедура Открытая() Экспорт\n    \
             Комментарий = \"{filler}\"; ПервыйОбщийМодуль.НеУстаревшаяФункция();\n\
             КонецПроцедуры\n"
        ),
    );
    let client = client_with_references(ws.path()).await;

    let answer = references(
        &client,
        &[("symbol", Value::from(STAND_METHOD)), ("include_preview", Value::from(true))],
    )
    .await;
    let entry = |name: &str| {
        answer["references"]
            .as_array()
            .expect("the list")
            .iter()
            .find(|entry| {
                entry["location"]["path"].as_str().is_some_and(|path| path.contains(name))
            })
            .unwrap_or_else(|| panic!("an occurrence in {name}: {answer}"))
            .clone()
    };

    let secret = entry("ДлинныйСекрет");
    let snippet = secret["snippet"].as_str().expect("a snippet");
    assert!(
        !snippet.contains("xxxx"),
        "the tail of a secret literal reached the wire because its marker was windowed away: \
         {snippet}",
    );
    assert_eq!(secret["snippet_redacted"], true, "{secret}");
    // Contains, not indexes: `snippet_redacted` withdraws the byte correspondence between
    // the published columns and the snippet, and that is the whole price of masking first.
    // What survives is the promise the window exists for — the occurrence is in the preview.
    assert!(
        snippet.contains("НеУстаревшаяФункция"),
        "masking must not cost the preview the occurrence it is about: {secret}",
    );

    // The control: the same line with a harmless name in front of the same literal. Nothing
    // is masked, so the assertion above is about the marker and not about every long line.
    let open = entry("ДлинныйОткрытый");
    assert!(open.get("snippet_redacted").is_none(), "nothing sensitive here: {open}");
    assert!(snippet_at_the_occurrence(&open).starts_with("НеУстаревшаяФункция"), "{open}",);

    client.cancel().await.ok();
}

/// Gate — `actual_line` says when it was capped or masked.
///
/// The field exists so a caller can hold it against its own buffer and see WHICH of the two
/// pictures moved. A line silently cut at the cap shows a difference in the tail that the
/// file does not have; a line silently masked shows `***` where the file has a literal. Both
/// turn the one piece of evidence in a refusal into a false one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_evidence_line_of_a_stale_anchor_says_when_it_was_altered() {
    let ws = stage_fixture();
    let long = "я".repeat(400);
    write_module(
        ws.path(),
        "Улика",
        &format!(
            "Процедура Улика() Экспорт\n    // {long}\n    Пароль = \"тайна\";\nКонецПроцедуры\n"
        ),
    );
    let client = client_with_references(ws.path()).await;
    let path = "CommonModules/Улика/Ext/Module.bsl";
    async fn ask(client: &Client, path: &str, line: u32) -> Value {
        references(
            client,
            &[
                ("root_id", Value::from("")),
                ("path", Value::from(path)),
                ("line", Value::from(line)),
                ("line_content", Value::from("такой строки в файле нет")),
            ],
        )
        .await
    }

    let capped = ask(&client, path, 1).await;
    assert_eq!(capped["outcome"], "anchor_stale", "{capped}");
    assert_eq!(capped["anchor_stale"]["actual_line_truncated"], true, "{capped}");
    assert!(capped["anchor_stale"].get("actual_line_redacted").is_none(), "{capped}");

    let masked = ask(&client, path, 2).await;
    let evidence = masked["anchor_stale"]["actual_line"].as_str().expect("the line");
    assert!(!evidence.contains("тайна"), "the secret left the server: {evidence}");
    assert_eq!(masked["anchor_stale"]["actual_line_redacted"], true, "{masked}");
    assert!(masked["anchor_stale"].get("actual_line_truncated").is_none(), "{masked}");

    // The control: a line that is neither long nor sensitive travels whole and unflagged, so
    // the two assertions above are about the transforms and not about a field that always
    // claims to have been altered.
    let plain = ask(&client, path, 0).await;
    assert_eq!(plain["anchor_stale"]["actual_line"], "Процедура Улика() Экспорт");
    assert!(plain["anchor_stale"].get("actual_line_truncated").is_none(), "{plain}");
    assert!(plain["anchor_stale"].get("actual_line_redacted").is_none(), "{plain}");

    client.cancel().await.ok();
}

/// Gate — a preview never pushes an answer past a ceiling it was inside without one.
///
/// `max_output_tokens` is a target with a floor of one item, not a hard wall — the list and
/// the histogram already keep one element each however small the budget. What a DECORATION
/// must never do is spend budget the answer itself was living within. The list and the
/// histogram are cut by the bytes they actually serialize to; an estimate for previews alone
/// would overrun by the two conditional flag keys and by every quote a snippet escapes, so
/// the stand is built to land on that seam: a long windowed line (which writes
/// `snippet_truncated`) full of quotes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_preview_never_spends_budget_the_answer_was_inside() {
    let ws = stage_fixture();
    // Doubled quotes are BSL's way of putting a quote INSIDE a literal, so the line stays one
    // well-formed string — and every one of those quotes costs two bytes on the way to JSON,
    // which is the term an estimate over `snippet.len()` misses. The literal stands AFTER the
    // call on purpose: the window follows the occurrence, so quotes placed before it would
    // never reach the snippet and the stand would measure nothing.
    let filler = "я\"\"".repeat(300);
    write_module(
        ws.path(),
        "Бюджетный",
        &format!(
            "Процедура Бюджетная() Экспорт\n    \
             ПервыйОбщийМодуль.НеУстаревшаяФункция(); Текст = \"{filler}\";\n\
             КонецПроцедуры\n"
        ),
    );
    let client = client_with_references(ws.path()).await;

    let mut ever_decorated = false;
    // Every budget across the seam, so the check does not rest on guessing the one value at
    // which an estimate happens to overrun.
    for budget in [400usize, 500, 600, 700, 800, 1000, 1200, 1600, 2400] {
        sweep(&client, budget, None, &mut ever_decorated).await;
    }
    assert!(
        ever_decorated,
        "no budget in the sweep attached a single preview, so this gate measured nothing",
    );

    // The second sweep is the one that can tell a measured cost from an estimated one. The
    // area filter leaves exactly ONE reference, and it sits on a line of escaped quotes: its
    // snippet doubles in length on the way to JSON and writes a truncation flag besides, so
    // an estimate over `snippet.len()` under-counts it by a third. One record means that
    // preview is always the one weighed, and a fine sweep means some budget lands in the gap
    // between what the estimate allows and what the bytes cost.
    let mut ever_decorated = false;
    for budget in (60usize..=800).step_by(2) {
        let call: Vec<(&str, Value)> = vec![
            ("symbol", Value::from(STAND_METHOD)),
            ("area_root_id", Value::from("")),
            ("area_path_prefix", Value::from("CommonModules/Бюджетный")),
            ("max_output_tokens", Value::from(budget)),
        ];
        let plain = references(&client, &call).await.to_string().len();
        let mut decorated_call = call.clone();
        decorated_call.push(("include_preview", Value::from(true)));
        let decorated = references(&client, &decorated_call).await;
        ever_decorated |= decorated.to_string().contains("\"snippet\"");
        if fully_previewed(&decorated) && plain <= budget * 4 {
            let size = decorated.to_string().len();
            assert!(
                size <= budget * 4,
                "at max_output_tokens={budget} the answer fitted in {plain} bytes and the \
                 preview took it to {size}, past the {} it promises: {decorated}",
                budget * 4,
            );
        }
    }
    assert!(ever_decorated, "the single-record sweep attached no preview either");
    client.cancel().await.ok();

    // The third sweep is the only one that can weigh the ENVELOPE reserve honestly. A `limit`
    // the answer overflows puts a `result_cap` reason into the completeness before previews
    // are measured, and a reserve computed from an empty one is short by the ~130 bytes that
    // reason costs. With no reason in play the two spellings are the same number, so the
    // sweeps above cannot tell them apart.
    let ws = stage_workspace();
    let client = client_with_references(ws.path()).await;
    let mut ever_decorated = false;
    for budget in (120usize..=900).step_by(2) {
        sweep(&client, budget, Some(1), &mut ever_decorated).await;
    }
    assert!(ever_decorated, "the capped sweep attached no preview either");

    client.cancel().await.ok();
}

/// One budget, both ways: if the answer fitted without previews, it must fit with them.
async fn sweep(client: &Client, budget: usize, limit: Option<usize>, ever_decorated: &mut bool) {
    {
        let mut call: Vec<(&str, Value)> =
            vec![("symbol", Value::from(STAND_METHOD)), ("max_output_tokens", Value::from(budget))];
        if let Some(limit) = limit {
            call.push(("limit", Value::from(limit)));
        }
        let plain = references(client, &call).await.to_string().len();
        call.push(("include_preview", Value::from(true)));
        let decorated = references(client, &call).await;
        *ever_decorated |= decorated.to_string().contains("\"snippet\"");

        if fully_previewed(&decorated) && plain <= budget * 4 {
            let size = decorated.to_string().len();
            assert!(
                size <= budget * 4,
                "at max_output_tokens={budget} the answer fitted in {plain} bytes and the \
                 previews took it to {size}, past the {} it promises: {decorated}",
                budget * 4,
            );
        }
        // And whatever happened to the previews, the BODY stays inside the budget it is
        // measured against — including the line that reports what did not fit, which is
        // written after the last measurement and so has to be reserved for.
        if plain <= budget * 4 {
            let body = body_size(&decorated);
            assert!(
                body <= budget * 4,
                "at max_output_tokens={budget} the body reached {body} bytes, past the {}: \
                 {decorated}",
                budget * 4,
            );
        }
    }
}

/// Whether every record that could carry a preview got one.
///
/// The ceiling is only a promise about answers of this shape. When a preview does NOT fit,
/// the answer still grows by the completeness reason that says so, and completeness is not
/// budgeted anywhere in this tool — the list and the histogram are sized against
/// `max_output_tokens` with no room kept for the envelope either, deliberately, because
/// keeping room would change answers that never asked for a preview. Holding those answers to
/// the ceiling would fail them for behaving as designed. The BODY of such an answer is a
/// different matter and is checked separately below.
fn fully_previewed(answer: &Value) -> bool {
    answer.to_string().contains("\"snippet\"") && answer.get("previews_omitted").is_none()
}

/// The answer without the envelope `finish` adds after the body is sized.
fn body_size(answer: &Value) -> usize {
    let mut body = answer.clone();
    if let Some(object) = body.as_object_mut() {
        object.remove("freshness");
    }
    body.to_string().len()
}

/// Gate — the hint for an ambiguous quote names only axes that can actually separate.
///
/// A `line` beside a quote stopped choosing when the pin was dropped: it marks the place it
/// stood at and nothing more. Advertising it would send the caller round a trip that cannot
/// arrive — and the control is exactly that trip, taken: the same request WITH the line
/// answers the same way, so the axis is provably dead and not merely unrecommended.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_hint_for_an_ambiguous_quote_names_no_axis_that_cannot_separate() {
    let ws = stage_fixture();
    // Two parameters named on ONE line: two symbols, and no line that could tell them apart.
    write_module(
        ws.path(),
        "ОднаСтрока",
        "Процедура Тест(Первый, Второй) Экспорт\n    Первый = Второй;\nКонецПроцедуры\n",
    );
    let client = client_with_references(ws.path()).await;

    let one_line = references(
        &client,
        &[
            ("root_id", Value::from("")),
            ("path", Value::from("CommonModules/ОднаСтрока/Ext/Module.bsl")),
            ("line_content", Value::from("Первый = Второй;")),
        ],
    )
    .await;
    assert_eq!(one_line["outcome"], "ambiguous", "{one_line}");
    let hint = one_line["resolution_hint"].as_str().expect("a hint");
    assert!(hint.contains("line_content"), "the axis that does work: {hint}");
    assert!(!hint.contains("pass the `line`"), "a `line` beside a quote chooses nothing: {hint}",);

    // Places on DIFFERENT lines — the shape where a line looks like it should help.
    let ws = stage_anchor_workspace();
    let two = client_with_references(ws.path()).await;
    let spread =
        references(&two, &anchor_args(&[("line_content", Value::from("Значение = "))])).await;
    assert_eq!(spread["outcome"], "ambiguous", "{spread}");
    let hint = spread["resolution_hint"].as_str().expect("a hint");
    assert!(
        !hint.contains("pass the `line`"),
        "even here a `line` chooses nothing, so the hint must not offer it: {hint}",
    );

    // The control that makes the assertion above more than a preference: take the advice
    // that was NOT given and watch it fail to arrive.
    let with_line = references(
        &two,
        &anchor_args(&[("line_content", Value::from("Значение = ")), ("line", Value::from(1))]),
    )
    .await;
    assert_eq!(
        with_line["outcome"], "ambiguous",
        "a repeated call with the `line` still does not resolve: {with_line}",
    );
    let pointed: Vec<&Value> = with_line["anchor_sites"]
        .as_array()
        .expect("the places")
        .iter()
        .filter(|site| site["pointed_by_line"] == true)
        .collect();
    assert_eq!(pointed.len(), 1, "what the line DOES do is mark one place: {with_line}");

    client.cancel().await.ok();
    two.cancel().await.ok();
}

/// Gate — a place is addressed by the PAIR `(root_id, path)`, and the hint that sends a
/// caller back to a published place has to say so.
///
/// The single-root fixture cannot see this: it puts every module under the workspace root, so
/// a path without its root resolves there by accident. Two roots are what make the pair
/// load-bearing — and the recovery path this stage recommends runs straight through it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_published_place_is_re_anchored_by_its_root_and_path_together() {
    let (ws, _ext) = stage_two_roots();
    let client = client_with_references(ws.path()).await;

    let answer = references(&client, &[("symbol", Value::from(STAND_METHOD))]).await;
    let place = answer["references"]
        .as_array()
        .expect("the list")
        .iter()
        .map(|entry| &entry["location"])
        .find(|location| location["root_id"].as_str().is_some_and(|id| !id.is_empty()))
        .expect("a place in the extension root")
        .clone();
    let path = place["path"].as_str().expect("a path");
    let line = place["range"]["start_line"].as_u64().expect("a line");
    let column = place["range"]["start_character"].as_u64().expect("a column");

    let paired = references(
        &client,
        &[
            ("root_id", place["root_id"].clone()),
            ("path", Value::from(path)),
            ("line", Value::from(line)),
            ("column", Value::from(column)),
        ],
    )
    .await;
    assert_eq!(paired["outcome"], "resolved", "the pair addresses the place: {paired}");

    // The control that gives the assertion its meaning: the same place, the same coordinates,
    // the root left off. A path alone is spelled against the workspace, and the file is not
    // there — so this must be refused rather than answered about something else.
    let orphan = CallToolRequestParams::new(TOOL).with_arguments(args(&[
        ("path", Value::from(path)),
        ("line", Value::from(line)),
        ("column", Value::from(column)),
    ]));
    assert!(
        client.call_tool(orphan).await.is_err(),
        "a path without its root names nothing, and the tool must say so",
    );

    client.cancel().await.ok();
}

/// Gate — the hint that sends a caller to a published place names the root beside the path.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_hint_names_the_root_beside_the_path_it_recommends() {
    let ws = stage_anchor_workspace();
    let client = client_with_references(ws.path()).await;

    let ambiguous =
        references(&client, &anchor_args(&[("line_content", Value::from("Значение = "))])).await;
    assert_eq!(ambiguous["outcome"], "ambiguous", "{ambiguous}");
    let hint = ambiguous["resolution_hint"].as_str().expect("a hint");
    assert!(
        hint.contains("root_id"),
        "a place is addressed by the pair, and half a pair is an address that is refused: \
         {hint}",
    );

    client.cancel().await.ok();
}

/// Gate — a column taken out of an answer goes back in unchanged.
///
/// Published columns are UTF-16, because that is what `position_encoding` declares. The
/// `column` parameter has to read them the same way, and the only input that can tell the two
/// unit systems apart is a character outside the BMP standing before the name: every BSL
/// identifier, Cyrillic included, is one UTF-16 unit per character and would agree either way.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_column_taken_from_an_answer_addresses_the_same_token_when_sent_back() {
    let ws = stage_fixture();
    // Twenty-five of them, not one: every astral character shifts a character-counted walk
    // one place to the right, and a shift smaller than the identifier lands INSIDE the same
    // token and resolves anyway. The stand has to move the column clear off the name, or it
    // measures nothing.
    let astral = "😀".repeat(25);
    write_module(
        ws.path(),
        "Астральный",
        &format!(
            "Процедура Астральная() Экспорт\n    \
             Текст = \"{astral}\"; ПервыйОбщийМодуль.НеУстаревшаяФункция();\n\
             КонецПроцедуры\n"
        ),
    );
    let client = client_with_references(ws.path()).await;

    let answer = references(&client, &[("symbol", Value::from(STAND_METHOD))]).await;
    let place = answer["references"]
        .as_array()
        .expect("the list")
        .iter()
        .map(|entry| &entry["location"])
        .find(|location| location["path"].as_str().is_some_and(|p| p.contains("Астральный")))
        .expect("the occurrence past the emoji")
        .clone();

    let round_trip = references(
        &client,
        &[
            ("root_id", place["root_id"].clone()),
            ("path", place["path"].clone()),
            ("line", place["range"]["start_line"].clone()),
            ("column", place["range"]["start_character"].clone()),
        ],
    )
    .await;
    assert_eq!(
        round_trip["outcome"], "resolved",
        "a column this answer published did not address the token it came from: {round_trip}",
    );
    assert_eq!(
        round_trip["total"], answer["total"],
        "and it addressed the same symbol: {round_trip}",
    );

    client.cancel().await.ok();
}
/// Gate — masking a line does not move the window onto a different occurrence of the name.
///
/// Redaction is not only a shortening: a literal of fewer than three characters grows into
/// `"***"`, and everything after it shifts RIGHT. The stand puts the SAME name on both sides
/// of such a literal, so a recovery search that only looked left would settle on the earlier
/// namesake and publish a window around the wrong place — a caption that reads as correct
/// because it does contain the right word.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn masking_that_lengthens_a_line_does_not_move_the_window_to_a_namesake() {
    let ws = stage_fixture();
    let pad = "я".repeat(160);
    write_module(
        ws.path(),
        "Растущий",
        &format!(
            "Процедура П() Экспорт\n    \
             ПервыйОбщийМодуль.НеУстаревшаяФункция(); Пад = \"{pad}\"; Пароль = \"a\"; \
             ПервыйОбщийМодуль.НеУстаревшаяФункция();\nКонецПроцедуры\n"
        ),
    );
    let client = client_with_references(ws.path()).await;

    let answer = references(
        &client,
        &[("symbol", Value::from(STAND_METHOD)), ("include_preview", Value::from(true))],
    )
    .await;
    let here: Vec<&Value> = answer["references"]
        .as_array()
        .expect("the list")
        .iter()
        .filter(|entry| entry["location"]["path"].as_str().is_some_and(|p| p.contains("Растущий")))
        .collect();
    assert_eq!(here.len(), 2, "both calls on the line: {answer}");

    let far = here
        .iter()
        .max_by_key(|entry| entry["location"]["range"]["start_character"].as_u64().unwrap_or(0))
        .expect("the call past the masked literal");
    assert_eq!(far["snippet_redacted"], true, "the stand has to mask something: {far}");
    assert!(
        far["snippet_start_character"].as_u64().expect("a start column") > 0,
        "the window stayed on the FIRST occurrence of the name instead of the one this \
         record is about: {far}",
    );

    // The control: the occurrence at the head of the line is the one whose window really does
    // start at zero, so the assertion above is about which occurrence was found and not about
    // windows in general.
    let near = here
        .iter()
        .min_by_key(|entry| entry["location"]["range"]["start_character"].as_u64().unwrap_or(0))
        .expect("the call before the literal");
    assert_eq!(near["snippet_start_character"], 0, "{near}");

    client.cancel().await.ok();
}

/// Gate — masking that SHORTENS a line does not move the window onto a namesake to the right.
///
/// The mirror of the lengthening case, and the one a nearest-match search gets wrong: when
/// masking pulls the line left by Δ, the real occurrence sits Δ away from the column it had,
/// while a namesake standing δ to the right of it sits |δ−Δ| away — nearer, for every δ under
/// 2Δ. The stand puts the name inside a message literal just past the call, behind a secret
/// long enough to make Δ large.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn masking_that_shortens_a_line_does_not_move_the_window_to_a_namesake() {
    let ws = stage_fixture();
    let secret = "A".repeat(60);
    let tail = "B".repeat(140);
    write_module(
        ws.path(),
        "Окно",
        &format!(
            "Процедура Испытание() Экспорт\n    \
             Пароль = \"{secret}\"; ПервыйОбщийМодуль.НеУстаревшаяФункция(); \
             Сообщить(\"после вызова НеУстаревшаяФункция продолжаем\"); Хвост = \"{tail}\";\n\
             КонецПроцедуры\n"
        ),
    );
    let client = client_with_references(ws.path()).await;

    let answer = references(
        &client,
        &[("symbol", Value::from(STAND_METHOD)), ("include_preview", Value::from(true))],
    )
    .await;
    let entry = answer["references"]
        .as_array()
        .expect("the list")
        .iter()
        .find(|entry| entry["location"]["path"].as_str().is_some_and(|p| p.contains("Окно")))
        .expect("the call behind the secret")
        .clone();

    assert_eq!(entry["snippet_redacted"], true, "the stand has to mask something: {entry}");
    assert_eq!(entry["snippet_truncated"], true, "and the line has to need a window: {entry}");
    let snippet = entry["snippet"].as_str().expect("a snippet");
    // The promise a redacted preview still carries: the occurrence is IN it, whole. A window
    // centred on the namesake clips the call's own name at the left edge.
    assert!(
        snippet.contains("ПервыйОбщийМодуль.НеУстаревшаяФункция()"),
        "the window was centred on the namesake in the message, not on the call this record \
         is about: {snippet}",
    );

    client.cancel().await.ok();
}

/// Gate — `anchor.line` is there exactly when the anchor stood somewhere.
///
/// The block is published in the tool's `outputSchema`, so a client may read it by contract.
/// A quote that named two symbols, or none, landed nowhere — and a `line` invented for those
/// answers would name a place the anchor never took.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_anchor_block_carries_a_line_exactly_when_it_stood_on_one() {
    let ws = stage_anchor_workspace();
    let client = client_with_references(ws.path()).await;

    let resolved =
        references(&client, &anchor_args(&[("line_content", Value::from(ANCHOR_CALL_QUOTE))]))
            .await;
    assert_eq!(resolved["outcome"], "resolved", "{resolved}");
    assert_eq!(resolved["anchor"]["line"], ANCHOR_CALL_LINE, "it stood here: {resolved}");

    let positional =
        references(&client, &anchor_args(&[("line", Value::from(ANCHOR_CALL_LINE))])).await;
    assert_eq!(positional["anchor"]["mode"], "position");
    assert_eq!(
        positional["anchor"]["line"], ANCHOR_CALL_LINE,
        "a position always stands where it was told to: {positional}",
    );

    let ambiguous =
        references(&client, &anchor_args(&[("line_content", Value::from("Значение = "))])).await;
    assert_eq!(ambiguous["outcome"], "ambiguous", "{ambiguous}");
    assert!(
        ambiguous["anchor"].get("line").is_none(),
        "an ambiguity landed nowhere, and a line here would name a place never taken: \
         {ambiguous}",
    );

    let stale = references(
        &client,
        &anchor_args(&[
            ("line", Value::from(ANCHOR_CALL_LINE)),
            ("line_content", Value::from("такой строки в файле нет")),
        ]),
    )
    .await;
    assert_eq!(stale["outcome"], "anchor_stale", "{stale}");
    assert!(stale["anchor"].get("line").is_none(), "{stale}");

    client.cancel().await.ok();
}

/// Gate — a secret whose marker wrapped onto the line above is still masked.
///
/// Redaction is armed by a marker standing before its literal in the same STATEMENT, and BSL
/// wraps long assignments freely. A filter handed one physical line never sees a `Пароль =`
/// that ended up on the line above, and the literal goes out in clear text — a leak the
/// preview feature would have introduced, since every caller before it passed whole bodies.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_secret_whose_marker_wrapped_onto_the_line_above_is_still_masked() {
    let ws = stage_fixture();
    write_module(
        ws.path(),
        "Перенос",
        "Процедура Перенесённая() Экспорт\n    Пароль =\n        \
         \"hunter2secret\"; ПервыйОбщийМодуль.НеУстаревшаяФункция();\nКонецПроцедуры\n",
    );
    let client = client_with_references(ws.path()).await;

    let answer = references(
        &client,
        &[("symbol", Value::from(STAND_METHOD)), ("include_preview", Value::from(true))],
    )
    .await;
    let entry = answer["references"]
        .as_array()
        .expect("the list")
        .iter()
        .find(|entry| entry["location"]["path"].as_str().is_some_and(|p| p.contains("Перенос")))
        .expect("the call sharing a line with the wrapped literal")
        .clone();

    let snippet = entry["snippet"].as_str().expect("a snippet");
    assert!(
        !snippet.contains("hunter2secret"),
        "the marker was on the line above, so the filter never armed and the secret left the \
         server: {snippet}",
    );
    assert_eq!(entry["snippet_redacted"], true, "{entry}");

    // The control: the same shape with a harmless name in front of the same literal. Nothing
    // is masked, so the assertion above is about the marker and not about a filter that now
    // masks every wrapped literal it sees.
    let ws = stage_fixture();
    write_module(
        ws.path(),
        "Открытый",
        "Процедура Открытая() Экспорт\n    Комментарий =\n        \
         \"hunter2secret\"; ПервыйОбщийМодуль.НеУстаревшаяФункция();\nКонецПроцедуры\n",
    );
    let control = client_with_references(ws.path()).await;
    let answer = references(
        &control,
        &[("symbol", Value::from(STAND_METHOD)), ("include_preview", Value::from(true))],
    )
    .await;
    let entry = answer["references"]
        .as_array()
        .expect("the list")
        .iter()
        .find(|entry| entry["location"]["path"].as_str().is_some_and(|p| p.contains("Открытый")))
        .expect("the same call, nothing sensitive above it")
        .clone();
    assert!(entry.get("snippet_redacted").is_none(), "nothing to mask here: {entry}");
    assert!(
        entry["snippet"].as_str().expect("a snippet").contains("hunter2secret"),
        "a literal nobody marked travels whole: {entry}",
    );

    client.cancel().await.ok();
    control.cancel().await.ok();
}

/// Gate — a line that BEGINS inside a `|`-continued literal is still quoted from its start.
///
/// The tracking the preview relies on resolves a point when the walk reaches it, and a whole
/// literal is consumed in one step. A point inside one — and the start of a continuation line
/// is exactly that — would otherwise be resolved past the entire literal, so the snippet would
/// begin wherever code resumed and silently drop the head of the line it claims to show.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_line_beginning_inside_a_continued_literal_is_quoted_from_its_start() {
    let ws = stage_fixture();
    // `пароль` inside a message arms nothing (the rule wants a marker without spaces), but it
    // does make the method sensitive — which is what sends the whole slice through the
    // tracking walk in the first place.
    write_module(
        ws.path(),
        "Перенос",
        "Процедура Перенесённая() Экспорт\n    Сообщить(\"пароль забыт\"); Хвост = \"начало\n    \
         |конец\"; ПервыйОбщийМодуль.НеУстаревшаяФункция();\nКонецПроцедуры\n",
    );
    let client = client_with_references(ws.path()).await;

    let answer = references(
        &client,
        &[("symbol", Value::from(STAND_METHOD)), ("include_preview", Value::from(true))],
    )
    .await;
    let entry = answer["references"]
        .as_array()
        .expect("the list")
        .iter()
        .find(|entry| entry["location"]["path"].as_str().is_some_and(|p| p.contains("Перенос")))
        .expect("the call on the literal's closing line")
        .clone();

    assert_eq!(
        entry["snippet"], "    |конец\"; ПервыйОбщийМодуль.НеУстаревшаяФункция();",
        "the snippet began past the literal instead of at the start of the line: {entry}",
    );
    assert!(
        entry.get("snippet_redacted").is_none(),
        "nothing on this line was masked, so nothing may claim it was: {entry}",
    );

    client.cancel().await.ok();
}

/// Callers of one popular name, enough that the walk is still running when the cancel
/// lands. Measured, not assigned: a cold walk over this many callers takes seconds while
/// the resident builds in milliseconds. A smaller stand lets the answer finish first, and
/// then the gate passes whatever the server does with a cancellation.
const CANCEL_STAND_CALLERS: usize = 800;

/// A cancelled call publishes nothing at all.
///
/// Asserted on the FRAMES, because a spec-compliant client cannot see this: rmcp resolves
/// a cancelled request locally the moment it sends `notifications/cancelled`. What the
/// server puts on the wire is still the contract — it is what a client written differently
/// would read — so the gate speaks the protocol itself.
///
/// Neither a body nor an error reaches the wire: the transport drops any response whose id
/// it has already seen cancelled, which is what the specification asks of a receiver. The
/// caller therefore learns of the cancellation from its own request, never from an answer.
#[tokio::test]
async fn a_cancelled_call_publishes_nothing() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let dst = stage_fixture();
    write_module(dst.path(), "Прогрев", "Процедура Прогреть() Экспорт\nКонецПроцедуры\n");
    for index in 0..CANCEL_STAND_CALLERS {
        write_module(
            dst.path(),
            &format!("Толпа{index:04}"),
            &format!(
                "Процедура Вызвать{index:04}() Экспорт\n    \
                 ПервыйОбщийМодуль.НеУстаревшаяФункция();\nКонецПроцедуры\n"
            ),
        );
    }

    let state = SharedState::workspace(dst.path().to_path_buf()).expect("valid workspace project");
    let gate = ToolGate::for_launch(McpProfile::Workspace, &[TOOL.to_owned()]);
    let server = McpServer::with_gate(McpProfile::Workspace, state, &gate);
    let (client_io, server_io) = tokio::io::duplex(4 * 1024 * 1024);
    tokio::spawn(serve_stream(server, server_io));
    let (read_half, mut write) = tokio::io::split(client_io);
    let mut read = BufReader::new(read_half);

    async fn send<W: tokio::io::AsyncWrite + Unpin>(write: &mut W, value: Value) {
        write.write_all(format!("{value}\n").as_bytes()).await.expect("frame written");
    }
    /// Bounded on purpose: the one failure this gate exists to catch is a server that
    /// does not answer a cancelled id, and an unbounded read would hang the run instead
    /// of reporting it.
    async fn next_frame<R: tokio::io::AsyncBufRead + Unpin>(read: &mut R) -> Value {
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(120), read.read_line(&mut line))
            .await
            .expect("the server must answer within the deadline")
            .expect("frame read");
        serde_json::from_str::<Value>(&line).expect("frame is JSON")
    }

    send(
        &mut write,
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "frame-gate", "version": "0"}
            }
        }),
    )
    .await;
    let hello = next_frame(&mut read).await;
    assert!(hello["result"]["serverInfo"].is_object(), "handshake answered: {hello}");
    send(&mut write, json!({"jsonrpc": "2.0", "method": "notifications/initialized"})).await;

    // Bring the resident up on a name whose walk touches almost nothing, so the popular
    // name below is still cold — and therefore still walking — when its call is cancelled.
    let mut id = 2;
    loop {
        send(
            &mut write,
            json!({
                "jsonrpc": "2.0", "id": id, "method": "tools/call",
                "params": {"name": TOOL, "arguments": {"symbol": "Прогрев.Прогреть"}}
            }),
        )
        .await;
        let answer = next_frame(&mut read).await;
        id += 1;
        if answer["result"]["structuredContent"]["status"] != "loading" {
            break;
        }
        assert!(id < 400, "the resident never became ready");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let subject = id;
    send(
        &mut write,
        json!({
            "jsonrpc": "2.0", "id": subject, "method": "tools/call",
            "params": {"name": TOOL, "arguments": {"symbol": STAND_METHOD}}
        }),
    )
    .await;
    send(
        &mut write,
        json!({
            "jsonrpc": "2.0", "method": "notifications/cancelled",
            "params": {"requestId": subject, "reason": "gate"}
        }),
    )
    .await;

    // The control is sent straight after the cancellation and travels the same wire, so it
    // both bounds the wait and proves the silence below is the cancellation's doing rather
    // than a wedged connection: a broken wire would starve the control too.
    let control = subject + 1;
    send(
        &mut write,
        json!({
            "jsonrpc": "2.0", "id": control, "method": "tools/call",
            "params": {"name": TOOL, "arguments": {"symbol": STAND_METHOD}}
        }),
    )
    .await;

    let answered = loop {
        let frame = next_frame(&mut read).await;
        assert!(
            frame["id"] != json!(subject),
            "a cancelled call published a frame; the transport drops responses for ids it has \
             already seen cancelled, so anything arriving here means the cancellation never \
             reached the request. If this stand became fast enough to finish before the \
             cancellation, raise CANCEL_STAND_CALLERS rather than relaxing the assertion: \
             {frame}"
        );
        if frame["id"] == json!(control) {
            break frame;
        }
    };
    assert_eq!(
        answered["result"]["structuredContent"]["outcome"], "resolved",
        "the control must publish the body the cancelled call withheld: {answered}"
    );
}

/// The call graph is one of the sources the anchor is looked for in, and `providers` says so.
///
/// A source that is never passed is reported as `not_asked`, which reads as "we chose not to
/// ask" — indistinguishable from "there is no graph here". Once the graph anchors names of
/// its own, its real state has to travel with the answer, or a consumer cannot tell a name
/// the graph does not hold from one it could not be asked about.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_answer_names_the_graph_among_the_sources_it_consulted() {
    let ws = stage_workspace();
    let client = client_with_references(ws.path()).await;

    let answer = references(&client, &[("symbol", Value::from("Добавить"))]).await;
    let providers = answer["lookup"]["providers"]
        .as_array()
        .unwrap_or_else(|| panic!("a name-dictionary envelope shows its providers: {answer}"));
    let graph = providers
        .iter()
        .find(|report| report["provider"] == "graph")
        .unwrap_or_else(|| panic!("the graph must be among the named sources: {answer}"));

    // Which state it is in depends on whether the graph finished building in this run; what
    // must never happen is the answer claiming the graph was not consulted at all.
    assert_ne!(
        graph["state"], "not_asked",
        "the graph is passed as an anchor source now, so `not_asked` would be untrue: {answer}",
    );

    // Control in the same answer: a source that genuinely is not consulted here still reads
    // as such, so the assertion above is about the graph and not about every provider having
    // been quietly relabelled.
    assert!(
        providers.iter().any(|report| report["state"] != "not_asked"),
        "at least one source answered, or the envelope proves nothing: {answer}",
    );

    client.cancel().await.ok();
}

/// The anchor carries the graph's id for the node it settled on — the SAME id `symbol_info`
/// prints for that symbol.
///
/// Two tools naming one node with two ids is worse than neither naming it: a client that
/// carries the id from one call into the other silently addresses nothing. The id is encoded
/// from the declaration, not asked of the graph, precisely so the two agree whatever the
/// build state is.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_anchor_carries_the_same_graph_id_symbol_info_prints() {
    let ws = stage_workspace();
    let client = client_with_references(ws.path()).await;

    let walk = references(&client, &[("symbol", Value::from(STAND_METHOD))]).await;
    assert_eq!(walk["outcome"], "resolved", "{walk}");
    let from_walk = walk["anchor"]["graph_id"]
        .as_str()
        .unwrap_or_else(|| panic!("a resolved method anchor names its graph node: {walk}"));

    let card = poll(&client, "symbol_info", args(&[("symbol", Value::from(STAND_METHOD))])).await;
    let from_card =
        card["graph_id"].as_str().unwrap_or_else(|| panic!("the card names the same node: {card}"));
    assert_eq!(
        from_walk, from_card,
        "one node, one id — a client carrying it between the two tools must land somewhere",
    );

    // The same symbol, spelled the way BSL lets it be spelled. Names here are matched without
    // regard to case, so a request may differ from the source by case alone and still resolve
    // — and an id built from the REQUEST then differs from one built from the DECLARATION.
    // Asking only in the declaration's own spelling cannot tell the two apart, which is why
    // this half of the gate exists: both tools have to answer with the declared spelling.
    let odd_case = STAND_METHOD.to_lowercase();
    assert_ne!(odd_case, STAND_METHOD, "the second spelling must differ, or this proves nothing");

    let walk = references(&client, &[("symbol", Value::from(odd_case.clone()))]).await;
    assert_eq!(walk["outcome"], "resolved", "case-insensitive resolution is the premise: {walk}");
    let card = poll(&client, "symbol_info", args(&[("symbol", Value::from(odd_case))])).await;
    assert_eq!(
        walk["anchor"]["graph_id"], card["graph_id"],
        "asked in another case, the two tools still name ONE node: {walk} / {card}",
    );
    assert_eq!(
        card["graph_id"], from_card,
        "and it is the node's own id, spelled as declared — not as asked for: {card}",
    );

    client.cancel().await.ok();
}

/// A positional anchor names the same graph node a named anchor does.
///
/// The id is a property of the symbol, not of the sentence a caller used to reach it. Before
/// this, an anchor by position carried no declaration at all, so the id — which is derived
/// from one — silently vanished for exactly the method that carries it when named.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_positional_anchor_names_the_same_node_as_a_named_one() {
    let ws = stage_workspace();
    let client = client_with_references(ws.path()).await;

    let named = references(&client, &[("symbol", Value::from(STAND_METHOD))]).await;
    let expected = named["anchor"]["graph_id"]
        .as_str()
        .unwrap_or_else(|| panic!("the named anchor names its node: {named}"))
        .to_owned();

    // Reached through one of its own occurrences, so the stand cannot drift apart from the
    // coordinates: they come out of the answer above.
    let place = named["references"]
        .as_array()
        .expect("the list")
        .iter()
        .map(|entry| &entry["location"])
        .next()
        .expect("at least one occurrence")
        .clone();

    let positional = references(
        &client,
        &[
            ("root_id", place["root_id"].clone()),
            ("path", place["path"].clone()),
            ("line", place["range"]["start_line"].clone()),
            ("column", place["range"]["start_character"].clone()),
        ],
    )
    .await;
    assert_eq!(positional["outcome"], "resolved", "{positional}");
    assert_eq!(positional["anchor"]["mode"], "position", "{positional}");
    assert_eq!(
        positional["anchor"]["graph_id"], expected,
        "the same method, reached by position, names the same node: {positional}",
    );

    client.cancel().await.ok();
}

/// A name that is not a method gets NO key, rather than an empty one.
///
/// The graph's grammar addresses methods; a module variable is not a node in it. An empty
/// string would be an id a client can carry to `graph` and get nothing for, which is exactly
/// the failure an absent key makes impossible.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_variable_anchor_carries_no_graph_id_at_all() {
    let ws = stage_workspace();
    write_module(
        ws.path(),
        "СоСостоянием",
        "Перем Счётчик Экспорт;

Процедура Считать() Экспорт
    Счётчик = 1;
КонецПроцедуры
",
    );
    let client = client_with_references(ws.path()).await;

    let walk = references(&client, &[("symbol", Value::from("СоСостоянием.Счётчик"))]).await;
    let anchor = &walk["anchor"];
    assert!(
        anchor.get("graph_id").is_none(),
        "a variable has no node in the graph, so the key must be absent, not empty: {walk}",
    );

    // Control in the same stand: a method of the very same shape DOES carry one, so the
    // assertion above is about what the anchor is and not about the key never being served.
    let method = references(&client, &[("symbol", Value::from("СоСостоянием.Считать"))]).await;
    assert!(
        method["anchor"]["graph_id"].is_string(),
        "the method anchor must carry a key, or the check above proves nothing: {method}",
    );

    client.cancel().await.ok();
}

/// One form event handler, three ways to reach it, one graph id.
///
/// A form module has no module-keyed scope, so its handler is addressed by the graph's
/// path-fallback id. Only the named card minted that fallback: the positional card and the
/// reference anchor computed a module-keyed id and got nothing back — so a handler that is
/// perfectly addressable had an id or not depending on how the client asked. The id is a
/// property of the symbol; this gate holds the three paths to one answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_form_handler_has_one_id_however_it_is_reached() {
    let ws = stage_workspace();
    let client = client_with_references(ws.path()).await;

    let symbol = "Документ.Документ1.Форма.ФормаДокумента.ПриЗаписиНаСервере";
    let named = poll(&client, "symbol_info", args(&[("symbol", Value::from(symbol))])).await;
    assert_eq!(named["status"], "ok", "the named card resolves the handler: {named}");
    let expected = named["graph_id"]
        .as_str()
        .unwrap_or_else(|| panic!("a form handler is addressable by the path fallback: {named}"))
        .to_owned();
    assert!(
        expected.starts_with("method/file/"),
        "and it is the fallback form, which is the whole point of this gate: {expected}",
    );

    let place = named["definitions"][0]["location"].clone();
    let positional_args = args(&[
        ("root_id", place["root_id"].clone()),
        ("path", place["path"].clone()),
        ("line", place["range"]["start_line"].clone()),
        ("column", place["range"]["start_character"].clone()),
    ]);

    let positional = poll(&client, "symbol_info", positional_args.clone()).await;
    assert_eq!(
        positional["graph_id"], expected,
        "the positional card names the same node: {positional}",
    );

    let anchored = references(
        &client,
        &[
            ("root_id", place["root_id"].clone()),
            ("path", place["path"].clone()),
            ("line", place["range"]["start_line"].clone()),
            ("column", place["range"]["start_character"].clone()),
        ],
    )
    .await;
    assert_eq!(
        anchored["anchor"]["graph_id"], expected,
        "and so does the reference anchor: {anchored}",
    );

    client.cancel().await.ok();
}
