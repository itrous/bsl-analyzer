//! An external object is a declared root of the workspace: the name dictionary
//! lists it under its own kind, beside a namesake of the base, and neither
//! shadows the other. The designer's view (`configurations_inventory`, module
//! pairing) is the surface that leaves externals out, not the inventory keyed
//! by `(kind, name)`.

use ide::{lookup_names, NameQuery, WorkspaceConfigsSnapshot, WorkspaceRootKind};
use ide_db::base_db::{SourceDatabase, SourceRoot, BSL_SOURCE_ROOT, METADATA_SOURCE_ROOT};
use ide_db::metadata::{MdoEntry, MetadataListingData};
use ide_db::RootDatabaseImpl;
use vfs::{FileId, FileSet, VfsPath};

const INTERNAL_XML: FileId = FileId(10);
const EXTERNAL_XML: FileId = FileId(11);

fn listing(kind: bsl_metadata::MdoType, main: FileId) -> MetadataListingData {
    MetadataListingData {
        entries: vec![MdoEntry { kind, name: "АРМ".to_string(), main, predefined: None }],
        ..Default::default()
    }
}

fn object_xml(element: &str, attribute: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:v8="http://v8.1c.ru/8.1/data/core" version="2.20">
	<{element} uuid="3696c164-ad14-4a0d-b659-10e3bf6d6ad2">
		<Properties><Name>АРМ</Name><Synonym/><Comment/></Properties>
		<ChildObjects><Attribute uuid="d010948a-27f1-4b21-80a2-361efec05def"><Properties><Name>{attribute}</Name><Type><v8:Type>xs:string</v8:Type></Type></Properties></Attribute></ChildObjects>
	</{element}>
</MetaDataObject>"#
    )
}

/// A base with an internal processor `АРМ` (attribute `Внутренний`) beside an
/// EPF export of a namesake (attribute `Внешний`), both registered and listed.
/// The XMLs live on disk too: a root's configuration is loaded from there.
fn two_roots() -> (RootDatabaseImpl, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("cf");
    let epf = dir.path().join("epf");
    std::fs::create_dir_all(base.join("DataProcessors")).unwrap();
    std::fs::create_dir_all(epf.join("АРМ")).unwrap();
    std::fs::write(base.join("DataProcessors/АРМ.xml"), object_xml("DataProcessor", "Внутренний"))
        .unwrap();
    std::fs::write(epf.join("АРМ.xml"), object_xml("ExternalDataProcessor", "Внешний")).unwrap();
    let mut db = RootDatabaseImpl::new();

    let mut metadata = FileSet::default();
    metadata.insert(INTERNAL_XML, VfsPath::from(base.join("DataProcessors/АРМ.xml")));
    metadata.insert(EXTERNAL_XML, VfsPath::from(epf.join("АРМ.xml")));
    db.set_source_root(BSL_SOURCE_ROOT, SourceRoot::new_local(FileSet::default()));
    db.set_source_root(METADATA_SOURCE_ROOT, SourceRoot::new_local(metadata));
    for (id, text) in [
        (INTERNAL_XML, object_xml("DataProcessor", "Внутренний")),
        (EXTERNAL_XML, object_xml("ExternalDataProcessor", "Внешний")),
    ] {
        db.set_file_source_root(id, METADATA_SOURCE_ROOT);
        db.set_file_text(id, &text);
    }

    let paths = vec![(None, base.clone()), (Some("АРМ".to_string()), epf.clone())];
    db.set_workspace_configs_snapshot(WorkspaceConfigsSnapshot {
        extension_scopes: vec![None, None],
        canonical_paths: paths.iter().map(|(_, p)| p.clone()).collect(),
        kinds: vec![
            WorkspaceRootKind::Base,
            WorkspaceRootKind::External(bsl_metadata::ExternalObjectKind::DataProcessor),
        ],
        paths,
        closures: vec![vec![], vec![]],
        topological_order: vec![0, 1],
        fingerprint: None,
        source_exclusions: Default::default(),
    });
    db.set_metadata_listing(
        &base.to_string_lossy(),
        listing(bsl_metadata::MdoType::DataProcessor, INTERNAL_XML),
    );
    db.set_metadata_listing(
        &epf.to_string_lossy(),
        listing(bsl_metadata::MdoType::ExternalDataProcessor, EXTERNAL_XML),
    );
    (db, dir)
}

#[test]
fn the_name_dictionary_lists_an_external_object_beside_the_base_namesake() {
    let (db, _dir) = two_roots();

    let found = lookup_names(&db, &NameQuery::new("АРМ", 20), &[]);
    let mut places: Vec<(String, FileId)> = found
        .candidates
        .iter()
        .filter_map(|c| Some((c.symbol.clone()?, c.place?.file_id)))
        .collect();
    places.sort();
    assert_eq!(
        places,
        vec![
            ("ВнешняяОбработка.АРМ".to_string(), EXTERNAL_XML),
            ("Обработка.АРМ".to_string(), INTERNAL_XML),
        ],
        "both objects are listed, each under its own kind and file"
    );
}

/// The by-name resolvers behind `symbol_info` and the MCP `metadata object`
/// tool have no file to anchor to; they must still reach an external object
/// under its own kind, and the namesake's attributes must stay apart.
#[test]
fn the_by_name_resolver_reaches_an_external_object_under_its_own_kind() {
    let (db, _dir) = two_roots();
    let attributes = |mdo_type: bsl_metadata::MdoType| -> Option<Vec<String>> {
        let object = db.resolve_metadata_object_across_roots(mdo_type, "АРМ")?;
        Some(object.attributes.iter().map(|a| a.name.to_string()).collect())
    };
    assert_eq!(
        attributes(bsl_metadata::MdoType::DataProcessor),
        Some(vec!["Внутренний".to_string()]),
        "control: the internal processor resolves to the base's object alone"
    );
    assert_eq!(
        attributes(bsl_metadata::MdoType::ExternalDataProcessor),
        Some(vec!["Внешний".to_string()]),
        "the external processor resolves to the export's object"
    );
}

/// A query handed in as bare text composes over the designer's view: SDBL
/// names no external object, and the roots the tool reports must be the roots
/// the resolver read.
#[test]
fn the_query_resolver_keeps_the_designer_view() {
    use bsl_metadata::QueryMetadataResolver as _;

    let (db, _dir) = two_roots();
    let resolver = ide::AcrossRootsQueryResolver::new(&db);
    assert!(
        resolver.resolve_metadata_object(bsl_metadata::MdoType::DataProcessor, "АРМ").is_some(),
        "control: the base's object resolves"
    );
    assert!(
        resolver
            .resolve_metadata_object(bsl_metadata::MdoType::ExternalDataProcessor, "АРМ")
            .is_none(),
        "the external object stays outside the query's view"
    );
}

/// The inventory of configurations feeds the graph's catalog of objects and
/// their members; keyed by kind and name like the dictionary, it lists the
/// external object under its own kind.
#[test]
fn the_configurations_inventory_lists_the_external_object() {
    use ide_db::RootDatabase as _;

    let (db, _dir) = two_roots();
    let kinds_of_arm: Vec<bsl_metadata::MdoType> = db
        .all_configurations_inventory()
        .iter()
        .flat_map(|(_, config)| {
            config
                .metadata_objects()
                .iter()
                .filter(|object| object.name == "АРМ")
                .map(|object| object.mdo_type)
                .collect::<Vec<_>>()
        })
        .collect();
    assert_eq!(
        kinds_of_arm,
        vec![bsl_metadata::MdoType::DataProcessor, bsl_metadata::MdoType::ExternalDataProcessor],
        "the base's object first, the export's after it"
    );
}
