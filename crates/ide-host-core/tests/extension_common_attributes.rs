use base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide::{CompletionItemKind, DiagnosticCode, DiagnosticsConfig, Locale};
use ide_db::{metadata::WorkspaceConfigsSnapshot, RootDatabase, RootDatabaseImpl};
use ide_host_core::{AnalysisHost, VfsWrite};
use std::{cell::RefCell, fs, path::PathBuf};
use vfs::{FileId, FileSet, Vfs, VfsPath};

const READER_B: FileId = FileId(1_000_001);
const READER_C: FileId = FileId(1_000_002);
const READER_CONTROL: FileId = FileId(1_000_003);
const READER_D: FileId = FileId(1_000_004);
const CODE: &str = r#"Процедура Проверить()
    Ссылка = Справочники.NativeCatalog.ПустаяСсылка();
    Число = Ссылка.OtherUse;
    Объект = Ссылка.ПолучитьОбъект();
    ЧислоОбъекта = Объект.OtherUse;
    ДругоеЧисло = Объект.OrdinaryAuto;
    Скрыто = Ссылка.AreaUse;
    ОшибкаОбъекта = Объект.Missing379;

    Набор = РегистрыСведений.NativeRegister.СоздатьНаборЗаписей();
    Набор.Отбор.OtherUse.Установить(Число);
    Набор.Отбор.OrdinaryAuto.Установить(Число);
    Запись = Набор.Добавить();
    Запись.OtherUse = Число;
    Запись.OrdinaryAuto = Число;
    Запись.AreaUse = 1;
    Менеджер = РегистрыСведений.NativeRegister.СоздатьМенеджерЗаписи();
    Менеджер.OtherUse = Число;
    Менеджер.Missing379 = 1;

    Запрос = Новый Запрос(
    "ВЫБРАТЬ Т.OtherUse, Т.AreaUse, Т.Missing379, Т.Partner.OtherUse ИЗ Справочник.NativeCatalog КАК Т");
    ЗапросРегистра = Новый Запрос(
    "ВЫБРАТЬ Р.OtherUse, Р.OrdinaryAuto, Р.AreaUse, Р.Missing379 ИЗ РегистрСведений.NativeRegister КАК Р");
    ЗапросСреза = Новый Запрос(
    "ВЫБРАТЬ Р.OtherUse, Р.OrdinaryAuto ИЗ РегистрСведений.NativeRegister.СрезПоследних(&Дата) КАК Р");
КонецПроцедуры
"#;

struct TestVfs(RefCell<Vfs>);

impl VfsWrite for TestVfs {
    fn with_write<R>(&self, f: impl FnOnce(&mut Vfs) -> R) -> R {
        f(&mut self.0.borrow_mut())
    }
}

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../bsl-metadata/fixtures/extension_common_attributes")
}

fn paths(base: PathBuf, a: PathBuf, b: PathBuf, c: PathBuf) -> Vec<(Option<String>, PathBuf)> {
    vec![(None, base), (Some("A".into()), a), (Some("B".into()), b), (Some("C".into()), c)]
}

fn snapshot(paths: Vec<(Option<String>, PathBuf)>) -> WorkspaceConfigsSnapshot {
    let mut snapshot = WorkspaceConfigsSnapshot::from_paths(paths);
    // B reads A's overlay; C is an independent reader. Scope belongs to the root that owns
    // each declaration, even when another extension reads the resulting object.
    snapshot.extension_scopes = vec![
        None,
        Some(bsl_metadata::ExtensionScope::DataSeparation),
        Some(bsl_metadata::ExtensionScope::InfoBase),
        Some(bsl_metadata::ExtensionScope::InfoBase),
    ];
    snapshot.closures = vec![vec![], vec![], vec![1], vec![]];
    snapshot.topological_order = vec![0, 1, 2, 3];
    snapshot
}

fn host_for(
    workspace: WorkspaceConfigsSnapshot,
    readers: &[(FileId, PathBuf, &str)],
    bootstrap: bool,
) -> (AnalysisHost, TestVfs) {
    let mut host = AnalysisHost::default();
    *host.raw_database_mut() = RootDatabaseImpl::new_with_salsa_events();
    host.raw_database_mut().set_workspace_configs_snapshot(workspace);
    let mut files = FileSet::default();
    for (file_id, path, _) in readers {
        files.insert(*file_id, VfsPath::new(path.to_string_lossy().as_ref()));
    }
    let source_root = SourceRootId(0);
    host.raw_database_mut().set_source_root(source_root, SourceRoot::new_local(files));
    for (file_id, _, code) in readers {
        host.raw_database_mut().set_file_source_root(*file_id, source_root);
        host.raw_database_mut().set_file_text(*file_id, code);
    }
    let vfs = TestVfs(RefCell::new(Vfs::default()));
    if bootstrap {
        host.bootstrap_metadata_substrate(&vfs);
    }
    (host, vfs)
}

fn assert_same_common_fields(
    actual: &[bsl_metadata::common_attribute::CommonAttributeField],
    expected: &[bsl_metadata::common_attribute::CommonAttributeField],
) {
    assert_eq!(actual, expected);
}

#[test]
fn bootstrapped_overlay_matches_full_fallback_and_respects_owner_visibility() {
    let fixture = fixture_root();
    // Use separate physical roots for B and C so longest-prefix ownership is unambiguous.
    let temp = tempfile::tempdir().expect("temporary fixture roots");
    let a_root = temp.path().join("A");
    let b_root = temp.path().join("B");
    let c_root = temp.path().join("C");
    copy_tree(&fixture.join("extensions/explicit-use"), &a_root);
    copy_reader_extension_root(&fixture.join("extensions/no-common"), &b_root, true);
    copy_reader_extension_root(&fixture.join("extensions/no-common"), &c_root, false);
    // The synthetic reference attribute exists only in this test-owned copy. It makes the
    // nested SDBL field scenario explicit without changing the canonical native fixture.
    let catalog_path = a_root.join("Catalogs/NativeCatalog.xml");
    let catalog_xml = fs::read_to_string(&catalog_path).unwrap();
    let with_reference = catalog_xml.replace(
        "<ChildObjects/>",
        "<ChildObjects><Attribute uuid=\"11111111-1111-1111-1111-111111111111\"><Properties><Name>Partner</Name><Type><v8:Type>cfg:CatalogRef.NativeCatalog</v8:Type></Type></Properties></Attribute></ChildObjects>",
    );
    assert_ne!(catalog_xml, with_reference, "synthetic reference attribute insertion point");
    fs::write(catalog_path, with_reference).unwrap();
    let roots = paths(fixture.join("base"), a_root, b_root.clone(), c_root.clone());
    let b_file = b_root.join("CommonModules/Probe/Ext/Module.bsl");
    let c_file = c_root.join("CommonModules/Probe/Ext/Module.bsl");
    let readers =
        [(READER_B, b_file, CODE), (READER_C, c_file, "Процедура Проверить()\nКонецПроцедуры")];

    let (fallback, _) = host_for(snapshot(roots.clone()), &readers, false);
    let (mut bootstrapped, vfs) = host_for(snapshot(roots.clone()), &readers, true);
    // The non-bootstrapped resolver is the whole-config merged fallback. A raw
    // `get_configuration` lookup only loads one root and would miss overlays.
    let full_object = fallback
        .raw_database()
        .resolve_metadata_object_for_file(READER_B, bsl_metadata::MdoType::Catalog, "NativeCatalog")
        .expect("whole-config fallback resolves A's extension-only catalog");
    let effective = bootstrapped
        .raw_database()
        .resolve_metadata_object_for_file(READER_B, bsl_metadata::MdoType::Catalog, "NativeCatalog")
        .expect("A's extension-only catalog is visible from dependent B");
    assert_same_common_fields(&effective.common_attributes, &full_object.common_attributes);
    assert_eq!(effective.common_attributes_open, full_object.common_attributes_open);
    assert!(effective.common_attributes.iter().any(|field| {
        field.name == "OtherUse"
            && field.separator
            && field.attr_type == bsl_metadata::AttributeType::Number { precision: 7, scale: 0 }
    }));
    assert!(effective.common_attributes.iter().any(|field| {
        field.name == "OrdinaryAuto"
            && !field.separator
            && field.attr_type == bsl_metadata::AttributeType::Number { precision: 7, scale: 0 }
    }));
    assert!(!effective.common_attributes.iter().any(|field| field.name == "AreaUse"));
    assert!(!effective.common_attributes_open, "a configured Scope and valid XML are closed");

    let effective_base_overlay = bootstrapped
        .raw_database()
        .resolve_metadata_object_for_file(READER_B, bsl_metadata::MdoType::Catalog, "BaseCatalog")
        .expect("base catalog after A and B overlays");
    let full_base_overlay = fallback
        .raw_database()
        .resolve_metadata_object_for_file(READER_B, bsl_metadata::MdoType::Catalog, "BaseCatalog")
        .expect("base catalog in full fallback");
    assert_eq!(effective_base_overlay.as_ref(), full_base_overlay.as_ref());

    let register = bootstrapped
        .raw_database()
        .resolve_register_for_file(
            READER_B,
            bsl_metadata::MdoType::InformationRegister,
            "NativeRegister",
        )
        .expect("typed register lookup");
    let register_by_name = bootstrapped
        .raw_database()
        .resolve_register_by_name_for_file(READER_B, "NativeRegister")
        .expect("by-name register lookup");
    let full_register = fallback
        .raw_database()
        .resolve_register_for_file(
            READER_B,
            bsl_metadata::MdoType::InformationRegister,
            "NativeRegister",
        )
        .expect("register in full fallback");
    assert_eq!(register.as_ref(), full_register.as_ref());
    assert_eq!(register.as_ref(), register_by_name.as_ref());
    assert!(!register.common_attributes_open());
    assert!(register.common_attributes().iter().any(|field| {
        field.name == "OtherUse"
            && field.separator
            && field.attr_type == bsl_metadata::AttributeType::Number { precision: 7, scale: 0 }
    }));
    assert!(!register.common_attributes().iter().any(|field| field.name == "AreaUse"));

    let b_configs = bootstrapped.raw_database().get_all_configurations(READER_B);
    assert_eq!(
        b_configs.iter().map(|(name, _)| name.as_deref()).collect::<Vec<_>>(),
        [None, Some("A"), Some("B")]
    );
    assert!(!b_configs.iter().any(|(name, config)| name.as_deref() == Some("B")
        && config.find_metadata_object(bsl_metadata::MdoType::Catalog, "NativeCatalog").is_some()));
    let c_configs = bootstrapped.raw_database().get_all_configurations(READER_C);
    assert_eq!(
        c_configs.iter().map(|(name, _)| name.as_deref()).collect::<Vec<_>>(),
        [None, Some("C")]
    );
    assert!(
        bootstrapped
            .raw_database()
            .resolve_metadata_object_for_file(
                READER_C,
                bsl_metadata::MdoType::Catalog,
                "NativeCatalog",
            )
            .is_none(),
        "C cannot read A's object without a dependency edge"
    );

    let analysis = bootstrapped.analysis();
    let diagnostics = analysis.diagnostics(READER_B, &DiagnosticsConfig::all_enabled());
    let full_diagnostics =
        fallback.analysis().diagnostics(READER_B, &DiagnosticsConfig::all_enabled());
    for code in [DiagnosticCode::UnresolvedField, DiagnosticCode::UnknownFieldInQuery] {
        let expected = full_diagnostics
            .iter()
            .filter(|d| d.code == code)
            .map(|d| (&d.range, &d.message))
            .collect::<Vec<_>>();
        let actual = diagnostics
            .iter()
            .filter(|d| d.code == code)
            .map(|d| (&d.range, &d.message))
            .collect::<Vec<_>>();
        assert_eq!(actual, expected, "full/bootstrap diagnostics for {code:?}");
    }
    let messages = |code| {
        diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.code == code)
            .map(|d| d.message.as_str())
            .collect::<Vec<_>>()
    };
    let unresolved = messages(DiagnosticCode::UnresolvedField);
    let unknown_query = messages(DiagnosticCode::UnknownFieldInQuery);
    assert!(
        unresolved.iter().any(|message| message.contains("AreaUse")),
        "hidden separator: {unresolved:?}"
    );
    assert!(
        unresolved.iter().any(|message| message.contains("Missing379")),
        "same object negative control: {unresolved:?}"
    );
    assert!(
        unknown_query.iter().filter(|message| message.contains("Missing379")).count() == 2,
        "SDBL Missing379 on the closed catalog and register: {unknown_query:?}"
    );
    assert!(
        unknown_query.iter().any(|message| message.contains("AreaUse")),
        "hidden SDBL separator: {unknown_query:?}"
    );
    assert!(!unknown_query.iter().any(|message| message.contains("OtherUse")));
    assert!(!unresolved.iter().any(|message| message.contains("OtherUse")));
    assert!(
        unresolved.iter().filter(|message| message.contains("OrdinaryAuto")).count() == 1,
        "ordinary common field is excluded from register filters: {unresolved:?}"
    );
    assert!(
        unknown_query.iter().any(|message| message.contains("AreaUse")),
        "register main query also excludes the hidden field: {unknown_query:?}"
    );
    assert!(
        !unknown_query.iter().any(|message| message.contains("OrdinaryAuto")),
        "register main and SliceLast expose ordinary common fields: {unknown_query:?}"
    );
    let field_offset = CODE.find("Ссылка.OtherUse").unwrap() as u32 + "Ссылка.".len() as u32;
    let hover = analysis.hover(READER_B, field_offset, Locale::Ru).expect("common-field hover");
    assert!(hover.markup.contains("Число"), "typed field hover: {}", hover.markup);
    drop(analysis);

    let completion_code = "Процедура Подсказка()\n    Ссылка = Справочники.NativeCatalog.ПустаяСсылка();\n    Ссылка.O\nКонецПроцедуры";
    bootstrapped.raw_database_mut().set_file_text(READER_B, completion_code);
    let completion_analysis = bootstrapped.analysis();
    let items = completion_analysis.completions(
        READER_B,
        completion_code.find("Ссылка.O").unwrap() as u32 + "Ссылка.".len() as u32 + 1,
        None,
        Locale::Ru,
    );
    let labels = items.iter().map(|item| item.label.as_str()).collect::<Vec<_>>();
    assert!(labels.contains(&"OtherUse"), "completion labels: {labels:?}");
    assert!(!labels.contains(&"AreaUse"), "hidden field completion labels: {labels:?}");
    let other_use = items.iter().find(|item| item.label == "OtherUse").unwrap();
    assert_eq!(other_use.kind, CompletionItemKind::Field);
    assert!(
        other_use.detail.as_deref().unwrap_or_default().contains("Число"),
        "completion retains the common field type: {:?}",
        other_use.detail
    );
    drop(completion_analysis);

    let nested_code = "Процедура Подсказка()\n    Запрос = Новый Запрос(\"ВЫБРАТЬ Т.Partner.O ИЗ Справочник.NativeCatalog КАК Т\");\nКонецПроцедуры";
    assert!(bootstrapped.raw_database_mut().salsa_events_reset());
    bootstrapped.raw_database_mut().set_file_text(READER_B, nested_code);
    let nested_analysis = bootstrapped.analysis();
    let nested_items = nested_analysis.completions(
        READER_B,
        nested_code.find("Т.Partner.O").unwrap() as u32 + "Т.Partner.O".len() as u32,
        None,
        Locale::Ru,
    );
    let nested_field = nested_items
        .iter()
        .find(|item| item.label == "OtherUse")
        .expect("nested SDBL reference exposes the confirmed common field");
    assert!(
        nested_field.detail.as_deref().unwrap_or_default().contains("Число"),
        "{nested_field:?}"
    );
    assert!(!nested_items.iter().any(|item| item.label == "AreaUse"));
    drop(nested_analysis);
    let nested_events = bootstrapped.raw_database().salsa_event_report().unwrap();
    for family in ["load_configuration", "chain_configuration"] {
        assert_eq!(
            nested_events
                .iter()
                .filter(|row| row.name.contains(family))
                .map(|row| row.execute)
                .sum::<u64>(),
            0,
            "nested completion must use per-MDO metadata instead of composing whole configurations"
        );
    }

    // A BSL-only edit must not refresh metadata inputs or rerun metadata parsers.
    let mut code_after_edit = CODE.to_string();
    code_after_edit
        .push_str("\nПроцедура ПослеИзмененияBSL()\n    ЛокальноеЗначение = 1;\nКонецПроцедуры\n");
    let unrelated_bsl = b_root.join("CommonModules/Probe/Ext/Module.bsl");
    assert!(bootstrapped.raw_database_mut().salsa_events_reset(), "event counters are enabled");
    bootstrapped.raw_database_mut().set_file_text(READER_B, &code_after_edit);
    assert!(!bootstrapped.refresh_metadata_substrate(&vfs, std::slice::from_ref(&unrelated_bsl)));
    let after_bsl_edit = bootstrapped
        .raw_database()
        .resolve_metadata_object_for_file(READER_B, bsl_metadata::MdoType::Catalog, "NativeCatalog")
        .unwrap();
    assert_eq!(after_bsl_edit.common_attributes, effective.common_attributes);
    let register_after_bsl_edit = bootstrapped
        .raw_database()
        .resolve_register_by_name_for_file(READER_B, "NativeRegister")
        .unwrap();
    assert_eq!(register_after_bsl_edit.as_ref(), register.as_ref());
    // Parse the edited BSL on purpose so the event report demonstrates ordinary code parsing
    // while proving that no configuration or metadata parser was pulled back in.
    let _ = bootstrapped.analysis().diagnostics(READER_B, &DiagnosticsConfig::all_enabled());
    let events = bootstrapped.raw_database().salsa_event_report().expect("event report enabled");
    let event_summary =
        events.iter().map(|row| (row.name.as_str(), row.execute)).collect::<Vec<_>>();
    for family in
        ["load_configuration", "common_attribute_set", "parse_mdo_query", "parse_register_query"]
    {
        let executions: u64 =
            events.iter().filter(|row| row.name.contains(family)).map(|row| row.execute).sum();
        assert_eq!(
            executions, 0,
            "{family} reran after an unrelated BSL-only edit: {event_summary:?}"
        );
    }
    assert!(
        events.iter().any(|row| row.name == "parse_query" && row.execute > 0),
        "edited BSL should parse independently: {event_summary:?}"
    );

    // The snapshot setter changes all ownership/scope/topology fields atomically. This valid
    // Scope transition need not change the visible projection, so compare it with a fresh
    // whole-config fallback built from the same new snapshot.
    let mut changed_scope = snapshot(roots.clone());
    changed_scope.extension_scopes[1] = Some(bsl_metadata::ExtensionScope::InfoBase);
    bootstrapped.raw_database_mut().set_workspace_configs_snapshot(changed_scope.clone());
    assert_eq!(
        bootstrapped.raw_database().workspace_configs_snapshot().extension_scopes,
        changed_scope.extension_scopes
    );
    let scoped_object = bootstrapped
        .raw_database()
        .resolve_metadata_object_for_file(READER_B, bsl_metadata::MdoType::Catalog, "NativeCatalog")
        .expect("object after atomic scope reload");
    let (fresh_scoped_fallback, _) = host_for(changed_scope, &readers, false);
    let expected_scoped_object = fresh_scoped_fallback
        .raw_database()
        .resolve_metadata_object_for_file(READER_B, bsl_metadata::MdoType::Catalog, "NativeCatalog")
        .expect("fresh fallback after atomic scope reload");
    assert_eq!(scoped_object.common_attributes, expected_scoped_object.common_attributes);
    assert_eq!(scoped_object.common_attributes_open, expected_scoped_object.common_attributes_open);
    let scoped_register = bootstrapped
        .raw_database()
        .resolve_register_by_name_for_file(READER_B, "NativeRegister")
        .unwrap();
    let expected_scoped_register = fresh_scoped_fallback
        .raw_database()
        .resolve_register_by_name_for_file(READER_B, "NativeRegister")
        .unwrap();
    assert_eq!(scoped_register.as_ref(), expected_scoped_register.as_ref());
}

#[test]
fn extension_only_workspace_does_not_treat_its_first_root_as_the_base() {
    let extension = fixture_root().join("extensions/explicit-use");
    let roots = vec![(Some("A".into()), extension.clone())];
    let reader = extension.join("CommonModules/Probe/Ext/Module.bsl");
    let readers = [(READER_B, reader, "Процедура Проверить()\nКонецПроцедуры")];
    let mut workspace = WorkspaceConfigsSnapshot::from_paths(roots.clone());
    workspace.extension_scopes = vec![Some(bsl_metadata::ExtensionScope::DataSeparation)];
    let (fallback, _) = host_for(workspace.clone(), &readers, false);
    let (bootstrapped, _) = host_for(workspace, &readers, true);
    let expected_object = fallback
        .raw_database()
        .resolve_metadata_object_for_file(READER_B, bsl_metadata::MdoType::Catalog, "NativeCatalog")
        .expect("extension-only whole-config fallback");
    let actual_object = bootstrapped
        .raw_database()
        .resolve_metadata_object_for_file(READER_B, bsl_metadata::MdoType::Catalog, "NativeCatalog")
        .expect("extension-only object");
    assert_eq!(actual_object.common_attributes, expected_object.common_attributes);
    assert_eq!(actual_object.common_attributes_open, expected_object.common_attributes_open);
    assert!(
        actual_object.common_attributes_open,
        "adopted common attributes without a base stay incomplete"
    );
    let expected_register = fallback
        .raw_database()
        .resolve_register_by_name_for_file(READER_B, "NativeRegister")
        .unwrap();
    let actual_register = bootstrapped
        .raw_database()
        .resolve_register_by_name_for_file(READER_B, "NativeRegister")
        .unwrap();
    assert_eq!(actual_register.as_ref(), expected_register.as_ref());
}

#[test]
fn legacy_unknown_separator_uses_the_extension_roots_legacy_projection() {
    let fixture = fixture_root();
    let temp = tempfile::tempdir().expect("temporary fixture root");
    let extension = temp.path().join("Extension");
    copy_tree(&fixture.join("extensions/no-common"), &extension);
    let attributes = extension.join("CommonAttributes");
    fs::create_dir_all(&attributes).unwrap();
    fs::write(
        attributes.join("LegacyUnknown.xml"),
        r#"<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:xr="http://v8.1c.ru/8.3/xcf/readable">
<CommonAttribute uuid="11111111-1111-1111-1111-111111111111"><Properties>
<ObjectBelonging>Own</ObjectBelonging><Name>LegacyUnknown</Name><AutoUse>Use</AutoUse>
<DataSeparation>Separate</DataSeparation>
</Properties></CommonAttribute></MetaDataObject>"#,
    )
    .unwrap();

    let roots = vec![(Some("Legacy".into()), extension.clone())];
    let reader = extension.join("CommonModules/Probe/Ext/Module.bsl");
    let readers = [(READER_B, reader, "Процедура Проверить()\nКонецПроцедуры")];
    let mut workspace = WorkspaceConfigsSnapshot::from_paths(roots);
    workspace.extension_scopes = vec![None];
    let (fallback, _) = host_for(workspace.clone(), &readers, false);
    let (bootstrapped, _) = host_for(workspace, &readers, true);

    let assert_legacy = |register: &bsl_metadata::Register| {
        assert!(register.common_attributes().iter().any(|field| {
            field.name == "LegacyUnknown"
                && field.attr_type == bsl_metadata::AttributeType::Unknown
                && field.separator
        }));
        assert!(!register.common_attributes_open());
    };
    for db in [fallback.raw_database(), bootstrapped.raw_database()] {
        let register = db
            .resolve_register_for_file(
                READER_B,
                bsl_metadata::MdoType::InformationRegister,
                "NativeRegister",
            )
            .expect("file-bound register");
        assert_legacy(&register);
    }
    let inventory_register = bootstrapped
        .raw_database()
        .resolve_register_across_designer_roots(
            bsl_metadata::MdoType::InformationRegister,
            "NativeRegister",
        )
        .expect("designer-wide register");
    assert_legacy(&inventory_register);
}

#[test]
fn partial_register_overlays_merge_by_kind_and_name_with_owner_scoped_metadata() {
    let fixture = fixture_root();
    let temp = tempfile::tempdir().expect("temporary fixture roots");
    let base = temp.path().join("base");
    let a = temp.path().join("A");
    let b = temp.path().join("B");
    let c = temp.path().join("C");
    let d = temp.path().join("D");
    let hidden = temp.path().join("Hidden");
    copy_tree(&fixture.join("base"), &base);
    let base_register_path = base.join("InformationRegisters/BaseRegister.xml");
    let base_register_xml = fs::read_to_string(&base_register_path).unwrap();
    fs::write(
        base_register_path,
        base_register_xml.replace(
            "</ChildObjects>",
            "<Attribute uuid=\"f0000000-0000-0000-0000-000000000001\"><Properties><Name>BaseAttribute</Name><Type><v8:Type>xs:boolean</v8:Type></Type></Properties></Attribute></ChildObjects>",
        ),
    )
    .unwrap();
    for (name, root) in [("A", &a), ("B", &b), ("C", &c), ("D", &d)] {
        copy_tree(&fixture.join("extensions/explicit-use"), root);
        let register = root.join("InformationRegisters/BaseRegister.xml");
        let mut xml = fs::read_to_string(&register).unwrap();
        match name {
            "A" => xml = xml.replace(
                "0974fddb-727e-5ee2-bbb2-f9896e3c8862</ExtendedConfigurationObject>",
                "11111111-1111-1111-1111-111111111111</ExtendedConfigurationObject>",
            ),
            "B" => xml = xml.replace(
                "<ExtendedConfigurationObject>0974fddb-727e-5ee2-bbb2-f9896e3c8862</ExtendedConfigurationObject>",
                "",
            ),
            "C" => xml = xml.replace(
                "0974fddb-727e-5ee2-bbb2-f9896e3c8862</ExtendedConfigurationObject>",
                "not-a-uuid</ExtendedConfigurationObject>",
            ),
            "D" => {}
            _ => unreachable!(),
        }
        let (dimension, resource, attribute) = match name {
            "A" => ("AKey", "AAmount", "AFlag"),
            "B" => ("BKey", "BAmount", "BFlag"),
            "C" => ("CKey", "CAmount", "CFlag"),
            "D" => ("DKey", "DAmount", "DFlag"),
            _ => unreachable!(),
        };
        let children = format!(
            "<Dimension uuid=\"{name}0000000-0000-0000-0000-000000000001\"><Properties><Name>{dimension}</Name><Type><v8:Type>xs:string</v8:Type><v8:StringQualifiers><v8:Length>20</v8:Length><v8:AllowedLength>Variable</v8:AllowedLength></v8:StringQualifiers></Type></Properties></Dimension>\
<Resource uuid=\"{name}0000000-0000-0000-0000-000000000002\"><Properties><Name>{resource}</Name><Type><v8:Type>xs:decimal</v8:Type><v8:NumberQualifiers><v8:Digits>10</v8:Digits><v8:FractionDigits>0</v8:FractionDigits><v8:AllowedSign>Any</v8:AllowedSign></v8:NumberQualifiers></Type></Properties></Resource>\
<Attribute uuid=\"{name}0000000-0000-0000-0000-000000000003\"><Properties><Name>{attribute}</Name><Type><v8:Type>xs:boolean</v8:Type></Type></Properties></Attribute>"
        );
        xml = xml.replace("<ChildObjects/>", &format!("<ChildObjects>{children}</ChildObjects>"));
        fs::write(register, xml).unwrap();
    }
    copy_reader_extension_root(&fixture.join("extensions/no-common"), &hidden, true);

    // A uses InfoBase, B deliberately has no scope, and C uses DataSeparation. This pins
    // owner-local scope selection across the mixed chain instead of applying C's scope globally.
    // A and C contribute scoped Own fields. B follows
    // the old e3c05f5e AutoUse/Content projection, including unknown types and raw separator.
    write_common_attribute(
        &a,
        "ACommon",
        "Own",
        "Use",
        "<DataSeparation>DontUse</DataSeparation>",
        "",
    );
    write_common_attribute(
        &b,
        "LegacyAuto",
        "Adopted",
        "Use",
        "<DataSeparation>Separate</DataSeparation>",
        "<Content><xr:Item><xr:Metadata>InformationRegister.BaseRegister</xr:Metadata><xr:Use>Auto</xr:Use></xr:Item></Content>",
    );
    write_common_attribute(
        &b,
        "LegacyUse",
        "Adopted",
        "DontUse",
        "<DataSeparation>DontUse</DataSeparation>",
        "<Content><xr:Item><xr:Metadata>InformationRegister.BaseRegister</xr:Metadata><xr:Use>Use</xr:Use></xr:Item></Content>",
    );
    write_common_attribute(
        &b,
        "LegacyDontUse",
        "Adopted",
        "Use",
        "<DataSeparation>DontUse</DataSeparation>",
        "<Content><xr:Item><xr:Metadata>InformationRegister.BaseRegister</xr:Metadata><xr:Use>DontUse</xr:Use></xr:Item></Content>",
    );
    write_common_attribute(
        &b,
        "LegacyIndependent",
        "Adopted",
        "Use",
        "<DataSeparation>Separate</DataSeparation><SeparatedDataUse>Independently</SeparatedDataUse>",
        "<Content><xr:Item><xr:Metadata>InformationRegister.BaseRegister</xr:Metadata><xr:Use>Use</xr:Use></xr:Item></Content>",
    );
    write_common_attribute(
        &c,
        "CCommon",
        "Own",
        "Use",
        "<DataSeparation>DontUse</DataSeparation>",
        "",
    );
    write_common_attribute(
        &d,
        "DCommon",
        "Own",
        "Use",
        "<DataSeparation>DontUse</DataSeparation>",
        "",
    );
    let legacy_auto = b.join("CommonAttributes/LegacyAuto.xml");
    let legacy_xml = fs::read_to_string(&legacy_auto).unwrap();
    fs::write(legacy_auto, legacy_xml.replace("<Type><v8:Type>xs:boolean</v8:Type></Type>", ""))
        .unwrap();

    let roots = vec![
        (None, base.clone()),
        (Some("A".into()), a.clone()),
        (Some("B".into()), b.clone()),
        (Some("C".into()), c.clone()),
        (Some("D".into()), d.clone()),
    ];
    let mut workspace = WorkspaceConfigsSnapshot::from_paths(roots);
    workspace.extension_scopes = vec![
        None,
        Some(bsl_metadata::ExtensionScope::InfoBase),
        None,
        Some(bsl_metadata::ExtensionScope::DataSeparation),
        Some(bsl_metadata::ExtensionScope::InfoBase),
    ];
    workspace.closures = vec![vec![], vec![], vec![1], vec![1, 2], vec![1, 2, 3]];
    workspace.topological_order = vec![0, 1, 2, 3, 4];
    let code = "Процедура Проверить()\n    Набор = РегистрыСведений.BaseRegister.СоздатьНаборЗаписей();\n    Запись = Набор.Добавить();\n    Значение = Запись.DCommon;\n    ОшибкаОткрытойМодели = Запись.DefinitelyMissing379;\n    Запрос = Новый Запрос(\"ВЫБРАТЬ Р.DCommon, Р.DefinitelyMissing379 ИЗ РегистрСведений.BaseRegister КАК Р\");\nКонецПроцедуры";
    let reader_b = b.join("CommonModules/Probe/Ext/Module.bsl");
    let reader_d = d.join("CommonModules/Probe/Ext/Module.bsl");
    let readers = [(READER_B, reader_b, code), (READER_D, reader_d, code)];
    let (fallback, _) = host_for(workspace.clone(), &readers, false);
    let (mut bootstrapped, _) = host_for(workspace, &readers, true);

    let fields = |register: &bsl_metadata::Register| {
        (
            register.dimensions().iter().map(|f| f.name().to_string()).collect::<Vec<_>>(),
            register.resources().iter().map(|f| f.name().to_string()).collect::<Vec<_>>(),
            register.attributes().iter().map(|f| f.name().to_string()).collect::<Vec<_>>(),
        )
    };
    let contains =
        |fields: &[String], expected: &str| fields.iter().any(|field| field.as_str() == expected);
    let c_full = fallback
        .raw_database()
        .resolve_register_for_file(
            READER_D,
            bsl_metadata::MdoType::InformationRegister,
            "BaseRegister",
        )
        .unwrap();
    let c_full_named = fallback
        .raw_database()
        .resolve_register_by_name_for_file(READER_D, "BaseRegister")
        .unwrap();
    let c_typed = bootstrapped
        .raw_database()
        .resolve_register_for_file(
            READER_D,
            bsl_metadata::MdoType::InformationRegister,
            "BaseRegister",
        )
        .unwrap();
    let c_named = bootstrapped
        .raw_database()
        .resolve_register_by_name_for_file(READER_D, "BaseRegister")
        .unwrap();
    let c_all = bootstrapped
        .raw_database()
        .resolve_register_across_roots(bsl_metadata::MdoType::InformationRegister, "BaseRegister")
        .unwrap();
    let c_designer = bootstrapped
        .raw_database()
        .resolve_register_across_designer_roots(
            bsl_metadata::MdoType::InformationRegister,
            "BaseRegister",
        )
        .unwrap();
    assert_eq!(c_typed.as_ref(), c_full.as_ref(), "bootstrap equals full chain fallback");
    assert_eq!(c_full_named.as_ref(), c_full.as_ref(), "fallback typed and by-name lookups agree");
    assert_eq!(c_named.as_ref(), c_typed.as_ref(), "typed and by-name file resolvers agree");
    assert_eq!(
        c_all.as_ref(),
        c_typed.as_ref(),
        "all-root projection uses each declaration owner's scope"
    );
    assert_eq!(
        c_designer.as_ref(),
        c_typed.as_ref(),
        "designer-root projection uses each declaration owner's scope"
    );
    let (dimensions, resources, attributes) = fields(&c_typed);
    for expected in ["Key", "AKey", "BKey", "CKey", "DKey"] {
        assert!(contains(&dimensions, expected), "dimensions={dimensions:?}");
    }
    for expected in ["Amount", "AAmount", "BAmount", "CAmount", "DAmount"] {
        assert!(contains(&resources, expected), "resources={resources:?}");
    }
    for expected in ["BaseAttribute", "AFlag", "BFlag", "CFlag", "DFlag"] {
        assert!(contains(&attributes, expected), "attributes={attributes:?}");
    }
    assert!(c_typed.common_attributes_open(), "each partial Adopted overlay preserves uncertainty");
    let register_a = bootstrapped
        .raw_database()
        .resolve_register_for_file(
            READER_B,
            bsl_metadata::MdoType::InformationRegister,
            "BaseRegister",
        )
        .unwrap();
    let (a_dimensions, a_resources, a_attributes) = fields(&register_a);
    assert!(contains(&a_dimensions, "AKey") && contains(&a_dimensions, "BKey"));
    assert!(!contains(&a_dimensions, "CKey"), "B cannot see C's dependent overlay");
    assert!(
        contains(&a_resources, "Amount")
            && contains(&a_resources, "AAmount")
            && contains(&a_resources, "BAmount")
    );
    assert!(contains(&a_attributes, "AFlag") && contains(&a_attributes, "BFlag"));
    assert!(!contains(&a_attributes, "CFlag"));
    assert!(register_a.common_attributes_open());
    assert!(register_a.common_attributes().iter().any(|field| field.name == "ACommon"));
    assert!(!register_a.common_attributes().iter().any(|field| field.name == "CCommon"));
    let legacy = c_typed.common_attributes();
    for expected in ["LegacyAuto", "LegacyUse", "LegacyIndependent"] {
        assert!(legacy.iter().any(|field| field.name == expected), "legacy={legacy:?}");
    }
    assert!(!legacy.iter().any(|field| field.name == "LegacyDontUse"), "legacy={legacy:?}");
    assert!(legacy.iter().any(|field| field.name == "LegacyAuto"
        && field.attr_type == bsl_metadata::AttributeType::Unknown
        && field.separator));
    assert!(legacy.iter().any(|field| field.name == "LegacyIndependent" && field.separator));
    assert!(register_a.common_attributes().iter().any(|field| field.name == "LegacyUse"));
    assert!(c_typed.common_attributes().iter().any(|field| field.name == "CCommon"));

    // Hidden is independent, so keep it out of the D-chain parity snapshot above. It still
    // sees the main configuration: its valid adopted UUID closes the model and contributes
    // only the fields declared by its own copy of BaseRegister.
    let mut hidden_workspace = WorkspaceConfigsSnapshot::from_paths(vec![
        (None, base.clone()),
        (Some("A".into()), a.clone()),
        (Some("B".into()), b.clone()),
        (Some("C".into()), c.clone()),
        (Some("D".into()), d.clone()),
        (Some("Hidden".into()), hidden.clone()),
    ]);
    hidden_workspace.extension_scopes = vec![
        None,
        Some(bsl_metadata::ExtensionScope::InfoBase),
        None,
        Some(bsl_metadata::ExtensionScope::DataSeparation),
        Some(bsl_metadata::ExtensionScope::InfoBase),
        Some(bsl_metadata::ExtensionScope::InfoBase),
    ];
    hidden_workspace.closures = vec![vec![], vec![], vec![1], vec![1, 2], vec![1, 2, 3], vec![]];
    hidden_workspace.topological_order = vec![0, 1, 2, 3, 4, 5];
    let reader_hidden = hidden.join("CommonModules/Probe/Ext/Module.bsl");
    let hidden_readers = [(READER_CONTROL, reader_hidden, code)];
    let (hidden_host, _) = host_for(hidden_workspace, &hidden_readers, true);
    let hidden_register = hidden_host
        .raw_database()
        .resolve_register_by_name_for_file(READER_CONTROL, "BaseRegister")
        .unwrap();
    assert!(
        hidden_register.dimensions().iter().any(|f| f.name() == "Key"),
        "the main register remains visible to the independent extension"
    );
    assert!(!hidden_register.dimensions().iter().any(|f| f.name() == "AKey"));
    assert!(
        !hidden_register.common_attributes_open(),
        "the matching adopted UUID closes the model"
    );

    assert!(c_typed.common_attributes().iter().any(|field| field.name == "DCommon"));
    let analysis = bootstrapped.analysis();
    let diagnostics = analysis.diagnostics(READER_D, &DiagnosticsConfig::all_enabled());
    assert!(
        !diagnostics.iter().any(|d| d.message.contains("DefinitelyMissing379")),
        "the partial register is open, so unlisted fields cannot be called unresolved"
    );
    assert!(!diagnostics.iter().any(|d| d.message.contains("DCommon")
        && matches!(
            d.code,
            DiagnosticCode::UnresolvedField | DiagnosticCode::UnknownFieldInQuery
        )));
    let offset = code.find("Запись.DCommon").unwrap() as u32 + "Запись.".len() as u32;
    let hover = analysis.hover(READER_D, offset, Locale::Ru).expect("known register field hover");
    assert!(hover.markup.contains("Булево"), "{}", hover.markup);
    drop(analysis);
    let completion_code = "Процедура Подсказка()\n    Набор = РегистрыСведений.BaseRegister.СоздатьНаборЗаписей();\n    Запись = Набор.Добавить();\n    Запись.D\nКонецПроцедуры";
    bootstrapped.raw_database_mut().set_file_text(READER_D, completion_code);
    let completion = bootstrapped.analysis().completions(
        READER_D,
        completion_code.find("Запись.D").unwrap() as u32 + "Запись.D".len() as u32,
        None,
        Locale::Ru,
    );
    let c_flag = completion
        .iter()
        .find(|item| item.label == "DCommon")
        .expect("known register field completion");
    assert!(c_flag.detail.as_deref().unwrap_or_default().contains("Булево"), "{c_flag:?}");
}

#[test]
fn legacy_and_scoped_own_objects_use_their_owner_scope_for_base_area_separators() {
    let fixture = fixture_root();
    let base_fixture = fixture.join("base");
    let no_common_fixture = fixture.join("extensions/no-common");
    let make_area_separators_application_visible = |base: &std::path::Path| {
        for name in ["AreaUse", "AreaNoAuto"] {
            let path = base.join(format!("CommonAttributes/{name}.xml"));
            let xml = fs::read_to_string(&path).unwrap();
            let simultaneous = xml.replace(
                "<SeparatedDataUse>Independently</SeparatedDataUse>",
                "<SeparatedDataUse>IndependentlyAndSimultaneously</SeparatedDataUse>",
            );
            assert_ne!(xml, simultaneous, "{name} fixture precondition changed");
            fs::write(path, simultaneous).unwrap();
        }
    };

    // no-common contains extension-owned objects but no local CommonAttributes/Content.
    // The base fixture has AreaUse and AreaNoAuto as extension-separated area separators.
    // A missing scope uses the old projection and must not infer membership from those base
    // declarations; a scoped DataSeparation owner does, while InfoBase is the negative control.
    for (root_name, scope, inherits_area_separators) in [
        ("Legacy", None, false),
        ("AreaScoped", Some(bsl_metadata::ExtensionScope::DataSeparation), true),
        ("InfoBaseScoped", Some(bsl_metadata::ExtensionScope::InfoBase), false),
    ] {
        let temp = tempfile::tempdir().expect("temporary roots");
        let base = temp.path().join("base");
        let extension = temp.path().join(root_name);
        copy_tree(&base_fixture, &base);
        make_area_separators_application_visible(&base);
        copy_tree(&no_common_fixture, &extension);
        let own_catalog_path = extension.join("Catalogs/NativeCatalog.xml");
        let own_catalog_xml = fs::read_to_string(&own_catalog_path).unwrap();
        fs::write(
            own_catalog_path,
            own_catalog_xml.replacen(
                "<Properties>",
                "<Properties><ObjectBelonging>Own</ObjectBelonging>",
                1,
            ),
        )
        .unwrap();
        let extension_reader = extension.join("CommonModules/Probe/Ext/Module.bsl");
        let code = "Процедура Проверить()\nКонецПроцедуры";
        let readers = [(READER_B, extension_reader, code)];
        let mut workspace = WorkspaceConfigsSnapshot::from_paths(vec![
            (None, base.clone()),
            (Some(root_name.into()), extension.clone()),
        ]);
        workspace.extension_scopes = vec![None, scope];
        workspace.closures = vec![vec![], vec![]];
        workspace.topological_order = vec![0, 1];
        let (fallback, _) = host_for(workspace.clone(), &readers, false);
        let (bootstrapped, _) = host_for(workspace, &readers, true);

        let base_configuration = bootstrapped.raw_database().configuration_for_root(&base);
        let base_set = base_configuration.common_attribute_set();
        for name in ["AreaUse", "AreaNoAuto"] {
            let area = base_set
                .attributes
                .iter()
                .find(|attribute| attribute.name == name)
                .expect("raw base area-separator declaration");
            assert_eq!(area.data_separation, Some(true), "{name}");
            assert_eq!(
                area.configuration_extensions_separation,
                Some(bsl_metadata::common_attribute::ConfigurationExtensionsSeparation::Separate),
                "{name}"
            );
            assert_eq!(
                area.separated_data_use,
                Some(bsl_metadata::common_attribute::SeparatedDataUse::IndependentlyAndSimultaneously),
                "{name} test copy makes inherited field visibility observable"
            );
        }

        let actual = bootstrapped
            .raw_database()
            .resolve_metadata_object_for_file(
                READER_B,
                bsl_metadata::MdoType::Catalog,
                "NativeCatalog",
            )
            .expect("extension-owned catalog");
        assert_eq!(actual.object_belonging(), bsl_metadata::ObjectBelonging::Own);
        let expected = fallback
            .raw_database()
            .resolve_metadata_object_for_file(
                READER_B,
                bsl_metadata::MdoType::Catalog,
                "NativeCatalog",
            )
            .expect("full fallback catalog");
        assert_eq!(actual.as_ref(), expected.as_ref(), "{root_name}: bootstrap/fallback parity");
        assert!(!actual.common_attributes_open, "{root_name}: no local ambiguity");
        for name in ["AreaUse", "AreaNoAuto"] {
            assert_eq!(
                actual.common_attributes.iter().any(|field| field.name == name),
                inherits_area_separators,
                "{root_name}: {name} membership follows this object's owner scope"
            );
        }
    }

    // C reads the same visible base+A+B+C roots through the full fallback, incremental
    // file resolver, all-roots view, and designer-roots view. Distinct Own objects let each
    // declaration retain the scope of its owner: A=InfoBase, B=legacy None, C=DataSeparation.
    let temp = tempfile::tempdir().expect("temporary mixed-scope roots");
    let base = temp.path().join("base");
    let a = temp.path().join("A");
    let b = temp.path().join("B");
    let c = temp.path().join("C");
    copy_tree(&base_fixture, &base);
    make_area_separators_application_visible(&base);
    for root in [&a, &b, &c] {
        copy_reader_extension_root(&no_common_fixture, root, false);
    }
    let native_catalog = fs::read_to_string(no_common_fixture.join("Catalogs/NativeCatalog.xml"))
        .expect("Own catalog source fixture");
    for (root, name, uuid) in [
        (&a, "OwnerA", "a0000000-0000-0000-0000-000000000001"),
        (&b, "OwnerB", "b0000000-0000-0000-0000-000000000001"),
        (&c, "OwnerC", "c0000000-0000-0000-0000-000000000001"),
    ] {
        let own_catalog = native_catalog
            .replace("NativeCatalog", name)
            .replace("399f8343-4746-561e-afd0-5ffac27ea5f7", uuid)
            .replacen("<Properties>", "<Properties><ObjectBelonging>Own</ObjectBelonging>", 1);
        let path = root.join(format!("Catalogs/{name}.xml"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, own_catalog).unwrap();
    }
    write_common_attribute(
        &a,
        "ACommon",
        "Own",
        "Use",
        "<DataSeparation>DontUse</DataSeparation>",
        "",
    );
    write_common_attribute(
        &b,
        "LegacyAuto",
        "Own",
        "Use",
        "<DataSeparation>Separate</DataSeparation>",
        "",
    );
    write_common_attribute(
        &c,
        "CCommon",
        "Own",
        "Use",
        "<DataSeparation>DontUse</DataSeparation>",
        "",
    );

    let roots = vec![
        (None, base.clone()),
        (Some("A".into()), a.clone()),
        (Some("B".into()), b.clone()),
        (Some("C".into()), c.clone()),
    ];
    let mut workspace = WorkspaceConfigsSnapshot::from_paths(roots);
    workspace.extension_scopes = vec![
        None,
        Some(bsl_metadata::ExtensionScope::InfoBase),
        None,
        Some(bsl_metadata::ExtensionScope::DataSeparation),
    ];
    workspace.closures = vec![vec![], vec![], vec![1], vec![1, 2]];
    workspace.topological_order = vec![0, 1, 2, 3];
    let reader = c.join("CommonModules/Probe/Ext/Module.bsl");
    let readers = [(READER_C, reader, "Процедура Проверить()\nКонецПроцедуры")];
    let (fallback, _) = host_for(workspace.clone(), &readers, false);
    let (bootstrapped, _) = host_for(workspace, &readers, true);

    let mixed_base = bootstrapped.raw_database().configuration_for_root(&base);
    for name in ["AreaUse", "AreaNoAuto"] {
        let area = mixed_base
            .common_attribute_set()
            .attributes
            .iter()
            .find(|attribute| attribute.name == name)
            .expect("raw mixed-chain base area separator");
        assert_eq!(area.data_separation, Some(true), "{name}");
        assert_eq!(
            area.configuration_extensions_separation,
            Some(bsl_metadata::common_attribute::ConfigurationExtensionsSeparation::Separate),
            "{name}"
        );
        assert_eq!(
            area.separated_data_use,
            Some(bsl_metadata::common_attribute::SeparatedDataUse::IndependentlyAndSimultaneously),
            "{name}"
        );
    }

    for (name, local_field, expect_area_separators) in
        [("OwnerA", "ACommon", false), ("OwnerB", "LegacyAuto", false), ("OwnerC", "CCommon", true)]
    {
        let actual = bootstrapped
            .raw_database()
            .resolve_metadata_object_for_file(READER_C, bsl_metadata::MdoType::Catalog, name)
            .expect("C sees each dependency-owned Own object");
        let full = fallback
            .raw_database()
            .resolve_metadata_object_for_file(READER_C, bsl_metadata::MdoType::Catalog, name)
            .expect("full fallback resolves the same Own object");
        let all = bootstrapped
            .raw_database()
            .resolve_metadata_object_across_roots(bsl_metadata::MdoType::Catalog, name)
            .expect("all-roots projection");
        let designer = bootstrapped
            .raw_database()
            .resolve_metadata_object_across_designer_roots(bsl_metadata::MdoType::Catalog, name)
            .expect("designer-roots projection");
        assert_eq!(actual.as_ref(), full.as_ref(), "{name}: full fallback parity");
        assert_eq!(actual.object_belonging(), bsl_metadata::ObjectBelonging::Own);
        assert_eq!(all.as_ref(), actual.as_ref(), "{name}: same roots across all-roots view");
        assert_eq!(
            designer.as_ref(),
            actual.as_ref(),
            "{name}: same roots across designer-roots view"
        );
        assert!(actual.common_attributes.iter().any(|field| field.name == local_field));
        assert!(!actual.common_attributes_open, "{name}: complete Own projection");
        for area_field in ["AreaUse", "AreaNoAuto"] {
            assert_eq!(
                actual.common_attributes.iter().any(|field| field.name == area_field),
                expect_area_separators,
                "{name}: inherited area membership follows its declaration owner's scope"
            );
        }
    }
}

#[test]
fn register_overlay_controls_keep_own_unknown_and_other_kinds_separate() {
    let fixture = fixture_root();
    for (case, belonging) in [("own", "Own"), ("unknown", "Unexpected"), ("confirmed", "Adopted")] {
        let temp = tempfile::tempdir().expect("temporary fixture roots");
        let base = temp.path().join("base");
        let extension = temp.path().join("extension");
        copy_tree(&fixture.join("base"), &base);
        copy_tree(&fixture.join("extensions/explicit-use"), &extension);
        let register_path = extension.join("InformationRegisters/BaseRegister.xml");
        let mut xml = fs::read_to_string(&register_path).unwrap();
        xml = xml.replace(
            "<ObjectBelonging>Adopted</ObjectBelonging>",
            &format!("<ObjectBelonging>{belonging}</ObjectBelonging>"),
        );
        if belonging != "Adopted" {
            xml = xml.replace(
                "<ExtendedConfigurationObject>0974fddb-727e-5ee2-bbb2-f9896e3c8862</ExtendedConfigurationObject>",
                "",
            );
        }
        xml = xml.replace(
            "<ChildObjects/>",
            "<ChildObjects><Dimension uuid=\"d0000000-0000-0000-0000-000000000001\"><Properties><Name>LocalOnly</Name><Type><v8:Type>xs:boolean</v8:Type></Type></Properties></Dimension></ChildObjects>",
        );
        fs::write(register_path, xml).unwrap();
        let reader_path = extension.join("CommonModules/Probe/Ext/Module.bsl");
        let code = if case == "confirmed" {
            "Процедура Проверить()\n    Набор = РегистрыСведений.BaseRegister.СоздатьНаборЗаписей();\n    Запись = Набор.Добавить();\n    Ошибка = Запись.MissingCompat379;\n    Запрос = Новый Запрос(\"ВЫБРАТЬ Р.MissingCompat379 ИЗ РегистрСведений.BaseRegister КАК Р\");\nКонецПроцедуры"
        } else {
            "Процедура Проверить()\nКонецПроцедуры"
        };
        let readers = [(READER_B, reader_path, code)];
        let mut workspace = WorkspaceConfigsSnapshot::from_paths(vec![
            (None, base.clone()),
            (Some("E".into()), extension.clone()),
        ]);
        workspace.extension_scopes = vec![None, Some(bsl_metadata::ExtensionScope::InfoBase)];
        workspace.closures = vec![vec![], vec![]];
        workspace.topological_order = vec![0, 1];
        let (fallback, _) = host_for(workspace.clone(), &readers, false);
        let (bootstrapped, _) = host_for(workspace, &readers, true);
        let actual = bootstrapped
            .raw_database()
            .resolve_register_for_file(
                READER_B,
                bsl_metadata::MdoType::InformationRegister,
                "BaseRegister",
            )
            .unwrap();
        let expected = fallback
            .raw_database()
            .resolve_register_for_file(
                READER_B,
                bsl_metadata::MdoType::InformationRegister,
                "BaseRegister",
            )
            .unwrap();
        assert_eq!(actual.as_ref(), expected.as_ref(), "{case}: bootstrap/fallback parity");
        assert!(actual.dimensions().iter().any(|field| field.name() == "LocalOnly"));
        if case == "confirmed" {
            assert!(actual.dimensions().iter().any(|field| field.name() == "Key"));
            assert!(actual.resources().iter().any(|field| field.name() == "Amount"));
            assert!(!actual.common_attributes_open(), "confirmed target UUID remains closed");
            let diagnostics =
                bootstrapped.analysis().diagnostics(READER_B, &DiagnosticsConfig::all_enabled());
            assert!(diagnostics.iter().any(|d| d.code == DiagnosticCode::UnresolvedField
                && d.message.contains("MissingCompat379")));
            assert!(diagnostics.iter().any(|d| d.code == DiagnosticCode::UnknownFieldInQuery
                && d.message.contains("MissingCompat379")));
        } else {
            assert!(
                !actual.dimensions().iter().any(|field| field.name() == "Key"),
                "{case}: independent declarations replace base instead of partial-merging"
            );
        }
    }

    // A different-kind namesake is independent. A later adopted information-register overlay
    // still folds into the base InfoRegister and becomes the effective same-name winner.
    let temp = tempfile::tempdir().expect("temporary fixture roots");
    let base = temp.path().join("base");
    let extension = temp.path().join("kind-extension");
    let later_extension = temp.path().join("later-extension");
    copy_tree(&fixture.join("base"), &base);
    copy_tree(&fixture.join("extensions/explicit-use"), &extension);
    let source = fs::read_to_string(
        fixture.join("extensions/explicit-use/InformationRegisters/BaseRegister.xml"),
    )
    .unwrap();
    fs::remove_file(extension.join("InformationRegisters/BaseRegister.xml")).unwrap();
    let other_kind = source
        .replace("<InformationRegister ", "<AccumulationRegister ")
        .replace("</InformationRegister>", "</AccumulationRegister>")
        .replace("<ObjectBelonging>Adopted</ObjectBelonging>", "<ObjectBelonging>Own</ObjectBelonging>")
        .replace("<ExtendedConfigurationObject>0974fddb-727e-5ee2-bbb2-f9896e3c8862</ExtendedConfigurationObject>", "")
        .replace("<ChildObjects/>", "<ChildObjects><Dimension uuid=\"e0000000-0000-0000-0000-000000000001\"><Properties><Name>OtherKindKey</Name></Properties></Dimension></ChildObjects>");
    let other_kind_path = extension.join("AccumulationRegisters/BaseRegister.xml");
    fs::create_dir_all(other_kind_path.parent().unwrap()).unwrap();
    fs::write(other_kind_path, other_kind).unwrap();
    copy_tree(&fixture.join("extensions/explicit-use"), &later_extension);
    let later_register_path = later_extension.join("InformationRegisters/BaseRegister.xml");
    let later_xml = fs::read_to_string(&later_register_path).unwrap();
    fs::write(
        later_register_path,
        later_xml.replace(
            "<ChildObjects/>",
            "<ChildObjects><Dimension uuid=\"e1000000-0000-0000-0000-000000000001\"><Properties><Name>LaterInfoKey</Name><Type><v8:Type>xs:boolean</v8:Type></Type></Properties></Dimension></ChildObjects>",
        ),
    )
    .unwrap();
    let reader_path = later_extension.join("CommonModules/Probe/Ext/Module.bsl");
    let readers = [(READER_B, reader_path, "Процедура Проверить()\nКонецПроцедуры")];
    let mut workspace = WorkspaceConfigsSnapshot::from_paths(vec![
        (None, base.clone()),
        (Some("E".into()), extension.clone()),
        (Some("F".into()), later_extension.clone()),
    ]);
    workspace.extension_scopes = vec![
        None,
        Some(bsl_metadata::ExtensionScope::InfoBase),
        Some(bsl_metadata::ExtensionScope::InfoBase),
    ];
    workspace.closures = vec![vec![], vec![], vec![1]];
    workspace.topological_order = vec![0, 1, 2];
    let (fallback, _) = host_for(workspace.clone(), &readers, false);
    let (bootstrapped, _) = host_for(workspace, &readers, true);
    let full_typed = fallback
        .raw_database()
        .resolve_register_for_file(
            READER_B,
            bsl_metadata::MdoType::InformationRegister,
            "BaseRegister",
        )
        .unwrap();
    let full_named = fallback
        .raw_database()
        .resolve_register_by_name_for_file(READER_B, "BaseRegister")
        .unwrap();
    let typed = bootstrapped
        .raw_database()
        .resolve_register_for_file(
            READER_B,
            bsl_metadata::MdoType::InformationRegister,
            "BaseRegister",
        )
        .unwrap();
    let named = bootstrapped
        .raw_database()
        .resolve_register_by_name_for_file(READER_B, "BaseRegister")
        .unwrap();
    assert_eq!(
        typed.as_ref(),
        full_typed.as_ref(),
        "full fallback typed lookup selects the later same-kind overlay"
    );
    assert_eq!(
        full_named.as_ref(),
        full_typed.as_ref(),
        "full fallback by-name and typed lookup agree"
    );
    assert_eq!(named.as_ref(), typed.as_ref(), "incremental by-name and typed lookup agree");
    assert_eq!(typed.mdo_type(), bsl_metadata::MdoType::InformationRegister);
    assert!(typed.dimensions().iter().any(|field| field.name() == "Key"));
    assert!(typed.dimensions().iter().any(|field| field.name() == "LaterInfoKey"));
    assert!(!typed.dimensions().iter().any(|field| field.name() == "OtherKindKey"));
    assert_eq!(named.mdo_type(), bsl_metadata::MdoType::InformationRegister);
    let accumulation = bootstrapped
        .raw_database()
        .resolve_register_for_file(
            READER_B,
            bsl_metadata::MdoType::AccumulationRegister,
            "BaseRegister",
        )
        .unwrap();
    assert_eq!(accumulation.mdo_type(), bsl_metadata::MdoType::AccumulationRegister);
    assert!(accumulation.dimensions().iter().any(|field| field.name() == "OtherKindKey"));
}

#[test]
fn refresh_matches_fresh_full_fallback_and_repairs_partial_common_metadata() {
    let temp = tempfile::tempdir().expect("temporary fixture roots");
    let base = temp.path().join("base");
    let extension = temp.path().join("extension");
    let fixture = fixture_root();
    copy_tree(&fixture.join("base"), &base);
    copy_tree(&fixture.join("extensions/explicit-use"), &extension);
    let roots = vec![(None, base.clone()), (Some("A".into()), extension.clone())];
    let mut roots_snapshot = WorkspaceConfigsSnapshot::from_paths(roots.clone());
    roots_snapshot.extension_scopes =
        vec![None, Some(bsl_metadata::ExtensionScope::DataSeparation)];
    let source_path = extension.join("CommonModules/Probe/Ext/Module.bsl");
    let control_code = "Процедура Контроль()\n    Ссылка = Справочники.BaseCatalog.ПустаяСсылка();\n    Ошибка = Ссылка.Missing379;\n    Запрос = Новый Запрос(\"ВЫБРАТЬ Т.Missing379 ИЗ Справочник.BaseCatalog КАК Т\");\nКонецПроцедуры";
    let readers = [
        (READER_B, source_path.clone(), CODE),
        (READER_CONTROL, base.join("CommonModules/Control/Ext/Module.bsl"), control_code),
    ];
    let (mut host, vfs) = host_for(roots_snapshot.clone(), &readers, true);

    let copy_fallback = || host_for(roots_snapshot.clone(), &readers, false).0;
    let assert_matches_fallback = |host: &AnalysisHost| {
        let fallback = copy_fallback();
        let expected_object = fallback
            .raw_database()
            .resolve_metadata_object_for_file(
                READER_B,
                bsl_metadata::MdoType::Catalog,
                "NativeCatalog",
            )
            .expect("fresh whole-config fallback");
        let actual = host
            .raw_database()
            .resolve_metadata_object_for_file(
                READER_B,
                bsl_metadata::MdoType::Catalog,
                "NativeCatalog",
            )
            .expect("bootstrapped object");
        assert_eq!(actual.common_attributes, expected_object.common_attributes);
        assert_eq!(actual.common_attributes_open, expected_object.common_attributes_open);
        let expected_register = fallback
            .raw_database()
            .resolve_register_by_name_for_file(READER_B, "NativeRegister")
            .unwrap();
        let actual_register = host
            .raw_database()
            .resolve_register_by_name_for_file(READER_B, "NativeRegister")
            .unwrap();
        assert_eq!(actual_register.as_ref(), expected_register.as_ref());
        (actual.common_attributes.clone(), actual.common_attributes_open)
    };

    assert_matches_fallback(&host);
    let assert_other_diagnostics = |host: &AnalysisHost, expected: bool| {
        let diagnostics = host.analysis().diagnostics(READER_B, &DiagnosticsConfig::all_enabled());
        for code in [DiagnosticCode::UnresolvedField, DiagnosticCode::UnknownFieldInQuery] {
            assert_eq!(
                diagnostics.iter().any(|d| d.code == code && d.message.contains("OtherUse")),
                expected,
                "diagnostics must reflect the refreshed fields for {code:?}: {diagnostics:?}"
            );
        }
    };
    assert_other_diagnostics(&host, false);

    let other_use_path = extension.join("CommonAttributes/OtherUse.xml");
    let other_use_original = fs::read_to_string(&other_use_path).unwrap();
    fs::write(
        &other_use_path,
        other_use_original.replace("<xr:Use>Use</xr:Use>", "<xr:Use>DontUse</xr:Use>"),
    )
    .unwrap();
    assert!(host.refresh_metadata_substrate(&vfs, std::slice::from_ref(&other_use_path)));
    let (fields, _) = assert_matches_fallback(&host);
    assert!(!fields.iter().any(|field| field.name == "OtherUse"));
    assert_other_diagnostics(&host, true);

    let ordinary_path = base.join("CommonAttributes/OrdinaryAuto.xml");
    let ordinary_original = fs::read_to_string(&ordinary_path).unwrap();
    fs::write(
        &ordinary_path,
        ordinary_original.replace("<v8:Digits>7</v8:Digits>", "<v8:Digits>9</v8:Digits>"),
    )
    .unwrap();
    assert!(host.refresh_metadata_substrate(&vfs, std::slice::from_ref(&ordinary_path)));
    let (fields, _) = assert_matches_fallback(&host);
    assert!(fields.iter().any(|field| {
        field.name == "OrdinaryAuto"
            && field.attr_type == bsl_metadata::AttributeType::Number { precision: 9, scale: 0 }
    }));

    // OtherUse is a separator with independently visible fields in the native fixture.
    let other_use_base = base.join("CommonAttributes/OtherUse.xml");
    let other_use_base_original = fs::read_to_string(&other_use_base).unwrap();
    fs::write(&other_use_path, &other_use_original).unwrap();
    assert!(host.refresh_metadata_substrate(&vfs, std::slice::from_ref(&other_use_path)));
    let (fields, _) = assert_matches_fallback(&host);
    assert!(fields.iter().any(|field| field.name == "OtherUse" && field.separator));
    assert_other_diagnostics(&host, false);

    fs::write(
        &other_use_base,
        other_use_base_original.replace(
            "<DataSeparation>Separate</DataSeparation>",
            "<DataSeparation>DontUse</DataSeparation>",
        ),
    )
    .unwrap();
    assert!(host.refresh_metadata_substrate(&vfs, std::slice::from_ref(&other_use_base)));
    let (fields, _) = assert_matches_fallback(&host);
    assert!(fields.iter().any(|field| field.name == "OtherUse" && !field.separator));

    fs::write(
        &other_use_base,
        other_use_base_original.replace(
            "<SeparatedDataUse>IndependentlyAndSimultaneously</SeparatedDataUse>",
            "<SeparatedDataUse>Independently</SeparatedDataUse>",
        ),
    )
    .unwrap();
    assert!(host.refresh_metadata_substrate(&vfs, std::slice::from_ref(&other_use_base)));
    let (fields, _) = assert_matches_fallback(&host);
    assert!(!fields.iter().any(|field| field.name == "OtherUse"));
    assert_other_diagnostics(&host, true);

    let area_path = base.join("CommonAttributes/AreaUse.xml");
    let area_original = fs::read_to_string(&area_path).unwrap();
    fs::write(
        &area_path,
        area_original.replace(
            "<ConfigurationExtensionsSeparation>Separate</ConfigurationExtensionsSeparation>",
            "<ConfigurationExtensionsSeparation>DontUse</ConfigurationExtensionsSeparation>",
        ),
    )
    .unwrap();
    assert!(host.refresh_metadata_substrate(&vfs, std::slice::from_ref(&area_path)));
    assert_matches_fallback(&host);

    // A broken extension declaration keeps other parsed fields and marks the model open; repair closes it.
    fs::write(&other_use_path, "<broken").unwrap();
    assert!(host.refresh_metadata_substrate(&vfs, std::slice::from_ref(&other_use_path)));
    let (fields, open) = assert_matches_fallback(&host);
    assert!(open, "corrupt common-attribute metadata must remain explicitly incomplete");
    assert!(fields.iter().any(|field| field.name == "OrdinaryAuto"));
    assert!(
        !host
            .raw_database()
            .resolve_metadata_object_for_file(
                READER_CONTROL,
                bsl_metadata::MdoType::Catalog,
                "BaseCatalog"
            )
            .unwrap()
            .common_attributes_open,
        "corruption in A must not open the base control"
    );
    let control_diagnostics =
        host.analysis().diagnostics(READER_CONTROL, &DiagnosticsConfig::all_enabled());
    for code in [DiagnosticCode::UnresolvedField, DiagnosticCode::UnknownFieldInQuery] {
        assert!(
            control_diagnostics.iter().any(|d| d.code == code && d.message.contains("Missing379")),
            "the closed control must retain {code:?}: {control_diagnostics:?}"
        );
    }

    fs::write(&other_use_path, &other_use_original).unwrap();
    assert!(host.refresh_metadata_substrate(&vfs, std::slice::from_ref(&other_use_path)));
    let (_, open) = assert_matches_fallback(&host);
    assert!(!open, "repair must close the model again");

    let adopted_uuid = "<ExtendedConfigurationObject>2f7445ba-531d-576b-85e5-0cbcf74b6444</ExtendedConfigurationObject>";
    let mismatched = other_use_original.replace(adopted_uuid, "<ExtendedConfigurationObject>00000000-0000-0000-0000-000000000000</ExtendedConfigurationObject>");
    fs::write(&other_use_path, &mismatched).unwrap();
    assert!(host.refresh_metadata_substrate(&vfs, std::slice::from_ref(&other_use_path)));
    let (fields, open) = assert_matches_fallback(&host);
    assert!(open, "an adopted declaration with an unconfirmed owner remains incomplete");
    assert!(fields.iter().any(|field| field.name == "OrdinaryAuto"));
    fs::write(&other_use_path, other_use_original).unwrap();
    assert!(host.refresh_metadata_substrate(&vfs, std::slice::from_ref(&other_use_path)));
    let (_, open) = assert_matches_fallback(&host);
    assert!(!open, "restoring the adopted identity closes the model");

    // Removing and restoring a declaration exercises structure refresh as well as content refresh.
    let ordinary_extension = extension.join("CommonAttributes/OrdinaryAuto.xml");
    let ordinary_extension_original = fs::read(&ordinary_extension).unwrap();
    fs::remove_file(&ordinary_extension).unwrap();
    assert!(host.refresh_metadata_substrate(&vfs, std::slice::from_ref(&ordinary_extension)));
    let (fields, _) = assert_matches_fallback(&host);
    assert!(!fields.iter().any(|field| field.name == "OrdinaryAuto"));
    fs::write(&ordinary_extension, ordinary_extension_original).unwrap();
    assert!(host.refresh_metadata_substrate(&vfs, std::slice::from_ref(&ordinary_extension)));
    let (fields, _) = assert_matches_fallback(&host);
    assert!(fields.iter().any(|field| field.name == "OrdinaryAuto"));
}

#[test]
fn source_property_origins_follow_actual_files_before_adopted_overlay() {
    let temp = tempfile::tempdir().expect("source roots");
    let base = temp.path().join("base");
    let extension = temp.path().join("extension");
    for root in [&base, &extension] {
        fs::create_dir_all(root.join("Catalogs")).unwrap();
        fs::write(root.join("Configuration.xml"), "<Configuration/>").unwrap();
    }
    fs::write(
        base.join("Catalogs/BaseFile.xml"),
        include_str!("fixtures/property_origins/base.xml"),
    )
    .unwrap();
    fs::write(
        extension.join("Catalogs/OverlayFile.xml"),
        include_str!("fixtures/property_origins/adopted.xml"),
    )
    .unwrap();
    let workspace = WorkspaceConfigsSnapshot::from_paths(vec![
        (None, base.clone()),
        (Some("SelectedOrigin".into()), extension.clone()),
    ]);
    let (fallback, _) = host_for(workspace.clone(), &[], false);
    let (mut prepared, _) = host_for(workspace, &[], true);
    prepared.raw_database_mut().use_prepared_metadata_inputs();
    let compose = |host: &AnalysisHost| {
        let base = host.raw_database().configuration_for_root(&base);
        let extension = host.raw_database().configuration_for_root(&extension);
        assert_ne!(extension.name(), "SelectedOrigin");
        base.merged_with_extension(
            &extension,
            Some(bsl_metadata::ExtensionScope::InfoBase),
            Some(base.common_attribute_set()),
        )
    };
    let full = compose(&fallback);
    let prepared = compose(&prepared);
    let find = |config: &bsl_metadata::Configuration| {
        config
            .find_metadata_object(bsl_metadata::MdoType::Catalog, "SharedCatalog")
            .unwrap()
            .clone()
    };
    let object = find(&full);
    assert_eq!(object, find(&prepared));
    let field = object
        .read_properties
        .members
        .iter()
        .find(|member| member.path == ["Attribute", "Field"])
        .expect("declared field");
    assert_eq!(
        field.choice.property_origins["ChoiceParameters"],
        format!(":{}", std::path::Path::new("Catalogs").join("BaseFile.xml").display())
    );
    assert_eq!(
        field.choice.property_origins["FillChecking"],
        format!(
            "SelectedOrigin:{}",
            std::path::Path::new("Catalogs").join("OverlayFile.xml").display()
        )
    );
    assert_eq!(field.choice.fill_checking.as_deref(), Some("ShowError"));
    assert!(field.choice.choice_parameters.is_some());

    let unknown_owner = RootDatabaseImpl::new().configuration_for_root(&base);
    let legacy = bsl_metadata::load_from_directory(&base).unwrap();
    for config in [&*unknown_owner, &legacy] {
        let object = find(config);
        assert!(object
            .read_properties
            .members
            .iter()
            .any(|member| { !member.choice.property_origins.is_empty() }));
        assert!(object
            .read_properties
            .members
            .iter()
            .all(|member| { member.choice.property_origins.values().all(String::is_empty) }));
    }
}

fn copy_tree(from: &std::path::Path, to: &std::path::Path) {
    fs::create_dir_all(to).expect("create fixture directory");
    for entry in fs::read_dir(from).expect("read fixture directory") {
        let entry = entry.expect("fixture entry");
        let source = entry.path();
        let target = to.join(entry.file_name());
        if source.is_dir() {
            copy_tree(&source, &target);
        } else {
            fs::copy(source, target).expect("copy fixture file");
        }
    }
}

fn copy_reader_extension_root(
    source: &std::path::Path,
    destination: &std::path::Path,
    adopt_base: bool,
) {
    fs::create_dir_all(destination).expect("create reader extension root");
    fs::copy(source.join("Configuration.xml"), destination.join("Configuration.xml"))
        .expect("copy reader extension configuration");
    if adopt_base {
        for relative in ["Catalogs/BaseCatalog.xml", "InformationRegisters/BaseRegister.xml"] {
            let target = destination.join(relative);
            fs::create_dir_all(target.parent().unwrap()).expect("create adopted object directory");
            fs::copy(source.join(relative), target).expect("copy adopted base object");
        }
    }
}

fn write_common_attribute(
    root: &std::path::Path,
    name: &str,
    belonging: &str,
    auto_use: &str,
    separation: &str,
    content: &str,
) {
    let path = root.join(format!("CommonAttributes/{name}.xml"));
    let uuid = match name {
        "ACommon" => "a1000000-0000-0000-0000-000000000001",
        "CCommon" => "c1000000-0000-0000-0000-000000000001",
        "DCommon" => "d1000000-0000-0000-0000-000000000001",
        "LegacyAuto" => "b1000000-0000-0000-0000-000000000001",
        "LegacyUse" => "b2000000-0000-0000-0000-000000000002",
        "LegacyDontUse" => "b3000000-0000-0000-0000-000000000003",
        "LegacyIndependent" => "b4000000-0000-0000-0000-000000000004",
        _ => unreachable!("test fixture name must have a stable UUID"),
    };
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        path,
        format!(
            r#"<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:xr="http://v8.1c.ru/8.3/xcf/readable" xmlns:v8="http://v8.1c.ru/8.1/data/core" xmlns:xs="http://www.w3.org/2001/XMLSchema">
<CommonAttribute uuid="{uuid}"><Properties>
<ObjectBelonging>{belonging}</ObjectBelonging><Name>{name}</Name><AutoUse>{auto_use}</AutoUse>
{separation}<Type><v8:Type>xs:boolean</v8:Type></Type>{content}
</Properties></CommonAttribute></MetaDataObject>"#
        ),
    )
    .unwrap();
}
