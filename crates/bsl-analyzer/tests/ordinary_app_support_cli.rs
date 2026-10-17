//! `ordinaryAppSupport` left out of the project config is taken from the main
//! configuration's `<DefaultRunMode>`, and `analyze` uses that one value both for
//! the diagnostics settings and for the execution model. An explicit setting still
//! wins.

use std::path::Path;
use std::process::Command;

use serde_json::Value;

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn common_module(cf: &Path, name: &str, uuid: &str, flags: &str, body: &str) {
    write(
        &cf.join(format!("CommonModules/{name}.xml")),
        &format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:v8="http://v8.1c.ru/8.1/data/core">
	<CommonModule uuid="{uuid}">
		<Properties><Name>{name}</Name><Synonym/><Comment/><Global>false</Global>{flags}<ServerCall>false</ServerCall><Privileged>false</Privileged><ReturnValuesReuse>DontUse</ReturnValuesReuse></Properties>
	</CommonModule>
</MetaDataObject>"#
        ),
    );
    write(&cf.join(format!("CommonModules/{name}/Ext/Module.bsl")), body);
}

/// `src/cf` with the given run mode, a session module, and two common modules: a
/// server one that calls a client one compiled only into the ordinary application.
/// `toml` is appended to the project config.
fn project(root: &Path, run_mode: &str, toml: &str) {
    let cf = root.join("src/cf");
    write(
        &cf.join("Configuration.xml"),
        &format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:v8="http://v8.1c.ru/8.1/data/core">
	<Configuration uuid="11111111-0000-0000-0000-000000000001">
		<Properties><Name>Конфигурация</Name><Synonym/><Comment/><DefaultRunMode>{run_mode}</DefaultRunMode><UseManagedFormInOrdinaryApplication>false</UseManagedFormInOrdinaryApplication><UseOrdinaryFormInManagedApplication>false</UseOrdinaryFormInManagedApplication><CompatibilityMode>Version8_3_17</CompatibilityMode></Properties>
		<ChildObjects><CommonModule>Серверный</CommonModule><CommonModule>ОбычныйКлиент</CommonModule></ChildObjects>
	</Configuration>
</MetaDataObject>"#
        ),
    );
    write(
        &cf.join("Ext/SessionModule.bsl"),
        "Процедура УстановкаПараметровСеанса(ТребуемыеПараметры)\nКонецПроцедуры\n",
    );
    common_module(
        &cf,
        "Серверный",
        "22222222-0000-0000-0000-000000000002",
        "<ClientManagedApplication>false</ClientManagedApplication><Server>true</Server>\
         <ExternalConnection>true</ExternalConnection>\
         <ClientOrdinaryApplication>true</ClientOrdinaryApplication>",
        "Процедура Проверить() Экспорт\n\tОбычныйКлиент.Показать();\nКонецПроцедуры\n",
    );
    common_module(
        &cf,
        "ОбычныйКлиент",
        "33333333-0000-0000-0000-000000000003",
        "<ClientManagedApplication>false</ClientManagedApplication><Server>false</Server>\
         <ExternalConnection>false</ExternalConnection>\
         <ClientOrdinaryApplication>true</ClientOrdinaryApplication>",
        "Процедура Показать() Экспорт\nКонецПроцедуры\n",
    );
    write(&root.join("bsl-analyzer.toml"), &format!("[source]\nroot = \"src/cf\"\n\n{toml}"));
}

/// `(code, module file)` pairs `analyze` reports for the two codes this test is
/// about: `OrdinaryAppSupport` (a diagnostics rule that reads the setting) and
/// `ModuleAccessibility` (decided by the execution model), sorted.
fn findings(root: &Path) -> Vec<(String, String)> {
    let output = Command::new(env!("CARGO_BIN_EXE_bsl-analyzer-app"))
        .arg("analyze")
        .arg("-s")
        .arg(root)
        .args(["--format", "jsonl"])
        .env_remove("ONEC_CONFIGURATIONS_ROOT")
        .output()
        .expect("failed to run the analyzer");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8(output.stdout).unwrap();
    let mut found: Vec<(String, String)> = stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .filter(|e| e["type"] == "file")
        .flat_map(|e| {
            let path = e["path"].as_str().unwrap_or_default().replace('\\', "/");
            let module = path.rsplit_once("/src/cf/").map_or(path.clone(), |(_, m)| m.to_owned());
            e["diagnostics"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter_map(|d| d["code"].as_str().map(str::to_owned))
                .filter(|code| {
                    ["OrdinaryAppSupport", "ModuleAccessibility"].contains(&code.as_str())
                })
                .map(move |code| (code, module.clone()))
                .collect::<Vec<_>>()
        })
        .collect();
    found.sort_unstable();
    found
}

fn ordinary_findings() -> Vec<(String, String)> {
    vec![
        ("ModuleAccessibility".to_owned(), "CommonModules/Серверный/Ext/Module.bsl".to_owned()),
        ("OrdinaryAppSupport".to_owned(), "Ext/SessionModule.bsl".to_owned()),
    ]
}

/// Under the ordinary application the session-module rule checks the form-kind
/// properties (diagnostics settings), and the client module compiles into the
/// ordinary thick client, so the call from the server is reported (execution
/// model) - with no `[diagnostics]` or `[features]` setting at all.
#[test]
fn ordinary_run_mode_turns_ordinary_app_support_on() {
    let dir = tempfile::tempdir().unwrap();
    project(dir.path(), "OrdinaryApplication", "");
    assert_eq!(findings(dir.path()), ordinary_findings());
}

#[test]
fn managed_run_mode_leaves_ordinary_app_support_off() {
    let dir = tempfile::tempdir().unwrap();
    project(dir.path(), "ManagedApplication", "");
    assert_eq!(findings(dir.path()), Vec::new());
}

#[test]
fn an_explicit_setting_overrides_the_run_mode() {
    let dir = tempfile::tempdir().unwrap();
    project(dir.path(), "OrdinaryApplication", "[diagnostics]\nordinaryAppSupport = false\n");
    assert_eq!(findings(dir.path()), Vec::new());

    let dir = tempfile::tempdir().unwrap();
    project(dir.path(), "ManagedApplication", "[diagnostics]\nordinaryAppSupport = true\n");
    assert_eq!(findings(dir.path()), ordinary_findings());
}
