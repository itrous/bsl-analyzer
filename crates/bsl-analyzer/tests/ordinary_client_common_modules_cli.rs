//! A common module flagged only «Клиент (обычное приложение)» exists in the thick
//! client of the ordinary application and nowhere else. That is a fact of its
//! metadata, not of the analyzer settings: a call to it from code compiled in another
//! environment (an ordinary form opened in the managed thick client, a managed form)
//! fails there, and `analyze` must report it even though the ordinary thick client
//! itself is not checked by default.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

use serde_json::Value;

const THIN_CLIENT: &str = "Тонкий клиент";
const WEB_CLIENT: &str = "Веб-клиент";
const MANAGED_THICK_CLIENT: &str = "Толстый клиент (управляемое приложение)";
const SERVER: &str = "Сервер";

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn common_module(uuid: &str, name: &str, managed_client: bool) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:v8="http://v8.1c.ru/8.1/data/core">
	<CommonModule uuid="{uuid}">
		<Properties><Name>{name}</Name><Synonym/><Comment/><Global>false</Global><ClientManagedApplication>{managed_client}</ClientManagedApplication><Server>false</Server><ExternalConnection>false</ExternalConnection><ClientOrdinaryApplication>true</ClientOrdinaryApplication><ServerCall>false</ServerCall><Privileged>false</Privileged><ReturnValuesReuse>DontUse</ReturnValuesReuse></Properties>
	</CommonModule>
</MetaDataObject>"#
    )
}

fn form_descriptor(uuid: &str, name: &str, form_type: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:v8="http://v8.1c.ru/8.1/data/core" version="2.20">
	<Form uuid="{uuid}">
		<Properties><Name>{name}</Name><Synonym/><Comment/><FormType>{form_type}</FormType></Properties>
	</Form>
</MetaDataObject>"#
    )
}

/// A configuration with three common modules flagged «Клиент (обычное приложение)»:
/// `ОбычныйКлиент` and `Вызывающий` with that flag alone, `ОбщийКлиент` also for the
/// managed application. `Вызывающий`, an ordinary form and a managed form (client and
/// server procedures) call the first two.
fn project(root: &Path, run_mode: &str, ordinary_forms_in_managed_app: &str) {
    let cf = root.join("src/cf");
    write(
        &cf.join("Configuration.xml"),
        &format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:v8="http://v8.1c.ru/8.1/data/core">
	<Configuration uuid="11111111-0000-0000-0000-000000000001">
		<Properties><Name>Конфигурация</Name><Synonym/><Comment/><DefaultRunMode>{run_mode}</DefaultRunMode><UseManagedFormInOrdinaryApplication>false</UseManagedFormInOrdinaryApplication><UseOrdinaryFormInManagedApplication>{ordinary_forms_in_managed_app}</UseOrdinaryFormInManagedApplication></Properties>
		<ChildObjects><CommonModule>ОбычныйКлиент</CommonModule><CommonModule>ОбщийКлиент</CommonModule><CommonModule>Вызывающий</CommonModule><Catalog>Товары</Catalog></ChildObjects>
	</Configuration>
</MetaDataObject>"#
        ),
    );
    let modules = cf.join("CommonModules");
    for (uuid, name, managed_client) in [
        ("22222222-0000-0000-0000-000000000002", "ОбычныйКлиент", false),
        ("22222222-0000-0000-0000-000000000003", "ОбщийКлиент", true),
        ("22222222-0000-0000-0000-000000000004", "Вызывающий", false),
    ] {
        write(&modules.join(format!("{name}.xml")), &common_module(uuid, name, managed_client));
    }
    let export = "Процедура Показать() Экспорт\nКонецПроцедуры\n";
    write(&modules.join("ОбычныйКлиент/Ext/Module.bsl"), export);
    write(&modules.join("ОбщийКлиент/Ext/Module.bsl"), export);
    write(
        &modules.join("Вызывающий/Ext/Module.bsl"),
        "Процедура Вызвать() Экспорт\n\
         \tОбычныйКлиент.Показать();\n\
         \tОбщийКлиент.Показать();\n\
         КонецПроцедуры\n",
    );
    write(
        &cf.join("Catalogs/Товары.xml"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:v8="http://v8.1c.ru/8.1/data/core" version="2.20">
	<Catalog uuid="33333333-0000-0000-0000-000000000003">
		<Properties><Name>Товары</Name><Synonym/><Comment/></Properties>
		<ChildObjects><Form>ФормаОбычная</Form><Form>ФормаУправляемая</Form></ChildObjects>
	</Catalog>
</MetaDataObject>"#,
    );
    let forms = cf.join("Catalogs/Товары/Forms");
    write(
        &forms.join("ФормаОбычная.xml"),
        &form_descriptor("44444444-0000-0000-0000-000000000004", "ФормаОбычная", "Ordinary"),
    );
    write(
        &forms.join("ФормаОбычная/Ext/Form/Module.bsl"),
        "Процедура ПриОткрытии()\n\
         \tОбычныйКлиент.Показать();\n\
         \tОбщийКлиент.Показать();\n\
         КонецПроцедуры\n",
    );
    write(
        &forms.join("ФормаУправляемая.xml"),
        &form_descriptor("55555555-0000-0000-0000-000000000005", "ФормаУправляемая", "Managed"),
    );
    write(
        &forms.join("ФормаУправляемая/Ext/Form.xml"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<Form xmlns="http://v8.1c.ru/8.3/xcf/logform" xmlns:v8="http://v8.1c.ru/8.1/data/core" version="2.20">
	<AutoCommandBar name="ФормаКоманднаяПанель" id="-1"/>
	<Attributes/>
</Form>"#,
    );
    write(
        &forms.join("ФормаУправляемая/Ext/Form/Module.bsl"),
        "&НаКлиенте\n\
         Процедура НаКлиенте()\n\
         \tОбычныйКлиент.Показать();\n\
         \tОбщийКлиент.Показать();\n\
         КонецПроцедуры\n\
         \n\
         &НаСервере\n\
         Процедура НаСервере()\n\
         \tОбычныйКлиент.Показать();\n\
         КонецПроцедуры\n",
    );
    write(&root.join("bsl-analyzer.toml"), "[source]\nroot = \"src/cf\"\n");
}

/// `ModuleAccessibility` of one module: `(line, module, environments)`, lines 0-based.
type Findings = BTreeSet<(u64, String, BTreeSet<String>)>;

struct Analysis {
    ordinary_form: Findings,
    managed_form: Findings,
    caller_module: Findings,
}

fn parse(message: &str) -> (String, BTreeSet<String>) {
    let rest = message.strip_prefix("Модуль '").unwrap_or_else(|| panic!("{message}"));
    let (module, rest) = rest.split_once("' недоступен [").unwrap_or_else(|| panic!("{message}"));
    let envs = rest.strip_suffix(']').unwrap_or_else(|| panic!("{message}"));
    (module.to_owned(), envs.split(", ").map(str::to_owned).collect())
}

fn analyze(root: &Path) -> Analysis {
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
    let files: Vec<Value> = stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .filter(|e| e["type"] == "file")
        .collect();
    let found = |tail: &str| -> Findings {
        let event = files
            .iter()
            .find(|e| e["path"].as_str().is_some_and(|p| Path::new(p).ends_with(tail)))
            .unwrap_or_else(|| panic!("{tail} was not analyzed; files: {files:?}"));
        assert_eq!(event["error"], Value::Null, "{event}");
        event["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|d| d["code"] == "ModuleAccessibility")
            .map(|d| {
                let (module, envs) = parse(d["message"].as_str().unwrap());
                (d["start_line"].as_u64().unwrap_or_else(|| panic!("{d}")), module, envs)
            })
            .collect()
    };
    Analysis {
        ordinary_form: found("Товары/Forms/ФормаОбычная/Ext/Form/Module.bsl"),
        managed_form: found("Товары/Forms/ФормаУправляемая/Ext/Form/Module.bsl"),
        caller_module: found("CommonModules/Вызывающий/Ext/Module.bsl"),
    }
}

fn finding(line: u64, module: &str, envs: &[&str]) -> (u64, String, BTreeSet<String>) {
    (line, module.to_owned(), envs.iter().map(|e| (*e).to_owned()).collect())
}

/// A managed form reaches the ordinary-client-only module from nowhere: neither from
/// a managed client nor from the server. A module flagged for both applications is
/// reachable from its client code.
fn assert_managed_form_reports(found: &Findings) {
    let expected: Findings = [
        finding(2, "ОбычныйКлиент", &[THIN_CLIENT, WEB_CLIENT, MANAGED_THICK_CLIENT]),
        finding(8, "ОбычныйКлиент", &[SERVER]),
    ]
    .into_iter()
    .collect();
    assert_eq!(found, &expected);
}

#[test]
fn ordinary_form_opened_in_the_managed_thick_client_cannot_call_an_ordinary_client_module() {
    let dir = tempfile::tempdir().unwrap();
    project(dir.path(), "ManagedApplication", "true");
    let found = analyze(dir.path());
    let expected: Findings =
        [finding(1, "ОбычныйКлиент", &[MANAGED_THICK_CLIENT])].into_iter().collect();
    assert_eq!(found.ordinary_form, expected);
    assert_managed_form_reports(&found.managed_form);
    assert!(found.caller_module.is_empty(), "{:?}", found.caller_module);
}

#[test]
fn ordinary_form_that_never_opens_in_the_managed_thick_client_is_not_judged_there() {
    let dir = tempfile::tempdir().unwrap();
    project(dir.path(), "ManagedApplication", "false");
    let found = analyze(dir.path());
    assert!(found.ordinary_form.is_empty(), "{:?}", found.ordinary_form);
    assert_managed_form_reports(&found.managed_form);
    assert!(found.caller_module.is_empty(), "{:?}", found.caller_module);
}

#[test]
fn ordinary_application_configuration_keeps_its_ordinary_forms_silent() {
    let dir = tempfile::tempdir().unwrap();
    project(dir.path(), "OrdinaryApplication", "false");
    let found = analyze(dir.path());
    assert!(found.ordinary_form.is_empty(), "{:?}", found.ordinary_form);
    assert!(found.caller_module.is_empty(), "{:?}", found.caller_module);
}
