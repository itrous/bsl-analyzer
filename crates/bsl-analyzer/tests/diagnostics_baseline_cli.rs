use std::{fs, path::Path, process::Command};

use tempfile::TempDir;

const BROKEN: &str = "Процедура Тест(\n";

fn run(root: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_bsl-analyzer-app"))
        .current_dir(root)
        .args(args)
        .output()
        .expect("run bsl-analyzer")
}

fn assert_success(output: &std::process::Output) {
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn setup() -> TempDir {
    let temp = tempfile::tempdir().unwrap();
    for root in ["src/cf", "src/cfe/ext"] {
        fs::create_dir_all(temp.path().join(root)).unwrap();
        fs::write(temp.path().join(root).join("Configuration.xml"), "<Configuration/>").unwrap();
    }
    fs::write(temp.path().join("src/cf/Main.bsl"), BROKEN).unwrap();
    fs::write(temp.path().join("src/cfe/ext/Ext.bsl"), BROKEN).unwrap();
    fs::write(
        temp.path().join("bsl-analyzer.toml"),
        r#"[source]
root = "src/cf"
extensions = [{ name = "Ext", path = "src/cfe/ext", scope = "InfoBase" }]

[diagnostics.baseline]
path = ".bsl-diagnostics-baseline.json"
"#,
    )
    .unwrap();
    temp
}

#[test]
fn diagnostics_baseline_cli() {
    let temp = setup();
    let root = temp.path();
    assert_success(&run(root, &["diagnostics", "baseline", "create", "-s", "."]));
    let baseline_path = root.join(".bsl-diagnostics-baseline.json");
    let baseline: serde_json::Value =
        serde_json::from_slice(&fs::read(&baseline_path).unwrap()).unwrap();
    assert_eq!(baseline["scope"]["source_root"], "src/cf");
    assert_eq!(baseline["scope"]["extensions"][0]["name"], "Ext");
    assert_eq!(baseline["scope"]["extensions"][0]["scope"], "InfoBase");
    assert!(!baseline["diagnostics"].as_array().unwrap().is_empty());

    let reports = root.join("reports");
    fs::create_dir(&reports).unwrap();
    assert_success(&run(
        root,
        &[
            "analyze",
            "-s",
            ".",
            "-w",
            root.parent().unwrap().to_str().unwrap(),
            "-r",
            "sarif",
            "-o",
            "reports",
            "-q",
        ],
    ));
    let sarif: serde_json::Value =
        serde_json::from_slice(&fs::read(reports.join("bsl-analyzer.sarif")).unwrap()).unwrap();
    assert_eq!(sarif["runs"][0]["properties"]["baseline"]["new"], 0);

    fs::write(root.join("src/cf/New.bsl"), BROKEN).unwrap();
    let analyze_with_new = run(root, &["analyze", "-s", ".", "--format", "jsonl"]);
    assert_success(&analyze_with_new);
    let done: serde_json::Value = String::from_utf8(analyze_with_new.stdout)
        .unwrap()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .find(|event: &serde_json::Value| event["type"] == "done")
        .unwrap();
    assert!(done["baseline"]["new"].as_u64().unwrap() > 0);

    fs::remove_file(root.join("src/cfe/ext/Ext.bsl")).unwrap();
    assert!(!run(root, &["diagnostics", "baseline", "check", "-s", "."]).status.success());
    assert_success(&run(root, &["diagnostics", "baseline", "update", "-s", "."]));
    assert_success(&run(root, &["diagnostics", "baseline", "check", "-s", "."]));

    let valid = fs::read(&baseline_path).unwrap();
    fs::write(&baseline_path, b"broken").unwrap();
    assert!(!run(root, &["analyze", "-s", ".", "-q"]).status.success());
    fs::remove_file(&baseline_path).unwrap();
    assert!(!run(root, &["analyze", "-s", ".", "-q"]).status.success());
    fs::write(&baseline_path, valid).unwrap();
}

#[test]
fn legacy_extension_baseline_identity_changes_when_scope_becomes_explicit() {
    let temp = setup();
    let root = temp.path();
    let config_path = root.join("bsl-analyzer.toml");
    let explicit = fs::read_to_string(&config_path).unwrap();
    fs::write(&config_path, explicit.replace(", scope = \"InfoBase\"", "")).unwrap();

    assert_success(&run(root, &["diagnostics", "baseline", "create", "-s", "."]));
    let baseline_path = root.join(".bsl-diagnostics-baseline.json");
    let legacy: serde_json::Value =
        serde_json::from_slice(&fs::read(&baseline_path).unwrap()).unwrap();
    let legacy_bytes = fs::read(&baseline_path).unwrap();
    assert_eq!(legacy["scope"]["extensions"][0]["legacyScope"], true);
    assert!(legacy["scope"]["extensions"][0].get("scope").is_none());
    assert_success(&run(root, &["diagnostics", "baseline", "check", "-s", "."]));

    fs::write(&config_path, explicit).unwrap();
    assert!(
        !run(root, &["diagnostics", "baseline", "check", "-s", "."]).status.success(),
        "explicit scope must invalidate the legacy baseline identity"
    );
    assert!(
        !run(root, &["diagnostics", "baseline", "update", "-s", "."]).status.success(),
        "update must not silently rewrite a baseline from another project scope"
    );
    assert_eq!(
        fs::read(&baseline_path).unwrap(),
        legacy_bytes,
        "refused update leaves the old baseline untouched"
    );
    fs::remove_file(&baseline_path).unwrap();
    assert_success(&run(root, &["diagnostics", "baseline", "create", "-s", "."]));
    let explicit_baseline: serde_json::Value =
        serde_json::from_slice(&fs::read(&baseline_path).unwrap()).unwrap();
    assert_eq!(explicit_baseline["scope"]["extensions"][0]["scope"], "InfoBase");
    assert!(explicit_baseline["scope"]["extensions"][0].get("legacyScope").is_none());
    assert_success(&run(root, &["diagnostics", "baseline", "check", "-s", "."]));
}

#[test]
fn create_without_configuration_fails_before_analysis() {
    let temp = setup();
    fs::write(
        temp.path().join("bsl-analyzer.toml"),
        "[source]\nroot = \"src/cf\"\nextensions = []\n",
    )
    .unwrap();

    let output = run(temp.path(), &["diagnostics", "baseline", "create", "-s", "."]);

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("not configured"));
    assert!(!temp.path().join(".bsl-diagnostics-baseline.json").exists());
}

#[test]
fn disabled_rule_is_not_written() {
    let create_codes = |disabled: bool| {
        let temp = setup();
        fs::write(
            temp.path().join("src/cf/Main.bsl"),
            "Процедура Тест()\n    Неиспользуемая = 1;\nКонецПроцедуры\n",
        )
        .unwrap();
        if disabled {
            let config = fs::read_to_string(temp.path().join("bsl-analyzer.toml")).unwrap();
            fs::write(
                temp.path().join("bsl-analyzer.toml"),
                format!("{config}\n[diagnostics.parameters]\nUnusedLocalVariable = false\n"),
            )
            .unwrap();
        }
        assert_success(&run(temp.path(), &["diagnostics", "baseline", "create", "-s", "."]));
        let baseline: serde_json::Value = serde_json::from_slice(
            &fs::read(temp.path().join(".bsl-diagnostics-baseline.json")).unwrap(),
        )
        .unwrap();
        baseline["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["code"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    };

    assert!(create_codes(false).iter().any(|code| code == "UnusedLocalVariable"));
    assert!(create_codes(true).iter().all(|code| code != "UnusedLocalVariable"));
}

#[test]
fn machine_output_reports_created_entries() {
    let temp = setup();
    let output =
        run(temp.path(), &["diagnostics", "baseline", "create", "-s", ".", "--format", "json"]);
    assert_success(&output);
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["operation"], "created");
    assert_eq!(result["success"], true);
    assert_eq!(
        result["added"].as_u64().unwrap(),
        result["diagnostics"].as_array().unwrap().len() as u64
    );
    assert!(!result["diagnostics"].as_array().unwrap().is_empty());

    fs::write(temp.path().join("src/cf/New.bsl"), BROKEN).unwrap();
    fs::remove_file(temp.path().join("src/cfe/ext/Ext.bsl")).unwrap();
    let output =
        run(temp.path(), &["diagnostics", "baseline", "check", "-s", ".", "--format", "json"]);
    assert!(!output.status.success());
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["operation"], "checked");
    assert_eq!(result["success"], false);
    assert!(result["added"].as_u64().unwrap() > 0);
    assert!(result["removed"].as_u64().unwrap() > 0);
    assert_eq!(
        result["diagnostics"].as_array().unwrap().len() as u64,
        result["added"].as_u64().unwrap() + result["removed"].as_u64().unwrap()
    );
}

#[test]
fn documented_usage() {
    let root = setup();
    let diagnostics = include_str!("../../../docs/configuration/DIAGNOSTICS.md");
    let ci = include_str!("../../../docs/CI_REPORTERS.md");
    for command in [
        "diagnostics baseline create -s .",
        "diagnostics baseline check -s .",
        "diagnostics baseline update -s .",
    ] {
        assert!(diagnostics.contains(command) || ci.contains(command), "missing {command}");
    }
    assert_success(&run(root.path(), &["diagnostics", "baseline", "create", "-s", "."]));
    assert_success(&run(root.path(), &["diagnostics", "baseline", "check", "-s", "."]));
}

#[test]
fn documented_migration() {
    let root = setup();
    assert_success(&run(root.path(), &["diagnostics", "baseline", "create", "-s", "."]));
    fs::write(root.path().join("src/cf/New.bsl"), BROKEN).unwrap();
    assert!(!run(root.path(), &["diagnostics", "baseline", "check", "-s", "."]).status.success());
    assert_success(&run(root.path(), &["diagnostics", "baseline", "update", "-s", "."]));
    assert_success(&run(root.path(), &["diagnostics", "baseline", "check", "-s", "."]));
}

/// A malformed suppression directive raises a protected diagnostic. Protected
/// diagnostics stay active and are deliberately kept out of the baseline file —
/// which must mean "not recorded", not "the command fails".
const MALFORMED_SUPPRESSION: &str =
    "Процедура Тест()\n    // bsl-analyzer:off NoSuchRule\n    А = А;\nКонецПроцедуры\n";

#[test]
fn protected_diagnostics_do_not_block_baseline_create_and_update() {
    let temp = setup();
    let root = temp.path();
    fs::write(root.join("src/cf/Main.bsl"), MALFORMED_SUPPRESSION).unwrap();

    assert_success(&run(root, &["diagnostics", "baseline", "create", "-s", "."]));
    let baseline: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join(".bsl-diagnostics-baseline.json")).unwrap())
            .unwrap();
    let codes: Vec<_> = baseline["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["code"].as_str().unwrap())
        .collect();
    assert!(
        !codes
            .iter()
            .any(|code| matches!(*code, "UnknownSuppressionCode" | "SuppressionWithoutCode")),
        "a protected diagnostic must never be recorded: {codes:?}"
    );

    assert_success(&run(root, &["diagnostics", "baseline", "update", "-s", "."]));
}

const RECORDED_AB: &str = "Процедура Тест()\n    А = А;\n    А = А;\nКонецПроцедуры\n";
const CURRENT_ABC: &str = "Процедура Тест()\n    А = А;\n    А = А;\n    А = А;\nКонецПроцедуры\n";
const FINALITY_MODULE: &str = "src/cf/CommonModules/М/Ext/Module.bsl";

/// A project whose module holds the recorded self-assignments a and b on 1-based lines 2
/// and 3, with `SelfAssign` named explicitly so the fixture does not rest on its default.
fn finality_project(baseline: &str) -> TempDir {
    let temp = tempfile::tempdir().unwrap();
    for root in ["src/cf", "src/cfe/ext"] {
        fs::create_dir_all(temp.path().join(root)).unwrap();
        fs::write(temp.path().join(root).join("Configuration.xml"), "<Configuration/>").unwrap();
    }
    let module = temp.path().join(FINALITY_MODULE);
    fs::create_dir_all(module.parent().unwrap()).unwrap();
    fs::write(&module, RECORDED_AB).unwrap();
    fs::write(
        temp.path().join("bsl-analyzer.toml"),
        format!(
            "[source]\nroot = \"src/cf\"\n\
             extensions = [{{ name = \"Ext\", path = \"src/cfe/ext\", scope = \"InfoBase\" }}]\n\n\
             [diagnostics.parameters]\nSelfAssign = true\n\n\
             {baseline}"
        ),
    )
    .unwrap();
    temp
}

/// The self-assignments a JSONL run reports, as 0-based start lines, with the baseline
/// summary of its `done` event; scoped to `hunks` of the module when given.
fn reported_self_assigns(root: &Path, hunks: Option<&str>) -> (Vec<u64>, serde_json::Value) {
    let report = root.join("diff-report.json");
    let mut args = vec!["analyze", "-s", ".", "--format", "jsonl", "-q"];
    if let Some(hunks) = hunks {
        fs::write(
            &report,
            format!(
                r#"{{"base_ref": "vendor", "head_ref": "HEAD", "files": {{"{FINALITY_MODULE}": {{"hunks": {hunks}}}}}}}"#
            ),
        )
        .unwrap();
        args.extend(["--diff-filter", report.to_str().unwrap()]);
    }
    let output = run(root, &args);
    assert_success(&output);
    let mut lines = Vec::new();
    let mut done = serde_json::Value::Null;
    for event in String::from_utf8(output.stdout).unwrap().lines() {
        let event: serde_json::Value = serde_json::from_str(event).unwrap();
        match event["type"].as_str() {
            Some("file") => lines.extend(
                event["diagnostics"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|d| d["code"] == "SelfAssign")
                    .map(|d| d["start_line"].as_u64().unwrap()),
            ),
            Some("done") => done = event,
            _ => {}
        }
    }
    (lines, done["baseline"].clone())
}

/// The counterexample end to end: the baseline records a and b, c is added, and a run
/// scoped to the lines of b and c shows c alone — numbered on the whole file, not
/// renumbered into b's place — for the v1 baseline and a partition set; an `Unsuppressed`
/// partition shows b and c, as does a project without a baseline.
#[test]
fn finality_scoped_baseline() {
    let cases = [
        ("[diagnostics.baseline]\npath = \".bsl-diagnostics-baseline.json\"\n", vec![3]),
        ("[diagnostics.baseline]\ndirectory = \"baselines\"\n", vec![3]),
        (
            "[diagnostics.baseline]\ndirectory = \"baselines\"\ninclude = [\"extension:Ext\"]\n",
            vec![2, 3],
        ),
        ("", vec![2, 3]),
    ];
    for (baseline, expected) in cases {
        let temp = finality_project(baseline);
        let root = temp.path();
        if !baseline.is_empty() {
            assert_success(&run(root, &["diagnostics", "baseline", "create", "-s", "."]));
        }
        fs::write(root.join(FINALITY_MODULE), CURRENT_ABC).unwrap();

        let (shown, summary) = reported_self_assigns(root, Some("[[3, 4]]"));
        assert_eq!(shown, expected, "{baseline}");
        // A scope of a and b leaves c, the new finding, outside it.
        let (cut, _) = reported_self_assigns(root, Some("[[2, 3]]"));
        let expected_cut: Vec<u64> = if expected == vec![3] { vec![] } else { vec![1, 2] };
        assert_eq!(cut, expected_cut, "{baseline}: the scope must cut c");
        if expected == vec![3] {
            let (_, whole) = reported_self_assigns(root, None);
            assert_eq!(summary["known"], whole["known"], "the summary counts the whole set");
            assert_eq!(summary["new"], 1, "{summary}");
            assert_eq!(summary["complete"], false, "{summary}");
        }
    }
}

/// Runs git in `dir` without the ambient `GIT_*` environment: inside a pre-commit hook it
/// would point at the real repository instead of the fixture.
fn run_git(dir: &Path, args: &[&str]) {
    let mut command = Command::new("git");
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("GIT_") {
            command.env_remove(&key);
        }
    }
    let output = command
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=test", "-c", "user.email=test@example.com"])
        .args(args)
        .output()
        .expect("run git");
    assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
}

/// Baseline commands keep refusing a scoped analysis: the scoped run classifies the whole
/// file only to decide what is shown, and that grants no right to write or check a
/// baseline from it. The refusal is the coverage one it always was, and the baseline an
/// unscoped `create` wrote stays byte for byte.
#[test]
fn finality_scoped_coverage() {
    let temp =
        finality_project("[diagnostics.baseline]\npath = \".bsl-diagnostics-baseline.json\"\n");
    let root = temp.path();
    run_git(root, &["init", "-q"]);
    run_git(root, &["add", "."]);
    run_git(root, &["commit", "-q", "-m", "vendor"]);
    run_git(root, &["branch", "vendor"]);
    fs::write(root.join(FINALITY_MODULE), CURRENT_ABC).unwrap();

    let config = root.join("bsl-analyzer.toml");
    let unscoped = fs::read_to_string(&config).unwrap();
    let scoped = format!("{unscoped}\n[analysis]\ndiff_base = \"vendor\"\n");
    let baseline_command = |command: &str| {
        let mut process = Command::new(env!("CARGO_BIN_EXE_bsl-analyzer-app"));
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("GIT_") {
                process.env_remove(&key);
            }
        }
        process
            .current_dir(root)
            .args(["diagnostics", "baseline", command, "-s", "."])
            .output()
            .unwrap()
    };
    let assert_refused = |command: &str| {
        let output = baseline_command(command);
        assert!(!output.status.success(), "{command} must refuse a scoped analysis");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("full diagnostics coverage required"), "{command}: {stderr}");
    };
    let baseline = root.join(".bsl-diagnostics-baseline.json");

    fs::write(&config, &scoped).unwrap();
    assert_refused("create");
    assert!(!baseline.exists(), "a refused create must not write");

    fs::write(&config, &unscoped).unwrap();
    assert_success(&baseline_command("create"));
    let written = fs::read(&baseline).unwrap();

    fs::write(&config, &scoped).unwrap();
    for command in ["check", "update"] {
        assert_refused(command);
        assert_eq!(fs::read(&baseline).unwrap(), written, "{command} touched the baseline");
    }
}
