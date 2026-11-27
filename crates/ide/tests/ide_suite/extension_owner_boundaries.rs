use std::path::Path;

use bsl_metadata::{AttributeType, ExtensionScope, MdoType};
use hir::{Builders, ConfigsDatabase, HirDatabase, MetadataKind, TypeId, TypeKernelDb, TypeKind};
use ide::{
    Analysis, DiagnosticCode, DiagnosticsConfig, SymbolInfoRequest, SymbolInfoSections,
    SymbolPosition,
};
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::metadata::{
    CommonModuleEntry, DefinedTypeEntry, MdoEntry, MetadataListingData, WorkspaceConfigsSnapshot,
};
use ide_db::{RootDatabase, RootDatabaseImpl};
use vfs::{FileId, FileSet, VfsPath};

struct Fixture {
    directory: tempfile::TempDir,
    db: RootDatabaseImpl,
    base: FileId,
    extension: FileId,
    document: FileId,
}

fn uuid(id: u32) -> String {
    format!("00000000-0000-0000-0000-{id:012}")
}

fn object(kind: &str, name: &str, id: u32, adopted: Option<u32>, children: &str) -> String {
    let ownership = adopted.map_or_else(String::new, |base| format!("<ObjectBelonging>Adopted</ObjectBelonging><ExtendedConfigurationObject>{}</ExtendedConfigurationObject>", uuid(base)));
    format!(
        r#"<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:v8="http://v8.1c.ru/8.1/data/core" xmlns:xs="http://www.w3.org/2001/XMLSchema" xmlns:d5p1="http://v8.1c.ru/8.1/data/enterprise/current-config" xmlns:foreign="urn:unrelated" version="2.20"><{kind} uuid="{}"><Properties><Name>{name}</Name>{ownership}</Properties><ChildObjects>{children}</ChildObjects></{kind}></MetaDataObject>"#,
        uuid(id)
    )
}

fn attribute(name: &str, ty: &str, id: u32) -> String {
    let element = if ty.starts_with("d5p1:DefinedType.") { "TypeSet" } else { "Type" };
    format!(
        r#"<Attribute uuid="{}"><Properties><Name>{name}</Name><Type><v8:{element}>{ty}</v8:{element}></Type></Properties></Attribute>"#,
        uuid(id)
    )
}

fn section(name: &str, fields: &str, id: u32) -> String {
    format!(
        r#"<TabularSection uuid="{}"><Properties><Name>{name}</Name></Properties><ChildObjects>{fields}</ChildObjects></TabularSection>"#,
        uuid(id)
    )
}

fn write(root: &Path, relative: &str, contents: &str) {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

fn predefined(name: &str, id: u32) -> String {
    format!(
        r#"<PredefinedData xmlns="http://v8.1c.ru/8.3/xcf/predef"><Item id="{}"><Name>{name}</Name><Code/><Description>{name}</Description><IsFolder>false</IsFolder></Item></PredefinedData>"#,
        uuid(id)
    )
}

fn setup(
    borrow_members: bool,
    scope: ExtensionScope,
    extension_source: &str,
    document_source: &str,
) -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let base = directory.path().join("base");
    let extension = directory.path().join("extension");
    let unrelated = directory.path().join("unrelated");
    for root in [&base, &extension, &unrelated] {
        write(root, "Configuration.xml", "<Configuration/>");
    }
    let base_fields = attribute("БазовыйРеквизит", "xs:string", 11)
        + &section("Товары", &attribute("БазовоеПоле", "xs:decimal", 13), 12);
    write(&base, "Documents/Заказ.xml", &object("Document", "Заказ", 10, None, &base_fields));
    let mut extension_fields = attribute("СвойРеквизит", "xs:boolean", 111)
        + &attribute("СвояСсылка", "d5p1:CatalogRef.Свой", 112)
        + &attribute("СвоеПеречисление", "d5p1:EnumRef.Статусы", 113)
        + &attribute("Определяемый", "d5p1:DefinedType.СвойТип", 114)
        + &attribute("НевидимыйТип", "d5p1:DefinedType.ЧужойТип", 116)
        + &attribute("ЧужоеПространство", "foreign:CatalogRef.Свой", 115);
    if borrow_members {
        extension_fields += &attribute("БазовыйРеквизит", "xs:string", 121);
        extension_fields +=
            &section("Товары", &attribute("СсылкаВСтроке", "d5p1:CatalogRef.Свой", 123), 122);
    }
    write(
        &extension,
        "Documents/Заказ.xml",
        &object("Document", "Заказ", 110, Some(10), &extension_fields),
    );
    for (root, id, field) in [(&extension, 20, "ПолеСвоего"), (&unrelated, 30, "ПолеЧужого")]
    {
        write(
            root,
            "Catalogs/Свой.xml",
            &object("Catalog", "Свой", id, None, &attribute(field, "xs:string", id + 1)),
        );
    }
    write(&unrelated, "Catalogs/Невидимый.xml", &object("Catalog", "Невидимый", 40, None, ""));
    write(&extension, "Enums/Статусы.xml", &object("Enum", "Статусы", 50, None, ""));
    for (root, name, id) in [(&extension, "СвойТип", 60), (&unrelated, "ЧужойТип", 61)]
    {
        let xml = object("DefinedType", name, id, None, "")
            .replace("</Properties>", "<Type><v8:Type>xs:decimal</v8:Type></Type></Properties>");
        write(root, &format!("DefinedTypes/{name}.xml"), &xml);
    }
    for (collection, kind, name, id) in [
        ("ChartsOfAccounts", "ChartOfAccounts", "Счета", 70),
        ("ChartsOfCharacteristicTypes", "ChartOfCharacteristicTypes", "Аналитики", 80),
    ] {
        write(&base, &format!("{collection}/{name}.xml"), &object(kind, name, id, None, ""));
        write(
            &extension,
            &format!("{collection}/{name}.xml"),
            &object(kind, name, id + 100, Some(id), ""),
        );
        write(
            &base,
            &format!("{collection}/{name}/Ext/Predefined.xml"),
            &predefined("Базовый", id + 1),
        );
        write(
            &extension,
            &format!("{collection}/{name}/Ext/Predefined.xml"),
            &predefined("Добавленный", id + 101),
        );
    }
    let common = |name: &str, id: u32, adopted| {
        object("CommonModule", name, id, adopted, "")
            .replace("</Properties>", "<Server>true</Server><Global>false</Global></Properties>")
    };
    write(&base, "CommonModules/Основной.xml", &common("Основной", 90, None));
    write(&extension, "CommonModules/Основной.xml", &common("Основной", 190, Some(90)));
    write(&extension, "CommonModules/СервисРасширения.xml", &common("СервисРасширения", 91, None));
    write(&unrelated, "CommonModules/ЧужойСервис.xml", &common("ЧужойСервис", 92, None));
    let modules = [
        (&base, "CommonModules/Основной/Ext/Module.bsl", "Функция Цель()\nЗапросКопии = Новый Запрос(\"ВЫБРАТЬ Т.НетПоляКопии ИЗ Документ.Заказ.Товары КАК Т\");\nВозврат 0;\nКонецФункции\nФункция ОбщийПомощник()\nВозврат 1;\nКонецФункции\nФункция КонтрольБазы()\nЗапросБазы = Новый Запрос(\"ВЫБРАТЬ Т.НетПоляБазы ИЗ Документ.Заказ.Товары КАК Т\");\nВозврат Справочники.Свой.ПустаяСсылка();\nКонецФункции"),
        (&extension, "CommonModules/Основной/Ext/Module.bsl", extension_source),
        (&extension, "Documents/Заказ/Ext/ObjectModule.bsl", document_source),
        (&extension, "CommonModules/СервисРасширения/Ext/Module.bsl", "Функция Значение() Экспорт\nВозврат Истина;\nКонецФункции"),
        (&unrelated, "CommonModules/ЧужойСервис/Ext/Module.bsl", "Функция Значение() Экспорт\nВозврат 42;\nКонецФункции"),
    ];
    let mut db = RootDatabaseImpl::new();
    let mut files = FileSet::new();
    for (index, (root, relative, source)) in modules.iter().enumerate() {
        write(root, relative, source);
        files.insert(
            FileId(index as u32),
            VfsPath::new(root.join(relative).to_string_lossy().as_ref()),
        );
    }
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(files));
    for (index, (_, _, source)) in modules.iter().enumerate() {
        let file = FileId(index as u32);
        db.set_file_source_root(file, SourceRootId(0));
        db.set_file_text(file, source);
    }
    db.set_all_config_paths(
        vec![
            (None, base),
            (Some("СвоеРасширение".to_owned()), extension),
            (Some("Независимое".to_owned()), unrelated),
        ],
        Some(scope),
    );
    Fixture { directory, db, base: FileId(0), extension: FileId(1), document: FileId(2) }
}

fn messages(fixture: &Fixture, file: FileId, code: DiagnosticCode) -> Vec<String> {
    Analysis::from_database(fixture.db.clone())
        .diagnostics(file, &DiagnosticsConfig::all_enabled())
        .into_iter()
        .filter(|diagnostic| diagnostic.code == code)
        .map(|diagnostic| diagnostic.message)
        .collect()
}

fn assert_query_ranges(fixture: &Fixture) {
    let reports: Vec<_> = Analysis::from_database(fixture.db.clone())
        .diagnostics(fixture.extension, &DiagnosticsConfig::all_enabled())
        .into_iter()
        .filter(|diagnostic| diagnostic.code == DiagnosticCode::UnknownFieldInQuery)
        .collect();
    assert_eq!(reports.len(), 2, "{reports:?}");
    let mut fields: Vec<_> = reports
        .iter()
        .map(|diagnostic| {
            &INSERTED[usize::from(diagnostic.range.start())..usize::from(diagnostic.range.end())]
        })
        .collect();
    fields.sort_unstable();
    assert_eq!(fields, ["ОбычнаяОшибка", "ПолеЧужого"]);
}

fn bootstrap_metadata(fixture: &mut Fixture) {
    use bsl_metadata::loader::{
        discover_common_module_structure, discover_defined_type_structure,
        discover_metadata_structure,
    };

    let bsl = fixture.db.source_root_input(SourceRootId(0)).root(&fixture.db).file_set().clone();
    let mut xml = FileSet::new();
    let mut inputs = Vec::new();
    let mut listings = Vec::new();
    let mut enroll = |path: &Path| {
        let vfs_path = VfsPath::new(path.to_string_lossy().as_ref());
        if let Some(id) = xml.file_for_path(&vfs_path).copied() {
            return id;
        }
        let id = FileId(100 + inputs.len() as u32);
        xml.insert(id, vfs_path);
        inputs.push((id, std::fs::read_to_string(path).unwrap()));
        id
    };
    for name in ["base", "extension", "unrelated"] {
        let root = fixture.directory.path().join(name);
        let entries = discover_metadata_structure(&root, &bsl_conventions::RealFs)
            .into_iter()
            .map(|entry| MdoEntry {
                kind: entry.mdo_type,
                name: entry.name,
                main: enroll(&entry.main),
                predefined: entry.predefined.as_deref().map(&mut enroll),
            })
            .collect();
        let defined_types = discover_defined_type_structure(&root, &bsl_conventions::RealFs)
            .into_iter()
            .map(|entry| DefinedTypeEntry { name: entry.name, main: enroll(&entry.main) })
            .collect();
        let common_modules = discover_common_module_structure(&root, &bsl_conventions::RealFs)
            .into_iter()
            .map(|entry| CommonModuleEntry {
                name: entry.name,
                main: enroll(&entry.main),
                module_file: entry.module_file.and_then(|path| {
                    bsl.file_for_path(&VfsPath::new(path.to_string_lossy().as_ref())).copied()
                }),
                unread_module_file: None,
            })
            .collect();
        listings.push((
            root,
            MetadataListingData {
                entries,
                defined_types,
                common_modules,
                ..MetadataListingData::default()
            },
        ));
    }
    fixture.db.set_source_root(SourceRootId(1), SourceRoot::new_metadata(xml));
    for (id, text) in inputs {
        fixture.db.set_file_source_root(id, SourceRootId(1));
        fixture.db.set_file_text(id, &text);
    }
    for (root, listing) in listings {
        fixture.db.set_metadata_listing(&root.to_string_lossy(), listing);
    }
}

fn effective(fixture: &Fixture) -> std::sync::Arc<hir::InferenceResult> {
    let target = ide_db::effective_target(&fixture.db, fixture.extension)
        .expect("extension must pair with its base");
    hir::infer_effective(&fixture.db, target)
}

fn ty(result: &hir::InferenceResult, name: &str) -> TypeId {
    *result
        .var_types
        .get(name)
        .unwrap_or_else(|| panic!("missing local {name}: {:?}", result.var_types))
}

const INSERTED: &str = r#"&ИзменениеИКонтроль("Цель")
Функция Расш_Цель()
    ЗапросКопии = Новый Запрос("ВЫБРАТЬ Т.НетПоляКопии ИЗ Документ.Заказ.Товары КАК Т");
#Вставка
    Локальный = СвойПомощник();
    Базовый = ОбщийПомощник();
    ОбщийМодуль = СервисРасширения.Значение();
    Справочник = Справочники.Свой.ПустаяСсылка();
    Субконто = ПланыВидовХарактеристик.Аналитики.Добавленный;
    Счет = ПланыСчетов.Счета.Добавленный;
    БазовыйСчет = ПланыСчетов.Счета.Базовый;
    Объект = Справочники.Свой.СоздатьЭлемент();
    СвоеПоле = Объект.ПолеСвоего;
    ЧужоеПоле = Объект.ПолеЧужого;
    ЗапросВставки = Новый Запрос("ВЫБРАТЬ Т.ПолеСвоего, Т.ПолеЧужого ИЗ Справочник.Свой КАК Т");
    НетСправочника = Справочники.Невидимый.ПустаяСсылка();
    НетПолучателя = ЧужойСервис.Значение();
    НесуществующийПомощник();
#КонецВставки
    Возврат 0;
КонецФункции
Функция СвойПомощник()
    Возврат "свой";
КонецФункции
Функция ОбщийПомощник()
    Возврат "расширение";
КонецФункции
Функция НеиспользуемыйПомощник()
    Возврат 42;
КонецФункции
Процедура Обычная()
    ОбычныйСправочник = Справочники.Свой.ПустаяСсылка();
    ОбычныйСервис = СервисРасширения.Значение();
    ОбычныйПомощник = СвойПомощник();
    ЗапросОбычный = Новый Запрос("ВЫБРАТЬ Т.ОбычнаяОшибка ИЗ Документ.Заказ.Товары КАК Т");
КонецПроцедуры"#;

#[test]
fn inserted_calls_keep_extension_owner_and_base_sibling_precedence() {
    let fixture = setup(false, ExtensionScope::InfoBase, INSERTED, "");
    let result = effective(&fixture);
    assert_eq!(ty(&result, "локальный"), fixture.db.string(None, false));
    assert_eq!(ty(&result, "базовый"), fixture.db.number(None, None));
    assert_eq!(ty(&result, "общиймодуль"), fixture.db.boolean());
    for code in [
        DiagnosticCode::UnresolvedBareCall,
        DiagnosticCode::UnresolvedName,
        DiagnosticCode::UnusedLocalMethod,
    ] {
        let reports = messages(&fixture, fixture.extension, code);
        assert!(reports.iter().all(|message| !message.contains("СвойПомощник")), "{reports:?}");
    }
    let unused = messages(&fixture, fixture.extension, DiagnosticCode::UnusedLocalMethod);
    assert!(unused.iter().any(|message| message.contains("НеиспользуемыйПомощник")), "{unused:?}");
    let unknown = messages(&fixture, fixture.extension, DiagnosticCode::UnresolvedName);
    assert!(
        unknown.iter().any(|message| message.contains("НесуществующийПомощник")),
        "{unknown:?}"
    );
    let receivers = messages(&fixture, fixture.extension, DiagnosticCode::UnresolvedMethodCall);
    assert!(receivers.iter().all(|message| !message.contains("СервисРасширения")), "{receivers:?}");
    assert!(unknown.iter().any(|message| message.contains("ЧужойСервис")), "{unknown:?}");
}

#[test]
fn inserted_catalogs_and_predefined_entries_use_only_the_visible_root() {
    for scope in [ExtensionScope::InfoBase, ExtensionScope::DataSeparation] {
        let fixture = setup(false, scope, INSERTED, "");
        let result = effective(&fixture);
        let ordinary = fixture.db.infer(fixture.extension);
        assert_eq!(ty(&ordinary, "обычныйсервис"), fixture.db.boolean());
        assert_eq!(ty(&ordinary, "обычныйпомощник"), fixture.db.string(None, false));
        assert!(
            matches!(fixture.db.lookup_type(ty(&result, "справочник")), TypeKind::MetadataRef(facet) if facet.kind == MetadataKind::CatalogRef && facet.name.as_str() == "Свой")
        );
        for (local, kind) in [
            ("субконто", MetadataKind::ChartOfCharacteristicTypesRef),
            ("счет", MetadataKind::ChartOfAccountsRef),
            ("базовыйсчет", MetadataKind::ChartOfAccountsRef),
        ] {
            assert!(
                matches!(fixture.db.lookup_type(ty(&result, local)), TypeKind::MetadataRef(facet) if facet.kind == kind)
            );
        }
        let fields = messages(&fixture, fixture.extension, DiagnosticCode::UnresolvedField);
        assert_eq!(fields.len(), 2, "{scope:?}: {fields:?}");
        assert!(fields.iter().any(|message| message.contains("ПолеЧужого")));
        assert!(fields.iter().any(|message| message.contains("Невидимый")));
        assert_query_ranges(&fixture);
        let target = ide_db::effective_target(&fixture.db, fixture.extension).unwrap();
        let queries = hir::all_sdbl_in_effective_file(&fixture.db, target);
        let identities: std::collections::HashSet<_> = queries.iter().map(|(id, _)| *id).collect();
        assert_eq!(identities.len(), queries.len());
        assert_eq!(queries.len(), 3);
        let base_queries = messages(&fixture, fixture.base, DiagnosticCode::UnknownFieldInQuery);
        assert_eq!(base_queries.len(), 2, "{base_queries:?}");
        assert!(base_queries.iter().any(|query| query.contains("НетПоляБазы")));
        assert!(base_queries.iter().any(|query| query.contains("НетПоляКопии")));
        let base = messages(&fixture, fixture.base, DiagnosticCode::UnresolvedField);
        assert_eq!(base.len(), 1, "{base:?}");
        assert!(base[0].contains("Свой"), "{base:?}");
    }
}

const LEXICAL: &str = r#"Процедура Проверить()
    БазовоеИмя = БазовыйРеквизит;
    ТабличноеИмя = Товары;
    СвоеИмя = СвойРеквизит;
    ЧерезОбъект = ЭтотОбъект.БазовыйРеквизит;
    СтрокаЧерезОбъект = ЭтотОбъект.Товары.Добавить();
    ПолеЧерезОбъект = СтрокаЧерезОбъект.БазовоеПоле;
    НеверноеПоле = ЭтотОбъект.НетРеквизита;
    Запрос = Новый Запрос("ВЫБРАТЬ Т.БазовоеПоле, Т.НетТакогоПоля ИЗ Документ.Заказ.Товары КАК Т");
КонецПроцедуры"#;

#[test]
fn unborrowed_bare_members_are_separate_from_the_effective_object_and_query() {
    for borrowed in [false, true] {
        let fixture = setup(borrowed, ExtensionScope::InfoBase, "", LEXICAL);
        let unresolved = messages(&fixture, fixture.document, DiagnosticCode::UnresolvedName);
        assert_eq!(unresolved.len(), if borrowed { 0 } else { 2 }, "{borrowed}: {unresolved:?}");
        if !borrowed {
            assert!(unresolved.iter().any(|message| message.contains("БазовыйРеквизит")));
            assert!(unresolved.iter().any(|message| message.contains("Товары")));
        }
        let inferred = fixture.db.infer(fixture.document);
        let implicit = hir::module_implicit_fields(&fixture.db, fixture.document);
        for name in ["БазовыйРеквизит", "Товары"] {
            assert_eq!(
                implicit.iter().any(|field| field.name.as_str() == name),
                borrowed,
                "{name}: {implicit:?}"
            );
        }
        assert!(implicit.iter().any(|field| field.name.as_str() == "СвойРеквизит"));
        assert_eq!(ty(&inferred, "черезобъект"), fixture.db.string(None, false));
        assert_eq!(ty(&inferred, "полечерезобъект"), fixture.db.number(None, None));
        assert_eq!(ty(&inferred, "своеимя"), fixture.db.boolean());
        let fields = messages(&fixture, fixture.document, DiagnosticCode::UnresolvedField);
        assert_eq!(fields.len(), 1, "{fields:?}");
        assert!(fields[0].contains("НетРеквизита"));
        let query = messages(&fixture, fixture.document, DiagnosticCode::UnknownFieldInQuery);
        assert_eq!(query.len(), 1, "{query:?}");
        assert!(query[0].contains("НетТакогоПоля"));
    }
}

#[test]
fn declared_dependency_is_visible_without_merging_independent_same_named_objects() {
    let mut fixture = setup(false, ExtensionScope::InfoBase, INSERTED, "");
    let root = fixture.directory.path();
    let mut snapshot = WorkspaceConfigsSnapshot::from_paths(vec![
        (None, root.join("base")),
        (Some("СвоеРасширение".to_owned()), root.join("extension")),
        (Some("Независимое".to_owned()), root.join("unrelated")),
    ]);
    snapshot.closures = vec![vec![], vec![2], vec![]];
    snapshot.topological_order = vec![0, 2, 1];
    snapshot.extension_scopes =
        vec![None, Some(ExtensionScope::InfoBase), Some(ExtensionScope::InfoBase)];
    fixture.db.set_workspace_configs_snapshot(snapshot);
    let result = effective(&fixture);
    assert_eq!(ty(&result, "нетполучателя"), fixture.db.number(None, None));
    let fields = messages(&fixture, fixture.extension, DiagnosticCode::UnresolvedField);
    assert_eq!(fields.len(), 1, "{fields:?}");
    assert!(fields[0].contains("ПолеЧужого"));
    assert_query_ranges(&fixture);
    assert!(messages(&fixture, fixture.extension, DiagnosticCode::UnresolvedMethodCall).is_empty());
}

#[test]
fn aliased_extension_types_agree_in_metadata_symbol_cards_and_inference() {
    let source = r#"Процедура Проверить()
    Каталог = ЭтотОбъект.СвояСсылка;
    Статус = ЭтотОбъект.СвоеПеречисление;
    Число = ЭтотОбъект.Определяемый;
    Неизвестный = ЭтотОбъект.ЧужоеПространство;
    Невидимый = ЭтотОбъект.НевидимыйТип;
    Строка = ЭтотОбъект.Товары.Добавить();
    СсылкаСтроки = Строка.СсылкаВСтроке;
    Запрос = Новый Запрос("ВЫБРАТЬ Т.СсылкаВСтроке ИЗ Документ.Заказ.Товары КАК Т");
    ЗапросОбъекта = Новый Запрос("ВЫБРАТЬ Т.СвояСсылка, Т.СвоеПеречисление, Т.Определяемый, Т.ЧужоеПространство, Т.НевидимыйТип ИЗ Документ.Заказ КАК Т");
КонецПроцедуры"#;
    let mut fixture = setup(true, ExtensionScope::InfoBase, "", source);
    bootstrap_metadata(&mut fixture);
    let object =
        fixture.db.resolve_metadata_object(fixture.document, MdoType::Document, "Заказ").unwrap();
    assert!(
        matches!(&object.find_attribute("СвояСсылка").unwrap().attr_type, AttributeType::Ref { mdo_type: MdoType::Catalog, name } if name == "Свой")
    );
    assert!(matches!(
        &object.find_attribute("ЧужоеПространство").unwrap().attr_type,
        AttributeType::UnknownNamed(_)
    ));
    assert!(
        matches!(&object.find_attribute("Определяемый").unwrap().attr_type, AttributeType::DefinedType { name } if name == "СвойТип")
    );
    let inferred = fixture.db.infer(fixture.document);
    for (local, kind, owner) in [
        ("каталог", MetadataKind::CatalogRef, "Свой"),
        ("статус", MetadataKind::EnumRef, "Статусы"),
        ("ссылкастроки", MetadataKind::CatalogRef, "Свой"),
    ] {
        assert!(
            matches!(fixture.db.lookup_type(ty(&inferred, local)), TypeKind::MetadataRef(facet) if facet.kind == kind && facet.name.as_str() == owner)
        );
    }
    assert_eq!(ty(&inferred, "число"), fixture.db.number(None, None));
    assert!(!inferred.var_types.contains_key("неизвестный"));
    assert!(!inferred.var_types.contains_key("невидимый"));
    let card = ide::symbol_info(
        &fixture.db,
        &SymbolInfoRequest {
            symbol: Some("Документ.Заказ.СвояСсылка".to_owned()),
            position: Some(SymbolPosition { file_id: fixture.document, line: 0, column: 0 }),
            locale: ide::Locale::default(),
            sections: SymbolInfoSections::all(),
            workspace_root: None,
        },
    )
    .expect("extension attribute must have a symbol card");
    assert!(card.return_type.as_ref().is_some_and(|ty| ty.contains("Свой")), "{card:?}");
    assert!(messages(&fixture, fixture.document, DiagnosticCode::UnknownFieldInQuery).is_empty());
    let packages = fixture.db.sdbl_hir_in_file(fixture.document);
    let fields: Vec<_> = packages
        .iter()
        .flat_map(|(_, package)| package.queries())
        .flat_map(|query| &query.hir.select.fields)
        .map(|field| &field.ty)
        .collect();
    assert_eq!(fields.len(), 6);
    for (index, kind, name) in [
        (0, MdoType::Catalog, "Свой"),
        (1, MdoType::Catalog, "Свой"),
        (2, MdoType::Enum, "Статусы"),
    ] {
        assert!(
            matches!(fields[index], sdbl_hir::SdblType::Ref(reference) if reference.mdo_type == kind && reference.name == name)
        );
    }
    assert!(
        matches!(fields[3], sdbl_hir::SdblType::DefinedType { name, underlying_type: Some(underlying) } if name == "СвойТип" && matches!(underlying.as_ref(), sdbl_hir::SdblType::Number { .. }))
    );
    assert_eq!(*fields[4], sdbl_hir::SdblType::Unknown);
    assert!(
        matches!(fields[5], sdbl_hir::SdblType::DefinedType { name, underlying_type: None } if name == "ЧужойТип")
    );
}
