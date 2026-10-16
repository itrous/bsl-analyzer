use std::{env, fs, process::Command};

use tempfile::TempDir;

#[path = "fixtures/diagnostic_rules/mod.rs"]
mod rules;

const WARNING: &str = "failed to deserialize project diagnostics config; using defaults";

fn analyze(project: &TempDir) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_bsl-analyzer-app"))
        .args(["analyze", "-s"])
        .arg(project.path())
        .arg("-q")
        .env("BSL_LOG", "warn")
        .output()
        .expect("run bsl-analyzer")
}

fn project() -> TempDir {
    let project = TempDir::new().expect("tempdir");
    fs::write(project.path().join("Module.bsl"), "Процедура Тест()\nКонецПроцедуры\n")
        .expect("fixture");
    project
}

#[test]
fn analyze_distinguishes_absent_and_invalid_diagnostics_config() {
    let absent = project();
    let output = analyze(&absent);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "stderr:\n{stderr}");
    assert_eq!(stderr.matches(WARNING).count(), 0, "stderr:\n{stderr}");

    let invalid = project();
    fs::write(
        invalid.path().join(".bsl-analyzer.json"),
        r#"{"diagnostics":{"ordinaryAppSupport":"invalid"}}"#,
    )
    .expect("config");
    let output = analyze(&invalid);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "stderr:\n{stderr}");
    assert_eq!(stderr.matches(WARNING).count(), 1, "stderr:\n{stderr}");
}

#[test]
fn invalid_rule_shape_warns_in_analyze_and_check_config_without_changing_exit_policy() {
    let project = TempDir::new().unwrap();
    rules::project(project.path());
    for args in
        [&["analyze", "-s", ".", "-q"][..], &["check-config", "--config", "bsl-analyzer.toml"][..]]
    {
        let output = Command::new(env!("CARGO_BIN_EXE_bsl-analyzer-app"))
            .args(args)
            .current_dir(project.path())
            .env("BSL_LOG", "warn")
            .env_remove("BSL_LOG_FILE")
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{args:?}: {stderr}");
        assert_eq!(rules::warning_count(&stderr), 1, "{args:?}: {stderr}");
    }
    let valid = rules::config(3).replace(
        &format!("LineLength = \"{}\"", rules::PRIVATE_VALUE),
        "LineLength = { maxLineLength = 120 }",
    );
    fs::write(project.path().join("bsl-analyzer.toml"), valid).unwrap();
    let output = analyze(&project);
    assert!(output.status.success());
    assert_eq!(rules::warning_count(&String::from_utf8_lossy(&output.stderr)), 0);
}
