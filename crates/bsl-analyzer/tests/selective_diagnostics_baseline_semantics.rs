use std::{fs, process::Command};

const MAIN_MODULE: &str = "БазовыйМодуль";
const EXT_MODULE: &str = "МодульРасширения";
const MISSING_MODULE: &str = "ЗаведомоНетТакогоМодуля";

fn configuration_xml(name: &str, module: &str, extension: bool) -> String {
    let purpose = if extension {
        "<ConfigurationExtensionPurpose>Customization</ConfigurationExtensionPurpose>"
    } else {
        ""
    };
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:v8="http://v8.1c.ru/8.1/data/core">
	<Configuration uuid="11111111-0000-0000-0000-000000000001">
		<Properties><Name>{name}</Name><Synonym/><Comment/><NamePrefix/>{purpose}<DefaultRunMode>ManagedApplication</DefaultRunMode></Properties>
		<ChildObjects><CommonModule>{module}</CommonModule></ChildObjects>
	</Configuration>
</MetaDataObject>"#
    )
}

fn common_module_xml(module: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:v8="http://v8.1c.ru/8.1/data/core">
	<CommonModule uuid="22222222-0000-0000-0000-000000000002">
		<Properties><Name>{module}</Name><Synonym/><Comment/><Global>false</Global><ClientManagedApplication>false</ClientManagedApplication><Server>true</Server><ExternalConnection>false</ExternalConnection><ClientOrdinaryApplication>false</ClientOrdinaryApplication><ServerCall>false</ServerCall><Privileged>false</Privileged><ReturnValuesReuse>DontUse</ReturnValuesReuse></Properties>
	</CommonModule>
</MetaDataObject>"#
    )
}

fn write_configuration(
    root: &std::path::Path,
    relative: &str,
    name: &str,
    module: &str,
    body: &str,
    extension: bool,
) {
    let directory = root.join(relative);
    let module_directory = directory.join("CommonModules").join(module).join("Ext");
    fs::create_dir_all(&module_directory).unwrap();
    fs::write(directory.join("Configuration.xml"), configuration_xml(name, module, extension))
        .unwrap();
    fs::write(
        directory.join("CommonModules").join(format!("{module}.xml")),
        common_module_xml(module),
    )
    .unwrap();
    fs::write(module_directory.join("Module.bsl"), body).unwrap();
}

#[test]
fn selective_semantics_keeps_full_topology_for_unsuppressed_extensions() {
    let temp = tempfile::tempdir().unwrap();
    write_configuration(
        temp.path(),
        "src/cf",
        "ОсновнаяКонфигурация",
        MAIN_MODULE,
        "Функция Экспортируемая() Экспорт\n    Возврат 1;\nКонецФункции\n",
        false,
    );
    write_configuration(
        temp.path(),
        "src/cfe/Ext",
        "Расширение",
        EXT_MODULE,
        &format!(
            "Процедура Вызвать() Экспорт\n    {MAIN_MODULE}.Экспортируемая();\n    {MISSING_MODULE}.НетТакогоМетода();\nКонецПроцедуры\n"
        ),
        true,
    );
    fs::write(temp.path().join("src/cf/Main.bsl"), "Процедура Тест(\n").unwrap();
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
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_bsl-analyzer-app"))
            .current_dir(temp.path())
            .args(args)
            .output()
            .unwrap()
    };
    assert!(run(&["diagnostics", "baseline", "create", "-s", "."]).status.success());

    let output = run(&["analyze", "-s", ".", "--format", "jsonl"]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let events = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    let files = events.iter().filter(|event| event["type"] == "file").collect::<Vec<_>>();
    let main = files.iter().find(|event| event["path"].as_str().unwrap().ends_with("Main.bsl"));
    let ext = files.iter().find(|event| {
        event["path"]
            .as_str()
            .unwrap()
            .replace('\\', "/")
            .ends_with(&format!("CommonModules/{EXT_MODULE}/Ext/Module.bsl"))
    });
    assert!(main.unwrap()["diagnostics"].as_array().unwrap().is_empty());
    let mut unresolved = ext.unwrap()["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|diagnostic| diagnostic["code"] == "UnresolvedMethodCall")
        .filter_map(|diagnostic| diagnostic["message"].as_str()?.split('\'').nth(1))
        .collect::<Vec<_>>();
    unresolved.sort_unstable();
    assert_eq!(unresolved, [MISSING_MODULE]);
    let done = events.iter().find(|event| event["type"] == "done").unwrap();
    assert_eq!(done["baseline"]["selection"], "selective");
    assert!(done["baseline"]["known"].as_u64().unwrap() > 0);
    assert!(done["baseline"]["unsuppressed"].as_u64().unwrap() > 0);
    assert_eq!(done["baseline"]["partitions"].as_array().unwrap().len(), 2);
}
