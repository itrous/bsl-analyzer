//! `MissingFormEventHandler` end to end: `analyze` over a configuration whose
//! managed common form binds a form event, an item event and a command action,
//! one of each declared in the form module and the rest not.

use std::path::Path;
use std::process::Command;

use serde_json::Value;

const CODE: &str = "MissingFormEventHandler";

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn fixture(root: &Path, module: &str) {
    write(
        &root.join("Configuration.xml"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:v8="http://v8.1c.ru/8.1/data/core">
	<Configuration uuid="11111111-0000-0000-0000-000000000001">
		<Properties><Name>Тест</Name><Synonym/><Comment/><DefaultRunMode>ManagedApplication</DefaultRunMode><CompatibilityMode>Version8_3_17</CompatibilityMode></Properties>
		<ChildObjects><CommonForm>Настройка</CommonForm></ChildObjects>
	</Configuration>
</MetaDataObject>"#,
    );
    write(
        &root.join("CommonForms/Настройка.xml"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.17">
	<CommonForm uuid="22222222-0000-0000-0000-000000000002">
		<Properties><Name>Настройка</Name><FormType>Managed</FormType></Properties>
	</CommonForm>
</MetaDataObject>"#,
    );
    write(
        &root.join("CommonForms/Настройка/Ext/Form.xml"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<Form xmlns="http://v8.1c.ru/8.3/xcf/logform" version="2.17">
	<Events>
		<Event name="OnOpen">ПриОткрытии</Event>
		<Event name="OnCreateAtServer">ПриСозданииНаСервере</Event>
	</Events>
	<ChildItems>
		<InputField name="Поле" id="1">
			<DataPath>Реквизит</DataPath>
			<Events>
				<Event name="OnChange">ПолеПриИзменении</Event>
			</Events>
		</InputField>
	</ChildItems>
	<Attributes>
		<Attribute name="Реквизит" id="1"><Type><v8:Type xmlns:v8="http://v8.1c.ru/8.1/data/core">xs:string</v8:Type></Type></Attribute>
	</Attributes>
	<Commands>
		<Command name="Выполнить" id="1">
			<Action>ВыполнитьКоманду</Action>
		</Command>
	</Commands>
</Form>"#,
    );
    write(&root.join("CommonForms/Настройка/Ext/Form/Module.bsl"), module);
}

/// The handler names [`CODE`] reports for the form module, sorted.
fn missing_handlers(root: &Path) -> Vec<String> {
    let output = Command::new(env!("CARGO_BIN_EXE_bsl-analyzer-app"))
        .arg("analyze")
        .arg("-s")
        .arg(root)
        .args(["--format", "jsonl", "--only-diagnostic", CODE])
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
                    Path::new(p).ends_with("CommonForms/Настройка/Ext/Form/Module.bsl")
                })
        })
        .unwrap_or_else(|| panic!("the form module was not analyzed: {stdout}"));
    event["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|d| d["code"] == CODE)
        .map(|d| {
            let message = d["message"].as_str().unwrap();
            message.split('"').nth(1).unwrap().to_string()
        })
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect()
}

#[test]
fn handlers_bound_in_the_form_but_absent_from_the_module_are_reported() {
    let dir = tempfile::tempdir().unwrap();
    fixture(
        dir.path(),
        "&НаСервере\nПроцедура приСозданииНаСервере(Отказ, СтандартнаяОбработка)\nКонецПроцедуры\n",
    );
    assert_eq!(
        missing_handlers(dir.path()),
        ["ВыполнитьКоманду", "ПолеПриИзменении", "ПриОткрытии"]
    );
}

#[test]
fn a_module_declaring_every_bound_handler_is_silent() {
    let dir = tempfile::tempdir().unwrap();
    fixture(
        dir.path(),
        "&НаКлиенте\nПроцедура ПриОткрытии(Отказ)\nКонецПроцедуры\n\n\
&НаСервере\nПроцедура ПриСозданииНаСервере(Отказ, СтандартнаяОбработка)\nКонецПроцедуры\n\n\
&НаКлиенте\nПроцедура ПолеПриИзменении(Элемент)\nКонецПроцедуры\n\n\
&НаКлиенте\nПроцедура ВыполнитьКоманду(Команда)\nКонецПроцедуры\n",
    );
    assert_eq!(missing_handlers(dir.path()), Vec::<String>::new());
}
