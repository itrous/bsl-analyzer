//! `analyze` reports the same findings, in JSONL and in SARIF, as the recorded output
//! of a configuration whose form module has handlers bound by string from another
//! module. Warming the source root's handler bindings ahead of the chunked run is a
//! scheduling change: it must not move one finding, and the bound handlers must stay
//! out of `UnusedLocalMethod` / `UnusedParameters` while the unbound procedure does not.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;
use tempfile::TempDir;

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/foreign_handler_bindings")
}

fn analyze(args: &[&str]) -> std::process::Output {
    let output = Command::new(env!("CARGO_BIN_EXE_bsl-analyzer-app"))
        .arg("analyze")
        .arg("-s")
        .arg(fixture_root())
        .args(args)
        .arg("-q")
        .env_remove("ONEC_CONFIGURATIONS_ROOT")
        .output()
        .expect("run bsl-analyzer");
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

/// The JSONL events with what legitimately varies between runs and machines taken out:
/// the version, the elapsed time, the fixture's location, and the order of the file
/// events, which follows the directory walk.
fn normalize_jsonl(text: &str, root: &Path) -> Vec<Value> {
    let root = root.to_string_lossy().replace('\\', "/");
    let mut events: Vec<Value> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("a JSON event per line"))
        .collect();
    for event in &mut events {
        match event["type"].as_str() {
            Some("start") => event["version"] = Value::from("<version>"),
            Some("file") => {
                let path = event["path"].as_str().expect("file path").replace('\\', "/");
                let relative = path.strip_prefix(&format!("{root}/")).unwrap_or(&path).to_owned();
                event["path"] = Value::from(relative);
            }
            Some("done") => {
                event.as_object_mut().expect("done event").remove("elapsed_secs");
            }
            _ => {}
        }
    }
    let file_events = events.iter().position(|e| e["type"] == "file").unwrap_or(events.len());
    let done = events.iter().position(|e| e["type"] == "done").unwrap_or(events.len());
    events[file_events..done]
        .sort_by(|a, b| a["path"].as_str().unwrap_or("").cmp(b["path"].as_str().unwrap_or("")));
    events
}

fn normalize_sarif(mut report: Value) -> Value {
    report["runs"][0]["tool"]["driver"]["semanticVersion"] = Value::from("<version>");
    if let Some(results) = report["runs"][0]["results"].as_array_mut() {
        results.sort_by_key(|result| result.to_string());
    }
    report
}

#[test]
fn jsonl_output_matches_the_recorded_report() {
    let root = fixture_root();
    let output = analyze(&["--format", "jsonl"]);
    let actual = normalize_jsonl(&String::from_utf8(output.stdout).expect("utf-8 stdout"), &root);
    let expected =
        normalize_jsonl(include_str!("fixtures/foreign_handler_bindings/expected.jsonl"), &root);
    assert_eq!(
        actual,
        expected,
        "actual:\n{}",
        actual.iter().map(Value::to_string).collect::<Vec<_>>().join("\n")
    );

    let unused: Vec<&str> = actual
        .iter()
        .filter(|event| event["type"] == "file")
        .flat_map(|event| event["diagnostics"].as_array().expect("diagnostics").iter())
        .filter(|d| d["code"] == "UnusedLocalMethod")
        .map(|d| d["message"].as_str().expect("message"))
        .collect();
    assert_eq!(
        unused,
        ["Неиспользуемый локальный метод \"ДействительноНеИспользуется\""],
        "only the procedure no module binds is unused"
    );
}

#[test]
fn sarif_output_matches_the_recorded_report() {
    let temp = TempDir::new().expect("tempdir");
    let output_dir = temp.path().join("reports");
    std::fs::create_dir_all(&output_dir).expect("output dir");
    analyze(&["-r", "sarif", "-o", output_dir.to_str().expect("utf-8 path")]);

    let actual: Value = serde_json::from_str(
        &std::fs::read_to_string(output_dir.join("bsl-analyzer.sarif")).expect("sarif report"),
    )
    .expect("valid sarif json");
    let expected: Value =
        serde_json::from_str(include_str!("fixtures/foreign_handler_bindings/expected.sarif"))
            .expect("valid expected sarif");
    assert_eq!(normalize_sarif(actual), normalize_sarif(expected));
}
