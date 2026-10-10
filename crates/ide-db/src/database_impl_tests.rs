use std::sync::Arc;

use base_db::{RootQueryDb, SourceDatabase, SourceRoot, SourceRootId};
use hir::{DefDatabase, ModuleId};
use vfs::FileId;
use vfs::{file_set::FileSet, VfsPath};

use super::RootDatabaseImpl;
use crate::metadata::{MetadataListingData, RootKind};
use crate::RootDatabase;

#[test]
fn workspace_load_gate_stubs_whole_config_loading() {
    use crate::metadata::{intern_configuration_path, MetadataDb as _};

    // An on-disk config root with one common module: the whole-config loader
    // parses it when, and only when, the load gate is open.
    let root =
        std::env::temp_dir().join(format!("bsl_load_gate_{}_{}", std::process::id(), line!()));
    let cm_dir = root.join("CommonModules");
    // The whole-config loader lists module DIRECTORIES and pairs each with its
    // sibling XML, so both must exist.
    std::fs::create_dir_all(cm_dir.join("МойМодуль/Ext")).unwrap();
    std::fs::write(cm_dir.join("МойМодуль/Ext/Module.bsl"), "").unwrap();
    std::fs::write(
        cm_dir.join("МойМодуль.xml"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <CommonModule uuid="00000000-0000-0000-0000-000000000031">
        <Properties><Name>МойМодуль</Name><Server>true</Server></Properties>
    </CommonModule>
</MetaDataObject>"#,
    )
    .unwrap();

    let mut db = RootDatabaseImpl::new();
    assert!(db.workspace_load_complete(), "the gate defaults to open for batch hosts");
    let root_str = root.to_string_lossy().to_string();

    db.set_workspace_load_complete(false);
    {
        let rev = db.config_root_revision_for_path(&root);
        let path_input = intern_configuration_path(&db, &root_str, rev);
        let gated = db.load_configuration(path_input);
        assert!(
            gated.find_common_module("МойМодуль").is_none(),
            "a closed gate must resolve nothing instead of parsing the config"
        );
    }

    db.set_workspace_load_complete(true);
    let rev = db.config_root_revision_for_path(&root);
    let path_input = intern_configuration_path(&db, &root_str, rev);
    let loaded = db.load_configuration(path_input);
    assert!(
        loaded.find_common_module("МойМодуль").is_some(),
        "reopening the gate must load the real configuration"
    );

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn changing_only_source_exclusions_invalidates_the_whole_configuration_loader() {
    use crate::metadata::{intern_configuration_path, MetadataDb as _, WorkspaceConfigsSnapshot};

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let write_module = |directory: &str, name: &str| {
        let module_root = root.join("CommonModules").join(directory);
        std::fs::create_dir_all(module_root.join("Ext")).unwrap();
        std::fs::write(module_root.join("Ext/Module.bsl"), "").unwrap();
        std::fs::write(
            root.join("CommonModules").join(format!("{directory}.xml")),
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <CommonModule uuid="00000000-0000-0000-0000-000000000041">
        <Properties><Name>{name}</Name><Server>true</Server></Properties>
    </CommonModule>
</MetaDataObject>"#
            ),
        )
        .unwrap();
    };
    write_module("Allowed", "Разрешенный");
    write_module("Hidden", "Скрытый");

    let mut db = RootDatabaseImpl::new();
    db.set_workspace_configs_snapshot(WorkspaceConfigsSnapshot::from_paths(vec![(
        None,
        root.clone(),
    )]));
    let root_str = root.to_string_lossy().to_string();
    let revision = db.config_root_revision_for_path(&root);
    {
        let path_input = intern_configuration_path(&db, &root_str, revision);
        let ordinary = db.load_configuration(path_input);
        assert!(ordinary.find_common_module("Разрешенный").is_some());
        assert!(ordinary.find_common_module("Скрытый").is_some());
    }

    let mut scoped = WorkspaceConfigsSnapshot::from_paths(vec![(None, root.clone())]);
    scoped.source_exclusions =
        project_model::ExcludedPaths::new([root.join("CommonModules/Hidden")]);
    db.set_workspace_configs_snapshot(scoped);
    {
        let path_input = intern_configuration_path(&db, &root_str, revision);
        let narrowed = db.load_configuration(path_input);
        assert!(narrowed.find_common_module("Разрешенный").is_some());
        assert!(narrowed.find_common_module("Скрытый").is_none());
    }

    db.set_workspace_configs_snapshot(WorkspaceConfigsSnapshot::from_paths(vec![(None, root)]));
    let path_input = intern_configuration_path(&db, &root_str, revision);
    let restored = db.load_configuration(path_input);
    assert!(restored.find_common_module("Разрешенный").is_some());
    assert!(restored.find_common_module("Скрытый").is_some());
}

#[test]
fn parse_mdo_query_parses_catalog_from_overlay() {
    use crate::metadata::{parse_mdo_query, MdoFiles};
    use bsl_metadata::MdoType;

    let mut db = RootDatabaseImpl::new();
    let file_id = FileId(0);

    let mut file_set = FileSet::new();
    file_set.insert(file_id, VfsPath::new("/Catalogs/Справочник1.xml"));
    let source_root = SourceRoot::new_local(file_set);
    db.set_source_root(SourceRootId(0), source_root);
    db.set_file_source_root(file_id, SourceRootId(0));

    let xml = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../bsl-metadata/fixtures/designer/Catalogs/Справочник1.xml"
    ));
    db.set_file_text(file_id, xml);

    let listing = crate::metadata::MetadataListingInput::new(
        &db,
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
    );
    let files = MdoFiles::new(&db, MdoType::Catalog, file_id, None, listing);
    let mdo = parse_mdo_query(&db, files).expect("catalog parsed via per-MDO query");
    assert_eq!(mdo.name, "Справочник1");

    // Re-query without any change returns the memoised Arc.
    let again = parse_mdo_query(&db, files).expect("catalog parsed again");
    assert!(Arc::ptr_eq(&mdo, &again), "parse_mdo_query should memoise");
}

#[test]
fn resolve_metadata_object_isolates_content_and_structure() {
    use crate::metadata::{config_index, resolve_metadata_object, MdoEntry, MetadataListingInput};
    use bsl_metadata::MdoType;
    use salsa::Setter;

    fn catalog_xml(name: &str, uuid: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <Catalog uuid="{uuid}">
        <Properties><Name>{name}</Name><CodeLength>9</CodeLength></Properties>
    </Catalog>
</MetaDataObject>"#
        )
    }

    let mut db = RootDatabaseImpl::new();
    let f1 = FileId(0);
    let f2 = FileId(1);

    let mut file_set = FileSet::new();
    file_set.insert(f1, VfsPath::new("/Catalogs/Справочник1.xml"));
    file_set.insert(f2, VfsPath::new("/Catalogs/Товары.xml"));
    db.set_source_root(SourceRootId(1), SourceRoot::new_local(file_set));
    db.set_file_source_root(f1, SourceRootId(1));
    db.set_file_source_root(f2, SourceRootId(1));

    db.set_file_text(
        f1,
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../bsl-metadata/fixtures/designer/Catalogs/Справочник1.xml"
        )),
    );
    db.set_file_text(f2, &catalog_xml("Товары", "00000000-0000-0000-0000-000000000002"));

    // Structure listing carries only Справочник1 at first.
    let listing = MetadataListingInput::new(
        &db,
        Arc::new(vec![MdoEntry {
            kind: MdoType::Catalog,
            name: "Справочник1".to_string(),
            main: f1,
            predefined: None,
        }]),
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
    );
    assert_eq!(config_index(&db, listing).len(), 1);

    let c1 = resolve_metadata_object(&db, listing, MdoType::Catalog, "Справочник1".to_string())
        .expect("Справочник1 resolves");
    assert_eq!(c1.name, "Справочник1");

    // Case-insensitive lookup.
    assert!(
        resolve_metadata_object(&db, listing, MdoType::Catalog, "справочник1".to_string())
            .is_some(),
        "lookup must be case-insensitive"
    );

    // An absent name resolves to None — and this miss depends on config_index,
    // so adding the MDO later must invalidate it.
    assert!(
        resolve_metadata_object(&db, listing, MdoType::Catalog, "Товары".to_string()).is_none(),
        "Товары is not in the listing yet"
    );

    // Re-resolving an unchanged MDO returns the memoised Arc.
    let c1_again =
        resolve_metadata_object(&db, listing, MdoType::Catalog, "Справочник1".to_string()).unwrap();
    assert!(Arc::ptr_eq(&c1, &c1_again), "unchanged resolution must memoise");

    // Structure change: add Товары to the listing.
    listing.set_entries(&mut db).to(Arc::new(vec![
        MdoEntry {
            kind: MdoType::Catalog,
            name: "Справочник1".to_string(),
            main: f1,
            predefined: None,
        },
        MdoEntry {
            kind: MdoType::Catalog, name: "Товары".to_string(), main: f2, predefined: None
        },
    ]));

    let tovary = resolve_metadata_object(&db, listing, MdoType::Catalog, "Товары".to_string())
        .expect("Товары resolves after being added to the structure");
    assert_eq!(tovary.name, "Товары");

    // The sibling (Справочник1) is untouched by a content edit to Товары: a
    // content edit re-parses only the edited MDO.
    let c1_before_edit =
        resolve_metadata_object(&db, listing, MdoType::Catalog, "Справочник1".to_string()).unwrap();
    db.set_file_text(f2, &catalog_xml("Товары", "00000000-0000-0000-0000-000000000099"));
    let tovary_after =
        resolve_metadata_object(&db, listing, MdoType::Catalog, "Товары".to_string()).unwrap();
    assert_eq!(tovary_after.name, "Товары");
    let c1_after_edit =
        resolve_metadata_object(&db, listing, MdoType::Catalog, "Справочник1".to_string()).unwrap();
    assert!(
        Arc::ptr_eq(&c1_before_edit, &c1_after_edit),
        "a content edit to one MDO must not re-resolve a sibling"
    );
}

#[test]
fn resolve_register_by_name_resolves_via_listing_substrate() {
    use crate::metadata::{resolve_register_by_name, MdoEntry, MetadataListingInput};
    use bsl_metadata::MdoType;

    let mut db = RootDatabaseImpl::new();
    let f = FileId(0);
    let mut file_set = FileSet::new();
    file_set.insert(f, VfsPath::new("/InformationRegisters/РегистрСведений1.xml"));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    db.set_file_source_root(f, SourceRootId(0));
    db.set_file_text(
        f,
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../bsl-metadata/fixtures/designer/InformationRegisters/РегистрСведений1.xml"
        )),
    );

    // The register lives in `entries` (MDOs + registers share the listing), keyed
    // by a register MdoType, so the name-only index picks it up.
    let listing = MetadataListingInput::new(
        &db,
        Arc::new(vec![MdoEntry {
            kind: MdoType::InformationRegister,
            name: "РегистрСведений1".to_string(),
            main: f,
            predefined: None,
        }]),
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
    );

    let reg = resolve_register_by_name(&db, listing, "РегистрСведений1".to_string())
        .expect("register resolves by name alone");
    assert_eq!(reg.mdo_type(), MdoType::InformationRegister);
    assert_eq!(reg.name(), "РегистрСведений1");

    // BSL is case-insensitive.
    assert!(
        resolve_register_by_name(&db, listing, "регистрсведений1".to_string()).is_some(),
        "by-name lookup must be case-insensitive"
    );

    // An unrelated name resolves to None — and this miss depends on config_index,
    // so adding the register later would invalidate it.
    assert!(
        resolve_register_by_name(&db, listing, "НетТакогоРегистра".to_string()).is_none(),
        "unknown register name must not resolve"
    );
}

#[test]
fn resolve_defined_type_isolates_content_and_structure() {
    use crate::metadata::{
        defined_type_index, resolve_defined_type, DefinedTypeEntry, MetadataListingInput,
    };
    use bsl_metadata::AttributeType;
    use salsa::Setter;

    fn defined_type_xml(name: &str, inner: &str) -> String {
        format!(
            concat!(
                "<MetaDataObject>",
                "<DefinedType uuid=\"00000000-0000-0000-0000-000000000010\">",
                "<Properties><Name>{}</Name><Type><Type>{}</Type></Type></Properties>",
                "</DefinedType></MetaDataObject>"
            ),
            name, inner
        )
    }

    let mut db = RootDatabaseImpl::new();
    let f1 = FileId(0);

    let mut file_set = FileSet::new();
    file_set.insert(f1, VfsPath::new("/DefinedTypes/ДенежнаяСумма.xml"));
    db.set_source_root(SourceRootId(1), SourceRoot::new_local(file_set));
    db.set_file_source_root(f1, SourceRootId(1));
    db.set_file_text(f1, &defined_type_xml("ДенежнаяСумма", "xs:boolean"));

    let listing = MetadataListingInput::new(
        &db,
        Arc::new(Vec::new()),
        Arc::new(vec![DefinedTypeEntry {
            name: "ДенежнаяСумма".to_string(), main: f1
        }]),
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
    );
    assert_eq!(defined_type_index(&db, listing).lookup("денежнаясумма"), Some(f1));

    let t1 = resolve_defined_type(&db, listing, "ДенежнаяСумма".to_string())
        .expect("ДенежнаяСумма resolves");
    assert_eq!(*t1, AttributeType::Boolean);

    // Case-insensitive, and an absent name is None (the miss depends on the index).
    assert!(resolve_defined_type(&db, listing, "денежнаясумма".to_string()).is_some());
    assert!(resolve_defined_type(&db, listing, "Нет".to_string()).is_none());

    // Re-resolving unchanged memoises.
    let t1_again = resolve_defined_type(&db, listing, "ДенежнаяСумма".to_string()).unwrap();
    assert!(Arc::ptr_eq(&t1, &t1_again), "unchanged resolution must memoise");

    // A content edit re-parses to the new underlying type.
    db.set_file_text(f1, &defined_type_xml("ДенежнаяСумма", "xs:string"));
    let t1_edited = resolve_defined_type(&db, listing, "ДенежнаяСумма".to_string()).unwrap();
    assert!(matches!(*t1_edited, AttributeType::String { .. }), "content edit must re-parse");

    // A structure removal tombstones it.
    listing.set_defined_types(&mut db).to(Arc::new(Vec::new()));
    assert!(resolve_defined_type(&db, listing, "ДенежнаяСумма".to_string()).is_none());
}

#[test]
fn resolve_common_module_by_name_and_by_body_file() {
    use crate::metadata::{
        common_module_index, resolve_common_module, resolve_common_module_by_file,
        CommonModuleEntry, MetadataListingInput,
    };
    use bsl_metadata::traits::MdObject;
    use salsa::Setter;

    fn common_module_xml(name: &str, global: bool) -> String {
        format!(
            concat!(
                "<MetaDataObject>",
                "<CommonModule uuid=\"00000000-0000-0000-0000-000000000020\">",
                "<Properties><Name>{}</Name><Global>{}</Global><Server>true</Server></Properties>",
                "</CommonModule></MetaDataObject>"
            ),
            name, global
        )
    }

    let mut db = RootDatabaseImpl::new();
    let xml_file = FileId(0);
    let bsl_file = FileId(1);

    let mut file_set = FileSet::new();
    file_set.insert(xml_file, VfsPath::new("/CommonModules/ОбщегоНазначения.xml"));
    file_set.insert(bsl_file, VfsPath::new("/CommonModules/ОбщегоНазначения/Ext/Module.bsl"));
    db.set_source_root(SourceRootId(1), SourceRoot::new_local(file_set));
    db.set_file_source_root(xml_file, SourceRootId(1));
    db.set_file_source_root(bsl_file, SourceRootId(1));
    db.set_file_text(xml_file, &common_module_xml("ОбщегоНазначения", true));

    let listing = MetadataListingInput::new(
        &db,
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
        Arc::new(vec![CommonModuleEntry {
            name: "ОбщегоНазначения".to_string(),
            main: xml_file,
            module_file: Some(bsl_file),
            unread_module_file: None,
        }]),
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
    );

    // The by-name lookup, the body-file-by-name lookup, and the by-body reverse
    // index all derive from the listing.
    assert_eq!(common_module_index(&db, listing).lookup("общегоназначения"), Some(xml_file));
    assert_eq!(
        common_module_index(&db, listing).lookup_module_file("общегоназначения"),
        Some(bsl_file),
        "the body Ext/Module.bsl must be resolvable by name for method/param validation"
    );
    assert!(common_module_index(&db, listing).lookup_module_file("Нет").is_none());

    let m = resolve_common_module(&db, listing, "ОбщегоНазначения".to_string())
        .expect("module resolves by name");
    assert_eq!(m.name(), "ОбщегоНазначения");
    assert!(m.is_global());

    // Case-insensitive; an absent name is None (the miss depends on the index).
    assert!(resolve_common_module(&db, listing, "общегоназначения".to_string()).is_some());
    assert!(resolve_common_module(&db, listing, "Нет".to_string()).is_none());

    // Reverse lookup: the module owning its `Ext/Module.bsl`; a non-body file is None.
    let by_file = resolve_common_module_by_file(&db, listing, bsl_file)
        .expect("module resolves by its body file");
    assert_eq!(by_file.name(), "ОбщегоНазначения");
    assert!(resolve_common_module_by_file(&db, listing, FileId(99)).is_none());

    // Re-resolving unchanged memoises.
    let m_again = resolve_common_module(&db, listing, "ОбщегоНазначения".to_string()).unwrap();
    assert!(Arc::ptr_eq(&m, &m_again), "unchanged resolution must memoise");

    // A content edit re-parses the flags.
    db.set_file_text(xml_file, &common_module_xml("ОбщегоНазначения", false));
    let m_edited = resolve_common_module(&db, listing, "ОбщегоНазначения".to_string()).unwrap();
    assert!(!m_edited.is_global(), "content edit must re-parse");

    // A structure removal tombstones both lookups.
    listing.set_common_modules(&mut db).to(Arc::new(Vec::new()));
    assert!(resolve_common_module(&db, listing, "ОбщегоНазначения".to_string()).is_none());
    assert!(resolve_common_module_by_file(&db, listing, bsl_file).is_none());
}

#[test]
fn resolve_event_subscription_isolates_content_and_structure() {
    use crate::metadata::{
        event_subscription_index, resolve_event_subscription, EventSubscriptionEntry,
        MetadataListingInput,
    };
    use salsa::Setter;

    fn event_subscription_xml(name: &str, event: &str, handler: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <EventSubscription uuid="00000000-0000-0000-0000-000000000051">
        <Properties>
            <Name>{name}</Name>
            <Source><Type>CatalogRef.Номенклатура</Type></Source>
            <Event>{event}</Event>
            <Handler>{handler}</Handler>
        </Properties>
    </EventSubscription>
</MetaDataObject>"#
        )
    }

    let mut db = RootDatabaseImpl::new();
    let before_file = FileId(0);
    let after_file = FileId(1);

    let mut file_set = FileSet::new();
    file_set.insert(before_file, VfsPath::new("/EventSubscriptions/ПередЗаписью.xml"));
    file_set.insert(after_file, VfsPath::new("/EventSubscriptions/ПослеЗаписи.xml"));
    db.set_source_root(SourceRootId(1), SourceRoot::new_local(file_set));
    db.set_file_source_root(before_file, SourceRootId(1));
    db.set_file_source_root(after_file, SourceRootId(1));
    db.set_file_text(
        before_file,
        &event_subscription_xml(
            "ПередЗаписью",
            "BeforeWrite",
            "CommonModule.ПодпискиНаСобытия.ПередЗаписью",
        ),
    );
    db.set_file_text(
        after_file,
        &event_subscription_xml(
            "ПослеЗаписи",
            "AfterWrite",
            "CommonModule.ПодпискиНаСобытия.ПослеЗаписи",
        ),
    );

    let listing = MetadataListingInput::new(
        &db,
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
        Arc::new(vec![EventSubscriptionEntry {
            name: "ПередЗаписью".to_string(),
            main: before_file,
        }]),
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
    );
    assert_eq!(event_subscription_index(&db, listing).lookup("передзаписью"), Some(before_file));

    let before = resolve_event_subscription(&db, listing, "ПередЗаписью".to_string())
        .expect("ПередЗаписью resolves");
    assert_eq!(before.name(), "ПередЗаписью");
    assert_eq!(before.event(), "BeforeWrite");
    assert_eq!(before.handler_string(), "CommonModule.ПодпискиНаСобытия.ПередЗаписью");

    assert!(resolve_event_subscription(&db, listing, "передзаписью".to_string()).is_some());
    assert!(resolve_event_subscription(&db, listing, "ПослеЗаписи".to_string()).is_none());

    let before_again =
        resolve_event_subscription(&db, listing, "ПередЗаписью".to_string()).unwrap();
    assert!(Arc::ptr_eq(&before, &before_again), "unchanged resolution must memoise");

    listing.set_event_subscriptions(&mut db).to(Arc::new(vec![
        EventSubscriptionEntry { name: "ПередЗаписью".to_string(), main: before_file },
        EventSubscriptionEntry { name: "ПослеЗаписи".to_string(), main: after_file },
    ]));
    let after = resolve_event_subscription(&db, listing, "ПослеЗаписи".to_string())
        .expect("ПослеЗаписи resolves after being added to the structure");
    assert_eq!(after.handler_string(), "CommonModule.ПодпискиНаСобытия.ПослеЗаписи");

    let before_before_edit =
        resolve_event_subscription(&db, listing, "ПередЗаписью".to_string()).unwrap();
    db.set_file_text(
        after_file,
        &event_subscription_xml(
            "ПослеЗаписи",
            "AfterWrite",
            "CommonModule.ПодпискиНаСобытия.ПослеЗаписиПовторно",
        ),
    );
    let after_edited = resolve_event_subscription(&db, listing, "ПослеЗаписи".to_string()).unwrap();
    assert_eq!(
        after_edited.handler_string(),
        "CommonModule.ПодпискиНаСобытия.ПослеЗаписиПовторно",
        "content edit must re-parse the edited subscription"
    );
    let before_after_edit =
        resolve_event_subscription(&db, listing, "ПередЗаписью".to_string()).unwrap();
    assert!(
        Arc::ptr_eq(&before_before_edit, &before_after_edit),
        "a content edit to one event subscription must not re-resolve a sibling"
    );

    listing.set_event_subscriptions(&mut db).to(Arc::new(Vec::new()));
    assert!(resolve_event_subscription(&db, listing, "ПередЗаписью".to_string()).is_none());
    assert!(resolve_event_subscription(&db, listing, "ПослеЗаписи".to_string()).is_none());
}

#[test]
fn resolve_event_subscription_for_file_uses_bootstrapped_listing() {
    use crate::metadata::EventSubscriptionEntry;

    fn event_subscription_xml(name: &str, event: &str, handler: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <EventSubscription uuid="00000000-0000-0000-0000-000000000052">
        <Properties>
            <Name>{name}</Name>
            <Source><Type>CatalogRef.Номенклатура</Type></Source>
            <Event>{event}</Event>
            <Handler>{handler}</Handler>
        </Properties>
    </EventSubscription>
</MetaDataObject>"#
        )
    }

    let root = std::env::temp_dir().join(format!(
        "bsl_event_subscription_for_file_{}_{}",
        std::process::id(),
        line!()
    ));
    let subscription_path = root.join("EventSubscriptions/ПередЗаписью.xml");
    std::fs::create_dir_all(subscription_path.parent().unwrap()).unwrap();

    let mut db = RootDatabaseImpl::new();
    let subscription_file = FileId(0);
    let module_file = FileId(1);
    let module_path = root.join("EventSubscriptionConsumer.bsl");

    let mut file_set = FileSet::new();
    file_set.insert(subscription_file, VfsPath::new(subscription_path.to_string_lossy().as_ref()));
    file_set.insert(module_file, VfsPath::new(module_path.to_string_lossy().as_ref()));
    db.set_source_root(SourceRootId(1), SourceRoot::new_local(file_set));
    db.set_file_source_root(subscription_file, SourceRootId(1));
    db.set_file_source_root(module_file, SourceRootId(1));
    db.set_file_text(
        subscription_file,
        &event_subscription_xml(
            "ПередЗаписью",
            "BeforeWrite",
            "CommonModule.ПодпискиНаСобытия.ПередЗаписью",
        ),
    );
    db.set_file_text(module_file, "Процедура Т() КонецПроцедуры");

    db.set_all_config_paths(vec![(None, root.clone())]);
    db.set_metadata_listing(
        &root.to_string_lossy(),
        MetadataListingData {
            common_attributes: Vec::new(),
            entries: Vec::new(),
            defined_types: Vec::new(),
            common_modules: Vec::new(),
            event_subscriptions: vec![EventSubscriptionEntry {
                name: "ПередЗаписью".to_string(),
                main: subscription_file,
            }],
            scheduled_jobs: Vec::new(),
            roles: Vec::new(),
            http_services: Vec::new(),
            web_services: Vec::new(),
            integration_services: Vec::new(),
            subsystems: Vec::new(),
        },
    );

    let resolved = db
        .resolve_event_subscription_for_file(module_file, "ПередЗаписью")
        .expect("event subscription resolves through the bootstrapped per-kind substrate");
    assert_eq!(resolved.name(), "ПередЗаписью");
    assert_eq!(resolved.event(), "BeforeWrite");
    assert!(
        db.resolve_event_subscription_for_file(module_file, "НетТакойПодписки").is_none(),
        "unknown event subscription name must not resolve"
    );

    std::fs::remove_dir_all(&root).ok();
}

/// Проект из одного расширения: главной конфигурации нет, и main-scope
/// диагностика обязана видеть подписки расширения — их владелец и есть
/// единственная конфигурация проекта (github#172).
#[test]
fn main_event_subscription_names_fall_back_to_the_chain_for_an_extension_only_project() {
    use crate::metadata::EventSubscriptionEntry;

    fn event_subscription_xml(name: &str, event: &str, handler: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <EventSubscription uuid="00000000-0000-0000-0000-000000000172">
        <Properties>
            <Name>{name}</Name>
            <Source><Type>CatalogRef.Номенклатура</Type></Source>
            <Event>{event}</Event>
            <Handler>{handler}</Handler>
        </Properties>
    </EventSubscription>
</MetaDataObject>"#
        )
    }

    let root = std::env::temp_dir().join(format!(
        "bsl_main_event_subscription_extension_only_{}_{}",
        std::process::id(),
        line!()
    ));
    let subscription_path = root.join("EventSubscriptions/ПередЗаписью.xml");
    std::fs::create_dir_all(subscription_path.parent().unwrap()).unwrap();

    let mut db = RootDatabaseImpl::new();
    let subscription_file = FileId(0);
    let module_file = FileId(1);
    let module_path = root.join("EventSubscriptionConsumer.bsl");

    let mut file_set = FileSet::new();
    file_set.insert(subscription_file, VfsPath::new(subscription_path.to_string_lossy().as_ref()));
    file_set.insert(module_file, VfsPath::new(module_path.to_string_lossy().as_ref()));
    db.set_source_root(SourceRootId(1), SourceRoot::new_local(file_set));
    db.set_file_source_root(subscription_file, SourceRootId(1));
    db.set_file_source_root(module_file, SourceRootId(1));
    db.set_file_text(
        subscription_file,
        &event_subscription_xml(
            "ПередЗаписью",
            "BeforeWrite",
            "CommonModule.ПодпискиНаСобытия.ПередЗаписью",
        ),
    );
    db.set_file_text(module_file, "Процедура Т() КонецПроцедуры");

    // Единственная конфигурация — расширение X: главной нет вовсе.
    db.set_all_config_paths(vec![(Some("X".to_string()), root.clone())]);
    db.set_metadata_listing(
        &root.to_string_lossy(),
        MetadataListingData {
            common_attributes: Vec::new(),
            entries: Vec::new(),
            defined_types: Vec::new(),
            common_modules: Vec::new(),
            event_subscriptions: vec![EventSubscriptionEntry {
                name: "ПередЗаписью".to_string(),
                main: subscription_file,
            }],
            scheduled_jobs: Vec::new(),
            roles: Vec::new(),
            http_services: Vec::new(),
            web_services: Vec::new(),
            integration_services: Vec::new(),
            subsystems: Vec::new(),
        },
    );

    assert_eq!(
        db.main_event_subscription_names_for_file(module_file),
        vec!["ПередЗаписью".to_string()],
        "without a base the main scope falls back to the file's extension chain"
    );
    assert_eq!(db.event_subscription_names_for_file(module_file), vec!["ПередЗаписью".to_string()]);

    std::fs::remove_dir_all(&root).ok();
}

/// Главная конфигурация зарегистрирована, но субстрат не поднят (batch/CLI):
/// main-область обязана остаться main-only — фолбэк на цепочку полагается лишь
/// проекту, у которого главного корня нет вовсе (github#172).
#[test]
fn main_event_subscription_names_stay_main_only_without_the_substrate() {
    fn event_subscription_xml(uuid: &str, name: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <EventSubscription uuid="{uuid}">
        <Properties>
            <Name>{name}</Name>
            <Source><Type>CatalogRef.Номенклатура</Type></Source>
            <Event>BeforeWrite</Event>
            <Handler>CommonModule.Сервер.Обработчик</Handler>
        </Properties>
    </EventSubscription>
</MetaDataObject>"#
        )
    }

    let root = std::env::temp_dir().join(format!(
        "bsl_main_event_subscription_main_unbootstrapped_{}_{}",
        std::process::id(),
        line!()
    ));
    let main_root = root.join("cf");
    let ext_root = root.join("cfe/X");
    std::fs::create_dir_all(main_root.join("EventSubscriptions")).unwrap();
    std::fs::create_dir_all(ext_root.join("EventSubscriptions")).unwrap();
    std::fs::write(main_root.join("Configuration.xml"), "<Configuration/>").unwrap();
    std::fs::write(ext_root.join("Configuration.xml"), "<Configuration/>").unwrap();
    std::fs::write(
        main_root.join("EventSubscriptions/ИзГлавной.xml"),
        event_subscription_xml("00000000-0000-0000-0000-000000000201", "ИзГлавной"),
    )
    .unwrap();
    std::fs::write(
        ext_root.join("EventSubscriptions/ИзРасширения.xml"),
        event_subscription_xml("00000000-0000-0000-0000-000000000202", "ИзРасширения"),
    )
    .unwrap();

    let mut db = RootDatabaseImpl::new();
    let module_file = FileId(0);
    let module_path = ext_root.join("EventSubscriptionConsumer.bsl");
    let mut file_set = FileSet::new();
    file_set.insert(module_file, VfsPath::new(module_path.to_string_lossy().as_ref()));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    db.set_file_source_root(module_file, SourceRootId(0));
    db.set_file_text(module_file, "Процедура Т() КонецПроцедуры");

    db.set_all_config_paths(vec![
        (None, main_root.clone()),
        (Some("X".to_string()), ext_root.clone()),
    ]);
    // Субстрат намеренно не поднят: `set_metadata_listing` не зовётся.

    assert_eq!(
        db.main_event_subscription_names_for_file(module_file),
        vec!["ИзГлавной".to_string()],
        "a registered main root keeps the main-only scope even without a substrate"
    );
    assert_eq!(
        db.event_subscription_names_for_file(module_file),
        vec!["ИзГлавной".to_string(), "ИзРасширения".to_string()],
        "the visible enumeration still sees the extension"
    );

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn resolve_scheduled_job_isolates_content_and_structure() {
    use crate::metadata::{
        resolve_scheduled_job, scheduled_job_index, MetadataListingInput, ScheduledJobEntry,
    };
    use salsa::Setter;

    fn scheduled_job_xml(name: &str, method_name: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <ScheduledJob uuid="00000000-0000-0000-0000-000000000071">
        <Properties>
            <Name>{name}</Name>
            <MethodName>{method_name}</MethodName>
            <Use>true</Use>
            <Predefined>false</Predefined>
        </Properties>
    </ScheduledJob>
</MetaDataObject>"#
        )
    }

    let mut db = RootDatabaseImpl::new();
    let before_file = FileId(0);
    let after_file = FileId(1);

    let mut file_set = FileSet::new();
    file_set.insert(before_file, VfsPath::new("/ScheduledJobs/РегламентноеЗадание1.xml"));
    file_set.insert(after_file, VfsPath::new("/ScheduledJobs/РегламентноеЗадание2.xml"));
    db.set_source_root(SourceRootId(1), SourceRoot::new_local(file_set));
    db.set_file_source_root(before_file, SourceRootId(1));
    db.set_file_source_root(after_file, SourceRootId(1));
    db.set_file_text(
        before_file,
        &scheduled_job_xml(
            "РегламентноеЗадание1",
            "CommonModule.ПервыйОбщийМодуль.НеУстаревшаяПроцедура",
        ),
    );
    db.set_file_text(
        after_file,
        &scheduled_job_xml(
            "РегламентноеЗадание2",
            "CommonModule.ПервыйОбщийМодуль.НеУстаревшаяПроцедура",
        ),
    );

    let listing = MetadataListingInput::new(
        &db,
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
        Arc::new(vec![ScheduledJobEntry {
            name: "РегламентноеЗадание1".to_string(),
            main: before_file,
        }]),
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
    );
    assert_eq!(scheduled_job_index(&db, listing).lookup("регламентноезадание1"), Some(before_file));

    let before = resolve_scheduled_job(&db, listing, "РегламентноеЗадание1".to_string())
        .expect("РегламентноеЗадание1 resolves");
    assert_eq!(before.name(), "РегламентноеЗадание1");
    assert_eq!(before.method_name(), "CommonModule.ПервыйОбщийМодуль.НеУстаревшаяПроцедура");

    assert!(resolve_scheduled_job(&db, listing, "регламентноезадание1".to_string()).is_some());
    assert!(resolve_scheduled_job(&db, listing, "РегламентноеЗадание2".to_string()).is_none());

    let before_again =
        resolve_scheduled_job(&db, listing, "РегламентноеЗадание1".to_string()).unwrap();
    assert!(Arc::ptr_eq(&before, &before_again), "unchanged resolution must memoise");

    listing.set_scheduled_jobs(&mut db).to(Arc::new(vec![
        ScheduledJobEntry {
            name: "РегламентноеЗадание1".to_string(), main: before_file
        },
        ScheduledJobEntry {
            name: "РегламентноеЗадание2".to_string(), main: after_file
        },
    ]));
    let after = resolve_scheduled_job(&db, listing, "РегламентноеЗадание2".to_string())
        .expect("РегламентноеЗадание2 resolves after being added to the structure");
    assert_eq!(after.method_name(), "CommonModule.ПервыйОбщийМодуль.НеУстаревшаяПроцедура");

    let before_before_edit =
        resolve_scheduled_job(&db, listing, "РегламентноеЗадание1".to_string()).unwrap();
    db.set_file_text(
        after_file,
        &scheduled_job_xml(
            "РегламентноеЗадание2",
            "CommonModule.ПервыйОбщийМодуль.НеУстаревшаяПроцедураПовторно",
        ),
    );
    let after_edited =
        resolve_scheduled_job(&db, listing, "РегламентноеЗадание2".to_string()).unwrap();
    assert_eq!(
        after_edited.method_name(),
        "CommonModule.ПервыйОбщийМодуль.НеУстаревшаяПроцедураПовторно",
        "content edit must re-parse the edited scheduled job"
    );
    let before_after_edit =
        resolve_scheduled_job(&db, listing, "РегламентноеЗадание1".to_string()).unwrap();
    assert!(
        Arc::ptr_eq(&before_before_edit, &before_after_edit),
        "a content edit to one scheduled job must not re-resolve a sibling"
    );

    listing.set_scheduled_jobs(&mut db).to(Arc::new(Vec::new()));
    assert!(resolve_scheduled_job(&db, listing, "РегламентноеЗадание1".to_string()).is_none());
    assert!(resolve_scheduled_job(&db, listing, "РегламентноеЗадание2".to_string()).is_none());
}

#[test]
fn resolve_scheduled_job_for_file_uses_bootstrapped_listing() {
    use crate::metadata::ScheduledJobEntry;

    fn scheduled_job_xml(name: &str, method_name: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <ScheduledJob uuid="00000000-0000-0000-0000-000000000072">
        <Properties>
            <Name>{name}</Name>
            <MethodName>{method_name}</MethodName>
            <Use>true</Use>
            <Predefined>false</Predefined>
        </Properties>
    </ScheduledJob>
</MetaDataObject>"#
        )
    }

    let root = std::env::temp_dir().join(format!(
        "bsl_scheduled_job_for_file_{}_{}",
        std::process::id(),
        line!()
    ));
    let job_path = root.join("ScheduledJobs/РегламентноеЗадание1.xml");
    std::fs::create_dir_all(job_path.parent().unwrap()).unwrap();

    let mut db = RootDatabaseImpl::new();
    let job_file = FileId(0);
    let module_file = FileId(1);
    let module_path = root.join("ScheduledJobConsumer.bsl");

    let mut file_set = FileSet::new();
    file_set.insert(job_file, VfsPath::new(job_path.to_string_lossy().as_ref()));
    file_set.insert(module_file, VfsPath::new(module_path.to_string_lossy().as_ref()));
    db.set_source_root(SourceRootId(1), SourceRoot::new_local(file_set));
    db.set_file_source_root(job_file, SourceRootId(1));
    db.set_file_source_root(module_file, SourceRootId(1));
    db.set_file_text(
        job_file,
        &scheduled_job_xml(
            "РегламентноеЗадание1",
            "CommonModule.ПервыйОбщийМодуль.НеУстаревшаяПроцедура",
        ),
    );
    db.set_file_text(module_file, "Процедура Т() КонецПроцедуры");

    db.set_all_config_paths(vec![(None, root.clone())]);
    db.set_metadata_listing(
        &root.to_string_lossy(),
        MetadataListingData {
            common_attributes: Vec::new(),
            entries: Vec::new(),
            defined_types: Vec::new(),
            common_modules: Vec::new(),
            event_subscriptions: Vec::new(),
            scheduled_jobs: vec![ScheduledJobEntry {
                name: "РегламентноеЗадание1".to_string(),
                main: job_file,
            }],
            roles: Vec::new(),
            http_services: Vec::new(),
            web_services: Vec::new(),
            integration_services: Vec::new(),
            subsystems: Vec::new(),
        },
    );

    let resolved = db
        .resolve_scheduled_job_for_file(module_file, "РегламентноеЗадание1")
        .expect("scheduled job resolves through the bootstrapped per-kind substrate");
    assert_eq!(resolved.name(), "РегламентноеЗадание1");
    assert_eq!(resolved.method_name(), "CommonModule.ПервыйОбщийМодуль.НеУстаревшаяПроцедура");
    assert!(
        db.resolve_scheduled_job_for_file(module_file, "НетТакогоРегламентногоЗадания").is_none(),
        "unknown scheduled job name must not resolve"
    );

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn resolve_role_isolates_main_and_rights_content() {
    use crate::metadata::{resolve_role, role_index, MetadataListingInput, RoleEntry};
    use salsa::Setter;

    fn role_xml(name: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <Role uuid="00000000-0000-0000-0000-000000000081">
        <Properties>
            <Name>{name}</Name>
            <Synonym/>
            <Comment/>
        </Properties>
    </Role>
</MetaDataObject>"#
        )
    }

    fn rights_xml(set_for_new_objects: bool, object_name: &str, condition: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<Rights xmlns="http://v8.1c.ru/8.2/roles" version="2.10">
    <setForNewObjects>{set_for_new_objects}</setForNewObjects>
    <setForAttributesByDefault>false</setForAttributesByDefault>
    <independentRightsOfChildObjects>false</independentRightsOfChildObjects>
    <object>
        <name>{object_name}</name>
        <right>
            <name>Read</name>
            <value>true</value>
            <restrictionByCondition>
                <condition>{condition}</condition>
            </restrictionByCondition>
        </right>
    </object>
</Rights>"#
        )
    }

    let mut db = RootDatabaseImpl::new();
    let role1_main = FileId(0);
    let role1_rights = FileId(1);
    let role2_main = FileId(2);
    let role2_rights = FileId(3);

    let mut file_set = FileSet::new();
    file_set.insert(role1_main, VfsPath::new("/Roles/ТестоваяРоль.xml"));
    file_set.insert(role1_rights, VfsPath::new("/Roles/ТестоваяРоль/Ext/Rights.xml"));
    file_set.insert(role2_main, VfsPath::new("/Roles/СоседняяРоль.xml"));
    file_set.insert(role2_rights, VfsPath::new("/Roles/СоседняяРоль/Ext/Rights.xml"));
    db.set_source_root(SourceRootId(1), SourceRoot::new_local(file_set));
    db.set_file_source_root(role1_main, SourceRootId(1));
    db.set_file_source_root(role1_rights, SourceRootId(1));
    db.set_file_source_root(role2_main, SourceRootId(1));
    db.set_file_source_root(role2_rights, SourceRootId(1));

    db.set_file_text(role1_main, &role_xml("ТестоваяРоль"));
    db.set_file_text(
        role1_rights,
        &rights_xml(
            false,
            "Catalog.Контрагенты",
            "Контрагенты.Ссылка В (ВЫБРАТЬ Ссылка ИЗ Справочник.Организации)",
        ),
    );
    db.set_file_text(role2_main, &role_xml("СоседняяРоль"));
    db.set_file_text(
        role2_rights,
        &rights_xml(false, "Catalog.Организации", "Организации.Код = \"01\""),
    );

    let listing = MetadataListingInput::new(
        &db,
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
        Arc::new(vec![
            RoleEntry {
                name: "ТестоваяРоль".to_string(),
                main: role1_main,
                rights: Some(role1_rights),
            },
            RoleEntry {
                name: "СоседняяРоль".to_string(),
                main: role2_main,
                rights: Some(role2_rights),
            },
        ]),
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
    );

    let files = role_index(&db, listing)
        .lookup("тестоваяроль")
        .expect("role index resolves main and rights files");
    assert_eq!(files.main, role1_main);
    assert_eq!(files.rights, Some(role1_rights));

    let role1 =
        resolve_role(&db, listing, "ТестоваяРоль".to_string()).expect("ТестоваяРоль resolves");
    assert_eq!(role1.name(), "ТестоваяРоль");
    assert!(!role1.data().set_for_new_objects());
    assert_eq!(role1.data().objects().len(), 1);
    assert_eq!(role1.data().objects()[0].name, "Контрагенты");
    assert_eq!(
        role1.data().objects()[0].restrictions,
        vec!["Контрагенты.Ссылка В (ВЫБРАТЬ Ссылка ИЗ Справочник.Организации)".to_string()]
    );

    let role2 = resolve_role(&db, listing, "СоседняяРоль".to_string()).expect("role resolves");
    assert_eq!(role2.name(), "СоседняяРоль");

    let role1_again = resolve_role(&db, listing, "ТестоваяРоль".to_string()).unwrap();
    assert!(Arc::ptr_eq(&role1, &role1_again), "unchanged resolution must memoise");

    db.set_file_text(
        role1_rights,
        &rights_xml(
            true,
            "Catalog.Контрагенты",
            "Контрагенты.Ссылка В (ВЫБРАТЬ Ссылка ИЗ Справочник.ФизическиеЛица)",
        ),
    );
    let role1_rights_edited = resolve_role(&db, listing, "ТестоваяРоль".to_string()).unwrap();
    assert!(role1_rights_edited.data().set_for_new_objects());
    assert_eq!(
        role1_rights_edited.data().objects()[0].restrictions,
        vec!["Контрагенты.Ссылка В (ВЫБРАТЬ Ссылка ИЗ Справочник.ФизическиеЛица)".to_string()]
    );
    let role2_after_rights_edit = resolve_role(&db, listing, "СоседняяРоль".to_string()).unwrap();
    assert!(
        Arc::ptr_eq(&role2, &role2_after_rights_edit),
        "a rights edit to one role must not re-resolve a sibling"
    );

    db.set_file_text(role1_main, &role_xml("ТестоваяРольПереименованная"));
    let role1_main_edited = resolve_role(&db, listing, "ТестоваяРоль".to_string()).unwrap();
    assert_eq!(role1_main_edited.name(), "ТестоваяРольПереименованная");

    listing.set_roles(&mut db).to(Arc::new(Vec::new()));
    assert!(resolve_role(&db, listing, "ТестоваяРоль".to_string()).is_none());
}

#[test]
fn resolve_role_for_file_uses_bootstrapped_listing() {
    use crate::metadata::RoleEntry;

    fn role_xml(name: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <Role uuid="00000000-0000-0000-0000-000000000082">
        <Properties>
            <Name>{name}</Name>
            <Synonym/>
            <Comment/>
        </Properties>
    </Role>
</MetaDataObject>"#
        )
    }

    fn rights_xml() -> &'static str {
        r#"<?xml version="1.0" encoding="UTF-8"?>
<Rights xmlns="http://v8.1c.ru/8.2/roles" version="2.10">
    <setForNewObjects>true</setForNewObjects>
    <object>
        <name>Catalog.Контрагенты</name>
        <right>
            <name>Read</name>
            <value>true</value>
            <restrictionByCondition>
                <condition>Контрагенты.Ссылка В (ВЫБРАТЬ Ссылка ИЗ Справочник.Организации)</condition>
            </restrictionByCondition>
        </right>
    </object>
</Rights>"#
    }

    let root =
        std::env::temp_dir().join(format!("bsl_role_for_file_{}_{}", std::process::id(), line!()));
    let role_path = root.join("Roles/ТестоваяРоль.xml");
    std::fs::create_dir_all(role_path.parent().unwrap()).unwrap();

    let mut db = RootDatabaseImpl::new();
    let role_main = FileId(0);
    let role_rights = FileId(1);
    let consumer_file = FileId(2);
    let consumer_path = root.join("RoleConsumer.bsl");

    let mut file_set = FileSet::new();
    file_set.insert(role_main, VfsPath::new(role_path.to_string_lossy().as_ref()));
    file_set.insert(
        role_rights,
        VfsPath::new(root.join("Roles/ТестоваяРоль/Ext/Rights.xml").to_string_lossy().as_ref()),
    );
    file_set.insert(consumer_file, VfsPath::new(consumer_path.to_string_lossy().as_ref()));
    db.set_source_root(SourceRootId(1), SourceRoot::new_local(file_set));
    db.set_file_source_root(role_main, SourceRootId(1));
    db.set_file_source_root(role_rights, SourceRootId(1));
    db.set_file_source_root(consumer_file, SourceRootId(1));
    db.set_file_text(role_main, &role_xml("ТестоваяРоль"));
    db.set_file_text(role_rights, rights_xml());
    db.set_file_text(consumer_file, "Процедура Т() КонецПроцедуры");

    db.set_all_config_paths(vec![(None, root.clone())]);
    db.set_metadata_listing(
        &root.to_string_lossy(),
        MetadataListingData {
            common_attributes: Vec::new(),
            entries: Vec::new(),
            defined_types: Vec::new(),
            common_modules: Vec::new(),
            event_subscriptions: Vec::new(),
            scheduled_jobs: Vec::new(),
            roles: vec![RoleEntry {
                name: "ТестоваяРоль".to_string(),
                main: role_main,
                rights: Some(role_rights),
            }],
            http_services: Vec::new(),
            web_services: Vec::new(),
            integration_services: Vec::new(),
            subsystems: Vec::new(),
        },
    );

    let resolved = db
        .resolve_role_for_file(consumer_file, "ТестоваяРоль")
        .expect("role resolves through the bootstrapped per-kind substrate");
    assert_eq!(resolved.name(), "ТестоваяРоль");
    assert!(
        db.resolve_role_for_file(consumer_file, "НетТакойРоли").is_none(),
        "unknown role name must not resolve"
    );

    std::fs::remove_dir_all(&root).ok();
}

/// Along a dependency chain the extension that OWNS the file wins: a borrowed
/// object declared in both the dependency and the borrower must resolve to the
/// borrower's. The chain is stored dependencies-first, so reading it forward hands
/// back the dependency — silently, and differently from the whole-config path the
/// CLI takes on the same tree. Names are listed once, not once per listing.
#[test]
fn a_chain_resolves_the_owning_extension_first_and_lists_each_name_once() {
    use crate::metadata::{RoleEntry, WorkspaceConfigsSnapshot};

    const DEP_UUID: &str = "00000000-0000-0000-0000-0000000000aa";
    const OWN_UUID: &str = "00000000-0000-0000-0000-0000000000bb";

    fn role_xml(uuid: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <Role uuid="{uuid}">
        <Properties><Name>РольОбщая</Name><Synonym/><Comment/></Properties>
    </Role>
</MetaDataObject>"#
        )
    }

    let base = std::env::temp_dir().join(format!(
        "bsl_chain_precedence_{}_{}",
        std::process::id(),
        line!()
    ));
    let dep_root = base.join("A");
    let own_root = base.join("B");
    std::fs::create_dir_all(dep_root.join("Roles")).unwrap();
    std::fs::create_dir_all(own_root.join("Roles")).unwrap();

    let mut db = RootDatabaseImpl::new();
    let dep_role = FileId(0);
    let own_role = FileId(1);
    let consumer = FileId(2);
    let mut file_set = FileSet::new();
    file_set.insert(
        dep_role,
        VfsPath::new(dep_root.join("Roles/РольОбщая.xml").to_string_lossy().as_ref()),
    );
    file_set.insert(
        own_role,
        VfsPath::new(own_root.join("Roles/РольОбщая.xml").to_string_lossy().as_ref()),
    );
    file_set
        .insert(consumer, VfsPath::new(own_root.join("Consumer.bsl").to_string_lossy().as_ref()));
    db.set_source_root(SourceRootId(1), SourceRoot::new_local(file_set));
    for file in [dep_role, own_role, consumer] {
        db.set_file_source_root(file, SourceRootId(1));
    }
    db.set_file_text(dep_role, &role_xml(DEP_UUID));
    db.set_file_text(own_role, &role_xml(OWN_UUID));
    db.set_file_text(consumer, "Процедура Т() КонецПроцедуры");

    // Slot 0 is the dependency, slot 1 the borrower whose closure names it. No base
    // slot: this is the extension-only shape, where the chain is all there is.
    db.set_workspace_configs_snapshot(WorkspaceConfigsSnapshot {
        paths: vec![
            (Some("A".to_owned()), dep_root.clone()),
            (Some("B".to_owned()), own_root.clone()),
        ],
        kinds: vec![RootKind::Extension, RootKind::Extension],
        canonical_paths: vec![dep_root.clone(), own_root.clone()],
        closures: vec![Vec::new(), vec![0]],
        topological_order: vec![0, 1],
        fingerprint: None,
        source_exclusions: Default::default(),
    });
    for (root, file) in [(&dep_root, dep_role), (&own_root, own_role)] {
        db.set_metadata_listing(
            &root.to_string_lossy(),
            MetadataListingData {
                common_attributes: Vec::new(),
                entries: Vec::new(),
                defined_types: Vec::new(),
                common_modules: Vec::new(),
                event_subscriptions: Vec::new(),
                scheduled_jobs: Vec::new(),
                roles: vec![RoleEntry {
                    name: "РольОбщая".to_string(), main: file, rights: None
                }],
                http_services: Vec::new(),
                web_services: Vec::new(),
                integration_services: Vec::new(),
                subsystems: Vec::new(),
            },
        );
    }

    let resolved = db.resolve_role_for_file(consumer, "РольОбщая").expect("the role resolves");
    assert_eq!(
        resolved.uuid().to_string(),
        OWN_UUID,
        "the borrower's own declaration wins over the dependency's"
    );
    assert_eq!(
        db.role_names_for_file(consumer),
        vec!["РольОбщая".to_string()],
        "and the shared name is listed once, not once per listing"
    );

    std::fs::remove_dir_all(&base).ok();
}

/// An extension-only project has no base root at all, so a substrate reader that
/// insists on the base answers "not found" for objects the extension plainly
/// declares. The per-file lookup must serve such a project from its chain, exactly
/// as it serves a based project from its base.
#[test]
fn per_file_lookups_serve_an_extension_only_project_from_its_chain() {
    use crate::metadata::RoleEntry;

    fn role_xml(name: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <Role uuid="00000000-0000-0000-0000-000000000083">
        <Properties><Name>{name}</Name><Synonym/><Comment/></Properties>
    </Role>
</MetaDataObject>"#
        )
    }

    // `label` is what makes this an extension root rather than a base one; the rest
    // of the stand is identical, so the label is the only variable under test.
    let resolve_with = |label: Option<String>| -> Option<String> {
        let root = std::env::temp_dir().join(format!(
            "bsl_extension_only_lookup_{}_{:?}",
            std::process::id(),
            label
        ));
        let role_path = root.join("Roles/РольРасширения.xml");
        std::fs::create_dir_all(role_path.parent().unwrap()).unwrap();

        let mut db = RootDatabaseImpl::new();
        let role_main = FileId(0);
        let consumer_file = FileId(1);
        let mut file_set = FileSet::new();
        file_set.insert(role_main, VfsPath::new(role_path.to_string_lossy().as_ref()));
        file_set.insert(
            consumer_file,
            VfsPath::new(root.join("Consumer.bsl").to_string_lossy().as_ref()),
        );
        db.set_source_root(SourceRootId(1), SourceRoot::new_local(file_set));
        db.set_file_source_root(role_main, SourceRootId(1));
        db.set_file_source_root(consumer_file, SourceRootId(1));
        db.set_file_text(role_main, &role_xml("РольРасширения"));
        db.set_file_text(consumer_file, "Процедура Т() КонецПроцедуры");

        db.set_all_config_paths(vec![(label, root.clone())]);
        db.set_metadata_listing(
            &root.to_string_lossy(),
            MetadataListingData {
                common_attributes: Vec::new(),
                entries: Vec::new(),
                defined_types: Vec::new(),
                common_modules: Vec::new(),
                event_subscriptions: Vec::new(),
                scheduled_jobs: Vec::new(),
                roles: vec![RoleEntry {
                    name: "РольРасширения".to_string(),
                    main: role_main,
                    rights: None,
                }],
                http_services: Vec::new(),
                web_services: Vec::new(),
                integration_services: Vec::new(),
                subsystems: Vec::new(),
            },
        );

        let resolved =
            db.resolve_role_for_file(consumer_file, "РольРасширения").map(|r| r.name().to_string());
        let listed = db.role_names_for_file(consumer_file);
        std::fs::remove_dir_all(&root).ok();
        assert_eq!(
            listed.contains(&"РольРасширения".to_string()),
            resolved.is_some(),
            "resolving a name and listing it must agree"
        );
        resolved
    };

    // The control: with a base root the object resolves, as it always did.
    assert_eq!(resolve_with(None).as_deref(), Some("РольРасширения"), "based project");
    assert_eq!(
        resolve_with(Some("Feature".to_owned())).as_deref(),
        Some("РольРасширения"),
        "an extension-only project resolves its own objects too"
    );
}

#[test]
fn role_links_to_object_rights_and_rls_condition_object_from_listed_substrate() {
    use crate::metadata::RoleEntry;
    use bsl_metadata::MdoType;
    use hir::call_graph::{EdgeKind, EdgeProvenance, GraphNode};
    use hir::graph_index::{project_workspace_role_edges, GraphBuildState};

    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("src/cf");
    std::fs::create_dir_all(root.join("Catalogs")).unwrap();
    std::fs::create_dir_all(root.join("Roles/ТестоваяРоль/Ext")).unwrap();
    std::fs::write(root.join("Configuration.xml"), "<Configuration/>").unwrap();

    let catalog = |name: &str| {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <Catalog uuid="00000000-0000-0000-0000-0000000000{:02}">
        <Properties><Name>{name}</Name></Properties>
    </Catalog>
</MetaDataObject>"#,
            name.len()
        )
    };
    std::fs::write(root.join("Catalogs/Контрагенты.xml"), catalog("Контрагенты")).unwrap();
    std::fs::write(root.join("Catalogs/Организации.xml"), catalog("Организации")).unwrap();

    let role_main = FileId(2000);
    let role_rights = FileId(2001);
    let consumer_file = FileId(2002);

    let mut db = RootDatabaseImpl::new();
    let mut file_set = FileSet::new();
    file_set.insert(
        role_main,
        VfsPath::new(root.join("Roles/ТестоваяРоль.xml").to_string_lossy().as_ref()),
    );
    file_set.insert(
        role_rights,
        VfsPath::new(root.join("Roles/ТестоваяРоль/Ext/Rights.xml").to_string_lossy().as_ref()),
    );
    file_set.insert(
        consumer_file,
        VfsPath::new(root.join("RoleConsumer.bsl").to_string_lossy().as_ref()),
    );
    db.set_source_root(SourceRootId(1), SourceRoot::new_local(file_set));
    db.set_file_source_root(role_main, SourceRootId(1));
    db.set_file_source_root(role_rights, SourceRootId(1));
    db.set_file_source_root(consumer_file, SourceRootId(1));
    db.set_file_text(
        role_main,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <Role uuid="00000000-0000-0000-0000-0000000000aa">
        <Properties><Name>ТестоваяРоль</Name></Properties>
    </Role>
</MetaDataObject>"#,
    );
    db.set_file_text(
        role_rights,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<Rights xmlns="http://v8.1c.ru/8.2/roles" version="2.10">
    <setForNewObjects>false</setForNewObjects>
    <object>
        <name>Catalog.Контрагенты</name>
        <right>
            <name>Read</name>
            <value>true</value>
            <restrictionByCondition>
                <condition>Контрагенты.Ссылка В (ВЫБРАТЬ Ссылка ИЗ Справочник.Организации)</condition>
            </restrictionByCondition>
        </right>
    </object>
</Rights>"#,
    );
    db.set_file_text(consumer_file, "Процедура Т() КонецПроцедуры");

    db.set_all_config_paths(vec![(None, root.clone())]);
    db.set_metadata_listing(
        &root.to_string_lossy(),
        MetadataListingData {
            common_attributes: Vec::new(),
            entries: Vec::new(),
            defined_types: Vec::new(),
            common_modules: Vec::new(),
            event_subscriptions: Vec::new(),
            scheduled_jobs: Vec::new(),
            roles: vec![RoleEntry {
                name: "ТестоваяРоль".to_string(),
                main: role_main,
                rights: Some(role_rights),
            }],
            http_services: Vec::new(),
            web_services: Vec::new(),
            integration_services: Vec::new(),
            subsystems: Vec::new(),
        },
    );

    let mut state = GraphBuildState::new();
    let edges = project_workspace_role_edges(&db, consumer_file, &mut state);

    let role_edge = |to_name: &str, prov: EdgeProvenance| {
        edges.iter().any(|e| {
            e.kind == EdgeKind::RoleReference
                && e.provenance == prov
                && matches!(&e.from, GraphNode::Mdo { mdo_type, object_name }
                    if *mdo_type == MdoType::Role && object_name.as_str() == "ТестоваяРоль")
                && matches!(&e.to, GraphNode::Mdo { mdo_type, object_name }
                    if *mdo_type == MdoType::Catalog && object_name.as_str() == to_name)
        })
    };

    assert!(
        role_edge("Контрагенты", EdgeProvenance::Resolved),
        "listed role substrate must still produce direct role → object edges"
    );
    assert!(
        role_edge("Организации", EdgeProvenance::Inferred),
        "listed role substrate must still produce inferred RLS condition edges"
    );

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn role_names_for_file_uses_bootstrapped_listing() {
    use crate::metadata::RoleEntry;

    fn role_xml(name: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <Role uuid="00000000-0000-0000-0000-000000000083">
        <Properties>
            <Name>{name}</Name>
            <Synonym/>
            <Comment/>
        </Properties>
    </Role>
</MetaDataObject>"#
        )
    }

    fn rights_xml() -> &'static str {
        r#"<?xml version="1.0" encoding="UTF-8"?>
<Rights xmlns="http://v8.1c.ru/8.2/roles" version="2.10">
    <setForNewObjects>false</setForNewObjects>
    <object>
        <name>Catalog.Контрагенты</name>
        <right><name>Read</name><value>true</value></right>
    </object>
</Rights>"#
    }

    let root = std::env::temp_dir().join(format!(
        "bsl_role_names_for_file_{}_{}",
        std::process::id(),
        line!()
    ));
    let role_path = root.join("Roles/ТестоваяРоль.xml");
    std::fs::create_dir_all(role_path.parent().unwrap()).unwrap();

    let mut db = RootDatabaseImpl::new();
    let role_main = FileId(0);
    let role_rights = FileId(1);
    let consumer_file = FileId(2);
    let consumer_path = root.join("RoleConsumer.bsl");

    let mut file_set = FileSet::new();
    file_set.insert(role_main, VfsPath::new(role_path.to_string_lossy().as_ref()));
    file_set.insert(
        role_rights,
        VfsPath::new(root.join("Roles/ТестоваяРоль/Ext/Rights.xml").to_string_lossy().as_ref()),
    );
    file_set.insert(consumer_file, VfsPath::new(consumer_path.to_string_lossy().as_ref()));
    db.set_source_root(SourceRootId(1), SourceRoot::new_local(file_set));
    db.set_file_source_root(role_main, SourceRootId(1));
    db.set_file_source_root(role_rights, SourceRootId(1));
    db.set_file_source_root(consumer_file, SourceRootId(1));
    db.set_file_text(role_main, &role_xml("ТестоваяРоль"));
    db.set_file_text(role_rights, rights_xml());
    db.set_file_text(consumer_file, "Процедура Т() КонецПроцедуры");

    db.set_all_config_paths(vec![(None, root.clone())]);
    db.set_metadata_listing(
        &root.to_string_lossy(),
        MetadataListingData {
            common_attributes: Vec::new(),
            entries: Vec::new(),
            defined_types: Vec::new(),
            common_modules: Vec::new(),
            event_subscriptions: Vec::new(),
            scheduled_jobs: Vec::new(),
            roles: vec![RoleEntry {
                name: "ТестоваяРоль".to_string(),
                main: role_main,
                rights: Some(role_rights),
            }],
            http_services: Vec::new(),
            web_services: Vec::new(),
            integration_services: Vec::new(),
            subsystems: Vec::new(),
        },
    );

    assert_eq!(db.role_names_for_file(consumer_file), vec!["ТестоваяРоль".to_string()]);
    assert_eq!(
        db.enumerate_roles_for_file(consumer_file)
            .iter()
            .map(|role| role.name().to_string())
            .collect::<Vec<_>>(),
        vec!["ТестоваяРоль".to_string()]
    );

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn test_root_database_basic() {
    let mut db = RootDatabaseImpl::new();
    let file_id = FileId(0);

    let mut file_set = FileSet::new();
    file_set.insert(file_id, VfsPath::new("/test.bsl"));
    let source_root = SourceRoot::new_local(file_set);
    db.set_source_root(SourceRootId(0), source_root);
    db.set_file_source_root(file_id, SourceRootId(0));

    db.set_file_text(file_id, "Процедура Тест() КонецПроцедуры");

    let parse = db.parse(file_id);
    assert!(!parse.has_errors());

    let tree = db.item_tree(file_id);
    assert_eq!(tree.top_level_items().len(), 1);

    let module_id = ModuleId::new(file_id);
    let module_data = db.module_data(module_id);
    assert_eq!(module_data.procedures.len(), 1);
    assert_eq!(module_data.functions.len(), 0);
    assert_eq!(module_data.variables.len(), 0);
}

#[test]
fn test_incremental_item_tree() {
    let mut db = RootDatabaseImpl::new();
    let file_id = FileId(0);

    let mut file_set = FileSet::new();
    file_set.insert(file_id, VfsPath::new("/test.bsl"));
    let source_root = SourceRoot::new_local(file_set);
    db.set_source_root(SourceRootId(0), source_root);
    db.set_file_source_root(file_id, SourceRootId(0));

    db.set_file_text(file_id, "Процедура Тест() КонецПроцедуры");
    let tree1 = db.item_tree(file_id);
    assert_eq!(tree1.top_level_items().len(), 1);

    db.set_file_text(
        file_id,
        r#"
Процедура Тест1() КонецПроцедуры
Функция Тест2() КонецФункции
        "#,
    );
    let tree2 = db.item_tree(file_id);
    assert_eq!(tree2.top_level_items().len(), 2);
}

#[test]
fn test_symbol_tree_query() {
    let mut db = RootDatabaseImpl::new();
    let file_id = FileId(0);

    let mut file_set = FileSet::new();
    file_set.insert(file_id, VfsPath::new("/test.bsl"));
    let source_root = SourceRoot::new_local(file_set);
    db.set_source_root(SourceRootId(0), source_root);
    db.set_file_source_root(file_id, SourceRootId(0));

    db.set_file_text(
        file_id,
        r#"
Процедура ПерваяПроцедура()
КонецПроцедуры

Функция ВтораяФункция() Экспорт
КонецФункции

Перем МодульнаяПеременная;
        "#,
    );

    let module_id = ModuleId::new(file_id);
    let symbol_tree = db.symbol_tree(module_id);

    assert_eq!(symbol_tree.methods().count(), 2);
    assert_eq!(symbol_tree.variables().count(), 1);
    assert_eq!(symbol_tree.exported_methods().count(), 1);
}

#[test]
fn test_symbol_tree_caching() {
    let mut db = RootDatabaseImpl::new();
    let file_id = FileId(0);

    let mut file_set = FileSet::new();
    file_set.insert(file_id, VfsPath::new("/test.bsl"));
    let source_root = SourceRoot::new_local(file_set);
    db.set_source_root(SourceRootId(0), source_root);
    db.set_file_source_root(file_id, SourceRootId(0));

    db.set_file_text(file_id, "Процедура Тест() КонецПроцедуры");

    let module_id = ModuleId::new(file_id);
    let tree1 = db.symbol_tree(module_id);
    assert_eq!(tree1.methods().count(), 1);

    let tree2 = db.symbol_tree(module_id);
    assert_eq!(tree2.methods().count(), 1);

    assert!(Arc::ptr_eq(&tree1, &tree2));
}

#[test]
fn test_symbol_tree_invalidation() {
    let mut db = RootDatabaseImpl::new();
    let file_id = FileId(0);

    let mut file_set = FileSet::new();
    file_set.insert(file_id, VfsPath::new("/test.bsl"));
    let source_root = SourceRoot::new_local(file_set);
    db.set_source_root(SourceRootId(0), source_root);
    db.set_file_source_root(file_id, SourceRootId(0));

    db.set_file_text(file_id, "Процедура Тест1() КонецПроцедуры");

    let module_id = ModuleId::new(file_id);
    let tree1 = db.symbol_tree(module_id);
    assert_eq!(tree1.methods().count(), 1);

    db.set_file_text(
        file_id,
        r#"
Процедура Тест1() КонецПроцедуры
Функция Тест2() КонецФункции
        "#,
    );

    let tree2 = db.symbol_tree(module_id);
    assert_eq!(tree2.methods().count(), 2);

    assert!(!Arc::ptr_eq(&tree1, &tree2));
}

#[test]
fn test_symbol_tree_case_insensitive() {
    let mut db = RootDatabaseImpl::new();
    let file_id = FileId(0);

    let mut file_set = FileSet::new();
    file_set.insert(file_id, VfsPath::new("/test.bsl"));
    let source_root = SourceRoot::new_local(file_set);
    db.set_source_root(SourceRootId(0), source_root);
    db.set_file_source_root(file_id, SourceRootId(0));

    db.set_file_text(file_id, "Процедура МояПроцедура() КонецПроцедуры");

    let module_id = ModuleId::new(file_id);
    let symbol_tree = db.symbol_tree(module_id);

    use hir::Name;
    assert!(symbol_tree.find_method(&Name::new("МояПроцедура")).is_some());
    assert!(symbol_tree.find_method(&Name::new("мояпроцедура")).is_some());
    assert!(symbol_tree.find_method(&Name::new("МОЯПРОЦЕДУРА")).is_some());
}

#[test]
fn test_symbol_tree_multi_file() {
    let mut db = RootDatabaseImpl::new();

    let mut file_set = FileSet::new();
    let file1 = FileId(0);
    let file2 = FileId(1);
    file_set.insert(file1, VfsPath::new("/module1.bsl"));
    file_set.insert(file2, VfsPath::new("/module2.bsl"));
    let source_root = SourceRoot::new_local(file_set);
    db.set_source_root(SourceRootId(0), source_root);
    db.set_file_source_root(file1, SourceRootId(0));
    db.set_file_source_root(file2, SourceRootId(0));

    db.set_file_text(file1, "Процедура Метод1() КонецПроцедуры");

    db.set_file_text(file2, "Функция Метод2() Экспорт КонецФункции");

    let module1 = ModuleId::new(file1);
    let tree1 = db.symbol_tree(module1);
    assert_eq!(tree1.methods().count(), 1);
    assert_eq!(tree1.exported_methods().count(), 0);

    let module2 = ModuleId::new(file2);
    let tree2 = db.symbol_tree(module2);
    assert_eq!(tree2.methods().count(), 1);
    assert_eq!(tree2.exported_methods().count(), 1);
}

#[test]
fn test_resolver_resolve_module_method() {
    use hir::Resolver;
    use hir::{ModuleId, Name};

    let mut db = RootDatabaseImpl::new();
    let file_id = FileId(0);
    let module_id = ModuleId::new(file_id);

    let mut file_set = FileSet::new();
    file_set.insert(file_id, VfsPath::new("/test.bsl"));
    let source_root = SourceRoot::new_local(file_set);
    db.set_source_root(SourceRootId(0), source_root);
    db.set_file_source_root(file_id, SourceRootId(0));

    db.set_file_text(
        file_id,
        r#"
Процедура МояПроцедура()
КонецПроцедуры

Функция МояФункция() Экспорт
КонецФункции
        "#,
    );

    let resolver = Resolver::for_module(module_id);

    let method_id = resolver.resolve_module_method(&db, &Name::new("МояПроцедура"));
    assert!(method_id.is_some());
    assert_eq!(method_id.unwrap().module, module_id);

    let method_id = resolver.resolve_module_method(&db, &Name::new("МояФункция"));
    assert!(method_id.is_some());
    assert_eq!(method_id.unwrap().module, module_id);

    let method_id = resolver.resolve_module_method(&db, &Name::new("НеСуществует"));
    assert!(method_id.is_none());
}

#[test]
fn test_resolver_resolve_module_method_case_insensitive() {
    use hir::Resolver;
    use hir::{ModuleId, Name};

    let mut db = RootDatabaseImpl::new();
    let file_id = FileId(0);
    let module_id = ModuleId::new(file_id);

    let mut file_set = FileSet::new();
    file_set.insert(file_id, VfsPath::new("/test.bsl"));
    let source_root = SourceRoot::new_local(file_set);
    db.set_source_root(SourceRootId(0), source_root);
    db.set_file_source_root(file_id, SourceRootId(0));

    db.set_file_text(file_id, "Процедура МояПроцедура() КонецПроцедуры");

    let resolver = Resolver::for_module(module_id);

    assert!(resolver.resolve_module_method(&db, &Name::new("МояПроцедура")).is_some());
    assert!(resolver.resolve_module_method(&db, &Name::new("мояпроцедура")).is_some());
    assert!(resolver.resolve_module_method(&db, &Name::new("МОЯПРОЦЕДУРА")).is_some());
}

#[test]
fn test_resolver_resolve_module_variable() {
    use hir::Resolver;
    use hir::{ModuleId, Name};

    let mut db = RootDatabaseImpl::new();
    let file_id = FileId(0);
    let module_id = ModuleId::new(file_id);

    let mut file_set = FileSet::new();
    file_set.insert(file_id, VfsPath::new("/test.bsl"));
    let source_root = SourceRoot::new_local(file_set);
    db.set_source_root(SourceRootId(0), source_root);
    db.set_file_source_root(file_id, SourceRootId(0));

    db.set_file_text(file_id, "Перем МодульнаяПеременная Экспорт;");

    let resolver = Resolver::for_module(module_id);

    let var_id = resolver.resolve_module_variable(&db, &Name::new("МодульнаяПеременная"));
    assert!(var_id.is_some());
    assert_eq!(var_id.unwrap().module, module_id);

    let var_id = resolver.resolve_module_variable(&db, &Name::new("НеСуществует"));
    assert!(var_id.is_none());
}

#[test]
fn test_resolver_resolve_name_hierarchy() {
    use hir::ExprScopes;
    use hir::{ModuleId, Name};
    use hir::{Resolution, Resolver};

    let mut db = RootDatabaseImpl::new();
    let file_id = FileId(0);
    let module_id = ModuleId::new(file_id);

    let mut file_set = FileSet::new();
    file_set.insert(file_id, VfsPath::new("/test.bsl"));
    let source_root = SourceRoot::new_local(file_set);
    db.set_source_root(SourceRootId(0), source_root);
    db.set_file_source_root(file_id, SourceRootId(0));

    db.set_file_text(
        file_id,
        r#"
Процедура Метод()
КонецПроцедуры

Перем Переменная;
        "#,
    );

    let mut expr_scopes = ExprScopes::new();
    expr_scopes.add_parameter(Name::new("Параметр"));

    let root_scope = expr_scopes.root_scope();
    let resolver =
        Resolver::for_module(module_id).push_expr_scope(Arc::new(expr_scopes), root_scope);

    let resolved = resolver.resolve_name(&db, &Name::new("Параметр"));
    assert!(matches!(resolved, Some(Resolution::Local(_))));

    let resolved = resolver.resolve_name(&db, &Name::new("Метод"));
    assert!(matches!(resolved, Some(Resolution::Method(_))));

    let resolved = resolver.resolve_name(&db, &Name::new("Переменная"));
    assert!(matches!(resolved, Some(Resolution::Variable(_))));

    let resolved = resolver.resolve_name(&db, &Name::new("НеСуществует"));
    assert!(resolved.is_none());
}

#[test]
fn test_resolver_shadowing_local_over_module() {
    use hir::ExprScopes;
    use hir::{ModuleId, Name};
    use hir::{Resolution, Resolver};

    let mut db = RootDatabaseImpl::new();
    let file_id = FileId(0);
    let module_id = ModuleId::new(file_id);

    let mut file_set = FileSet::new();
    file_set.insert(file_id, VfsPath::new("/test.bsl"));
    let source_root = SourceRoot::new_local(file_set);
    db.set_source_root(SourceRootId(0), source_root);
    db.set_file_source_root(file_id, SourceRootId(0));

    db.set_file_text(file_id, "Перем Значение;");

    let mut expr_scopes = ExprScopes::new();
    expr_scopes.add_local_variable(expr_scopes.root_scope(), Name::new("Значение"));

    let root_scope = expr_scopes.root_scope();
    let resolver =
        Resolver::for_module(module_id).push_expr_scope(Arc::new(expr_scopes), root_scope);

    let resolved = resolver.resolve_name(&db, &Name::new("Значение"));
    assert!(matches!(resolved, Some(Resolution::Local(_))));
}

#[test]
fn test_resolver_with_workspace_scope() {
    use hir::ModuleId;
    use hir::Resolver;

    let file_id = FileId(0);
    let module_id = ModuleId::new(file_id);

    let resolver = Resolver::with_workspace_scope(module_id);

    assert_eq!(resolver.scopes.len(), 2);
}

#[test]
fn test_resolver_cross_module_gated_by_configurations() {
    use hir::{ModuleId, Name, PathResolution, QualifiedName, Resolver};

    let mut db = RootDatabaseImpl::new();
    let test_file = FileId(0);
    let om_file = FileId(1);

    let mut file_set = FileSet::new();
    file_set.insert(test_file, VfsPath::new("/test.bsl"));
    file_set.insert(om_file, VfsPath::new("/CommonModules/ОбщегоНазначения/Ext/Module.bsl"));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    db.set_file_source_root(test_file, SourceRootId(0));
    db.set_file_source_root(om_file, SourceRootId(0));

    db.set_file_text(test_file, "Процедура Тест() КонецПроцедуры");
    db.set_file_text(om_file, "Функция ПолучитьЗначение() Экспорт\n    Возврат 1;\nКонецФункции");

    let resolver = Resolver::with_workspace_scope(ModuleId::new(test_file));
    let path = QualifiedName::from_segments([
        Name::new("ОбщегоНазначения"),
        Name::new("ПолучитьЗначение"),
    ]);
    let before = resolver.resolve_path(&db, &path);
    assert!(
        matches!(before, PathResolution::Method(_)),
        "baseline: empty-config fallback must still resolve path-based lookup, got {:?}",
        before
    );

    db.set_all_config_paths(vec![(None, std::path::PathBuf::from("/does-not-exist"))]);

    let after = resolver.resolve_path(&db, &path);
    assert!(
        matches!(after, PathResolution::Unresolved(_)),
        "with a config registered but no matching declaration, resolution must fail, got {:?}",
        after
    );
}

#[test]
fn resolve_register_by_name_maps_to_configured_mdo_type() {
    use bsl_metadata::MdoType;
    use hir::{ModuleId, Name, Resolver};

    let mut db = RootDatabaseImpl::new();
    let file = FileId(0);
    let mut file_set = FileSet::new();
    file_set.insert(file, VfsPath::new("/Documents/Док/Ext/ObjectModule.bsl"));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    db.set_file_source_root(file, SourceRootId(0));
    db.set_file_text(file, "Процедура ОбработкаПроведения() КонецПроцедуры");

    let config_path =
        concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-metadata/fixtures/designer").to_string();
    db.set_all_config_paths(vec![(None, std::path::PathBuf::from(config_path))]);

    let resolver = Resolver::with_workspace_scope(ModuleId::new(file));

    // The movement syntax carries only the register name; the config supplies the type.
    assert_eq!(
        resolver.resolve_register_by_name(&db, &Name::new("РегистрСведений1")),
        Some((MdoType::InformationRegister, Name::new("РегистрСведений1"))),
        "a register name must resolve to its configured metadata type"
    );
    // BSL is case-insensitive.
    assert!(resolver.resolve_register_by_name(&db, &Name::new("регистрсведений1")).is_some());
    // An unknown name is surfaced as unresolved rather than guessed.
    assert_eq!(resolver.resolve_register_by_name(&db, &Name::new("НетТакогоРегистра")), None);
}

#[test]
fn test_all_sdbl_in_file_basic() {
    let mut db = RootDatabaseImpl::new();
    let file_id = FileId(0);

    let mut file_set = FileSet::new();
    file_set.insert(file_id, VfsPath::new("/test.bsl"));
    let source_root = SourceRoot::new_local(file_set);
    db.set_source_root(SourceRootId(0), source_root);
    db.set_file_source_root(file_id, SourceRootId(0));

    db.set_file_text(
        file_id,
        r#"Процедура Тест()
    Запрос = "ВЫБРАТЬ Код ИЗ Справочник.Товары";
КонецПроцедуры"#,
    );

    let queries = db.all_sdbl_in_file(file_id);
    assert_eq!(queries.len(), 1, "Should extract 1 SDBL query");
    assert!(queries[0].1.is_valid(), "SDBL should parse successfully");

    db.set_file_text(
        file_id,
        r#"Процедура Тест()
    Запрос1 = "ВЫБРАТЬ Код ИЗ Справочник.Товары";
    Запрос2 = "ВЫБРАТЬ Наименование ИЗ Справочник.Категории";
КонецПроцедуры"#,
    );

    let queries = db.all_sdbl_in_file(file_id);
    assert_eq!(queries.len(), 2, "Should extract 2 SDBL queries");
    assert!(queries.iter().all(|(_, q)| q.is_valid()));
}

#[test]
fn sdbl_hir_resolves_tables_via_per_mdo_accessors() {
    use sdbl_hir::ResolvedTable;

    fn db_with_query(with_config: bool) -> (RootDatabaseImpl, FileId) {
        let mut db = RootDatabaseImpl::new();
        let file_id = FileId(0);
        let mut file_set = FileSet::new();
        file_set.insert(file_id, VfsPath::new("/test.bsl"));
        db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
        db.set_file_source_root(file_id, SourceRootId(0));
        db.set_file_text(
            file_id,
            r#"Процедура Тест()
    Запрос = "ВЫБРАТЬ Код ИЗ Справочник.Справочник1";
КонецПроцедуры"#,
        );
        if with_config {
            let config_path =
                concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-metadata/fixtures/designer");
            db.set_all_config_paths(vec![(None, std::path::PathBuf::from(config_path))]);
        }
        (db, file_id)
    }

    fn from_table_fields(entries: &crate::SdblHirEntries) -> Vec<String> {
        let package = &entries.first().expect("one SDBL package").1;
        let query = package.queries().first().expect("one query");
        let table = query.hir.from.first().expect("one FROM table");
        match &table.metadata {
            Some(ResolvedTable::Metadata { fields, .. }) => {
                fields.iter().map(|f| f.name.clone()).collect()
            }
            other => panic!("FROM table must resolve to a metadata table, got {other:?}"),
        }
    }

    // With a config root the db-backed resolver resolves Справочник1 through the
    // per-MDO accessor, so the FROM table carries its declared attributes.
    let (db, file_id) = db_with_query(true);
    let fields = from_table_fields(&db.sdbl_hir_in_file(file_id));
    assert!(
        fields.iter().any(|f| f == "Реквизит1"),
        "resolved table must expose its declared attribute, got: {fields:?}"
    );

    // Without any config root the resolver is gated off (has_config_root == false),
    // so lowering resolves no fields — preserving the pre-narrowing contract that a
    // standalone module with no config does not validate query tables.
    let (db, file_id) = db_with_query(false);
    let fields = from_table_fields(&db.sdbl_hir_in_file(file_id));
    assert!(
        fields.is_empty(),
        "without a config root the table must carry no resolved fields, got: {fields:?}"
    );
}

#[test]
fn test_all_sdbl_in_file_keyword_filter() {
    let mut db = RootDatabaseImpl::new();
    let file_id = FileId(0);

    let mut file_set = FileSet::new();
    file_set.insert(file_id, VfsPath::new("/test.bsl"));
    let source_root = SourceRoot::new_local(file_set);
    db.set_source_root(SourceRootId(0), source_root);
    db.set_file_source_root(file_id, SourceRootId(0));

    db.set_file_text(
        file_id,
        r#"Процедура Тест()
    Строка = "Это просто строка без ключевых слов";
    Запрос = "ВЫБРАТЬ * ИЗ Справочник.Товары";
КонецПроцедуры"#,
    );

    let queries = db.all_sdbl_in_file(file_id);
    assert_eq!(queries.len(), 1, "Should filter by SELECT/ВЫБРАТЬ keyword");
    assert!(queries[0].1.query_text.contains("ВЫБРАТЬ"));
}

#[test]
fn test_all_sdbl_in_file_multiline() {
    let mut db = RootDatabaseImpl::new();
    let file_id = FileId(0);

    let mut file_set = FileSet::new();
    file_set.insert(file_id, VfsPath::new("/test.bsl"));
    let source_root = SourceRoot::new_local(file_set);
    db.set_source_root(SourceRootId(0), source_root);
    db.set_file_source_root(file_id, SourceRootId(0));

    db.set_file_text(
        file_id,
        r#"Процедура Тест()
    Запрос = "ВЫБРАТЬ
             |    Ссылка,
             |    Наименование
             |ИЗ Справочник.Товары";
КонецПроцедуры"#,
    );

    let queries = db.all_sdbl_in_file(file_id);
    assert_eq!(queries.len(), 1, "Should extract multiline SDBL query");
    assert!(queries[0].1.is_valid(), "Multiline query should parse successfully");

    let query_text = &queries[0].1.query_text;
    assert!(query_text.contains("Ссылка"));
    assert!(query_text.contains("Наименование"));
    assert!(query_text.contains("Справочник.Товары"));
}

#[test]
fn test_all_sdbl_in_file_assignment_patterns() {
    let mut db = RootDatabaseImpl::new();
    let file_id = FileId(0);

    let mut file_set = FileSet::new();
    file_set.insert(file_id, VfsPath::new("/test.bsl"));
    let source_root = SourceRoot::new_local(file_set);
    db.set_source_root(SourceRootId(0), source_root);
    db.set_file_source_root(file_id, SourceRootId(0));

    db.set_file_text(
        file_id,
        r#"Процедура Тест()
    // Direct assignment
    Запрос1 = "ВЫБРАТЬ * ИЗ Справочник.Товары";

    // Assignment in method call
    Результат = ВыполнитьЗапрос("ВЫБРАТЬ * ИЗ Документ.Продажа");

    // Assignment in array
    Массив = Новый Массив();
    Массив.Добавить("ВЫБРАТЬ * ИЗ Регистр.Остатки");
КонецПроцедуры"#,
    );

    let queries = db.all_sdbl_in_file(file_id);
    assert_eq!(queries.len(), 3, "Should extract queries from various contexts");

    for (_, query_info) in queries.iter() {
        assert!(query_info.is_valid(), "All queries should parse successfully");
    }
}

#[test]
fn test_all_sdbl_in_file_with_parameters() {
    let mut db = RootDatabaseImpl::new();
    let file_id = FileId(0);

    let mut file_set = FileSet::new();
    file_set.insert(file_id, VfsPath::new("/test.bsl"));
    let source_root = SourceRoot::new_local(file_set);
    db.set_source_root(SourceRootId(0), source_root);
    db.set_file_source_root(file_id, SourceRootId(0));

    db.set_file_text(
        file_id,
        r#"Процедура ПолучитьДанные()
    Запрос = "ВЫБРАТЬ
             |    Ссылка,
             |    Наименование
             |ИЗ Справочник.Товары
             |ГДЕ
             |    Код = &Значение1
             |    И Наименование ПОДОБНО &Значение2
             |    И Родитель = &Значение3";
КонецПроцедуры"#,
    );

    let queries = db.all_sdbl_in_file(file_id);

    assert_eq!(queries.len(), 1, "Should extract query with parameters");

    assert!(queries[0].1.is_valid(), "Query with parameters should parse successfully");

    assert!(queries[0].1.query_text.contains("&Значение1"));
    assert!(queries[0].1.query_text.contains("&Значение2"));
    assert!(queries[0].1.query_text.contains("&Значение3"));
}

#[test]
fn test_module_metadata_creation() {
    let mut db = RootDatabaseImpl::new();
    let file_id = FileId(0);

    let mut file_set = FileSet::new();
    file_set.insert(file_id, VfsPath::new("/CommonModules/ОбщегоНазначения/Ext/Module.bsl"));
    let source_root = SourceRoot::new_local(file_set);
    db.set_source_root(SourceRootId(0), source_root);
    db.set_file_source_root(file_id, SourceRootId(0));

    db.set_file_text(file_id, "Процедура Тест() КонецПроцедуры");

    let module_id = ModuleId::new(file_id);
    let metadata = db.module_metadata(module_id);

    assert_eq!(
        metadata.module_type,
        bsl_metadata::ModuleType::CommonModule,
        "Should detect CommonModule type from path"
    );
}

#[test]
fn test_module_bodies_and_metadata_separate() {
    let mut db = RootDatabaseImpl::new();
    let file_id = FileId(0);

    let mut file_set = FileSet::new();
    file_set.insert(file_id, VfsPath::new("/test.bsl"));
    let source_root = SourceRoot::new_local(file_set);
    db.set_source_root(SourceRootId(0), source_root);
    db.set_file_source_root(file_id, SourceRootId(0));

    db.set_file_text(file_id, "Процедура Тест() КонецПроцедуры");

    let module_id = ModuleId::new(file_id);
    let _module_bodies = db.module_bodies(module_id);
    let _module_metadata = db.module_metadata(module_id);
}

#[test]
fn test_module_metadata_cache_invalidation() {
    let mut db = RootDatabaseImpl::new();
    let file_id = FileId(0);

    let mut file_set = FileSet::new();
    file_set.insert(file_id, VfsPath::new("/test.bsl"));
    let source_root = SourceRoot::new_local(file_set);
    db.set_source_root(SourceRootId(0), source_root);
    db.set_file_source_root(file_id, SourceRootId(0));

    db.set_file_text(file_id, "Процедура Тест() КонецПроцедуры");
    let module_id = ModuleId::new(file_id);
    let _metadata1 = db.module_metadata(module_id);

    db.set_file_text(file_id, "Процедура Тест2() КонецПроцедуры");
    let _metadata2 = db.module_metadata(module_id);
}

/// A metadata XML change under one config root must reload only that root's
/// configuration; a sibling root's loaded `Configuration` stays memoized.
#[test]
fn metadata_xml_change_invalidates_only_its_config_root() {
    fn write_catalog(dir: &std::path::Path, name: &str, uuid: &str) {
        std::fs::create_dir_all(dir.join("Catalogs")).unwrap();
        std::fs::write(
            dir.join(format!("Catalogs/{name}.xml")),
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <Catalog uuid="{uuid}">
        <Properties><Name>{name}</Name><CodeLength>9</CodeLength></Properties>
    </Catalog>
</MetaDataObject>"#,
            ),
        )
        .unwrap();
    }

    let temp = tempfile::tempdir().unwrap();
    let main_root = temp.path().join("src/cf");
    let ext_root = temp.path().join("src/cfe/X");
    std::fs::create_dir_all(&main_root).unwrap();
    std::fs::create_dir_all(&ext_root).unwrap();
    std::fs::write(main_root.join("Configuration.xml"), "<Configuration/>").unwrap();
    std::fs::write(ext_root.join("Configuration.xml"), "<Configuration/>").unwrap();
    write_catalog(&main_root, "Товары", "00000000-0000-0000-0000-000000000001");
    write_catalog(&ext_root, "ДопДанные", "00000000-0000-0000-0000-000000000002");

    let mut db = RootDatabaseImpl::new();
    db.set_all_config_paths(vec![
        (None, main_root.clone()),
        (Some("X".to_string()), ext_root.clone()),
    ]);

    let main_file = FileId(0);
    let ext_file = FileId(1);
    let mut file_set = FileSet::new();
    let main_path = main_root.join("CommonModules/М/Ext/Module.bsl");
    let ext_path = ext_root.join("CommonModules/М/Ext/Module.bsl");
    file_set.insert(main_file, VfsPath::new(main_path.to_string_lossy().as_ref()));
    file_set.insert(ext_file, VfsPath::new(ext_path.to_string_lossy().as_ref()));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    db.set_file_source_root(main_file, SourceRootId(0));
    db.set_file_source_root(ext_file, SourceRootId(0));
    db.set_file_text(main_file, "Процедура Т() КонецПроцедуры");
    db.set_file_text(ext_file, "Процедура Т() КонецПроцедуры");

    let main_before = db.get_configuration(main_file).expect("main config loads");
    let ext_before = db.get_configuration(ext_file).expect("ext config loads");
    assert_eq!(main_before.metadata_objects().len(), 1);

    // Add a second catalog to the MAIN root only, then bump that root.
    let new_xml = main_root.join("Catalogs/Услуги.xml");
    write_catalog(&main_root, "Услуги", "00000000-0000-0000-0000-000000000003");
    db.bump_config_for_path(&new_xml);

    let main_after = db.get_configuration(main_file).expect("main config reloads");
    let ext_after = db.get_configuration(ext_file).expect("ext config still loads");

    assert!(
        !Arc::ptr_eq(&main_before, &main_after),
        "main root's config must reload after its XML changed"
    );
    assert_eq!(
        main_after.metadata_objects().len(),
        2,
        "reloaded main config must reflect the added catalog"
    );
    assert!(
        Arc::ptr_eq(&ext_before, &ext_after),
        "sibling extension config must stay memoized — its XML did not change"
    );
}

/// End-to-end proof of the `&ИзменениеИКонтроль` effective merge: code inside a
/// `#Вставка` is analyzed against the EFFECTIVE module (base text with the marked
/// body spliced in), so a call to a base-module sibling resolves — no false
/// `UnresolvedMethodCall` — while a genuinely missing method is still flagged,
/// which proves the inserted code is really inferred rather than silently dropped.
#[test]
fn infer_effective_resolves_base_sibling_in_insertion() {
    let temp = tempfile::tempdir().unwrap();
    let main_root = temp.path().join("src/cf");
    let ext_root = temp.path().join("src/cfe/X");
    std::fs::create_dir_all(&main_root).unwrap();
    std::fs::create_dir_all(&ext_root).unwrap();

    let mut db = RootDatabaseImpl::new();
    db.set_all_config_paths(vec![
        (None, main_root.clone()),
        (Some("X".to_string()), ext_root.clone()),
    ]);

    let main_file = FileId(0);
    let ext_file = FileId(1);
    let mut file_set = FileSet::new();
    let main_path = main_root.join("CommonModules/М/Ext/Module.bsl");
    let ext_path = ext_root.join("CommonModules/М/Ext/Module.bsl");
    file_set.insert(main_file, VfsPath::new(main_path.to_string_lossy().as_ref()));
    file_set.insert(ext_file, VfsPath::new(ext_path.to_string_lossy().as_ref()));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    db.set_file_source_root(main_file, SourceRootId(0));
    db.set_file_source_root(ext_file, SourceRootId(0));

    db.set_file_text(
        main_file,
        "Функция Сосед() Экспорт\n\tВозврат 1;\nКонецФункции\n\
         \n\
         Функция Цель() Экспорт\n\tВозврат 0;\nКонецФункции",
    );
    db.set_file_text(
        ext_file,
        "&ИзменениеИКонтроль(\"Цель\")\n\
         Функция Расш1_Цель()\n\
         #Вставка\n\
         \tЗначение1 = Сосед();\n\
         \tЗначение2 = НетТакого();\n\
         #КонецВставки\n\
         \tВозврат 0;\n\
         КонецФункции",
    );

    let eid = hir::EffectiveModuleId::new(&db, main_file, ext_file);
    let result = hir::infer_effective(&db, eid);

    // `var_types` is keyed by the fold-lowered local name and only records non-unknown
    // RHS types. The base sibling `Сосед()` returns `1`, so `Значение1` must be typed
    // as Число — and that number type can ONLY come from resolving `Сосед()` against
    // the effective module's base sibling (no platform global is named `Сосед`). The
    // missing `НетТакого()` leaves `Значение2` absent, proving the inserted code is
    // genuinely inferred rather than dropped.
    use bsl_types::builders::Builders;
    use stdx::case::CaseExt;
    let number = db.number(None, None);

    assert_eq!(
        result.var_types.get(&"Значение1".fold_lower()).copied(),
        Some(number),
        "base sibling `Сосед()` (returns 1) called from `#Вставка` must resolve via the \
         effective module and type `Значение1` as Число; var_types = {:?}",
        result.var_types.keys().collect::<Vec<_>>()
    );
    assert!(
        !result.var_types.contains_key(&"Значение2".fold_lower()),
        "a missing method `НетТакого()` must stay unresolved (target untyped), proving \
         the inserted code is genuinely inferred, not dropped; var_types = {:?}",
        result.var_types.keys().collect::<Vec<_>>()
    );
}

/// [high] #2 regression: a `&ИзменениеИКонтроль` method's return type is inferred from
/// its CHANGED body, and a sibling that calls it in the effective module must see that
/// changed return — not the base body's. Base `Цель` returns a string; the extension
/// deletes that and inserts `Возврат 42`, so the effective `Цель` returns Число, and the
/// (verbatim-copied) caller's `Знач = Цель()` must type as Число. Without the two-pass
/// effective-return threading this would read the base string return.
#[test]
fn infer_effective_uses_changed_method_return_for_sibling_call() {
    let temp = tempfile::tempdir().unwrap();
    let main_root = temp.path().join("src/cf");
    let ext_root = temp.path().join("src/cfe/X");
    std::fs::create_dir_all(&main_root).unwrap();
    std::fs::create_dir_all(&ext_root).unwrap();

    let mut db = RootDatabaseImpl::new();
    db.set_all_config_paths(vec![
        (None, main_root.clone()),
        (Some("X".to_string()), ext_root.clone()),
    ]);

    let main_file = FileId(0);
    let ext_file = FileId(1);
    let mut file_set = FileSet::new();
    let main_path = main_root.join("CommonModules/М/Ext/Module.bsl");
    let ext_path = ext_root.join("CommonModules/М/Ext/Module.bsl");
    file_set.insert(main_file, VfsPath::new(main_path.to_string_lossy().as_ref()));
    file_set.insert(ext_file, VfsPath::new(ext_path.to_string_lossy().as_ref()));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    db.set_file_source_root(main_file, SourceRootId(0));
    db.set_file_source_root(ext_file, SourceRootId(0));

    // The caller is ALSO a change-and-validate method: `infer_effective` materialises only the
    // changed methods (a copied-base body's inference is never published — its diagnostics are
    // dropped by the `#Вставка` remap and the extension file shows only the changed methods), so
    // the sibling that observes the changed return must itself be one of them.
    db.set_file_text(
        main_file,
        "Функция Цель() Экспорт\n\tВозврат \"строка\";\nКонецФункции\n\
         \n\
         Функция Вызывающий() Экспорт\n\tВозврат 1;\nКонецФункции",
    );
    db.set_file_text(
        ext_file,
        "&ИзменениеИКонтроль(\"Цель\")\n\
         Функция Расш1_Цель()\n\
         #Удаление\n\
         \tВозврат \"строка\";\n\
         #КонецУдаления\n\
         #Вставка\n\
         \tВозврат 42;\n\
         #КонецВставки\n\
         КонецФункции\n\
         \n\
         &ИзменениеИКонтроль(\"Вызывающий\")\n\
         Функция Расш1_Вызывающий()\n\
         #Удаление\n\
         \tВозврат 1;\n\
         #КонецУдаления\n\
         #Вставка\n\
         \tРез = Цель();\n\
         \tВозврат Рез;\n\
         #КонецВставки\n\
         КонецФункции",
    );

    let eid = hir::EffectiveModuleId::new(&db, main_file, ext_file);
    let result = hir::infer_effective(&db, eid);

    use bsl_types::builders::Builders;
    use stdx::case::CaseExt;
    let number = db.number(None, None);

    assert_eq!(
        result.var_types.get(&"Рез".fold_lower()).copied(),
        Some(number),
        "the changed `Цель` returns Число (Возврат 42), so the changed sibling's `Рез = Цель()` \
         must type as Число via effective-return threading; var_types = {:?}",
        result.var_types
    );
}

#[test]
fn test_sdbl_hir_in_file_basic() {
    let mut db = RootDatabaseImpl::new();
    let file_id = FileId(0);

    let mut file_set = FileSet::new();
    file_set.insert(file_id, VfsPath::new("/test.bsl"));
    let source_root = SourceRoot::new_local(file_set);
    db.set_source_root(SourceRootId(0), source_root);
    db.set_file_source_root(file_id, SourceRootId(0));

    db.set_file_text(
        file_id,
        r#"Процедура Тест()
    Запрос = "ВЫБРАТЬ Код ИЗ Справочник.Товары";
КонецПроцедуры"#,
    );

    let hirs = db.sdbl_hir_in_file(file_id);
    assert_eq!(hirs.len(), 1, "Should have 1 SDBL HIR");

    let (_, sdbl_hir) = &hirs[0];
    assert!(!sdbl_hir.queries()[0].hir.from.is_empty(), "Should have FROM clause");
    assert_eq!(sdbl_hir.queries()[0].hir.from[0].full_name, "Справочник.Товары");
}

#[test]
fn sdbl_hir_for_extension_file_uses_base_configuration_standard_attributes() {
    let temp_dir = tempfile::tempdir().unwrap();
    let main_root = temp_dir.path().join("src/cf");
    let extension_root = temp_dir.path().join("src/cfe/BMS_RU_UT");
    std::fs::create_dir_all(main_root.join("Catalogs")).unwrap();
    std::fs::create_dir_all(extension_root.join("Catalogs")).unwrap();

    std::fs::write(main_root.join("Configuration.xml"), "<Configuration/>").unwrap();
    std::fs::write(extension_root.join("Configuration.xml"), "<Configuration/>").unwrap();
    std::fs::write(
        main_root.join("Catalogs/Номенклатура.xml"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <Catalog uuid="00000000-0000-0000-0000-000000000001">
        <Properties>
            <Name>Номенклатура</Name>
            <Hierarchical>true</Hierarchical>
            <CodeLength>9</CodeLength>
            <DescriptionLength>25</DescriptionLength>
        </Properties>
    </Catalog>
</MetaDataObject>"#,
    )
    .unwrap();
    std::fs::write(
        extension_root.join("Catalogs/Номенклатура.xml"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <Catalog uuid="00000000-0000-0000-0000-000000000002">
        <Properties>
            <ObjectBelonging>Adopted</ObjectBelonging>
            <ExtendedConfigurationObject>00000000-0000-0000-0000-000000000001</ExtendedConfigurationObject>
            <Name>Номенклатура</Name>
        </Properties>
    </Catalog>
</MetaDataObject>"#,
    )
    .unwrap();

    let mut db = RootDatabaseImpl::new();
    db.set_all_config_paths(vec![
        (None, main_root.clone()),
        (Some("BMS_RU_UT".to_string()), extension_root.clone()),
    ]);

    let file_id = FileId(0);
    let file_path = extension_root.join("CommonModules/Модуль/Ext/Module.bsl");
    let mut file_set = FileSet::new();
    file_set.insert(file_id, VfsPath::new(file_path.to_string_lossy().as_ref()));
    let source_root = SourceRoot::new_local(file_set);
    db.set_source_root(SourceRootId(0), source_root);
    db.set_file_source_root(file_id, SourceRootId(0));

    db.set_file_text(
        file_id,
        r#"Процедура Тест()
    Запрос = "ВЫБРАТЬ Номенклатура.Родитель КАК Родитель ИЗ Справочник.Номенклатура КАК Номенклатура";
КонецПроцедуры"#,
    );

    let hirs = db.sdbl_hir_in_file(file_id);
    assert_eq!(hirs.len(), 1, "Should have 1 SDBL HIR");

    let package = &hirs[0].1;
    let unresolved: Vec<_> = package
        .source_map
        .tokens_by_category(sdbl_hir::TokenCategory::UnresolvedFieldName)
        .iter()
        .map(|token| token.text.as_str())
        .collect();
    let resolved: Vec<_> = package
        .source_map
        .tokens_by_category(sdbl_hir::TokenCategory::FieldName)
        .iter()
        .map(|token| token.text.as_str())
        .collect();

    assert!(resolved.contains(&"Родитель"), "Родитель should resolve: {resolved:?}");
    assert!(!unresolved.contains(&"Родитель"), "Родитель must not be unresolved: {unresolved:?}");
}

#[test]
fn test_sdbl_hir_in_file_multiple_queries() {
    let mut db = RootDatabaseImpl::new();
    let file_id = FileId(0);

    let mut file_set = FileSet::new();
    file_set.insert(file_id, VfsPath::new("/test.bsl"));
    let source_root = SourceRoot::new_local(file_set);
    db.set_source_root(SourceRootId(0), source_root);
    db.set_file_source_root(file_id, SourceRootId(0));

    db.set_file_text(
        file_id,
        r#"Процедура Тест()
    Запрос1 = "ВЫБРАТЬ Код ИЗ Справочник.Товары";
    Запрос2 = "ВЫБРАТЬ Номер ИЗ Документ.РасходнаяНакладная";
КонецПроцедуры"#,
    );

    let hirs = db.sdbl_hir_in_file(file_id);
    assert_eq!(hirs.len(), 2, "Should have 2 SDBL HIRs");

    assert_eq!(hirs[0].1.queries()[0].hir.from[0].full_name, "Справочник.Товары");

    assert_eq!(hirs[1].1.queries()[0].hir.from[0].full_name, "Документ.РасходнаяНакладная");
}

#[test]
fn test_sdbl_hir_in_file_caching() {
    let mut db = RootDatabaseImpl::new();
    let file_id = FileId(0);

    let mut file_set = FileSet::new();
    file_set.insert(file_id, VfsPath::new("/test.bsl"));
    let source_root = SourceRoot::new_local(file_set);
    db.set_source_root(SourceRootId(0), source_root);
    db.set_file_source_root(file_id, SourceRootId(0));

    db.set_file_text(
        file_id,
        r#"Процедура Тест()
    Запрос = "ВЫБРАТЬ Код ИЗ Справочник.Товары";
КонецПроцедуры"#,
    );

    let hirs1 = db.sdbl_hir_in_file(file_id);

    let hirs2 = db.sdbl_hir_in_file(file_id);

    assert!(Arc::ptr_eq(&hirs1, &hirs2), "Should return cached result");
}

#[test]
fn test_sdbl_hir_in_file_invalidation() {
    let mut db = RootDatabaseImpl::new();
    let file_id = FileId(0);

    let mut file_set = FileSet::new();
    file_set.insert(file_id, VfsPath::new("/test.bsl"));
    let source_root = SourceRoot::new_local(file_set);
    db.set_source_root(SourceRootId(0), source_root);
    db.set_file_source_root(file_id, SourceRootId(0));

    db.set_file_text(
        file_id,
        r#"Процедура Тест()
    Запрос = "ВЫБРАТЬ Код ИЗ Справочник.Товары";
КонецПроцедуры"#,
    );
    let hirs1 = db.sdbl_hir_in_file(file_id);
    assert_eq!(hirs1[0].1.queries()[0].hir.from[0].full_name, "Справочник.Товары");

    db.set_file_text(
        file_id,
        r#"Процедура Тест()
    Запрос = "ВЫБРАТЬ Номер ИЗ Документ.Продажа";
КонецПроцедуры"#,
    );
    let hirs2 = db.sdbl_hir_in_file(file_id);

    assert!(!Arc::ptr_eq(&hirs1, &hirs2), "Should invalidate cache on file change");

    assert_eq!(hirs2[0].1.queries()[0].hir.from[0].full_name, "Документ.Продажа");
}

#[test]
fn test_resolved_module_summary_targets() {
    use hir::call_graph::{CallTarget, EdgeProvenance, ResolvedTarget};
    use hir::ConfigsDatabase;

    let mut db = RootDatabaseImpl::new();
    let caller = FileId(0);
    let utils = FileId(1);

    let mut file_set = FileSet::new();
    file_set.insert(caller, VfsPath::new("/src/CommonModules/Клиент/Ext/Module.bsl"));
    file_set.insert(utils, VfsPath::new("/src/CommonModules/Утилиты/Ext/Module.bsl"));
    let source_root = SourceRoot::new_local(file_set);
    db.set_source_root(SourceRootId(0), source_root);
    db.set_file_source_root(caller, SourceRootId(0));
    db.set_file_source_root(utils, SourceRootId(0));

    db.set_file_text(
        utils,
        "Функция ПроверитьИНН() Экспорт КонецФункции\n\
         Процедура Приватная() КонецПроцедуры",
    );
    db.set_file_text(
        caller,
        "Процедура ЛокальнаяЦель() Экспорт КонецПроцедуры\n\
         Процедура Главная() Экспорт\n\
         ЛокальнаяЦель();\n\
         Утилиты.ПроверитьИНН();\n\
         Утилиты.Приватная();\n\
         НетТакогоМодуля.Метод();\n\
         КонецПроцедуры",
    );

    let summary = db.resolved_module_summary(ModuleId::new(caller));
    let caller_module = ModuleId::new(caller);
    let utils_module = ModuleId::new(utils);

    let resolved: Vec<_> =
        summary.edges.iter().filter(|e| e.provenance == EdgeProvenance::Resolved).collect();
    assert_eq!(resolved.len(), 2, "local + exported-qualified call resolve");

    // Local call resolves to a method in the caller's own module.
    assert!(resolved.iter().any(|e| matches!(
        &e.target,
        ResolvedTarget::Method(m) if m.module == caller_module
    )));
    // Exported qualified call resolves to a method in the target common module.
    assert!(resolved.iter().any(|e| matches!(
        &e.target,
        ResolvedTarget::Method(m) if m.module == utils_module
    )));

    // Non-exported qualified target is visible but unreachable across modules,
    // and the original target payload is preserved (surfaced, not dropped).
    let blocked: Vec<_> = summary
        .edges
        .iter()
        .filter(|e| e.provenance == EdgeProvenance::VisibilityBlocked)
        .collect();
    assert_eq!(blocked.len(), 1);
    assert!(matches!(
        &blocked[0].target,
        ResolvedTarget::Unresolved(CallTarget::QualifiedModule { method_name, .. })
            if method_name.as_str() == "Приватная"
    ));

    // Unknown module → honestly surfaced as unresolved with its original name preserved.
    let unresolved: Vec<_> =
        summary.edges.iter().filter(|e| e.provenance == EdgeProvenance::Unresolved).collect();
    assert_eq!(unresolved.len(), 1);
    assert!(matches!(
        &unresolved[0].target,
        ResolvedTarget::Unresolved(CallTarget::QualifiedModule { module_name, .. })
            if module_name.as_str() == "НетТакогоМодуля"
    ));
}

#[test]
fn test_resolved_module_summary_manager_access() {
    use hir::call_graph::{EdgeProvenance, ResolvedTarget};
    use hir::ConfigsDatabase;

    let mut db = RootDatabaseImpl::new();
    let caller = FileId(0);
    let mgr = FileId(1);

    let mut file_set = FileSet::new();
    file_set.insert(caller, VfsPath::new("/src/CommonModules/Клиент/Ext/Module.bsl"));
    file_set.insert(mgr, VfsPath::new("/src/Catalogs/Контрагенты/Ext/ManagerModule.bsl"));
    let source_root = SourceRoot::new_local(file_set);
    db.set_source_root(SourceRootId(0), source_root);
    db.set_file_source_root(caller, SourceRootId(0));
    db.set_file_source_root(mgr, SourceRootId(0));

    db.set_file_text(
        mgr,
        "Функция НайтиПоИНН() Экспорт КонецФункции\n\
         Процедура Внутренняя() КонецПроцедуры",
    );
    db.set_file_text(
        caller,
        "Процедура Главная() Экспорт\n\
         Справочники.Контрагенты.НайтиПоИНН();\n\
         Справочники.Контрагенты.Внутренняя();\n\
         Справочники.Контрагенты.СоздатьЭлемент();\n\
         КонецПроцедуры",
    );

    let summary = db.resolved_module_summary(ModuleId::new(caller));
    let mgr_module = ModuleId::new(mgr);

    // A user-defined, exported manager-module method on a fully-literal path resolves to
    // its node with Resolved trust (the manager module is uniquely determined).
    assert!(
        summary.edges.iter().any(|e| e.provenance == EdgeProvenance::Resolved
            && matches!(&e.target, ResolvedTarget::Method(m) if m.module == mgr_module)),
        "Справочники.Контрагенты.НайтиПоИНН should resolve to the manager-module method"
    );
    // A non-exported manager-module method is visible but unreachable across modules.
    assert_eq!(
        summary.edges.iter().filter(|e| e.provenance == EdgeProvenance::VisibilityBlocked).count(),
        1,
        "Справочники.Контрагенты.Внутренняя is non-export → VisibilityBlocked"
    );
    // A platform creation method (СоздатьЭлемент) is not a user node — it touches
    // the metadata object, so it resolves to an Mdo target via a ManagerCreates edge.
    use bsl_metadata::MdoType;
    use hir::call_graph::EdgeKind;
    assert!(
        summary.edges.iter().any(|e| e.provenance == EdgeProvenance::Inferred
            && e.kind == EdgeKind::ManagerCreates
            && matches!(&e.target, ResolvedTarget::Mdo { mdo_type, object_name }
                if *mdo_type == MdoType::Catalog && object_name.as_str() == "Контрагенты")),
        "Платформенный СоздатьЭлемент should resolve to an Mdo node via manager_creates"
    );
}

#[test]
fn test_workspace_call_graph_callers_and_callees() {
    use hir::call_graph::{GraphNode, ResolvedTarget};
    use hir::ConfigsDatabase;

    let mut db = RootDatabaseImpl::new();
    let caller = FileId(0);
    let utils = FileId(1);

    let mut file_set = FileSet::new();
    file_set.insert(caller, VfsPath::new("/src/CommonModules/Клиент/Ext/Module.bsl"));
    file_set.insert(utils, VfsPath::new("/src/CommonModules/Утилиты/Ext/Module.bsl"));
    let source_root = SourceRoot::new_local(file_set);
    db.set_source_root(SourceRootId(0), source_root);
    db.set_file_source_root(caller, SourceRootId(0));
    db.set_file_source_root(utils, SourceRootId(0));

    db.set_file_text(utils, "Функция ПроверитьИНН() Экспорт КонецФункции");
    db.set_file_text(
        caller,
        "Процедура Главная() Экспорт\n\
         Утилиты.ПроверитьИНН();\n\
         КонецПроцедуры",
    );

    let caller_module = ModuleId::new(caller);
    let utils_module = ModuleId::new(utils);

    // Derive the resolved target MethodId without hardcoding a local_id.
    let caller_summary = db.resolved_module_summary(caller_module);
    let target = caller_summary
        .edges
        .iter()
        .find_map(|e| match &e.target {
            ResolvedTarget::Method(m) if m.module == utils_module => Some(*m),
            _ => None,
        })
        .expect("Утилиты.ПроверитьИНН should resolve");

    let graph = db.workspace_call_graph(SourceRootId(0));

    // Reverse adjacency: callers of the utils method include a method in the caller module.
    let callers = graph.callers(&GraphNode::Method(target));
    assert!(!callers.is_empty(), "utils method must have a caller");
    assert!(callers.iter().all(|e| e.to == GraphNode::Method(target)));
    assert!(callers
        .iter()
        .any(|e| matches!(e.from, GraphNode::Method(m) if m.module == caller_module)));

    // Forward adjacency: the caller node lists the utils method as a callee.
    let caller_node = match &callers[0].from {
        GraphNode::Method(_) => callers[0].from.clone(),
        other => panic!("expected a method caller, got {other:?}"),
    };
    let callees = graph.callees(&caller_node);
    assert!(callees.iter().any(|e| e.to == GraphNode::Method(target)));
}

#[test]
fn test_workspace_call_graph_module_code_and_multiple_callers() {
    use hir::call_graph::{GraphNode, ResolvedTarget};
    use hir::ConfigsDatabase;

    let mut db = RootDatabaseImpl::new();
    let caller = FileId(0);
    let utils = FileId(1);

    let mut file_set = FileSet::new();
    file_set.insert(caller, VfsPath::new("/src/CommonModules/Клиент/Ext/Module.bsl"));
    file_set.insert(utils, VfsPath::new("/src/CommonModules/Утилиты/Ext/Module.bsl"));
    let source_root = SourceRoot::new_local(file_set);
    db.set_source_root(SourceRootId(0), source_root);
    db.set_file_source_root(caller, SourceRootId(0));
    db.set_file_source_root(utils, SourceRootId(0));

    db.set_file_text(utils, "Функция Ц() Экспорт КонецФункции");
    // Two methods plus trailing module-body code all call the same target.
    db.set_file_text(
        caller,
        "Процедура П1() Экспорт\n\
         Утилиты.Ц();\n\
         КонецПроцедуры\n\
         Процедура П2() Экспорт\n\
         Утилиты.Ц();\n\
         КонецПроцедуры\n\
         Утилиты.Ц();",
    );

    let caller_module = ModuleId::new(caller);
    let utils_module = ModuleId::new(utils);

    let target = db
        .resolved_module_summary(caller_module)
        .edges
        .iter()
        .find_map(|e| match &e.target {
            ResolvedTarget::Method(m) if m.module == utils_module => Some(*m),
            _ => None,
        })
        .expect("Утилиты.Ц should resolve");

    let graph = db.workspace_call_graph(SourceRootId(0));
    let callers = graph.callers(&GraphNode::Method(target));

    assert_eq!(callers.len(), 3, "two methods + module-body code call the target");
    assert!(
        callers.iter().any(|e| e.from == GraphNode::ModuleCode(caller_module)),
        "module-body call is attributed to the ModuleCode node"
    );
    let method_callers = callers.iter().filter(|e| matches!(e.from, GraphNode::Method(_))).count();
    assert_eq!(method_callers, 2, "П1 and П2 are distinct method callers");

    // The callee is client-capable (default), so no edge — including the
    // ModuleCode caller — is a client→server crossing.
    assert!(callers.iter().all(|e| !e.crosses_client_to_server));
}

#[test]
fn project_batch_method_pairs_include_resolved_callbacks_and_exclude_non_methods() {
    use hir::graph_index::{project_batch_method_call_pairs, GraphIndex};

    const BASE_CALLER: &str = "Процедура Главная() Экспорт\n\
         ЛокальнаяЦель();\n\
         Сервер.Считать();\n\
         Справочники.Контрагенты.НайтиПоИНН();\n\
         Оп1 = Новый ОписаниеОповещения(\"ЛокальныйОбработчик\", ЭтотОбъект);\n\
         Оп2 = Новый ОписаниеОповещения(\"Считать\", Сервер);\n\
         ПодключитьОбработчикОжидания(\"ОбновитьЭкран\", 1);\n\
         КонецПроцедуры\n\
         Процедура ЛокальнаяЦель() Экспорт КонецПроцедуры\n\
         Процедура ЛокальныйОбработчик() Экспорт КонецПроцедуры\n\
         Процедура ОбновитьЭкран() Экспорт КонецПроцедуры\n\
         Процедура ОбработчикДействия() Экспорт КонецПроцедуры";
    const IGNORED_CALLER: &str = "Процедура Главная() Экспорт\n\
         ЛокальнаяЦель();\n\
         Сервер.Считать();\n\
         Справочники.Контрагенты.НайтиПоИНН();\n\
         Справочники.Номенклатура.СоздатьЭлемент();\n\
         Оп1 = Новый ОписаниеОповещения(\"ЛокальныйОбработчик\", ЭтотОбъект);\n\
         Оп2 = Новый ОписаниеОповещения(\"Считать\", Сервер);\n\
         ПодключитьОбработчикОжидания(\"ОбновитьЭкран\", 1);\n\
         Элементы.Кнопка.УстановитьДействие(\"Нажатие\", \"ОбработчикДействия\");\n\
         Запрос = Новый Запрос(\"ВЫБРАТЬ Ссылка ИЗ Справочник.Номенклатура\");\n\
         КонецПроцедуры\n\
         Процедура ЛокальнаяЦель() Экспорт КонецПроцедуры\n\
         Процедура ЛокальныйОбработчик() Экспорт КонецПроцедуры\n\
         Процедура ОбновитьЭкран() Экспорт КонецПроцедуры\n\
         Процедура ОбработчикДействия() Экспорт КонецПроцедуры\n\
         ЛокальнаяЦель();";
    let files = [
        ("/src/CommonModules/Клиент/Ext/Module.bsl", BASE_CALLER),
        ("/src/CommonModules/Сервер/Ext/Module.bsl", "Функция Считать() Экспорт КонецФункции"),
        (
            "/src/Catalogs/Контрагенты/Ext/ManagerModule.bsl",
            "Функция НайтиПоИНН() Экспорт КонецФункции",
        ),
    ];
    let client = FileId(0);
    let server = FileId(1);
    let manager = FileId(2);
    let modules = [ModuleId::new(client), ModuleId::new(server), ModuleId::new(manager)];
    let make_db = |caller: &str| {
        let mut db = RootDatabaseImpl::new();
        let mut file_set = FileSet::new();
        for (index, (path, _)) in files.iter().enumerate() {
            file_set.insert(FileId(index as u32), VfsPath::new(*path));
        }
        db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
        for (index, (_, text)) in files.iter().enumerate() {
            let file_id = FileId(index as u32);
            db.set_file_source_root(file_id, SourceRootId(0));
            db.set_file_text(file_id, if file_id == client { caller } else { text });
        }
        db
    };

    // Given: direct, qualified, manager, and callback calls in one caller module.
    let base = make_db(BASE_CALLER);
    let index = GraphIndex::build(&base, &modules);
    let pool = rayon::ThreadPoolBuilder::new().build().expect("projection pool");

    // When: the index-backed batch projection resolves the caller module.
    let pairs = project_batch_method_call_pairs(&pool, &base, &index, &modules);
    fn nth_method_key(db: &dyn hir::DefDatabase, file: FileId, n: usize) -> hir::MethodKey {
        let tree = db.item_tree(file);
        let key = tree.methods().nth(n).expect("the fixture declares that many methods").key();
        key
    }
    let main =
        hir::MethodId { module: ModuleId::new(client), local_id: nth_method_key(&base, client, 0) };

    // Then: every supported target is emitted once as a method pair.
    assert_eq!(
        pairs,
        vec![
            hir::MethodCallPair::new(
                main,
                hir::MethodId {
                    module: ModuleId::new(client),
                    local_id: nth_method_key(&base, client, 1)
                }
            ),
            hir::MethodCallPair::new(
                main,
                hir::MethodId {
                    module: ModuleId::new(client),
                    local_id: nth_method_key(&base, client, 2)
                }
            ),
            hir::MethodCallPair::new(
                main,
                hir::MethodId {
                    module: ModuleId::new(client),
                    local_id: nth_method_key(&base, client, 3)
                }
            ),
            hir::MethodCallPair::new(
                main,
                hir::MethodId {
                    module: ModuleId::new(server),
                    local_id: nth_method_key(&base, server, 0)
                }
            ),
            hir::MethodCallPair::new(
                main,
                hir::MethodId {
                    module: ModuleId::new(manager),
                    local_id: nth_method_key(&base, manager, 0)
                }
            ),
        ],
        "local, qualified, manager, notify, and idle-handler targets are method-only pairs",
    );

    // Given: the same calls plus module code, an MDO call, a query, and SetAction.
    let ignored = make_db(IGNORED_CALLER);
    assert_eq!(
        ignored.module_call_summary(ModuleId::new(client)).set_action_regs.len(),
        1,
        "the fixture must exercise SetAction extraction",
    );

    // When/Then: ignored graph domains cannot change the method-pair digest.
    assert_eq!(
        project_batch_method_call_pairs(&pool, &ignored, &index, &modules),
        pairs,
        "module code, MDO, query, and SetAction additions must not alter the method-pair digest",
    );
}

#[test]
fn test_workspace_call_graph_client_server_boundary() {
    use hir::call_graph::{GraphNode, ResolvedTarget};
    use hir::ConfigsDatabase;

    let mut db = RootDatabaseImpl::new();
    let caller = FileId(0);
    let utils = FileId(1);

    let mut file_set = FileSet::new();
    file_set.insert(caller, VfsPath::new("/src/CommonModules/Клиент/Ext/Module.bsl"));
    file_set.insert(utils, VfsPath::new("/src/CommonModules/Сервер/Ext/Module.bsl"));
    let source_root = SourceRoot::new_local(file_set);
    db.set_source_root(SourceRootId(0), source_root);
    db.set_file_source_root(caller, SourceRootId(0));
    db.set_file_source_root(utils, SourceRootId(0));

    db.set_file_text(
        utils,
        "&НаСервере\n\
         Функция СерверныйМетод() Экспорт КонецФункции\n\
         &НаКлиентеНаСервере\n\
         Функция Универсальный() Экспорт КонецФункции",
    );
    db.set_file_text(
        caller,
        "&НаКлиенте\n\
         Процедура Клиентский() Экспорт\n\
         Сервер.СерверныйМетод();\n\
         Сервер.Универсальный();\n\
         КонецПроцедуры",
    );

    let utils_module = ModuleId::new(utils);
    let resolve = |method: &str| {
        db.resolved_module_summary(ModuleId::new(caller))
            .edges
            .iter()
            .filter_map(|e| match &e.target {
                ResolvedTarget::Method(m) if m.module == utils_module => Some(*m),
                _ => None,
            })
            .find(|m| {
                db.symbol_tree(utils_module)
                    .find_method_by_id(*m)
                    .is_some_and(|s| s.name.as_str() == method)
            })
            .unwrap_or_else(|| panic!("Сервер.{method} should resolve"))
    };
    let server_method = resolve("СерверныйМетод");
    let universal = resolve("Универсальный");

    let graph = db.workspace_call_graph(SourceRootId(0));

    // Node dispatch is attached: the &НаСервере target is server-only.
    let dispatch = graph
        .dispatch(&GraphNode::Method(server_method))
        .expect("server method must have known dispatch");
    assert!(dispatch.is_server_only(), "&НаСервере method is server-only");

    // The client→server-only call is flagged as a boundary crossing.
    let server_callers = graph.callers(&GraphNode::Method(server_method));
    assert!(!server_callers.is_empty());
    assert!(
        server_callers.iter().all(|e| e.crosses_client_to_server),
        "&НаКлиенте → &НаСервере is a client→server roundtrip"
    );

    // A &НаКлиентеНаСервере callee is not server-only → NOT a boundary crossing.
    let universal_callers = graph.callers(&GraphNode::Method(universal));
    assert!(!universal_callers.is_empty());
    assert!(
        universal_callers.iter().all(|e| !e.crosses_client_to_server),
        "&НаКлиентеНаСервере callee is reachable on the client — no roundtrip"
    );
}

#[test]
fn test_workspace_call_graph_query_ref_links_method_to_mdo() {
    use bsl_metadata::MdoType;
    use hir::call_graph::{EdgeKind, EdgeProvenance, GraphNode};
    use hir::ConfigsDatabase;

    // The SDBL table must resolve against a configuration, so declare the catalog.
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("src/cf");
    std::fs::create_dir_all(root.join("Catalogs")).unwrap();
    std::fs::write(root.join("Configuration.xml"), "<Configuration/>").unwrap();
    std::fs::write(
        root.join("Catalogs/Номенклатура.xml"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <Catalog uuid="00000000-0000-0000-0000-000000000001">
        <Properties>
            <Name>Номенклатура</Name>
            <CodeLength>9</CodeLength>
        </Properties>
    </Catalog>
</MetaDataObject>"#,
    )
    .unwrap();

    let mut db = RootDatabaseImpl::new();
    db.set_all_config_paths(vec![(None, root.clone())]);

    let file_id = FileId(0);
    let file_path = root.join("CommonModules/Отчеты/Ext/Module.bsl");
    let mut file_set = FileSet::new();
    file_set.insert(file_id, VfsPath::new(file_path.to_string_lossy().as_ref()));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    db.set_file_source_root(file_id, SourceRootId(0));
    db.set_file_text(
        file_id,
        "Процедура Считать() Экспорт\n\
         Запрос = \"ВЫБРАТЬ Код ИЗ Справочник.Номенклатура\";\n\
         КонецПроцедуры",
    );

    let graph = db.workspace_call_graph(SourceRootId(0));
    let qref = graph
        .edges()
        .find(|e| e.kind == EdgeKind::QueryRef)
        .expect("the query reads Справочник.Номенклатура → one query_ref edge");
    assert!(matches!(&qref.from, GraphNode::Method(_)), "the reading method is the edge source");
    assert!(
        matches!(&qref.to, GraphNode::Mdo { mdo_type, object_name }
            if *mdo_type == MdoType::Catalog && object_name.as_str() == "Номенклатура"),
        "the edge targets the read object's Mdo node"
    );
    assert_eq!(qref.provenance, EdgeProvenance::Inferred);
    assert!(!qref.crosses_client_to_server);
}

/// Golden equivalence: building the whole-config graph through the resident
/// `GraphIndex` (the streaming-build path) must produce byte-for-byte the same
/// `WorkspaceCallGraph` as the monolithic Salsa fold, AND the same per-module
/// `ResolvedModuleSummary` (which carries the VisibilityBlocked/Unresolved
/// outcomes the graph itself drops).
///
/// No configuration is registered, so the visibility gate is a no-op and
/// resolution proceeds on the path-based module index alone — exactly like the
/// existing `test_resolved_module_summary_*` fixtures. This lets the calls
/// actually reach every resolution arm. Coverage is asserted explicitly (below)
/// so the equality is not silently vacuous.
#[test]
fn an_unread_manager_module_still_yields_a_platform_manager_edge() {
    use bsl_metadata::MdoType;
    use hir::call_graph::{EdgeProvenance, ResolvedTarget};
    use hir::graph_index::{resolve_module_summary_via_index, GraphIndex};
    use hir::ConfigsDatabase;

    // A manager call has an answer that does not depend on any body: the platform
    // manager member. So an unreadable manager module must leave that reading
    // intact — `to_mdo()` with `Inferred` — instead of erasing the edge into
    // `Unresolved` the way the qualified-module route does, because THAT route has
    // no body-independent answer to fall back on.
    //
    // Asserted on the INDEX-backed projection, and this is not a detail: the Salsa
    // fold maps EVERY resolution error to `to_mdo()`, unread included, so it holds
    // this invariant no matter what the index path does. A gate reading the Salsa
    // summary is green while the two paths disagree — which is exactly what the
    // first version of this test did.
    //
    // Not observable through the reference walk either, which reports files and not
    // provenance: a build that answered `Unresolved` here passes every gate in
    // `references_by_name`.
    let files: &[(&str, &str)] = &[
        (
            "/src/CommonModules/Вызывающий/Ext/Module.bsl",
            "Процедура Прогон() Экспорт\n\
             Справочники.Контрагенты.НайтиПоИНН();\n\
             КонецПроцедуры",
        ),
        (
            "/src/Catalogs/Контрагенты/Ext/ManagerModule.bsl",
            "Функция НайтиПоИНН() Экспорт КонецФункции",
        ),
    ];

    let mut db = RootDatabaseImpl::new();
    let mut file_set = FileSet::new();
    for (i, (path, _)) in files.iter().enumerate() {
        file_set.insert(FileId(i as u32), VfsPath::new(*path));
    }
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    for (i, (_, text)) in files.iter().enumerate() {
        let fid = FileId(i as u32);
        db.set_file_source_root(fid, SourceRootId(0));
        db.set_file_text(fid, text);
    }

    let caller = ModuleId::new(FileId(0));
    let modules = vec![caller, ModuleId::new(FileId(1))];
    let shape = |summary: &hir::ResolvedModuleSummary| {
        summary.edges.iter().map(|e| (e.target.clone(), e.provenance)).collect::<Vec<_>>()
    };

    // Control: while the manager body IS readable, the call resolves to its method.
    // Without this half every assertion below passes on a build that never resolves
    // manager calls at all.
    let readable = resolve_module_summary_via_index(&db, caller, &GraphIndex::build(&db, &modules));
    assert!(
        readable.edges.iter().any(|e| e.provenance == EdgeProvenance::Resolved
            && matches!(e.target, ResolvedTarget::Method(_))),
        "control: a readable manager module resolves the call to its method: {:?}",
        shape(&readable),
    );

    db.set_file_unreadable(FileId(1));

    let summary = resolve_module_summary_via_index(&db, caller, &GraphIndex::build(&db, &modules));
    assert!(
        summary.edges.iter().any(|e| e.provenance == EdgeProvenance::Inferred
            && matches!(&e.target, ResolvedTarget::Mdo { mdo_type, .. } if *mdo_type == MdoType::Catalog)),
        "an unread manager module keeps the platform reading: {:?}",
        shape(&summary),
    );
    assert!(
        !summary.edges.iter().any(|e| e.provenance == EdgeProvenance::Unresolved),
        "the edge must not be erased into Unresolved: {:?}",
        shape(&summary),
    );
    assert_eq!(
        shape(&summary),
        shape(&db.resolved_module_summary(caller)),
        "the index projection and the Salsa fold must agree on an unread manager module",
    );
}

#[test]
fn workspace_call_graph_via_index_matches_salsa_fold() {
    use bsl_metadata::MdoType;
    use hir::call_graph::{EdgeKind, EdgeProvenance, ResolvedTarget};
    use hir::graph_index::{
        resolve_module_summary_via_index, workspace_call_graph_via_index, GraphIndex,
    };
    use hir::ConfigsDatabase;

    let files: &[(&str, &str)] = &[
        (
            "/src/CommonModules/Клиент/Ext/Module.bsl",
            "&НаКлиенте\n\
             Процедура Главная() Экспорт\n\
             ЛокальнаяЦель();\n\
             Сервер.Считать();\n\
             Сервер.Приватная();\n\
             НетМодуля.Метод();\n\
             ЭтотОбъект.НетМетода();\n\
             Справочники.Контрагенты.НайтиПоИНН();\n\
             Справочники.Контрагенты.Внутренняя();\n\
             Справочники.Контрагенты.НетТакого();\n\
             Справочники.Номенклатура.СоздатьЭлемент();\n\
             Справочники.Номенклатура.НайтиПоКоду();\n\
             Оп1 = Новый ОписаниеОповещения(\"ЛокальныйОбработчик\", ЭтотОбъект);\n\
             Оп2 = Новый ОписаниеОповещения(\"Считать\", Сервер);\n\
             Оп3 = Новый ОписаниеОповещения(\"Приватная\", Сервер);\n\
             Оп4 = Новый ОписаниеОповещения(\"Что\", Объекты[0]);\n\
             ПодключитьОбработчикОжидания(\"ОбновитьЭкран\", 1, Истина);\n\
             КонецПроцедуры\n\
             &НаКлиенте\n\
             Процедура ЛокальнаяЦель() Экспорт КонецПроцедуры\n\
             &НаКлиенте\n\
             Процедура ЛокальныйОбработчик(Результат, Параметры) Экспорт КонецПроцедуры\n\
             &НаКлиенте\n\
             Процедура ОбновитьЭкран() Экспорт КонецПроцедуры",
        ),
        (
            "/src/CommonModules/Сервер/Ext/Module.bsl",
            "&НаСервере\n\
             Функция Считать() Экспорт КонецФункции\n\
             &НаСервере\n\
             Функция Приватная() КонецФункции",
        ),
        (
            "/src/Catalogs/Контрагенты/Ext/ManagerModule.bsl",
            "Функция НайтиПоИНН() Экспорт КонецФункции\n\
             Процедура Внутренняя() КонецПроцедуры",
        ),
    ];

    let mut db = RootDatabaseImpl::new();
    let mut file_set = FileSet::new();
    for (i, (path, _)) in files.iter().enumerate() {
        file_set.insert(FileId(i as u32), VfsPath::new(*path));
    }
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    for (i, (_, text)) in files.iter().enumerate() {
        let fid = FileId(i as u32);
        db.set_file_source_root(fid, SourceRootId(0));
        db.set_file_text(fid, text);
    }

    // Enumerate modules exactly as the fold does (same iteration order → same
    // edge insertion order, so the two graphs compare equal).
    let source_root = db.source_root_input(SourceRootId(0)).root(&db);
    let file_set = source_root.file_set();
    let modules: Vec<ModuleId> = source_root
        .iter()
        .filter(|&f| hir::is_bsl_source(file_set, f))
        .map(ModuleId::new)
        .collect();

    let salsa = db.workspace_call_graph(SourceRootId(0));
    let index = GraphIndex::build(&db, &modules);
    let via_index = workspace_call_graph_via_index(&db, &modules, &index);

    assert_eq!(via_index, *salsa, "index-backed graph must equal the Salsa fold");

    // Coverage: prove the caller's summary actually hits every resolution arm, so
    // the equality above is not vacuous. (The index path equals this summary by
    // the per-module assertion below, so reaching the arm here proves it there.)
    let caller = db.resolved_module_summary(ModuleId::new(FileId(0)));
    let has = |pred: &dyn Fn(&hir::ResolvedCallEdge) -> bool| caller.edges.iter().any(pred);
    assert!(
        has(&|e| e.provenance == EdgeProvenance::Resolved
            && matches!(e.target, ResolvedTarget::Method(_))),
        "local + exported-qualified → Resolved method"
    );
    assert!(
        caller.edges.iter().filter(|e| e.provenance == EdgeProvenance::VisibilityBlocked).count()
            >= 2,
        "non-exported qualified (Приватная) and manager (Внутренняя) → VisibilityBlocked"
    );
    assert!(
        has(&|e| e.provenance == EdgeProvenance::Unresolved),
        "unknown module / ThisObject method → Unresolved"
    );
    assert!(
        has(&|e| e.provenance == EdgeProvenance::Resolved
            && matches!(&e.target, ResolvedTarget::Method(m) if m.module == ModuleId::new(FileId(2)))),
        "exported manager-module method (НайтиПоИНН) on a literal path → Resolved method in the manager module"
    );
    assert!(
        has(&|e| e.kind == EdgeKind::ManagerCreates
            && matches!(&e.target, ResolvedTarget::Mdo { mdo_type, .. } if *mdo_type == MdoType::Catalog)),
        "platform СоздатьЭлемент on a manager-less object → Mdo + ManagerCreates"
    );
    assert!(
        has(&|e| e.kind == EdgeKind::ManagerAccess
            && matches!(e.target, ResolvedTarget::Mdo { .. })),
        "platform find / absent manager method → Mdo + ManagerAccess"
    );
    // String-dispatched callbacks: ЭтотОбъект handler + exported cross-module handler
    // both resolve to a NotifyRef method edge with StringResolved provenance.
    assert_eq!(
        caller
            .edges
            .iter()
            .filter(|e| e.kind == EdgeKind::NotifyRef
                && e.provenance == EdgeProvenance::StringResolved
                && matches!(e.target, ResolvedTarget::Method(_)))
            .count(),
        2,
        "ОписаниеОповещения(ЭтотОбъект) + ОписаниеОповещения(exported Сервер) → 2 NotifyRef methods"
    );
    assert!(
        has(&|e| e.kind == EdgeKind::IdleHandler
            && e.provenance == EdgeProvenance::StringResolved
            && matches!(e.target, ResolvedTarget::Method(_))),
        "ПодключитьОбработчикОжидания → IdleHandler method"
    );
    // The non-exported callback (Приватная) is surfaced VisibilityBlocked, never as an
    // edge; the unsupported receiver (Объекты[0]) yields nothing at all.
    assert!(
        !has(&|e| e.kind == EdgeKind::NotifyRef
            && matches!(&e.target, ResolvedTarget::Method(_))
            && e.provenance == EdgeProvenance::VisibilityBlocked),
        "VisibilityBlocked callbacks carry an Unresolved target, not a Method"
    );

    for &module in &modules {
        let salsa_summary = db.resolved_module_summary(module);
        let index_summary = resolve_module_summary_via_index(&db, module, &index);
        assert_eq!(
            index_summary, *salsa_summary,
            "per-module ResolvedModuleSummary must match for {module:?}"
        );
    }
}

fn method_call_digest_from_fold(
    graph: &hir::call_graph::WorkspaceCallGraph,
    encoder: &hir::graph_index::GraphRowEncoder<'_>,
) -> hir::MethodCallDigest {
    use hir::GraphNode;

    hir::MethodCallDigest::from_rows(graph.edges().filter_map(|edge| {
        let (GraphNode::Method(caller), GraphNode::Method(target)) = (&edge.from, &edge.to) else {
            return None;
        };
        Some((
            encoder.encode(&GraphNode::Method(*target)).0,
            encoder.encode(&GraphNode::Method(*caller)).0,
        ))
    }))
}

fn compact_call_hierarchy_index<DB: hir::ConfigsDatabase + Clone + Send>(
    db: &DB,
    modules: &[ModuleId],
    batch_size: usize,
) -> (hir::graph_index::GraphIndex, hir::CallHierarchyReverseIndex) {
    use hir::graph_index::{project_batch_method_call_pairs, GraphIndex};

    let graph_index = GraphIndex::build(db, modules);
    let pool = rayon::ThreadPoolBuilder::new().build().expect("projection pool");

    let mut reverse_index = hir::CallHierarchyReverseIndex::new();
    for batch in modules.chunks(batch_size.max(1)) {
        let pairs = project_batch_method_call_pairs(&pool, db, &graph_index, batch);
        for &module in batch {
            let layout_hash = graph_index
                .module_layout_hash(module)
                .expect("indexed fixture module has a layout hash");
            reverse_index.replace_module(
                module,
                pairs.iter().filter(|pair| pair.caller.module == module).copied(),
                layout_hash,
            );
        }
    }
    (graph_index, reverse_index)
}

#[test]
fn call_hierarchy_index_fixture_parity() {
    use hir::call_graph::{EdgeKind, GraphNode};
    use hir::graph_index::GraphRowEncoder;
    use hir::ConfigsDatabase;
    use rustc_hash::FxHashMap;

    // Given: all method-only call forms supported by the compact projection.
    let files: &[(FileId, &str, &str)] = &[
        (
            FileId(0),
            "/src/CommonModules/Вызыватель/Ext/Module.bsl",
            "Процедура Главная() Экспорт\n\
             ЛокальнаяЦель();\n\
             Общий.Метод();\n\
             Справочники.Товары.МетодМенеджера();\n\
             Новый ОписаниеОповещения(\"Оповещение\", ЭтотОбъект);\n\
             ПодключитьОбработчикОжидания(\"Ожидание\", 1);\n\
             УстановитьДействие(\"ПриНажатии\", \"Исключен\");\n\
             КонецПроцедуры\n\
             Процедура ЛокальнаяЦель() Экспорт КонецПроцедуры\n\
             Процедура Оповещение() Экспорт КонецПроцедуры\n\
             Процедура Ожидание() Экспорт КонецПроцедуры\n\
             Процедура Исключен() Экспорт КонецПроцедуры",
        ),
        (
            FileId(1),
            "/src/CommonModules/Общий/Ext/Module.bsl",
            "Процедура Метод() Экспорт КонецПроцедуры",
        ),
        (
            FileId(2),
            "/src/Catalogs/Товары/Ext/ManagerModule.bsl",
            "Процедура МетодМенеджера() Экспорт КонецПроцедуры",
        ),
    ];
    let source_root_id = SourceRootId(0);
    let mut db = RootDatabaseImpl::new();
    let mut file_set = FileSet::new();
    let mut paths = FxHashMap::default();
    let mut texts = FxHashMap::default();
    for &(file_id, path, text) in files {
        file_set.insert(file_id, VfsPath::new(path));
        paths.insert(file_id, path.to_string());
        texts.insert(file_id, text);
    }
    let source_root = SourceRoot::new_local(file_set);
    db.set_source_root(source_root_id, source_root.clone());
    for (&file_id, &text) in &texts {
        db.set_file_source_root(file_id, source_root_id);
        db.set_file_text(file_id, text);
    }
    let modules: Vec<_> = files.iter().map(|(file_id, _, _)| ModuleId::new(*file_id)).collect();

    // When: the compact batch projection builds the reverse index.
    let (graph_index, reverse_index) = compact_call_hierarchy_index(&db, &modules, 1);
    let compact = hir::call_hierarchy_method_digest(&reverse_index, &graph_index, &paths, None);
    let no_objects = hir::graph_index::MdoFiles::default();
    let encoder = GraphRowEncoder::new(&graph_index, &paths, None, &no_objects);
    let salsa = db.workspace_call_graph(source_root_id);
    let folded = method_call_digest_from_fold(&salsa, &encoder);

    // Then: every retained method edge agrees with the Salsa incoming graph.
    assert_eq!(compact, folded);
    assert_eq!(compact.len(), 5, "direct, qualified, manager, notify, and idle rows");
    let caller = ModuleId::new(FileId(0));
    assert!(salsa.edges().any(|edge| {
        edge.kind == EdgeKind::DirectLocal
            && matches!((&edge.from, &edge.to),
                (GraphNode::Method(from), GraphNode::Method(to)) if from.module == caller && to.module == caller)
    }));
    assert!(salsa.edges().any(|edge| {
        edge.kind == EdgeKind::DirectQualifiedModule
            && matches!((&edge.from, &edge.to),
                (GraphNode::Method(from), GraphNode::Method(to))
                    if from.module == caller && to.module == ModuleId::new(FileId(1)))
    }));
    assert!(salsa.edges().any(|edge| {
        matches!((&edge.from, &edge.to),
            (GraphNode::Method(from), GraphNode::Method(to))
                if from.module == caller && to.module == ModuleId::new(FileId(2)))
    }));
    assert!(salsa.edges().any(|edge| {
        edge.kind == EdgeKind::NotifyRef
            && matches!((&edge.from, &edge.to),
                (GraphNode::Method(from), GraphNode::Method(to)) if from.module == caller && to.module == caller)
    }));
    assert!(salsa.edges().any(|edge| {
        edge.kind == EdgeKind::IdleHandler
            && matches!((&edge.from, &edge.to),
                (GraphNode::Method(from), GraphNode::Method(to)) if from.module == caller && to.module == caller)
    }));
    assert!(
        !compact.rows().iter().any(|(_, caller_id)| caller_id.ends_with("/Исключен")),
        "SetAction registrations are not call-hierarchy method pairs"
    );
}

#[test]
fn call_hierarchy_index_selects_source_root_by_anchor_file() {
    use hir::MethodId;

    // Given: overlapping module names in deliberately separate BSL source roots.
    let root_a = SourceRootId(10);
    let root_b = SourceRootId(11);
    let files: &[(FileId, SourceRootId, &str, &str)] = &[
        (
            FileId(0),
            root_a,
            "/root-a/CommonModules/CallerA/Ext/Module.bsl",
            "Процедура Вызвать() Экспорт\nShared.Цель();\nКонецПроцедуры",
        ),
        (
            FileId(1),
            root_a,
            "/root-a/CommonModules/Shared/Ext/Module.bsl",
            "Процедура Цель() Экспорт КонецПроцедуры",
        ),
        (
            FileId(2),
            root_b,
            "/root-b/CommonModules/CallerB/Ext/Module.bsl",
            "Процедура Вызвать() Экспорт\nShared.Цель();\nКонецПроцедуры",
        ),
        (
            FileId(3),
            root_b,
            "/root-b/CommonModules/Shared/Ext/Module.bsl",
            "Процедура Цель() Экспорт КонецПроцедуры",
        ),
    ];
    let mut db = RootDatabaseImpl::new();
    let mut root_a_files = FileSet::new();
    let mut root_b_files = FileSet::new();
    for &(file_id, root, path, text) in files {
        match root {
            id if id == root_a => root_a_files.insert(file_id, VfsPath::new(path)),
            id if id == root_b => root_b_files.insert(file_id, VfsPath::new(path)),
            _ => unreachable!("fixture owns exactly two source roots"),
        }
        db.set_file_source_root(file_id, root);
        db.set_file_text(file_id, text);
    }
    db.set_source_root(root_a, SourceRoot::new_local(root_a_files));
    db.set_source_root(root_b, SourceRoot::new_local(root_b_files));
    let modules_a = [ModuleId::new(FileId(0)), ModuleId::new(FileId(1))];
    let modules_b = [ModuleId::new(FileId(2)), ModuleId::new(FileId(3))];

    // When: each root is built independently and selected from its anchor file.
    let (_, index_a) = compact_call_hierarchy_index(&db, &modules_a, 1);
    let (_, index_b) = compact_call_hierarchy_index(&db, &modules_b, 1);
    let indexes = std::collections::BTreeMap::from([(root_a, index_a), (root_b, index_b)]);
    let index_for = |anchor: FileId| {
        let root = db.file_source_root_input(anchor).source_root_id(&db);
        indexes.get(&root).expect("each anchor root has exactly one compact index")
    };

    // Then: independent roots never share reverse pairs or lifecycle layout state.
    let caller = hir::MethodKey::first("Вызвать");
    let target = hir::MethodKey::first("Цель");
    let caller_a = MethodId { module: ModuleId::new(FileId(0)), local_id: caller };
    let target_a = MethodId { module: ModuleId::new(FileId(1)), local_id: target };
    let caller_b = MethodId { module: ModuleId::new(FileId(2)), local_id: caller };
    let target_b = MethodId { module: ModuleId::new(FileId(3)), local_id: target };
    let selected_a = index_for(FileId(0));
    let selected_b = index_for(FileId(2));
    assert_eq!(selected_a.callers(target_a), &[caller_a]);
    assert_eq!(selected_b.callers(target_b), &[caller_b]);
    assert!(selected_a.callers(target_b).is_empty());
    assert!(selected_b.callers(target_a).is_empty());
    assert!(selected_a.layout_hash(target_b.module).is_none());
    assert!(selected_b.layout_hash(target_a.module).is_none());
}

#[test]
fn call_hierarchy_index_cfe_parity() {
    use hir::call_graph::GraphNode;
    use hir::graph_index::GraphRowEncoder;
    use hir::ConfigsDatabase;
    use rustc_hash::FxHashMap;
    use test_fixture::CfeFixtureBuilder;

    fn common_module_xml(name: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <CommonModule uuid="00000000-0000-0000-0000-000000000001">
        <Properties><Name>{name}</Name><Server>true</Server></Properties>
    </CommonModule>
</MetaDataObject>"#
        )
    }

    // Given: base and extension modules share one artificial BSL source root.
    let base_source = "Процедура Цель() Экспорт КонецПроцедуры";
    let extension_source = "Процедура Вызвать() Экспорт\nБазовыйApi.Цель();\nКонецПроцедуры";
    let mut builder = CfeFixtureBuilder::new("");
    builder.add_extension("Extension", "").add_extension_module(
        "Extension",
        "ExtensionCaller",
        extension_source,
    );
    let fixture = builder.build();
    let base_dir = fixture.root().join("CommonModules/БазовыйApi/Ext");
    std::fs::create_dir_all(&base_dir).expect("create base module directory");
    let base_path = base_dir.join("Module.bsl");
    std::fs::write(&base_path, base_source).expect("write base module");
    std::fs::write(
        fixture.root().join("CommonModules/БазовыйApi.xml"),
        common_module_xml("БазовыйApi"),
    )
    .expect("write base metadata");
    let extension = &fixture.extensions()[0];
    let extension_path = extension.modules()[0].path().to_path_buf();
    std::fs::write(
        extension.root().join("CommonModules/ExtensionCaller.xml"),
        common_module_xml("ExtensionCaller"),
    )
    .expect("write extension metadata");

    let source_root_id = SourceRootId(0);
    let base_file = FileId(0);
    let extension_file = FileId(1);
    let mut db = RootDatabaseImpl::new();
    let mut file_set = FileSet::new();
    file_set.insert(base_file, VfsPath::new(base_path.to_string_lossy().as_ref()));
    file_set.insert(extension_file, VfsPath::new(extension_path.to_string_lossy().as_ref()));
    let source_root = SourceRoot::new_local(file_set);
    db.set_source_root(source_root_id, source_root.clone());
    db.set_file_source_root(base_file, source_root_id);
    db.set_file_source_root(extension_file, source_root_id);
    db.set_file_text(base_file, base_source);
    db.set_file_text(extension_file, extension_source);
    let config_paths = fixture.config_paths();
    db.set_all_config_paths(config_paths.clone());
    let modules = [ModuleId::new(base_file), ModuleId::new(extension_file)];
    let mut paths = FxHashMap::default();
    paths.insert(base_file, base_path.to_string_lossy().into_owned());
    paths.insert(extension_file, extension_path.to_string_lossy().into_owned());

    // When: the compact index resolves the extension against the base module.
    let (graph_index, reverse_index) = compact_call_hierarchy_index(&db, &modules, 1);
    let compact = hir::call_hierarchy_method_digest(&reverse_index, &graph_index, &paths, None);
    let no_objects = hir::graph_index::MdoFiles::default();
    let encoder = GraphRowEncoder::new(&graph_index, &paths, None, &no_objects);
    let salsa = db.workspace_call_graph(source_root_id);
    let folded = method_call_digest_from_fold(&salsa, &encoder);

    // Then: CFE directory membership does not create a second Salsa source root.
    assert_eq!(
        db.file_source_root_input(base_file).source_root_id(&db),
        db.file_source_root_input(extension_file).source_root_id(&db)
    );
    assert_eq!(compact, folded);
    assert_eq!(compact.len(), 1);
    assert!(salsa.edges().any(|edge| {
        matches!(
            (&edge.from, &edge.to),
            (GraphNode::Method(from), GraphNode::Method(to))
                if from.module == ModuleId::new(extension_file) && to.module == ModuleId::new(base_file)
        )
    }));
}

fn cfe_common_module_xml(name: &str, global: bool) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <CommonModule uuid="00000000-0000-0000-0000-000000000002">
        <Properties><Name>{name}</Name><Server>true</Server><Global>{global}</Global></Properties>
    </CommonModule>
</MetaDataObject>"#
    )
}

/// Write one common module of a configuration root in the layout the whole-config
/// loader expects: a sibling XML next to a `<Имя>/Ext/Module.bsl` body.
fn write_cfe_common_module(
    root: &std::path::Path,
    name: &str,
    global: bool,
    source: &str,
) -> std::path::PathBuf {
    let modules_dir = root.join("CommonModules");
    let body_dir = modules_dir.join(name).join("Ext");
    std::fs::create_dir_all(&body_dir).expect("create CommonModule body directory");
    std::fs::write(modules_dir.join(format!("{name}.xml")), cfe_common_module_xml(name, global))
        .expect("write CommonModule metadata");
    let path = body_dir.join("Module.bsl");
    std::fs::write(&path, source).expect("write CommonModule body");
    path
}

/// A two-body common module `Сервер` (base + the caller's own extension), both bodies
/// declaring `П`, with a caller in the extension calling it both qualified and through a
/// string-dispatched callback. `base_unread` marks the base body's bytes unreadable.
///
/// Returns the database plus the module ids in `(base, extension body, caller)` order.
fn cfe_db_with_two_server_bodies(
    fixture: &test_fixture::CfeFixture,
    base_unread: bool,
) -> (RootDatabaseImpl, [ModuleId; 3]) {
    let base_path = write_cfe_common_module(
        fixture.root(),
        "Сервер",
        false,
        "Процедура П() Экспорт КонецПроцедуры",
    );
    let ext_root = fixture.extensions()[0].root().to_path_buf();
    let ext_server_path =
        write_cfe_common_module(&ext_root, "Сервер", false, "Процедура П() Экспорт КонецПроцедуры");
    let caller_source = "Процедура Т() Экспорт\n\
                         Сервер.П();\n\
                         Оп = Новый ОписаниеОповещения(\"П\", Сервер);\n\
                         КонецПроцедуры";
    let caller_path = write_cfe_common_module(&ext_root, "Вызов", false, caller_source);

    let (base, ext_server, caller) = (FileId(0), FileId(1), FileId(2));
    let mut db = RootDatabaseImpl::new();
    let mut file_set = FileSet::new();
    for (id, path) in [(base, &base_path), (ext_server, &ext_server_path), (caller, &caller_path)] {
        file_set.insert(id, VfsPath::new(path.to_string_lossy().as_ref()));
    }
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    for id in [base, ext_server, caller] {
        db.set_file_source_root(id, SourceRootId(0));
    }
    db.set_file_text(ext_server, "Процедура П() Экспорт КонецПроцедуры");
    db.set_file_text(caller, caller_source);
    if base_unread {
        db.set_file_unreadable(base);
    } else {
        db.set_file_text(base, "Процедура П() Экспорт КонецПроцедуры");
    }
    db.set_all_config_paths(fixture.config_paths());

    (db, [ModuleId::new(base), ModuleId::new(ext_server), ModuleId::new(caller)])
}

/// The compact index and the Salsa fold must answer the same thing about a call whose
/// callee module has an unread body ahead of a readable one. The unread base may well
/// declare the method, and its declaration would outrank the extension's — so neither
/// route may let the extension body answer in its place.
#[test]
fn graph_index_matches_the_salsa_fold_when_a_base_body_is_unread() {
    use hir::call_graph::{EdgeProvenance, ResolvedTarget};
    use hir::graph_index::{resolve_module_summary_via_index, GraphIndex};
    use hir::ConfigsDatabase;
    use test_fixture::CfeFixtureBuilder;

    // Positive control: with every body readable the same fixture resolves the call to
    // the BASE declaration. Without it the equality below could hold vacuously — two
    // routes agreeing that nothing resolves at all.
    let mut builder = CfeFixtureBuilder::new("");
    builder.add_extension("Расш", "");
    let readable_fixture = builder.build();
    let (readable_db, [base, _, caller]) = cfe_db_with_two_server_bodies(&readable_fixture, false);
    let control = readable_db.resolved_module_summary(caller);
    assert!(
        control.edges.iter().any(|e| e.provenance == EdgeProvenance::Resolved
            && matches!(&e.target, ResolvedTarget::Method(m) if m.module == base)),
        "control: a readable base body must win the call, else the fixture proves nothing"
    );

    let mut builder = CfeFixtureBuilder::new("");
    builder.add_extension("Расш", "");
    let fixture = builder.build();
    let (db, modules) = cfe_db_with_two_server_bodies(&fixture, true);
    let [_, ext_server, caller] = modules;
    let index = GraphIndex::build(&db, &modules);

    let salsa = db.resolved_module_summary(caller);
    assert!(
        !salsa
            .edges
            .iter()
            .any(|e| matches!(&e.target, ResolvedTarget::Method(m) if m.module == ext_server)),
        "a body behind an unread one must not answer for it"
    );
    for &module in &modules {
        assert_eq!(
            resolve_module_summary_via_index(&db, module, &index),
            *db.resolved_module_summary(module),
            "index and Salsa must agree about {module:?} when a base body is unread"
        );
    }
}

/// The bare-call surface of a GLOBAL common module is walked in the same priority order
/// as a qualified call, and stops at the same place: a body behind an unread one is not
/// the module's answer, so its exports must not enter the global context either.
#[test]
fn global_common_module_exports_stop_at_an_unread_body() {
    use test_fixture::CfeFixtureBuilder;

    fn exports_of(
        fixture: &test_fixture::CfeFixture,
        base_unread: bool,
        base_registered: bool,
    ) -> (Vec<(ModuleId, String, bool)>, bool) {
        let base_path = write_cfe_common_module(
            fixture.root(),
            "Глоб",
            true,
            "Функция П() Экспорт КонецФункции",
        );
        let ext_root = fixture.extensions()[0].root().to_path_buf();
        let ext_path =
            write_cfe_common_module(&ext_root, "Глоб", true, "Функция П() Экспорт КонецФункции");
        let caller_source = "Процедура Т() Экспорт КонецПроцедуры";
        let caller_path = write_cfe_common_module(&ext_root, "Вызов", false, caller_source);

        let (base, ext, caller) = (FileId(0), FileId(1), FileId(2));
        let mut db = RootDatabaseImpl::new();
        let mut file_set = FileSet::new();
        if base_registered {
            file_set.insert(base, VfsPath::new(base_path.to_string_lossy().as_ref()));
        }
        for (id, path) in [(ext, &ext_path), (caller, &caller_path)] {
            file_set.insert(id, VfsPath::new(path.to_string_lossy().as_ref()));
        }
        db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
        if base_registered {
            db.set_file_source_root(base, SourceRootId(0));
        }
        for id in [ext, caller] {
            db.set_file_source_root(id, SourceRootId(0));
        }
        db.set_file_text(ext, "Функция П() Экспорт КонецФункции");
        db.set_file_text(caller, caller_source);
        if base_registered {
            if base_unread {
                db.set_file_unreadable(base);
            } else {
                db.set_file_text(base, "Функция П() Экспорт КонецФункции");
            }
        }
        db.set_all_config_paths(fixture.config_paths());

        let surface = hir::Resolver::with_workspace_scope(ModuleId::new(caller))
            .global_common_module_exports(&db);
        let complete = surface.is_complete();
        let entries = surface
            .entries
            .into_iter()
            .map(|entry| match entry.definition {
                hir::GlobalExportDefinition::Method(id) => {
                    (id.module, entry.name.as_str().to_string(), true)
                }
                hir::GlobalExportDefinition::Variable(id) => {
                    (id.module, entry.name.as_str().to_string(), false)
                }
            })
            .collect();
        (entries, complete)
    }

    let mut builder = CfeFixtureBuilder::new("");
    builder.add_extension("Расш", "");
    let readable_fixture = builder.build();
    let (control, complete) = exports_of(&readable_fixture, false, true);
    assert!(complete, "every readable candidate makes the common-module surface complete");
    assert!(
        control.iter().any(|(module, name, is_method)| {
            *module == ModuleId::new(FileId(0)) && name == "П" && *is_method
        }),
        "control: a readable global base body must export method П"
    );
    let mut builder = CfeFixtureBuilder::new("");
    builder.add_extension("Расш", "");
    let fixture = builder.build();
    let (exports, complete) = exports_of(&fixture, true, true);
    assert!(!complete, "an unread body must leave an explicit completeness gap");
    assert!(
        !exports.iter().any(|(_, name, _)| name == "П"),
        "an unread base body bars its own symbols from the global context: {exports:?}"
    );

    let mut builder = CfeFixtureBuilder::new("");
    builder.add_extension("Расш", "");
    let fixture = builder.build();
    let (_, complete) = exports_of(&fixture, false, false);
    assert!(
        !complete,
        "a metadata-declared global module without a locatable body is a surface gap"
    );
}

#[test]
fn application_module_exports_are_typed_and_preserve_unread_gaps() {
    use hir::{GlobalExportDefinition, GlobalExportHost, GlobalSurfaceGap};
    use test_fixture::CfeFixtureBuilder;

    fn surface(kind: hir::ApplicationModuleKind, unread: bool) -> hir::GlobalExports {
        let fixture = CfeFixtureBuilder::new("").build();
        let app_path = fixture.root().join(kind.relative_path());
        std::fs::create_dir_all(app_path.parent().unwrap()).unwrap();
        let app_source = "Перем ГлобальнаяПеременная Экспорт;\n\
                          Процедура ГлобальнаяПроцедура() Экспорт\n\
                          КонецПроцедуры";
        std::fs::write(&app_path, app_source).unwrap();
        let caller_path = write_cfe_common_module(
            fixture.root(),
            "Caller",
            false,
            "Процедура Тест() Экспорт КонецПроцедуры",
        );

        let app = FileId(0);
        let caller = FileId(1);
        let mut db = RootDatabaseImpl::new();
        let mut file_set = FileSet::new();
        file_set.insert(app, VfsPath::new(app_path.to_string_lossy().as_ref()));
        file_set.insert(caller, VfsPath::new(caller_path.to_string_lossy().as_ref()));
        db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
        for file in [app, caller] {
            db.set_file_source_root(file, SourceRootId(0));
        }
        if unread {
            db.set_file_unreadable(app);
        } else {
            db.set_file_text(app, app_source);
        }
        db.set_file_text(caller, "Процедура Тест() Экспорт КонецПроцедуры");
        db.set_all_config_paths(fixture.config_paths());

        hir::Resolver::with_workspace_scope(ModuleId::new(caller)).application_module_exports(&db)
    }

    let readable = surface(hir::ApplicationModuleKind::Managed, false);
    assert!(readable.is_complete(), "all fixed application paths were enumerable");
    let variable = readable
        .entries
        .iter()
        .find(|entry| entry.name.as_str() == "ГлобальнаяПеременная")
        .expect("exported application variable");
    assert!(matches!(variable.definition, GlobalExportDefinition::Variable(_)));
    assert_eq!(variable.capabilities.readable_as_value, Some(true));
    assert_eq!(
        variable.host,
        GlobalExportHost::ApplicationModule(hir::ApplicationModuleKind::Managed)
    );
    let method = readable
        .entries
        .iter()
        .find(|entry| entry.name.as_str() == "ГлобальнаяПроцедура")
        .expect("exported application method");
    assert!(matches!(method.definition, GlobalExportDefinition::Method(_)));
    assert_eq!(method.capabilities.callable, Some(true));
    assert_eq!(method.capabilities.readable_as_value, Some(false));

    let unread = surface(hir::ApplicationModuleKind::Managed, true);
    assert!(unread.entries.is_empty(), "unread body contributes no guessed exports");
    assert!(unread.gaps.iter().any(|gap| matches!(
        gap,
        GlobalSurfaceGap::UnreadApplicationModule { kind: hir::ApplicationModuleKind::Managed }
    )));

    let external = surface(hir::ApplicationModuleKind::ExternalConnection, false);
    assert!(
        external.entries.iter().any(|entry| {
            entry.name.as_str() == "ГлобальнаяПеременная"
                && entry.host
                    == GlobalExportHost::ApplicationModule(
                        hir::ApplicationModuleKind::ExternalConnection,
                    )
        }),
        "external-connection module exports belong to its global context"
    );
}

#[test]
fn common_module_body_lookup_marks_declared_but_unmapped_body() {
    use test_fixture::CfeFixtureBuilder;

    let fixture = CfeFixtureBuilder::new("").build();
    let caller_source = "Процедура Тест() Экспорт\nКонецПроцедуры";
    let caller_path = write_cfe_common_module(fixture.root(), "Caller", false, caller_source);
    let missing_path = write_cfe_common_module(
        fixture.root(),
        "DeclaredWithoutBody",
        false,
        "Процедура Метод() Экспорт\nКонецПроцедуры",
    );
    std::fs::remove_file(missing_path).expect("remove enrolled body from fixture");

    let caller = FileId(0);
    let mut db = RootDatabaseImpl::new();
    let mut file_set = FileSet::new();
    file_set.insert(caller, VfsPath::new(caller_path.to_string_lossy().as_ref()));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    db.set_file_source_root(caller, SourceRootId(0));
    db.set_file_text(caller, caller_source);
    db.set_all_config_paths(fixture.config_paths());

    let bodies = db.resolve_common_module_files_for_file(caller, "DeclaredWithoutBody");
    assert!(bodies.is_empty());
    assert!(
        bodies.has_missing_expected_body(),
        "a metadata declaration without a mapped body must preserve the completeness gap"
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn platform_global_read_does_not_force_reaching_definitions() {
    use hir::HirDatabase;

    let file = FileId(0);
    let mut db = RootDatabaseImpl::new_with_salsa_events();
    let mut file_set = FileSet::new();
    file_set.insert(file, VfsPath::new("/test.bsl"));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    db.set_file_source_root(file, SourceRootId(0));
    db.set_file_text(file, "Процедура Тест()\nРезультат = Метаданные;\nКонецПроцедуры");

    let _ = db.infer(file);
    let rows = db.salsa_event_report().expect("event counters enabled");
    assert!(
        rows.iter().all(|row| !row.name.contains("reaching_definitions")),
        "a read with no same-named assignment has no reason to build module dataflow"
    );
}

#[test]
fn application_module_path_scan_is_memoized_across_method_bodies() {
    use hir::HirDatabase;
    use test_fixture::CfeFixtureBuilder;

    let fixture = CfeFixtureBuilder::new("").build();
    let source = (0..12)
        .map(|index| {
            format!(
                "Процедура Тест{index}() Экспорт\nЗначение{index} = Метаданные;\nКонецПроцедуры\n"
            )
        })
        .collect::<String>();
    let caller_path = write_cfe_common_module(fixture.root(), "Caller", false, &source);
    let caller = FileId(0);
    let mut db = RootDatabaseImpl::new_with_salsa_events();
    let mut file_set = FileSet::new();
    file_set.insert(caller, VfsPath::new(caller_path.to_string_lossy().as_ref()));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    db.set_file_source_root(caller, SourceRootId(0));
    db.set_file_text(caller, &source);
    db.set_all_config_paths(fixture.config_paths());

    let _ = db.infer(caller);
    let rows = db.salsa_event_report().expect("event counters enabled");
    let query = rows
        .iter()
        .find(|row| row.name.contains("application_module_files_query"))
        .expect("tracked application-module lookup must execute");
    assert_eq!(
        query.execute,
        hir::ApplicationModuleKind::ALL.len() as u64,
        "each fixed host path is scanned once per file, not once per inferred body"
    );
}

#[test]
fn application_module_path_scan_is_memoized_across_files_of_one_root() {
    use hir::HirDatabase;
    use test_fixture::CfeFixtureBuilder;

    let fixture = CfeFixtureBuilder::new("").build();
    let source = "Процедура Тест() Экспорт\nЗначение = Метаданные;\nКонецПроцедуры\n";
    let first_path = write_cfe_common_module(fixture.root(), "CallerA", false, source);
    let second_path = write_cfe_common_module(fixture.root(), "CallerB", false, source);
    let first = FileId(0);
    let second = FileId(1);
    let mut db = RootDatabaseImpl::new_with_salsa_events();
    let mut file_set = FileSet::new();
    file_set.insert(first, VfsPath::new(first_path.to_string_lossy().as_ref()));
    file_set.insert(second, VfsPath::new(second_path.to_string_lossy().as_ref()));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    db.set_file_source_root(first, SourceRootId(0));
    db.set_file_source_root(second, SourceRootId(0));
    db.set_file_text(first, source);
    db.set_file_text(second, source);
    db.set_all_config_paths(fixture.config_paths());

    let _ = db.infer(first);
    let _ = db.infer(second);
    let rows = db.salsa_event_report().expect("event counters enabled");
    let executes =
        |name: &str| rows.iter().find(|row| row.name.contains(name)).map_or(0, |row| row.execute);
    let kinds = hir::ApplicationModuleKind::ALL.len() as u64;
    assert_eq!(
        executes("application_module_files_query"),
        2 * kinds,
        "the per-file lookup runs once per asking file and kind"
    );
    assert_eq!(
        executes("resolve_vfs_path_ci_scan_query"),
        kinds,
        "a missing host path is scanned once per candidate, not once per asking file"
    );
}

#[test]
fn application_export_shadows_same_named_platform_property_for_reads() {
    use hir::HirDatabase;
    use test_fixture::CfeFixtureBuilder;

    let fixture = CfeFixtureBuilder::new("").build();
    let app_path = fixture.root().join("Ext/ManagedApplicationModule.bsl");
    std::fs::create_dir_all(app_path.parent().unwrap()).unwrap();
    let app_source = "Перем Метаданные Экспорт;";
    std::fs::write(&app_path, app_source).unwrap();
    let caller_source = "Процедура Тест() Экспорт\nРезультат = Метаданные;\nКонецПроцедуры";
    let caller_path = write_cfe_common_module(fixture.root(), "Caller", false, caller_source);

    let app = FileId(0);
    let caller = FileId(1);
    let mut db = RootDatabaseImpl::new();
    let mut file_set = FileSet::new();
    file_set.insert(app, VfsPath::new(app_path.to_string_lossy().as_ref()));
    file_set.insert(caller, VfsPath::new(caller_path.to_string_lossy().as_ref()));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    for file in [app, caller] {
        db.set_file_source_root(file, SourceRootId(0));
    }
    db.set_file_text(app, app_source);
    db.set_file_text(caller, caller_source);
    db.set_all_config_paths(fixture.config_paths());

    assert!(
        !db.infer(caller).var_types.contains_key("результат"),
        "the user export has Unknown type; a platform-property type here would prove wrong precedence"
    );
}

#[test]
fn target_platform_version_invalidates_unresolved_name_certainty() {
    use hir::HirDatabase;
    use std::sync::Arc;
    use test_fixture::CfeFixtureBuilder;

    let fixture = CfeFixtureBuilder::new("").build();
    let source = "Процедура Тест() Экспорт\n\
                  Результат = ЗаведомоНеизвестноеИмя;\n\
                  КонецПроцедуры";
    let caller_path = write_cfe_common_module(fixture.root(), "Caller", false, source);
    let caller = FileId(0);
    let mut db = RootDatabaseImpl::new();
    let mut file_set = FileSet::new();
    file_set.insert(caller, VfsPath::new(caller_path.to_string_lossy().as_ref()));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    db.set_file_source_root(caller, SourceRootId(0));
    db.set_file_text(caller, source);
    db.set_all_config_paths(fixture.config_paths());

    let unresolved_count = |db: &RootDatabaseImpl| {
        db.infer(caller)
            .diagnostics
            .iter()
            .filter(|(_, diagnostic)| {
                matches!(diagnostic, hir::InferenceDiagnostic::UnresolvedName { .. })
            })
            .count()
    };
    assert_eq!(unresolved_count(&db), 1, "bundled 8.3.27 target is complete");

    db.set_target_platform_version(Some(Arc::<str>::from("8.3.28")));
    assert_eq!(
        unresolved_count(&db),
        0,
        "a newer runtime target must invalidate the inference query and restore Indeterminate"
    );
}

#[test]
fn boot_window_keeps_user_global_surface_indeterminate() {
    use hir::HirDatabase;
    use test_fixture::CfeFixtureBuilder;

    let fixture = CfeFixtureBuilder::new("").build();
    let source = "Процедура Тест() Экспорт\nНеизвестныйМодуль.Метод();\nКонецПроцедуры";
    let caller_path = write_cfe_common_module(fixture.root(), "Caller", false, source);
    let caller = FileId(0);
    let mut db = RootDatabaseImpl::new();
    let mut file_set = FileSet::new();
    file_set.insert(caller, VfsPath::new(caller_path.to_string_lossy().as_ref()));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    db.set_file_source_root(caller, SourceRootId(0));
    db.set_file_text(caller, source);
    db.set_all_config_paths(fixture.config_paths());
    db.set_workspace_load_complete(false);

    let unresolved_count = |db: &RootDatabaseImpl| {
        db.infer(caller)
            .diagnostics
            .iter()
            .filter(|(_, diagnostic)| {
                matches!(diagnostic, hir::InferenceDiagnostic::UnresolvedName { .. })
            })
            .count()
    };
    assert_eq!(unresolved_count(&db), 0, "boot metadata is not a complete surface");

    db.set_workspace_load_complete(true);
    assert_eq!(
        unresolved_count(&db),
        1,
        "the load-state input must invalidate inference once the empty fixture is complete"
    );
}

/// A `Движения.<Регистр>` movement must resolve to the register's `Mdo` node — and the
/// Salsa fold and the resident index must agree — given a configuration that supplies the
/// register's metadata type. Uses the checked-in designer fixture (which defines the
/// `РегистрСведений1` information register).
#[test]
fn register_movement_resolves_to_register_mdo_in_both_paths() {
    use bsl_metadata::MdoType;
    use hir::call_graph::{EdgeKind, EdgeProvenance, ResolvedTarget};
    use hir::graph_index::{resolve_module_summary_via_index, GraphIndex};
    use hir::ConfigsDatabase;

    let mut db = RootDatabaseImpl::new();
    let caller = FileId(0);
    let mut file_set = FileSet::new();
    file_set.insert(caller, VfsPath::new("/Documents/Док/Ext/ObjectModule.bsl"));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    db.set_file_source_root(caller, SourceRootId(0));
    db.set_file_text(
        caller,
        "Процедура ОбработкаПроведения()\n\
         Движения.РегистрСведений1.Записать();\n\
         КонецПроцедуры",
    );

    let config_path =
        concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-metadata/fixtures/designer").to_string();
    db.set_all_config_paths(vec![(None, std::path::PathBuf::from(config_path))]);

    let is_register_edge = |e: &hir::ResolvedCallEdge| {
        e.kind == EdgeKind::RegisterMovement
            && e.provenance == EdgeProvenance::Inferred
            && matches!(&e.target, ResolvedTarget::Mdo { mdo_type, object_name }
                if *mdo_type == MdoType::InformationRegister
                    && object_name.as_str() == "РегистрСведений1")
    };

    let salsa_summary = db.resolved_module_summary(ModuleId::new(caller));
    assert!(
        salsa_summary.edges.iter().any(is_register_edge),
        "Движения.РегистрСведений1.Записать() must resolve to the register's Mdo node"
    );

    let modules = vec![ModuleId::new(caller)];
    let index = GraphIndex::build(&db, &modules);
    let index_summary = resolve_module_summary_via_index(&db, ModuleId::new(caller), &index);
    assert_eq!(
        index_summary, *salsa_summary,
        "the resident index must resolve register movements identically to the Salsa fold"
    );
}

/// A global-context `ПодключитьОбработчикОжидания` handler that lives in a global common
/// module (not the calling module) must resolve cross-module, per the platform help. Uses
/// the designer fixture, which declares one global common module (`ГлобальныйСерверныйМодуль`).
#[test]
fn idle_handler_resolves_to_a_global_common_module() {
    use hir::call_graph::{EdgeKind, EdgeProvenance, ResolvedTarget};
    use hir::ConfigsDatabase;

    let mut db = RootDatabaseImpl::new();
    let caller = FileId(0);
    let global = FileId(1);
    let mut file_set = FileSet::new();
    file_set.insert(caller, VfsPath::new("/CommonModules/Вызыватель/Ext/Module.bsl"));
    file_set
        .insert(global, VfsPath::new("/CommonModules/ГлобальныйСерверныйМодуль/Ext/Module.bsl"));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    db.set_file_source_root(caller, SourceRootId(0));
    db.set_file_source_root(global, SourceRootId(0));
    db.set_file_text(
        caller,
        "Процедура Старт() Экспорт\n\
         ПодключитьОбработчикОжидания(\"ОбновитьЭкран\", 5);\n\
         КонецПроцедуры",
    );
    db.set_file_text(global, "Процедура ОбновитьЭкран() Экспорт КонецПроцедуры");

    let config_path =
        concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-metadata/fixtures/designer").to_string();
    db.set_all_config_paths(vec![(None, std::path::PathBuf::from(config_path))]);

    let is_global_idle = |e: &hir::ResolvedCallEdge| {
        e.kind == EdgeKind::IdleHandler
            && e.provenance == EdgeProvenance::StringResolved
            && matches!(&e.target, ResolvedTarget::Method(m) if m.module == ModuleId::new(global))
    };

    let salsa_summary = db.resolved_module_summary(ModuleId::new(caller));
    assert!(
        salsa_summary.edges.iter().any(is_global_idle),
        "an idle handler exported by a global common module must resolve cross-module"
    );

    // The resident index must resolve the cross-module idle handler identically.
    use hir::graph_index::{resolve_module_summary_via_index, GraphIndex};
    let modules = vec![ModuleId::new(caller), ModuleId::new(global)];
    let index = GraphIndex::build(&db, &modules);
    let index_summary = resolve_module_summary_via_index(&db, ModuleId::new(caller), &index);
    assert_eq!(
        index_summary, *salsa_summary,
        "the resident index must resolve cross-module idle handlers identically to the Salsa fold"
    );
}

/// The batched build path: a call from a module in one batch to a module in
/// another must resolve through the resident `GraphIndex`, even though the target
/// module's text is absent from the batch's database. Asserts the edge SET
/// collected across per-batch databases equals the Salsa fold's.
#[test]
fn project_batch_edges_resolves_across_batches() {
    use hir::call_graph::WorkspaceCallEdge;
    use hir::graph_index::{project_batch_edges, GraphBuildState, GraphIndex};
    use hir::ConfigsDatabase;

    let files: &[(&str, &str)] = &[
        (
            "/src/CommonModules/A/Ext/Module.bsl",
            "Процедура Т() Экспорт\nB.Метод();\nКонецПроцедуры",
        ),
        ("/src/CommonModules/B/Ext/Module.bsl", "Функция Метод() Экспорт Возврат 1; КонецФункции"),
    ];
    let a = FileId(0);
    let b = FileId(1);
    let module_a = ModuleId::new(a);
    let module_b = ModuleId::new(b);

    let make_db = |texts: &[(FileId, &str)]| -> RootDatabaseImpl {
        let mut db = RootDatabaseImpl::new();
        let mut file_set = FileSet::new();
        for (i, (path, _)) in files.iter().enumerate() {
            file_set.insert(FileId(i as u32), VfsPath::new(*path));
        }
        db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
        for (i, _) in files.iter().enumerate() {
            db.set_file_source_root(FileId(i as u32), SourceRootId(0));
        }
        for &(fid, text) in texts {
            db.set_file_text(fid, text);
        }
        db
    };

    // The index is built over ALL modules (whole config), here from one db.
    let full = make_db(&[(a, files[0].1), (b, files[1].1)]);
    let index = GraphIndex::build(&full, &[module_a, module_b]);

    // Batch 0 sees only A's text; batch 1 only B's. A's call to B.Метод must still
    // resolve through the index.
    let db0 = make_db(&[(a, files[0].1)]);
    let db1 = make_db(&[(b, files[1].1)]);
    let pool = rayon::ThreadPoolBuilder::new().build().unwrap();
    let mut state = GraphBuildState::new();
    let mut batched: Vec<WorkspaceCallEdge> = Vec::new();
    batched.extend(project_batch_edges(&pool, &db0, &[module_a], &index, &mut state));
    batched.extend(project_batch_edges(&pool, &db1, &[module_b], &index, &mut state));

    let salsa = full.workspace_call_graph(SourceRootId(0));
    let folded: Vec<WorkspaceCallEdge> = salsa.edges().cloned().collect();

    // The cross-batch call resolved (not dropped to Unresolved).
    assert!(
        batched.iter().any(|e| matches!(
            (&e.from, &e.to),
            (hir::GraphNode::Method(f), hir::GraphNode::Method(t))
                if f.module == module_a && t.module == module_b
        )),
        "A.Т → B.Метод must resolve through the index across batches"
    );

    // Same edge MULTISET as the fold (order differs — per-batch vs. global
    // passes). This fixture has no metadata objects, so node spelling cannot
    // diverge; a per-edge count comparison guards against duplicates too.
    let count = |edges: &[WorkspaceCallEdge], target: &WorkspaceCallEdge| {
        edges.iter().filter(|e| *e == target).count()
    };
    assert_eq!(batched.len(), folded.len(), "batched and fold edge counts must match");
    for edge in folded.iter().chain(batched.iter()) {
        assert_eq!(
            count(&batched, edge),
            count(&folded, edge),
            "edge multiplicity differs between batched build and fold: {edge:?}"
        );
    }
}

#[test]
fn event_subscription_links_to_its_exported_handler() {
    use bsl_metadata::MdoType;
    use hir::call_graph::{EdgeKind, EdgeProvenance, GraphNode};
    use hir::graph_index::{project_workspace_subscription_edges, GraphBuildState, GraphIndex};

    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("src/cf");
    std::fs::create_dir_all(root.join("EventSubscriptions")).unwrap();
    std::fs::write(root.join("Configuration.xml"), "<Configuration/>").unwrap();
    std::fs::write(
        root.join("EventSubscriptions/ПриЗаписи.xml"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <EventSubscription uuid="00000000-0000-0000-0000-000000000010">
        <Properties>
            <Name>ПриЗаписи</Name>
            <Source><Type>cfg:CatalogObject.Номенклатура</Type></Source>
            <Event>OnWrite</Event>
            <Handler>CommonModule.ОбщийМодуль.Обработчик</Handler>
        </Properties>
    </EventSubscription>
</MetaDataObject>"#,
    )
    .unwrap();

    let mut db = RootDatabaseImpl::new();
    db.set_all_config_paths(vec![(None, root.clone())]);

    let file_id = FileId(0);
    let file_path = root.join("CommonModules/ОбщийМодуль/Ext/Module.bsl");
    let mut file_set = FileSet::new();
    file_set.insert(file_id, VfsPath::new(file_path.to_string_lossy().as_ref()));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    db.set_file_source_root(file_id, SourceRootId(0));
    db.set_file_text(file_id, "Процедура Обработчик(Источник, Отказ) Экспорт\nКонецПроцедуры");

    let modules = [ModuleId::new(file_id)];
    let pool = rayon::ThreadPoolBuilder::new().build().unwrap();
    let mut index = GraphIndex::new();
    index.add_batch(&pool, &db, &modules);

    let mut state = GraphBuildState::new();
    let edges = project_workspace_subscription_edges(&db, file_id, &index, &mut state);

    let sub = edges
        .iter()
        .find(|e| e.kind == EdgeKind::EventSubscriptionRef)
        .expect("the subscription handler resolves to one event_subscription edge");
    assert_eq!(sub.provenance, EdgeProvenance::StringResolved);
    assert!(
        matches!(&sub.from, GraphNode::Mdo { mdo_type, object_name }
            if *mdo_type == MdoType::EventSubscription && object_name.as_str() == "ПриЗаписи"),
        "edge source is the subscription's Mdo node"
    );
    assert!(matches!(&sub.to, GraphNode::Method(_)), "edge target is the handler method");
}

#[test]
fn subsystem_membership_links_to_member_objects() {
    use bsl_metadata::MdoType;
    use hir::call_graph::{EdgeKind, EdgeProvenance, GraphNode};
    use hir::graph_index::{project_workspace_subsystem_edges, GraphBuildState};

    // The designer fixture's `Подсистема1` contains an information register and a catalog.
    let mut db = RootDatabaseImpl::new();
    let config_path = std::path::PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../bsl-metadata/fixtures/designer"
    ));
    db.set_all_config_paths(vec![(None, config_path)]);

    let file_id = FileId(0);
    let mut file_set = FileSet::new();
    file_set.insert(file_id, VfsPath::new("/Documents/Док/Ext/ObjectModule.bsl"));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    db.set_file_source_root(file_id, SourceRootId(0));
    db.set_file_text(file_id, "Процедура Х() КонецПроцедуры");

    let mut state = GraphBuildState::new();
    let edges = project_workspace_subsystem_edges(&db, file_id, &mut state);

    let membership_to = |to_type: MdoType, to_name: &str| {
        edges.iter().any(|e| {
            e.kind == EdgeKind::SubsystemMembership
                && e.provenance == EdgeProvenance::Resolved
                && matches!(&e.from, GraphNode::Mdo { mdo_type, object_name }
                    if *mdo_type == MdoType::Subsystem && object_name.as_str() == "Подсистема1")
                && matches!(&e.to, GraphNode::Mdo { mdo_type, object_name }
                    if *mdo_type == to_type && object_name.as_str() == to_name)
        })
    };
    assert!(
        membership_to(MdoType::InformationRegister, "РегистрСведений1"),
        "subsystem → information-register member edge"
    );
    assert!(membership_to(MdoType::Catalog, "Справочник1"), "subsystem → catalog member edge");
}

#[test]
fn document_links_to_posted_registers_via_register_records() {
    use bsl_metadata::MdoType;
    use hir::call_graph::{EdgeKind, EdgeProvenance, GraphNode};
    use hir::graph_index::{project_workspace_register_records_edges, GraphBuildState};

    // The designer fixture's `Документ1` declares four registers in its `RegisterRecords`.
    let mut db = RootDatabaseImpl::new();
    let config_path = std::path::PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../bsl-metadata/fixtures/designer"
    ));
    db.set_all_config_paths(vec![(None, config_path)]);

    let file_id = FileId(0);
    let mut file_set = FileSet::new();
    file_set.insert(file_id, VfsPath::new("/Documents/Документ1/Ext/ObjectModule.bsl"));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    db.set_file_source_root(file_id, SourceRootId(0));
    db.set_file_text(file_id, "Процедура Х() КонецПроцедуры");

    let mut state = GraphBuildState::new();
    let edges = project_workspace_register_records_edges(&db, file_id, &mut state);

    let posts_to = |to_type: MdoType, to_name: &str| {
        edges.iter().any(|e| {
            e.kind == EdgeKind::RegisterRecords
                && e.provenance == EdgeProvenance::Resolved
                && matches!(&e.from, GraphNode::Mdo { mdo_type, object_name }
                    if *mdo_type == MdoType::Document && object_name.as_str() == "Документ1")
                && matches!(&e.to, GraphNode::Mdo { mdo_type, object_name }
                    if *mdo_type == to_type && object_name.as_str() == to_name)
        })
    };
    assert!(
        posts_to(MdoType::AccumulationRegister, "РегистрНакопления1"),
        "document → accumulation-register post edge"
    );
    assert!(
        posts_to(MdoType::CalculationRegister, "РегистрРасчета1"),
        "document → calculation-register post edge"
    );
    assert!(
        posts_to(MdoType::InformationRegister, "РегистрСведений2"),
        "document → information-register post edge"
    );
    assert!(
        posts_to(MdoType::AccountingRegister, "РегистрБухгалтерии1"),
        "document → accounting-register post edge"
    );
    // Every edge is a register_records edge from a document to a register node — no other
    // shapes, and never a non-register target.
    assert!(
        edges.iter().all(|e| {
            e.kind == EdgeKind::RegisterRecords
                && matches!(&e.from, GraphNode::Mdo { mdo_type, .. } if *mdo_type == MdoType::Document)
                && matches!(&e.to, GraphNode::Mdo { mdo_type, .. } if matches!(
                    mdo_type,
                    MdoType::AccumulationRegister
                        | MdoType::InformationRegister
                        | MdoType::AccountingRegister
                        | MdoType::CalculationRegister
                ))
        }),
        "every emitted edge is a document → register register_records edge"
    );
}

#[test]
fn role_links_to_object_rights_and_rls_condition_object() {
    use bsl_metadata::MdoType;
    use hir::call_graph::{EdgeKind, EdgeProvenance, GraphNode};
    use hir::graph_index::{project_workspace_role_edges, GraphBuildState};

    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("src/cf");
    std::fs::create_dir_all(root.join("Catalogs")).unwrap();
    std::fs::create_dir_all(root.join("Roles/ТестоваяРоль/Ext")).unwrap();
    std::fs::write(root.join("Configuration.xml"), "<Configuration/>").unwrap();

    let catalog = |name: &str| {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <Catalog uuid="00000000-0000-0000-0000-0000000000{:02}">
        <Properties><Name>{name}</Name></Properties>
    </Catalog>
</MetaDataObject>"#,
            name.len()
        )
    };
    std::fs::write(root.join("Catalogs/Контрагенты.xml"), catalog("Контрагенты")).unwrap();
    std::fs::write(root.join("Catalogs/Организации.xml"), catalog("Организации")).unwrap();

    std::fs::write(
        root.join("Roles/ТестоваяРоль.xml"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <Role uuid="00000000-0000-0000-0000-0000000000aa">
        <Properties><Name>ТестоваяРоль</Name></Properties>
    </Role>
</MetaDataObject>"#,
    )
    .unwrap();
    // Direct read right on Контрагенты, restricted by an RLS condition that names Организации
    // only inside its subquery — so Организации is reachable solely through the RLS text.
    std::fs::write(
        root.join("Roles/ТестоваяРоль/Ext/Rights.xml"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<Rights xmlns="http://v8.1c.ru/8.2/roles" version="2.10">
    <setForNewObjects>false</setForNewObjects>
    <object>
        <name>Catalog.Контрагенты</name>
        <right>
            <name>Read</name>
            <value>true</value>
            <restrictionByCondition>
                <condition>Контрагенты.Ссылка В (ВЫБРАТЬ Ссылка ИЗ Справочник.Организации)</condition>
            </restrictionByCondition>
        </right>
    </object>
</Rights>"#,
    )
    .unwrap();

    let mut db = RootDatabaseImpl::new();
    db.set_all_config_paths(vec![(None, root.clone())]);
    let file_id = FileId(0);
    let mut file_set = FileSet::new();
    file_set.insert(file_id, VfsPath::new(root.join("X.bsl").to_string_lossy().as_ref()));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    db.set_file_source_root(file_id, SourceRootId(0));
    db.set_file_text(file_id, "Процедура Х() КонецПроцедуры");

    let mut state = GraphBuildState::new();
    let edges = project_workspace_role_edges(&db, file_id, &mut state);

    let role_edge = |to_name: &str, prov: EdgeProvenance| {
        edges.iter().any(|e| {
            e.kind == EdgeKind::RoleReference
                && e.provenance == prov
                && matches!(&e.from, GraphNode::Mdo { mdo_type, object_name }
                    if *mdo_type == MdoType::Role && object_name.as_str() == "ТестоваяРоль")
                && matches!(&e.to, GraphNode::Mdo { mdo_type, object_name }
                    if *mdo_type == MdoType::Catalog && object_name.as_str() == to_name)
        })
    };
    assert!(
        role_edge("Контрагенты", EdgeProvenance::Resolved),
        "direct object-rights edge role → Контрагенты is `resolved`"
    );
    assert!(
        role_edge("Организации", EdgeProvenance::Inferred),
        "RLS condition object role → Организации is `inferred` (parsed from the restriction text)"
    );
}

#[test]
fn event_subscription_with_missing_handler_yields_no_edge() {
    use hir::call_graph::EdgeKind;
    use hir::graph_index::{project_workspace_subscription_edges, GraphBuildState, GraphIndex};

    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("src/cf");
    std::fs::create_dir_all(root.join("EventSubscriptions")).unwrap();
    std::fs::write(root.join("Configuration.xml"), "<Configuration/>").unwrap();
    std::fs::write(
        root.join("EventSubscriptions/Сирота.xml"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <EventSubscription uuid="00000000-0000-0000-0000-000000000011">
        <Properties>
            <Name>Сирота</Name>
            <Event>OnWrite</Event>
            <Handler>CommonModule.ОбщийМодуль.НетТакого</Handler>
        </Properties>
    </EventSubscription>
</MetaDataObject>"#,
    )
    .unwrap();

    let mut db = RootDatabaseImpl::new();
    db.set_all_config_paths(vec![(None, root.clone())]);

    let file_id = FileId(0);
    let file_path = root.join("CommonModules/ОбщийМодуль/Ext/Module.bsl");
    let mut file_set = FileSet::new();
    file_set.insert(file_id, VfsPath::new(file_path.to_string_lossy().as_ref()));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    db.set_file_source_root(file_id, SourceRootId(0));
    // Handler module exists but the named method does not → no edge.
    db.set_file_text(file_id, "Процедура Другой() Экспорт\nКонецПроцедуры");

    let modules = [ModuleId::new(file_id)];
    let pool = rayon::ThreadPoolBuilder::new().build().unwrap();
    let mut index = GraphIndex::new();
    index.add_batch(&pool, &db, &modules);

    let mut state = GraphBuildState::new();
    let edges = project_workspace_subscription_edges(&db, file_id, &index, &mut state);
    assert!(
        !edges.iter().any(|e| e.kind == EdgeKind::EventSubscriptionRef),
        "an unresolved handler must not produce an edge"
    );
}

/// Proof of the weaving inference fallback: a `&Вместо` interceptor in an extension
/// module calls a base-module function `БазХелпер` (which returns 1) and assigns its
/// result. Inferred via `infer_weaving` the call resolves through the base sibling
/// fallback, so the assigned variable types as Число; standalone inference of the same
/// extension module — which has no base sibling in scope — cannot resolve the call, so the
/// variable stays untyped. The difference proves the base fallback is what resolves it.
#[test]
fn infer_weaving_resolves_base_sibling() {
    use crate::weaving_target;
    use hir::HirDatabase;

    let temp = tempfile::tempdir().unwrap();
    let main_root = temp.path().join("src/cf");
    let ext_root = temp.path().join("src/cfe/X");
    std::fs::create_dir_all(&main_root).unwrap();
    std::fs::create_dir_all(&ext_root).unwrap();

    let mut db = RootDatabaseImpl::new();
    db.set_all_config_paths(vec![
        (None, main_root.clone()),
        (Some("X".to_string()), ext_root.clone()),
    ]);

    let main_file = FileId(0);
    let ext_file = FileId(1);
    let mut file_set = FileSet::new();
    let main_path = main_root.join("CommonModules/М/Ext/Module.bsl");
    let ext_path = ext_root.join("CommonModules/М/Ext/Module.bsl");
    file_set.insert(main_file, VfsPath::new(main_path.to_string_lossy().as_ref()));
    file_set.insert(ext_file, VfsPath::new(ext_path.to_string_lossy().as_ref()));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    db.set_file_source_root(main_file, SourceRootId(0));
    db.set_file_source_root(ext_file, SourceRootId(0));

    db.set_file_text(main_file, "Функция БазХелпер() Экспорт\n\tВозврат 1;\nКонецФункции");
    db.set_file_text(
        ext_file,
        "&Вместо(\"М\")\n\
         Процедура Расш_М()\n\
         \tЗначение = БазХелпер();\n\
         КонецПроцедуры",
    );

    use bsl_types::builders::Builders;
    use stdx::case::CaseExt;
    let number = db.number(None, None);
    let key = "Значение".fold_lower();

    // Standalone: the extension module alone has no `БазХелпер` sibling, so the call does
    // not resolve and `Значение` stays untyped.
    let standalone = HirDatabase::infer(&db, ext_file);
    assert!(
        !standalone.var_types.contains_key(&key),
        "baseline: standalone inference cannot resolve the base call, so `Значение` is \
         untyped; var_types = {:?}",
        standalone.var_types.keys().collect::<Vec<_>>()
    );

    // Weaving: the base module is a same-module sibling fallback → `БазХелпер()` resolves
    // and `Значение` types as Число (the base function returns 1).
    let wid = weaving_target(&db, ext_file).expect("ext module pairs to a base");
    let woven = hir::infer_weaving(&db, wid);
    assert_eq!(
        woven.var_types.get(&key).copied(),
        Some(number),
        "weaving inference must resolve the base function `БазХелпер()` via the base \
         fallback and type `Значение` as Число; var_types = {:?}",
        woven.var_types.keys().collect::<Vec<_>>()
    );
}

/// Proof of `ПродолжитьВызов` return typing under weaving: a `&Вместо("М")` interceptor in an
/// extension module calls `ПродолжитьВызов()` — which re-enters the original base function `М`
/// (returns 1) — and assigns its result. Inferred via `infer_weaving` the call types as the base
/// method's return, so the assigned variable types as Число. Without the `&Вместо` proceed
/// wiring `ПродолжитьВызов` would carry only the platform global's generic (non-Число) return.
#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn infer_weaving_types_proceed_with_call_return() {
    use crate::weaving_target;

    let temp = tempfile::tempdir().unwrap();
    let main_root = temp.path().join("src/cf");
    let ext_root = temp.path().join("src/cfe/X");
    std::fs::create_dir_all(&main_root).unwrap();
    std::fs::create_dir_all(&ext_root).unwrap();

    let mut db = RootDatabaseImpl::new();
    db.set_all_config_paths(vec![
        (None, main_root.clone()),
        (Some("X".to_string()), ext_root.clone()),
    ]);

    let main_file = FileId(0);
    let ext_file = FileId(1);
    let mut file_set = FileSet::new();
    let main_path = main_root.join("CommonModules/М/Ext/Module.bsl");
    let ext_path = ext_root.join("CommonModules/М/Ext/Module.bsl");
    file_set.insert(main_file, VfsPath::new(main_path.to_string_lossy().as_ref()));
    file_set.insert(ext_file, VfsPath::new(ext_path.to_string_lossy().as_ref()));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    db.set_file_source_root(main_file, SourceRootId(0));
    db.set_file_source_root(ext_file, SourceRootId(0));

    db.set_file_text(main_file, "Функция М() Экспорт\n\tВозврат 1;\nКонецФункции");
    db.set_file_text(
        ext_file,
        "&Вместо(\"М\")\n\
         Функция Расш_М()\n\
         \tРез = ПродолжитьВызов();\n\
         \tВозврат Рез;\n\
         КонецФункции",
    );

    use bsl_types::builders::Builders;
    use stdx::case::CaseExt;
    let number = db.number(None, None);
    let key = "Рез".fold_lower();

    let wid = weaving_target(&db, ext_file).expect("ext module pairs to a base");
    let woven = hir::infer_weaving(&db, wid);
    assert_eq!(
        woven.var_types.get(&key).copied(),
        Some(number),
        "weaving inference must type `ПродолжитьВызов()` in a &Вместо interceptor as the base \
         method `М`'s return (Число); var_types = {:?}",
        woven.var_types.keys().collect::<Vec<_>>()
    );
}

/// A base-configuration file has no base counterpart to pair against, so `weaving_target`
/// returns `None` (the base does not weave onto itself).
#[test]
fn weaving_target_none_for_base_file() {
    use crate::weaving_target;

    let temp = tempfile::tempdir().unwrap();
    let main_root = temp.path().join("src/cf");
    let ext_root = temp.path().join("src/cfe/X");
    std::fs::create_dir_all(&main_root).unwrap();
    std::fs::create_dir_all(&ext_root).unwrap();

    let mut db = RootDatabaseImpl::new();
    db.set_all_config_paths(vec![
        (None, main_root.clone()),
        (Some("X".to_string()), ext_root.clone()),
    ]);

    let main_file = FileId(0);
    let mut file_set = FileSet::new();
    let main_path = main_root.join("CommonModules/М/Ext/Module.bsl");
    file_set.insert(main_file, VfsPath::new(main_path.to_string_lossy().as_ref()));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    db.set_file_source_root(main_file, SourceRootId(0));
    db.set_file_text(main_file, "Функция БазХелпер() Экспорт\n\tВозврат 1;\nКонецФункции");

    assert!(
        weaving_target(&db, main_file).is_none(),
        "a base-configuration file has no base counterpart to weave onto"
    );
}

#[test]
fn parse_http_service_query_reads_nested_methods() {
    use crate::metadata::{parse_http_service_query, HTTPServiceFile};

    fn http_service_xml(name: &str, root_url: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <HTTPService uuid="4797cd39-952d-4e4d-9685-014e4d5a8e25">
        <Properties>
            <Name>{name}</Name>
            <RootURL>{root_url}</RootURL>
        </Properties>
        <ChildObjects>
            <URLTemplate uuid="7124b2c7-d38e-40b9-a934-e6eb9de99340">
                <Properties>
                    <Name>URLTemplate1</Name>
                    <Template>/storage/{{Storage}}/{{ID}}</Template>
                </Properties>
                <ChildObjects>
                    <Method uuid="605f52a9-e95b-4900-9e41-449d7da01348">
                        <Properties>
                            <Name>GET</Name>
                            <HTTPMethod>GET</HTTPMethod>
                            <Handler>URLTemplate1GET</Handler>
                        </Properties>
                    </Method>
                    <Method uuid="462355c3-a1d9-488b-91ea-979f880f910f">
                        <Properties>
                            <Name>POST</Name>
                            <HTTPMethod>POST</HTTPMethod>
                            <Handler>URLTemplate1POST</Handler>
                        </Properties>
                    </Method>
                </ChildObjects>
            </URLTemplate>
        </ChildObjects>
    </HTTPService>
</MetaDataObject>"#
        )
    }

    let mut db = RootDatabaseImpl::new();
    let main_file = FileId(0);

    let mut file_set = FileSet::new();
    file_set.insert(main_file, VfsPath::new("/HTTPServices/МойHTTPСервис.xml"));
    db.set_source_root(SourceRootId(1), SourceRoot::new_local(file_set));
    db.set_file_source_root(main_file, SourceRootId(1));
    db.set_file_text(main_file, &http_service_xml("МойHTTPСервис", "http"));

    let file = HTTPServiceFile::new(&db, main_file);
    let service =
        parse_http_service_query(&db, file).expect("HTTP service parses via per-service query");
    assert_eq!(service.name(), "МойHTTPСервис");
    assert_eq!(service.root_url(), "http");
    assert_eq!(service.url_templates().len(), 1);

    let template = &service.url_templates()[0];
    assert_eq!(template.name(), "URLTemplate1");
    assert_eq!(template.methods().len(), 2);
    assert_eq!(template.methods()[0].handler(), "URLTemplate1GET");
    assert_eq!(template.methods()[1].handler(), "URLTemplate1POST");

    let again = parse_http_service_query(&db, file).expect("HTTP service parses again");
    assert!(Arc::ptr_eq(&service, &again), "parse_http_service_query should memoise");
}

#[test]
fn parse_web_service_query_reads_operations() {
    use crate::metadata::{parse_web_service_query, WebServiceFile};

    fn web_service_xml(name: &str, namespace: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <WebService uuid="0b4a4c9c-76e9-455c-9471-249051a8301d">
        <Properties>
            <Name>{name}</Name>
            <Namespace>{namespace}</Namespace>
        </Properties>
        <ChildObjects>
            <Operation uuid="bc99d837-aee6-40ee-8940-3a81dddf477c">
                <Properties>
                    <Name>Операция1</Name>
                    <ProcedureName>Операция1</ProcedureName>
                </Properties>
                <ChildObjects/>
            </Operation>
            <Operation uuid="bc09d837-aee6-40ee-8940-3a81dddf477c">
                <Properties>
                    <Name>ОперацияБезОбработчика</Name>
                    <ProcedureName/>
                </Properties>
                <ChildObjects/>
            </Operation>
        </ChildObjects>
    </WebService>
</MetaDataObject>"#
        )
    }

    let mut db = RootDatabaseImpl::new();
    let main_file = FileId(0);

    let mut file_set = FileSet::new();
    file_set.insert(main_file, VfsPath::new("/WebServices/МойWebСервис.xml"));
    db.set_source_root(SourceRootId(1), SourceRoot::new_local(file_set));
    db.set_file_source_root(main_file, SourceRootId(1));
    db.set_file_text(main_file, &web_service_xml("МойWebСервис", "test.com"));

    let file = WebServiceFile::new(&db, main_file);
    let service =
        parse_web_service_query(&db, file).expect("web service parses via per-service query");
    assert_eq!(service.name(), "МойWebСервис");
    assert_eq!(service.namespace(), "test.com");
    assert_eq!(service.operations().len(), 2);

    let op = &service.operations()[0];
    assert_eq!(op.name(), "Операция1");
    assert_eq!(op.procedure_name(), "Операция1");

    let empty_op = &service.operations()[1];
    assert_eq!(empty_op.name(), "ОперацияБезОбработчика");
    assert!(empty_op.is_handler_empty(), "operation with empty ProcedureName has no handler");

    let again = parse_web_service_query(&db, file).expect("web service parses again");
    assert!(Arc::ptr_eq(&service, &again), "parse_web_service_query should memoise");
}

#[test]
fn parse_integration_service_query_reads_channels() {
    use crate::metadata::{parse_integration_service_query, IntegrationServiceFile};

    fn integration_service_xml(name: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.20">
    <IntegrationService uuid="c512a1cd-1240-4e46-8bad-8b7b27c5c25a">
        <Properties>
            <Name>{name}</Name>
        </Properties>
        <ChildObjects>
            <IntegrationServiceChannel uuid="1ef0581c-b1d8-4115-87f1-7856f6c06bb6">
                <Properties>
                    <Name>input_from_SM_normal_priority</Name>
                    <MessageDirection>Receive</MessageDirection>
                    <ReceiveMessageProcessing>ОбработатьСообщениеОбычныйПриоритет</ReceiveMessageProcessing>
                </Properties>
            </IntegrationServiceChannel>
            <IntegrationServiceChannel uuid="b017ac62-a4a2-47bd-b963-50e0764a7d4e">
                <Properties>
                    <Name>output_to_SM_high_priority</Name>
                    <MessageDirection>Send</MessageDirection>
                    <ReceiveMessageProcessing/>
                </Properties>
            </IntegrationServiceChannel>
        </ChildObjects>
    </IntegrationService>
</MetaDataObject>"#
        )
    }

    let mut db = RootDatabaseImpl::new();
    let main_file = FileId(0);

    let mut file_set = FileSet::new();
    file_set.insert(main_file, VfsPath::new("/IntegrationServices/ОбменСообщениями.xml"));
    db.set_source_root(SourceRootId(1), SourceRoot::new_local(file_set));
    db.set_file_source_root(main_file, SourceRootId(1));
    db.set_file_text(main_file, &integration_service_xml("ОбменСообщениями"));

    let file = IntegrationServiceFile::new(&db, main_file);
    let service = parse_integration_service_query(&db, file)
        .expect("integration service parses via per-service query");
    assert_eq!(service.name(), "ОбменСообщениями");
    assert_eq!(service.channels().len(), 2);

    let handlers: Vec<&str> = service.receive_handlers().collect();
    assert_eq!(handlers, vec!["ОбработатьСообщениеОбычныйПриоритет"]);
    assert_eq!(service.channels()[0].name(), "input_from_SM_normal_priority");
    assert_eq!(service.channels()[1].receive_message_processing(), "");

    let again =
        parse_integration_service_query(&db, file).expect("integration service parses again");
    assert!(Arc::ptr_eq(&service, &again), "parse_integration_service_query should memoise");
}

#[test]
fn service_indexes_are_case_insensitive_and_track_module_file() {
    use crate::metadata::{
        http_service_index, integration_service_index, web_service_index, HTTPServiceEntry,
        IntegrationServiceEntry, MetadataListingInput, WebServiceEntry,
    };
    use salsa::Setter;

    let mut db = RootDatabaseImpl::new();
    let http_main = FileId(0);
    let http_module = FileId(1);
    let web_main = FileId(2);
    let web_module = FileId(3);
    let isvc_main = FileId(4);
    let isvc_module = FileId(5);

    let mut file_set = FileSet::new();
    file_set.insert(http_main, VfsPath::new("/HTTPServices/Сервис1.xml"));
    file_set.insert(http_module, VfsPath::new("/HTTPServices/Сервис1/Ext/Module.bsl"));
    file_set.insert(web_main, VfsPath::new("/WebServices/Сервис2.xml"));
    file_set.insert(web_module, VfsPath::new("/WebServices/Сервис2/Ext/Module.bsl"));
    file_set.insert(isvc_main, VfsPath::new("/IntegrationServices/Сервис3.xml"));
    file_set.insert(isvc_module, VfsPath::new("/IntegrationServices/Сервис3/Ext/Module.bsl"));
    db.set_source_root(SourceRootId(1), SourceRoot::new_local(file_set));
    for fid in [http_main, http_module, web_main, web_module, isvc_main, isvc_module] {
        db.set_file_source_root(fid, SourceRootId(1));
    }
    db.set_file_text(http_main, "<MetaDataObject/>");
    db.set_file_text(web_main, "<MetaDataObject/>");
    db.set_file_text(isvc_main, "<MetaDataObject/>");

    let listing = MetadataListingInput::new(
        &db,
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
    );
    listing.set_http_services(&mut db).to(Arc::new(vec![HTTPServiceEntry {
        name: "Сервис1".to_string(),
        main: http_main,
        module_file: Some(http_module),
    }]));
    listing.set_web_services(&mut db).to(Arc::new(vec![WebServiceEntry {
        name: "Сервис2".to_string(),
        main: web_main,
        module_file: Some(web_module),
    }]));
    listing.set_integration_services(&mut db).to(Arc::new(vec![IntegrationServiceEntry {
        name: "Сервис3".to_string(),
        main: isvc_main,
        module_file: Some(isvc_module),
    }]));

    let http_idx = http_service_index(&db, listing);
    assert_eq!(http_idx.lookup("сервис1"), Some(http_main));
    assert_eq!(http_idx.lookup_module_file("Сервис1"), Some(http_module));
    assert!(http_idx.lookup_module_file("НетТакогоСервиса").is_none());

    let web_idx = web_service_index(&db, listing);
    assert_eq!(web_idx.lookup("СЕРВИС2"), Some(web_main));
    assert_eq!(web_idx.lookup_module_file("сервис2"), Some(web_module));

    let isvc_idx = integration_service_index(&db, listing);
    assert_eq!(isvc_idx.lookup("сервис3"), Some(isvc_main));
    assert_eq!(isvc_idx.lookup_module_file("СЕРВИС3"), Some(isvc_module));
}

#[test]
fn module_metadata_http_service_late_module_file_registration_falls_back_to_whole_config() {
    use crate::metadata::HTTPServiceEntry;

    fn http_service_xml(name: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <HTTPService uuid="00000000-0000-0000-0000-000000000071">
        <Properties><Name>{name}</Name><RootURL>api</RootURL></Properties>
        <ChildObjects/>
    </HTTPService>
</MetaDataObject>"#
        )
    }

    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("cf");
    let service_name = "OrdersApi";
    let service_path = root.join(format!("HTTPServices/{service_name}.xml"));
    let module_path = root.join(format!("HTTPServices/{service_name}/Ext/Module.bsl"));
    std::fs::create_dir_all(module_path.parent().unwrap()).unwrap();
    std::fs::write(root.join("Configuration.xml"), "<Configuration/>").unwrap();
    std::fs::write(&service_path, http_service_xml(service_name)).unwrap();
    std::fs::write(&module_path, "Процедура GET() КонецПроцедуры").unwrap();

    let mut db = RootDatabaseImpl::new();
    db.set_all_config_paths(vec![(None, root.clone())]);

    let service_file = FileId(0);
    let module_file = FileId(1);
    let mut file_set = FileSet::new();
    file_set.insert(service_file, VfsPath::new(service_path.to_string_lossy().as_ref()));
    db.set_source_root(SourceRootId(1), SourceRoot::new_local(file_set.clone()));
    db.set_file_source_root(service_file, SourceRootId(1));
    db.set_file_text(service_file, &http_service_xml(service_name));
    db.set_metadata_listing(
        &root.to_string_lossy(),
        MetadataListingData {
            common_attributes: Vec::new(),
            entries: Vec::new(),
            defined_types: Vec::new(),
            common_modules: Vec::new(),
            event_subscriptions: Vec::new(),
            scheduled_jobs: Vec::new(),
            roles: Vec::new(),
            http_services: vec![HTTPServiceEntry {
                name: service_name.to_string(),
                main: service_file,
                module_file: None,
            }],
            web_services: Vec::new(),
            integration_services: Vec::new(),
            subsystems: Vec::new(),
        },
    );

    file_set.insert(module_file, VfsPath::new(module_path.to_string_lossy().as_ref()));
    db.set_source_root(SourceRootId(1), SourceRoot::new_local(file_set));
    db.set_file_source_root(module_file, SourceRootId(1));
    db.set_file_text(module_file, "Процедура GET() КонецПроцедуры");

    let metadata = db.module_metadata(ModuleId::new(module_file));
    assert_eq!(metadata.module_type, bsl_metadata::ModuleType::HTTPServiceModule);
    assert_eq!(metadata.http_service.as_ref().map(|service| service.name()), Some(service_name));
}

#[test]
fn module_metadata_web_service_late_module_file_registration_falls_back_to_whole_config() {
    use crate::metadata::WebServiceEntry;

    fn web_service_xml(name: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <WebService uuid="00000000-0000-0000-0000-000000000072">
        <Properties><Name>{name}</Name><Namespace>urn:test</Namespace></Properties>
        <ChildObjects/>
    </WebService>
</MetaDataObject>"#
        )
    }

    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("cf");
    let service_name = "OrdersSoap";
    let service_path = root.join(format!("WebServices/{service_name}.xml"));
    let module_path = root.join(format!("WebServices/{service_name}/Ext/Module.bsl"));
    std::fs::create_dir_all(module_path.parent().unwrap()).unwrap();
    std::fs::write(root.join("Configuration.xml"), "<Configuration/>").unwrap();
    std::fs::write(&service_path, web_service_xml(service_name)).unwrap();
    std::fs::write(&module_path, "Процедура Операция1() КонецПроцедуры").unwrap();

    let mut db = RootDatabaseImpl::new();
    db.set_all_config_paths(vec![(None, root.clone())]);

    let service_file = FileId(0);
    let module_file = FileId(1);
    let mut file_set = FileSet::new();
    file_set.insert(service_file, VfsPath::new(service_path.to_string_lossy().as_ref()));
    db.set_source_root(SourceRootId(1), SourceRoot::new_local(file_set.clone()));
    db.set_file_source_root(service_file, SourceRootId(1));
    db.set_file_text(service_file, &web_service_xml(service_name));
    db.set_metadata_listing(
        &root.to_string_lossy(),
        MetadataListingData {
            common_attributes: Vec::new(),
            entries: Vec::new(),
            defined_types: Vec::new(),
            common_modules: Vec::new(),
            event_subscriptions: Vec::new(),
            scheduled_jobs: Vec::new(),
            roles: Vec::new(),
            http_services: Vec::new(),
            web_services: vec![WebServiceEntry {
                name: service_name.to_string(),
                main: service_file,
                module_file: None,
            }],
            integration_services: Vec::new(),
            subsystems: Vec::new(),
        },
    );

    file_set.insert(module_file, VfsPath::new(module_path.to_string_lossy().as_ref()));
    db.set_source_root(SourceRootId(1), SourceRoot::new_local(file_set));
    db.set_file_source_root(module_file, SourceRootId(1));
    db.set_file_text(module_file, "Процедура Операция1() КонецПроцедуры");

    let metadata = db.module_metadata(ModuleId::new(module_file));
    assert_eq!(metadata.module_type, bsl_metadata::ModuleType::WebServiceModule);
    assert_eq!(metadata.web_service.as_ref().map(|service| service.name()), Some(service_name));
}

/// Wave 2e: a Subsystem entry in the typed substrate must resolve
/// case-insensitively — the same fold_lower contract the other typed indexes
/// (roles, scheduled jobs, services) already enforce. Red until
/// `SubsystemEntry`, the `subsystems` listing field, and
/// `resolve_subsystem_for_file` / `enumerate_subsystems_for_file` exist.
#[test]
fn subsystem_index_is_case_insensitive() {
    use crate::metadata::SubsystemEntry;

    fn subsystem_xml(name: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns:xr="http://v8.1c.ru/8.3/xcf/readable" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
    <Subsystem uuid="00000000-0000-0000-0000-000000000091">
        <Properties>
            <Name>{name}</Name>
        </Properties>
    </Subsystem>
</MetaDataObject>"#
        )
    }

    let root =
        std::env::temp_dir().join(format!("bsl_subsystem_case_{}_{}", std::process::id(), line!()));
    let sub_path = root.join("Subsystems/МояПодсистема.xml");
    std::fs::create_dir_all(sub_path.parent().unwrap()).unwrap();

    let mut db = RootDatabaseImpl::new();
    let sub_main = FileId(0);
    let consumer_file = FileId(1);
    let consumer_path = root.join("SubsystemConsumer.bsl");

    let mut file_set = FileSet::new();
    file_set.insert(sub_main, VfsPath::new(sub_path.to_string_lossy().as_ref()));
    file_set.insert(consumer_file, VfsPath::new(consumer_path.to_string_lossy().as_ref()));
    db.set_source_root(SourceRootId(1), SourceRoot::new_local(file_set));
    db.set_file_source_root(sub_main, SourceRootId(1));
    db.set_file_source_root(consumer_file, SourceRootId(1));
    db.set_file_text(sub_main, &subsystem_xml("МояПодсистема"));
    db.set_file_text(consumer_file, "Процедура Т() КонецПроцедуры");

    db.set_all_config_paths(vec![(None, root.clone())]);
    db.set_metadata_listing(
        &root.to_string_lossy(),
        MetadataListingData {
            common_attributes: Vec::new(),
            entries: Vec::new(),
            defined_types: Vec::new(),
            common_modules: Vec::new(),
            event_subscriptions: Vec::new(),
            scheduled_jobs: Vec::new(),
            roles: Vec::new(),
            http_services: Vec::new(),
            web_services: Vec::new(),
            integration_services: Vec::new(),
            subsystems: vec![SubsystemEntry {
                name: "МояПодсистема".to_string(), main: sub_main
            }],
        },
    );

    // The declared name is `МояПодсистема`; BSL identifiers are case-insensitive,
    // so every spelling must resolve to the same parsed subsystem (pointer-equal,
    // proving a single parse, not a re-parse per casing).
    let canonical = db
        .resolve_subsystem_for_file(consumer_file, "МояПодсистема")
        .expect("canonical spelling resolves through the bootstrapped substrate");
    assert_eq!(canonical.name(), "МояПодсистема");
    let lower = db
        .resolve_subsystem_for_file(consumer_file, "мояподсистема")
        .expect("lower-case spelling resolves case-insensitively");
    assert!(
        Arc::ptr_eq(&canonical, &lower),
        "lower-case lookup must return the same parsed subsystem"
    );
    let upper = db
        .resolve_subsystem_for_file(consumer_file, "МОЯПОДСИСТЕМА")
        .expect("upper-case spelling resolves case-insensitively");
    assert!(
        Arc::ptr_eq(&canonical, &upper),
        "upper-case lookup must return the same parsed subsystem"
    );
    assert!(
        db.resolve_subsystem_for_file(consumer_file, "НетТакойПодсистемы").is_none(),
        "unknown subsystem name must not resolve"
    );

    let enumerated: Vec<String> = db
        .enumerate_subsystems_for_file(consumer_file)
        .iter()
        .map(|sub| sub.name().to_string())
        .collect();
    assert_eq!(enumerated, vec!["МояПодсистема".to_string()]);

    std::fs::remove_dir_all(&root).ok();
}

/// Wave 2e: the graph consumer `project_workspace_subsystem_edges` must consume
/// the listed Subsystem substrate (not `db.configurations(representative)`) and
/// emit membership edges for base content, extension-added content (overlay
/// merge of a same-named subsystem), and child subsystem membership. The on-disk
/// `Configuration.xml` stubs carry no subsystems, so the only possible source of
/// these edges is the substrate. Red until the `subsystems` listing field,
/// `SubsystemEntry`, and the `enumerate_subsystems` consumer migration exist.
/// The pre-existing `subsystem_membership_links_to_member_objects` fallback
/// (whole-config path) is intentionally left untouched.
#[test]
fn subsystem_membership_from_listed_substrate() {
    use crate::metadata::SubsystemEntry;
    use bsl_metadata::MdoType;
    use hir::call_graph::{EdgeKind, EdgeProvenance, GraphNode};
    use hir::graph_index::{project_workspace_subsystem_edges, GraphBuildState};

    fn subsystem_xml(name: &str, content: &[&str], children: &[&str]) -> String {
        let items = content
            .iter()
            .map(|c| format!("        <xr:Item xsi:type=\"xr:MDObjectRef\">{c}</xr:Item>"))
            .collect::<Vec<_>>()
            .join("\n");
        let child_tags = children
            .iter()
            .map(|c| format!("        <Subsystem>{c}</Subsystem>"))
            .collect::<Vec<_>>()
            .join("\n");
        let items_block =
            if items.is_empty() { String::new() } else { format!("\n{items}\n            ") };
        let children_block =
            if child_tags.is_empty() { String::new() } else { format!("\n{child_tags}\n        ") };
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns:xr="http://v8.1c.ru/8.3/xcf/readable" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
    <Subsystem uuid="00000000-0000-0000-0000-000000000092">
        <Properties>
            <Name>{name}</Name>
            <Content>{items_block}</Content>
        </Properties>
        <ChildObjects>{children_block}</ChildObjects>
    </Subsystem>
</MetaDataObject>"#
        )
    }

    let temp = tempfile::tempdir().unwrap();
    let main_root = temp.path().join("cf");
    let ext_root = temp.path().join("cfe/X");
    std::fs::create_dir_all(main_root.join("Subsystems")).unwrap();
    std::fs::create_dir_all(ext_root.join("Subsystems")).unwrap();
    // Stub configurations with NO subsystems: `db.configurations()` must not be
    // the source of any subsystem edge. The only source is the substrate below.
    std::fs::write(main_root.join("Configuration.xml"), "<Configuration/>").unwrap();
    std::fs::write(ext_root.join("Configuration.xml"), "<Configuration/>").unwrap();

    let base_sub_main = FileId(100);
    let ext_sub_main = FileId(101);
    let consumer_file = FileId(102);

    let mut db = RootDatabaseImpl::new();
    let mut file_set = FileSet::new();
    file_set.insert(
        base_sub_main,
        VfsPath::new(main_root.join("Subsystems/МояПодсистема.xml").to_string_lossy().as_ref()),
    );
    file_set.insert(
        ext_sub_main,
        VfsPath::new(ext_root.join("Subsystems/МояПодсистема.xml").to_string_lossy().as_ref()),
    );
    file_set.insert(
        consumer_file,
        VfsPath::new(main_root.join("SubsystemConsumer.bsl").to_string_lossy().as_ref()),
    );
    db.set_source_root(SourceRootId(1), SourceRoot::new_local(file_set));
    db.set_file_source_root(base_sub_main, SourceRootId(1));
    db.set_file_source_root(ext_sub_main, SourceRootId(1));
    db.set_file_source_root(consumer_file, SourceRootId(1));
    // Base subsystem owns `Catalog.Товары` and a child subsystem `Дочерняя`.
    db.set_file_text(
        base_sub_main,
        &subsystem_xml("МояПодсистема", &["Catalog.Товары"], &["Дочерняя"]),
    );
    // Extension overlay for the SAME subsystem name: adds `Catalog.Услуги`.
    db.set_file_text(ext_sub_main, &subsystem_xml("МояПодсистема", &["Catalog.Услуги"], &[]));
    db.set_file_text(consumer_file, "Процедура Т() КонецПроцедуры");

    db.set_all_config_paths(vec![
        (None, main_root.clone()),
        (Some("X".to_string()), ext_root.clone()),
    ]);
    db.set_metadata_listing(
        &main_root.to_string_lossy(),
        MetadataListingData {
            common_attributes: Vec::new(),
            entries: Vec::new(),
            defined_types: Vec::new(),
            common_modules: Vec::new(),
            event_subscriptions: Vec::new(),
            scheduled_jobs: Vec::new(),
            roles: Vec::new(),
            http_services: Vec::new(),
            web_services: Vec::new(),
            integration_services: Vec::new(),
            subsystems: vec![SubsystemEntry {
                name: "МояПодсистема".to_string(),
                main: base_sub_main,
            }],
        },
    );
    db.set_metadata_listing(
        &ext_root.to_string_lossy(),
        MetadataListingData {
            common_attributes: Vec::new(),
            entries: Vec::new(),
            defined_types: Vec::new(),
            common_modules: Vec::new(),
            event_subscriptions: Vec::new(),
            scheduled_jobs: Vec::new(),
            roles: Vec::new(),
            http_services: Vec::new(),
            web_services: Vec::new(),
            integration_services: Vec::new(),
            subsystems: vec![SubsystemEntry {
                name: "МояПодсистема".to_string(),
                main: ext_sub_main,
            }],
        },
    );

    let mut state = GraphBuildState::new();
    let edges = project_workspace_subsystem_edges(&db, consumer_file, &mut state);

    let membership_to = |to_type: MdoType, to_name: &str| {
        edges.iter().any(|e| {
            e.kind == EdgeKind::SubsystemMembership
                && e.provenance == EdgeProvenance::Resolved
                && matches!(&e.from, GraphNode::Mdo { mdo_type, object_name }
                    if *mdo_type == MdoType::Subsystem && object_name.as_str() == "МояПодсистема")
                && matches!(&e.to, GraphNode::Mdo { mdo_type, object_name }
                    if *mdo_type == to_type && object_name.as_str() == to_name)
        })
    };

    assert!(
        membership_to(MdoType::Catalog, "Товары"),
        "base subsystem content must produce a membership edge from the listed substrate"
    );
    assert!(
        membership_to(MdoType::Catalog, "Услуги"),
        "extension-added content must produce a membership edge after the overlay merge"
    );
    assert!(
        membership_to(MdoType::Subsystem, "Дочерняя"),
        "child subsystem membership must produce a subsystem→subsystem edge"
    );
}

#[test]
fn subsystem_reference_resolver_uses_listed_substrate() {
    use crate::metadata::SubsystemEntry;
    use hir::MetadataReferenceKind;
    use hir_ty::{DbObjectResolver, ObjectResolver};

    fn subsystem_xml(name: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns:xr="http://v8.1c.ru/8.3/xcf/readable" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
    <Subsystem uuid="00000000-0000-0000-0000-000000000093">
        <Properties>
            <Name>{name}</Name>
            <Content/>
        </Properties>
        <ChildObjects/>
    </Subsystem>
</MetaDataObject>"#
        )
    }

    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("cf");
    let ext_root = root.join("ext");
    let subsystem_path = root.join("Subsystems/МояПодсистема.xml");
    let ext_subsystem_path = ext_root.join("Subsystems/МояПодсистема.xml");
    let ext_only_path = ext_root.join("Subsystems/ТолькоРасширение.xml");
    std::fs::create_dir_all(subsystem_path.parent().unwrap()).unwrap();
    std::fs::create_dir_all(ext_subsystem_path.parent().unwrap()).unwrap();
    std::fs::create_dir_all(ext_only_path.parent().unwrap()).unwrap();
    std::fs::write(root.join("Configuration.xml"), "<Configuration/>").unwrap();

    let subsystem_file = FileId(110);
    let ext_subsystem_file = FileId(111);
    let ext_only_file = FileId(112);
    let consumer_file = FileId(113);
    let consumer_path = ext_root.join("SubsystemConsumer.bsl");

    let mut db = RootDatabaseImpl::new();
    let mut file_set = FileSet::new();
    file_set.insert(subsystem_file, VfsPath::new(subsystem_path.to_string_lossy().as_ref()));
    file_set
        .insert(ext_subsystem_file, VfsPath::new(ext_subsystem_path.to_string_lossy().as_ref()));
    file_set.insert(ext_only_file, VfsPath::new(ext_only_path.to_string_lossy().as_ref()));
    file_set.insert(consumer_file, VfsPath::new(consumer_path.to_string_lossy().as_ref()));
    db.set_source_root(SourceRootId(1), SourceRoot::new_local(file_set));
    db.set_file_source_root(subsystem_file, SourceRootId(1));
    db.set_file_source_root(ext_subsystem_file, SourceRootId(1));
    db.set_file_source_root(ext_only_file, SourceRootId(1));
    db.set_file_source_root(consumer_file, SourceRootId(1));
    db.set_file_text(subsystem_file, &subsystem_xml("МояПодсистема"));
    db.set_file_text(ext_subsystem_file, &subsystem_xml("МояПодсистема"));
    db.set_file_text(ext_only_file, &subsystem_xml("ТолькоРасширение"));
    db.set_file_text(consumer_file, "Процедура Т() КонецПроцедуры");

    db.set_all_config_paths(vec![
        (None, root.clone()),
        (Some("Ext".to_string()), ext_root.clone()),
    ]);
    db.set_metadata_listing(
        &root.to_string_lossy(),
        MetadataListingData {
            common_attributes: Vec::new(),
            entries: Vec::new(),
            defined_types: Vec::new(),
            common_modules: Vec::new(),
            event_subscriptions: Vec::new(),
            scheduled_jobs: Vec::new(),
            roles: Vec::new(),
            http_services: Vec::new(),
            web_services: Vec::new(),
            integration_services: Vec::new(),
            subsystems: vec![SubsystemEntry {
                name: "МояПодсистема".to_string(),
                main: subsystem_file,
            }],
        },
    );
    db.set_metadata_listing(
        &ext_root.to_string_lossy(),
        MetadataListingData {
            common_attributes: Vec::new(),
            entries: Vec::new(),
            defined_types: Vec::new(),
            common_modules: Vec::new(),
            event_subscriptions: Vec::new(),
            scheduled_jobs: Vec::new(),
            roles: Vec::new(),
            http_services: Vec::new(),
            web_services: Vec::new(),
            integration_services: Vec::new(),
            subsystems: vec![
                SubsystemEntry {
                    name: "МояПодсистема".to_string(), main: ext_subsystem_file
                },
                SubsystemEntry {
                    name: "ТолькоРасширение".to_string(), main: ext_only_file
                },
            ],
        },
    );

    let resolver = DbObjectResolver::new(&db, consumer_file);
    let resolved = resolver
        .resolve_metadata_reference(MetadataReferenceKind::Subsystem, "МояПодсистема")
        .expect("listed subsystem reference must resolve without whole Configuration data");
    assert_eq!(resolved.as_str(), "МояПодсистема");

    let ext_only_resolved = resolver
        .resolve_metadata_reference(MetadataReferenceKind::Subsystem, "ТолькоРасширение")
        .expect("extension-only subsystem reference must resolve without whole Configuration data");
    assert_eq!(ext_only_resolved.as_str(), "ТолькоРасширение");

    let names = db.subsystem_names_for_file(consumer_file);
    assert_eq!(names, vec!["МояПодсистема".to_string(), "ТолькоРасширение".to_string()]);

    let members = resolver.metadata_reference_members(MetadataReferenceKind::Subsystem);
    let member_names: Vec<String> = members.iter().map(|name| name.as_str().to_string()).collect();
    assert_eq!(member_names, vec!["МояПодсистема".to_string(), "ТолькоРасширение".to_string()]);
}

/// A structure listing with every kind empty, so a test fills only the fields it needs
/// via struct-update syntax (`MetadataListingData { entries: …, ..empty_listing_data() }`).
fn empty_listing_data() -> MetadataListingData {
    MetadataListingData {
        common_attributes: Vec::new(),
        entries: Vec::new(),
        defined_types: Vec::new(),
        common_modules: Vec::new(),
        event_subscriptions: Vec::new(),
        scheduled_jobs: Vec::new(),
        roles: Vec::new(),
        http_services: Vec::new(),
        web_services: Vec::new(),
        integration_services: Vec::new(),
        subsystems: Vec::new(),
    }
}

/// The root-scoped `object`-tool resolver merges base + every extension with no file
/// anchor: a base-only object resolves, an extension-only object resolves (the deliberate
/// widening over today's base-only read), and an object present in both is base composed
/// with the extension overlay — proven by the extension contributing the `Code` standard
/// attribute (from its `CodeLength`) that the base lacks.
#[test]
fn resolve_metadata_object_across_roots_merges_base_and_extension() {
    use crate::metadata::MdoEntry;
    use bsl_metadata::MdoType;

    fn catalog_xml(name: &str, uuid: &str, code_length: Option<u32>) -> String {
        let code = code_length.map(|c| format!("<CodeLength>{c}</CodeLength>")).unwrap_or_default();
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <Catalog uuid="{uuid}">
        <Properties><Name>{name}</Name>{code}</Properties>
    </Catalog>
</MetaDataObject>"#
        )
    }

    let base_only = FileId(0);
    let base_shared = FileId(1);
    let ext_only = FileId(2);
    let ext_shared = FileId(3);

    let mut db = RootDatabaseImpl::new();
    let mut file_set = FileSet::new();
    file_set.insert(base_only, VfsPath::new("/base/Catalogs/ТолькоБаза.xml"));
    file_set.insert(base_shared, VfsPath::new("/base/Catalogs/Общий.xml"));
    file_set.insert(ext_only, VfsPath::new("/ext/Catalogs/ТолькоРасш.xml"));
    file_set.insert(ext_shared, VfsPath::new("/ext/Catalogs/Общий.xml"));
    db.set_source_root(SourceRootId(1), SourceRoot::new_local(file_set));
    for f in [base_only, base_shared, ext_only, ext_shared] {
        db.set_file_source_root(f, SourceRootId(1));
    }
    db.set_file_text(
        base_only,
        &catalog_xml("ТолькоБаза", "00000000-0000-0000-0000-000000000001", None),
    );
    db.set_file_text(
        base_shared,
        &catalog_xml("Общий", "00000000-0000-0000-0000-000000000002", None),
    );
    db.set_file_text(
        ext_only,
        &catalog_xml("ТолькоРасш", "00000000-0000-0000-0000-000000000003", None),
    );
    db.set_file_text(
        ext_shared,
        &catalog_xml("Общий", "00000000-0000-0000-0000-000000000004", Some(9)),
    );

    let cat = |name: &str, main| MdoEntry {
        kind: MdoType::Catalog,
        name: name.to_string(),
        main,
        predefined: None,
    };
    db.set_all_config_paths(vec![
        (None, std::path::PathBuf::from("/base")),
        (Some("Расш".to_string()), std::path::PathBuf::from("/ext")),
    ]);
    db.set_metadata_listing(
        "/base",
        MetadataListingData {
            entries: vec![cat("ТолькоБаза", base_only), cat("Общий", base_shared)],
            ..empty_listing_data()
        },
    );
    db.set_metadata_listing(
        "/ext",
        MetadataListingData {
            entries: vec![cat("ТолькоРасш", ext_only), cat("Общий", ext_shared)],
            ..empty_listing_data()
        },
    );

    assert_eq!(
        db.resolve_metadata_object_across_roots(MdoType::Catalog, "ТолькоБаза").unwrap().name,
        "ТолькоБаза",
        "base-only object resolves",
    );
    assert_eq!(
        db.resolve_metadata_object_across_roots(MdoType::Catalog, "ТолькоРасш").unwrap().name,
        "ТолькоРасш",
        "extension-only object resolves — the widening over today's base-only read",
    );
    let merged = db
        .resolve_metadata_object_across_roots(MdoType::Catalog, "Общий")
        .expect("shared object resolves");
    assert!(
        merged.attributes.iter().any(|a| a.name_en.as_deref() == Some("Code")),
        "the extension overlay must contribute the Code attribute the base lacked: {:?}",
        merged.attributes.iter().map(|a| a.name.clone()).collect::<Vec<_>>(),
    );
    assert!(
        db.resolve_metadata_object_across_roots(MdoType::Catalog, "НетТакого").is_none(),
        "an absent object resolves to None",
    );
}

#[test]
fn effective_metadata_members_keep_topological_winner_and_source() {
    use crate::metadata::{MdoEntry, WorkspaceConfigsSnapshot};
    use crate::EffectiveMetadataMemberValue;
    use bsl_metadata::{AttributeType, MdoType};

    fn catalog_xml(attribute: &str, ty: &str) -> String {
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../bsl-metadata/fixtures/cfe_dependencies/base/Catalogs/Товары.xml"
        ))
        .replace("<Name>Цвет</Name>", &format!("<Name>{attribute}</Name>"))
        .replace("<v8:Type>xs:string</v8:Type>", &format!("<v8:Type>{ty}</v8:Type>"))
    }

    let base = FileId(0);
    let winner = FileId(1);
    let dependency = FileId(2);
    let unique = FileId(3);
    let unread = FileId(4);
    let roots = ["/base", "/winner", "/dependency", "/unique", "/unread"];

    let mut db = RootDatabaseImpl::new();
    let mut file_set = FileSet::new();
    for (file, root) in [base, winner, dependency, unique, unread].into_iter().zip(roots) {
        file_set.insert(file, VfsPath::new(format!("{root}/Catalogs/Товары.xml")));
    }
    db.set_source_root(SourceRootId(1), SourceRoot::new_local(file_set));
    for file in [base, winner, dependency, unique, unread] {
        db.set_file_source_root(file, SourceRootId(1));
    }
    db.set_file_text(base, &catalog_xml("Цвет", "xs:string"));
    db.set_file_text(dependency, &catalog_xml("Цвет", "xs:boolean"));
    db.set_file_text(winner, &catalog_xml("цВет", "xs:decimal"));
    db.set_file_text(unique, &catalog_xml("Вес", "xs:string"));
    db.set_file_text(unread, &catalog_xml("Цвет", "xs:dateTime"));
    db.set_file_unreadable(unread);

    // Declaration order intentionally puts the dependent before its dependency.
    // The explicit topology must still compose dependency first, dependent last.
    let paths: Vec<(Option<String>, std::path::PathBuf)> = vec![
        (None, roots[0].into()),
        (Some("Победитель".to_string()), roots[1].into()),
        (Some("Зависимость".to_string()), roots[2].into()),
        (Some("Уникальное".to_string()), roots[3].into()),
        (Some("Нечитаемое".to_string()), roots[4].into()),
    ];
    db.set_workspace_configs_snapshot(WorkspaceConfigsSnapshot {
        canonical_paths: paths.iter().map(|(_, path)| path.clone()).collect(),
        kinds: paths
            .iter()
            .map(|(label, _)| if label.is_none() { RootKind::Base } else { RootKind::Extension })
            .collect(),
        paths,
        closures: vec![vec![], vec![2], vec![], vec![], vec![]],
        topological_order: vec![0, 2, 1, 3, 4],
        fingerprint: Some("test-topology".to_string()),
        source_exclusions: Default::default(),
    });
    let listing = |main| MetadataListingData {
        entries: vec![MdoEntry {
            kind: MdoType::Catalog,
            name: "Товары".to_string(),
            main,
            predefined: None,
        }],
        ..empty_listing_data()
    };
    for (root, file) in roots.into_iter().zip([base, winner, dependency, unique, unread]) {
        db.set_metadata_listing(root, listing(file));
    }

    let base_listing = db.metadata_listing("/base").unwrap();
    let base_object_before = crate::metadata::resolve_metadata_object(
        &db,
        base_listing,
        MdoType::Catalog,
        "Товары".to_string(),
    )
    .unwrap();

    let members = db
        .effective_metadata_members_across_roots(MdoType::Catalog, "Товары")
        .expect("object exists in readable roots");
    let colors: Vec<_> = members
        .iter()
        .filter(|member| stdx::case::eq_ignore_case(member.member.name(), "Цвет"))
        .collect();
    assert_eq!(colors.len(), 1, "Unicode-case-equivalent overlays replace instead of duplicating");
    let color = colors[0];
    assert_eq!(color.source_extension.as_deref(), Some("Победитель"));
    assert!(matches!(
        &color.member,
        EffectiveMetadataMemberValue::Attribute(attribute)
            if matches!(attribute.attr_type, AttributeType::Number { .. })
    ));
    let weights: Vec<_> = members.iter().filter(|member| member.member.name() == "Вес").collect();
    assert_eq!(weights.len(), 1, "a unique extension member is present once");
    let weight = weights[0];
    assert_eq!(weight.source_extension.as_deref(), Some("Уникальное"));

    let base_visible = db
        .effective_metadata_members_for_file(base, MdoType::Catalog, "Товары")
        .expect("base object is visible from its own root");
    let base_color = base_visible.iter().find(|member| member.member.name() == "Цвет").unwrap();
    assert_eq!(base_color.source_extension, None);
    assert!(matches!(
        &base_color.member,
        EffectiveMetadataMemberValue::Attribute(attribute)
            if matches!(attribute.attr_type, AttributeType::String { .. })
    ));

    let visible = db
        .effective_metadata_members_for_file(winner, MdoType::Catalog, "Товары")
        .expect("dependent root sees base, dependency and itself");
    assert!(visible.iter().all(|member| member.member.name() != "Вес"));
    let visible_color = visible
        .iter()
        .find(|member| stdx::case::eq_ignore_case(member.member.name(), "Цвет"))
        .unwrap();
    assert_eq!(visible_color.source_extension.as_deref(), Some("Победитель"));

    let base_object_after = crate::metadata::resolve_metadata_object(
        &db,
        base_listing,
        MdoType::Catalog,
        "Товары".to_string(),
    )
    .unwrap();
    assert!(
        Arc::ptr_eq(&base_object_before, &base_object_after),
        "effective enumeration must reuse the per-MDO parsed query"
    );
}

#[test]
fn effective_module_exports_cover_composition_matrix_for_all_module_roles() {
    use crate::effective_exports::{
        effective_module_exports_query, EffectiveModuleRole, ExportComposition,
    };
    use crate::metadata::WorkspaceConfigsSnapshot;
    use bsl_metadata::MdoType;
    use stdx::case::CaseExt;

    const BASE: &str = r#"
Перем БазоваяПеременная Экспорт;
Процедура Базовая() Экспорт
КонецПроцедуры
Процедура Изменяемый() Экспорт
КонецПроцедуры
Процедура Замещаемый() Экспорт
КонецПроцедуры
Процедура До() Экспорт
КонецПроцедуры
Процедура После() Экспорт
КонецПроцедуры
"#;
    const EARLY: &str = r#"
Перем РанняяПеременная Экспорт;
Процедура ТолькоРаннее() Экспорт
КонецПроцедуры
&ИзменениеИКонтроль("Изменяемый")
Процедура РаннееИзменение()
#Вставка
    А = 1;
#КонецВставки
КонецПроцедуры
&Вместо("Замещаемый")
Процедура РаннееВместо()
КонецПроцедуры
&Перед("До")
Процедура РаннееПеред()
КонецПроцедуры
&После("После")
Процедура РаннееПосле()
КонецПроцедуры
"#;
    const LATE: &str = r#"
Перем ПоздняяПеременная Экспорт;
Процедура ТолькоПозднее() Экспорт
КонецПроцедуры
&ИзменениеИКонтроль("Изменяемый")
Процедура ПозднееИзменение()
#Вставка
    А = 2;
#КонецВставки
КонецПроцедуры
&Вместо("Замещаемый")
Процедура ПозднееВместо()
КонецПроцедуры
"#;
    const UNREAD: &str = "Процедура НеДолженПоявиться() Экспорт\nКонецПроцедуры";

    let roots = ["/base", "/late", "/early", "/unread"];
    let paths: Vec<(Option<String>, std::path::PathBuf)> = vec![
        (None, roots[0].into()),
        (Some("Позднее".to_string()), roots[1].into()),
        (Some("Раннее".to_string()), roots[2].into()),
        (Some("Нечитаемое".to_string()), roots[3].into()),
    ];
    let mut db = RootDatabaseImpl::new();
    db.set_workspace_configs_snapshot(WorkspaceConfigsSnapshot {
        canonical_paths: paths.iter().map(|(_, path)| path.clone()).collect(),
        kinds: paths
            .iter()
            .map(|(label, _)| if label.is_none() { RootKind::Base } else { RootKind::Extension })
            .collect(),
        paths,
        // `Позднее` depends on `Раннее`, despite being declared first.
        closures: vec![vec![], vec![2], vec![], vec![]],
        topological_order: vec![0, 2, 1, 3],
        fingerprint: Some("exports-topology".to_string()),
        source_exclusions: Default::default(),
    });

    let relative_paths = [
        "Catalogs/Товары/Ext/ObjectModule.bsl",
        "Catalogs/Товары/Ext/ManagerModule.bsl",
        "Catalogs/Товары/Forms/Форма/Ext/Form/Module.bsl",
    ];
    let mut file_set = FileSet::new();
    let mut enrolled = Vec::new();
    let mut next = 0u32;
    for relative in relative_paths {
        for (root_idx, root) in roots.iter().enumerate() {
            let file = FileId(next);
            next += 1;
            file_set.insert(file, VfsPath::new(format!("{root}/{relative}")));
            enrolled.push((file, root_idx));
        }
    }
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    for &(file, root_idx) in &enrolled {
        db.set_file_source_root(file, SourceRootId(0));
        db.set_file_text(file, [BASE, LATE, EARLY, UNREAD][root_idx]);
        if root_idx == 3 {
            db.set_file_unreadable(file);
        }
    }

    for role in [
        EffectiveModuleRole::Object,
        EffectiveModuleRole::Manager,
        EffectiveModuleRole::ManagedForm,
    ] {
        let exports = effective_module_exports_query(
            &db,
            SourceRootId(0),
            None,
            role,
            MdoType::Catalog,
            "Товары".to_string(),
            (role == EffectiveModuleRole::ManagedForm).then(|| "Форма".to_string()),
        );
        let named = |name: &str| {
            exports
                .methods
                .iter()
                .filter(|method| method.name.as_str().fold_lower() == name.fold_lower())
                .collect::<Vec<_>>()
        };

        assert_eq!(named("Базовая").len(), 1, "{role:?}: base export");
        assert_eq!(named("ТолькоРаннее")[0].source_extension.as_deref(), Some("Раннее"));
        assert_eq!(named("ТолькоПозднее")[0].source_extension.as_deref(), Some("Позднее"));

        let changed = named("Изменяемый");
        assert_eq!(changed.len(), 1, "{role:?}: replaced base is not effective");
        assert_eq!(changed[0].source_extension.as_deref(), Some("Позднее"));
        assert_eq!(changed[0].composition, ExportComposition::ChangeAndValidate);
        assert_eq!(changed[0].method.name.as_str(), "ПозднееИзменение");

        let instead = named("Замещаемый");
        assert_eq!(instead.len(), 1, "{role:?}: later &Вместо wins");
        assert_eq!(instead[0].source_extension.as_deref(), Some("Позднее"));
        assert_eq!(instead[0].composition, ExportComposition::Instead);
        assert_eq!(instead[0].method.name.as_str(), "ПозднееВместо");

        let before = named("До");
        assert_eq!(before.len(), 2, "{role:?}: base and &Перед stay separate");
        assert!(before.iter().any(|method| method.composition == ExportComposition::Base));
        assert!(before.iter().any(|method| {
            method.composition == ExportComposition::Before
                && method.source_extension.as_deref() == Some("Раннее")
        }));
        let after = named("После");
        assert_eq!(after.len(), 2, "{role:?}: base and &После stay separate");
        assert!(after.iter().any(|method| method.composition == ExportComposition::After));

        for internal in [
            "РаннееИзменение",
            "ПозднееИзменение",
            "РаннееВместо",
            "ПозднееВместо",
            "РаннееПеред",
            "РаннееПосле",
            "НеДолженПоявиться",
        ] {
            assert!(named(internal).is_empty(), "{role:?}: internal name {internal} leaked");
        }
        let variables: Vec<_> =
            exports.variables.iter().map(|variable| variable.variable.name.as_str()).collect();
        assert!(variables.contains(&"БазоваяПеременная"));
        assert!(variables.contains(&"РанняяПеременная"));
        assert!(variables.contains(&"ПоздняяПеременная"));
    }
}

#[test]
fn ba010_effective_module_variables_resolve_for_object_and_manager_facets() {
    use crate::metadata::WorkspaceConfigsSnapshot;
    use bsl_metadata::MdoType;
    use bsl_types::testing::RootConfigCtx;
    use hir::{Builders, HirDatabase, InferenceDiagnostic, MetadataKind, Name};
    use hir_ty::{lookup_field, lookup_manager_field, DbObjectResolver};

    const BASE_OBJECT: &str = "Перем БазоваяОбъекта Экспорт; Перем ПриватнаяБазоваяОбъекта;";
    const EARLY_OBJECT: &str = "Перем РанняяОбъекта Экспорт; Перем ПриватнаяРанняяОбъекта;";
    const LATE_OBJECT: &str = r#"
Перем ПоздняяОбъекта Экспорт;
Процедура Проверка()
    А = ЭтотОбъект.БазоваяОбъекта;
    Б = ЭтотОбъект.РанняяОбъекта;
    В = ЭтотОбъект.ПоздняяОбъекта;
КонецПроцедуры
"#;
    const BASE_MANAGER: &str = "Перем БазоваяМенеджера Экспорт;";
    const EARLY_MANAGER: &str = "Перем РанняяМенеджера Экспорт;";
    const LATE_MANAGER: &str = "Перем ПоздняяМенеджера Экспорт;";

    let paths: Vec<(Option<String>, std::path::PathBuf)> = vec![
        (None, "/base".into()),
        (Some("Раннее".to_string()), "/early".into()),
        (Some("Позднее".to_string()), "/late".into()),
    ];
    let mut db = RootDatabaseImpl::new();
    db.set_workspace_configs_snapshot(WorkspaceConfigsSnapshot {
        canonical_paths: paths.iter().map(|(_, path)| path.clone()).collect(),
        kinds: paths
            .iter()
            .map(|(label, _)| if label.is_none() { RootKind::Base } else { RootKind::Extension })
            .collect(),
        paths,
        closures: vec![vec![], vec![], vec![1]],
        topological_order: vec![0, 1, 2],
        fingerprint: Some("ba010".to_string()),
        source_exclusions: Default::default(),
    });

    let files = [
        (FileId(0), "/base/Catalogs/Товары/Ext/ObjectModule.bsl", BASE_OBJECT),
        (FileId(1), "/early/Catalogs/Товары/Ext/ObjectModule.bsl", EARLY_OBJECT),
        (FileId(2), "/late/Catalogs/Товары/Ext/ObjectModule.bsl", LATE_OBJECT),
        (FileId(3), "/base/Catalogs/Товары/Ext/ManagerModule.bsl", BASE_MANAGER),
        (FileId(4), "/early/Catalogs/Товары/Ext/ManagerModule.bsl", EARLY_MANAGER),
        (FileId(5), "/late/Catalogs/Товары/Ext/ManagerModule.bsl", LATE_MANAGER),
    ];
    let mut file_set = FileSet::new();
    for (file, path, _) in files {
        file_set.insert(file, VfsPath::new(path));
    }
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    for (file, _, text) in files {
        db.set_file_source_root(file, SourceRootId(0));
        db.set_file_text(file, text);
    }

    let object_ty =
        db.metadata_ref(MetadataKind::CatalogObject, "Товары".to_string(), &RootConfigCtx);
    let late_object_resolver = DbObjectResolver::new(&db, FileId(2));
    for name in ["БазоваяОбъекта", "РанняяОбъекта", "ПоздняяОбъекта"]
    {
        assert!(
            lookup_field(&db, &late_object_resolver, object_ty, &Name::new(name)).is_some(),
            "late object facet must see {name}"
        );
    }
    for name in ["ПриватнаяБазоваяОбъекта", "ПриватнаяРанняяОбъекта"]
    {
        assert!(
            lookup_field(&db, &late_object_resolver, object_ty, &Name::new(name)).is_none(),
            "non-exported object-module variable must stay private: {name}"
        );
    }
    assert!(
        lookup_field(
            &db,
            &DbObjectResolver::new(&db, FileId(0)),
            object_ty,
            &Name::new("РанняяОбъекта"),
        )
        .is_none(),
        "base object facet must not see an extension variable"
    );
    assert!(
        lookup_field(
            &db,
            &DbObjectResolver::new(&db, FileId(0)),
            object_ty,
            &Name::new("ПриватнаяБазоваяОбъекта"),
        )
        .is_none(),
        "non-exported base object-module variable stays private too"
    );

    let manager_ty = db.object_manager(MdoType::Catalog, "Товары".to_string(), &RootConfigCtx);
    let late_manager_resolver = DbObjectResolver::new(&db, FileId(5));
    for name in ["БазоваяМенеджера", "РанняяМенеджера", "ПоздняяМенеджера"]
    {
        assert!(
            lookup_manager_field(&db, &late_manager_resolver, manager_ty, &Name::new(name))
                .is_some(),
            "late manager facet must see {name}"
        );
    }
    assert!(
        lookup_manager_field(
            &db,
            &DbObjectResolver::new(&db, FileId(3)),
            manager_ty,
            &Name::new("РанняяМенеджера"),
        )
        .is_none(),
        "base manager facet must not see an extension variable"
    );

    let unresolved: Vec<_> = db
        .infer(FileId(2))
        .diagnostics
        .iter()
        .filter_map(|(_, diagnostic)| match diagnostic {
            InferenceDiagnostic::UnresolvedField { field_name, .. } => {
                Some(field_name.as_str().to_string())
            }
            _ => None,
        })
        .collect();
    assert!(unresolved.is_empty(), "BA-010 false UnresolvedField: {unresolved:?}");
}

#[test]
fn ba021_module_exports_follow_the_callers_extension_visibility_chain() {
    use crate::effective_exports::{effective_module_exports_query, EffectiveModuleRole};
    use crate::metadata::WorkspaceConfigsSnapshot;
    use bsl_metadata::MdoType;

    let roots = ["/base", "/dependency", "/consumer", "/unrelated"];
    let paths: Vec<(Option<String>, std::path::PathBuf)> = vec![
        (None, roots[0].into()),
        (Some("Зависимость".to_string()), roots[1].into()),
        (Some("Потребитель".to_string()), roots[2].into()),
        (Some("Стороннее".to_string()), roots[3].into()),
    ];
    let mut db = RootDatabaseImpl::new();
    db.set_workspace_configs_snapshot(WorkspaceConfigsSnapshot {
        canonical_paths: paths.iter().map(|(_, path)| path.clone()).collect(),
        kinds: paths
            .iter()
            .map(|(label, _)| if label.is_none() { RootKind::Base } else { RootKind::Extension })
            .collect(),
        paths,
        closures: vec![vec![], vec![], vec![1], vec![]],
        topological_order: vec![0, 1, 2, 3],
        fingerprint: Some("ba021-visibility".to_string()),
        source_exclusions: Default::default(),
    });

    let source = [
        "Процедура База() Экспорт\nКонецПроцедуры",
        "Процедура ИзЗависимости() Экспорт\nКонецПроцедуры",
        "Процедура Вызов()\n    Товары.ИзЗависимости();\nКонецПроцедуры",
        "Процедура СтороннийЭкспорт() Экспорт\nКонецПроцедуры",
    ];
    let mut file_set = FileSet::new();
    let mut caller = FileId(0);
    for (index, root) in roots.iter().enumerate() {
        let file = FileId(index as u32);
        if index == 2 {
            caller = file;
        }
        file_set.insert(file, VfsPath::new(format!("{root}/Catalogs/Товары/Ext/ObjectModule.bsl")));
    }
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    for (index, text) in source.into_iter().enumerate() {
        let file = FileId(index as u32);
        db.set_file_source_root(file, SourceRootId(0));
        db.set_file_text(file, text);
    }

    let exports = effective_module_exports_query(
        &db,
        SourceRootId(0),
        Some(caller),
        EffectiveModuleRole::Object,
        MdoType::Catalog,
        "Товары".to_string(),
        None,
    );
    let names =
        exports.methods.iter().map(|method| method.method.name.as_str()).collect::<Vec<_>>();
    assert!(names.contains(&"База"), "base exports remain visible: {names:?}");
    assert!(names.contains(&"ИзЗависимости"), "explicit dependency exports resolve: {names:?}");
    assert!(
        !names.contains(&"СтороннийЭкспорт"),
        "unrelated installed extension stays invisible: {names:?}"
    );
}

/// A register kind reached through the root-scoped resolver: an extension-only register is
/// found (base listing empty), confirming registers participate in the base + extension
/// scan just like objects.
#[test]
fn resolve_register_across_roots_finds_extension_register() {
    use crate::metadata::MdoEntry;
    use bsl_metadata::MdoType;

    let reg_file = FileId(0);
    let mut db = RootDatabaseImpl::new();
    let mut file_set = FileSet::new();
    file_set.insert(reg_file, VfsPath::new("/ext/InformationRegisters/РегистрСведений1.xml"));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    db.set_file_source_root(reg_file, SourceRootId(0));
    db.set_file_text(
        reg_file,
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../bsl-metadata/fixtures/designer/InformationRegisters/РегистрСведений1.xml"
        )),
    );

    db.set_all_config_paths(vec![
        (None, std::path::PathBuf::from("/base")),
        (Some("Расш".to_string()), std::path::PathBuf::from("/ext")),
    ]);
    db.set_metadata_listing("/base", empty_listing_data());
    db.set_metadata_listing(
        "/ext",
        MetadataListingData {
            entries: vec![MdoEntry {
                kind: MdoType::InformationRegister,
                name: "РегистрСведений1".to_string(),
                main: reg_file,
                predefined: None,
            }],
            ..empty_listing_data()
        },
    );

    let reg = db
        .resolve_register_across_roots(MdoType::InformationRegister, "РегистрСведений1")
        .expect("extension-only register resolves across roots");
    assert_eq!(reg.name(), "РегистрСведений1");
    assert_eq!(reg.mdo_type(), MdoType::InformationRegister);
    assert!(
        db.resolve_register_across_roots(MdoType::InformationRegister, "НетТакого").is_none(),
        "an absent register resolves to None",
    );
}

/// A cold kind (event subscription, which carries no extension overlay) through the
/// root-scoped resolver: base and extension-only subscriptions both resolve, base taking
/// precedence on a name collision.
#[test]
fn resolve_event_subscription_across_roots_surfaces_base_and_extension() {
    use crate::metadata::EventSubscriptionEntry;

    fn sub_xml(name: &str, event: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <EventSubscription uuid="00000000-0000-0000-0000-000000000052">
        <Properties>
            <Name>{name}</Name>
            <Source><Type>CatalogRef.Номенклатура</Type></Source>
            <Event>{event}</Event>
            <Handler>CommonModule.Подписки.{name}</Handler>
        </Properties>
    </EventSubscription>
</MetaDataObject>"#
        )
    }

    let base_sub = FileId(0);
    let ext_sub = FileId(1);
    let mut db = RootDatabaseImpl::new();
    let mut file_set = FileSet::new();
    file_set.insert(base_sub, VfsPath::new("/base/EventSubscriptions/ПодпискаБаза.xml"));
    file_set.insert(ext_sub, VfsPath::new("/ext/EventSubscriptions/ПодпискаРасш.xml"));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    db.set_file_source_root(base_sub, SourceRootId(0));
    db.set_file_source_root(ext_sub, SourceRootId(0));
    db.set_file_text(base_sub, &sub_xml("ПодпискаБаза", "BeforeWrite"));
    db.set_file_text(ext_sub, &sub_xml("ПодпискаРасш", "OnWrite"));

    db.set_all_config_paths(vec![
        (None, std::path::PathBuf::from("/base")),
        (Some("Расш".to_string()), std::path::PathBuf::from("/ext")),
    ]);
    db.set_metadata_listing(
        "/base",
        MetadataListingData {
            event_subscriptions: vec![EventSubscriptionEntry {
                name: "ПодпискаБаза".to_string(),
                main: base_sub,
            }],
            ..empty_listing_data()
        },
    );
    db.set_metadata_listing(
        "/ext",
        MetadataListingData {
            event_subscriptions: vec![EventSubscriptionEntry {
                name: "ПодпискаРасш".to_string(),
                main: ext_sub,
            }],
            ..empty_listing_data()
        },
    );

    let base = db
        .resolve_event_subscription_across_roots("ПодпискаБаза")
        .expect("base subscription resolves");
    assert_eq!(base.event(), "BeforeWrite");
    let ext = db
        .resolve_event_subscription_across_roots("ПодпискаРасш")
        .expect("extension-only subscription resolves");
    assert_eq!(ext.event(), "OnWrite");
    assert!(
        db.resolve_event_subscription_across_roots("НетТакой").is_none(),
        "an absent subscription resolves to None",
    );
}

/// The register overlay-fold branch: a register present in BOTH base and an extension is
/// base composed with the extension overlay — the extension contributes a dimension the
/// base lacked (via `Register::apply_extension_overlay`), not just a base-only read.
#[test]
fn resolve_register_across_roots_folds_extension_overlay() {
    use crate::metadata::MdoEntry;
    use bsl_metadata::MdoType;

    let base_reg = FileId(0);
    let ext_reg = FileId(1);
    let mut db = RootDatabaseImpl::new();
    let mut file_set = FileSet::new();
    file_set.insert(base_reg, VfsPath::new("/base/InformationRegisters/РегистрСведений1.xml"));
    file_set.insert(ext_reg, VfsPath::new("/ext/InformationRegisters/РегистрСведений1.xml"));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    db.set_file_source_root(base_reg, SourceRootId(0));
    db.set_file_source_root(ext_reg, SourceRootId(0));
    // Base register carries no dimension.
    db.set_file_text(
        base_reg,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <InformationRegister uuid="59f8d329-f39c-4999-b470-ae9fc74511ac">
        <Properties><Name>РегистрСведений1</Name></Properties>
    </InformationRegister>
</MetaDataObject>"#,
    );
    // The extension's copy of the register adds the Справочник1 dimension (real fixture).
    db.set_file_text(
        ext_reg,
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../bsl-metadata/fixtures/designer/InformationRegisters/РегистрСведений1.xml"
        )),
    );

    let entry = |main| MdoEntry {
        kind: MdoType::InformationRegister,
        name: "РегистрСведений1".to_string(),
        main,
        predefined: None,
    };
    db.set_all_config_paths(vec![
        (None, std::path::PathBuf::from("/base")),
        (Some("Расш".to_string()), std::path::PathBuf::from("/ext")),
    ]);
    db.set_metadata_listing(
        "/base",
        MetadataListingData { entries: vec![entry(base_reg)], ..empty_listing_data() },
    );
    db.set_metadata_listing(
        "/ext",
        MetadataListingData { entries: vec![entry(ext_reg)], ..empty_listing_data() },
    );

    let merged = db
        .resolve_register_across_roots(MdoType::InformationRegister, "РегистрСведений1")
        .expect("register resolves across roots");
    assert!(
        merged.dimensions().iter().any(|d| d.name() == "Справочник1"),
        "the extension overlay must contribute the dimension the base lacked: {:?}",
        merged.dimensions().iter().map(|d| d.name().to_string()).collect::<Vec<_>>(),
    );
}

struct ExtensionMetadataDbFixture {
    db: RootDatabaseImpl,
    consumer: FileId,
    base_document_xml: FileId,
    extension_document_xml: FileId,
    base_common_xml: FileId,
    extension_common_xml: FileId,
    base_common_module: FileId,
    extension_common_module: FileId,
    extension_session_module: FileId,
    owner_shaped_impostor: FileId,
    unrelated_module: FileId,
}

fn extension_metadata_db_fixture(with_listings: bool) -> ExtensionMetadataDbFixture {
    use crate::metadata::{CommonModuleEntry, MdoEntry};
    use bsl_metadata::MdoType;

    let fixture = std::path::PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../bsl-metadata/fixtures/extension_metadata"
    ));
    let base = fixture.join("base");
    let extension = fixture.join("extension");

    let consumer = FileId(0);
    let base_document_xml = FileId(1);
    let extension_document_xml = FileId(2);
    let base_common_xml = FileId(3);
    let extension_common_xml = FileId(4);
    let base_common_module = FileId(5);
    let extension_common_module = FileId(6);
    let unrelated_module = FileId(7);
    let base_session_xml = FileId(8);
    let extension_session_xml = FileId(9);
    let base_session_module = FileId(10);
    let extension_session_module = FileId(11);
    let owner_shaped_impostor = FileId(12);

    let paths = [
        (consumer, extension.join("Documents/Заказ/Ext/ObjectModule.bsl")),
        (base_document_xml, base.join("Documents/Заказ.xml")),
        (extension_document_xml, extension.join("Documents/Заказ.xml")),
        (base_common_xml, base.join("CommonModules/Сервер.xml")),
        (extension_common_xml, extension.join("CommonModules/Сервер.xml")),
        (base_common_module, base.join("CommonModules/Сервер/Ext/Module.bsl")),
        (extension_common_module, extension.join("CommonModules/Сервер/Ext/Module.bsl")),
        (base_session_xml, base.join("CommonModules/СерверСеанса.xml")),
        (extension_session_xml, extension.join("CommonModules/СерверСеанса.xml")),
        (base_session_module, base.join("CommonModules/СерверСеанса/Ext/Module.bsl")),
        (extension_session_module, extension.join("CommonModules/СерверСеанса/Ext/Module.bsl")),
        (owner_shaped_impostor, extension.join("CommonModules/Сервер/Ext/UnreadModule.bsl")),
        (unrelated_module, extension.join("Other/Module.bsl")),
    ];

    let mut db = RootDatabaseImpl::new();
    let mut file_set = FileSet::new();
    for (file_id, path) in &paths {
        file_set.insert(*file_id, VfsPath::new(path.to_string_lossy().as_ref()));
    }
    db.set_source_root(SourceRootId(1), SourceRoot::new_local(file_set));
    for (file_id, path) in &paths {
        db.set_file_source_root(*file_id, SourceRootId(1));
        let text = std::fs::read_to_string(path)
            .unwrap_or_else(|_| "Процедура Тест() КонецПроцедуры".to_string());
        db.set_file_text(*file_id, &text);
    }
    db.set_all_config_paths(vec![
        (None, base.clone()),
        (Some("Расширение".to_string()), extension.clone()),
    ]);

    if with_listings {
        db.set_metadata_listing(
            &base.to_string_lossy(),
            MetadataListingData {
                entries: vec![MdoEntry {
                    kind: MdoType::Document,
                    name: "Заказ".to_string(),
                    main: base_document_xml,
                    predefined: None,
                }],
                common_modules: vec![
                    CommonModuleEntry {
                        name: "Сервер".to_string(),
                        main: base_common_xml,
                        module_file: Some(base_common_module),
                        unread_module_file: None,
                    },
                    CommonModuleEntry {
                        name: "СерверСеанса".to_string(),
                        main: base_session_xml,
                        module_file: Some(base_session_module),
                        unread_module_file: None,
                    },
                ],
                ..empty_listing_data()
            },
        );
        db.set_metadata_listing(
            &extension.to_string_lossy(),
            MetadataListingData {
                entries: vec![MdoEntry {
                    kind: MdoType::Document,
                    name: "Заказ".to_string(),
                    main: extension_document_xml,
                    predefined: None,
                }],
                common_modules: vec![
                    CommonModuleEntry {
                        name: "Сервер".to_string(),
                        main: extension_common_xml,
                        module_file: Some(extension_common_module),
                        unread_module_file: None,
                    },
                    CommonModuleEntry {
                        name: "СерверСеанса".to_string(),
                        main: extension_session_xml,
                        module_file: Some(extension_session_module),
                        unread_module_file: None,
                    },
                ],
                ..empty_listing_data()
            },
        );
    }

    ExtensionMetadataDbFixture {
        db,
        consumer,
        base_document_xml,
        extension_document_xml,
        base_common_xml,
        extension_common_xml,
        base_common_module,
        extension_common_module,
        extension_session_module,
        owner_shaped_impostor,
        unrelated_module,
    }
}

fn extension_metadata_field_names(db: &RootDatabaseImpl, consumer: FileId) -> Vec<String> {
    db.resolve_metadata_object_for_file(consumer, bsl_metadata::MdoType::Document, "Заказ")
        .expect("effective Заказ")
        .find_tabular_section("Товары")
        .expect("effective Товары")
        .attributes()
        .iter()
        .map(|attribute| attribute.name().to_string())
        .collect()
}

fn extension_metadata_field_types(
    db: &RootDatabaseImpl,
    consumer: FileId,
) -> Vec<(String, bsl_metadata::AttributeType)> {
    db.resolve_metadata_object_for_file(consumer, bsl_metadata::MdoType::Document, "Заказ")
        .expect("effective Заказ")
        .find_tabular_section("Товары")
        .expect("effective Товары")
        .attributes()
        .iter()
        .map(|attribute| (attribute.name().to_string(), attribute.attr_type().clone()))
        .collect()
}

#[test]
fn extension_metadata_resolvers_match_substrate_and_filesystem_fallback() {
    use bsl_metadata::traits::{MdObject, Module};

    let substrate = extension_metadata_db_fixture(true);
    let fallback = extension_metadata_db_fixture(false);

    for (fixture, expects_uri) in [(&substrate, false), (&fallback, true)] {
        assert_eq!(
            extension_metadata_field_names(&fixture.db, fixture.consumer),
            ["Номенклатура", "Количество", "РасшПоле"],
        );
        assert_eq!(
            extension_metadata_field_types(&fixture.db, fixture.consumer),
            [
                (
                    "Номенклатура".to_string(),
                    bsl_metadata::AttributeType::String { length: Some(50) },
                ),
                (
                    "Количество".to_string(),
                    bsl_metadata::AttributeType::String { length: Some(15) },
                ),
                ("РасшПоле".to_string(), bsl_metadata::AttributeType::String { length: Some(25) },),
            ]
        );
        let document = fixture
            .db
            .resolve_metadata_object_for_file(
                fixture.consumer,
                bsl_metadata::MdoType::Document,
                "Заказ",
            )
            .unwrap();
        assert!(document.find_attribute("БазовыйРеквизит").is_some());
        assert!(document.find_attribute("РасшРеквизит").is_some());
        let upper_document = fixture
            .db
            .resolve_metadata_object_for_file(
                fixture.consumer,
                bsl_metadata::MdoType::Document,
                "ЗАКАЗ",
            )
            .expect("Cyrillic case variant resolves the effective document");
        let upper_section = upper_document
            .find_tabular_section("ТОВАРЫ")
            .expect("Cyrillic case variant resolves the effective tabular section");
        assert!(upper_section
            .attributes()
            .iter()
            .any(|attribute| stdx::case::eq_ignore_case(attribute.name(), "НОМЕНКЛАТУРА")));

        let common = fixture
            .db
            .resolve_common_module_for_file(fixture.extension_common_module, "Сервер")
            .expect("effective common module");
        assert_eq!(common.name(), "Сервер");
        assert_eq!(
            common.uuid().to_string(),
            "15500000-0000-0000-0000-000000000120",
            "effective identity comes from the selected extension overlay"
        );
        assert_eq!(
            common.return_values_reuse(),
            bsl_metadata::ReturnValueReuse::DontUse,
            "the missing extension property inherits the base value"
        );
        assert!(!common.is_server(), "the extension's explicit false wins");
        assert!(!common.is_global(), "the extension's explicit false wins");
        assert!(common.is_client_managed_application());
        assert!(common.is_client_ordinary_application());
        assert!(!common.is_external_connection(), "the extension's explicit false wins");
        assert!(!common.is_server_call(), "the extension's explicit false wins");
        assert!(common.is_privileged());
        let upper_common = fixture
            .db
            .resolve_common_module_for_file(fixture.extension_common_module, "СЕРВЕР")
            .expect("Cyrillic case variant resolves the effective common module");
        assert_eq!(upper_common.as_ref(), common.as_ref());

        let by_file = fixture
            .db
            .common_module_for_file_id(fixture.extension_common_module)
            .expect("module owner resolves by file");
        assert_eq!(by_file.as_ref(), common.as_ref(), "by-name and by-file must agree fully");
        let metadata = fixture.db.module_metadata(ModuleId::new(fixture.extension_common_module));
        assert_eq!(metadata.common_module.as_deref(), Some(common.as_ref()));
        assert_eq!(
            metadata.execution_context,
            Some(hir::ExecutionContext::Client),
            "execution context must be computed from the effective overlay, not the server base"
        );

        let inherited_session = fixture
            .db
            .resolve_common_module_for_file(fixture.extension_session_module, "СерверСеанса")
            .expect("DuringSession module resolves through the borrowed overlay");
        assert_eq!(
            inherited_session.return_values_reuse(),
            bsl_metadata::ReturnValueReuse::DuringSession
        );
        let session_by_file = fixture
            .db
            .common_module_for_file_id(fixture.extension_session_module)
            .expect("borrowed DuringSession module resolves by file");
        assert_eq!(session_by_file.as_ref(), inherited_session.as_ref());
        assert_eq!(
            fixture
                .db
                .module_metadata(ModuleId::new(fixture.extension_session_module))
                .common_module
                .as_deref(),
            Some(inherited_session.as_ref())
        );

        let base_by_name = fixture
            .db
            .resolve_common_module_for_file(fixture.base_common_module, "Сервер")
            .expect("base module resolves by name");
        let base_by_file = fixture
            .db
            .common_module_for_file_id(fixture.base_common_module)
            .expect("base module resolves by file");
        assert_eq!(base_by_name.uuid().to_string(), "15500000-0000-0000-0000-000000000020");
        assert_eq!(base_by_file.as_ref(), base_by_name.as_ref());
        let base_metadata = fixture.db.module_metadata(ModuleId::new(fixture.base_common_module));
        assert_eq!(base_metadata.common_module.as_deref(), Some(base_by_name.as_ref()));
        assert_eq!(base_metadata.execution_context, Some(hir::ExecutionContext::ServerCall));
        assert!(
            fixture.db.common_module_for_file_id(fixture.unrelated_module).is_none(),
            "a non-common-module file must not borrow an owner by name"
        );
        assert_eq!(
            by_file.uri().is_some(),
            expects_uri,
            "filesystem loading attaches the body URI; the per-MDO model keeps XML properties"
        );
        assert!(
            fixture.db.common_module_for_file_id(fixture.owner_shaped_impostor).is_none(),
            "a common-module-shaped path outside module_file/unread_module_file has no owner"
        );
    }
}

#[test]
fn extension_metadata_substrate_rejects_wrong_adoption_target_and_own_namesake() {
    use bsl_metadata::traits::MdObject;

    let mut fixture = extension_metadata_db_fixture(true);
    let root = std::path::PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../bsl-metadata/fixtures/extension_metadata"
    ));
    let extension_document =
        std::fs::read_to_string(root.join("extension/Documents/Заказ.xml")).unwrap().replace(
            "15500000-0000-0000-0000-000000000010</ExtendedConfigurationObject>",
            "15500000-0000-0000-0000-000000000099</ExtendedConfigurationObject>",
        );
    fixture.db.set_file_text(fixture.extension_document_xml, &extension_document);
    let standalone_document = fixture
        .db
        .resolve_metadata_object_for_file(
            fixture.consumer,
            bsl_metadata::MdoType::Document,
            "Заказ",
        )
        .unwrap();
    assert!(standalone_document.find_attribute("РасшРеквизит").is_some());
    assert!(
        standalone_document.find_attribute("БазовыйРеквизит").is_none(),
        "an adopted document targeting another UUID must not inherit base fields"
    );

    let extension_common =
        std::fs::read_to_string(root.join("extension/CommonModules/Сервер.xml")).unwrap();
    let assert_standalone = |module: &bsl_metadata::CommonModule| {
        assert_eq!(module.uuid().to_string(), "15500000-0000-0000-0000-000000000120");
        assert_eq!(module.return_values_reuse(), bsl_metadata::ReturnValueReuse::Unknown);
        assert!(!module.is_client_managed_application());
        assert!(!module.is_client_ordinary_application());
        assert!(!module.is_privileged());
    };
    let wrong_target = extension_common.replace(
        "15500000-0000-0000-0000-000000000020</ExtendedConfigurationObject>",
        "15500000-0000-0000-0000-000000000099</ExtendedConfigurationObject>",
    );
    fixture.db.set_file_text(fixture.extension_common_xml, &wrong_target);
    let wrong_target_module = fixture
        .db
        .resolve_common_module_for_file(fixture.extension_common_module, "Сервер")
        .unwrap();
    assert_standalone(&wrong_target_module);

    let own = extension_common
        .replace("      <ObjectBelonging>Adopted</ObjectBelonging>\n", "")
        .replace(
            "      <ExtendedConfigurationObject>15500000-0000-0000-0000-000000000020</ExtendedConfigurationObject>\n",
            "",
        );
    fixture.db.set_file_text(fixture.extension_common_xml, &own);
    let own_module = fixture
        .db
        .resolve_common_module_for_file(fixture.extension_common_module, "Сервер")
        .unwrap();
    assert_standalone(&own_module);
}

#[test]
fn extension_metadata_filesystem_fallback_keeps_own_namesake_common_module_independent() {
    use bsl_metadata::traits::MdObject;

    let root = tempfile::tempdir().unwrap();
    let base = root.path().join("base");
    let extension = root.path().join("extension");
    let configuration_xml = |uuid: &str, name: &str| {
        format!(
            r#"<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses"><Configuration uuid="{uuid}"><Properties><Name>{name}</Name></Properties><ChildObjects><CommonModule>Shared</CommonModule></ChildObjects></Configuration></MetaDataObject>"#
        )
    };
    for (path, text) in [
        (
            base.join("Configuration.xml"),
            configuration_xml("15500000-0000-0000-0000-000000000701", "Base"),
        ),
        (
            extension.join("Configuration.xml"),
            configuration_xml("15500000-0000-0000-0000-000000000702", "Extension"),
        ),
        (
            base.join("CommonModules/Shared.xml"),
            r#"<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses"><CommonModule uuid="15500000-0000-0000-0000-000000000703"><Properties><Name>Shared</Name><Server>true</Server><Global>true</Global><ClientManagedApplication>true</ClientManagedApplication><ClientOrdinaryApplication>true</ClientOrdinaryApplication><ExternalConnection>true</ExternalConnection><ServerCall>true</ServerCall><Privileged>true</Privileged><ReturnValuesReuse>DontUse</ReturnValuesReuse></Properties></CommonModule></MetaDataObject>"#.to_string(),
        ),
        (
            extension.join("CommonModules/Shared.xml"),
            r#"<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses"><CommonModule uuid="15500000-0000-0000-0000-000000000704"><Properties><Name>Shared</Name></Properties></CommonModule></MetaDataObject>"#.to_string(),
        ),
    ] {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    let module_path = extension.join("CommonModules/Shared/Ext/Module.bsl");
    std::fs::create_dir_all(module_path.parent().unwrap()).unwrap();
    std::fs::write(&module_path, "Процедура Тест() Экспорт\nКонецПроцедуры").unwrap();

    let module_file = FileId(40);
    let mut db = RootDatabaseImpl::new();
    let mut file_set = FileSet::new();
    file_set.insert(module_file, VfsPath::new(module_path.to_string_lossy().as_ref()));
    db.set_source_root(SourceRootId(4), SourceRoot::new_local(file_set));
    db.set_file_source_root(module_file, SourceRootId(4));
    db.set_file_text(module_file, "Процедура Тест() Экспорт\nКонецПроцедуры");
    db.set_all_config_paths(vec![(None, base), (Some("Extension".to_string()), extension)]);

    let module = db
        .resolve_common_module_for_file(module_file, "Shared")
        .expect("own module resolves through the filesystem fallback");
    assert_eq!(module.uuid().to_string(), "15500000-0000-0000-0000-000000000704");
    assert_eq!(module.return_values_reuse(), bsl_metadata::ReturnValueReuse::Unknown);
    assert!(!module.is_server());
    assert!(!module.is_global());
    assert!(!module.is_client_managed_application());
    assert!(!module.is_client_ordinary_application());
    assert!(!module.is_external_connection());
    assert!(!module.is_server_call());
    assert!(!module.is_privileged());
}

#[test]
fn extension_metadata_dependency_chain_composes_only_visible_overlays() {
    use bsl_metadata::traits::MdObject;

    let root = std::path::PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../bsl-metadata/fixtures/extension_metadata"
    ));
    let base = root.join("base");
    let extension = root.join("extension");
    let dependent = root.join("dependent");
    let extension_file = FileId(20);
    let dependent_file = FileId(21);
    let dependent_common = FileId(22);
    let file_paths = [
        (extension_file, extension.join("Documents/Заказ/Ext/ObjectModule.bsl")),
        (dependent_file, dependent.join("Documents/Заказ/Ext/ObjectModule.bsl")),
        (dependent_common, dependent.join("CommonModules/Сервер/Ext/Module.bsl")),
    ];

    let mut db = RootDatabaseImpl::new();
    let mut file_set = FileSet::new();
    for (file_id, path) in &file_paths {
        file_set.insert(*file_id, VfsPath::new(path.to_string_lossy().as_ref()));
    }
    db.set_source_root(SourceRootId(2), SourceRoot::new_local(file_set));
    for (file_id, path) in &file_paths {
        db.set_file_source_root(*file_id, SourceRootId(2));
        db.set_file_text(*file_id, &std::fs::read_to_string(path).unwrap());
    }
    let paths = vec![
        (None, base.clone()),
        (Some("Расширение".to_string()), extension.clone()),
        (Some("Зависимое".to_string()), dependent.clone()),
    ];
    db.set_workspace_configs_snapshot(crate::metadata::WorkspaceConfigsSnapshot {
        canonical_paths: paths.iter().map(|(_, path)| path.clone()).collect(),
        kinds: vec![RootKind::Base, RootKind::Extension, RootKind::Extension],
        paths,
        closures: vec![vec![], vec![], vec![1]],
        topological_order: vec![0, 1, 2],
        fingerprint: None,
        source_exclusions: Default::default(),
    });

    assert_eq!(
        extension_metadata_field_names(&db, dependent_file),
        ["Номенклатура", "Количество", "РасшПоле", "ЗависимоеПоле"]
    );
    assert_eq!(
        extension_metadata_field_names(&db, extension_file),
        ["Номенклатура", "Количество", "РасшПоле"],
        "a dependency must not see its dependent overlay"
    );
    let common = db
        .resolve_common_module_for_file(dependent_common, "Сервер")
        .expect("common module is inherited through the chain");
    assert!(!common.is_server(), "the dependency's explicit false remains effective");
    assert!(!common.is_global(), "the dependency's explicit false remains effective");
    assert_eq!(common.return_values_reuse(), bsl_metadata::ReturnValueReuse::DontUse);
    assert_eq!(common.uuid().to_string(), "15500000-0000-0000-0000-000000000220");
    let by_file = db
        .common_module_for_file_id(dependent_common)
        .expect("dependent common module resolves by file");
    assert_eq!(by_file.as_ref(), common.as_ref());
    let metadata = db.module_metadata(ModuleId::new(dependent_common));
    assert_eq!(metadata.common_module.as_deref(), Some(common.as_ref()));
    assert_eq!(metadata.execution_context, Some(hir::ExecutionContext::Client));
}

#[test]
fn extension_metadata_dependency_chain_composes_in_substrate() {
    use crate::metadata::{CommonModuleEntry, MdoEntry};
    use bsl_metadata::traits::MdObject;

    let root = std::path::PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../bsl-metadata/fixtures/extension_metadata"
    ));
    let base = root.join("base");
    let extension = root.join("extension");
    let dependent = root.join("dependent");
    let base_doc = FileId(40);
    let extension_doc = FileId(41);
    let dependent_doc = FileId(42);
    let base_common = FileId(43);
    let extension_common = FileId(44);
    let dependent_common_xml = FileId(45);
    let extension_file = FileId(46);
    let dependent_file = FileId(47);
    let dependent_common_file = FileId(48);
    let file_paths = [
        (base_doc, base.join("Documents/Заказ.xml")),
        (extension_doc, extension.join("Documents/Заказ.xml")),
        (dependent_doc, dependent.join("Documents/Заказ.xml")),
        (base_common, base.join("CommonModules/Сервер.xml")),
        (extension_common, extension.join("CommonModules/Сервер.xml")),
        (dependent_common_xml, dependent.join("CommonModules/Сервер.xml")),
        (extension_file, extension.join("Documents/Заказ/Ext/ObjectModule.bsl")),
        (dependent_file, dependent.join("Documents/Заказ/Ext/ObjectModule.bsl")),
        (dependent_common_file, dependent.join("CommonModules/Сервер/Ext/Module.bsl")),
    ];
    let mut db = RootDatabaseImpl::new();
    let mut file_set = FileSet::new();
    for (file_id, path) in &file_paths {
        file_set.insert(*file_id, VfsPath::new(path.to_string_lossy().as_ref()));
    }
    db.set_source_root(SourceRootId(3), SourceRoot::new_local(file_set));
    for (file_id, path) in &file_paths {
        db.set_file_source_root(*file_id, SourceRootId(3));
        db.set_file_text(*file_id, &std::fs::read_to_string(path).unwrap());
    }
    let paths = vec![
        (None, base.clone()),
        (Some("Расширение".to_string()), extension.clone()),
        (Some("Зависимое".to_string()), dependent.clone()),
    ];
    db.set_workspace_configs_snapshot(crate::metadata::WorkspaceConfigsSnapshot {
        canonical_paths: paths.iter().map(|(_, path)| path.clone()).collect(),
        kinds: vec![RootKind::Base, RootKind::Extension, RootKind::Extension],
        paths,
        closures: vec![vec![], vec![], vec![1]],
        topological_order: vec![0, 1, 2],
        fingerprint: None,
        source_exclusions: Default::default(),
    });
    let listing = |main, common, module_file| MetadataListingData {
        entries: vec![MdoEntry {
            kind: bsl_metadata::MdoType::Document,
            name: "Заказ".to_string(),
            main,
            predefined: None,
        }],
        common_modules: vec![CommonModuleEntry {
            name: "Сервер".to_string(),
            main: common,
            module_file,
            unread_module_file: None,
        }],
        ..empty_listing_data()
    };
    db.set_metadata_listing(&base.to_string_lossy(), listing(base_doc, base_common, None));
    db.set_metadata_listing(
        &extension.to_string_lossy(),
        listing(extension_doc, extension_common, None),
    );
    db.set_metadata_listing(
        &dependent.to_string_lossy(),
        listing(dependent_doc, dependent_common_xml, Some(dependent_common_file)),
    );

    assert_eq!(
        extension_metadata_field_names(&db, dependent_file),
        ["Номенклатура", "Количество", "РасшПоле", "ЗависимоеПоле"]
    );
    assert_eq!(
        extension_metadata_field_names(&db, extension_file),
        ["Номенклатура", "Количество", "РасшПоле"]
    );
    let common = db
        .resolve_common_module_for_file(dependent_common_file, "Сервер")
        .expect("common module is inherited through the listed chain");
    assert!(!common.is_server());
    assert!(!common.is_global());
    assert_eq!(common.return_values_reuse(), bsl_metadata::ReturnValueReuse::DontUse);
    assert_eq!(common.uuid().to_string(), "15500000-0000-0000-0000-000000000220");
    let by_file = db
        .common_module_for_file_id(dependent_common_file)
        .expect("listed dependent common module resolves by file");
    assert_eq!(by_file.as_ref(), common.as_ref());
    let metadata = db.module_metadata(ModuleId::new(dependent_common_file));
    assert_eq!(metadata.common_module.as_deref(), Some(common.as_ref()));
    assert_eq!(metadata.execution_context, Some(hir::ExecutionContext::Client));
}

#[test]
fn extension_metadata_substrate_reacts_to_common_module_and_document_edits() {
    let mut fixture = extension_metadata_db_fixture(true);

    let before = fixture
        .db
        .resolve_common_module_for_file(fixture.extension_common_module, "Сервер")
        .unwrap();
    assert_eq!(before.return_values_reuse(), bsl_metadata::ReturnValueReuse::DontUse);
    assert_eq!(
        fixture
            .db
            .module_metadata(ModuleId::new(fixture.extension_common_module))
            .common_module
            .as_ref()
            .map(|module| module.return_values_reuse()),
        Some(bsl_metadata::ReturnValueReuse::DontUse)
    );

    let base_xml = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../bsl-metadata/fixtures/extension_metadata/base/CommonModules/Сервер.xml"
    ))
    .unwrap()
    .replace("DontUse", "DuringRequest");
    fixture.db.set_file_text(fixture.base_common_xml, &base_xml);
    let inherited = fixture
        .db
        .resolve_common_module_for_file(fixture.extension_common_module, "Сервер")
        .unwrap();
    assert_eq!(inherited.return_values_reuse(), bsl_metadata::ReturnValueReuse::DuringRequest);
    assert_eq!(
        fixture
            .db
            .module_metadata(ModuleId::new(fixture.extension_common_module))
            .common_module
            .as_ref()
            .map(|module| module.return_values_reuse()),
        Some(bsl_metadata::ReturnValueReuse::DuringRequest),
        "ModuleMetadata must invalidate with the base XML input"
    );

    let extension_xml = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../bsl-metadata/fixtures/extension_metadata/extension/CommonModules/Сервер.xml"
    ))
    .unwrap()
    .replace(
        "<Global>false</Global>",
        "<Global>false</Global><ReturnValuesReuse>DontUse</ReturnValuesReuse>",
    );
    fixture.db.set_file_text(fixture.extension_common_xml, &extension_xml);
    let overridden = fixture
        .db
        .resolve_common_module_for_file(fixture.extension_common_module, "Сервер")
        .unwrap();
    assert_eq!(overridden.return_values_reuse(), bsl_metadata::ReturnValueReuse::DontUse);
    assert_eq!(
        fixture
            .db
            .module_metadata(ModuleId::new(fixture.extension_common_module))
            .common_module
            .as_ref()
            .map(|module| module.return_values_reuse()),
        Some(bsl_metadata::ReturnValueReuse::DontUse),
        "ModuleMetadata must invalidate with the extension XML input"
    );

    let original_document = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../bsl-metadata/fixtures/extension_metadata/base/Documents/Заказ.xml"
    ))
    .unwrap();
    let added = r#"<Attribute uuid="15500000-0000-0000-0000-000000000099"><Properties><Name>ПослеИзменения</Name><Type><v8:Type>xs:string</v8:Type></Type></Properties></Attribute>"#;
    let changed_document = original_document.replacen(
        "        </ChildObjects>\n      </TabularSection>",
        &format!("          {added}\n        </ChildObjects>\n      </TabularSection>"),
        1,
    );
    assert_ne!(changed_document, original_document, "the fixture edit must hit the section");
    fixture.db.set_file_text(fixture.base_document_xml, &changed_document);
    let types = extension_metadata_field_types(&fixture.db, fixture.consumer);
    assert!(
        types.iter().any(|(name, ty)| {
            name == "ПослеИзменения" && ty == &bsl_metadata::AttributeType::String { length: None }
        }),
        "the same database must expose the newly added typed base column: {types:?}"
    );
}

/// The Channel-2 configuration accessor (`info`/`tree` payload source): it loads the
/// whole configuration via `load_configuration`, and — because that query keys on the
/// config-root revision — re-reads after `bump_config_for_paths` (the same invalidation a
/// metadata drift fires). Asserted through object enumeration, the real header/tree
/// payload; `load_from_directory` does NOT read the config name/uuid (they default), so
/// this does not assert those.
#[test]
fn configuration_for_root_reads_objects_and_rereads_after_bump() {
    use bsl_metadata::MdoType;

    fn catalog_xml(name: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <Catalog uuid="00000000-0000-0000-0000-000000000001">
        <Properties><Name>{name}</Name><CodeLength>9</CodeLength></Properties>
    </Catalog>
</MetaDataObject>"#
        )
    }

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("Catalogs")).unwrap();
    std::fs::write(root.join("Catalogs/Товары.xml"), catalog_xml("Товары")).unwrap();

    let mut db = RootDatabaseImpl::new();
    db.set_all_config_paths(vec![(None, root.to_path_buf())]);

    let cfg = db.configuration_for_root(root);
    assert!(
        cfg.find_metadata_object(MdoType::Catalog, "Товары").is_some(),
        "configuration reads objects through load_configuration",
    );
    let before = cfg.metadata_objects().len();

    // Add an object and bump the root's config revision; the cached query must re-run.
    std::fs::write(root.join("Catalogs/Услуги.xml"), catalog_xml("Услуги")).unwrap();
    db.bump_config_for_paths(std::iter::once(root));
    let cfg2 = db.configuration_for_root(root);
    assert!(
        cfg2.find_metadata_object(MdoType::Catalog, "Услуги").is_some(),
        "configuration re-reads after bump_config_for_paths invalidates the config revision",
    );
    assert!(cfg2.metadata_objects().len() > before, "the added object is visible after re-read");
}

#[test]
fn salsa_events_count_query_executes() {
    // Off by default: no callback installed, so no report at all.
    let plain = RootDatabaseImpl::new_inner(false);
    assert!(plain.salsa_event_report().is_none(), "no counters unless events are enabled");

    // On: a real query execution is recorded as an `execute`, and the report
    // resolves the ingredient to its debug name.
    let mut db = RootDatabaseImpl::new_with_salsa_events();
    let file_id = FileId(0);
    let mut file_set = FileSet::new();
    file_set.insert(file_id, VfsPath::new("/test.bsl"));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    db.set_file_source_root(file_id, SourceRootId(0));
    db.set_file_text(file_id, "Процедура Тест() КонецПроцедуры");

    let _ = db.parse(file_id);

    let rows = db.salsa_event_report().expect("events enabled must yield a report");
    let parse_row = rows
        .iter()
        .find(|r| r.name.contains("parse"))
        .expect("the parse query must have executed and be named");
    assert!(parse_row.execute >= 1, "parse executed at least once");
    assert!(db.salsa_event_global().is_some(), "global counters available when enabled");
}

#[test]
fn salsa_key_event_report_decodes_file_keyed_query() {
    // Off by default: no per-key report either.
    let plain = RootDatabaseImpl::new_inner(false);
    assert!(plain.salsa_key_event_report(10).is_none(), "no per-key report unless enabled");

    let mut db = RootDatabaseImpl::new_with_salsa_events();
    let file_id = FileId(0);
    let mut file_set = FileSet::new();
    file_set.insert(file_id, VfsPath::new("/hot.bsl"));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    db.set_file_source_root(file_id, SourceRootId(0));
    db.set_file_text(file_id, "Процедура Тест() КонецПроцедуры");

    // A file-keyed query (`parse_query`) executes for this file.
    let _ = db.parse(file_id);

    let rows = db.salsa_key_event_report(40).expect("events enabled must yield a per-key report");
    let parse_key = rows
        .iter()
        .find(|r| r.name.contains("parse"))
        .expect("the parse query key must appear in the hot-key report");
    assert!(parse_key.execute >= 1, "the parse key executed at least once");
    // The interned FileIdInput must decode back to a readable path, not a raw Id.
    assert!(
        parse_key.name.contains("/hot.bsl"),
        "file-keyed query must decode to its path, got {:?}",
        parse_key.name
    );
    assert!(
        !parse_key.name.contains("Id("),
        "a decoded key must not fall back to a raw Id, got {:?}",
        parse_key.name
    );
}

#[test]
fn salsa_event_window_resets_and_reports_full_key_set() {
    // Off by default: reset is a no-op and the window is unavailable.
    let plain = RootDatabaseImpl::new_inner(false);
    assert!(!plain.salsa_events_reset(), "reset must report disabled counters");
    assert!(plain.salsa_key_event_window().is_none(), "no window unless enabled");

    let mut db = RootDatabaseImpl::new_with_salsa_events();
    let file_id = FileId(0);
    let mut file_set = FileSet::new();
    file_set.insert(file_id, VfsPath::new("/window.bsl"));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    db.set_file_source_root(file_id, SourceRootId(0));
    db.set_file_text(file_id, "Процедура Тест() КонецПроцедуры");

    // Warm phase: executions recorded here must NOT leak into the window.
    let _ = db.parse(file_id);
    assert!(db.salsa_events_reset(), "reset must succeed with counters enabled");
    let window = db.salsa_key_event_window().expect("window available when enabled");
    assert_eq!(window.distinct_keys, 0, "reset must clear all per-key entries");
    assert!(
        db.salsa_event_report().expect("per-ingredient report available").is_empty(),
        "reset must clear per-ingredient counters too"
    );

    // Window: one revision-bumping change, then the observed re-execution.
    db.set_file_text(file_id, "Процедура Тест()\n\tФ = 1;\nКонецПроцедуры");
    let _ = db.parse(file_id);

    let window = db.salsa_key_event_window().expect("window available when enabled");
    assert!(window.distinct_keys >= 1, "the re-parse must be recorded");
    assert_eq!(window.distinct_keys, window.rows.len(), "count must cover ALL keys, not top-K");
    let parse_row = window
        .rows
        .iter()
        .find(|r| r.name.contains("parse"))
        .expect("the parse key must appear in the window");
    assert!(parse_row.execute >= 1);
    assert!(
        parse_row.name.contains("/window.bsl"),
        "keys must decode to module paths in the window's revision, got {:?}",
        parse_row.name
    );
}

/// A root counts as warmed only once its own configuration is loaded.
///
/// Going through `module_metadata` does not establish that: for a service module
/// whose service resolves out of the DECLARED roots the query returns before it
/// ever reads the attributed root, and the per-root dedup would then skip that
/// root's remaining modules — leaving the whole-config load to happen lazily
/// inside the build's worker pool, which is what the warm-up exists to prevent.
#[test]
fn warming_loads_the_attributed_root_of_a_service_module() {
    use hir::ConfigsDatabase as _;

    fn http_service_xml(name: &str) -> String {
        format!(
            concat!(
                "<MetaDataObject>",
                "<HTTPService uuid=\"00000000-0000-0000-0000-000000000040\">",
                "<Properties><Name>{}</Name><RootURL>http</RootURL></Properties>",
                "</HTTPService></MetaDataObject>"
            ),
            name
        )
    }

    let ws =
        std::env::temp_dir().join(format!("bsl_warm_roots_{}_{}", std::process::id(), line!()));
    let deep = ws.join("deep");

    // The SAME service name in both configurations: the declared root's copy is
    // what the early return finds, standing in for the nested root that still
    // has to be loaded.
    for root in [&ws, &deep] {
        std::fs::create_dir_all(root.join("HTTPServices/Сервис/Ext")).unwrap();
        std::fs::write(root.join("HTTPServices/Сервис.xml"), http_service_xml("Сервис")).unwrap();
        std::fs::write(root.join("HTTPServices/Сервис/Ext/Module.bsl"), "").unwrap();
    }
    // `CommonModules/` is what stops the attribution walk at `deep` — the nested
    // directory is a configuration root that nothing declared.
    std::fs::create_dir_all(deep.join("CommonModules/Модуль/Ext")).unwrap();
    std::fs::write(deep.join("CommonModules/Модуль/Ext/Module.bsl"), "").unwrap();

    let mut db = RootDatabaseImpl::new();
    let cache = Arc::new(crate::GraphConfigCache::default());
    db.set_graph_config_cache(Arc::clone(&cache));

    let service_file = FileId(0);
    let module_file = FileId(1);
    let mut file_set = FileSet::new();
    let path_of = |p: std::path::PathBuf| VfsPath::new(p.to_string_lossy().into_owned());
    file_set.insert(service_file, path_of(deep.join("HTTPServices/Сервис/Ext/Module.bsl")));
    file_set.insert(module_file, path_of(deep.join("CommonModules/Модуль/Ext/Module.bsl")));
    db.set_source_root(SourceRootId(1), SourceRoot::new_local(file_set));
    db.set_file_source_root(service_file, SourceRootId(1));
    db.set_file_source_root(module_file, SourceRootId(1));
    db.set_all_config_paths(vec![(None, ws.clone())]);

    // The service module first, so it is the representative the dedup keeps.
    db.warm_config_roots(&[ModuleId::new(service_file), ModuleId::new(module_file)]);

    // Spelled through the very function the key is built with: the cache is keyed
    // by the interned path, which on Windows is lower-cased and slash-normalized,
    // so a raw `PathBuf` would miss there while passing here.
    let key = |path: &std::path::Path| {
        std::path::PathBuf::from(crate::metadata::canonicalize_configuration_path(
            &path.to_string_lossy(),
        ))
    };

    assert!(
        cache.contains_key(&key(&ws)),
        "sanity: the declared root is loaded, so the early return has something to find"
    );
    assert!(cache.contains_key(&key(&deep)), "the attributed root must be loaded by the warm-up");

    std::fs::remove_dir_all(&ws).ok();
}

#[test]
fn a_case_variant_configuration_xml_names_the_configuration_root() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("cf");
    std::fs::create_dir_all(root.join("Catalogs/X/Ext")).unwrap();
    std::fs::write(root.join("CONFIGURATION.XML"), "<Configuration/>").unwrap();
    let module = root.join("Catalogs/X/Ext/ObjectModule.bsl");
    std::fs::write(&module, "").unwrap();
    let db = crate::RootDatabaseImpl::default();
    assert_eq!(
        db.find_configuration_root(&module),
        Some(root.clone()),
        "корень опознан по CONFIGURATION.XML"
    );
}

/// A defined type composes differently from an object or a register, and the difference is
/// the point: an extension does not overlay a defined type, it REPLACES its underlying type
/// wholesale. Copying the neighbours' fold here would silently keep the base's type.
///
/// The no-collision input is the control: a resolver that simply returned the last root would
/// pass the collision half and fail this one.
#[test]
fn resolve_defined_type_across_roots_lets_an_extension_replace_the_base() {
    use crate::metadata::{DefinedTypeEntry, MetadataListingData};

    fn defined_type_xml(name: &str, inner: &str) -> String {
        format!(
            concat!(
                "<MetaDataObject>",
                "<DefinedType uuid=\"00000000-0000-0000-0000-000000000010\">",
                "<Properties><Name>{}</Name><Type><Type>{}</Type></Type></Properties>",
                "</DefinedType></MetaDataObject>"
            ),
            name, inner
        )
    }

    let base_shared = FileId(0);
    let ext_shared = FileId(1);
    let base_only = FileId(2);

    let mut db = RootDatabaseImpl::new();
    let mut file_set = FileSet::new();
    file_set.insert(base_shared, VfsPath::new("/base/DefinedTypes/Цена.xml"));
    file_set.insert(ext_shared, VfsPath::new("/ext/DefinedTypes/Цена.xml"));
    file_set.insert(base_only, VfsPath::new("/base/DefinedTypes/ТолькоБаза.xml"));
    db.set_source_root(SourceRootId(1), SourceRoot::new_local(file_set));
    for f in [base_shared, ext_shared, base_only] {
        db.set_file_source_root(f, SourceRootId(1));
    }
    db.set_file_text(base_shared, &defined_type_xml("Цена", "xs:decimal"));
    db.set_file_text(ext_shared, &defined_type_xml("Цена", "xs:string"));
    db.set_file_text(base_only, &defined_type_xml("ТолькоБаза", "xs:boolean"));

    db.set_all_config_paths(vec![
        (None, std::path::PathBuf::from("/base")),
        (Some("Расш".to_string()), std::path::PathBuf::from("/ext")),
    ]);
    db.set_metadata_listing(
        "/base",
        MetadataListingData {
            defined_types: vec![
                DefinedTypeEntry { name: "Цена".to_string(), main: base_shared },
                DefinedTypeEntry { name: "ТолькоБаза".to_string(), main: base_only },
            ],
            ..empty_listing_data()
        },
    );
    db.set_metadata_listing(
        "/ext",
        MetadataListingData {
            defined_types: vec![DefinedTypeEntry {
                name: "Цена".to_string(), main: ext_shared
            }],
            ..empty_listing_data()
        },
    );

    let collided = db.resolve_defined_type_across_roots("Цена").expect("the name resolves");
    assert!(
        format!("{collided:?}").contains("String"),
        "the extension replaces the base wholesale, got {collided:?}",
    );

    let base_only_type =
        db.resolve_defined_type_across_roots("ТолькоБаза").expect("the name resolves");
    assert!(
        format!("{base_only_type:?}").contains("Bool"),
        "a name no extension defines still comes from the base, got {base_only_type:?}",
    );

    assert!(
        db.resolve_defined_type_across_roots("НетТакого").is_none(),
        "an absent defined type resolves to None",
    );
}

/// An external object's root has neither `Configuration.xml` nor `CommonModules/`,
/// so the disk walk that used to attribute a file to its configuration finds
/// nothing there. The registered roots must answer first; the designer-wide order
/// must leave the external out while the file's own chain sees every extension.
#[test]
fn an_external_root_binds_its_files_through_the_snapshot_not_the_disk() {
    let base = std::env::temp_dir().join(format!(
        "bsl_external_root_binding_{}_{}",
        std::process::id(),
        line!()
    ));
    let cf = base.join("cf");
    let ext = base.join("ext");
    let epf = base.join("epf");
    std::fs::create_dir_all(cf.join("CommonModules")).unwrap();
    std::fs::create_dir_all(&ext).unwrap();
    std::fs::write(ext.join("Configuration.xml"), "<x/>").unwrap();
    std::fs::create_dir_all(epf.join("АРМ/Forms/Форма/Ext/Form")).unwrap();
    std::fs::write(
        epf.join("АРМ.xml"),
        "<MetaDataObject><ExternalDataProcessor/></MetaDataObject>",
    )
    .unwrap();

    let mut db = RootDatabaseImpl::new();
    let module = FileId(0);
    let mut file_set = FileSet::new();
    file_set.insert(
        module,
        VfsPath::new(epf.join("АРМ/Forms/Форма/Ext/Form/Module.bsl").to_string_lossy().as_ref()),
    );
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    db.set_file_source_root(module, SourceRootId(0));
    db.set_file_text(module, "Процедура Т() КонецПроцедуры");

    let paths = vec![
        (None, cf.clone()),
        (Some("EXT".to_owned()), ext.clone()),
        (Some("EPF".to_owned()), epf.clone()),
    ];
    db.set_workspace_configs_snapshot(crate::metadata::WorkspaceConfigsSnapshot {
        canonical_paths: paths.iter().map(|(_, path)| path.clone()).collect(),
        kinds: vec![
            RootKind::Base,
            RootKind::Extension,
            RootKind::External(bsl_metadata::ExternalObjectKind::DataProcessor),
        ],
        paths,
        closures: vec![vec![], vec![], vec![1]],
        topological_order: vec![0, 1, 2],
        fingerprint: Some("external".to_string()),
        source_exclusions: Default::default(),
    });

    let bound = db
        .configuration_input_for_path(&epf.join("АРМ/Forms/Форма/Ext/Form/Module.bsl"))
        .expect("the file binds to a registered root");
    assert_eq!(
        bound.path(&db).as_str(),
        epf.to_string_lossy().as_ref(),
        "and that root is the external's"
    );

    let roots = db.visible_roots_for_file(module).expect("visibility resolves");
    assert_eq!(roots.main.as_deref(), Some(cf.as_path()));
    assert_eq!(
        roots.chain,
        vec![("EXT".to_owned(), ext.clone()), ("EPF".to_owned(), epf.clone())],
        "the external sees the extension, then itself"
    );

    assert_eq!(
        db.ordered_config_roots(),
        vec![
            cf.to_string_lossy().into_owned(),
            ext.to_string_lossy().into_owned(),
            epf.to_string_lossy().into_owned()
        ],
        "a lookup by kind and name reaches the external last"
    );
    assert_eq!(db.designer_config_paths().len(), 2, "while the designer's view leaves it out");
    assert_eq!(db.all_config_paths().len(), 3, "while every root stays registered");

    std::fs::remove_dir_all(&base).ok();
}

/// A root configured through one spelling and enumerated through another — a
/// symlinked checkout, an MCP walk that canonicalizes — must still claim its
/// files, and the key it hands back must be the configured spelling, which is
/// the one a bump on that root increments.
#[cfg(unix)]
#[test]
fn an_external_root_matches_its_canonical_spelling_and_keys_by_the_configured_one() {
    let base = std::env::temp_dir().join(format!(
        "bsl_external_root_spelling_{}_{}",
        std::process::id(),
        line!()
    ));
    let epf_real = base.join("epf-real");
    let epf_link = base.join("epf-link");
    std::fs::create_dir_all(epf_real.join("АРМ/Forms/Форма/Ext/Form")).unwrap();
    std::os::unix::fs::symlink(&epf_real, &epf_link).unwrap();
    let file = epf_real.join("АРМ/Forms/Форма/Ext/Form/Module.bsl");

    let mut db = RootDatabaseImpl::new();
    let paths = vec![(Some("EPF".to_owned()), epf_link.clone())];
    db.set_workspace_configs_snapshot(crate::metadata::WorkspaceConfigsSnapshot {
        canonical_paths: vec![epf_real.clone()],
        kinds: vec![RootKind::External(bsl_metadata::ExternalObjectKind::DataProcessor)],
        paths,
        closures: vec![vec![]],
        topological_order: vec![0],
        fingerprint: Some("spelling".to_string()),
        source_exclusions: Default::default(),
    });

    let bound = db
        .configuration_input_for_path(&file)
        .expect("the canonical spelling of the file still binds to the root");
    assert_eq!(bound.path(&db).as_str(), epf_link.to_string_lossy().as_ref());

    // A marked root behind an alias shallower than its target: the walk finds
    // the canonical directory, which is the SAME root, not a nested one — the
    // key must stay the configured spelling or the configuration loads twice.
    let cf_real = base.join("deep/er/cf-real");
    let cf_link = base.join("cf");
    std::fs::create_dir_all(cf_real.join("CommonModules/Foo/Ext")).unwrap();
    std::os::unix::fs::symlink(&cf_real, &cf_link).unwrap();
    db.set_workspace_configs_snapshot(crate::metadata::WorkspaceConfigsSnapshot {
        paths: vec![(None, cf_link.clone())],
        kinds: vec![RootKind::Base],
        canonical_paths: vec![cf_real.clone()],
        closures: vec![vec![]],
        topological_order: vec![0],
        fingerprint: Some("alias".to_string()),
        source_exclusions: Default::default(),
    });
    let bound = db
        .configuration_input_for_path(&cf_real.join("CommonModules/Foo/Ext/Module.bsl"))
        .expect("binds");
    assert_eq!(
        bound.path(&db).as_str(),
        cf_link.to_string_lossy().as_ref(),
        "the walk's canonical spelling of the same root must not become a second key"
    );

    std::fs::remove_dir_all(&base).ok();
}

/// Hover and go-to-definition reach a platform method through
/// `hir_ty::resolve_method`, a path separate from inference. An external
/// object's methods live under its bare platform type, and that path must find
/// them too, or the same call is typed but not navigable.
#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn navigation_resolves_a_platform_method_on_an_external_object() {
    use bsl_types::builders::Builders;
    use bsl_types::kind::MetadataKind;
    use bsl_types::testing::RootConfigCtx;

    let db = RootDatabaseImpl::new();
    let name = hir::Name::new("ПолучитьФорму");
    let receiver = |kind: MetadataKind| db.metadata_ref(kind, "АРМ".to_string(), &RootConfigCtx);

    hir_ty::resolve_method(&db, receiver(MetadataKind::DataProcessorObject), &name)
        .expect("control: an internal object's method resolves");

    let resolved =
        hir_ty::resolve_method(&db, receiver(MetadataKind::ExternalDataProcessorObject), &name)
            .expect("the external object's method resolves through its bare platform type");
    assert!(
        resolved.handle.lookup(&db).is_some(),
        "the handle hover reads back must round-trip to the method"
    );
    hir_ty::resolve_method(&db, receiver(MetadataKind::ExternalReportObject), &name)
        .expect("and so does the external report's");
}

fn borrowed_form_fixture_root() -> std::path::PathBuf {
    std::path::PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../bsl-metadata/fixtures/extension_metadata"
    ))
}

fn borrowed_form_module(root: &std::path::Path, owner: &str, form: &str) -> std::path::PathBuf {
    root.join(format!("Documents/{owner}/Forms/{form}/Ext/Form/Module.bsl"))
}

/// A database over `roots` (label, path, kind) whose single source root holds
/// `modules`, numbered from `FileId(0)` in order.
fn borrowed_form_db(
    roots: &[(Option<&str>, &std::path::Path, RootKind)],
    modules: &[std::path::PathBuf],
) -> RootDatabaseImpl {
    let mut db = RootDatabaseImpl::new();
    let mut file_set = FileSet::new();
    for (idx, path) in modules.iter().enumerate() {
        file_set.insert(FileId(idx as u32), VfsPath::new(path.to_string_lossy().as_ref()));
    }
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    for (idx, path) in modules.iter().enumerate() {
        db.set_file_source_root(FileId(idx as u32), SourceRootId(0));
        db.set_file_text(FileId(idx as u32), &std::fs::read_to_string(path).unwrap());
    }
    let paths: Vec<(Option<String>, std::path::PathBuf)> = roots
        .iter()
        .map(|(label, path, _)| (label.map(str::to_string), path.to_path_buf()))
        .collect();
    db.set_workspace_configs_snapshot(crate::metadata::WorkspaceConfigsSnapshot {
        canonical_paths: paths
            .iter()
            .map(|(_, path)| std::fs::canonicalize(path).unwrap_or_else(|_| path.clone()))
            .collect(),
        kinds: roots.iter().map(|(_, _, kind)| *kind).collect(),
        closures: vec![Vec::new(); paths.len()],
        topological_order: (0..paths.len()).collect(),
        paths,
        fingerprint: None,
        source_exclusions: Default::default(),
    });
    db
}

fn borrowed_form(db: &RootDatabaseImpl, file: FileId) -> Option<Arc<bsl_metadata::Form>> {
    crate::queries::module_metadata_query(db, base_db::FileIdInput::new(db, file)).form.clone()
}

fn borrowed_form_local(path: &std::path::Path) -> bsl_metadata::Form {
    bsl_metadata::xml_parser::parse_form_from_bsl_path(path).expect("local form parses")
}

fn borrowed_form_names(form: &bsl_metadata::Form) -> Vec<&str> {
    form.attribute_names().collect()
}

/// Copy the base and extension fixture trees into a scratch directory, so a
/// test may delete or rewrite files without touching the shared fixture.
fn borrowed_form_fixture_copy() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    fn copy_dir(from: &std::path::Path, to: &std::path::Path) {
        std::fs::create_dir_all(to).unwrap();
        for entry in std::fs::read_dir(from).unwrap() {
            let entry = entry.unwrap();
            let target = to.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                copy_dir(&entry.path(), &target);
            } else {
                std::fs::copy(entry.path(), target).unwrap();
            }
        }
    }
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path().join("base");
    let extension = temp.path().join("extension");
    copy_dir(&borrowed_form_fixture_root().join("base"), &base);
    copy_dir(&borrowed_form_fixture_root().join("extension"), &extension);
    (temp, base, extension)
}

#[test]
fn borrowed_form_inherits_base_attributes_across_the_fixture_matrix() {
    use bsl_metadata::AttributeType;

    let root = borrowed_form_fixture_root();
    let base = root.join("base");
    let extension = root.join("extension");
    let modules = vec![
        borrowed_form_module(&extension, "Заказ", "ФормаДокумента"),
        borrowed_form_module(&extension, "Заказ", "ФормаСBaseForm"),
        borrowed_form_module(&extension, "Заказ", "формаёж"),
        borrowed_form_module(&extension, "Локальный", "ФормаДокумента"),
        borrowed_form_module(&extension, "Заказ", "ОбычнаяФорма"),
        borrowed_form_module(&extension, "Заказ", "БезПары"),
        borrowed_form_module(&base, "Заказ", "ФормаДокумента"),
        borrowed_form_module(&base, "Заказ", "ФормаСBaseForm"),
        borrowed_form_module(&base, "Заказ", "ФормаЁж"),
        borrowed_form_module(&base, "Локальный", "ФормаДокумента"),
        borrowed_form_module(&base, "Заказ", "ОбычнаяФорма"),
    ];
    let db = borrowed_form_db(
        &[(None, &base, RootKind::Base), (Some("Расширение"), &extension, RootKind::Extension)],
        &modules,
    );
    let form = |idx: u32| borrowed_form(&db, FileId(idx)).expect("form loads");

    let direct = form(0);
    assert_eq!(borrowed_form_names(&direct), ["Объект", "БазовыйРеквизитФормы"]);
    let main = direct.main_attribute().expect("inherited main attribute");
    assert_eq!(main.name, "Объект");
    assert_eq!(
        main.attr_type,
        AttributeType::Ref { mdo_type: bsl_metadata::MdoType::Document, name: "Заказ".into() }
    );
    let local = borrowed_form_local(&modules[0]);
    assert!(local.attributes().is_empty());
    let mut expected_dialog = local.clone();
    expected_dialog.attributes = direct.attributes.clone();
    assert_eq!(*direct, expected_dialog, "the dialog is the extension's alone");
    assert!(direct.find_element("БазовоеПоле").is_none());
    assert!(direct.is_handler("Расш_ПриОткрытии"));
    assert!(!direct.is_handler("БазовыйПриОткрытии"));
    assert!(!direct.is_handler("БазоваяКомандаОбработка"));

    assert!(borrowed_form_local(&modules[1]).attributes().is_empty(), "BaseForm is not parsed");
    assert_eq!(borrowed_form_names(&form(1)), ["Объект", "БазовыйРеквизитФормы"]);

    let unicode = form(2);
    let counters: Vec<_> = unicode
        .attributes()
        .iter()
        .filter(|attr| stdx::case::eq_ignore_case(&attr.name, "СЧЁТЧИК"))
        .collect();
    assert_eq!(counters.len(), 1, "a single attribute for the conflict: {unicode:?}");
    assert_eq!(counters[0].name, "счётчик");
    assert_eq!(counters[0].attr_type, AttributeType::String { length: None });
    assert_eq!(
        borrowed_form_names(&unicode),
        ["Объект", "БазовыйРеквизитФормы", "счётчик", "РасшРеквизитФормы"]
    );

    assert_eq!(borrowed_form_names(&form(3)), ["ЛокальныйРеквизитФормы"], "Own owner");

    let ordinary = form(4);
    assert_eq!(*ordinary, borrowed_form_local(&modules[4]));
    assert!(ordinary.attributes().is_empty());
    assert!(ordinary.is_handler("ПередОткрытием"), "default handlers stay");

    assert_eq!(borrowed_form_names(&form(5)), ["СвойРеквизитФормы"], "no base counterpart");

    for idx in 6..11 {
        assert_eq!(
            *form(idx),
            borrowed_form_local(&modules[idx as usize]),
            "a base form stays local: {}",
            modules[idx as usize].display()
        );
    }
}

#[test]
fn borrowed_form_keeps_the_local_form_when_the_pair_is_incomplete() {
    let (temp, base, extension) = borrowed_form_fixture_copy();
    let ext_module = borrowed_form_module(&extension, "Заказ", "ФормаДокумента");
    let base_module = borrowed_form_module(&base, "Заказ", "ФормаДокумента");
    let local = borrowed_form_local(&ext_module);
    let roots = [
        (None, base.as_path(), RootKind::Base),
        (Some("Расширение"), extension.as_path(), RootKind::Extension),
    ];
    let paired = || borrowed_form_db(&roots, &[ext_module.clone(), base_module.clone()]);

    let control = paired();
    assert_eq!(
        borrowed_form_names(&borrowed_form(&control, FileId(0)).unwrap()),
        ["Объект", "БазовыйРеквизитФормы"]
    );

    let no_base_root = borrowed_form_db(
        &[(Some("Расширение"), extension.as_path(), RootKind::Extension)],
        &[ext_module.clone(), base_module.clone()],
    );
    assert_eq!(*borrowed_form(&no_base_root, FileId(0)).unwrap(), local, "no base root");

    let no_base_file = borrowed_form_db(&roots, std::slice::from_ref(&ext_module));
    assert_eq!(*borrowed_form(&no_base_file, FileId(0)).unwrap(), local, "base not in VFS");

    let base_form_xml = base.join("Documents/Заказ/Forms/ФормаДокумента/Ext/Form.xml");
    let base_dialog = std::fs::read_to_string(&base_form_xml).unwrap();
    std::fs::remove_file(&base_form_xml).unwrap();
    let no_base_dialog = paired();
    assert!(borrowed_form(&no_base_dialog, FileId(1)).is_none(), "base form does not load");
    assert_eq!(*borrowed_form(&no_base_dialog, FileId(0)).unwrap(), local, "no base form");
    std::fs::write(&base_form_xml, base_dialog).unwrap();

    let ext_owner = extension.join("Documents/Заказ.xml");
    let owner_xml = std::fs::read_to_string(&ext_owner).unwrap();
    std::fs::write(&ext_owner, owner_xml.replace("Adopted", "Own")).unwrap();
    let own_owner = paired();
    assert_eq!(*borrowed_form(&own_owner, FileId(0)).unwrap(), local, "owner turned Own");
    std::fs::write(&ext_owner, owner_xml.replace("<ObjectBelonging>Adopted</ObjectBelonging>", ""))
        .unwrap();
    let no_belonging = paired();
    assert_eq!(
        *borrowed_form(&no_belonging, FileId(0)).unwrap(),
        local,
        "owner without an explicit Adopted flag"
    );
    std::fs::remove_file(&ext_owner).unwrap();
    let no_owner = paired();
    assert_eq!(*borrowed_form(&no_owner, FileId(0)).unwrap(), local, "owner unknown");
    std::fs::write(&ext_owner, owner_xml).unwrap();

    let external = temp.path().join("external");
    let external_module = borrowed_form_module(&external, "Заказ", "ФормаДокумента");
    std::fs::create_dir_all(external_module.parent().unwrap()).unwrap();
    std::fs::write(&external_module, "").unwrap();
    std::fs::copy(
        extension.join("Documents/Заказ/Forms/ФормаДокумента/Ext/Form.xml"),
        external.join("Documents/Заказ/Forms/ФормаДокумента/Ext/Form.xml"),
    )
    .unwrap();
    std::fs::copy(ext_owner, external.join("Documents/Заказ.xml")).unwrap();
    let with_external = borrowed_form_db(
        &[
            (None, base.as_path(), RootKind::Base),
            (Some("Расширение"), extension.as_path(), RootKind::Extension),
            (
                Some("АРМ"),
                external.as_path(),
                RootKind::External(bsl_metadata::ExternalObjectKind::DataProcessor),
            ),
        ],
        &[external_module.clone(), base_module.clone(), ext_module.clone()],
    );
    assert_eq!(
        *borrowed_form(&with_external, FileId(0)).unwrap(),
        borrowed_form_local(&external_module),
        "an external form stays local"
    );
    assert_eq!(
        *borrowed_form(&with_external, FileId(1)).unwrap(),
        borrowed_form_local(&base_module),
        "the base form gains nothing from the extension"
    );
    assert_eq!(
        borrowed_form_names(&borrowed_form(&with_external, FileId(2)).unwrap()),
        ["Объект", "БазовыйРеквизитФормы"]
    );
}

#[test]
fn borrowed_form_keeps_common_forms_local_even_with_a_base_pair() {
    let (_temp, base, extension) = borrowed_form_fixture_copy();
    let mut modules = Vec::new();
    for (root, owner, name) in
        [(&extension, "Заказ", "БезПары"), (&base, "Заказ", "ФормаДокумента")]
    {
        let source = borrowed_form_module(root, owner, name);
        let common = root.join("CommonForms/ОбщаяФорма/Ext/Form/Module.bsl");
        std::fs::create_dir_all(common.parent().unwrap()).unwrap();
        std::fs::copy(&source, &common).unwrap();
        std::fs::copy(
            source.parent().unwrap().parent().unwrap().join("Form.xml"),
            common.parent().unwrap().parent().unwrap().join("Form.xml"),
        )
        .unwrap();
        modules.push(common);
    }
    let db = borrowed_form_db(
        &[(None, &base, RootKind::Base), (Some("Расширение"), &extension, RootKind::Extension)],
        &modules,
    );
    let local = borrowed_form_local(&modules[0]);
    assert_eq!(borrowed_form_names(&local), ["СвойРеквизитФормы"]);
    assert_eq!(
        borrowed_form_names(&borrowed_form(&db, FileId(1)).unwrap()),
        ["Объект", "БазовыйРеквизитФормы"],
        "the same-named base common form is readable and has distinct attributes"
    );
    assert_eq!(*borrowed_form(&db, FileId(0)).unwrap(), local);
}

#[test]
fn borrowed_form_skips_an_ordinary_base_without_dialog() {
    let (_temp, base, extension) = borrowed_form_fixture_copy();
    let ext_module = borrowed_form_module(&extension, "Заказ", "ОбычнаяФорма");
    let base_module = borrowed_form_module(&base, "Заказ", "ОбычнаяФорма");
    std::fs::copy(
        base.join("Documents/Заказ/Forms/ОбычнаяФорма/Ext/Form.xml"),
        extension.join("Documents/Заказ/Forms/ОбычнаяФорма/Ext/Form.xml"),
    )
    .unwrap();
    let ext_dialog = extension.join("Documents/Заказ/Forms/ОбычнаяФорма/Ext/Form.xml");
    let dialog = std::fs::read_to_string(&ext_dialog).unwrap();
    std::fs::write(
        &ext_dialog,
        dialog
            .replace("БазовыйРеквизитФормы", "РасшРеквизитФормы")
            .replace("<MainAttribute>true</MainAttribute>", ""),
    )
    .unwrap();
    let roots = [
        (None, base.as_path(), RootKind::Base),
        (Some("Расширение"), extension.as_path(), RootKind::Extension),
    ];

    let control = borrowed_form_db(&roots, &[ext_module.clone(), base_module.clone()]);
    assert_eq!(
        borrowed_form_names(&borrowed_form(&control, FileId(0)).unwrap()),
        ["Объект", "БазовыйРеквизитФормы", "РасшРеквизитФормы"],
        "readable dialogs on both sides merge"
    );

    std::fs::remove_file(base.join("Documents/Заказ/Forms/ОбычнаяФорма/Ext/Form.xml")).unwrap();
    let db = borrowed_form_db(&roots, &[ext_module.clone(), base_module]);
    let base_form = borrowed_form(&db, FileId(1)).unwrap();
    assert!(base_form.is_ordinary() && base_form.is_handler("ПередОткрытием"));
    let merged = borrowed_form(&db, FileId(0)).unwrap();
    assert_eq!(*merged, borrowed_form_local(&ext_module));
    assert!(!merged.is_handler("ПередОткрытием"), "base default handlers do not leak");
}

#[test]
fn borrowed_form_follows_a_base_dialog_change_on_disk() {
    use bsl_types::intern::TypeKernelDb;
    use hir::HirDatabase;

    let (_temp, base, extension) = borrowed_form_fixture_copy();
    let ext_module = borrowed_form_module(&extension, "Заказ", "ФормаДокумента");
    let base_module = borrowed_form_module(&base, "Заказ", "ФормаДокумента");
    let mut db = borrowed_form_db(
        &[(None, &base, RootKind::Base), (Some("Расширение"), &extension, RootKind::Extension)],
        &[ext_module.clone(), base_module],
    );
    db.set_file_text(
        FileId(0),
        "&НаКлиенте\nПроцедура Проверка()\n\tНовое = НовыйРеквизитФормы;\nКонецПроцедуры\n",
    );

    let before = borrowed_form(&db, FileId(0)).unwrap();
    assert!(before.find_attribute("НовыйРеквизитФормы").is_none());
    let _ = db.infer(FileId(0));

    let base_dialog = base.join("Documents/Заказ/Forms/ФормаДокумента/Ext/Form.xml");
    let added = r#"<Attribute name="НовыйРеквизитФормы" id="90"><Type><v8:Type>xs:decimal</v8:Type><v8:NumberQualifiers><v8:Digits>5</v8:Digits><v8:FractionDigits>0</v8:FractionDigits></v8:NumberQualifiers></Type></Attribute>
  </Attributes>"#;
    let changed =
        std::fs::read_to_string(&base_dialog).unwrap().replacen("  </Attributes>", added, 1);
    std::fs::write(&base_dialog, changed).unwrap();
    db.bump_config_for_path(&base);

    let after = borrowed_form(&db, FileId(0)).unwrap();
    assert_eq!(
        after.find_attribute("НовыйРеквизитФормы").map(|attr| &attr.attr_type),
        Some(&bsl_metadata::AttributeType::Number { precision: 5, scale: 0 })
    );
    let ty = db.infer(FileId(0)).var_types.get("новое").copied().expect("variable typed");
    assert!(
        matches!(db.lookup_type(ty), bsl_types::kind::TypeKind::Number(..)),
        "inference sees the new base attribute: {:?}",
        db.lookup_type(ty)
    );
}

#[test]
fn borrowed_form_picks_up_a_base_dialog_that_appears_on_disk() {
    let (_temp, base, extension) = borrowed_form_fixture_copy();
    let ext_module = borrowed_form_module(&extension, "Заказ", "ФормаДокумента");
    let base_module = borrowed_form_module(&base, "Заказ", "ФормаДокумента");
    let base_dialog = base.join("Documents/Заказ/Forms/ФормаДокумента/Ext/Form.xml");
    let dialog = std::fs::read_to_string(&base_dialog).unwrap();
    std::fs::remove_file(&base_dialog).unwrap();
    let mut db = borrowed_form_db(
        &[(None, &base, RootKind::Base), (Some("Расширение"), &extension, RootKind::Extension)],
        &[ext_module.clone(), base_module],
    );
    assert_eq!(*borrowed_form(&db, FileId(0)).unwrap(), borrowed_form_local(&ext_module));

    std::fs::write(&base_dialog, dialog).unwrap();
    db.bump_config_for_path(&base);
    assert_eq!(
        borrowed_form_names(&borrowed_form(&db, FileId(0)).unwrap()),
        ["Объект", "БазовыйРеквизитФормы"],
        "a bump of the base root alone must re-run the extension's form"
    );
}
