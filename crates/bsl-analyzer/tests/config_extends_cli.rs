//! `extends` in `bsl-analyzer.toml`: the project file inherits a shared parent
//! that lives elsewhere, the parent's relative `[source].root` is read against the
//! project, and the project can switch off a rule the parent leaves on.

use std::path::Path;
use std::process::Command;

use serde_json::Value;

const CODE: &str = "CommonModuleInvalidType";

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// An ordinary-application configuration under `project/src/cf` whose only common
/// module (`Сервер` + `ВнешнееСоединение`) is a `CommonModuleInvalidType` finding,
/// and a shared parent config in `shared/` that points `[source].root` at `src/cf`.
fn fixture(project: &Path, shared: &Path) {
    let cf = project.join("src/cf");
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
    write(
        &shared.join("base.toml"),
        "[source]\nroot = \"src/cf\"\nextensions = []\nexternals = []\n\n[diagnostics.parameters]\nLineLength = false\n",
    );
}

/// `extends` pointing at the shared parent, relative to the project directory.
fn child(project: &Path, extra: &str) {
    write(
        &project.join("bsl-analyzer.toml"),
        &format!("extends = \"../shared/base.toml\"\n{extra}"),
    );
}

/// How many [`CODE`] findings `analyze` reports for the common module. Not
/// narrowed with `--only-diagnostic`: that flag overrides the config's switches.
fn findings(project: &Path) -> usize {
    let output = Command::new(env!("CARGO_BIN_EXE_bsl-analyzer-app"))
        .arg("analyze")
        .arg("-s")
        .arg(project)
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
fn the_parent_sets_the_source_root_and_leaves_the_rule_on() {
    let dir = tempfile::tempdir().unwrap();
    let (project, shared) = (dir.path().join("project"), dir.path().join("shared"));
    fixture(&project, &shared);
    child(&project, "");
    assert_eq!(findings(&project), 1);
}

#[test]
fn the_child_switches_off_a_rule_the_parent_leaves_on() {
    let dir = tempfile::tempdir().unwrap();
    let (project, shared) = (dir.path().join("project"), dir.path().join("shared"));
    fixture(&project, &shared);
    child(&project, &format!("\n[diagnostics.parameters]\n{CODE} = false\n"));
    assert_eq!(findings(&project), 0);
}

#[test]
fn a_missing_parent_fails_the_run_naming_it() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    fixture(&project, &dir.path().join("elsewhere"));
    child(&project, "");
    let output = Command::new(env!("CARGO_BIN_EXE_bsl-analyzer-app"))
        .arg("analyze")
        .arg("-s")
        .arg(&project)
        .args(["--format", "jsonl"])
        .output()
        .expect("failed to run the analyzer");
    assert!(!output.status.success(), "a config with a missing parent must not load");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("base.toml"), "{stderr}");
}
