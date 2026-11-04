use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;
use tempfile::TempDir;

const ISSUE_SOURCE: &str = include_str!("../../parser/tests/fixtures/issue150_ext_directives.bsl");

fn configuration_xml(name: &str, extension: bool) -> String {
    let purpose = if extension {
        "<ConfigurationExtensionPurpose>Customization</ConfigurationExtensionPurpose>"
    } else {
        ""
    };
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses">
  <Configuration uuid="11111111-1111-1111-1111-111111111111">
    <Properties><Name>{name}</Name>{purpose}</Properties>
    <ChildObjects><CommonModule>Appearance</CommonModule></ChildObjects>
  </Configuration>
</MetaDataObject>"#
    )
}

fn common_module_xml() -> &'static str {
    r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses">
  <CommonModule uuid="22222222-2222-2222-2222-222222222222">
    <Properties>
      <Name>Appearance</Name><Global>false</Global><Server>true</Server>
    </Properties>
  </CommonModule>
</MetaDataObject>"#
}

fn write_configuration(root: &Path, source: &str, extension: bool) -> PathBuf {
    let module = root.join("CommonModules/Appearance/Ext/Module.bsl");
    std::fs::create_dir_all(module.parent().expect("module parent")).expect("module dir");
    std::fs::write(
        root.join("Configuration.xml"),
        configuration_xml(if extension { "Repro" } else { "Base" }, extension),
    )
    .expect("configuration xml");
    std::fs::write(root.join("CommonModules/Appearance.xml"), common_module_xml())
        .expect("module xml");
    std::fs::write(&module, source).expect("module body");
    module
}

fn canonical_issue_source() -> String {
    ISSUE_SOURCE
        .replace("#Вставка\n\t|\tТ.Договор КАК Договор,\n#КонецВставки\n", "\t|\tТ.Договор КАК Договор,\n")
        .replace("\t\t#Вставка\n\t\tИ Не Данные.Флаг\n\t\t#КонецВставки\n", "\t\tИ Не Данные.Флаг\n")
        .replace(
            "\t\t#Удаление\n\t\tДанные.Ссылка,\n\t\t#КонецУдаления\n\t\t#Вставка\n\t\tДанные.НоваяСсылка,\n\t\t#КонецВставки\n",
            "\t\tДанные.НоваяСсылка,\n",
        )
}

fn run(source: &str) -> Vec<(String, String)> {
    let temp = TempDir::new().expect("tempdir");
    let project = temp.path();
    let cf = project.join("cf");
    let cfe = project.join("cfe/Repro");
    write_configuration(
        &cf,
        "Функция ЕстьТакойМетод() Экспорт\n    Возврат Истина;\nКонецФункции\n",
        false,
    );
    let changed = write_configuration(&cfe, source, true);
    std::fs::write(
        project.join("bsl-analyzer.toml"),
        r#"[source]
root = "cf"
extensions = [
  { name = "Repro", path = "cfe/Repro", scope = "InfoBase" },
]

[diagnostics.parameters]
ParseError = true
"#,
    )
    .expect("analyzer config");

    let output = Command::new(env!("CARGO_BIN_EXE_bsl-analyzer-app"))
        .current_dir(project)
        .args(["analyze", "--incremental", "-s", ".", "--changed-files"])
        .arg(&changed)
        .args(["--format", "jsonl", "--quiet"])
        .env_remove("ONEC_CONFIGURATIONS_ROOT")
        .output()
        .expect("run analyzer");
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let events: Vec<Value> = String::from_utf8(output.stdout)
        .expect("utf-8 jsonl")
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("json event"))
        .collect();
    let file = events
        .iter()
        .find(|event| {
            event["type"] == "file"
                && event["path"].as_str().is_some_and(|path| {
                    path.replace('\\', "/")
                        .ends_with("cfe/Repro/CommonModules/Appearance/Ext/Module.bsl")
                })
        })
        .unwrap_or_else(|| panic!("changed extension module was not analyzed: {events:?}"));
    assert_eq!(file["error"], Value::Null, "analysis failed: {file}");
    let done = events.iter().find(|event| event["type"] == "done").expect("done event");
    assert_eq!(done["failed_files"], 0);

    let mut claims: Vec<_> = file["diagnostics"]
        .as_array()
        .expect("diagnostics")
        .iter()
        .map(|diagnostic| {
            (
                diagnostic["code"].as_str().unwrap_or_default().to_string(),
                diagnostic["message"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect();
    claims.sort();
    claims
}

#[test]
fn changed_extension_module_matches_canonical_through_real_incremental_cli() {
    let actual = run(ISSUE_SOURCE);
    let expected = run(&canonical_issue_source());
    assert_eq!(actual, expected, "the cf+cfe CLI route must preserve all independent findings");
    assert!(
        actual.iter().all(|(code, _)| code != "ParseError" && code != "QueryParseError"),
        "the issue constructs must be clean through analyze --incremental: {actual:?}"
    );
}
