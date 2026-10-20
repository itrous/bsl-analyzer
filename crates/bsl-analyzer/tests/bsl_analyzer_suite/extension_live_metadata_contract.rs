use std::fs;
use std::path::PathBuf;

use serde_json::Value;

fn extension_file(path: &str) -> String {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../extension/src");
    fs::read_to_string(root.join(path)).unwrap()
}

#[test]
fn extension_exposes_live_metadata_endpoints() {
    let service = extension_file("HTTPServices/BSLAnalyzerService.xml");
    let module = extension_file("HTTPServices/BSLAnalyzerService/Ext/Module.bsl");
    for (template, handler) in [
        ("/metadata-list", "СписокМетаданныхPOST"),
        ("/metadata-structure", "СтруктураМетаданныхPOST"),
    ] {
        assert!(service.contains(template), "missing endpoint {template}");
        assert!(service.contains(handler), "missing handler binding {handler}");
        assert!(module.contains(&format!("Функция {handler}(")), "missing handler {handler}");
    }
}

#[test]
fn arbitrary_bsl_requires_separate_role() {
    let basic = extension_file("Roles/BSL_ОсновнаяРоль/Ext/Rights.xml");
    let execute = extension_file("Roles/BSL_ВыполнениеКода/Ext/Rights.xml");
    for method in ["Выполнить", "Вычислить"] {
        assert!(!basic.contains(&format!("URLTemplate.{method}.Method.POST")));
        assert!(execute.contains(&format!("URLTemplate.{method}.Method.POST")));
    }
}

#[test]
fn metadata_structure_exposes_locale_independent_type_variants() {
    let module = extension_file("HTTPServices/BSLAnalyzerService/Ext/Module.bsl");
    assert!(parser::parse(&module).errors().is_empty(), "invalid BSL syntax");
    for required in [
        "Метаданные.НайтиПоТипу(ТипВарианта)",
        "ТипЗнч(Значение)",
        "Элемент.Тип.Типы()",
        "Описание.Вставить(\"type\", Строка(Элемент.Тип))",
        "Описание.Вставить(\"typeVariants\", ВариантыТипа)",
        "Описание.Вставить(\"technicalName\", ТехническоеИмя)",
        "Описание.Вставить(\"presentation\", Строка(ТипВарианта))",
    ] {
        assert!(module.contains(required), "missing producer contract: {required}");
    }
    assert!(!module.contains("ТехническоеИмя = Строка(ТипВарианта)"));

    let fixture: Value =
        serde_json::from_str(include_str!("../fixtures/live_metadata_type_variants.json")).unwrap();
    let attributes = |locale| fixture[locale]["Реквизиты"].as_array().unwrap();
    let find = |locale, name| attributes(locale).iter().find(|item| item["name"] == name).unwrap();

    for name in [
        "Primitive",
        "Platform",
        "Applied",
        "ReportApplied",
        "Composite",
        "SamePresentation",
        "Unsupported",
    ] {
        let ru = find("ru", name);
        let en = find("en", name);
        assert!(ru["type"].as_str().is_some_and(|value| !value.is_empty()));
        assert!(en["type"].as_str().is_some_and(|value| !value.is_empty()));
        assert_ne!(ru["type"], en["type"]);
        let technical_names = |item: &Value| {
            item["typeVariants"]
                .as_array()
                .unwrap()
                .iter()
                .map(|variant| variant["technicalName"].clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(technical_names(ru), technical_names(en));
    }

    assert_eq!(find("ru", "Primitive")["typeVariants"][0]["technicalName"], "Строка");
    assert_eq!(
        find("ru", "Platform")["typeVariants"][0]["technicalName"],
        "УникальныйИдентификатор"
    );
    assert_eq!(
        find("ru", "Applied")["typeVariants"][0]["technicalName"],
        "СправочникСсылка.Товары"
    );
    assert_eq!(
        find("ru", "ReportApplied")["typeVariants"][0]["technicalName"],
        "ОтчетОбъект.Продажи"
    );
    assert!(module.contains("Метаданные.Отчеты, \"ОтчетОбъект\""));
    assert!(!module.contains("Метаданные.Отчеты, \"ОтчётОбъект\""));
    assert_eq!(find("ru", "Composite")["typeVariants"].as_array().unwrap().len(), 2);
    let same = find("ru", "SamePresentation")["typeVariants"].as_array().unwrap();
    assert_eq!(same.len(), 2);
    assert_eq!(same[0]["presentation"], same[1]["presentation"]);
    assert_ne!(same[0]["technicalName"], same[1]["technicalName"]);
    assert!(find("ru", "Unsupported")["typeVariants"][0]["technicalName"].is_null());
}
