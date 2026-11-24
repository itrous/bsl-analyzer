use super::*;
use base_db::{SourceDatabase, METADATA_SOURCE_ROOT};

struct TestVfs(std::cell::RefCell<vfs::Vfs>);
impl ide_host_core::VfsWrite for TestVfs {
    fn with_write<R>(&self, f: impl FnOnce(&mut vfs::Vfs) -> R) -> R {
        f(&mut self.0.borrow_mut())
    }
}

fn resident(
    properties: &str,
    include_extension: bool,
) -> (ide::RootDatabaseImpl, tempfile::TempDir) {
    let directory = tempfile::tempdir().unwrap();
    let mut db = ide::RootDatabaseImpl::new();
    let base = directory.path().join("base");
    let visible = directory.path().join("visible");
    let hidden = directory.path().join("hidden");
    let mut next = 100;
    for (root, objects, definitions) in [
        (&base, vec![(MdoType::ChartOfCharacteristicTypes, "Properties", properties.to_string())], vec![("Amount", "<v8:Type>primitive:string</v8:Type><v8:StringQualifiers><v8:Length>12</v8:Length></v8:StringQualifiers>"), ("Cycle", "<v8:TypeSet>own:DefinedType.Cycle</v8:TypeSet>"), ("Bundle", "<v8:Type>primitive:boolean</v8:Type><v8:TypeSet>own:DefinedType.Amount</v8:TypeSet>")]),
        (&visible, vec![(MdoType::Catalog, "ExtensionCatalog", String::new())], vec![("Amount", "<v8:Type>primitive:decimal</v8:Type><v8:NumberQualifiers><v8:Digits>15</v8:Digits><v8:FractionDigits>2</v8:FractionDigits></v8:NumberQualifiers>")]),
        (&hidden, vec![(MdoType::Catalog, "HiddenCatalog", String::new())], vec![("OnlyHidden", "<v8:Type>primitive:boolean</v8:Type>")]),
    ] {
        std::fs::create_dir_all(root).unwrap();
        std::fs::write(root.join("Configuration.xml"), xml("Configuration", "Root", "", next)).unwrap(); next += 1;
        for (kind, name, props) in objects {
            let container = match kind { MdoType::Catalog => "Catalogs", MdoType::ChartOfCharacteristicTypes => "ChartsOfCharacteristicTypes", _ => unreachable!() };
            std::fs::create_dir_all(root.join(container)).unwrap();
            std::fs::write(root.join(container).join(format!("{name}.xml")), xml(kind.english_name(), name, &props, next)).unwrap(); next += 1;
        }
        for (name, ty) in definitions {
            std::fs::create_dir_all(root.join("DefinedTypes")).unwrap();
            std::fs::write(root.join("DefinedTypes").join(format!("{name}.xml")), xml("DefinedType", name, &format!("<Type>{ty}</Type>"), next)).unwrap(); next += 1;
        }
    }
    db.set_all_config_paths(
        vec![
            (None, base.clone()),
            (Some("Visible".into()), visible.clone()),
            (Some("Hidden".into()), hidden.clone()),
        ],
        None,
    );
    let vfs = TestVfs(std::cell::RefCell::new(vfs::Vfs::default()));
    ide_host_core::bootstrap_metadata_substrate(&mut db, &vfs);
    let root = db.source_root_input(METADATA_SOURCE_ROOT).root(&db);
    let resident = root
        .iter()
        .map(|id| (id, root.file_set().path_for_file(&id).unwrap().as_path().to_path_buf()))
        .collect::<Vec<_>>();
    for (id, path) in resident {
        db.set_file_text(id, &std::fs::read_to_string(path).unwrap());
    }
    let mut selected = vec![(None, base)];
    if include_extension {
        selected.push((Some("Visible".into()), visible));
    }
    db.set_all_config_paths(selected, None);
    assert!(db.metadata_listing_data(hidden.to_str().unwrap()).is_some());
    (db, directory)
}

fn xml(kind: &str, name: &str, properties: &str, id: u32) -> String {
    format!("<MetaDataObject xmlns='http://v8.1c.ru/8.3/MDClasses' xmlns:v8='http://v8.1c.ru/8.1/data/core' xmlns:primitive='http://www.w3.org/2001/XMLSchema' xmlns:own='http://v8.1c.ru/8.1/data/enterprise/current-config'><{kind} uuid='00000000-0000-0000-0000-{id:012}'><Properties><Name>{name}</Name><Synonym/>{properties}</Properties><ChildObjects/></{kind}></MetaDataObject>")
}

#[test]
fn characteristic_card_keeps_own_compound_type_distinct_from_instance_value_type() {
    let (db, files) = resident("<Type><v8:Type>primitive:boolean</v8:Type><v8:Type>own:CatalogRef.ExtensionCatalog</v8:Type></Type>", true);
    std::fs::write(
        files.path().join("base/ChartsOfCharacteristicTypes/Properties.xml"),
        "not XML after the resident inputs were pinned",
    )
    .unwrap();
    for kind in ["ChartOfCharacteristicTypes", "ПланВидовХарактеристик"] {
        let response =
            bound_source_response(object_from_db(&db, kind, "Properties").unwrap(), 4096).unwrap();
        let body = response.structured_content.as_ref().unwrap();
        assert_eq!(
            body["object"]["read_properties"]["characteristic_value_type"],
            json!({"Composite":{"types":["Boolean",{"Ref":{"mdo_type":"Catalog","name":"ExtensionCatalog"}}]}})
        );
        let instance = body["object"]["attributes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|attribute| attribute["name"] == "ТипЗначения")
            .unwrap();
        assert_ne!(
            instance["attr_type"],
            body["object"]["read_properties"]["characteristic_value_type"]
        );
        assert!(body["completeness"]["reasons"].as_array().unwrap().iter().all(|reason| !reason
            ["path"]
            .as_str()
            .unwrap()
            .contains("characteristic_value_type")));
        let text = &response.content[0].as_text().unwrap().text;
        assert!(
            text.contains("Тип значения характеристик: Булево; Справочник.ExtensionCatalog"),
            "{text}"
        );
    }
}

#[test]
fn characteristic_card_resolves_defined_types_through_selected_resident_roots() {
    for (extension, resolved) in [
        (false, json!({"String":{"length":12}})),
        (true, json!({"Number":{"precision":15,"scale":2}})),
    ] {
        let (db, _files) =
            resident("<Type><v8:TypeSet>own:DefinedType.Amount</v8:TypeSet></Type>", extension);
        let response = object_from_db(&db, "ChartOfCharacteristicTypes", "Properties").unwrap();
        let body = response.structured_content.as_ref().unwrap();
        assert_eq!(
            body["object"]["read_properties"]["characteristic_value_type"],
            json!({"DefinedType":{"name":"Amount"}})
        );
        assert_eq!(body["object"]["resolved_characteristic_value_type"], resolved);
        assert!(body["completeness"]["reasons"].as_array().unwrap().iter().all(|reason| !reason
            ["path"]
            .as_str()
            .unwrap()
            .contains("characteristic_value_type")));
    }
}

#[test]
fn characteristic_card_marks_missing_cyclic_and_invisible_types_unresolved() {
    for (tag, ty) in [
        ("TypeSet", "DefinedType.Missing"),
        ("TypeSet", "DefinedType.Cycle"),
        ("TypeSet", "DefinedType.OnlyHidden"),
        ("Type", "CatalogRef.Missing"),
        ("Type", "CatalogRef.HiddenCatalog"),
    ] {
        let (db, _files) = resident(&format!("<Type><v8:{tag}>own:{ty}</v8:{tag}></Type>"), true);
        let response = object_from_db(&db, "ChartOfCharacteristicTypes", "Properties").unwrap();
        let body = response.structured_content.as_ref().unwrap();
        assert_eq!(body["completeness"]["status"], "partial", "{ty}: {body:#}");
        assert!(!body["object"]["read_properties"]["characteristic_value_type"].is_null());
        assert!(body["completeness"]["reasons"]
            .as_array()
            .unwrap()
            .iter()
            .any(|reason| reason["path"].as_str().is_some_and(
                |path| path.starts_with("/object/resolved_characteristic_value_type")
            )));
    }
}

#[test]
fn characteristic_card_never_substitutes_an_instance_field_for_missing_own_type() {
    let (db, _files) = resident("", true);
    let response = object_from_db(&db, "ChartOfCharacteristicTypes", "Properties").unwrap();
    let body = response.structured_content.as_ref().unwrap();
    assert!(body["object"]["read_properties"]["characteristic_value_type"].is_null());
    assert_eq!(body["completeness"]["status"], "partial");
    assert!(response.content[0]
        .as_text()
        .unwrap()
        .text
        .contains("Тип значения характеристик: unknown"));
}
#[test]
fn characteristic_card_reference_resolution_uses_the_selected_context() {
    for (ty, extension, known) in [
        ("ChartOfCharacteristicTypesRef.Properties", false, true),
        ("CatalogRef.ExtensionCatalog", true, true),
        ("CatalogRef.ExtensionCatalog", false, false),
    ] {
        let (db, _files) =
            resident(&format!("<Type><v8:Type>own:{ty}</v8:Type></Type>"), extension);
        let result = object_from_db(&db, "ChartOfCharacteristicTypes", "Properties").unwrap();
        let body = result.structured_content.as_ref().unwrap();
        let resolved = &body["object"]["resolved_characteristic_value_type"];
        if known {
            assert_eq!(resolved, &body["object"]["read_properties"]["characteristic_value_type"]);
        } else {
            assert_eq!(resolved, &json!("Unknown"));
        }
        let failures = body["completeness"]["reasons"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|reason| {
                reason["path"].as_str().unwrap().contains("resolved_characteristic_value_type")
            })
            .count();
        assert_eq!(failures == 0, known);
        assert_eq!(body["source_roots"].as_array().unwrap().len(), if extension { 2 } else { 1 });
    }
}

#[test]
fn characteristic_expansion_and_combined_wire_output_remain_bounded() {
    let config = bsl_metadata::Configuration::from_xml_str("<MetaDataObject><Configuration><Properties><Name>Types</Name></Properties></Configuration></MetaDataObject>").unwrap();
    let declared = bsl_metadata::AttributeType::Composite {
        types: vec![
            bsl_metadata::AttributeType::Boolean;
            crate::tools::response::DEFAULT_OUTPUT_BUDGET_TOKENS + 1
        ],
    };
    let (resolved, issues) = bsl_metadata::metadata_resolver::resolve_type_description(
        &declared,
        &config,
        crate::tools::response::DEFAULT_OUTPUT_BUDGET_TOKENS,
    );
    assert!(issues.iter().any(|issue| issue.kind
        == bsl_metadata::metadata_resolver::TypeResolutionIssueKind::LimitExceeded));
    let mut object =
        bsl_metadata::MetadataObject::new(MdoType::ChartOfCharacteristicTypes, "Types");
    object.read_properties.characteristic_value_type = Some(declared);
    let response = characteristic_type_response(&object, &config, None).unwrap();
    assert!(
        response.structured_content.as_ref().unwrap()["object"]
            ["resolved_characteristic_value_type"]
            == json!(resolved)
    );
    let bounded = bound_source_response(response, 512).unwrap();
    assert!(live_result_payload_bytes(&bounded) <= 2048);
    assert_eq!(bounded.structured_content.unwrap()["truncated"], true);
}

#[test]
fn characteristic_card_expands_a_defined_compound_without_replacing_its_declaration() {
    let (db, _files) =
        resident("<Type><v8:TypeSet>own:DefinedType.Bundle</v8:TypeSet></Type>", true);
    let result = object_from_db(&db, "ChartOfCharacteristicTypes", "Properties").unwrap();
    let body = result.structured_content.as_ref().unwrap();
    assert_eq!(
        body["object"]["read_properties"]["characteristic_value_type"],
        json!({"DefinedType":{"name":"Bundle"}})
    );
    assert_eq!(
        body["object"]["resolved_characteristic_value_type"],
        json!({"Composite":{"types":["Boolean",{"Number":{"precision":15,"scale":2}}]}})
    );
    assert_eq!(body["completeness"]["status"], "complete");
    assert!(result.content[0].as_text().unwrap().text.contains("Bundle"));
}
