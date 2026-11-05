use std::{collections::BTreeMap, fs, path::Path, process::Command};

fn run(root: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_bsl-analyzer-app"))
        .current_dir(root)
        .args(args)
        .output()
        .unwrap()
}

fn setup() -> tempfile::TempDir {
    let temp = tempfile::tempdir().unwrap();
    for root in ["src/cf", "src/cfe/Ext"] {
        fs::create_dir_all(temp.path().join(root)).unwrap();
        fs::write(temp.path().join(root).join("Configuration.xml"), "<Configuration/>").unwrap();
    }
    fs::write(temp.path().join("src/cf/Main.bsl"), "Процедура Тест(\n").unwrap();
    fs::write(temp.path().join("src/cfe/Ext/Ext.bsl"), "Процедура Тест(\n").unwrap();
    fs::write(
        temp.path().join("bsl-analyzer.toml"),
        r#"[source]
root = "src/cf"
extensions = [{ name = "Ext", path = "src/cfe/Ext", scope = "InfoBase" }]
[diagnostics.baseline]
directory = "baselines"
include = ["main"]
"#,
    )
    .unwrap();
    assert!(run(temp.path(), &["diagnostics", "baseline", "create", "-s", "."]).status.success());
    fs::write(temp.path().join("src/cf/New.bsl"), "Процедура Новая(\n").unwrap();
    temp
}

#[test]
fn selective_baseline_reporters_keep_existing_containers_and_show_policy() {
    let temp = setup();
    fs::create_dir(temp.path().join("reports")).unwrap();
    let output = run(
        temp.path(),
        &["analyze", "-s", ".", "-r", "console,json,sarif,junit,codequality", "-o", "reports"],
    );
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let console = String::from_utf8(output.stdout).unwrap();
    assert!(console.contains("Selection: selective (enabled 1, unsuppressed 1)"));
    assert!(console.contains("main: full [baseline]"));
    assert!(console.contains("extension:Ext: full [unsuppressed]"));

    let json: serde_json::Value =
        serde_json::from_slice(&fs::read(temp.path().join("reports/bsl-json.json")).unwrap())
            .unwrap();
    assert_eq!(json["baseline"]["selection"], "selective");
    assert!(json["baseline"]["unsuppressed"].as_u64().unwrap() > 0);
    assert!(json["diagnostics"].as_array().unwrap().iter().any(|file| file["file"]
        .as_str()
        .unwrap()
        .replace('\\', "/")
        .starts_with("src/cfe/Ext/")));

    let sarif: serde_json::Value =
        serde_json::from_slice(&fs::read(temp.path().join("reports/bsl-analyzer.sarif")).unwrap())
            .unwrap();
    assert_eq!(sarif["runs"][0]["properties"]["baseline"]["selection"], "selective");
    assert!(sarif["runs"][0]["results"]
        .as_array()
        .unwrap()
        .iter()
        .all(|result| result.get("baselineState").is_none()));
    // The selection and per-partition policy are reported by SARIF and JSONL, asserted
    // above and below. JUnit carries no baseline summary at all: `<properties>` under
    // `<testsuites>` is not valid in the schema its consumers validate against.
    let junit = fs::read_to_string(temp.path().join("reports/bsl-analyzer.junit.xml")).unwrap();
    assert!(!junit.contains("<properties>"));

    let jsonl = run(temp.path(), &["analyze", "-s", ".", "--format", "jsonl"]);
    assert!(jsonl.status.success());
    let done: serde_json::Value = String::from_utf8(jsonl.stdout)
        .unwrap()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .find(|event: &serde_json::Value| event["type"] == "done")
        .unwrap();
    assert_eq!(done["baseline"]["selection"], "selective");
    assert!(done["baseline"]["partitions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|partition| partition["policy"] == "unsuppressed"));
}

#[test]
fn selective_baseline_codequality_and_sarif_preserve_fingerprint_semantics() {
    let temp = setup();
    fs::create_dir(temp.path().join("selective")).unwrap();
    let output = run(
        temp.path(),
        &["analyze", "-s", ".", "-r", "codequality,sarif", "-o", "selective", "-q"],
    );
    assert!(output.status.success());
    let selective: serde_json::Value = serde_json::from_slice(
        &fs::read(temp.path().join("selective/gl-code-quality-report.json")).unwrap(),
    )
    .unwrap();
    let fingerprints = |value: &serde_json::Value| {
        value
            .as_array()
            .unwrap()
            .iter()
            .filter(|finding| {
                finding["location"]["path"].as_str().unwrap().starts_with("src/cfe/Ext/")
            })
            .map(|finding| {
                (
                    finding["check_name"].as_str().unwrap().to_owned(),
                    finding["fingerprint"].as_str().unwrap().to_owned(),
                )
            })
            .collect::<BTreeMap<_, _>>()
    };
    let selective_fingerprints = fingerprints(&selective);
    assert!(!selective_fingerprints.is_empty());

    fs::write(
        temp.path().join("bsl-analyzer.toml"),
        r#"[source]
root = "src/cf"
extensions = [{ name = "Ext", path = "src/cfe/Ext", scope = "InfoBase" }]
"#,
    )
    .unwrap();
    fs::create_dir(temp.path().join("plain")).unwrap();
    assert!(run(temp.path(), &["analyze", "-s", ".", "-r", "codequality", "-o", "plain", "-q"],)
        .status
        .success());
    let plain: serde_json::Value = serde_json::from_slice(
        &fs::read(temp.path().join("plain/gl-code-quality-report.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(selective_fingerprints, fingerprints(&plain));

    let sarif: serde_json::Value = serde_json::from_slice(
        &fs::read(temp.path().join("selective/bsl-analyzer.sarif")).unwrap(),
    )
    .unwrap();
    assert!(sarif["runs"][0]["results"]
        .as_array()
        .unwrap()
        .iter()
        .all(|result| result.get("baselineState").is_none()));
}
