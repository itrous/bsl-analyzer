use hir::{HirDatabase, MetadataKind, TypeKernelDb, TypeKind};
use ide::{Analysis, Diagnostic, DiagnosticCode, DiagnosticsConfig};
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::metadata::{RootKind, WorkspaceConfigsSnapshot};
use ide_db::RootDatabaseImpl;
use std::path::{Path, PathBuf};
use stdx::case::CaseExt;
use vfs::{FileId, FileSet, VfsPath};

struct Files {
    base_object: FileId,
    extension_object: FileId,
    external_object: FileId,
    base_server: FileId,
    extension_server: FileId,
    cached_server: FileId,
    borrowed_cached_server: FileId,
    borrowed_session_cached_server: FileId,
    explicit_unknown_cached_server: FileId,
    disabled_cached_server: FileId,
    enabled_cached_server: FileId,
    named_cached_server: FileId,
    borrowed_form: FileId,
    borrowed_base_form_form: FileId,
    borrowed_unicode_form: FileId,
    own_owner_form: FileId,
    borrowed_ordinary_form: FileId,
    unpaired_form: FileId,
    base_form: FileId,
    base_unicode_form: FileId,
    base_base_form_form: FileId,
    base_own_owner_form: FileId,
    base_ordinary_form: FileId,
}

struct Fixture {
    analysis: Analysis,
    files: Files,
}

fn fixture_root() -> PathBuf {
    PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../bsl-metadata/fixtures/extension_metadata"
    ))
}

fn setup(external_sees_extension: bool) -> Fixture {
    let root = fixture_root();
    let base = root.join("base");
    let extension = root.join("extension");
    let external = root.join("external");
    let files = Files {
        base_object: FileId(0),
        extension_object: FileId(1),
        external_object: FileId(2),
        base_server: FileId(3),
        extension_server: FileId(4),
        cached_server: FileId(5),
        named_cached_server: FileId(6),
        borrowed_cached_server: FileId(7),
        disabled_cached_server: FileId(8),
        enabled_cached_server: FileId(9),
        borrowed_session_cached_server: FileId(10),
        explicit_unknown_cached_server: FileId(11),
        borrowed_form: FileId(12),
        borrowed_base_form_form: FileId(13),
        borrowed_unicode_form: FileId(14),
        own_owner_form: FileId(15),
        borrowed_ordinary_form: FileId(16),
        unpaired_form: FileId(17),
        base_form: FileId(18),
        base_unicode_form: FileId(19),
        base_base_form_form: FileId(20),
        base_own_owner_form: FileId(21),
        base_ordinary_form: FileId(22),
    };
    let form = |root: &Path, owner: &str, name: &str| {
        root.join(format!("Documents/{owner}/Forms/{name}/Ext/Form/Module.bsl"))
    };
    let paths = [
        (files.base_object, base.join("Documents/Заказ/Ext/ObjectModule.bsl")),
        (files.extension_object, extension.join("Documents/Заказ/Ext/ObjectModule.bsl")),
        (files.external_object, external.join("АРМ/Ext/ObjectModule.bsl")),
        (files.base_server, base.join("CommonModules/Сервер/Ext/Module.bsl")),
        (files.extension_server, extension.join("CommonModules/Сервер/Ext/Module.bsl")),
        (files.cached_server, base.join("CommonModules/СерверЗапросов/Ext/Module.bsl")),
        (files.named_cached_server, base.join("CommonModules/СерверПовтИсп/Ext/Module.bsl")),
        (
            files.borrowed_cached_server,
            extension.join("CommonModules/СерверЗапросов/Ext/Module.bsl"),
        ),
        (
            files.borrowed_session_cached_server,
            extension.join("CommonModules/СерверСеанса/Ext/Module.bsl"),
        ),
        (
            files.explicit_unknown_cached_server,
            extension.join("CommonModules/СерверНеизвестный/Ext/Module.bsl"),
        ),
        (
            files.disabled_cached_server,
            extension.join("CommonModules/СерверОтключаемый/Ext/Module.bsl"),
        ),
        (
            files.enabled_cached_server,
            extension.join("CommonModules/СерверВключаемый/Ext/Module.bsl"),
        ),
        (files.borrowed_form, form(&extension, "Заказ", "ФормаДокумента")),
        (files.borrowed_base_form_form, form(&extension, "Заказ", "ФормаСBaseForm")),
        (files.borrowed_unicode_form, form(&extension, "Заказ", "формаёж")),
        (files.own_owner_form, form(&extension, "Локальный", "ФормаДокумента")),
        (files.borrowed_ordinary_form, form(&extension, "Заказ", "ОбычнаяФорма")),
        (files.unpaired_form, form(&extension, "Заказ", "БезПары")),
        (files.base_form, form(&base, "Заказ", "ФормаДокумента")),
        (files.base_unicode_form, form(&base, "Заказ", "ФормаЁж")),
        (files.base_base_form_form, form(&base, "Заказ", "ФормаСBaseForm")),
        (files.base_own_owner_form, form(&base, "Локальный", "ФормаДокумента")),
        (files.base_ordinary_form, form(&base, "Заказ", "ОбычнаяФорма")),
    ];

    let mut db = RootDatabaseImpl::new();
    let mut file_set = FileSet::new();
    for (file_id, path) in &paths {
        file_set.insert(*file_id, VfsPath::new(path.to_string_lossy().as_ref()));
    }
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    for (file_id, path) in &paths {
        db.set_file_source_root(*file_id, SourceRootId(0));
        db.set_file_text(*file_id, &std::fs::read_to_string(path).unwrap());
    }

    let paths = vec![
        (None, base.clone()),
        (Some("Расширение".to_string()), extension.clone()),
        (Some("АРМ".to_string()), external.clone()),
    ];
    db.set_workspace_configs_snapshot(WorkspaceConfigsSnapshot {
        canonical_paths: paths
            .iter()
            .map(|(_, path)| std::fs::canonicalize(path).unwrap_or_else(|_| path.clone()))
            .collect(),
        kinds: vec![
            RootKind::Base,
            RootKind::Extension,
            RootKind::External(bsl_metadata::ExternalObjectKind::DataProcessor),
        ],
        paths,
        closures: vec![vec![], vec![], if external_sees_extension { vec![1] } else { vec![] }],
        topological_order: vec![0, 1, 2],
        fingerprint: None,
        source_exclusions: Default::default(),
    });

    Fixture { analysis: Analysis::from_database(db), files }
}

fn diagnostics(fixture: &Fixture, file: FileId, code: DiagnosticCode) -> Vec<Diagnostic> {
    fixture
        .analysis
        .diagnostics(file, &DiagnosticsConfig::all_enabled())
        .into_iter()
        .filter(|diagnostic| diagnostic.code == code)
        .collect()
}

fn messages(fixture: &Fixture, file: FileId, code: DiagnosticCode) -> Vec<String> {
    diagnostics(fixture, file, code).into_iter().map(|diagnostic| diagnostic.message).collect()
}

fn assert_mentions(messages: &[String], field: &str, expected: bool, context: &str) {
    let folded_field = field.fold_lower();
    assert_eq!(
        messages.iter().any(|message| message.fold_lower().contains(&folded_field)),
        expected,
        "{context}: field {field:?}, diagnostics: {messages:?}"
    );
}

fn assert_field_matrix(
    fixture: &Fixture,
    file: FileId,
    extension_visible: bool,
    has_iteration: bool,
) {
    let unresolved = messages(fixture, file, DiagnosticCode::UnresolvedField);
    let query = messages(fixture, file, DiagnosticCode::UnknownFieldInQuery);

    for field in ["Номенклатура", "Количество"] {
        assert_mentions(&unresolved, field, false, "UnresolvedField");
        assert_mentions(&query, field, false, "UnknownFieldInQuery");
    }
    assert_mentions(&unresolved, "РасшПоле", !extension_visible, "UnresolvedField");
    assert_mentions(&query, "РасшПоле", !extension_visible, "UnknownFieldInQuery");
    assert_mentions(
        &unresolved,
        "НетТакогоПослеДобавить",
        true,
        "UnresolvedField after Добавить()",
    );
    assert_mentions(&unresolved, "НетТакогоВЦикле", has_iteration, "UnresolvedField in iteration");
    assert_mentions(
        &query,
        "НетТакогоВЗапросе",
        true,
        "UnknownFieldInQuery in Документ.Заказ.Товары",
    );
}

fn assert_row_type(fixture: &Fixture, file: FileId, variable: &str, section: &str) {
    let db = fixture.analysis.database();
    let inferred = db.infer(file);
    let ty = inferred
        .var_types
        .get(variable)
        .copied()
        .unwrap_or_else(|| panic!("{variable} must have a tabular-section row type"));
    assert!(
        matches!(
            db.lookup_type(ty),
            TypeKind::MetadataRef(facet)
                if facet.kind == MetadataKind::TabularSectionRow {
                    parent: bsl_metadata::MdoType::Document
                } && stdx::case::eq_ignore_case(facet.name.as_str(), &format!("Заказ.{section}"))
        ),
        "{variable} must be a DocumentTabularSectionRow for Заказ.{section}, got {:?}",
        db.lookup_type(ty)
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn extension_metadata_inference_keeps_rows_typed_for_iteration_and_add() {
    let fixture = setup(true);

    assert_row_type(&fixture, fixture.files.base_object, "строка", "Товары");
    assert_row_type(&fixture, fixture.files.base_object, "контрольнаястрока", "Товары");
    assert_row_type(&fixture, fixture.files.extension_object, "строка", "Товары");
    assert_row_type(&fixture, fixture.files.extension_object, "новаястрока", "Товары");
    assert_row_type(&fixture, fixture.files.extension_object, "новаярасшстрока", "РасшТаблица");
    assert_row_type(&fixture, fixture.files.external_object, "новаястрока", "Товары");
    assert_row_type(&fixture, fixture.files.external_object, "внешняястрока", "Товары");
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn extension_metadata_diagnostics_cover_base_extension_and_external_visibility() {
    let fixture = setup(true);

    assert_field_matrix(&fixture, fixture.files.base_object, false, true);
    assert_field_matrix(&fixture, fixture.files.extension_object, true, true);
    assert_field_matrix(&fixture, fixture.files.external_object, true, true);
    let extension_only =
        messages(&fixture, fixture.files.extension_object, DiagnosticCode::UnresolvedField);
    assert_mentions(&extension_only, "Добавлено", false, "extension-only section field");
    assert_mentions(
        &extension_only,
        "НетТакогоВРасшТаблице",
        true,
        "extension-only section control",
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn extension_metadata_external_without_dependency_keeps_only_base_fields() {
    let fixture = setup(false);

    assert_field_matrix(&fixture, fixture.files.external_object, false, true);
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn extension_metadata_external_dependency_selects_only_named_extension_metadata() {
    let root = fixture_root();
    let base = root.join("base");
    let extension = root.join("extension");
    let dependent = root.join("dependent");
    let external = root.join("external");
    let external_file = FileId(50);
    let external_path = external.join("АРМ/Ext/ObjectModule.bsl");
    let mut db = RootDatabaseImpl::new();
    let mut file_set = FileSet::new();
    file_set.insert(external_file, VfsPath::new(external_path.to_string_lossy().as_ref()));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    db.set_file_source_root(external_file, SourceRootId(0));
    db.set_file_text(external_file, &std::fs::read_to_string(external_path).unwrap());
    let paths = vec![
        (None, base.clone()),
        (Some("Расширение".to_string()), extension.clone()),
        (Some("Зависимое".to_string()), dependent.clone()),
        (Some("АРМ".to_string()), external.clone()),
    ];
    db.set_workspace_configs_snapshot(WorkspaceConfigsSnapshot {
        canonical_paths: paths
            .iter()
            .map(|(_, path)| std::fs::canonicalize(path).unwrap_or_else(|_| path.clone()))
            .collect(),
        kinds: vec![
            RootKind::Base,
            RootKind::Extension,
            RootKind::Extension,
            RootKind::External(bsl_metadata::ExternalObjectKind::DataProcessor),
        ],
        paths,
        closures: vec![vec![], vec![], vec![1], vec![1]],
        topological_order: vec![0, 1, 2, 3],
        fingerprint: None,
        source_exclusions: Default::default(),
    });
    let analysis = Analysis::from_database(db);
    let messages = |code| {
        analysis
            .diagnostics(external_file, &DiagnosticsConfig::all_enabled())
            .into_iter()
            .filter(|diagnostic| diagnostic.code == code)
            .map(|diagnostic| diagnostic.message)
            .collect::<Vec<_>>()
    };
    let unresolved = messages(DiagnosticCode::UnresolvedField);
    let query = messages(DiagnosticCode::UnknownFieldInQuery);
    for diagnostics in [&unresolved, &query] {
        assert!(!diagnostics.iter().any(|message| message.contains("Номенклатура")));
        assert!(!diagnostics.iter().any(|message| message.contains("РасшПоле")));
        assert!(
            diagnostics.iter().any(|message| message.contains("ЗависимоеПоле")),
            "metadata from the excluded dependent extension must stay invisible: {diagnostics:?}"
        );
    }
}

#[test]
fn extension_metadata_common_module_diagnostic_uses_effective_properties() {
    let fixture = setup(true);
    let cached = DiagnosticCode::CommonModuleNameCached;

    assert!(
        diagnostics(&fixture, fixture.files.base_server, cached).is_empty(),
        "base DontUse must be clean"
    );
    assert!(
        diagnostics(&fixture, fixture.files.extension_server, cached).is_empty(),
        "a borrowed module without ReturnValuesReuse inherits base DontUse"
    );
    assert_eq!(
        diagnostics(&fixture, fixture.files.cached_server, cached).len(),
        1,
        "a genuinely cached module without the naming marker is reported"
    );
    assert!(
        diagnostics(&fixture, fixture.files.named_cached_server, cached).is_empty(),
        "the naming marker remains the positive clean control"
    );
    assert_eq!(
        diagnostics(&fixture, fixture.files.borrowed_cached_server, cached).len(),
        1,
        "a borrowed module inherits the base DuringRequest value"
    );
    assert_eq!(
        diagnostics(&fixture, fixture.files.borrowed_session_cached_server, cached).len(),
        1,
        "a borrowed module inherits the base DuringSession value"
    );
    assert_eq!(
        diagnostics(&fixture, fixture.files.explicit_unknown_cached_server, cached).len(),
        1,
        "an explicitly present Unknown remains cached because only DontUse disables caching"
    );
    assert!(
        diagnostics(&fixture, fixture.files.disabled_cached_server, cached).is_empty(),
        "an explicit extension DontUse overrides a cached base"
    );
    assert_eq!(
        diagnostics(&fixture, fixture.files.enabled_cached_server, cached).len(),
        1,
        "an explicit extension DuringRequest overrides base DontUse"
    );
}

fn empty_borrowed_document_xml() -> &'static str {
    r#"<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses">
<Document uuid="15500000-0000-0000-0000-000000000610">
<Properties><Name>Заказ</Name><ObjectBelonging>Adopted</ObjectBelonging><ExtendedConfigurationObject>15500000-0000-0000-0000-000000000010</ExtendedConfigurationObject></Properties>
<ChildObjects><TabularSection uuid="15500000-0000-0000-0000-000000000611">
<Properties><Name>Товары</Name></Properties><ChildObjects/>
</TabularSection></ChildObjects></Document></MetaDataObject>"#
}

fn assert_inherited_base_types(db: &RootDatabaseImpl, file: FileId) {
    let document = db
        .resolve_metadata_object_for_file(file, bsl_metadata::MdoType::Document, "Заказ")
        .unwrap();
    let goods = document.find_tabular_section("Товары").unwrap();
    let field = |name: &str| goods.attributes().iter().find(|field| field.name() == name).unwrap();
    assert_eq!(
        field("Номенклатура").attr_type(),
        &bsl_metadata::AttributeType::String { length: Some(50) }
    );
    assert_eq!(
        field("Количество").attr_type(),
        &bsl_metadata::AttributeType::Number { precision: 10, scale: 0 }
    );
}

fn diagnostic_count(db: &RootDatabaseImpl, file: FileId, code: DiagnosticCode) -> usize {
    ide_diagnostics::file_diagnostics(db, file, &DiagnosticsConfig::all_enabled())
        .into_iter()
        .filter(|diagnostic| diagnostic.code == code)
        .count()
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn extension_metadata_substrate_invalidates_module_diagnostics_and_added_columns() {
    use ide_db::metadata::{CommonModuleEntry, MdoEntry, MetadataListingData};

    let root = fixture_root();
    let base = root.join("base");
    let extension = root.join("extension");
    let base_common_xml = FileId(20);
    let extension_common_xml = FileId(21);
    let extension_common_module = FileId(22);
    let base_document_xml = FileId(23);
    let extension_document_xml = FileId(24);
    let object_module = FileId(25);
    let paths = [
        (base_common_xml, base.join("CommonModules/Сервер.xml")),
        (extension_common_xml, extension.join("CommonModules/Сервер.xml")),
        (extension_common_module, extension.join("CommonModules/Сервер/Ext/Module.bsl")),
        (base_document_xml, base.join("Documents/Заказ.xml")),
        (extension_document_xml, extension.join("Documents/Заказ.xml")),
        (object_module, extension.join("Documents/Заказ/Ext/ObjectModule.bsl")),
    ];
    let mut db = RootDatabaseImpl::new();
    let mut file_set = FileSet::new();
    for (file_id, path) in &paths {
        file_set.insert(*file_id, VfsPath::new(path.to_string_lossy().as_ref()));
    }
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    for (file_id, path) in &paths {
        db.set_file_source_root(*file_id, SourceRootId(0));
        db.set_file_text(*file_id, &std::fs::read_to_string(path).unwrap());
    }
    let object_text = "Процедура Тест()\nДок = Документы.Заказ.СоздатьДокумент();\nСтрока = Док.Товары.Добавить();\nСтрока.ПослеИзменения = 1;\nЗапрос = Новый Запрос(\"ВЫБРАТЬ Т.ПослеИзменения ИЗ Документ.Заказ.Товары КАК Т\");\nКонецПроцедуры";
    db.set_file_text(object_module, object_text);
    db.set_all_config_paths(vec![
        (None, base.clone()),
        (Some("Расширение".to_string()), extension.clone()),
    ]);
    db.set_metadata_listing(
        &base.to_string_lossy(),
        MetadataListingData {
            entries: vec![MdoEntry {
                kind: bsl_metadata::MdoType::Document,
                name: "Заказ".to_string(),
                main: base_document_xml,
                predefined: None,
            }],
            common_modules: vec![CommonModuleEntry {
                name: "Сервер".to_string(),
                main: base_common_xml,
                module_file: None,
                unread_module_file: None,
            }],
            ..Default::default()
        },
    );
    db.set_metadata_listing(
        &extension.to_string_lossy(),
        MetadataListingData {
            entries: vec![MdoEntry {
                kind: bsl_metadata::MdoType::Document,
                name: "Заказ".to_string(),
                main: extension_document_xml,
                predefined: None,
            }],
            common_modules: vec![CommonModuleEntry {
                name: "Сервер".to_string(),
                main: extension_common_xml,
                module_file: Some(extension_common_module),
                unread_module_file: None,
            }],
            ..Default::default()
        },
    );

    assert_eq!(
        diagnostic_count(&db, extension_common_module, DiagnosticCode::CommonModuleNameCached),
        0
    );
    assert_eq!(diagnostic_count(&db, object_module, DiagnosticCode::UnresolvedField), 1);
    assert_eq!(diagnostic_count(&db, object_module, DiagnosticCode::UnknownFieldInQuery), 1);

    let cached_base = std::fs::read_to_string(base.join("CommonModules/Сервер.xml"))
        .unwrap()
        .replace("DontUse", "DuringRequest");
    db.set_file_text(base_common_xml, &cached_base);
    assert_eq!(
        diagnostic_count(&db, extension_common_module, DiagnosticCode::CommonModuleNameCached),
        1
    );

    let disabled_extension =
        std::fs::read_to_string(extension.join("CommonModules/Сервер.xml")).unwrap().replace(
            "<Global>false</Global>",
            "<Global>false</Global><ReturnValuesReuse>DontUse</ReturnValuesReuse>",
        );
    db.set_file_text(extension_common_xml, &disabled_extension);
    assert_eq!(
        diagnostic_count(&db, extension_common_module, DiagnosticCode::CommonModuleNameCached),
        0
    );

    let original_document = std::fs::read_to_string(base.join("Documents/Заказ.xml")).unwrap();
    let added = r#"<Attribute uuid="15500000-0000-0000-0000-000000000098"><Properties><Name>ПослеИзменения</Name><Type><v8:Type>xs:decimal</v8:Type><v8:NumberQualifiers><v8:Digits>12</v8:Digits><v8:FractionDigits>2</v8:FractionDigits></v8:NumberQualifiers></Type></Properties></Attribute>"#;
    let changed_document = original_document.replacen(
        "        </ChildObjects>\n      </TabularSection>",
        &format!("          {added}\n        </ChildObjects>\n      </TabularSection>"),
        1,
    );
    db.set_file_text(base_document_xml, &changed_document);
    assert_eq!(diagnostic_count(&db, object_module, DiagnosticCode::UnresolvedField), 0);
    assert_eq!(
        diagnostic_count(&db, object_module, DiagnosticCode::UnknownFieldInQuery),
        0,
        "the same substrate DB must invalidate its SDBL field scope"
    );
    let row_ty = db.infer(object_module).var_types.get("строка").copied().unwrap();
    assert!(matches!(
        db.lookup_type(row_ty),
        TypeKind::MetadataRef(facet)
            if facet.kind == MetadataKind::TabularSectionRow {
                parent: bsl_metadata::MdoType::Document
            }
    ));

    db.set_file_text(extension_document_xml, empty_borrowed_document_xml());
    assert_inherited_base_types(&db, object_module);
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn extension_metadata_filesystem_fallback_rereads_after_revision_bump() {
    let root = tempfile::tempdir().unwrap();
    let base = root.path().join("base");
    let extension = root.path().join("extension");
    for (target, source) in [
        (
            base.join("CommonModules/Сервер.xml"),
            fixture_root().join("base/CommonModules/Сервер.xml"),
        ),
        (
            extension.join("CommonModules/Сервер.xml"),
            fixture_root().join("extension/CommonModules/Сервер.xml"),
        ),
        (base.join("Documents/Заказ.xml"), fixture_root().join("base/Documents/Заказ.xml")),
        (
            extension.join("Documents/Заказ.xml"),
            fixture_root().join("extension/Documents/Заказ.xml"),
        ),
    ] {
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::copy(source, target).unwrap();
    }
    let base_common_body = base.join("CommonModules/Сервер/Ext/Module.bsl");
    std::fs::create_dir_all(base_common_body.parent().unwrap()).unwrap();
    std::fs::write(&base_common_body, "Процедура База() Экспорт\nКонецПроцедуры").unwrap();
    let common_path = extension.join("CommonModules/Сервер/Ext/Module.bsl");
    let object_path = extension.join("Documents/Заказ/Ext/ObjectModule.bsl");
    std::fs::create_dir_all(common_path.parent().unwrap()).unwrap();
    std::fs::create_dir_all(object_path.parent().unwrap()).unwrap();
    std::fs::write(&common_path, "Процедура Тест() Экспорт\nКонецПроцедуры").unwrap();
    let object_text = "Процедура Тест()\nДок = Документы.Заказ.СоздатьДокумент();\nСтрока = Док.Товары.Добавить();\nСтрока.ПослеИзменения = 1;\nЗапрос = Новый Запрос(\"ВЫБРАТЬ Т.ПослеИзменения ИЗ Документ.Заказ.Товары КАК Т\");\nКонецПроцедуры";
    std::fs::write(&object_path, object_text).unwrap();

    let common_file = FileId(30);
    let object_file = FileId(31);
    let mut db = RootDatabaseImpl::new();
    let mut file_set = FileSet::new();
    file_set.insert(common_file, VfsPath::new(common_path.to_string_lossy().as_ref()));
    file_set.insert(object_file, VfsPath::new(object_path.to_string_lossy().as_ref()));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    for (file_id, text) in
        [(common_file, "Процедура Тест() Экспорт\nКонецПроцедуры"), (object_file, object_text)]
    {
        db.set_file_source_root(file_id, SourceRootId(0));
        db.set_file_text(file_id, text);
    }
    db.set_all_config_paths(vec![
        (None, base.clone()),
        (Some("Расширение".to_string()), extension.clone()),
    ]);

    assert_eq!(diagnostic_count(&db, common_file, DiagnosticCode::CommonModuleNameCached), 0);
    assert_eq!(diagnostic_count(&db, object_file, DiagnosticCode::UnresolvedField), 1);
    assert_eq!(diagnostic_count(&db, object_file, DiagnosticCode::UnknownFieldInQuery), 1);

    let base_common_path = base.join("CommonModules/Сервер.xml");
    let cached =
        std::fs::read_to_string(&base_common_path).unwrap().replace("DontUse", "DuringRequest");
    std::fs::write(&base_common_path, cached).unwrap();
    db.bump_config_for_path(&base);
    assert_eq!(diagnostic_count(&db, common_file, DiagnosticCode::CommonModuleNameCached), 1);

    let extension_common_path = extension.join("CommonModules/Сервер.xml");
    let disabled = std::fs::read_to_string(&extension_common_path).unwrap().replace(
        "<Global>false</Global>",
        "<Global>false</Global><ReturnValuesReuse>DontUse</ReturnValuesReuse>",
    );
    std::fs::write(&extension_common_path, disabled).unwrap();
    db.bump_config_for_path(&extension);
    assert_eq!(diagnostic_count(&db, common_file, DiagnosticCode::CommonModuleNameCached), 0);

    let base_document_path = base.join("Documents/Заказ.xml");
    let original = std::fs::read_to_string(&base_document_path).unwrap();
    let added = r#"<Attribute uuid="15500000-0000-0000-0000-000000000097"><Properties><Name>ПослеИзменения</Name><Type><v8:Type>xs:string</v8:Type></Type></Properties></Attribute>"#;
    let changed = original.replacen(
        "        </ChildObjects>\n      </TabularSection>",
        &format!("          {added}\n        </ChildObjects>\n      </TabularSection>"),
        1,
    );
    std::fs::write(&base_document_path, changed).unwrap();
    db.bump_config_for_path(&base);
    assert_eq!(diagnostic_count(&db, object_file, DiagnosticCode::UnresolvedField), 0);
    assert_eq!(
        diagnostic_count(&db, object_file, DiagnosticCode::UnknownFieldInQuery),
        0,
        "the filesystem revision bump must invalidate the SDBL field scope"
    );

    std::fs::write(extension.join("Documents/Заказ.xml"), empty_borrowed_document_xml()).unwrap();
    db.bump_config_for_path(&extension);
    assert_inherited_base_types(&db, object_file);
}

#[test]
fn extension_metadata_fixture_files_exist_at_the_expected_roots() {
    let root = fixture_root();
    for relative in
        ["base/Documents/Заказ.xml", "extension/Documents/Заказ.xml", "external/АРМ.xml"]
    {
        assert!(Path::new(&root).join(relative).is_file(), "missing fixture file {relative}");
    }
}

fn assert_unresolved_names(fixture: &Fixture, file: FileId, expected: &[(&str, bool)]) {
    let unresolved = messages(fixture, file, DiagnosticCode::UnresolvedName);
    assert_eq!(
        unresolved.len(),
        expected.iter().filter(|(_, reported)| *reported).count(),
        "unexpected UnresolvedName diagnostics for {file:?}: {unresolved:?}"
    );
    for (name, reported) in expected {
        assert_mentions(&unresolved, name, *reported, "UnresolvedName");
    }
}

#[test]
fn borrowed_form_module_resolves_base_form_attributes() {
    use hir::DefDatabase;

    let fixture = setup(true);
    let files = &fixture.files;

    for file in [files.borrowed_form, files.borrowed_base_form_form] {
        assert_unresolved_names(
            &fixture,
            file,
            &[
                ("Объект", false),
                ("БазовыйРеквизитФормы", false),
                ("НетТакогоРеквизитаФормы", true),
            ],
        );
    }
    assert_unresolved_names(
        &fixture,
        files.borrowed_unicode_form,
        &[
            ("Объект", false),
            ("БазовыйРеквизитФормы", false),
            ("СЧЁТЧИК", false),
            ("РасшРеквизитФормы", false),
            ("НетТакогоРеквизитаФормы", true),
        ],
    );
    assert_unresolved_names(
        &fixture,
        files.own_owner_form,
        &[("ЛокальныйРеквизитФормы", false), ("БазовыйРеквизитФормы", true)],
    );
    assert_unresolved_names(
        &fixture,
        files.unpaired_form,
        &[("СвойРеквизитФормы", false), ("БазовыйРеквизитФормы", true)],
    );
    for file in [
        files.base_form,
        files.base_unicode_form,
        files.base_base_form_form,
        files.base_own_owner_form,
    ] {
        assert_unresolved_names(
            &fixture,
            file,
            &[("Объект", false), ("БазовыйРеквизитФормы", false)],
        );
    }
    assert_unresolved_names(
        &fixture,
        files.borrowed_ordinary_form,
        &[("БазовыйРеквизитФормы", true)],
    );
    assert_unresolved_names(
        &fixture,
        files.base_ordinary_form,
        &[("Объект", true), ("БазовыйРеквизитФормы", true)],
    );
    assert!(
        fixture
            .analysis
            .database()
            .module_metadata(hir::ModuleId::new(files.borrowed_ordinary_form))
            .form
            .as_ref()
            .is_some_and(|form| form.is_ordinary() && form.attributes().is_empty()),
        "an ordinary form without a dialog gains no base attributes"
    );
}

#[test]
fn borrowed_form_module_types_inherited_and_overridden_attributes() {
    let fixture = setup(true);
    let db = fixture.analysis.database();
    let var = |file: FileId, name: &str| {
        let ty = db.infer(file).var_types.get(name).copied();
        db.lookup_type(ty.unwrap_or_else(|| panic!("{name} must be typed"))).clone()
    };

    for file in [
        fixture.files.borrowed_form,
        fixture.files.borrowed_base_form_form,
        fixture.files.borrowed_unicode_form,
    ] {
        match var(file, "данные") {
            TypeKind::FormData { kind, underlying: Some(owner) } => {
                assert_eq!(kind, bsl_types::facet::FormDataFacet::Structure);
                assert_eq!(owner.mdo_type, bsl_metadata::MdoType::Document);
                assert_eq!(owner.name.as_str(), "Заказ");
            }
            other => panic!("the inherited main attribute keeps its form-data type, got {other:?}"),
        }
    }
    assert!(
        matches!(var(fixture.files.borrowed_unicode_form, "счёт"), TypeKind::String(..)),
        "the extension's счётчик wins over the base Number"
    );
    assert!(
        matches!(var(fixture.files.base_unicode_form, "число"), TypeKind::Number(..)),
        "the base keeps its own Number"
    );
}
