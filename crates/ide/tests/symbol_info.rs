use ide::{
    is_well_formed_symbol, symbol_info, SymbolInfoRequest, SymbolInfoSections, SymbolPosition,
};
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::metadata::{MdoEntry, MetadataListingData, WorkspaceConfigsSnapshot};
use ide_db::RootDatabaseImpl;
use std::path::PathBuf;
use test_fixture::Fixture;
use vfs::{FileId, FileSet, VfsPath};

fn designer_fixture_path() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-metadata/fixtures/designer"))
}

const CATALOG_XML: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../bsl-metadata/fixtures/designer/Catalogs/Справочник1.xml"
));

const CFE_CATALOG_XML: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../bsl-metadata/fixtures/cfe_dependencies/base/Catalogs/Товары.xml"
));

const DATA_PROCESSOR_XML: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../bsl-metadata/fixtures/designer/DataProcessors/ТестоваяОбработка.xml"
));

const REGISTER_XML: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../bsl-metadata/fixtures/designer/InformationRegisters/РегистрСведений1.xml"
));

const DOCUMENT_XML: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../bsl-metadata/fixtures/designer/Documents/Документ1.xml"
));

const DOCUMENT_FORM_MODULE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../bsl-metadata/fixtures/designer/Documents/Документ1/Forms/ФормаДокумента/Ext/Form/Module.bsl"
));

const DOCUMENT_FORM_XML: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../bsl-metadata/fixtures/designer/Documents/Документ1/Forms/ФормаДокумента/Ext/Form.xml"
));

const COMMON_FORM_MODULE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../bsl-metadata/fixtures/designer/CommonForms/ТестоваяФорма/Ext/Form/Module.bsl"
));

const DATA_PROCESSOR_FORM_MODULE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../bsl-metadata/fixtures/designer/DataProcessors/ТестоваяОбработка/Forms/Форма/Ext/Form/Module.bsl"
));

const REPORT_XML: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../bsl-metadata/fixtures/designer/Reports/ТестовыйОтчёт.xml"
));

const REPORT_FORM_MODULE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../bsl-metadata/fixtures/designer/Reports/ТестовыйОтчёт/Forms/Форма/Ext/Form/Module.bsl"
));

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

/// A db carrying only BSL modules, with the module index built from file paths (no metadata
/// substrate). Enough for common-module and positional resolution.
fn setup_bsl(fixture_text: &str) -> (RootDatabaseImpl, FileId) {
    let fixture = Fixture::parse(fixture_text);
    let mut db = RootDatabaseImpl::new();
    let mut file_set = FileSet::default();
    for (file_id, file) in &fixture.files {
        file_set.insert(*file_id, file.path.clone());
    }
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    for (file_id, file) in &fixture.files {
        db.set_file_source_root(*file_id, SourceRootId(0));
        db.set_file_text(*file_id, &file.content);
    }
    let test_file = fixture
        .files
        .iter()
        .find(|(_, f)| f.path.as_path().to_string_lossy().ends_with("/test.bsl"))
        .map(|(id, _)| *id)
        .unwrap_or_else(|| *fixture.files.keys().next().expect("fixture has a file"));
    (db, test_file)
}

/// A db with the `Справочник.Справочник1` catalog wired into the metadata substrate the way
/// the resident host does (config path + a listing entry mapping the object to its XML).
fn setup_catalog() -> RootDatabaseImpl {
    let mut db = RootDatabaseImpl::new();
    let bsl = FileId(0);
    let xml = FileId(1);
    let data_processor_xml = FileId(2);
    let catalog_object_module = FileId(3);
    let catalog_manager_module = FileId(4);
    let designer = designer_fixture_path();
    let mut file_set = FileSet::new();
    file_set.insert(bsl, VfsPath::new("/test.bsl"));
    file_set.insert(xml, VfsPath::from(designer.join("Catalogs/Справочник1.xml")));
    file_set.insert(
        data_processor_xml,
        VfsPath::from(designer.join("DataProcessors/ТестоваяОбработка.xml")),
    );
    file_set.insert(
        catalog_object_module,
        VfsPath::from(designer.join("Catalogs/Справочник1/Ext/ObjectModule.bsl")),
    );
    file_set.insert(
        catalog_manager_module,
        VfsPath::from(designer.join("Catalogs/Справочник1/Ext/ManagerModule.bsl")),
    );
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    db.set_file_source_root(bsl, SourceRootId(0));
    db.set_file_source_root(xml, SourceRootId(0));
    db.set_file_source_root(data_processor_xml, SourceRootId(0));
    db.set_file_source_root(catalog_object_module, SourceRootId(0));
    db.set_file_source_root(catalog_manager_module, SourceRootId(0));
    db.set_file_text(bsl, "Процедура Тест()\nКонецПроцедуры\n");
    db.set_file_text(xml, CATALOG_XML);
    db.set_file_text(data_processor_xml, DATA_PROCESSOR_XML);
    db.set_file_text(
        catalog_object_module,
        "Перем ЭкспортнаяПеременная, ТолькоОбъект Экспорт;\n\
         Функция ЭкспортныйМетод(Параметр) Экспорт\n\
             Возврат Параметр;\n\
         КонецФункции\n",
    );
    db.set_file_text(
        catalog_manager_module,
        "Перем ЭкспортнаяПеременнаяМенеджера Экспорт;\n\
         Функция ЭкспортныйМетодМенеджера() Экспорт\n\
             Возврат Истина;\n\
         КонецФункции\n",
    );
    db.set_all_config_paths(vec![(None, designer.clone())]);
    db.set_metadata_listing(
        &designer.to_string_lossy(),
        MetadataListingData {
            entries: vec![
                MdoEntry {
                    kind: bsl_metadata::MdoType::Catalog,
                    name: "Справочник1".to_string(),
                    main: xml,
                    predefined: None,
                },
                MdoEntry {
                    kind: bsl_metadata::MdoType::DataProcessor,
                    name: "ТестоваяОбработка".to_string(),
                    main: data_processor_xml,
                    predefined: None,
                },
            ],
            ..empty_listing_data()
        },
    );
    db
}

/// A db with `РегистрСведений.РегистрСведений1` wired into the metadata substrate, mirroring
/// how the resident host lists a register object.
fn setup_register() -> RootDatabaseImpl {
    let mut db = RootDatabaseImpl::new();
    let bsl = FileId(0);
    let xml = FileId(1);
    let designer = designer_fixture_path();
    let mut file_set = FileSet::new();
    file_set.insert(bsl, VfsPath::new("/test.bsl"));
    file_set.insert(xml, VfsPath::from(designer.join("InformationRegisters/РегистрСведений1.xml")));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    db.set_file_source_root(bsl, SourceRootId(0));
    db.set_file_source_root(xml, SourceRootId(0));
    db.set_file_text(bsl, "Процедура Тест()\nКонецПроцедуры\n");
    db.set_file_text(xml, REGISTER_XML);
    db.set_all_config_paths(vec![(None, designer.clone())]);
    db.set_metadata_listing(
        &designer.to_string_lossy(),
        MetadataListingData {
            entries: vec![MdoEntry {
                kind: bsl_metadata::MdoType::InformationRegister,
                name: "РегистрСведений1".to_string(),
                main: xml,
                predefined: None,
            }],
            ..empty_listing_data()
        },
    );
    db
}

fn setup_forms() -> RootDatabaseImpl {
    setup_forms_with_extension(None)
}

fn setup_forms_with_extension(extension_module: Option<&str>) -> RootDatabaseImpl {
    let mut db = RootDatabaseImpl::new();
    let document_form = FileId(0);
    let common_form = FileId(1);
    let document_xml = FileId(2);
    let data_processor_form = FileId(3);
    let report_form = FileId(4);
    let data_processor_xml = FileId(5);
    let report_xml = FileId(6);
    let extension_form = FileId(7);
    let designer = designer_fixture_path();
    let extension = PathBuf::from("/extension");
    let document_form_path =
        designer.join("Documents/Документ1/Forms/ФормаДокумента/Ext/Form/Module.bsl");
    let common_form_path = designer.join("CommonForms/ТестоваяФорма/Ext/Form/Module.bsl");
    let document_xml_path = designer.join("Documents/Документ1.xml");
    let data_processor_form_path =
        designer.join("DataProcessors/ТестоваяОбработка/Forms/Форма/Ext/Form/Module.bsl");
    let report_form_path = designer.join("Reports/ТестовыйОтчёт/Forms/Форма/Ext/Form/Module.bsl");
    let data_processor_xml_path = designer.join("DataProcessors/ТестоваяОбработка.xml");
    let report_xml_path = designer.join("Reports/ТестовыйОтчёт.xml");
    let mut file_set = FileSet::new();
    file_set.insert(document_form, VfsPath::from(document_form_path));
    file_set.insert(common_form, VfsPath::from(common_form_path));
    file_set.insert(document_xml, VfsPath::from(document_xml_path));
    file_set.insert(data_processor_form, VfsPath::from(data_processor_form_path));
    file_set.insert(report_form, VfsPath::from(report_form_path));
    file_set.insert(data_processor_xml, VfsPath::from(data_processor_xml_path));
    file_set.insert(report_xml, VfsPath::from(report_xml_path));
    if extension_module.is_some() {
        file_set.insert(
            extension_form,
            VfsPath::from(
                extension.join("Documents/Документ1/Forms/ФормаДокумента/Ext/Form/Module.bsl"),
            ),
        );
    }
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    db.set_file_source_root(document_form, SourceRootId(0));
    db.set_file_source_root(common_form, SourceRootId(0));
    db.set_file_source_root(document_xml, SourceRootId(0));
    db.set_file_source_root(data_processor_form, SourceRootId(0));
    db.set_file_source_root(report_form, SourceRootId(0));
    db.set_file_source_root(data_processor_xml, SourceRootId(0));
    db.set_file_source_root(report_xml, SourceRootId(0));
    if extension_module.is_some() {
        db.set_file_source_root(extension_form, SourceRootId(0));
    }
    db.set_file_text(document_form, DOCUMENT_FORM_MODULE);
    db.set_file_text(common_form, COMMON_FORM_MODULE);
    db.set_file_text(document_xml, DOCUMENT_XML);
    db.set_file_text(data_processor_form, DATA_PROCESSOR_FORM_MODULE);
    db.set_file_text(report_form, REPORT_FORM_MODULE);
    db.set_file_text(data_processor_xml, DATA_PROCESSOR_XML);
    db.set_file_text(report_xml, REPORT_XML);
    if let Some(module) = extension_module {
        db.set_file_text(extension_form, module);
        db.set_all_config_paths(vec![
            (None, designer.clone()),
            (Some("Расширение".to_string()), extension.clone()),
        ]);
        db.set_metadata_listing(&extension.to_string_lossy(), empty_listing_data());
    } else {
        db.set_all_config_paths(vec![(None, designer.clone())]);
    }
    db.set_metadata_listing(
        &designer.to_string_lossy(),
        MetadataListingData {
            entries: vec![
                MdoEntry {
                    kind: bsl_metadata::MdoType::Document,
                    name: "Документ1".to_string(),
                    main: document_xml,
                    predefined: None,
                },
                MdoEntry {
                    kind: bsl_metadata::MdoType::DataProcessor,
                    name: "ТестоваяОбработка".to_string(),
                    main: data_processor_xml,
                    predefined: None,
                },
                MdoEntry {
                    kind: bsl_metadata::MdoType::Report,
                    name: "ТестовыйОтчёт".to_string(),
                    main: report_xml,
                    predefined: None,
                },
            ],
            ..empty_listing_data()
        },
    );
    db
}

fn by_name(db: &RootDatabaseImpl, symbol: &str) -> Option<ide::SymbolInfoCard> {
    symbol_info(
        db,
        &SymbolInfoRequest {
            symbol: Some(symbol.to_string()),
            position: None,
            locale: ide::Locale::default(),
            sections: SymbolInfoSections::all(),
            // Form-handler `graph_id` is encoded relative to this root; the fixtures live under
            // the designer root, so a handler path strips to `Documents/…/Module.bsl`.
            workspace_root: Some(ide::StripRoot::resolve(&designer_fixture_path())),
        },
    )
}

fn by_name_at(db: &RootDatabaseImpl, symbol: &str, file_id: FileId) -> Option<ide::SymbolInfoCard> {
    symbol_info(
        db,
        &SymbolInfoRequest {
            symbol: Some(symbol.to_string()),
            position: Some(SymbolPosition { file_id, line: 0, column: 0 }),
            locale: ide::Locale::default(),
            sections: SymbolInfoSections::all(),
            workspace_root: None,
        },
    )
}

fn setup_applied_visibility() -> RootDatabaseImpl {
    let roots = ["/base", "/extension-a", "/extension-b"];
    let paths = vec![
        (None, PathBuf::from(roots[0])),
        (Some("РасширениеА".to_string()), PathBuf::from(roots[1])),
        (Some("РасширениеБ".to_string()), PathBuf::from(roots[2])),
    ];
    let mut db = RootDatabaseImpl::new();
    db.set_workspace_configs_snapshot(WorkspaceConfigsSnapshot {
        kinds: ide_db::metadata::RootKind::from_labels(&paths),
        canonical_paths: paths.iter().map(|(_, path)| path.clone()).collect(),
        paths,
        closures: vec![vec![], vec![], vec![]],
        topological_order: vec![0, 1, 2],
        fingerprint: Some("symbol-info-visibility".to_string()),
        source_exclusions: Default::default(),
    });

    let mut file_set = FileSet::new();
    for (index, root) in roots.iter().enumerate() {
        file_set.insert(FileId(index as u32), VfsPath::new(format!("{root}/Catalogs/Товары.xml")));
        file_set.insert(
            FileId((index + 3) as u32),
            VfsPath::new(format!("{root}/Catalogs/Товары/Ext/ObjectModule.bsl")),
        );
    }
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));

    let attributes = ["Цвет", "ТолькоА", "ТолькоБ"];
    let exports = ["Базовый", "ТолькоА", "ТолькоБ"];
    for index in 0..3 {
        let xml = FileId(index as u32);
        let module = FileId((index + 3) as u32);
        db.set_file_source_root(xml, SourceRootId(0));
        db.set_file_source_root(module, SourceRootId(0));
        db.set_file_text(
            xml,
            &CFE_CATALOG_XML
                .replace("<Name>Цвет</Name>", &format!("<Name>{}</Name>", attributes[index])),
        );
        db.set_file_text(
            module,
            &format!("Процедура {}() Экспорт\nКонецПроцедуры\n", exports[index]),
        );
        db.set_metadata_listing(
            roots[index],
            MetadataListingData {
                entries: vec![MdoEntry {
                    kind: bsl_metadata::MdoType::Catalog,
                    name: "Товары".to_string(),
                    main: xml,
                    predefined: None,
                }],
                ..empty_listing_data()
            },
        );
    }
    db
}

fn setup_form_resolution_visibility() -> (RootDatabaseImpl, PathBuf) {
    let temp_root = std::env::temp_dir().join(format!(
        "bsl-analyzer-form-visibility-{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    ));
    let roots =
        [temp_root.join("base"), temp_root.join("extension-a"), temp_root.join("extension-b")];
    for root in &roots {
        let ext = root.join("Documents/Документ1/Forms/ФормаДокумента/Ext");
        std::fs::create_dir_all(ext.join("Form")).unwrap();
        std::fs::write(ext.join("Form.xml"), DOCUMENT_FORM_XML).unwrap();
    }
    let paths = vec![
        (None, roots[0].clone()),
        (Some("РасширениеА".to_string()), roots[1].clone()),
        (Some("РасширениеБ".to_string()), roots[2].clone()),
    ];
    let mut db = RootDatabaseImpl::new();
    db.set_workspace_configs_snapshot(WorkspaceConfigsSnapshot {
        kinds: ide_db::metadata::RootKind::from_labels(&paths),
        canonical_paths: paths.iter().map(|(_, path)| path.clone()).collect(),
        paths,
        closures: vec![vec![], vec![], vec![]],
        topological_order: vec![0, 1, 2],
        fingerprint: Some("form-resolution-visibility".to_string()),
        source_exclusions: Default::default(),
    });

    let xml = FileId(0);
    let extension_a_xml = FileId(1);
    let extension_b_xml = FileId(2);
    let extension_a = FileId(3);
    let extension_b = FileId(4);
    let mut file_set = FileSet::new();
    file_set.insert(xml, VfsPath::from(roots[0].join("Documents/Документ1.xml")));
    file_set.insert(extension_a_xml, VfsPath::from(roots[1].join("Documents/Документ1.xml")));
    file_set.insert(extension_b_xml, VfsPath::from(roots[2].join("Documents/Документ1.xml")));
    file_set.insert(
        extension_a,
        VfsPath::from(
            roots[1].join("Documents/Документ1/Forms/ФормаДокумента/Ext/Form/Module.bsl"),
        ),
    );
    file_set.insert(
        extension_b,
        VfsPath::from(
            roots[2].join("Documents/Документ1/Forms/ФормаДокумента/Ext/Form/Module.bsl"),
        ),
    );
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    for file in [xml, extension_a_xml, extension_b_xml, extension_a, extension_b] {
        db.set_file_source_root(file, SourceRootId(0));
    }
    db.set_file_text(xml, DOCUMENT_XML);
    db.set_file_text(extension_a_xml, DOCUMENT_XML);
    db.set_file_text(extension_b_xml, DOCUMENT_XML);
    db.set_file_text(extension_a, "Процедура ЛокальныйА()\nКонецПроцедуры\n");
    db.set_file_text(extension_b, "Процедура ЛокальныйБ()\nКонецПроцедуры\n");
    db.set_metadata_listing(
        &roots[0].to_string_lossy(),
        MetadataListingData {
            entries: vec![MdoEntry {
                kind: bsl_metadata::MdoType::Document,
                name: "Документ1".to_string(),
                main: xml,
                predefined: None,
            }],
            ..empty_listing_data()
        },
    );
    for (root, main) in [(&roots[1], extension_a_xml), (&roots[2], extension_b_xml)] {
        db.set_metadata_listing(
            &root.to_string_lossy(),
            MetadataListingData {
                entries: vec![MdoEntry {
                    kind: bsl_metadata::MdoType::Document,
                    name: "Документ1".to_string(),
                    main,
                    predefined: None,
                }],
                ..empty_listing_data()
            },
        );
    }
    (db, temp_root)
}

const COMMON_MODULE: &str = r#"
//- /CommonModules/МойМодуль/Ext/Module.bsl
// Складывает два числа.
Функция Сложить(А, Б) Экспорт
    Возврат А + Б;
КонецФункции

Процедура ВнутренняяПроцедура()
КонецПроцедуры

Функция СУмолчаниями(Знач Ссылка, Флаг = Ложь, Значение = Неопределено) Экспорт
    Возврат Ссылка;
КонецФункции

Функция СоСтрокой(Текст = "Иван  Петров") Экспорт
    Возврат Текст;
КонецФункции

//- /test.bsl
Процедура Тест(Парам)
    Б = Парам;
КонецПроцедуры
"#;

const OBJECT_MODULE: &str = r#"
//- /Documents/Документ1/Ext/ObjectModule.bsl
Процедура ПубличныйМетод() Экспорт
КонецПроцедуры

Процедура ПриватныйМетод()
КонецПроцедуры

//- /test.bsl
Процедура Тест()
КонецПроцедуры
"#;

#[test]
fn common_module_method_by_qualified_name() {
    let (db, _) = setup_bsl(COMMON_MODULE);
    let card = by_name(&db, "МойМодуль.Сложить").expect("resident resolves the method");
    assert_eq!(card.kind, "function");
    assert_eq!(card.symbol, "МойМодуль.Сложить");
    let container = card.container.expect("method carries a container");
    assert_eq!(container.kind, "ОбщийМодуль");
    assert_eq!(container.name, "МойМодуль");
    let signature = card.signature.expect("signature rendered");
    // A definition-site declaration: keyword + bare name, no call qualifier, export marker kept.
    assert!(
        signature.starts_with("Функция Сложить("),
        "declaration starts with keyword+bare name: {signature:?}"
    );
    assert!(
        !signature.contains("МойМодуль"),
        "qualifier must not leak into the declaration: {signature:?}"
    );
    assert!(signature.contains("Экспорт"), "export marker present: {signature:?}");
    let def = card.definition.expect("definition site");
    assert!(def.line >= 1);
    assert!(def.snippet.is_some());
}

#[test]
fn declaration_renders_by_value_marker_and_default_values() {
    let (db, _) = setup_bsl(COMMON_MODULE);
    let card = by_name(&db, "МойМодуль.СУмолчаниями").expect("resident resolves the method");
    let signature = card.signature.expect("signature rendered");
    // The declaration keeps `Знач` and the default-value expressions, and drops the qualifier.
    assert_eq!(
        signature,
        "Функция СУмолчаниями(Знач Ссылка, Флаг = Ложь, Значение = Неопределено) Экспорт",
        "faithful declaration with Знач + defaults: {signature:?}"
    );
}

#[test]
fn string_literal_default_keeps_internal_spaces() {
    let (db, _) = setup_bsl(COMMON_MODULE);
    let card = by_name(&db, "МойМодуль.СоСтрокой").expect("resident resolves the method");
    let signature = card.signature.expect("signature rendered");
    // A single-line string-literal default must survive verbatim — its internal double space is
    // not collapsed (only multi-line defaults are whitespace-normalised).
    assert_eq!(
        signature, "Функция СоСтрокой(Текст = \"Иван  Петров\") Экспорт",
        "string default preserved: {signature:?}"
    );
}

#[test]
fn non_exported_common_module_method_does_not_resolve() {
    // Only exported common-module methods are addressable by qualified name.
    let (db, _) = setup_bsl(COMMON_MODULE);
    assert!(by_name(&db, "МойМодуль.ВнутренняяПроцедура").is_none());
}

#[test]
fn unknown_symbol_is_a_miss_not_a_panic() {
    let (db, _) = setup_bsl(COMMON_MODULE);
    // A resident miss returns None; the adapter offers graph candidates.
    assert!(by_name(&db, "НетТакогоМодуля.НетТакогоМетода").is_none());
}

#[test]
fn metadata_object_card_lists_members() {
    let db = setup_catalog();
    let card = by_name(&db, "Справочник.Справочник1").expect("object resolves");
    assert_eq!(card.kind, "metadata object");
    let container = card.container.expect("object container");
    assert_eq!(container.kind, "Справочник");
    assert!(
        card.members.iter().any(|m| m.name == "Реквизит1" && m.kind == "Реквизит"),
        "members were {:?}",
        card.members
    );
}

#[test]
fn metadata_attribute_card_has_type_and_ownership() {
    let db = setup_catalog();
    let card = by_name(&db, "Справочник.Справочник1.Реквизит1").expect("attribute resolves");
    assert_eq!(card.kind, "attribute");
    assert!(card.return_type.is_some(), "attribute carries its type");
    let container = card.container.expect("attribute ownership");
    assert_eq!(container.kind, "Справочник");
    assert_eq!(container.name, "Справочник1");
}

#[test]
fn symbol_info_whole_object_form_card_lists_attributes_items_and_handlers() {
    // Given: a resident db with the real document form fixture.
    let db = setup_forms();

    // When: resolving the qualified object-owned form name with the `Форма` marker.
    let card = by_name(&db, "Документ.Документ1.Форма.ФормаДокумента").expect("form resolves");

    // Then: the card describes the form structure rather than an object method.
    assert_eq!(card.kind, "form");
    let container = card.container.expect("form container");
    assert_eq!(container.kind, "Форма");
    assert_eq!(container.name, "Документ1.ФормаДокумента");
    assert!(
        card.signature.as_deref().is_some_and(|s| s.contains("реквизит") && s.contains("элемент")),
        "signature was {:?}",
        card.signature
    );
    assert!(
        card.members.iter().any(|m| m.name == "Объект" && m.kind.contains("Реквизит")),
        "members were {:?}",
        card.members
    );
    assert!(
        card.members.iter().any(|m| m.name == "Реквизит1" && m.kind == "ПолеФормы"),
        "members were {:?}",
        card.members
    );
    assert!(
        card.members.iter().any(|m| m.name == "ПриЗаписиНаСервере" && m.kind == "Обработчик"),
        "members were {:?}",
        card.members
    );
    assert!(card.graph_id.is_none(), "whole-form card must not carry usages id");
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn managed_form_full_and_exact_queries_share_candidates_and_keep_private_fallback() {
    let mut db = setup_forms();
    db.set_file_text(
        FileId(0),
        "Процедура Реквизит1() Экспорт\nКонецПроцедуры\n\
         Процедура ЗакрытыйHelper()\nКонецПроцедуры\n",
    );

    let full = by_name(&db, "Документ.Документ1.Форма.ФормаДокумента").unwrap();
    assert!(full.members.iter().any(|member| {
        member.name == "Реквизит1" && member.origin == ide::SymbolMemberOrigin::Module
    }));
    assert!(full.members.iter().any(|member| member.origin == ide::SymbolMemberOrigin::Platform));

    let exact = by_name(&db, "Документ.Документ1.Форма.ФормаДокумента.Реквизит1").unwrap();
    assert_eq!(exact.kind, "member candidates");
    assert_eq!(
        exact.members.iter().map(|member| member.origin).collect::<Vec<_>>(),
        [ide::SymbolMemberOrigin::Metadata, ide::SymbolMemberOrigin::Module]
    );

    let helper = by_name(&db, "Документ.Документ1.Форма.ФормаДокумента.ЗакрытыйHelper")
        .expect("non-exported local method remains the fallback");
    assert_eq!(helper.kind, "procedure");
    assert!(helper.signature.as_deref().is_some_and(|value| value.contains("ЗакрытыйHelper")));
}

/// A local method keeps its card when the platform form type declares the same name.
///
/// `ФормаКлиентскогоПриложения` brings 41 methods — `Закрыть`, `Открыть`,
/// `ПроверитьЗаполнение` — and a form module routinely declares a handler named after one of
/// them. The fallback that answers by the module's own method must not be shadowed by the
/// platform member that merely shares the name.
#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn managed_form_local_method_survives_a_platform_name_collision() {
    let mut db = setup_forms();
    db.set_file_text(FileId(0), "Процедура Закрыть()\nКонецПроцедуры\n");

    let full = by_name(&db, "Документ.Документ1.Форма.ФормаДокумента").unwrap();
    assert!(
        full.members
            .iter()
            .any(|member| member.name == "Закрыть"
                && member.origin == ide::SymbolMemberOrigin::Platform),
        "стенд обязан содержать платформенное имя-двойник, иначе проверка холостая"
    );

    let exact = by_name(&db, "Документ.Документ1.Форма.ФормаДокумента.Закрыть")
        .expect("локальный метод формы остаётся адресуемым");
    assert_eq!(exact.kind, "procedure", "{exact:?}");
    assert!(exact.signature.as_deref().is_some_and(|value| value.contains("Закрыть")));
}

#[test]
fn managed_form_uses_effective_extension_exports_for_full_and_exact_cards() {
    let mut db = setup_forms_with_extension(Some(
        "Процедура ТолькоРасширение() Экспорт\nКонецПроцедуры\n\
         &Вместо(\"Замещаемый\")\nПроцедура РасширениеВместо()\nКонецПроцедуры\n\
         &Перед(\"Обернутый\")\nПроцедура РасширениеПеред()\nКонецПроцедуры\n",
    ));
    db.set_file_text(
        FileId(0),
        "Процедура Замещаемый() Экспорт\nКонецПроцедуры\n\
         Процедура Обернутый() Экспорт\nКонецПроцедуры\n",
    );

    let full = by_name(&db, "Документ.Документ1.Форма.ФормаДокумента").unwrap();
    assert!(full.members.iter().any(|member| {
        member.name == "ТолькоРасширение"
            && member.source_extension.as_deref() == Some("Расширение")
    }));
    let replaced =
        full.members.iter().filter(|member| member.name == "Замещаемый").collect::<Vec<_>>();
    assert_eq!(replaced.len(), 1);
    assert_eq!(replaced[0].source_extension.as_deref(), Some("Расширение"));

    let extension_only = by_name(&db, "Документ.Документ1.Форма.ФормаДокумента.ТолькоРасширение")
        .expect("extension-only export remains an exact candidate");
    assert_eq!(extension_only.kind, "member candidates");
    assert_eq!(extension_only.members.len(), 1);
    assert_eq!(extension_only.members[0].source_extension.as_deref(), Some("Расширение"));

    let wrapped = by_name(&db, "Документ.Документ1.Форма.ФормаДокумента.Обернутый")
        .expect("base and before candidates remain separate");
    assert_eq!(wrapped.kind, "member candidates");
    assert_eq!(wrapped.members.len(), 2);
    assert!(wrapped.members.iter().any(|member| member.source_extension.is_none()));
    assert!(wrapped
        .members
        .iter()
        .any(|member| { member.source_extension.as_deref() == Some("Расширение") }));
}

#[test]
fn managed_form_position_excludes_an_invisible_extension_module() {
    let db =
        setup_forms_with_extension(Some("Процедура ТолькоРасширение() Экспорт\nКонецПроцедуры\n"));

    let designer = by_name(&db, "Документ.Документ1.Форма.ФормаДокумента").unwrap();
    assert!(designer.members.iter().any(|member| member.name == "ТолькоРасширение"));

    let from_base = by_name_at(&db, "Документ.Документ1.Форма.ФормаДокумента", FileId(0)).unwrap();
    assert!(from_base.members.iter().all(|member| member.name != "ТолькоРасширение"));
}

#[test]
fn managed_form_position_resolves_only_a_visible_form_module() {
    let (db, temp_root) = setup_form_resolution_visibility();

    assert!(by_name(&db, "Документ.Документ1.Форма.ФормаДокумента.ЛокальныйА").is_some());
    assert!(
        by_name_at(&db, "Документ.Документ1.Форма.ФормаДокумента.ЛокальныйБ", FileId(4),).is_some()
    );
    assert!(
        by_name_at(&db, "Документ.Документ1.Форма.ФормаДокумента.ЛокальныйА", FileId(4),).is_none()
    );
    std::fs::remove_dir_all(temp_root).unwrap();
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn managed_form_platform_extensions_follow_main_attribute_type() {
    let db = setup_forms();
    let document = by_name(&db, "Документ.Документ1.Форма.ФормаДокумента").unwrap();
    let processing = by_name(&db, "Обработка.ТестоваяОбработка.Форма.Форма").unwrap();
    let report = by_name(&db, "Отчет.ТестовыйОтчёт.Форма.Форма").unwrap();

    let document_member = document
        .members
        .iter()
        .find(|member| member.name == "ПриЗаписиПерепроводить")
        .expect("document form extension property");
    assert_eq!(
        document_member.availability.context_status,
        ide::SymbolMemberContextStatus::NotEvaluated
    );
    assert!(document_member.availability.contexts.as_ref().is_some_and(|contexts| {
        contexts.contains(&"thin_client") && contexts.contains(&"server")
    }));
    assert!(document.members.iter().all(|member| member.name != "СкомпоноватьРезультат"));

    assert!(processing
        .members
        .iter()
        .any(|member| member.name == "ПолучитьНавигационнуюСсылкуОбработки"));
    assert!(processing.members.iter().all(|member| member.name != "ПриЗаписиПерепроводить"));

    assert!(report.members.iter().any(|member| member.name == "СкомпоноватьРезультат"));
    assert!(report.members.iter().all(|member| member.name != "ПриЗаписиПерепроводить"));
}

#[test]
fn symbol_info_form_main_attribute_card_has_lowered_type_and_form_container() {
    // Given: a resident db with a managed document form.
    let db = setup_forms();

    // When: resolving the form's main attribute by member name.
    let card = by_name(&db, "Документ.Документ1.Форма.ФормаДокумента.Объект")
        .expect("form attribute resolves");

    // Then: the attribute card is typed and owned by the form, not by the metadata object directly.
    assert_eq!(card.kind, "form attribute");
    assert!(
        card.return_type.as_deref().is_some_and(|ty| ty.contains("Документ1")),
        "return_type was {:?}",
        card.return_type
    );
    let container = card.container.expect("form attribute container");
    assert_eq!(container.kind, "Форма");
    assert_eq!(container.name, "Документ1.ФормаДокумента");
    assert!(card.graph_id.is_none(), "form attribute card must not carry usages id");
}

#[test]
fn symbol_info_form_item_card_shows_inferred_data_path_type() {
    // Given: a resident db with a form item bound to `Объект.Реквизит1`.
    let db = setup_forms();

    // When: resolving the item by form-qualified member name.
    let card = by_name(&db, "Документ.Документ1.Форма.ФормаДокумента.Реквизит1")
        .expect("form item resolves");

    // Then: the item card exposes its control kind and data binding without graph usages.
    assert_eq!(card.kind, "form item");
    assert_eq!(card.return_type.as_deref(), Some("ПолеФормы:Объект.Реквизит1 -> Строка"));
    assert!(
        card.signature.as_deref().is_some_and(|s| s.contains("Объект.Реквизит1")),
        "signature should show data_path, got {:?}",
        card.signature
    );
    assert!(card.graph_id.is_none(), "form item card must not carry usages id");
}

#[test]
fn symbol_info_non_exported_form_handler_resolves_with_file_graph_id() {
    // Given: a non-export form event handler in a real form module.
    let db = setup_forms();

    // When: resolving the handler through the form member grammar.
    let card = by_name(&db, "Документ.Документ1.Форма.ФормаДокумента.ПриЗаписиНаСервере")
        .expect("form handler resolves without export gate");

    // Then: the normal method card is returned and the graph id uses the file fallback form.
    assert_eq!(card.kind, "procedure");
    assert!(
        card.signature.as_deref().is_some_and(|s| s.starts_with("Процедура ПриЗаписиНаСервере(")),
        "signature was {:?}",
        card.signature
    );
    assert!(
        card.graph_id
            .as_deref()
            .is_some_and(|id| id.starts_with("method/file/Documents/Документ1/Forms/ФормаДокумента/Ext/Form/Module.bsl::ПриЗаписиНаСервере")),
        "graph_id was {:?}",
        card.graph_id
    );
    let container = card.container.expect("form handler container");
    assert_eq!(container.kind, "Форма");
    assert_eq!(container.name, "Документ1.ФормаДокумента");
}

#[test]
fn form_handler_graph_id_is_relative_to_workspace_root_not_config() {
    // The graph builder encodes a form handler's fallback id relative to the WORKSPACE root
    // (the resident's source_dir), which in a real install differs from the config root
    // (`<workspace>/src/cf`). Regression guard: the id must track the workspace root passed in.
    let db = setup_forms();
    let designer = designer_fixture_path();
    let symbol = "Документ.Документ1.Форма.ФормаДокумента.ПриЗаписиНаСервере";

    // Workspace root at the fixture's PARENT → the fixture dir becomes part of the rel.
    let card = symbol_info(
        &db,
        &SymbolInfoRequest {
            symbol: Some(symbol.to_string()),
            position: None,
            locale: ide::Locale::default(),
            sections: SymbolInfoSections::all(),
            workspace_root: designer.parent().map(ide::StripRoot::resolve),
        },
    )
    .expect("handler resolves");
    let base = designer.file_name().unwrap().to_string_lossy();
    assert!(
        card.graph_id.as_deref().is_some_and(|id| id.starts_with(&format!(
            "method/file/{base}/Documents/Документ1/Forms/ФормаДокумента/Ext/Form/Module.bsl::ПриЗаписиНаСервере"
        ))),
        "graph id must be relative to the workspace root: {:?}",
        card.graph_id
    );

    // No workspace root → an absolute path has no resolvable rel → no graph id (usages off),
    // but the card itself still resolves.
    let card_no_root = symbol_info(
        &db,
        &SymbolInfoRequest {
            symbol: Some(symbol.to_string()),
            position: None,
            locale: ide::Locale::default(),
            sections: SymbolInfoSections::all(),
            workspace_root: None,
        },
    )
    .expect("handler still resolves without a workspace root");
    assert!(card_no_root.graph_id.is_none(), "no root ⇒ no graph id: {:?}", card_no_root.graph_id);
}

#[test]
fn symbol_info_non_event_form_module_method_resolves_without_xml_handler() {
    // Given: a non-export form module procedure that is not listed in Form.xml events.
    let db = setup_forms();

    // When: resolving it through the form member grammar.
    let card = by_name(&db, "Документ.Документ1.Форма.ФормаДокумента.ТестОписанийОповещения")
        .expect("form module method resolves without XML handler entry");

    // Then: it is a normal form method card, not restricted to event handlers.
    assert_eq!(card.kind, "procedure");
    assert!(
        card.signature
            .as_deref()
            .is_some_and(|s| s.starts_with("Процедура ТестОписанийОповещения(")),
        "signature was {:?}",
        card.signature
    );
    assert!(
        card.graph_id
            .as_deref()
            .is_some_and(|id| id.starts_with("method/file/Documents/Документ1/Forms/ФормаДокумента/Ext/Form/Module.bsl::ТестОписанийОповещения")),
        "graph_id was {:?}",
        card.graph_id
    );
}

#[test]
fn symbol_info_whole_common_form_card_resolves_by_common_form_keyword() {
    // Given: a resident db with a real common form fixture.
    let db = setup_forms();

    // When: resolving the common form through the localized common-form keyword.
    let card = by_name(&db, "ОбщаяФорма.ТестоваяФорма").expect("common form resolves");

    // Then: it returns the form structure and does not require an owning metadata object.
    assert_eq!(card.kind, "form");
    let container = card.container.expect("common form container");
    assert_eq!(container.kind, "Форма");
    assert_eq!(container.name, "ТестоваяФорма");
    assert!(
        card.members.iter().any(|m| m.name == "ПриСозданииНаСервере" && m.kind == "Обработчик"),
        "members were {:?}",
        card.members
    );
    assert!(
        card.members.iter().any(|m| m.name == "КомандаОК" && m.kind == "Обработчик"),
        "members were {:?}",
        card.members
    );
}

#[test]
fn ordinary_form_keeps_legacy_full_card_and_exact_local_method() {
    let mut db = setup_forms();
    db.set_file_text(FileId(1), "Процедура ОбычныйHelper()\nКонецПроцедуры\n");

    let full = by_name(&db, "ОбщаяФорма.ТестоваяФорма").expect("ordinary form resolves");
    assert_eq!(full.kind, "form");
    assert!(full.members.iter().all(|member| {
        member.origin == ide::SymbolMemberOrigin::Metadata && member.source_extension.is_none()
    }));

    let exact = by_name(&db, "ОбщаяФорма.ТестоваяФорма.ОбычныйHelper")
        .expect("ordinary local method keeps legacy resolution");
    assert_eq!(exact.kind, "procedure");
    assert!(exact.signature.as_deref().is_some_and(|signature| {
        signature.starts_with("Процедура ОбычныйHelper(")
    }));
}

#[test]
fn symbol_info_bogus_form_or_form_member_is_a_miss() {
    // Given: a resident db with one document form fixture.
    let db = setup_forms();

    // When/Then: unknown form names and unknown form members remain resident misses.
    assert!(by_name(&db, "Документ.Документ1.Форма.Несуществующая").is_none());
    assert!(by_name(&db, "Документ.Документ1.Форма.ФормаДокумента.Несуществующий").is_none());
}

#[test]
fn symbol_info_english_form_marker_resolves_case_insensitively() {
    // Given: a resident db with a document form fixture.
    let db = setup_forms();

    // When: resolving with English keywords and mixed case.
    let card = by_name(&db, "Document.Документ1.fOrM.ФормаДокумента").expect("form resolves");

    // Then: the same whole-form card is returned.
    assert_eq!(card.kind, "form");
}

#[test]
fn plural_mdo_keyword_resolves_the_same_object() {
    let db = setup_catalog();
    // The MdoType keyword accepts the plural form too.
    let card = by_name(&db, "Справочники.Справочник1").expect("plural keyword resolves");
    assert_eq!(card.kind, "metadata object");
}

#[test]
fn named_applied_facets_resolve_without_mixing_surfaces() {
    let db = setup_catalog();
    let cases = [
        ("СправочникОбъект.Справочник1", "СправочникОбъект"),
        ("СправочникСсылка.Справочник1", "СправочникСсылка"),
        ("СправочникМенеджер.Справочник1", "СправочникМенеджер"),
        ("ОбработкаОбъект.ТестоваяОбработка", "ОбработкаОбъект"),
    ];
    for (symbol, expected_facet) in cases {
        let card = by_name(&db, symbol).unwrap_or_else(|| panic!("{symbol} must resolve"));
        assert_eq!(
            card.container.as_ref().map(|container| container.kind.as_str()),
            Some(expected_facet)
        );
    }

    assert!(
        !by_name(&db, "СправочникОбъект.Справочник1").unwrap().members.is_empty(),
        "the object facet keeps its metadata surface"
    );
    assert!(by_name(&db, "СправочникСсылка.Справочник1")
        .unwrap()
        .members
        .iter()
        .all(|member| member.origin != ide::SymbolMemberOrigin::Module));
    assert!(by_name(&db, "СправочникМенеджер.Справочник1")
        .unwrap()
        .members
        .iter()
        .all(|member| member.name != "ТолькоОбъект"));
    assert!(by_name(&db, "НеизвестнаяГрань.Справочник1").is_none());
    assert!(by_name(&db, "СправочникОбъект.НетТакого").is_none());
}

#[test]
fn applied_facet_position_uses_only_visible_extension_roots() {
    let db = setup_applied_visibility();
    let designer = by_name(&db, "СправочникОбъект.Товары").unwrap();
    assert!(designer.members.iter().any(|member| {
        member.name == "ТолькоБ" && member.source_extension.as_deref() == Some("РасширениеБ")
    }));

    let from_extension_a = by_name_at(&db, "СправочникОбъект.Товары", FileId(4)).unwrap();
    assert!(from_extension_a.members.iter().any(|member| {
        member.name == "ТолькоА" && member.source_extension.as_deref() == Some("РасширениеА")
    }));
    assert!(from_extension_a
        .members
        .iter()
        .all(|member| { member.source_extension.as_deref() != Some("РасширениеБ") }));
}

#[test]
fn applied_facet_exact_lookup_uses_the_same_member_collection() {
    let db = setup_catalog();
    let full = by_name(&db, "СправочникОбъект.Справочник1").unwrap();
    let exact = by_name(&db, "СправочникОбъект.Справочник1.Реквизит1").unwrap();

    assert_eq!(exact.kind, "member candidates");
    assert_eq!(exact.members.len(), 1);
    assert_eq!(exact.members[0].name, "Реквизит1");
    assert!(full.members.iter().any(|member| member == &exact.members[0]));
    assert!(by_name(&db, "СправочникОбъект.Справочник1.НетТакого").is_none());
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn object_facets_merge_effective_metadata_module_exports_and_platform_surface() {
    let db = setup_catalog();
    let catalog = by_name(&db, "СправочникОбъект.Справочник1").unwrap();

    assert!(catalog.members.iter().any(|member| {
        member.name == "Реквизит1" && member.origin == ide::SymbolMemberOrigin::Metadata
    }));
    assert!(catalog.members.iter().any(|member| {
        member.name == "ЭкспортныйМетод" && member.origin == ide::SymbolMemberOrigin::Module
    }));
    assert!(catalog.members.iter().any(|member| {
        member.name == "ЭкспортнаяПеременная"
            && member.member_kind == "property"
            && member.origin == ide::SymbolMemberOrigin::Module
    }));
    assert!(catalog.members.iter().any(|member| {
        member.kind == "СтандартныйРеквизит" && member.origin == ide::SymbolMemberOrigin::Platform
    }));
    assert!(catalog.members.iter().any(|member| {
        member.member_kind == "method" && member.origin == ide::SymbolMemberOrigin::Platform
    }));
    let exact = by_name(&db, "СправочникОбъект.Справочник1.ЭкспортныйМетод").unwrap();
    assert!(exact.members.iter().any(|member| {
        member.name == "ЭкспортныйМетод" && member.origin == ide::SymbolMemberOrigin::Module
    }));

    let processing = by_name(&db, "ОбработкаОбъект.ТестоваяОбработка").unwrap();
    assert!(!processing.members.is_empty(), "a module-less processor keeps platform members");
    assert!(processing
        .members
        .iter()
        .all(|member| member.origin != ide::SymbolMemberOrigin::Module));
}

#[test]
fn an_unread_object_module_does_not_hide_metadata_or_platform_members() {
    let mut db = setup_catalog();
    db.set_file_unreadable(FileId(3));

    let card = by_name(&db, "СправочникОбъект.Справочник1").unwrap();
    assert!(card.members.iter().any(|member| member.origin == ide::SymbolMemberOrigin::Metadata));
    assert!(card.members.iter().any(|member| member.origin == ide::SymbolMemberOrigin::Platform));
    assert!(card.members.iter().all(|member| member.origin != ide::SymbolMemberOrigin::Module));
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn reference_and_manager_facets_keep_their_own_surfaces() {
    let db = setup_catalog();

    let reference = by_name(&db, "СправочникСсылка.Справочник1").unwrap();
    assert!(reference.members.iter().any(|member| {
        member.name == "Реквизит1" && member.origin == ide::SymbolMemberOrigin::Metadata
    }));
    assert!(reference
        .members
        .iter()
        .any(|member| member.origin == ide::SymbolMemberOrigin::Platform));
    assert!(reference
        .members
        .iter()
        .all(|member| member.origin != ide::SymbolMemberOrigin::Module));

    let manager = by_name(&db, "СправочникМенеджер.Справочник1").unwrap();
    assert!(manager.members.iter().any(|member| {
        member.name == "ЭкспортнаяПеременнаяМенеджера"
            && member.member_kind == "property"
            && member.origin == ide::SymbolMemberOrigin::Module
    }));
    assert!(manager.members.iter().any(|member| {
        member.name == "ЭкспортныйМетодМенеджера"
            && member.origin == ide::SymbolMemberOrigin::Module
    }));
    assert!(manager.members.iter().any(|member| {
        member.member_kind == "method" && member.origin == ide::SymbolMemberOrigin::Platform
    }));
    assert!(manager.members.iter().all(|member| member.name != "ТолькоОбъект"));
    assert!(manager.members.iter().all(|member| member.name != "Реквизит1"));

    let exact =
        by_name(&db, "СправочникМенеджер.Справочник1.ЭкспортнаяПеременнаяМенеджера").unwrap();
    assert_eq!(exact.members.len(), 1);
    assert_eq!(exact.members[0].origin, ide::SymbolMemberOrigin::Module);

    let manager_type =
        hir::root_object_manager_type(&db, bsl_metadata::MdoType::Catalog, "Справочник1").unwrap();
    assert!(hir::Type::from_id(&db, FileId(4), manager_type)
        .has_field(&hir::Name::new("ЭкспортнаяПеременнаяМенеджера")));
}

#[test]
fn qualified_symbol_shape_rejects_empty_or_non_identifier_segments() {
    assert!(is_well_formed_symbol("СправочникОбъект.Справочник1"));
    for malformed in ["", ".Справочник1", "Справочник1.", "A..B", "A.B-C", "A.1B"]
    {
        assert!(!is_well_formed_symbol(malformed), "{malformed:?} must be malformed");
    }
}

#[test]
fn positional_local_or_parameter_resolves() {
    let (db, file_id) = setup_bsl(COMMON_MODULE);
    // `Б = Парам;` on line 1 (0-based) of /test.bsl; `Парам` starts at column 8.
    let card = symbol_info(
        &db,
        &SymbolInfoRequest {
            symbol: None,
            position: Some(SymbolPosition { file_id, line: 1, column: 8 }),
            locale: ide::Locale::default(),
            sections: SymbolInfoSections::all(),
            workspace_root: None,
        },
    )
    .expect("positional resolution");
    assert_eq!(card.symbol, "Парам");
    assert!(matches!(card.kind, "parameter" | "local variable"), "unexpected kind {:?}", card.kind);

    // The position is how the caller got here, so the contract location must carry it. A
    // location without ranges means "the whole file", which would be strictly less than the
    // legacy `line` beside it.
    let def = card.definition.expect("definition site");
    assert_eq!(def.line, 2, "the legacy 1-based line stays as it was");
    let name = def.name_range.expect("a local names its own position");
    assert_eq!(name.start_line, 1, "0-based line of the same declaration");
    assert_eq!(name.start_character, 8);
    assert_eq!(name.end_character, 8 + "Парам".chars().count() as u32);
}

#[test]
fn positional_out_of_range_column_is_a_miss() {
    let (db, file_id) = setup_bsl(COMMON_MODULE);
    // Column 999 is past the end of `    Б = Парам;`; the resolver must not bleed into the next
    // line's token and answer for a symbol the caller never pointed at.
    let card = symbol_info(
        &db,
        &SymbolInfoRequest {
            symbol: None,
            position: Some(SymbolPosition { file_id, line: 1, column: 999 }),
            locale: ide::Locale::default(),
            sections: SymbolInfoSections::all(),
            workspace_root: None,
        },
    );
    assert!(card.is_none(), "out-of-range column must be a miss, got {card:?}");
}

#[test]
fn exported_object_module_method_resolves_and_private_does_not() {
    let (db, _) = setup_bsl(OBJECT_MODULE);
    // A qualified `<Object>.<Method>` names an externally addressable method: only the exported
    // one resolves; the private object-module method is not a valid qualified symbol.
    assert!(
        by_name(&db, "Документ.Документ1.ПубличныйМетод").is_some(),
        "exported object-module method resolves"
    );
    assert!(
        by_name(&db, "Документ.Документ1.ПриватныйМетод").is_none(),
        "non-exported object-module method is not addressable by qualified name"
    );
}

#[test]
fn register_card_lists_dimensions() {
    let db = setup_register();
    let card = by_name(&db, "РегистрСведений.РегистрСведений1").expect("register resolves");
    assert_eq!(card.kind, "metadata object");
    let container = card.container.expect("register container");
    assert_eq!(container.kind, "РегистрСведений");
    assert!(
        card.members.iter().any(|m| m.name == "Справочник1" && m.kind == "Измерение"),
        "dimensions were {:?}",
        card.members
    );
}

#[test]
fn register_dimension_card_has_type_and_ownership() {
    let db = setup_register();
    let card = by_name(&db, "РегистрСведений.РегистрСведений1.Справочник1")
        .expect("register dimension resolves");
    assert_eq!(card.kind, "attribute");
    let container = card.container.expect("dimension ownership");
    assert!(
        container.kind.starts_with("РегистрСведений"),
        "container kind was {:?}",
        container.kind
    );
    assert_eq!(container.name, "РегистрСведений1");
}

/// A module whose first declaration line ends in a run of spaces, and a second one that does
/// not. The pair is the point: the second line reads the same under any implementation, so a
/// difference between the two answers can only come from how the trailing run is handled.
const TRAILING_WHITESPACE_MODULE: &str = "
//- /CommonModules/МойМодуль/Ext/Module.bsl
Функция СХвостом(А) Экспорт   
    Возврат А;
КонецФункции

Функция БезХвоста(А) Экспорт
    Возврат А;
КонецФункции
";

/// The declaration line a card publishes carries no trailing whitespace, whatever reads it
/// out of the file.
#[test]
fn a_declaration_line_is_published_without_its_trailing_whitespace() {
    let (db, _) = setup_bsl(TRAILING_WHITESPACE_MODULE);
    let snippet = |symbol: &str| {
        by_name(&db, symbol)
            .expect("resident resolves the method")
            .definition
            .expect("definition site")
            .snippet
            .expect("declaration line")
    };

    assert_eq!(snippet("МойМодуль.СХвостом"), "Функция СХвостом(А) Экспорт");
    // The control: a declaration with nothing to strip publishes the same bytes either way,
    // so the assertion above is about the trailing run and not about the reader at large.
    assert_eq!(snippet("МойМодуль.БезХвоста"), "Функция БезХвоста(А) Экспорт");
}
