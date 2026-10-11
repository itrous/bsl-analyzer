use hir::{DefDatabase, HirDatabase, InferenceDiagnostic, MetadataReferenceKind, ModuleId};
use ide::{Analysis, CompletionItem, CompletionItemKind};
use ide_db::base_db::{SourceDatabase, SourceRoot, BSL_SOURCE_ROOT, METADATA_SOURCE_ROOT};
use ide_db::metadata::{
    HTTPServiceEntry, IntegrationServiceEntry, MetadataListingData, RoleEntry, ScheduledJobEntry,
    WebServiceEntry,
};
use ide_db::RootDatabaseImpl;
use test_fixture::Fixture;
use vfs::{FileId, FileSet, VfsPath};

const ROLE_XML_FILE_ID: FileId = FileId(1000);
const ROLE_RIGHTS_XML_FILE_ID: FileId = FileId(1002);
const SCHEDULED_JOB_XML_FILE_ID: FileId = FileId(1001);
const HTTP_SERVICE_XML_FILE_ID: FileId = FileId(1003);
const WEB_SERVICE_XML_FILE_ID: FileId = FileId(1005);
const INTEGRATION_SERVICE_XML_FILE_ID: FileId = FileId(1007);

fn designer_fixture_path() -> std::path::PathBuf {
    std::path::PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../bsl-metadata/fixtures/designer"
    ))
}

fn role_xml_text() -> &'static str {
    include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../bsl-metadata/fixtures/designer/Roles/ПолныеПрава.xml"
    ))
}

fn setup(fixture_text: &str) -> (Analysis, FileId, u32) {
    setup_with_role_xml(fixture_text, role_xml_text())
}

fn setup_with_role_xml(fixture_text: &str, role_xml: &str) -> (Analysis, FileId, u32) {
    let (fixture_text, test_path, cursor_offset) = extract_cursor(fixture_text);
    let fixture = Fixture::parse(&fixture_text);
    let mut db = RootDatabaseImpl::new();
    let mut file_set = FileSet::default();
    for (file_id, file) in &fixture.files {
        file_set.insert(*file_id, file.path.clone());
    }

    let mut metadata_file_set = FileSet::default();
    metadata_file_set.insert(
        ROLE_XML_FILE_ID,
        VfsPath::from(designer_fixture_path().join("Roles/ПолныеПрава.xml")),
    );

    db.set_source_root(BSL_SOURCE_ROOT, SourceRoot::new_local(file_set));
    db.set_source_root(METADATA_SOURCE_ROOT, SourceRoot::new_local(metadata_file_set));
    for (file_id, file) in &fixture.files {
        db.set_file_source_root(*file_id, BSL_SOURCE_ROOT);
        db.set_file_text(*file_id, &file.content);
    }
    db.set_file_source_root(ROLE_XML_FILE_ID, METADATA_SOURCE_ROOT);
    db.set_file_text(ROLE_XML_FILE_ID, role_xml);
    db.set_all_config_paths(vec![(None, designer_fixture_path())]);

    let test_file = fixture
        .files
        .iter()
        .find(|(_, f)| f.path.as_path().to_string_lossy().ends_with(&test_path))
        .map(|(id, _)| *id)
        .expect("cursor-bearing file not found");

    (Analysis::from_database(db), test_file, cursor_offset)
}

fn setup_with_role_substrate(fixture_text: &str) -> (Analysis, FileId, u32) {
    let (fixture_text, test_path, cursor_offset) = extract_cursor(fixture_text);
    let fixture = Fixture::parse(&fixture_text);
    let mut db = RootDatabaseImpl::new();
    let mut file_set = FileSet::default();
    for (file_id, file) in &fixture.files {
        file_set.insert(*file_id, file.path.clone());
    }

    let root = std::env::temp_dir().join(format!(
        "bsl_role_navigation_{}_{}",
        std::process::id(),
        line!()
    ));
    std::fs::create_dir_all(root.join("Roles/ПолныеПрава/Ext")).unwrap();
    std::fs::write(root.join("Configuration.xml"), "<Configuration/>").unwrap();

    let mut metadata_file_set = FileSet::default();
    metadata_file_set.insert(ROLE_XML_FILE_ID, VfsPath::from(root.join("Roles/ПолныеПрава.xml")));
    metadata_file_set.insert(
        ROLE_RIGHTS_XML_FILE_ID,
        VfsPath::from(root.join("Roles/ПолныеПрава/Ext/Rights.xml")),
    );

    db.set_source_root(BSL_SOURCE_ROOT, SourceRoot::new_local(file_set));
    db.set_source_root(METADATA_SOURCE_ROOT, SourceRoot::new_local(metadata_file_set));
    for (file_id, file) in &fixture.files {
        db.set_file_source_root(*file_id, BSL_SOURCE_ROOT);
        db.set_file_text(*file_id, &file.content);
    }
    db.set_file_source_root(ROLE_XML_FILE_ID, METADATA_SOURCE_ROOT);
    db.set_file_source_root(ROLE_RIGHTS_XML_FILE_ID, METADATA_SOURCE_ROOT);
    db.set_file_text(
        ROLE_XML_FILE_ID,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <Role uuid="00000000-0000-0000-0000-000000000092">
        <Properties>
            <Name>ПолныеПрава</Name>
            <Synonym/>
            <Comment/>
        </Properties>
    </Role>
</MetaDataObject>"#,
    );
    db.set_file_text(
        ROLE_RIGHTS_XML_FILE_ID,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<Rights xmlns="http://v8.1c.ru/8.2/roles" version="2.10">
    <setForNewObjects>true</setForNewObjects>
    <object>
        <name>Catalog.Контрагенты</name>
        <right>
            <name>Read</name>
            <value>true</value>
        </right>
    </object>
</Rights>"#,
    );

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
                name: "ПолныеПрава".to_string(),
                main: ROLE_XML_FILE_ID,
                rights: Some(ROLE_RIGHTS_XML_FILE_ID),
            }],
            http_services: Vec::new(),
            web_services: Vec::new(),
            integration_services: Vec::new(),
            subsystems: Vec::new(),
        },
    );

    let test_file = fixture
        .files
        .iter()
        .find(|(_, f)| f.path.as_path().to_string_lossy().ends_with(&test_path))
        .map(|(id, _)| *id)
        .expect("cursor-bearing file not found");

    (Analysis::from_database(db), test_file, cursor_offset)
}

fn scheduled_job_xml_text() -> &'static str {
    include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../bsl-metadata/fixtures/designer/ScheduledJobs/РегламентноеЗадание1.xml"
    ))
}

fn http_service_xml_text() -> &'static str {
    include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../bsl-metadata/fixtures/designer/HTTPServices/HTTPСервис1.xml"
    ))
}

fn web_service_xml_text() -> &'static str {
    include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../bsl-metadata/fixtures/designer/WebServices/WebСервис1.xml"
    ))
}

fn integration_service_xml_text() -> &'static str {
    include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../bsl-metadata/fixtures/designer/IntegrationServices/ОбменСообщениями.xml"
    ))
}

/// Bootstrapped-substrate setup for an HTTP service module (RED Wave 2d): no full
/// `Configuration` is loaded, so the listing's `http_services` entry is the only signal.
fn setup_with_http_service_substrate(fixture_text: &str) -> (Analysis, FileId, u32) {
    let (fixture_text, test_path, cursor_offset) = extract_cursor(fixture_text);
    let fixture = Fixture::parse(&fixture_text);
    let mut db = RootDatabaseImpl::new();
    let mut file_set = FileSet::default();
    for (file_id, file) in &fixture.files {
        file_set.insert(*file_id, file.path.clone());
    }

    let root = std::env::temp_dir().join(format!(
        "bsl_http_service_navigation_{}_{}",
        std::process::id(),
        line!()
    ));
    std::fs::create_dir_all(root.join("HTTPServices/HTTPСервис1/Ext")).unwrap();
    std::fs::write(root.join("Configuration.xml"), "<Configuration/>").unwrap();

    let mut metadata_file_set = FileSet::default();
    metadata_file_set
        .insert(HTTP_SERVICE_XML_FILE_ID, VfsPath::from(root.join("HTTPServices/HTTPСервис1.xml")));

    db.set_source_root(BSL_SOURCE_ROOT, SourceRoot::new_local(file_set));
    db.set_source_root(METADATA_SOURCE_ROOT, SourceRoot::new_local(metadata_file_set));
    for (file_id, file) in &fixture.files {
        db.set_file_source_root(*file_id, BSL_SOURCE_ROOT);
        db.set_file_text(*file_id, &file.content);
    }
    db.set_file_source_root(HTTP_SERVICE_XML_FILE_ID, METADATA_SOURCE_ROOT);
    db.set_file_text(HTTP_SERVICE_XML_FILE_ID, http_service_xml_text());

    let test_file = fixture
        .files
        .iter()
        .find(|(_, f)| f.path.as_path().to_string_lossy().ends_with(&test_path))
        .map(|(id, _)| *id)
        .expect("cursor-bearing file not found");

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
            http_services: vec![HTTPServiceEntry {
                name: "HTTPСервис1".to_string(),
                main: HTTP_SERVICE_XML_FILE_ID,
                module_file: Some(test_file),
            }],
            web_services: Vec::new(),
            integration_services: Vec::new(),
            subsystems: Vec::new(),
        },
    );

    (Analysis::from_database(db), test_file, cursor_offset)
}

/// Bootstrapped-substrate setup for a Web service module (RED Wave 2d).
fn setup_with_web_service_substrate(fixture_text: &str) -> (Analysis, FileId, u32) {
    let (fixture_text, test_path, cursor_offset) = extract_cursor(fixture_text);
    let fixture = Fixture::parse(&fixture_text);
    let mut db = RootDatabaseImpl::new();
    let mut file_set = FileSet::default();
    for (file_id, file) in &fixture.files {
        file_set.insert(*file_id, file.path.clone());
    }

    let root = std::env::temp_dir().join(format!(
        "bsl_web_service_navigation_{}_{}",
        std::process::id(),
        line!()
    ));
    std::fs::create_dir_all(root.join("WebServices/WebСервис1/Ext")).unwrap();
    std::fs::write(root.join("Configuration.xml"), "<Configuration/>").unwrap();

    let mut metadata_file_set = FileSet::default();
    metadata_file_set
        .insert(WEB_SERVICE_XML_FILE_ID, VfsPath::from(root.join("WebServices/WebСервис1.xml")));

    db.set_source_root(BSL_SOURCE_ROOT, SourceRoot::new_local(file_set));
    db.set_source_root(METADATA_SOURCE_ROOT, SourceRoot::new_local(metadata_file_set));
    for (file_id, file) in &fixture.files {
        db.set_file_source_root(*file_id, BSL_SOURCE_ROOT);
        db.set_file_text(*file_id, &file.content);
    }
    db.set_file_source_root(WEB_SERVICE_XML_FILE_ID, METADATA_SOURCE_ROOT);
    db.set_file_text(WEB_SERVICE_XML_FILE_ID, web_service_xml_text());

    let test_file = fixture
        .files
        .iter()
        .find(|(_, f)| f.path.as_path().to_string_lossy().ends_with(&test_path))
        .map(|(id, _)| *id)
        .expect("cursor-bearing file not found");

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
            web_services: vec![WebServiceEntry {
                name: "WebСервис1".to_string(),
                main: WEB_SERVICE_XML_FILE_ID,
                module_file: Some(test_file),
            }],
            integration_services: Vec::new(),
            subsystems: Vec::new(),
        },
    );

    (Analysis::from_database(db), test_file, cursor_offset)
}

/// Bootstrapped-substrate setup for an integration service module (RED Wave 2d). Has no
/// `Метаданные.СервисыИнтеграции` plural surface; the substrate exists only to track the
/// module file so handler diagnostics can resolve the channel handler.
fn setup_with_integration_service_substrate(fixture_text: &str) -> (Analysis, FileId, u32) {
    let (fixture_text, test_path, cursor_offset) = extract_cursor(fixture_text);
    let fixture = Fixture::parse(&fixture_text);
    let mut db = RootDatabaseImpl::new();
    let mut file_set = FileSet::default();
    for (file_id, file) in &fixture.files {
        file_set.insert(*file_id, file.path.clone());
    }

    let root = std::env::temp_dir().join(format!(
        "bsl_integration_service_navigation_{}_{}",
        std::process::id(),
        line!()
    ));
    std::fs::create_dir_all(root.join("IntegrationServices/ОбменСообщениями/Ext")).unwrap();
    std::fs::write(root.join("Configuration.xml"), "<Configuration/>").unwrap();

    let mut metadata_file_set = FileSet::default();
    metadata_file_set.insert(
        INTEGRATION_SERVICE_XML_FILE_ID,
        VfsPath::from(root.join("IntegrationServices/ОбменСообщениями.xml")),
    );

    db.set_source_root(BSL_SOURCE_ROOT, SourceRoot::new_local(file_set));
    db.set_source_root(METADATA_SOURCE_ROOT, SourceRoot::new_local(metadata_file_set));
    for (file_id, file) in &fixture.files {
        db.set_file_source_root(*file_id, BSL_SOURCE_ROOT);
        db.set_file_text(*file_id, &file.content);
    }
    db.set_file_source_root(INTEGRATION_SERVICE_XML_FILE_ID, METADATA_SOURCE_ROOT);
    db.set_file_text(INTEGRATION_SERVICE_XML_FILE_ID, integration_service_xml_text());

    let test_file = fixture
        .files
        .iter()
        .find(|(_, f)| f.path.as_path().to_string_lossy().ends_with(&test_path))
        .map(|(id, _)| *id)
        .expect("cursor-bearing file not found");

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
            integration_services: vec![IntegrationServiceEntry {
                name: "ОбменСообщениями".to_string(),
                main: INTEGRATION_SERVICE_XML_FILE_ID,
                module_file: Some(test_file),
            }],
            subsystems: Vec::new(),
        },
    );

    (Analysis::from_database(db), test_file, cursor_offset)
}

fn setup_with_scheduled_job_xml(
    fixture_text: &str,
    scheduled_job_xml: &str,
) -> (Analysis, FileId, u32) {
    let (fixture_text, test_path, cursor_offset) = extract_cursor(fixture_text);
    let fixture = Fixture::parse(&fixture_text);
    let mut db = RootDatabaseImpl::new();
    let mut file_set = FileSet::default();
    for (file_id, file) in &fixture.files {
        file_set.insert(*file_id, file.path.clone());
    }

    let root = std::env::temp_dir().join(format!(
        "bsl_scheduled_job_navigation_{}_{}",
        std::process::id(),
        line!()
    ));
    let job_path = root.join("ScheduledJobs/РегламентноеЗадание1.xml");
    std::fs::create_dir_all(job_path.parent().unwrap()).unwrap();
    std::fs::write(root.join("Configuration.xml"), "<Configuration/>").unwrap();

    let mut metadata_file_set = FileSet::default();
    metadata_file_set.insert(SCHEDULED_JOB_XML_FILE_ID, VfsPath::from(job_path.clone()));

    db.set_source_root(BSL_SOURCE_ROOT, SourceRoot::new_local(file_set));
    db.set_source_root(METADATA_SOURCE_ROOT, SourceRoot::new_local(metadata_file_set));
    for (file_id, file) in &fixture.files {
        db.set_file_source_root(*file_id, BSL_SOURCE_ROOT);
        db.set_file_text(*file_id, &file.content);
    }
    db.set_file_source_root(SCHEDULED_JOB_XML_FILE_ID, METADATA_SOURCE_ROOT);
    db.set_file_text(SCHEDULED_JOB_XML_FILE_ID, scheduled_job_xml);
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
                main: SCHEDULED_JOB_XML_FILE_ID,
            }],
            roles: Vec::new(),
            http_services: Vec::new(),
            web_services: Vec::new(),
            integration_services: Vec::new(),
            subsystems: Vec::new(),
        },
    );

    let test_file = fixture
        .files
        .iter()
        .find(|(_, f)| f.path.as_path().to_string_lossy().ends_with(&test_path))
        .map(|(id, _)| *id)
        .expect("cursor-bearing file not found");

    (Analysis::from_database(db), test_file, cursor_offset)
}

fn extract_cursor(fixture_text: &str) -> (String, String, u32) {
    let abs_idx = fixture_text.find("$0").expect("fixture must contain $0 cursor marker");
    let prefix = &fixture_text[..abs_idx];
    let last_header_start = prefix.rfind("//- ").expect("cursor must be inside a //- file");
    let header_end =
        prefix[last_header_start..].find('\n').expect("//- header must end with newline")
            + last_header_start;
    let path_line = &prefix[last_header_start + 4..header_end];
    let file_offset_in_prefix = header_end + 1;
    let cursor_in_file =
        u32::try_from(abs_idx - file_offset_in_prefix).expect("fixture cursor offset must fit u32");
    let cleaned = fixture_text.replacen("$0", "", 1);
    (cleaned, path_line.to_string(), cursor_in_file)
}

fn complete(fixture: &str) -> Vec<CompletionItem> {
    let (analysis, file_id, offset) = setup(fixture);
    analysis.completions(file_id, offset, None, ide::Locale::Ru)
}

fn labels(items: &[CompletionItem]) -> Vec<&str> {
    items.iter().map(|item| item.label.as_str()).collect()
}

fn item<'a>(items: &'a [CompletionItem], label: &str) -> Option<&'a CompletionItem> {
    items.iter().find(|item| item.label == label)
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn cold_plurals_complete_under_metadata() {
    let items = complete(
        r#"//- /test.bsl
Процедура Тест()
    Метаданные.$0
КонецПроцедуры
"#,
    );

    for label in [
        "Роли",
        "ПодпискиНаСобытия",
        "РегламентныеЗадания",
        "HTTPСервисы",
        "WebСервисы",
        "Подсистемы",
    ] {
        let found = item(&items, label).unwrap_or_else(|| {
            panic!("{label} must complete under Метаданные.; got {:?}", labels(&items))
        });
        assert_eq!(found.kind, CompletionItemKind::MdoType, "{label} must be a metadata kind");
    }
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn cold_collection_completes_existing_objects() {
    let items = complete(
        r#"//- /test.bsl
Процедура Тест()
    Метаданные.Роли.$0
КонецПроцедуры
"#,
    );

    let role = item(&items, "ПолныеПрава").unwrap_or_else(|| {
        panic!("ПолныеПрава must complete under Роли; got {:?}", labels(&items))
    });
    assert_eq!(role.kind, CompletionItemKind::MdoObject);
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn manager_metadata_collection_still_completes_existing_objects() {
    let items = complete(
        r#"//- /test.bsl
Процедура Тест()
    Метаданные.Справочники.$0
КонецПроцедуры
"#,
    );

    let catalog = item(&items, "Справочник1").unwrap_or_else(|| {
        panic!("manager metadata path must still complete catalogs; got {:?}", labels(&items))
    });
    assert_eq!(catalog.kind, CompletionItemKind::MdoObject);
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn scheduled_job_cold_collection_completes_existing_objects_from_bootstrap() {
    let (analysis, file_id, offset) = setup_with_scheduled_job_xml(
        r#"//- /test.bsl
Процедура Тест()
    Значение = Метаданные.РегламентныеЗадания.$0
КонецПроцедуры
"#,
        scheduled_job_xml_text(),
    );

    let items = analysis.completions(file_id, offset, None, ide::Locale::Ru);
    let job = item(&items, "РегламентноеЗадание1").unwrap_or_else(|| {
        panic!(
            "РегламентноеЗадание1 must complete under РегламентныеЗадания; got {:?}",
            labels(&items)
        )
    });
    assert_eq!(job.kind, CompletionItemKind::MdoObject);
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn scheduled_job_cold_metadata_ref_hover_and_goto_target_xml_in_metadata_source_root() {
    let (analysis, file_id, offset) = setup_with_scheduled_job_xml(
        r#"//- /test.bsl
Процедура Тест()
    Значение = Метаданные.РегламентныеЗадания.Регламентное$0Задание1;
КонецПроцедуры
"#,
        scheduled_job_xml_text(),
    );

    let hover =
        analysis.hover(file_id, offset, ide::Locale::Ru).expect("scheduled job hover must resolve");
    assert!(
        hover.markup.contains("РегламентноеЗадание1"),
        "hover must name the concrete scheduled job: {}",
        hover.markup
    );

    let nav = analysis.goto_definition(file_id, offset).expect("scheduled job goto must resolve");
    assert_eq!(
        nav.file_id, SCHEDULED_JOB_XML_FILE_ID,
        "scheduled job goto must target XML metadata file"
    );
    assert!(!nav.range.is_empty(), "scheduled job goto range must point at real XML name text");
}

#[test]
fn metadata_object_member_is_not_typed_as_a_manager() {
    // `Метаданные.Справочники.Справочник1` is a metadata-description object (ОбъектМетаданных),
    // not a `СправочникМенеджер`, so it must NOT surface manager methods like СоздатьЭлемент.
    let items = complete(
        r#"//- /test.bsl
Процедура Тест()
    Метаданные.Справочники.Справочник1.$0
КонецПроцедуры
"#,
    );

    assert!(
        item(&items, "СоздатьЭлемент").is_none(),
        "metadata-description object must not expose manager methods; got {:?}",
        labels(&items)
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn cold_metadata_ref_hover_and_goto_target_xml_in_metadata_source_root() {
    let (analysis, file_id, offset) = setup(
        r#"//- /test.bsl
Процедура Тест()
    Значение = Метаданные.Роли.Полные$0Права;
КонецПроцедуры
"#,
    );

    let hover = analysis.hover(file_id, offset, ide::Locale::Ru).expect("role hover must resolve");
    assert!(hover.markup.contains("Роль"), "hover must name the cold kind: {}", hover.markup);
    assert!(
        hover.markup.contains("ПолныеПрава"),
        "hover must name the concrete role: {}",
        hover.markup
    );
    assert!(
        !hover.markup.contains("Менеджер"),
        "cold metadata ref must not hover as a manager: {}",
        hover.markup
    );

    let nav = analysis.goto_definition(file_id, offset).expect("role goto must resolve");
    assert_eq!(nav.file_id, ROLE_XML_FILE_ID, "role goto must target XML metadata file");
    assert!(!nav.range.is_empty(), "role goto range must point at real XML name text");
    let xml_text = analysis.database().file_text(nav.file_id);
    let start = u32::from(nav.range.start()) as usize;
    let end = u32::from(nav.range.end()) as usize;
    assert_eq!(&xml_text[start..end], "ПолныеПрава");
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn cold_metadata_role_substrate_completion_hover_and_goto() {
    let (analysis, file_id, offset) = setup_with_role_substrate(
        r#"//- /test.bsl
Процедура Тест()
    Значение = Метаданные.Роли.Полные$0Права;
КонецПроцедуры
"#,
    );

    let items = analysis.completions(file_id, offset, None, ide::Locale::Ru);
    let role = item(&items, "ПолныеПрава").unwrap_or_else(|| {
        panic!("role substrate must still complete ПолныеПрава; got {:?}", labels(&items))
    });
    assert_eq!(role.kind, CompletionItemKind::MdoObject);

    let hover = analysis.hover(file_id, offset, ide::Locale::Ru).expect("role hover must resolve");
    assert!(hover.markup.contains("Роль"), "hover must name the cold kind: {}", hover.markup);
    assert!(
        hover.markup.contains("ПолныеПрава"),
        "hover must name the concrete role: {}",
        hover.markup
    );

    let nav = analysis.goto_definition(file_id, offset).expect("role goto must resolve");
    assert_eq!(nav.file_id, ROLE_XML_FILE_ID, "role goto must target XML metadata file");
    assert!(!nav.range.is_empty(), "role goto range must point at real XML name text");
}

#[test]
fn cold_metadata_goto_without_xml_name_range_returns_none() {
    let (analysis, file_id, offset) = setup_with_role_xml(
        r#"//- /test.bsl
Процедура Тест()
    Значение = Метаданные.Роли.Полные$0Права;
КонецПроцедуры
"#,
        r#"<MetaDataObject><Role><Properties><Name>ДругоеИмя</Name></Properties></Role></MetaDataObject>"#,
    );

    assert!(
        analysis.goto_definition(file_id, offset).is_none(),
        "role goto must not synthesize an empty range when XML lacks the role name"
    );
}

#[test]
fn missing_cold_member_stays_unknown_without_unresolved_field_diagnostic() {
    let (analysis, file_id, _offset) = setup(
        r#"//- /test.bsl
Процедура Тест()
    Значение = Метаданные.Роли.НетТакойРоли$0;
КонецПроцедуры
"#,
    );

    let diagnostics = analysis.database().arg_diagnostics(file_id);
    let unresolved_fields: Vec<_> = diagnostics
        .iter()
        .filter_map(|(_, diagnostic)| match diagnostic {
            InferenceDiagnostic::UnresolvedField { field_name, .. } => Some(field_name.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        unresolved_fields.is_empty(),
        "missing cold metadata members must degrade to Unknown without UnresolvedField; got {unresolved_fields:?}"
    );
}

#[test]
fn local_metadata_name_shadows_cold_plural_completion_without_blocking_receiver_fields() {
    let items = complete(
        r#"//- /test.bsl
Процедура Тест()
    Метаданные = Новый Структура("Поле");
    Метаданные.$0
КонецПроцедуры
"#,
    );

    assert!(
        item(&items, "Поле").is_some(),
        "shadowed Метаданные must fall through to regular receiver completions; got {:?}",
        labels(&items)
    );
    assert!(
        item(&items, "Роли").is_none(),
        "local Метаданные must shadow global metadata completions; got {:?}",
        labels(&items)
    );
}

// ===== Wave 2d: HTTPService / WebService cold substrate navigation =====
//
// These tests exercise the bootstrapped-substrate path: no full Configuration is loaded,
// so `Метаданные.HTTPСервисы.<X>` must resolve through MetadataListingData's http_services
// field. They RED-fail until the substrate wires `HTTPServiceEntry` / `WebServiceEntry`
// into ide_db::metadata and the cold navigation queries consume them.

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn http_service_cold_collection_completes_existing_objects_from_bootstrap() {
    let (analysis, file_id, offset) = setup_with_http_service_substrate(
        r#"//- /test.bsl
Процедура Тест()
    Значение = Метаданные.HTTPСервисы.$0
КонецПроцедуры
"#,
    );

    let items = analysis.completions(file_id, offset, None, ide::Locale::Ru);
    let service = item(&items, "HTTPСервис1").unwrap_or_else(|| {
        panic!(
            "HTTPСервис1 must complete under HTTPСервисы from bootstrapped substrate; got {:?}",
            labels(&items)
        )
    });
    assert_eq!(service.kind, CompletionItemKind::MdoObject);
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn http_service_cold_metadata_ref_hover_and_goto_target_xml_in_metadata_source_root() {
    let (analysis, file_id, offset) = setup_with_http_service_substrate(
        r#"//- /test.bsl
Процедура Тест()
    Значение = Метаданные.HTTPСервисы.HTTP$0Сервис1;
КонецПроцедуры
"#,
    );

    let hover = analysis
        .hover(file_id, offset, ide::Locale::Ru)
        .expect("http service hover must resolve from substrate");
    assert!(
        hover.markup.contains("HTTPСервис1"),
        "hover must name the concrete http service from substrate: {}",
        hover.markup
    );

    let nav = analysis
        .goto_definition(file_id, offset)
        .expect("http service goto must resolve from substrate");
    assert_eq!(
        nav.file_id, HTTP_SERVICE_XML_FILE_ID,
        "http service goto must target XML metadata file from substrate"
    );
    assert!(!nav.range.is_empty(), "http service goto range must point at real XML name text");
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn web_service_cold_collection_completes_existing_objects_from_bootstrap() {
    let (analysis, file_id, offset) = setup_with_web_service_substrate(
        r#"//- /test.bsl
Процедура Тест()
    Значение = Метаданные.WebСервисы.$0
КонецПроцедуры
"#,
    );

    let items = analysis.completions(file_id, offset, None, ide::Locale::Ru);
    let service = item(&items, "WebСервис1").unwrap_or_else(|| {
        panic!(
            "WebСервис1 must complete under WebСервисы from bootstrapped substrate; got {:?}",
            labels(&items)
        )
    });
    assert_eq!(service.kind, CompletionItemKind::MdoObject);
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn web_service_cold_metadata_ref_hover_and_goto_target_xml_in_metadata_source_root() {
    let (analysis, file_id, offset) = setup_with_web_service_substrate(
        r#"//- /test.bsl
Процедура Тест()
    Значение = Метаданные.WebСервисы.Web$0Сервис1;
КонецПроцедуры
"#,
    );

    let hover = analysis
        .hover(file_id, offset, ide::Locale::Ru)
        .expect("web service hover must resolve from substrate");
    assert!(
        hover.markup.contains("WebСервис1"),
        "hover must name the concrete web service from substrate: {}",
        hover.markup
    );

    let nav = analysis
        .goto_definition(file_id, offset)
        .expect("web service goto must resolve from substrate");
    assert_eq!(
        nav.file_id, WEB_SERVICE_XML_FILE_ID,
        "web service goto must target XML metadata file from substrate"
    );
    assert!(!nav.range.is_empty(), "web service goto range must point at real XML name text");
}

#[test]
fn integration_service_plural_stays_absent_from_cold_metadata_surface() {
    // IntegrationService is intentionally NOT a MetadataReferenceKind variant: there is no
    // `Метаданные.СервисыИнтеграции` plural surface in the platform, and Wave 2d adds the
    // substrate only for module-path / handler scope, not for metadata-reference navigation.
    // This test pins that absence against accidental future drift.
    let items = complete(
        r#"//- /test.bsl
Процедура Тест()
    Метаданные.$0
КонецПроцедуры
"#,
    );

    assert!(MetadataReferenceKind::from_plural("СервисыИнтеграции").is_none());
    if let Some(platform_item) = item(&items, "СервисыИнтеграции") {
        assert_ne!(
            platform_item.kind,
            CompletionItemKind::MdoType,
            "IntegrationService may exist as a platform property, but must not be a metadata-reference collection"
        );
    }
    assert!(
        item(&items, "IntegrationServices").is_none(),
        "IntegrationService EN plural must not surface either; got {:?}",
        labels(&items)
    );
}

// ===== Wave 2d: substrate-driven module metadata for service modules =====
//
// When the substrate is bootstrapped (no full Configuration), opening a service module
// file must still produce a ModuleMetadata whose http_service / web_service /
// integration_service slot is populated from the corresponding MetadataListingData entry.
// That is what lets the wrong_*_handler and unused_local_method diagnostics recognize
// service handlers from typed module metadata rather than a whole-config side-path.
// All three tests RED-fail until ide_db::metadata::build_module_metadata consults the
// bootstrapped listing for service modules.

#[test]
fn http_service_module_metadata_populated_from_substrate_listing() {
    let (analysis, file_id, _offset) = setup_with_http_service_substrate(
        r#"//- /HTTPServices/HTTPСервис1/Ext/Module.bsl
Функция Метод$01(Запрос)
    Возврат Запрос;
КонецФункции
"#,
    );

    let db = analysis.database();
    let module = ModuleId::new(file_id);
    let metadata = db.module_metadata(module);
    assert_eq!(
        metadata.module_type,
        bsl_metadata::ModuleType::HTTPServiceModule,
        "substrate module file must classify as HTTPServiceModule"
    );
    let http = metadata
        .http_service
        .as_ref()
        .expect("substrate listing must populate http_service on ModuleMetadata");
    assert_eq!(http.name(), "HTTPСервис1");
}

#[test]
fn web_service_module_metadata_populated_from_substrate_listing() {
    let (analysis, file_id, _offset) = setup_with_web_service_substrate(
        r#"//- /WebServices/WebСервис1/Ext/Module.bsl
Функция Операция$01(СтрокаПараметров)
    Возврат Неопределено;
КонецФункции
"#,
    );

    let db = analysis.database();
    let module = ModuleId::new(file_id);
    let metadata = db.module_metadata(module);
    assert_eq!(
        metadata.module_type,
        bsl_metadata::ModuleType::WebServiceModule,
        "substrate module file must classify as WebServiceModule"
    );
    let web = metadata
        .web_service
        .as_ref()
        .expect("substrate listing must populate web_service on ModuleMetadata");
    assert_eq!(web.name(), "WebСервис1");
}

#[test]
fn integration_service_module_metadata_populated_from_substrate_listing() {
    let (analysis, file_id, _offset) = setup_with_integration_service_substrate(
        r#"//- /IntegrationServices/ОбменСообщениями/Ext/Module.bsl
Процедура ОбработатьСообщение$0ОбычныйПриоритет(Сообщение, Отказ)
КонецПроцедуры
"#,
    );

    let db = analysis.database();
    let module = ModuleId::new(file_id);
    let metadata = db.module_metadata(module);
    assert_eq!(
        metadata.module_type,
        bsl_metadata::ModuleType::IntegrationServiceModule,
        "substrate module file must classify as IntegrationServiceModule"
    );
    let isvc = metadata
        .integration_service
        .as_ref()
        .expect("substrate listing must populate integration_service on ModuleMetadata");
    assert_eq!(isvc.name(), "ОбменСообщениями");
}
