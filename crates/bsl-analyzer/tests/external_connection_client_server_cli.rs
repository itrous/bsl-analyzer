//! With the external connection among the checked environments, object and manager
//! modules are judged there as in the client-server variant: the connection sees
//! only common modules compiled for it (`ВнешнееСоединение`) or called on the server
//! (`ВызовСервера`), and an own method under `&НаСервере` does not exist in it. The
//! default set of checked environments leaves the external connection out, and the
//! verdicts stay as they were.

use std::path::Path;
use std::process::Command;

use serde_json::Value;

const MANAGED_THICK_CLIENT: &str = "Толстый клиент (управляемое приложение)";
const EXTERNAL_CONNECTION: &str = "Внешнее соединение";

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn common_module(cf: &Path, uuid: &str, name: &str, flags: &[&str]) {
    let flag = |key: &str| if flags.contains(&key) { "true" } else { "false" };
    write(
        &cf.join(format!("CommonModules/{name}.xml")),
        &format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:v8="http://v8.1c.ru/8.1/data/core">
	<CommonModule uuid="{uuid}">
		<Properties><Name>{name}</Name><Synonym/><Comment/><Global>false</Global><ClientManagedApplication>{}</ClientManagedApplication><Server>{}</Server><ExternalConnection>{}</ExternalConnection><ClientOrdinaryApplication>false</ClientOrdinaryApplication><ServerCall>{}</ServerCall><Privileged>false</Privileged><ReturnValuesReuse>DontUse</ReturnValuesReuse></Properties>
	</CommonModule>
</MetaDataObject>"#,
            flag("ClientManagedApplication"),
            flag("Server"),
            flag("ExternalConnection"),
            flag("ServerCall"),
        ),
    );
    write(
        &cf.join(format!("CommonModules/{name}/Ext/Module.bsl")),
        "Процедура Сделать() Экспорт\nКонецПроцедуры\n",
    );
}

/// A managed configuration with a report whose object module calls common modules of
/// every kind and own server methods from code without a directive.
fn project(root: &Path, toml: &str) {
    let cf = root.join("src/cf");
    write(
        &cf.join("Configuration.xml"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:v8="http://v8.1c.ru/8.1/data/core">
	<Configuration uuid="11111111-0000-0000-0000-000000000001">
		<Properties><Name>Конфигурация</Name><Synonym/><Comment/><DefaultRunMode>ManagedApplication</DefaultRunMode><UseManagedFormInOrdinaryApplication>false</UseManagedFormInOrdinaryApplication><UseOrdinaryFormInManagedApplication>false</UseOrdinaryFormInManagedApplication></Properties>
		<ChildObjects><CommonModule>Серверный</CommonModule><CommonModule>СерверныйВнешнее</CommonModule><CommonModule>СерверныйВызовСервера</CommonModule><Report>Отчет</Report></ChildObjects>
	</Configuration>
</MetaDataObject>"#,
    );
    common_module(&cf, "22222222-0000-0000-0000-000000000002", "Серверный", &["Server"]);
    common_module(
        &cf,
        "33333333-0000-0000-0000-000000000003",
        "СерверныйВнешнее",
        &["Server", "ExternalConnection"],
    );
    common_module(
        &cf,
        "44444444-0000-0000-0000-000000000004",
        "СерверныйВызовСервера",
        &["Server", "ServerCall"],
    );
    write(
        &cf.join("Reports/Отчет.xml"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:v8="http://v8.1c.ru/8.1/data/core" version="2.20">
	<Report uuid="66666666-0000-0000-0000-000000000006">
		<Properties><Name>Отчет</Name><Synonym/><Comment/></Properties>
		<ChildObjects/>
	</Report>
</MetaDataObject>"#,
    );
    write(
        &cf.join("Reports/Отчет/Ext/ObjectModule.bsl"),
        "Процедура БезДирективы() Экспорт\n\
         \tСерверный.Сделать();\n\
         \tСерверныйВнешнее.Сделать();\n\
         \tСерверныйВызовСервера.Сделать();\n\
         \tСерверная();\n\
         \tБезКонтекста();\n\
         \tОбщая();\n\
         КонецПроцедуры\n\
         \n\
         &НаСервере\n\
         Процедура Серверная() Экспорт\n\
         \tСерверный.Сделать();\n\
         \tБезКонтекста();\n\
         КонецПроцедуры\n\
         \n\
         &НаСервереБезКонтекста\n\
         Процедура БезКонтекста() Экспорт\n\
         КонецПроцедуры\n\
         \n\
         Процедура Общая() Экспорт\n\
         КонецПроцедуры\n",
    );
    write(&root.join("bsl-analyzer.toml"), toml);
}

/// `(line, message)` of the `ModuleAccessibility` findings of the report object module,
/// in line order.
fn module_accessibility(root: &Path) -> Vec<(u64, String)> {
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
    let event = stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .find(|e| {
            e["type"] == "file"
                && e["path"]
                    .as_str()
                    .is_some_and(|p| Path::new(p).ends_with("Reports/Отчет/Ext/ObjectModule.bsl"))
        })
        .expect("the report object module was not analyzed");
    assert_eq!(event["error"], Value::Null, "{event}");
    let mut found: Vec<(u64, String)> = event["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|d| d["code"] == "ModuleAccessibility")
        .map(|d| {
            (
                d["start_line"].as_u64().unwrap_or_else(|| panic!("no start_line: {d}")),
                d["message"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect();
    found.sort();
    found
}

fn messages(found: &[(u64, String)]) -> Vec<&str> {
    found.iter().map(|(_, message)| message.as_str()).collect()
}

#[test]
fn object_module_is_judged_in_the_client_server_external_connection() {
    let dir = tempfile::tempdir().unwrap();
    project(
        dir.path(),
        "[source]\nroot = \"src/cf\"\n\n[features]\nchecked_environments = \
         [\"ТонкийКлиент\", \"ВебКлиент\", \"ТолстыйКлиентУправляемоеПриложение\", \
         \"Сервер\", \"ВнешнееСоединение\"]\n",
    );
    let found = module_accessibility(dir.path());
    assert_eq!(
        messages(&found),
        [
            format!(
                "Модуль 'Серверный' недоступен [{MANAGED_THICK_CLIENT}, {EXTERNAL_CONNECTION}]"
            ),
            format!("Модуль 'СерверныйВнешнее' недоступен [{MANAGED_THICK_CLIENT}]"),
            format!("Метод 'Серверная' недоступен [{MANAGED_THICK_CLIENT}, {EXTERNAL_CONNECTION}]"),
            format!(
                "Метод 'БезКонтекста' недоступен [{MANAGED_THICK_CLIENT}, {EXTERNAL_CONNECTION}]"
            ),
        ],
        "the `ВызовСервера` module is reachable from the external connection, the \
         `ВнешнееСоединение` one is compiled there, and the server method's own calls \
         are fine: {found:?}"
    );
}

#[test]
fn external_connection_left_out_by_default_keeps_the_verdicts() {
    let dir = tempfile::tempdir().unwrap();
    project(dir.path(), "[source]\nroot = \"src/cf\"\n");
    let found = module_accessibility(dir.path());
    assert_eq!(
        messages(&found),
        [
            format!("Модуль 'Серверный' недоступен [{MANAGED_THICK_CLIENT}]"),
            format!("Модуль 'СерверныйВнешнее' недоступен [{MANAGED_THICK_CLIENT}]"),
            format!("Метод 'Серверная' недоступен [{MANAGED_THICK_CLIENT}]"),
            format!("Метод 'БезКонтекста' недоступен [{MANAGED_THICK_CLIENT}]"),
        ],
        "{found:?}"
    );
}
