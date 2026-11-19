use std::{
    cell::RefCell,
    fs,
    path::{Path, PathBuf},
};

use base_db::{SourceDatabase, SourceRoot, BSL_SOURCE_ROOT};
use ide::{DiagnosticCode, DiagnosticsConfig};
use ide_db::{metadata::WorkspaceConfigsSnapshot, RootDatabaseImpl};
use ide_host_core::{AnalysisHost, VfsWrite};
use project_model::Project;
use vfs::{FileId, FileSet, Vfs, VfsPath};

#[path = "external_compilation_context/deprecated_replacements.rs"]
mod deprecated_replacements;
#[path = "external_compilation_context/method_declaration_conflicts.rs"]
mod method_declaration_conflicts;
#[path = "external_compilation_context/replacement_proofs.rs"]
mod replacement_proofs;

struct TestVfs(RefCell<Vfs>);
impl VfsWrite for TestVfs {
    fn with_write<R>(&self, f: impl FnOnce(&mut Vfs) -> R) -> R {
        f(&mut self.0.borrow_mut())
    }
}

fn write(root: &Path, relative: &str, text: &str) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn configuration(root: &Path, name: &str, mode: &str, compatibility: &str) {
    write(
        root,
        "Configuration.xml",
        &format!(
            r#"<MetaDataObject><Configuration uuid="00000000-0000-0000-0000-000000000001"><Properties><Name>{name}</Name><DefaultRunMode>{mode}</DefaultRunMode><CompatibilityMode>{compatibility}</CompatibilityMode><UseOrdinaryFormInManagedApplication>false</UseOrdinaryFormInManagedApplication></Properties></Configuration></MetaDataObject>"#
        ),
    );
    write(
        root,
        &format!("Catalogs/{name}.xml"),
        &format!(
            r#"<MetaDataObject><Catalog uuid="00000000-0000-0000-0000-000000000002"><Properties><Name>{name}</Name></Properties></Catalog></MetaDataObject>"#
        ),
    );
}

fn external(root: &Path, object: &str, code: &str) -> PathBuf {
    write(
        root,
        &format!("{object}.xml"),
        &format!(
            r#"<MetaDataObject><ExternalDataProcessor uuid="00000000-0000-0000-0000-000000000003"><Properties><Name>{object}</Name></Properties></ExternalDataProcessor></MetaDataObject>"#
        ),
    );
    let relative = format!("{object}/Ext/ObjectModule.bsl");
    write(root, &relative, code);
    root.join(relative)
}

fn project(root: &Path) -> Project {
    write(
        root,
        "bsl-analyzer.toml",
        r#"[source]
root = "modern"
externals = [
    { name = "LegacyObject", path = "legacy-external", configurationRoot = "legacy" },
    { name = "ModernObject", path = "modern-external", configuration_root = "modern" },
]
"#,
    );
    Project::new(root).expect("both declared main configurations are valid")
}

fn host(project: &Project, files: &[(PathBuf, &str)]) -> (AnalysisHost, TestVfs, Vec<FileId>) {
    let vfs = TestVfs(RefCell::new(Vfs::default()));
    let mut file_set = FileSet::default();
    let mut ids = Vec::new();
    for (path, _) in files {
        let path = VfsPath::from(path.clone());
        let id = vfs.with_write(|vfs| vfs.alloc_file_id(path.clone()));
        file_set.insert(id, path);
        ids.push(id);
    }
    let mut db = RootDatabaseImpl::new();
    db.set_workspace_configs_snapshot(WorkspaceConfigsSnapshot::from_project(project));
    db.set_source_root(BSL_SOURCE_ROOT, SourceRoot::new_local(file_set));
    for (id, (_, text)) in ids.iter().zip(files) {
        db.set_file_source_root(*id, BSL_SOURCE_ROOT);
        db.set_file_text(*id, text);
    }
    ide_host_core::apply_project_settings_to_db(&mut db, &project.config, Some(project));
    let mut host = AnalysisHost::from_database(db);
    host.bootstrap_metadata_substrate(&vfs);
    (host, vfs, ids)
}

#[test]
fn explicit_external_owners_resolve_their_own_metadata_only() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    configuration(&root.join("modern"), "ModernOnly", "ManagedApplication", "Version8_3_27");
    configuration(&root.join("legacy"), "LegacyOnly", "OrdinaryApplication", "Version8_2_16");
    let code = "Процедура Проверить()\nКонецПроцедуры";
    let legacy = external(&root.join("legacy-external"), "LegacyProcessor", code);
    let modern = external(&root.join("modern-external"), "ModernProcessor", code);
    let project = project(root);
    let (host, _, ids) = host(&project, &[(legacy, code), (modern, code)]);
    for (file, visible, hidden) in
        [(ids[0], "LegacyOnly", "ModernOnly"), (ids[1], "ModernOnly", "LegacyOnly")]
    {
        assert!(
            host.raw_database()
                .resolve_metadata_object_for_file(file, bsl_metadata::MdoType::Catalog, visible)
                .is_some(),
            "{visible} must resolve in its selected owner"
        );
        assert!(
            host.raw_database()
                .resolve_metadata_object_for_file(file, bsl_metadata::MdoType::Catalog, hidden)
                .is_none(),
            "{hidden} belongs to another owner"
        );
    }
    assert_eq!(
        project.source_roots().len(),
        3,
        "owner dependencies are not diagnostic subject roots"
    );
    assert_eq!(
        host.raw_database().designer_config_paths().len(),
        1,
        "the additional owner is not a designer extension"
    );
}

#[test]
fn same_named_external_objects_are_distinct_in_proven_different_owners() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    configuration(&root.join("modern"), "ModernOnly", "ManagedApplication", "Version8_3_27");
    configuration(&root.join("legacy"), "LegacyOnly", "OrdinaryApplication", "Version8_2_16");
    external(&root.join("legacy-external"), "Shared", "");
    external(&root.join("modern-external"), "Shared", "");
    let project = project(root);
    assert_eq!(project.external_paths().len(), 2);
}

#[test]
fn ordinary_external_object_does_not_gain_an_unconditional_server_environment() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    configuration(&root.join("modern"), "ModernOnly", "ManagedApplication", "Version8_3_27");
    configuration(&root.join("legacy"), "LegacyOnly", "OrdinaryApplication", "Version8_2_16");
    let code = "Процедура Проверить()\n    Параметр = ПараметрЗапуска;\n    ЗавершитьРаботуСистемы();\nКонецПроцедуры";
    let legacy = external(&root.join("legacy-external"), "LegacyProcessor", code);
    let modern = external(&root.join("modern-external"), "ModernProcessor", code);
    let project = project(root);
    let (host, _, ids) = host(&project, &[(legacy, code), (modern, code)]);
    let config = DiagnosticsConfig::all_enabled();
    let count = |id| {
        host.analysis()
            .diagnostics(id, &config)
            .into_iter()
            .filter(|d| d.code == DiagnosticCode::UnavailableInEnvironment)
            .count()
    };
    assert_eq!(count(ids[0]), 0, "ordinary external object compiles in its selected client");
    assert!(count(ids[1]) > 0, "managed/server control retains environment checks");
}

fn managed_form(root: &Path, object: &str, code: &str) -> PathBuf {
    write(
        root,
        &format!("{object}/Forms/Форма.xml"),
        r#"<MetaDataObject><Form uuid="00000000-0000-0000-0000-000000000004"><Properties><Name>Форма</Name><FormType>Managed</FormType></Properties></Form></MetaDataObject>"#,
    );
    write(
        root,
        &format!("{object}/Forms/Форма/Ext/Form.xml"),
        &format!(
            r#"<Form xmlns="http://v8.1c.ru/8.3/xcf/logform" xmlns:v8="http://v8.1c.ru/8.1/data/core" xmlns:cfg="http://v8.1c.ru/8.1/data/enterprise/current-config"><Attributes><Attribute name="Объект" id="1"><Type><v8:Type>cfg:ExternalDataProcessorObject.{object}</v8:Type></Type><MainAttribute>true</MainAttribute></Attribute></Attributes></Form>"#
        ),
    );
    let path = format!("{object}/Forms/Форма/Ext/Form/Module.bsl");
    write(root, &path, code);
    root.join(path)
}

#[test]
fn converted_external_form_object_keeps_own_export_and_guard_scope() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    configuration(&root.join("modern"), "ModernOnly", "ManagedApplication", "Version8_3_27");
    configuration(&root.join("legacy"), "LegacyOnly", "ManagedApplication", "Version8_2_16");
    let legacy_code = "#Если Сервер Или ТолстыйКлиентОбычноеПриложение Или ВнешнееСоединение Тогда\nПроцедура OldExport() Экспорт\nКонецПроцедуры\n#КонецЕсли\nПроцедура PrivateMethod()\nКонецПроцедуры\n#Если Клиент Тогда\nПроцедура ClientOnly() Экспорт\nКонецПроцедуры\n#КонецЕсли";
    let modern_code = "Процедура ModernExport() Экспорт\nКонецПроцедуры";
    let form_code = "&НаСервере\nПроцедура Проверить()\n    Значение = РеквизитФормыВЗначение(\"Объект\");\n    Значение.OldExport();\n    Значение.PrivateMethod();\n    Значение.ModernExport();\n    Значение.ClientOnly();\nКонецПроцедуры";
    let legacy = external(&root.join("legacy-external"), "Shared", legacy_code);
    let modern = external(&root.join("modern-external"), "Shared", modern_code);
    let form = managed_form(&root.join("legacy-external"), "Shared", form_code);
    let project = project(root);
    let (host, _, ids) =
        host(&project, &[(legacy, legacy_code), (modern, modern_code), (form, form_code)]);
    let analysis = host.analysis();
    let target = analysis
        .goto_definition(
            ids[2],
            (form_code.find("Значение.OldExport").unwrap() + "Значение.".len() + 2) as u32,
        )
        .expect("the converted value owns the external object's export");
    assert_eq!(target.file_id, ids[0]);
    let diagnostics = analysis.diagnostics(ids[2], &DiagnosticsConfig::all_enabled());
    let methods = diagnostics
        .iter()
        .filter(|d| d.code == DiagnosticCode::UnresolvedMethodCall)
        .map(|d| &d.message)
        .collect::<Vec<_>>();
    assert_eq!(methods.len(), 2, "private and foreign-owner methods only: {diagnostics:?}");
    assert!(methods.iter().any(|message| message.contains("PrivateMethod")), "{methods:?}");
    assert!(methods.iter().any(|message| message.contains("ModernExport")), "{methods:?}");
    assert!(
        diagnostics
            .iter()
            .any(|d| d.code == DiagnosticCode::ModuleAccessibility
                && d.message.contains("ClientOnly")),
        "client guard remains unavailable to the server: {diagnostics:?}"
    );
}

#[test]
fn selected_owner_context_follows_prepared_headers_in_independent_views() {
    use hir::CompilationContextOrigin;
    use ide_host_core::{EditorView, WorkspaceSnapshot};
    use std::sync::Arc;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    configuration(&root.join("modern"), "ModernOnly", "ManagedApplication", "Version8_3_27");
    configuration(&root.join("legacy"), "LegacyOnly", "OrdinaryApplication", "Version8_2_16");
    let legacy = external(&root.join("legacy-external"), "LegacyProcessor", "");
    let modern = external(&root.join("modern-external"), "ModernProcessor", "");
    let project = project(root);
    let (host, vfs, ids) = host(&project, &[(legacy, ""), (modern, "")]);
    let context = host.raw_database().file_compilation_context(ids[0]).unwrap();
    assert_eq!(context.origin, CompilationContextOrigin::ExplicitExternalOwner);
    assert_eq!(context.compatibility_mode.as_deref(), Some("Version8_2_16"));
    assert_eq!(context.default_run_mode.as_deref(), Some("OrdinaryApplication"));
    assert!(context.environment.ordinary_app_support);
    assert!(!context.environment.managed_run_mode);
    assert_eq!(context.environment.ordinary_forms_in_managed_app, Some(false));
    assert_eq!(
        host.raw_database().metadata_symbol_roots_for_file(ids[0], bsl_metadata::MdoType::Catalog),
        Some(vec![root.join("legacy")])
    );
    assert_eq!(
        host.raw_database()
            .metadata_symbol_roots_for_file(ids[0], bsl_metadata::MdoType::ExternalDataProcessor),
        Some(vec![root.join("legacy-external")])
    );

    let header_path = root.join("legacy/Configuration.xml");
    let header = vfs.with_write(|vfs| vfs.file_id(&VfsPath::from(header_path.clone())).unwrap());
    let mut view = EditorView::new(WorkspaceSnapshot::capture(host.raw_database(), 0));
    let text = fs::read_to_string(&header_path).unwrap().replace("Version8_2_16", "Version8_3_10");
    view.open(header, header_path, Arc::from(text));
    assert_eq!(
        view.database().file_compilation_context(ids[0]).unwrap().compatibility_mode.as_deref(),
        Some("Version8_3_10")
    );
    assert_eq!(
        host.raw_database().file_compilation_context(ids[0]).unwrap().compatibility_mode.as_deref(),
        Some("Version8_2_16"),
        "the independent view must not rewrite the owner backend"
    );
    assert_eq!(
        view.database().file_compilation_context(ids[1]).unwrap().compatibility_mode.as_deref(),
        Some("Version8_3_27")
    );
    assert!(view.change(header, Arc::from("<broken>")));
    let invalid = view.database().file_compilation_context(ids[0]).unwrap();
    assert_eq!(invalid.origin, CompilationContextOrigin::Unknown);
    assert!(invalid.compatibility_mode.is_none());
    assert!(
        view.database()
            .metadata_symbol_roots_for_file(header, bsl_metadata::MdoType::Catalog)
            .is_none(),
        "an unreadable dependency header cannot prove a host metadata identity"
    );
    assert!(view
        .database()
        .metadata_symbol_roots_for_file(ids[0], bsl_metadata::MdoType::Catalog)
        .is_none());
    let extension = fs::read_to_string(root.join("legacy/Configuration.xml")).unwrap().replace(
        "</Properties>",
        "<ConfigurationExtensionPurpose>Customization</ConfigurationExtensionPurpose></Properties>",
    );
    assert!(view.change(header, Arc::from(extension)));
    let invalid = view.database().file_compilation_context(ids[0]).unwrap();
    assert_eq!(invalid.origin, CompilationContextOrigin::Unknown);
    assert!(
        invalid.compatibility_mode.is_none(),
        "an extension header cannot establish main-configuration compatibility"
    );
}

#[test]
fn legacy_default_and_unknown_owner_keep_distinct_provenance() {
    for has_main in [true, false] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        if has_main {
            configuration(
                &root.join("modern"),
                "ModernOnly",
                "ManagedApplication",
                "Version8_3_27",
            );
        }
        let code = "Процедура Проверить()\n    Значение = НеизвестноеИмяКонфигурации;\n    Ссылка = Справочники.ОтсутствующийКаталог.ПустаяСсылка();\n    Параметр = ПараметрЗапуска;\n    ЗавершитьРаботуСистемы();\nКонецПроцедуры";
        let source = external(&root.join("external"), "Processor", code);
        let config = if has_main {
            "[source]\nroot = \"modern\"\nexternals = [{name = \"Object\", path = \"external\"}]"
        } else {
            "[source]\nexternals = [{name = \"Object\", path = \"external\"}]"
        };
        write(root, "bsl-analyzer.toml", config);
        let project = Project::new(root).unwrap();
        let (host, _, ids) = host(&project, &[(source, code)]);
        let db = host.raw_database();
        let context = db.file_compilation_context(ids[0]).unwrap();
        assert_eq!(
            context.origin,
            if has_main {
                hir::CompilationContextOrigin::LegacyExternalDefault
            } else {
                hir::CompilationContextOrigin::Unknown
            }
        );
        assert!(!context.has_proven_external_owner());
        assert!(db
            .metadata_symbol_roots_for_file(ids[0], bsl_metadata::MdoType::Catalog)
            .is_none());
        assert!(db
            .metadata_symbol_roots_for_file(ids[0], bsl_metadata::MdoType::ExternalDataProcessor)
            .is_some());
        if has_main {
            assert!(
                db.resolve_metadata_object_for_file(
                    ids[0],
                    bsl_metadata::MdoType::Catalog,
                    "ModernOnly"
                )
                .is_some(),
                "legacy type visibility is preserved"
            );
        } else {
            assert!(context.compatibility_mode.is_none());
            let diagnostics =
                host.analysis().diagnostics(ids[0], &DiagnosticsConfig::all_enabled());
            assert!(
                !diagnostics.iter().any(|d| matches!(
                    d.code,
                    DiagnosticCode::UnavailableInEnvironment
                        | DiagnosticCode::ModuleAccessibility
                        | DiagnosticCode::UnresolvedName
                        | DiagnosticCode::UnresolvedField
                        | DiagnosticCode::UnresolvedBareCall
                )),
                "unknown owner cannot prove host-dependent failure: {diagnostics:?}"
            );
        }
    }
}

fn common_module(root: &Path, export: &str) -> PathBuf {
    write(
        root,
        "CommonModules/SharedLibrary.xml",
        r#"<MetaDataObject><CommonModule uuid="00000000-0000-0000-0000-000000000005"><Properties><Name>SharedLibrary</Name><Server>true</Server><ClientManagedApplication>false</ClientManagedApplication><ServerCall>false</ServerCall></Properties></CommonModule></MetaDataObject>"#,
    );
    let path = "CommonModules/SharedLibrary/Ext/Module.bsl";
    write(root, path, &format!("Процедура {export}() Экспорт\nКонецПроцедуры"));
    root.join(path)
}

#[test]
fn selected_owner_bodies_are_prepared_without_becoming_diagnostic_targets() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    configuration(&root.join("modern"), "ModernOnly", "ManagedApplication", "Version8_3_27");
    configuration(&root.join("legacy"), "LegacyOnly", "ManagedApplication", "Version8_2_16");
    let old_library = common_module(&root.join("legacy"), "LegacyExport").canonicalize().unwrap();
    let modern_library = common_module(&root.join("modern"), "ModernExport");
    let modern_library_code = fs::read_to_string(&modern_library).unwrap();
    let code = "&НаСервере\nПроцедура Проверить()\n    SharedLibrary.LegacyExport();\n    SharedLibrary.ModernExport();\nКонецПроцедуры";
    let legacy = external(&root.join("legacy-external"), "LegacyProcessor", code);
    let modern = external(&root.join("modern-external"), "ModernProcessor", code);
    let project = project(root);
    let (host, vfs, ids) =
        host(&project, &[(legacy, code), (modern, code), (modern_library, &modern_library_code)]);
    let analysis = host.analysis();
    let offset =
        (code.find("SharedLibrary.LegacyExport").unwrap() + "SharedLibrary.".len() + 2) as u32;
    let target = analysis
        .goto_definition(ids[0], offset)
        .expect("owner dependency body was prepared by the shared host");
    assert_eq!(
        target.file_id,
        vfs.with_write(|vfs| vfs.file_id(&VfsPath::from(old_library)).unwrap())
    );
    for (id, missing) in [(ids[0], "ModernExport"), (ids[1], "LegacyExport")] {
        let diagnostics = analysis.diagnostics(id, &DiagnosticsConfig::all_enabled());
        let unresolved = diagnostics
            .iter()
            .filter(|d| d.code == DiagnosticCode::UnresolvedMethodCall)
            .collect::<Vec<_>>();
        assert_eq!(unresolved.len(), 1, "{diagnostics:?}");
        assert!(unresolved[0].message.contains(missing), "{unresolved:?}");
    }
    let view = ide_host_core::EditorView::new(ide_host_core::WorkspaceSnapshot::capture(
        host.raw_database(),
        0,
    ));
    assert_eq!(view.analysis().goto_definition(ids[0], offset).unwrap().file_id, target.file_id);
    assert!(!project.source_roots().contains(&root.join("legacy")));
}

#[test]
fn same_owner_namesakes_and_foreign_extension_dependencies_are_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    configuration(&root.join("modern"), "ModernOnly", "ManagedApplication", "Version8_3_27");
    configuration(&root.join("legacy"), "LegacyOnly", "ManagedApplication", "Version8_2_16");
    external(&root.join("first"), "Shared", "");
    external(&root.join("second"), "Shared", "");
    for owner in [", configuration_root = \"modern\"", ""] {
        write(root, "bsl-analyzer.toml", &format!("[source]\nroot = \"modern\"\nexternals = [{{name=\"First\",path=\"first\"{owner}}},{{name=\"Second\",path=\"second\"{owner}}}]"));
        assert!(
            Project::new(root).is_err(),
            "same owner or an unproved owner cannot separate namesakes"
        );
    }
    write(root, "extension/Configuration.xml", "<MetaDataObject><Configuration><Properties><Name>Extension</Name><ConfigurationExtensionPurpose>Customization</ConfigurationExtensionPurpose></Properties></Configuration></MetaDataObject>");
    write(root, "bsl-analyzer.toml", "[source]\nroot=\"modern\"\nextensions=[{name=\"Extension\",path=\"extension\",dependsOn=[]}]\nexternals=[{name=\"First\",path=\"first\",configuration_root=\"legacy\",dependsOn=[\"Extension\"]}]");
    let error = Project::new(root).unwrap_err().to_string();
    assert!(error.contains("extension dependencies"), "{error}");
}

#[test]
fn external_report_main_attribute_reaches_the_report_object_export() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    configuration(&root.join("modern"), "ModernOnly", "ManagedApplication", "Version8_3_27");
    write(
        root,
        "report/Shared.xml",
        r#"<MetaDataObject><ExternalReport uuid="00000000-0000-0000-0000-000000000006"><Properties><Name>Shared</Name></Properties></ExternalReport></MetaDataObject>"#,
    );
    let object_code = "Процедура ReportExport() Экспорт\nКонецПроцедуры";
    let object_path = root.join("report/Shared/Ext/ObjectModule.bsl");
    write(root, "report/Shared/Ext/ObjectModule.bsl", object_code);
    let form_code = "&НаСервере\nПроцедура Проверить()\n    Значение = РеквизитФормыВЗначение(\"Объект\");\n    Значение.ReportExport();\nКонецПроцедуры";
    let form_path = managed_form(&root.join("report"), "Shared", form_code);
    let form_xml = root.join("report/Shared/Forms/Форма/Ext/Form.xml");
    let text = fs::read_to_string(&form_xml)
        .unwrap()
        .replace("ExternalDataProcessorObject", "ExternalReportObject");
    fs::write(form_xml, text).unwrap();
    write(root, "bsl-analyzer.toml", "[source]\nroot=\"modern\"\nexternals=[{name=\"Report\",path=\"report\",configuration_root=\"modern\"}]");
    let project = Project::new(root).unwrap();
    let (host, _, ids) = host(&project, &[(object_path, object_code), (form_path, form_code)]);
    let analysis = host.analysis();
    let target = analysis
        .goto_definition(
            ids[1],
            (form_code.find("Значение.ReportExport").unwrap() + "Значение.".len() + 2) as u32,
        )
        .expect("main report attribute keeps ExternalReportObject");
    assert_eq!(target.file_id, ids[0]);
    let diagnostics = analysis.diagnostics(ids[1], &DiagnosticsConfig::all_enabled());
    assert!(
        !diagnostics.iter().any(|d| matches!(
            d.code,
            DiagnosticCode::UnresolvedMethodCall | DiagnosticCode::ModuleAccessibility
        )),
        "{diagnostics:?}"
    );
    assert_eq!(
        host.raw_database()
            .metadata_symbol_roots_for_file(ids[1], bsl_metadata::MdoType::ExternalReport),
        Some(vec![root.join("report")])
    );
    assert_eq!(
        host.raw_database()
            .metadata_symbol_roots_for_file(ids[1], bsl_metadata::MdoType::ExternalDataProcessor),
        Some(Vec::new())
    );
}

#[test]
fn main_project_overrides_do_not_become_facts_of_a_different_owner() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    configuration(&root.join("modern"), "ModernOnly", "ManagedApplication", "Version8_3_27");
    configuration(&root.join("legacy"), "LegacyOnly", "ManagedApplication", "Version8_2_16");
    let legacy = external(&root.join("legacy-external"), "LegacyProcessor", "");
    let modern = external(&root.join("modern-external"), "ModernProcessor", "");
    let _ = project(root);
    let config_path = root.join("bsl-analyzer.toml");
    let source_config = fs::read_to_string(&config_path)
        .unwrap()
        .replace("configuration_root = \"modern\"", "configuration_root = \"modern/.\"");
    fs::write(config_path, format!("compatibility_mode=\"Version8_3_5\"\n{source_config}\n[diagnostics]\nordinaryAppSupport=true\n[features]\nchecked_environments=[\"Сервер\"]\n")).unwrap();
    let project = Project::new(root).unwrap();
    let (host, _, ids) = host(&project, &[(legacy, ""), (modern, "")]);
    let different_owner = host.raw_database().file_compilation_context(ids[0]).unwrap();
    let main_owner = host.raw_database().file_compilation_context(ids[1]).unwrap();
    assert_eq!(different_owner.compatibility_mode.as_deref(), Some("Version8_2_16"));
    assert!(!different_owner.environment.ordinary_app_support);
    assert_eq!(main_owner.compatibility_mode.as_deref(), Some("Version8_3_5"));
    assert!(main_owner.environment.ordinary_app_support);
    assert_eq!(
        different_owner.environment.checked_environments,
        hir::execution_env::EnvFlags::SERVER
    );
    assert_eq!(main_owner.environment.checked_environments, hir::execution_env::EnvFlags::SERVER);
}

#[test]
fn dependency_body_and_header_changes_reach_existing_and_reopened_views_without_a_loop() {
    use ide_host_core::{EditorView, SourceBatchChange, WorkspaceBackend};
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    configuration(&root.join("modern"), "ModernOnly", "ManagedApplication", "Version8_3_27");
    configuration(&root.join("legacy"), "LegacyOnly", "ManagedApplication", "Version8_2_16");
    let library = common_module(&root.join("legacy"), "LegacyExport");
    let code = "&НаСервере\nПроцедура Проверить()\n    SharedLibrary.LegacyExport();\n    SharedLibrary.NewExport();\nКонецПроцедуры";
    let legacy = external(&root.join("legacy-external"), "LegacyProcessor", code);
    let modern = external(&root.join("modern-external"), "ModernProcessor", "");
    let project = project(root);
    let (host, vfs, ids) = host(&project, &[(legacy, code), (modern, "")]);
    let mut backend = WorkspaceBackend::new(host);
    let mut view = EditorView::new(backend.snapshot());
    let missing = |view: &EditorView| {
        view.analysis()
            .diagnostics(ids[0], &DiagnosticsConfig::all_enabled())
            .into_iter()
            .filter(|d| d.code == DiagnosticCode::UnresolvedMethodCall)
            .map(|d| d.message)
            .collect::<Vec<_>>()
    };
    assert!(missing(&view).iter().any(|message| message.contains("NewExport")));
    fs::write(&library, "Процедура NewExport() Экспорт\nКонецПроцедуры").unwrap();
    let changed = backend
        .apply_source_batch(Vec::<SourceBatchChange>::new(), |db, _| {
            ide_host_core::refresh_metadata_substrate(db, &vfs, std::slice::from_ref(&library))
        })
        .expect("dependency body edit publishes a generation");
    assert!(view.catch_up_deltas(backend.snapshot(), &[changed]));
    let after = missing(&view);
    assert_eq!(after.len(), 1, "{after:?}");
    assert!(after[0].contains("LegacyExport"), "{after:?}");
    for _ in 0..3 {
        assert!(
            backend
                .apply_source_batch(Vec::<SourceBatchChange>::new(), |db, _| {
                    ide_host_core::refresh_metadata_substrate(
                        db,
                        &vfs,
                        std::slice::from_ref(&library),
                    )
                })
                .is_none(),
            "unchanged input does not publish or rebuild"
        );
    }
    let header = root.join("legacy/Configuration.xml");
    let text = fs::read_to_string(&header).unwrap().replace("Version8_2_16", "Version8_3_10");
    fs::write(&header, text).unwrap();
    let changed = backend
        .apply_source_batch(Vec::<SourceBatchChange>::new(), |db, _| {
            ide_host_core::refresh_metadata_substrate(db, &vfs, std::slice::from_ref(&header))
        })
        .expect("owner header edit publishes a generation");
    assert!(view.catch_up_deltas(backend.snapshot(), &[changed]));
    assert_eq!(
        view.database().file_compilation_context(ids[0]).unwrap().compatibility_mode.as_deref(),
        Some("Version8_3_10")
    );
    let reopened = EditorView::new(backend.snapshot());
    assert_eq!(missing(&view), missing(&reopened));
    assert_eq!(
        reopened.database().file_compilation_context(ids[0]).unwrap().compatibility_mode.as_deref(),
        Some("Version8_3_10")
    );
}

#[test]
fn ordinary_external_form_checks_own_object_method_guards() {
    use hir::execution_env::EnvFlags;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    configuration(&root.join("base"), "Base", "OrdinaryApplication", "Version8_2_16");
    let object_code = "#Если Сервер Или ТолстыйКлиентОбычноеПриложение Тогда\nПроцедура PublicMethod() Экспорт\nКонецПроцедуры\nПроцедура PrivateMethod()\nКонецПроцедуры\n#КонецЕсли\n#Если Сервер Тогда\nПроцедура ServerOnly() Экспорт\nКонецПроцедуры\n#КонецЕсли\n#Если Не ТолстыйКлиентОбычноеПриложение Тогда\nПроцедура NotOrdinary() Экспорт\nКонецПроцедуры\n#КонецЕсли";
    let object = external(&root.join("epf"), "Probe", object_code);
    write(
        root,
        "epf/Probe/Forms/Форма.xml",
        r#"<MetaDataObject><Form><Properties><Name>Форма</Name><FormType>Ordinary</FormType></Properties></Form></MetaDataObject>"#,
    );
    write(
        root,
        "epf/Probe/Forms/Форма/Ext/Form/Form.json",
        r#"{"schema":2,"attributes":[{"name":"ОбработкаОбъект","is_main":true,"attr_type":{"Ref":{"mdo_type":"ExternalDataProcessor","name":"Probe"}}}]}"#,
    );
    let form_code = "Процедура Проверить()\n ОбработкаОбъект.PublicMethod();\n ОбработкаОбъект.PrivateMethod();\n ОбработкаОбъект.ServerOnly();\n ОбработкаОбъект.NotOrdinary();\n ЭтаФорма.ОбработкаОбъект.PublicMethod();\n ThisForm.ОбработкаОбъект.PrivateMethod();\n ЭтотОбъект.ОбработкаОбъект.NotOrdinary();\nКонецПроцедуры\nПроцедура Shadowed(ЭтаФорма, ThisObject)\n ЭтаФорма.ОбработкаОбъект.PrivateMethod();\n ThisObject.ОбработкаОбъект.NotOrdinary();\nКонецПроцедуры\nПроцедура LocalShadow()\n ЭтаФорма = Новый Структура;\n ЭтаФорма.ОбработкаОбъект.PrivateMethod();\nКонецПроцедуры\nПроцедура OtherForm()\n ДругаяФорма = ПолучитьФорму(\"Другая\");\n ДругаяФорма.ОбработкаОбъект.PrivateMethod();\nКонецПроцедуры";
    let form = root.join("epf/Probe/Forms/Форма/Ext/Form/Module.bsl");
    write(root, "epf/Probe/Forms/Форма/Ext/Form/Module.bsl", form_code);
    write(root, "bsl-analyzer.toml", "[source]\nroot=\"base\"\nexternals=[{name=\"Probe\",path=\"epf\",configurationRoot=\"base\"}]\n[features]\nchecked_environments=[\"ТолстыйКлиентОбычноеПриложение\",\"Сервер\"]\n");
    let project = Project::new(root).unwrap();
    let (host, _, ids) = host(&project, &[(object, object_code), (form, form_code)]);
    assert!(host
        .raw_database()
        .file_compilation_context(ids[1])
        .unwrap()
        .environment
        .checked_environments
        .contains(EnvFlags::THICK_CLIENT_ORDINARY));
    use hir::DefDatabase;
    let db = host.raw_database();
    let module = hir::ModuleId::new(ids[0]);
    let method =
        db.module_interface(module).find_method(&hir::Name::new("ServerOnly")).unwrap().clone();
    let metadata = db.module_metadata(module);
    let context = db.file_compilation_context(ids[0]).unwrap();
    assert_eq!(
        context.body_environment(&metadata, &method.directives),
        EnvFlags::THICK_CLIENT_ORDINARY
    );
    assert!(method.preproc_undecided.contains(EnvFlags::THICK_CLIENT_ORDINARY));
    let caller = db.module_metadata(hir::ModuleId::new(ids[1]));
    assert!(caller.is_ordinary_form_module());
    assert!(hir::module_implicit_fields(db, ids[1])
        .iter()
        .any(|f| f.name.as_str() == "ОбработкаОбъект"));
    use hir::{HirDatabase, TypeKernelDb, TypeKind};
    let other = *db.infer(ids[1]).var_types.get("другаяформа").unwrap();
    let alternatives = match db.lookup_type(other) {
        TypeKind::Union(types) => types.to_vec(),
        _ => vec![other],
    };
    assert!(
        alternatives.iter().any(|ty| matches!(db.lookup_type(*ty), TypeKind::PlatformObject(_))),
        "other form must have a platform type"
    );
    let found = host.analysis().diagnostics(ids[1], &DiagnosticsConfig::all_enabled());
    let access =
        found.iter().filter(|d| d.code == DiagnosticCode::ModuleAccessibility).collect::<Vec<_>>();
    assert_eq!(access.len(), 2, "{found:?}");
    assert!(access[0].message.contains("NotOrdinary"));
    let unresolved =
        found.iter().filter(|d| d.code == DiagnosticCode::UnresolvedMethodCall).collect::<Vec<_>>();
    assert_eq!(unresolved.len(), 2, "{found:?}");
    assert!(unresolved[0].message.contains("PrivateMethod"));
}
