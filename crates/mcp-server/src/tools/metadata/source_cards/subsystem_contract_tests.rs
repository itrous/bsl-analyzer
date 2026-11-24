use super::*;
use base_db::{SourceDatabase, METADATA_SOURCE_ROOT};

struct TestVfs(std::cell::RefCell<vfs::Vfs>);
impl ide_host_core::VfsWrite for TestVfs {
    fn with_write<R>(&self, f: impl FnOnce(&mut vfs::Vfs) -> R) -> R {
        f(&mut self.0.borrow_mut())
    }
}

fn xml(kind: &str, name: &str, id: u32, properties: &str, children: &str) -> String {
    format!("<MetaDataObject xmlns:v8='http://v8.1c.ru/8.1/data/core' xmlns:xs='http://www.w3.org/2001/XMLSchema'><{kind} uuid='00000000-0000-0000-0000-{id:012}'><Properties><Name>{name}</Name><Synonym/>{properties}</Properties><ChildObjects>{children}</ChildObjects></{kind}></MetaDataObject>")
}

fn resident(
    matching_adoption: bool,
    include_extension: bool,
) -> (ide::RootDatabaseImpl, tempfile::TempDir) {
    let directory = tempfile::tempdir().unwrap();
    let base = directory.path().join("base");
    let extension = directory.path().join("extension");
    let adopted = |id| {
        format!("<ObjectBelonging>Adopted</ObjectBelonging><ExtendedConfigurationObject>00000000-0000-0000-0000-{:012}</ExtendedConfigurationObject>", if matching_adoption { id } else { 999 })
    };
    for (root, path, body) in [
        (&base, "Configuration.xml", xml("Configuration", "Base", 1, "", "")),
        (&extension, "Configuration.xml", xml("Configuration", "Extension", 2, "", "")),
        (&base, "Subsystems/Operations.xml", include_str!("fixtures/subsystem/base-operations.xml").into()),
        (&base, "Subsystems/Operations/Subsystems/Rules.xml", xml("Subsystem", "Rules", 12, "<ObjectBelonging>Own</ObjectBelonging><Content><Item>Document.Sale</Item></Content><Picture>CommonPicture.BasePicture</Picture>", "")),
        (&extension, "Subsystems/Operations.xml", include_str!("fixtures/subsystem/extension-operations.xml").replace("00000000-0000-0000-0000-000000000011", if matching_adoption { "00000000-0000-0000-0000-000000000011" } else { "00000000-0000-0000-0000-000000000999" })),
        (&extension, "Subsystems/Operations/Subsystems/Rules.xml", xml("Subsystem", "Rules", 22, &format!("{}<Content><Item>CommonCommand.Open</Item></Content><Picture/>", adopted(12)), "")),
        (&extension, "Subsystems/Operations/Subsystems/ExtensionTools.xml", xml("Subsystem", "ExtensionTools", 23, "<ObjectBelonging>Own</ObjectBelonging><Content/><Picture>CommonPicture.ExtensionPicture</Picture>", "")),
        (&base, "Subsystems/Other.xml", xml("Subsystem", "Other", 13, "<ObjectBelonging>Own</ObjectBelonging><Content/><Picture/>", "<Subsystem>Rules</Subsystem>")),
        (&base, "Subsystems/Other/Subsystems/Rules.xml", xml("Subsystem", "Rules", 14, "<ObjectBelonging>Own</ObjectBelonging><Content/><Picture/>", "")),
        (&base, "Subsystems/Unknown.xml", xml("Subsystem", "Unknown", 15, "<ObjectBelonging>Own</ObjectBelonging>", "")),
        (&base, "CommonCommands/Open.xml", xml("CommonCommand", "Open", 31, "<ObjectBelonging>Own</ObjectBelonging><Group>NavigationPanelOrdinary</Group><Picture>CommonPicture.BasePicture</Picture><CommandParameterType><v8:Type>xs:boolean</v8:Type></CommandParameterType><ModifiesData>false</ModifiesData>", "")),
        (&extension, "CommonCommands/Open.xml", xml("CommonCommand", "Open", 41, &format!("{}<Picture>CommonPicture.ExtensionPicture</Picture>", adopted(31)), "")),
        (&extension, "CommonCommands/ExtensionCommand.xml", xml("CommonCommand", "ExtensionCommand", 42, "<ObjectBelonging>Own</ObjectBelonging><Group/><Picture/><CommandParameterType/><ModifiesData>true</ModifiesData>", "")),
        (&base, "CommonPictures/BasePicture.xml", xml("CommonPicture", "BasePicture", 51, "<ObjectBelonging>Own</ObjectBelonging>", "")),
        (&extension, "CommonPictures/ExtensionPicture.xml", xml("CommonPicture", "ExtensionPicture", 52, "<ObjectBelonging>Own</ObjectBelonging>", "")),
    ] {
        let file = root.join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, body).unwrap();
    }
    let mut db = ide::RootDatabaseImpl::new();
    db.set_all_config_paths(
        vec![(None, base.clone()), (Some("Extension".into()), extension.clone())],
        None,
    );
    pin(&mut db);
    if !include_extension {
        db.set_all_config_paths(vec![(None, base)], None);
    }
    (db, directory)
}

fn pin(db: &mut ide::RootDatabaseImpl) {
    let vfs = TestVfs(std::cell::RefCell::new(vfs::Vfs::default()));
    ide_host_core::bootstrap_metadata_substrate(db, &vfs);
    let source = db.source_root_input(METADATA_SOURCE_ROOT).root(db);
    let files = source
        .iter()
        .map(|id| (id, source.file_set().path_for_file(&id).unwrap().as_path().to_path_buf()))
        .collect::<Vec<_>>();
    for (id, path) in files {
        db.set_file_text(id, &std::fs::read_to_string(path).unwrap());
    }
}

fn text(response: &CallToolResult) -> &str {
    &response.content[0].as_text().unwrap().text
}

#[test]
fn subsystem_contract_nested_resident_cards_preserve_parent_and_uuid_attested_content() {
    let (db, directory) = resident(true, true);
    std::fs::write(
        directory.path().join("extension/Subsystems/Operations.xml"),
        "invalid after resident pin",
    )
    .unwrap();
    for kind in ["Subsystem", "Подсистема"] {
        let response = object_from_db(&db, kind, "Operations").unwrap();
        let body = response.structured_content.unwrap();
        assert_eq!(
            body["object"]["content"],
            json!(["Catalog.Products", "CommonCommand.Open", "Document.ExtensionSale"])
        );
        assert_eq!(body["object"]["child_subsystems"], json!(["Rules", "ExtensionTools"]));
        assert_eq!(body["object"]["picture"], "CommonPicture.ExtensionPicture");
        assert_eq!(body["object"]["uuid"], "00000000-0000-0000-0000-000000000011");
        assert_eq!(body["object"]["object_belonging"], "Own");
        assert_eq!(
            body["object"]["source_roots"],
            json!([directory.path().join("base"), directory.path().join("extension")])
        );
        assert_eq!(body["completeness"]["status"], "complete");
    }
    let nested = object_from_db(&db, "Subsystem", "Operations.Subsystem.Rules")
        .unwrap()
        .structured_content
        .unwrap();
    assert_eq!(nested["object"]["parent"], "Operations");
    assert_eq!(nested["object"]["content"], json!(["Document.Sale", "CommonCommand.Open"]));
    assert_eq!(nested["object"]["picture"], "");
    let other = object_from_db(&db, "Subsystem", "Other.Subsystem.Rules")
        .unwrap()
        .structured_content
        .unwrap();
    assert_eq!(other["object"]["parent"], "Other");
    assert_eq!(other["object"]["content"], json!([]));
    assert!(object_from_db(&db, "Subsystem", "Rules").is_err());
}

#[test]
fn subsystem_contract_tree_opens_extension_owned_subsystems_commands_and_pictures() {
    let (db, directory) = resident(true, true);
    let (config, extensions) = configs_from_db(&db);
    for (filters, kind, name) in [
        (["Subsystems", "Подсистемы"], "Subsystem", "Operations.Subsystem.ExtensionTools"),
        (["CommonCommands", "ОбщиеКоманды"], "CommonCommand", "ExtensionCommand"),
        (["CommonPictures", "ОбщиеКартинки"], "CommonPicture", "ExtensionPicture"),
    ] {
        for filter in filters {
            let tree =
                get_metadata_tree(&db, &config, &extensions, Some(filter.into()), 6000).unwrap();
            assert!(text(&tree).contains(name), "{filter}: {}", text(&tree));
            assert!(text(&tree).contains("Extension"));
            assert!(text(&tree).contains("ObjectBelonging=Own"));
            if kind == "Subsystem" {
                assert!(text(&tree).contains("ObjectBelonging=Adopted"));
                assert!(text(&tree).contains("00000000-0000-0000-0000-000000000011"));
            }
            let card = object_from_db(&db, kind, name).unwrap();
            let body = card.structured_content.unwrap();
            assert_eq!(body["object"]["name"], name);
            assert_eq!(body["object"]["object_belonging"], "Own");
            let roots = if kind == "Subsystem" {
                &body["object"]["source_roots"]
            } else {
                &body["source_roots"]
            };
            assert_eq!(roots.as_array().unwrap().len(), 1);
            assert_eq!(
                std::path::Path::new(roots[0].as_str().unwrap()).canonicalize().unwrap(),
                directory.path().join("extension").canonicalize().unwrap()
            );
        }
    }
}

#[test]
fn subsystem_contract_common_command_overlay_retains_known_base_properties() {
    let (db, directory) = resident(true, true);
    for kind in ["CommonCommand", "ОбщаяКоманда"] {
        let response = object_from_db(&db, kind, "Open").unwrap();
        assert!(text(&response).contains("CommonPicture.ExtensionPicture"));
        let body = response.structured_content.unwrap();
        assert_eq!(body["object"]["uuid"], "00000000-0000-0000-0000-000000000031");
        assert_eq!(body["object"]["group"], "NavigationPanelOrdinary");
        assert_eq!(body["object"]["parameter_type"], "Boolean");
        assert_eq!(body["object"]["modifies_data"], false);
        assert_eq!(
            body["source_roots"],
            json!([directory.path().join("base"), directory.path().join("extension")])
        );
        assert_eq!(body["completeness"]["status"], "complete");
    }
}

#[test]
fn subsystem_contract_unproved_adoption_is_partial_without_invented_base_content() {
    let (db, _directory) = resident(false, true);
    for (kind, name) in [("Subsystem", "Operations"), ("CommonCommand", "Open")] {
        let card = object_from_db(&db, kind, name).unwrap();
        assert!(text(&card).contains("partial"));
        let body = card.structured_content.unwrap();
        assert_eq!(body["object"]["object_belonging"], "Adopted");
        assert_eq!(body["completeness"]["status"], "partial");
        assert!(
            body["completeness"]["reasons"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r["code"] == "unresolved_adoption"
                    && r["path"] == "/object/extended_configuration_object"),
            "{body:#}"
        );
        if kind == "Subsystem" {
            assert_eq!(body["object"]["content"], json!(["Document.ExtensionSale"]));
        }
    }
}

#[test]
fn subsystem_contract_missing_properties_and_picture_bytes_are_explicitly_partial() {
    let (db, _directory) = resident(true, true);
    for (kind, name, paths) in [
        ("Subsystem", "Unknown", vec!["/object/content", "/object/picture"]),
        ("CommonPicture", "BasePicture", vec!["/object/picture"]),
    ] {
        let card = object_from_db(&db, kind, name).unwrap();
        let body = card.structured_content.unwrap();
        assert_eq!(body["completeness"]["status"], "partial");
        for path in paths {
            assert!(
                body["completeness"]["reasons"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|r| r["path"] == path),
                "{body:#}"
            );
        }
    }
    assert!(object_from_db(&db, "Subsystem", "Missing").is_err());
    assert!(object_from_db(&db, "CommonCommand", "Missing").is_err());
    assert!(object_from_db(&db, "CommonPicture", "Missing").is_err());
}

#[test]
fn subsystem_contract_selected_roots_and_combined_output_budget_are_respected() {
    let (db, _directory) = resident(true, false);
    assert!(object_from_db(&db, "Subsystem", "Operations.Subsystem.ExtensionTools").is_err());
    assert!(object_from_db(&db, "CommonCommand", "ExtensionCommand").is_err());
    let card = object_from_db(&db, "Subsystem", "Operations").unwrap();
    assert_eq!(
        card.structured_content.as_ref().unwrap()["object"]["source_roots"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let mut oversized = card;
    oversized.structured_content.as_mut().unwrap()["object"]["content"] =
        json!(["x".repeat(12000)]);
    let bounded = bound_source_response(oversized, 512).unwrap();
    assert!(live_result_payload_bytes(&bounded) <= 2048);
    assert_eq!(bounded.structured_content.unwrap()["truncated"], true);
    let (base, extensions) = configs_from_db(&db);
    let tree = get_metadata_tree(&db, &base, &extensions, Some("Subsystems".into()), 512).unwrap();
    assert!(text(&tree).len() <= 2048);
    let (db, _directory) = resident(true, true);
    let (base, mut extensions) = configs_from_db(&db);
    for i in 0..200 {
        extensions[0].1.add_subsystem(bsl_metadata::Subsystem::new(format!("ExtensionMember{i}")));
    }
    let tree = get_metadata_tree(&db, &base, &extensions, Some("Subsystems".into()), 512).unwrap();
    assert!(text(&tree).len() <= 2048);
    assert!(text(&tree).contains("усеч"));
}

#[test]
fn subsystem_contract_conflicting_own_declarations_do_not_claim_complete_visibility() {
    let (mut db, _directory) = resident(true, true);
    let source = db.source_root_input(METADATA_SOURCE_ROOT).root(&db);
    let changed = source
        .iter()
        .filter_map(|id| {
            let path = source.file_set().path_for_file(&id)?.as_path().to_path_buf();
            (path.ends_with("extension/Subsystems/Operations.xml")
                || path.ends_with("extension/CommonCommands/Open.xml"))
            .then_some((id, path))
        })
        .collect::<Vec<_>>();
    assert_eq!(changed.len(), 2);
    for (id, path) in changed {
        let xml = std::fs::read_to_string(&path).unwrap().replace(
            "<ObjectBelonging>Adopted</ObjectBelonging>",
            "<ObjectBelonging>Own</ObjectBelonging>",
        );
        std::fs::write(path, &xml).unwrap();
        db.set_file_text(id, &xml);
    }
    for (kind, name) in [("Subsystem", "Operations"), ("CommonCommand", "Open")] {
        let card = object_from_db(&db, kind, name).unwrap();
        assert!(text(&card).contains("partial"));
        let body = card.structured_content.unwrap();
        assert_eq!(body["completeness"]["status"], "partial");
        assert!(
            body["completeness"]["reasons"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r["code"] == "source_resolution_incomplete"),
            "{body:#}"
        );
    }
}

#[test]
fn subsystem_contract_uncertainty_survives_serialization_and_further_adoption() {
    let (mut db, directory) = resident(false, true);
    let subsystem = db.resolve_subsystem_across_roots("Operations").unwrap();
    let cached: bsl_metadata::Subsystem =
        serde_json::from_value(serde_json::to_value(&*subsystem).unwrap()).unwrap();
    assert!(cached.resolution_incomplete);
    let (command, _) = db
        .resolve_read_object_across_roots(
            bsl_metadata::read_object::ReadObjectKind::CommonCommand,
            "Open",
        )
        .unwrap();
    let cached: bsl_metadata::read_object::ReadObject =
        serde_json::from_value(serde_json::to_value(&*command).unwrap()).unwrap();
    assert!(cached.resolution_incomplete);
    let followup = directory.path().join("followup");
    for (path, body) in [
        ("Configuration.xml", xml("Configuration", "Followup", 3, "", "")),
        ("Subsystems/Operations.xml", xml("Subsystem", "Operations", 61, "<ObjectBelonging>Adopted</ObjectBelonging><ExtendedConfigurationObject>00000000-0000-0000-0000-000000000021</ExtendedConfigurationObject><Content><Item>CommonCommand.Followup</Item></Content><Picture/>", "")),
        ("CommonCommands/Open.xml", xml("CommonCommand", "Open", 62, "<ObjectBelonging>Adopted</ObjectBelonging><ExtendedConfigurationObject>00000000-0000-0000-0000-000000000041</ExtendedConfigurationObject><Group/><CommandParameterType/><ModifiesData>false</ModifiesData>", "")),
    ] {
        let file = followup.join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, body).unwrap();
    }
    db.set_all_config_paths(
        vec![
            (None, directory.path().join("base")),
            (Some("Extension".into()), directory.path().join("extension")),
            (Some("Followup".into()), followup),
        ],
        None,
    );
    pin(&mut db);
    for (kind, name) in [("Subsystem", "Operations"), ("CommonCommand", "Open")] {
        let response = object_from_db(&db, kind, name).unwrap();
        let body = response.structured_content.unwrap();
        assert_eq!(body["completeness"]["status"], "partial");
        assert!(
            body["completeness"]["reasons"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r["code"] == "source_resolution_incomplete"),
            "{body:#}"
        );
        let origins = if kind == "Subsystem" {
            &body["object"]["source_roots"]
        } else {
            &body["source_roots"]
        };
        assert_eq!(origins.as_array().unwrap().len(), 3);
    }
}
