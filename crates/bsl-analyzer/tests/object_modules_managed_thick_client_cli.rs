//! A configuration run as a managed application compiles the object and manager
//! modules of its objects in the thick client of the managed application too. There
//! that client sees only common modules compiled for the client or called on the
//! server (`ВызовСервера`), and an own method under `&НаСервере` does not exist in it
//! (the client-server variant). An `OrdinaryApplication` configuration is not judged
//! there.

use std::path::Path;
use std::process::Command;

use serde_json::Value;

const MANAGED_THICK_CLIENT: &str = "Толстый клиент (управляемое приложение)";

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
		<Properties><Name>{name}</Name><Synonym/><Comment/><Global>false</Global><ClientManagedApplication>{}</ClientManagedApplication><Server>{}</Server><ExternalConnection>false</ExternalConnection><ClientOrdinaryApplication>false</ClientOrdinaryApplication><ServerCall>{}</ServerCall><Privileged>false</Privileged><ReturnValuesReuse>DontUse</ReturnValuesReuse></Properties>
	</CommonModule>
</MetaDataObject>"#,
            flag("ClientManagedApplication"),
            flag("Server"),
            flag("ServerCall"),
        ),
    );
    write(
        &cf.join(format!("CommonModules/{name}/Ext/Module.bsl")),
        "Процедура Сделать() Экспорт\nКонецПроцедуры\n",
    );
}

/// A configuration with a report whose object module calls common modules of every
/// kind and an own `&НаСервере` method, all from code without a directive.
fn project(root: &Path, run_mode: &str) {
    let cf = root.join("src/cf");
    write(
        &cf.join("Configuration.xml"),
        &format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:v8="http://v8.1c.ru/8.1/data/core">
	<Configuration uuid="11111111-0000-0000-0000-000000000001">
		<Properties><Name>Конфигурация</Name><Synonym/><Comment/><DefaultRunMode>{run_mode}</DefaultRunMode><UseManagedFormInOrdinaryApplication>false</UseManagedFormInOrdinaryApplication><UseOrdinaryFormInManagedApplication>false</UseOrdinaryFormInManagedApplication></Properties>
		<ChildObjects><CommonModule>Серверный</CommonModule><CommonModule>СерверныйВызовСервера</CommonModule><CommonModule>КлиентСервер</CommonModule><CommonModule>Клиентский</CommonModule><Report>Отчет</Report></ChildObjects>
	</Configuration>
</MetaDataObject>"#
        ),
    );
    common_module(&cf, "22222222-0000-0000-0000-000000000002", "Серверный", &["Server"]);
    common_module(
        &cf,
        "33333333-0000-0000-0000-000000000003",
        "СерверныйВызовСервера",
        &["Server", "ServerCall"],
    );
    common_module(
        &cf,
        "44444444-0000-0000-0000-000000000004",
        "КлиентСервер",
        &["Server", "ClientManagedApplication"],
    );
    common_module(
        &cf,
        "55555555-0000-0000-0000-000000000005",
        "Клиентский",
        &["ClientManagedApplication"],
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
         \tСерверныйВызовСервера.Сделать();\n\
         \tКлиентСервер.Сделать();\n\
         \tСерверная();\n\
         \tОбщая();\n\
         \t#Если Клиент Тогда\n\
         \tКлиентский.Сделать();\n\
         \t#КонецЕсли\n\
         КонецПроцедуры\n\
         \n\
         &НаСервере\n\
         Процедура Серверная() Экспорт\n\
         \tСерверный.Сделать();\n\
         \tСерверная();\n\
         КонецПроцедуры\n\
         \n\
         Процедура Общая() Экспорт\n\
         КонецПроцедуры\n",
    );
    write(&root.join("bsl-analyzer.toml"), "[source]\nroot = \"src/cf\"\n");
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

#[test]
fn object_module_is_judged_in_the_managed_thick_client_of_a_managed_configuration() {
    for run_mode in ["ManagedApplication", "Auto"] {
        let dir = tempfile::tempdir().unwrap();
        project(dir.path(), run_mode);
        let found = module_accessibility(dir.path());
        let messages: Vec<&str> = found.iter().map(|(_, message)| message.as_str()).collect();
        assert_eq!(
            messages,
            [
                format!("Модуль 'Серверный' недоступен [{MANAGED_THICK_CLIENT}]"),
                format!("Метод 'Серверная' недоступен [{MANAGED_THICK_CLIENT}]"),
            ],
            "{run_mode}: only the server-only module and the own server method, \
             each from code without a directive: {found:?}"
        );
    }
}

#[test]
fn object_module_of_an_ordinary_configuration_is_not_judged_in_the_managed_thick_client() {
    let dir = tempfile::tempdir().unwrap();
    project(dir.path(), "OrdinaryApplication");
    let found = module_accessibility(dir.path());
    assert!(found.is_empty(), "{found:?}");
}
