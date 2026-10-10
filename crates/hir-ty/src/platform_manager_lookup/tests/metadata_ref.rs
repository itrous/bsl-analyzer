#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn metadata_ref_catalog_object_resolves_write_as_procedure() {
    let db = InMemoryDb::new();
    let res = resolve_platform_metadata_ref_method(
        &db,
        MetadataKind::CatalogObject,
        &Name::new("Номенклатура"),
        &Name::new("Записать"),
    )
    .expect("platform data indexes Write under CatalogObject");
    assert_eq!(res.return_ty, db.undefined());
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn any_metadata_ref_resolves_common_method_without_name() {
    let db = InMemoryDb::new();
    let res = resolve_platform_any_metadata_ref_method(
        &db,
        MdoType::Catalog,
        &Name::new("Метаданные"),
    );
    assert!(res.is_some(), "Metadata() must resolve on AnyMetadataRef{{Catalog}}");
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn any_metadata_ref_object_return_degrades_to_unknown() {
    let db = InMemoryDb::new();
    let res = resolve_platform_any_metadata_ref_method(
        &db,
        MdoType::Catalog,
        &Name::new("ПолучитьОбъект"),
    )
    .expect("GetObject must resolve on AnyMetadataRef{Catalog}");
    assert_eq!(res.return_ty, db.unknown(), "object return has no name to bind → Unknown");
}

#[test]
fn any_metadata_ref_unknown_method_is_none() {
    let db = InMemoryDb::new();
    assert!(resolve_platform_any_metadata_ref_method(
        &db,
        MdoType::Catalog,
        &Name::new("НесуществующийМетод"),
    )
    .is_none());
}

#[test]
fn any_metadata_ref_register_flavour_has_no_ref_surface() {
    let db = InMemoryDb::new();
    assert!(resolve_platform_any_metadata_ref_method(
        &db,
        MdoType::InformationRegister,
        &Name::new("Метаданные"),
    )
    .is_none());
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn metadata_ref_register_record_manager_resolves_write() {
    let db = InMemoryDb::new();
    let res = resolve_platform_metadata_ref_method(
        &db,
        MetadataKind::InformationRegisterRecordManager,
        &Name::new("Курсы"),
        &Name::new("Записать"),
    )
    .expect("platform data indexes Write under InformationRegisterRecordManager");
    assert_eq!(res.return_ty, db.undefined());
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn metadata_ref_information_register_record_set_resolves_load() {
    let db = InMemoryDb::new();
    let res = resolve_platform_metadata_ref_method(
        &db,
        MetadataKind::InformationRegisterRecordSet,
        &Name::new("Курсы"),
        &Name::new("Загрузить"),
    )
    .expect("platform data indexes Load under InformationRegisterRecordSet");
    assert_eq!(res.return_ty, db.undefined());
}

/// An external object's corpus entries carry a bare `type_name`, so the manager
/// prefix index — the path every other object kind takes — lists nothing for it.
/// The control asserts that emptiness explicitly: were the prefix route ever to
/// start answering, the bare-type route would be a duplicate, not a fix.
#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn external_object_methods_come_from_the_bare_platform_type_not_the_prefix_index() {
    assert!(
        bsl_platform::find_prefixed_methods("ExternalDataProcessorObject", "ПолучитьФорму")
            .is_empty(),
        "control: the prefix index has no dotted entries for an external object"
    );
    let db = InMemoryDb::new();
    for method in ["ПолучитьФорму", "Метаданные", "GetForm"] {
        let res = resolve_platform_metadata_ref_method(
            &db,
            MetadataKind::ExternalDataProcessorObject,
            &Name::new("АРМПроизводство"),
            &Name::new(method),
        );
        assert!(res.is_some(), "{method} must resolve on ВнешняяОбработкаОбъект");
    }
    assert!(
        resolve_platform_metadata_ref_method(
            &db,
            MetadataKind::ExternalDataProcessorObject,
            &Name::new("АРМПроизводство"),
            &Name::new("НетТакогоМетода"),
        )
        .is_none(),
        "an unknown name stays unresolved"
    );
    assert!(
        platform_methods_for_metadata_kind(MetadataKind::ExternalReportObject)
            .iter()
            .any(|m| m.name == "ПолучитьФорму"),
        "the report kind enumerates the bare type's methods too"
    );
}
