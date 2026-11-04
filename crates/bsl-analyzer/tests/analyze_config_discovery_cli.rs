//! `analyze` with no `-c` finds the project config above `--source-dir`: the source
//! directory is the configuration root (`<project>/src/cf`) while the config lives at
//! the project root, and a lookup in the source directory alone ran on defaults
//! without saying so.

use std::path::Path;
use std::process::Command;

use serde_json::Value;

const CODE: &str = "CommonModuleInvalidType";

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// An ordinary-application configuration under `project/src/cf` whose only common
/// module (`Сервер` + `ВнешнееСоединение`) is a `CommonModuleInvalidType` finding.
fn fixture(project: &Path) {
    fixture_at(&project.join("src/cf"));
}

fn fixture_at(cf: &Path) {
    write(
        &cf.join("Configuration.xml"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:v8="http://v8.1c.ru/8.1/data/core">
	<Configuration uuid="11111111-0000-0000-0000-000000000001">
		<Properties><Name>Тест</Name><Synonym/><Comment/><DefaultRunMode>OrdinaryApplication</DefaultRunMode><CompatibilityMode>Version8_3_17</CompatibilityMode></Properties>
		<ChildObjects><CommonModule>СерверныйМодуль</CommonModule></ChildObjects>
	</Configuration>
</MetaDataObject>"#,
    );
    write(
        &cf.join("CommonModules/СерверныйМодуль.xml"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:v8="http://v8.1c.ru/8.1/data/core">
	<CommonModule uuid="22222222-0000-0000-0000-000000000002">
		<Properties><Name>СерверныйМодуль</Name><Synonym/><Comment/><Global>false</Global><ClientManagedApplication>false</ClientManagedApplication><Server>true</Server><ExternalConnection>true</ExternalConnection><ClientOrdinaryApplication>false</ClientOrdinaryApplication><ServerCall>false</ServerCall><Privileged>false</Privileged><ReturnValuesReuse>DontUse</ReturnValuesReuse></Properties>
	</CommonModule>
</MetaDataObject>"#,
    );
    write(
        &cf.join("CommonModules/СерверныйМодуль/Ext/Module.bsl"),
        "Процедура Проверить() Экспорт\nКонецПроцедуры\n",
    );
}

/// How many [`CODE`] findings `analyze --source-dir <project>/src/cf` reports with no
/// `-c`. Not narrowed with `--only-diagnostic`: that flag overrides the config's switches.
fn findings(project: &Path) -> usize {
    findings_in(&project.join("src/cf"))
}

fn findings_in(source_dir: &Path) -> usize {
    findings_with(source_dir, &[])
}

fn findings_with(source_dir: &Path, flags: &[&str]) -> usize {
    let output = Command::new(env!("CARGO_BIN_EXE_bsl-analyzer-app"))
        .arg("analyze")
        .arg("-s")
        .arg(source_dir)
        .args(flags)
        .args(["--format", "jsonl"])
        .env_remove("ONEC_CONFIGURATIONS_ROOT")
        .output()
        .expect("failed to run the analyzer");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8(output.stdout).unwrap();
    let event = stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .find(|e| {
            e["type"] == "file"
                && e["path"].as_str().is_some_and(|p| {
                    Path::new(p).ends_with("CommonModules/СерверныйМодуль/Ext/Module.bsl")
                })
        })
        .unwrap_or_else(|| panic!("the common module was not analyzed: {stdout}"));
    event["diagnostics"].as_array().unwrap().iter().filter(|d| d["code"] == CODE).count()
}

#[test]
fn a_config_at_the_project_root_applies_to_the_source_dir_below_it() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    write(
        &dir.path().join("bsl-analyzer.toml"),
        &format!("[diagnostics.parameters]\n{CODE} = false\n"),
    );
    assert_eq!(findings(dir.path()), 0);
}

#[test]
fn without_a_config_the_rule_still_reports() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    assert_eq!(findings(dir.path()), 1);
}

#[test]
fn a_discovered_configs_relative_paths_resolve_against_its_own_directory() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    write(
        &dir.path().join("ext/E/Configuration.xml"),
        "<Properties><ConfigurationExtensionPurpose>Customization</ConfigurationExtensionPurpose></Properties>",
    );
    write(
        &dir.path().join("bsl-analyzer.toml"),
        "[source]\nextensions = [{ name = \"E\", path = \"ext/E\", dependsOn = [], scope = \"InfoBase\" }]\n",
    );
    // Against `-s` the path would be `src/cf/ext/E`, which does not exist, and the run would fail.
    assert_eq!(findings(dir.path()), 1);
}

#[test]
fn extension_flags_stay_relative_to_the_source_dir_under_a_discovered_config() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    // A config above `-s` makes the project root differ from it; the flags are typed from `-s`.
    write(&dir.path().join("bsl-analyzer.toml"), "");
    let manifest = "<Properties><ConfigurationExtensionPurpose>Customization</ConfigurationExtensionPurpose></Properties>";
    for name in ["F", "G"] {
        write(&dir.path().join(format!("src/cf/ext/{name}/Configuration.xml")), manifest);
    }
    let source_dir = dir.path().join("src/cf");
    assert_eq!(findings_with(&source_dir, &["--extension", "F=ext/F"]), 1);
    assert_eq!(
        findings_with(&source_dir, &["--extension", "ext/*"]),
        1,
        "a glob must survive the rebase"
    );
}
