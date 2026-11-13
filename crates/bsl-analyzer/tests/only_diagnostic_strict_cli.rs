//! `--only-diagnostic` with a name that is not a diagnostic code must fail the run:
//! ignoring it would widen the run to every code and still exit 0.

use std::path::Path;
use std::process::{Command, Output};

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn fixture(root: &Path) {
    write(
        &root.join("Configuration.xml"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:v8="http://v8.1c.ru/8.1/data/core">
	<Configuration uuid="11111111-0000-0000-0000-000000000001">
		<Properties><Name>Тест</Name><Synonym/><Comment/><DefaultRunMode>ManagedApplication</DefaultRunMode><CompatibilityMode>Version8_3_17</CompatibilityMode></Properties>
		<ChildObjects><CommonModule>Модуль</CommonModule></ChildObjects>
	</Configuration>
</MetaDataObject>"#,
    );
    write(&root.join("CommonModules/Модуль/Ext/Module.bsl"), "Процедура Тест()\nКонецПроцедуры\n");
}

fn analyze(root: &Path, only_diagnostic: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_bsl-analyzer-app"))
        .arg("analyze")
        .arg("-s")
        .arg(root)
        .args(["--format", "jsonl", "--only-diagnostic", only_diagnostic])
        .env_remove("ONEC_CONFIGURATIONS_ROOT")
        .output()
        .expect("failed to run the analyzer")
}

fn output_text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// Exit code 2 marks argument misuse, and nothing reaches stdout: the JSONL stream
/// does not even start, so no consumer mistakes the run for an empty result.
fn assert_rejected(output: &Output) -> String {
    let text = output_text(output);
    assert_eq!(output.status.code(), Some(2), "{text}");
    assert!(output.stdout.is_empty(), "{text}");
    text
}

#[test]
fn an_unknown_name_fails_the_run_and_is_named() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    let output = analyze(dir.path(), "NoSuchDiagnosticCode");
    let text = assert_rejected(&output);
    assert!(text.contains("NoSuchDiagnosticCode"), "{text}");
    assert!(text.contains("--only-diagnostic"), "{text}");
}

#[test]
fn a_comma_list_is_one_unknown_name_and_fails_the_run() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    let output = analyze(dir.path(), "LineLength,UnusedLocalVariable");
    let text = assert_rejected(&output);
    assert!(text.contains("LineLength,UnusedLocalVariable"), "{text}");
}

#[test]
fn a_known_code_still_runs() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    let output = analyze(dir.path(), "LineLength");
    assert!(output.status.success(), "{}", output_text(&output));
}
