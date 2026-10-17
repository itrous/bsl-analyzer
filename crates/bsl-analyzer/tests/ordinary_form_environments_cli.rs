//! An ordinary form opens in the thick client of the managed application only when
//! the configuration allows it (`UseOrdinaryFormInManagedApplication`); otherwise
//! the platform never compiles its module there, and `analyze` must not judge it
//! there. A managed form does not depend on the property.

use std::path::Path;
use std::process::Command;

use serde_json::Value;

const CODES: [&str; 2] = ["UnavailableInEnvironment", "ModuleAccessibility"];
const MANAGED_THICK_CLIENT: &str = "Толстый клиент (управляемое приложение)";

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
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

/// An ordinary-application configuration with one server common module and a
/// catalog with an ordinary and a managed form; both forms call the server-only
/// platform function and the server module from client code.
fn project(root: &Path, ordinary_forms_in_managed_app: &str) {
    let cf = root.join("src/cf");
    write(
        &cf.join("Configuration.xml"),
        &format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:v8="http://v8.1c.ru/8.1/data/core">
	<Configuration uuid="11111111-0000-0000-0000-000000000001">
		<Properties><Name>Конфигурация</Name><Synonym/><Comment/><DefaultRunMode>OrdinaryApplication</DefaultRunMode><UseManagedFormInOrdinaryApplication>false</UseManagedFormInOrdinaryApplication><UseOrdinaryFormInManagedApplication>{ordinary_forms_in_managed_app}</UseOrdinaryFormInManagedApplication></Properties>
		<ChildObjects><CommonModule>Серверный</CommonModule><Catalog>Товары</Catalog></ChildObjects>
	</Configuration>
</MetaDataObject>"#
        ),
    );
    write(
        &cf.join("CommonModules/Серверный.xml"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:v8="http://v8.1c.ru/8.1/data/core">
	<CommonModule uuid="22222222-0000-0000-0000-000000000002">
		<Properties><Name>Серверный</Name><Synonym/><Comment/><Global>false</Global><ClientManagedApplication>false</ClientManagedApplication><Server>true</Server><ExternalConnection>false</ExternalConnection><ClientOrdinaryApplication>false</ClientOrdinaryApplication><ServerCall>false</ServerCall><Privileged>false</Privileged><ReturnValuesReuse>DontUse</ReturnValuesReuse></Properties>
	</CommonModule>
</MetaDataObject>"#,
    );
    write(
        &cf.join("CommonModules/Серверный/Ext/Module.bsl"),
        "Процедура Сделать() Экспорт\nКонецПроцедуры\n",
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
         \tДоступен = ОсновнойСерверДоступен();\n\
         \tСерверный.Сделать();\n\
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
         Процедура Проверить()\n\
         \tДоступен = ОсновнойСерверДоступен();\n\
         \tСерверный.Сделать();\n\
         КонецПроцедуры\n",
    );
    write(&root.join("bsl-analyzer.toml"), "[source]\nroot = \"src/cf\"\n");
}

/// `(code, message)` of the environment diagnostics of one module.
type Findings = Vec<(String, String)>;

/// [`Findings`] of the ordinary and the managed form module.
fn analyze(root: &Path) -> (Findings, Findings) {
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
            .filter(|d| d["code"].as_str().is_some_and(|code| CODES.contains(&code)))
            .map(|d| {
                (
                    d["code"].as_str().unwrap().to_owned(),
                    d["message"].as_str().unwrap_or_default().to_owned(),
                )
            })
            .collect()
    };
    (
        found("Товары/Forms/ФормаОбычная/Ext/Form/Module.bsl"),
        found("Товары/Forms/ФормаУправляемая/Ext/Form/Module.bsl"),
    )
}

fn codes(found: &[(String, String)]) -> Vec<&str> {
    let mut codes: Vec<&str> = found.iter().map(|(code, _)| code.as_str()).collect();
    codes.sort_unstable();
    codes
}

/// The managed form reports both calls in every managed client, whatever the property.
fn assert_managed_form_reports(managed: &[(String, String)]) {
    assert_eq!(codes(managed), ["ModuleAccessibility", "UnavailableInEnvironment"], "{managed:?}");
    for (_, message) in managed {
        assert!(message.contains("Тонкий клиент"), "{message}");
        assert!(message.contains(MANAGED_THICK_CLIENT), "{message}");
    }
}

#[test]
fn ordinary_form_is_not_judged_in_the_managed_thick_client_when_it_never_opens_there() {
    let dir = tempfile::tempdir().unwrap();
    project(dir.path(), "false");
    let (ordinary, managed) = analyze(dir.path());
    assert!(
        ordinary.is_empty(),
        "the platform compiles this module only in the ordinary thick client: {ordinary:?}"
    );
    assert_managed_form_reports(&managed);
}

#[test]
fn ordinary_form_is_judged_in_the_managed_thick_client_when_it_opens_there() {
    let dir = tempfile::tempdir().unwrap();
    project(dir.path(), "true");
    let (ordinary, managed) = analyze(dir.path());
    assert_eq!(
        codes(&ordinary),
        ["ModuleAccessibility", "UnavailableInEnvironment"],
        "{ordinary:?}"
    );
    for (_, message) in &ordinary {
        assert!(message.contains(&format!("[{MANAGED_THICK_CLIENT}]")), "{message}");
    }
    assert_managed_form_reports(&managed);
}
