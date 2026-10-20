//! `UnresolvedMetadataPath`: metadata named by the string argument of a platform
//! function (`Тип`, `ПредопределенноеЗначение`, `ОткрытьФорму`, `ПолучитьФорму`,
//! `ОткрытьФормуМодально`), judged against a synthetic configuration on disk.
//!
//! The code is matched by name, so the file builds where the rule does not exist and
//! shows what it adds.

use std::path::Path;

use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use ide_diagnostics::DiagnosticsConfig;
use vfs::{FileId, FileSet, VfsPath};

const CODE: &str = "UnresolvedMetadataPath";

const HEADER: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:v8="http://v8.1c.ru/8.1/data/core" xmlns:xs="http://www.w3.org/2001/XMLSchema" version="2.20">"#;

fn write(root: &Path, relative: &str, text: &str) {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().expect("fixture file has a parent"))
        .expect("create fixture directory");
    std::fs::write(path, text).expect("write fixture file");
}

/// A catalog with one predefined item, an enum with one value, an information
/// register, a data processor and the common module under test.
fn write_configuration(root: &Path) {
    write(
        root,
        "Configuration.xml",
        &format!(
            r#"{HEADER}
    <Configuration uuid="5a5a0000-0000-0000-0000-000000000001">
        <Properties>
            <Name>Синтетика</Name>
        </Properties>
        <ChildObjects>
            <CommonModule>Тест</CommonModule>
            <Catalog>Товары</Catalog>
            <Enum>Цвета</Enum>
            <InformationRegister>Курсы</InformationRegister>
            <DataProcessor>Загрузка</DataProcessor>
        </ChildObjects>
    </Configuration>
</MetaDataObject>"#
        ),
    );
    write(
        root,
        "CommonModules/Тест.xml",
        &format!(
            r#"{HEADER}
    <CommonModule uuid="5a5a0000-0000-0000-0000-000000000002">
        <Properties>
            <Name>Тест</Name>
            <ClientManagedApplication>true</ClientManagedApplication>
            <Server>true</Server>
        </Properties>
    </CommonModule>
</MetaDataObject>"#
        ),
    );
    write(
        root,
        "Catalogs/Товары.xml",
        &format!(
            r#"{HEADER}
    <Catalog uuid="5a5a0000-0000-0000-0000-000000000003">
        <Properties>
            <Name>Товары</Name>
        </Properties>
    </Catalog>
</MetaDataObject>"#
        ),
    );
    write(
        root,
        "Catalogs/Товары/Ext/Predefined.xml",
        r#"<?xml version="1.0" encoding="UTF-8"?>
<PredefinedData xmlns="http://v8.1c.ru/8.3/xcf/predef" version="2.20">
    <Item id="5a5a0000-0000-0000-0000-000000000004">
        <Name>Основной</Name>
        <Code/>
        <Description>Основной</Description>
        <IsFolder>false</IsFolder>
    </Item>
</PredefinedData>"#,
    );
    write(
        root,
        "Enums/Цвета.xml",
        &format!(
            r#"{HEADER}
    <Enum uuid="5a5a0000-0000-0000-0000-000000000005">
        <Properties>
            <Name>Цвета</Name>
        </Properties>
        <ChildObjects>
            <EnumValue uuid="5a5a0000-0000-0000-0000-000000000006">
                <Properties>
                    <Name>Красный</Name>
                </Properties>
            </EnumValue>
        </ChildObjects>
    </Enum>
</MetaDataObject>"#
        ),
    );
    write(
        root,
        "InformationRegisters/Курсы.xml",
        &format!(
            r#"{HEADER}
    <InformationRegister uuid="5a5a0000-0000-0000-0000-000000000007">
        <Properties>
            <Name>Курсы</Name>
            <InformationRegisterPeriodicity>Nonperiodical</InformationRegisterPeriodicity>
            <WriteMode>Independent</WriteMode>
        </Properties>
    </InformationRegister>
</MetaDataObject>"#
        ),
    );
    write(
        root,
        "DataProcessors/Загрузка.xml",
        &format!(
            r#"{HEADER}
    <DataProcessor uuid="5a5a0000-0000-0000-0000-000000000008">
        <Properties>
            <Name>Загрузка</Name>
        </Properties>
    </DataProcessor>
</MetaDataObject>"#
        ),
    );
}

/// An extension of the configuration that adds the catalog `Услуги`.
fn write_extension(root: &Path) {
    write(
        root,
        "Configuration.xml",
        &format!(
            r#"{HEADER}
    <Configuration uuid="5a5a0000-0000-0000-0000-000000000011">
        <Properties>
            <Name>Расширение</Name>
        </Properties>
        <ChildObjects>
            <Catalog>Услуги</Catalog>
        </ChildObjects>
    </Configuration>
</MetaDataObject>"#
        ),
    );
    write(
        root,
        "Catalogs/Услуги.xml",
        &format!(
            r#"{HEADER}
    <Catalog uuid="5a5a0000-0000-0000-0000-000000000012">
        <Properties>
            <Name>Услуги</Name>
        </Properties>
    </Catalog>
</MetaDataObject>"#
        ),
    );
}

/// The messages of the rule for `source` as the module of the common module `Тест`.
fn reported(source: &str) -> Vec<String> {
    reported_in(source, false)
}

/// [`reported`], with an extension that adds the catalog `Услуги` registered beside
/// the configuration.
fn reported_in(source: &str, with_extension: bool) -> Vec<String> {
    findings_in(source, with_extension, true).into_iter().map(|(message, _)| message).collect()
}

/// The messages of the rule with the source text each one points at; `loaded` is
/// whether the host has finished loading the workspace.
fn findings_in(source: &str, with_extension: bool, loaded: bool) -> Vec<(String, String)> {
    let dir = tempfile::tempdir().expect("create fixture directory");
    let root = dir.path().join("cf");
    let root = root.as_path();
    write_configuration(root);
    let mut config_paths = vec![(None, root.to_path_buf())];
    if with_extension {
        let extension = dir.path().join("cfe");
        write_extension(&extension);
        config_paths.push((Some("Расширение".to_string()), extension));
    }
    let path = root.join("CommonModules/Тест/Ext/Module.bsl");
    write(root, "CommonModules/Тест/Ext/Module.bsl", source);

    let mut db = RootDatabaseImpl::new();
    let file_id = FileId(0);
    let mut files = FileSet::default();
    files.insert(file_id, VfsPath::new(path.to_string_lossy().into_owned()));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(files));
    db.set_file_source_root(file_id, SourceRootId(0));
    db.set_file_text(file_id, source);
    db.set_all_config_paths(config_paths, Some(bsl_metadata::ExtensionScope::InfoBase));
    db.set_workspace_load_complete(loaded);
    ide_diagnostics::file_diagnostics(&db, file_id, &DiagnosticsConfig::all_enabled())
        .into_iter()
        .filter(|diag| diag.code.as_str() == CODE)
        .map(|diag| (diag.message, source[diag.range].to_string()))
        .collect()
}

#[test]
fn paths_to_existing_metadata_are_silent() {
    let source = r#"
Процедура Тест() Экспорт
    Т1 = Тип("СправочникСсылка.Товары");
    Т2 = Тип("ПеречислениеСсылка.Цвета");
    Т3 = Тип("РегистрСведенийНаборЗаписей.Курсы");
    Т4 = Тип("ОбработкаОбъект.Загрузка");
    З1 = ПредопределенноеЗначение("Перечисление.Цвета.Красный");
    З2 = ПредопределенноеЗначение("Справочник.Товары.Основной");
    З3 = ПредопределенноеЗначение("Перечисление.Цвета.ПустаяСсылка");
    ОткрытьФорму("Справочник.Товары.ФормаСписка");
    ОткрытьФорму("Обработка.Загрузка.Форма");
    ОткрытьФорму("РегистрСведений.Курсы.Форма.ФормаЗаписи");
КонецПроцедуры
"#;
    assert_eq!(reported(source), Vec::<String>::new());
}

#[test]
fn a_missing_object_is_reported_for_every_function() {
    let source = r#"
Процедура Тест() Экспорт
    Т1 = Тип("СправочникСсылка.Услуги");
    Т2 = Тип("РегистрСведенийНаборЗаписей.Цены");
    З1 = ПредопределенноеЗначение("Перечисление.Размеры.Большой");
    ОткрытьФорму("ОбщаяФорма.Настройки");
    ОткрытьФорму("Обработка.Выгрузка.Форма");
    Ф = ПолучитьФорму("Справочник.Услуги.Форма.ФормаЭлемента");
КонецПроцедуры
"#;
    assert_eq!(
        reported(source),
        vec![
            "Объект метаданных \"Справочник.Услуги\" не найден в конфигурации",
            "Объект метаданных \"РегистрСведений.Цены\" не найден в конфигурации",
            "Объект метаданных \"Перечисление.Размеры\" не найден в конфигурации",
            "Объект метаданных \"Обработка.Выгрузка\" не найден в конфигурации",
            "Объект метаданных \"Справочник.Услуги\" не найден в конфигурации",
        ],
        "a common form is not in the configuration model and is not judged"
    );
}

#[test]
fn a_missing_predefined_value_is_reported() {
    let source = r#"
Процедура Тест() Экспорт
    З1 = ПредопределенноеЗначение("Перечисление.Цвета.Зеленый");
    З2 = ПредопределенноеЗначение("Справочник.Товары.Прочий");
КонецПроцедуры
"#;
    assert_eq!(
        reported(source),
        vec![
            "Предопределенное значение \"Зеленый\" не найдено у \"Перечисление.Цвета\"",
            "Предопределенное значение \"Прочий\" не найдено у \"Справочник.Товары\"",
        ]
    );
}

#[test]
fn english_spellings_are_judged_the_same_way() {
    let source = r#"
Procedure Test() Export
    T1 = Type("CatalogRef.Товары");
    V1 = PredefinedValue("Enum.Цвета.Красный");
    OpenForm("Catalog.Товары.ListForm");
    T2 = Type("CatalogRef.Услуги");
    V2 = PredefinedValue("Enum.Цвета.Зеленый");
EndProcedure
"#;
    assert_eq!(
        reported(source),
        vec![
            "Объект метаданных \"Справочник.Услуги\" не найден в конфигурации",
            "Предопределенное значение \"Зеленый\" не найдено у \"Перечисление.Цвета\"",
        ]
    );
}

#[test]
fn computed_or_foreign_paths_are_silent() {
    let source = r#"
Процедура Тест(ИмяФормы, ИмяСправочника) Экспорт
    ОткрытьФорму(ИмяФормы);
    ОткрытьФорму("Справочник." + ИмяСправочника + ".ФормаСписка");
    Т1 = Тип("СправочникСсылка." + ИмяСправочника);
    ОткрытьФорму("ВнешняяОбработка.Услуги.Форма");
    Т2 = Тип("ВнешняяОбработкаОбъект.Услуги");
    ОткрытьФорму("Справочник.Товары.Форма.НетТакойФормы");
КонецПроцедуры
"#;
    assert_eq!(
        reported(source),
        Vec::<String>::new(),
        "a computed path, an external processor and a form name are not judged"
    );
}

#[test]
fn a_module_method_of_the_same_name_takes_the_call_over() {
    let source = r#"
Функция Тип(Имя)
    Возврат Имя;
КонецФункции

Процедура Тест() Экспорт
    Т = Тип("СправочникСсылка.Услуги");
КонецПроцедуры
"#;
    assert_eq!(reported(source), Vec::<String>::new());
}

#[test]
fn an_object_another_extension_adds_is_silent() {
    let source = r#"
Процедура Тест() Экспорт
    Т = Тип("СправочникСсылка.Услуги");
    ОткрытьФорму("Справочник.Услуги.ФормаСписка");
КонецПроцедуры
"#;
    assert_eq!(reported(source).len(), 2, "without the extension the catalog is missing");
    assert_eq!(
        reported_in(source, true),
        Vec::<String>::new(),
        "the extension is applied when the call runs"
    );
}

#[test]
fn modal_and_get_form_paths_are_judged_too() {
    let source = r#"
Процедура Тест() Экспорт
    ОткрытьФормуМодально("Справочник.Услуги.ФормаВыбора");
    Ф = GetForm("Catalog.Услуги.ItemForm");
    OpenFormModal("Catalog.Товары.ChoiceForm");
КонецПроцедуры
"#;
    assert_eq!(
        reported(source),
        vec![
            "Объект метаданных \"Справочник.Услуги\" не найден в конфигурации",
            "Объект метаданных \"Справочник.Услуги\" не найден в конфигурации",
        ]
    );
}

#[test]
fn the_finding_points_at_the_string_literal() {
    let source = r#"
Процедура Тест() Экспорт
    Если ТипЗнч(Неопределено) = Тип("СправочникСсылка.Услуги") Тогда
        ОткрытьФорму("Справочник.Товары.ФормаСписка", , , ПредопределенноеЗначение("Перечисление.Цвета.Зеленый"));
    КонецЕсли;
КонецПроцедуры
"#;
    let ranges: Vec<String> =
        findings_in(source, false, true).into_iter().map(|(_, text)| text).collect();
    assert_eq!(ranges, vec!["\"СправочникСсылка.Услуги\"", "\"Перечисление.Цвета.Зеленый\""]);
}

#[test]
fn unknown_prefixes_and_shapes_are_silent() {
    let source = r#"
Процедура Тест() Экспорт
    Т1 = Тип("ОпределяемыйТип.Услуги");
    Т2 = Тип("КонстантаМенеджерЗначения.Услуги");
    Т3 = Тип("СправочникСсылка");
    Т4 = Тип("СправочникСсылка.Услуги.Лишний");
    Т5 = Тип("СправочникСсылка.Не Имя");
    З1 = ПредопределенноеЗначение("НеВид.Услуги.Основной");
    ОткрытьФорму("ЖурналДокументов.Услуги.ФормаСписка");
    ОткрытьФорму("ФормаСписка");
КонецПроцедуры
"#;
    assert_eq!(reported(source), Vec::<String>::new());
}

#[test]
fn a_module_without_a_visible_configuration_is_silent() {
    let source = r#"
Процедура Тест() Экспорт
    Т = Тип("СправочникСсылка.Услуги");
    З = ПредопределенноеЗначение("Перечисление.Цвета.Зеленый");
    ОткрытьФорму("Справочник.Услуги.ФормаСписка");
КонецПроцедуры
"#;
    let dir = tempfile::tempdir().expect("create fixture directory");
    let path = dir.path().join("Module.bsl");
    std::fs::write(&path, source).expect("write module");

    let mut db = RootDatabaseImpl::new();
    let file_id = FileId(0);
    let mut files = FileSet::default();
    files.insert(file_id, VfsPath::new(path.to_string_lossy().into_owned()));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(files));
    db.set_file_source_root(file_id, SourceRootId(0));
    db.set_file_text(file_id, source);
    let reported: Vec<String> =
        ide_diagnostics::file_diagnostics(&db, file_id, &DiagnosticsConfig::all_enabled())
            .into_iter()
            .filter(|diag| diag.code.as_str() == CODE)
            .map(|diag| diag.message)
            .collect();
    assert_eq!(reported, Vec::<String>::new());
}

/// The rule's messages for `source` written at `module` (relative to `dir`), with the
/// workspace registering `roots`; the root at `external`, if any, is an external data
/// processor export.
fn reported_in_workspace(
    dir: &Path,
    roots: Vec<(Option<String>, std::path::PathBuf)>,
    external: Option<usize>,
    module: &str,
    source: &str,
) -> Vec<String> {
    let path = dir.join(module);
    write(dir, module, source);

    let mut db = RootDatabaseImpl::new();
    let file_id = FileId(0);
    let mut files = FileSet::default();
    files.insert(file_id, VfsPath::new(path.to_string_lossy().into_owned()));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(files));
    db.set_file_source_root(file_id, SourceRootId(0));
    db.set_file_text(file_id, source);
    let mut snapshot = ide_db::metadata::WorkspaceConfigsSnapshot::from_paths(roots);
    if let Some(external) = external {
        snapshot.kinds[external] =
            ide_db::metadata::RootKind::External(bsl_metadata::ExternalObjectKind::DataProcessor);
    }
    db.set_workspace_configs_snapshot(snapshot);
    ide_diagnostics::file_diagnostics(&db, file_id, &DiagnosticsConfig::all_enabled())
        .into_iter()
        .filter(|diag| diag.code.as_str() == CODE)
        .map(|diag| diag.message)
        .collect()
}

const MISSING_CATALOG: &str = r#"
Процедура Тест() Экспорт
    Т = Тип("СправочникСсылка.Нет");
КонецПроцедуры
"#;

#[test]
fn a_module_of_an_extension_is_judged_against_the_main_configuration() {
    let dir = tempfile::tempdir().expect("create fixture directory");
    let (cf, cfe) = (dir.path().join("cf"), dir.path().join("cfe"));
    write_configuration(&cf);
    write_extension(&cfe);
    assert_eq!(
        reported_in_workspace(
            dir.path(),
            vec![(None, cf), (Some("Расширение".to_string()), cfe)],
            None,
            "cfe/CommonModules/Тест/Ext/Module.bsl",
            MISSING_CATALOG,
        ),
        vec!["Объект метаданных \"Справочник.Нет\" не найден в конфигурации"]
    );
}

#[test]
fn a_module_outside_the_configuration_roots_is_silent() {
    let dir = tempfile::tempdir().expect("create fixture directory");
    let cf = dir.path().join("cf");
    write_configuration(&cf);
    assert_eq!(
        reported_in_workspace(
            dir.path(),
            vec![(None, cf)],
            None,
            "scratch/Module.bsl",
            MISSING_CATALOG,
        ),
        Vec::<String>::new()
    );
}

#[test]
fn an_external_data_processor_is_silent() {
    let dir = tempfile::tempdir().expect("create fixture directory");
    let (cf, epf) = (dir.path().join("cf"), dir.path().join("epf"));
    write_configuration(&cf);
    assert_eq!(
        reported_in_workspace(
            dir.path(),
            vec![(None, cf), (Some("Внешняя".to_string()), epf)],
            Some(1),
            "epf/Внешняя/Ext/ObjectModule.bsl",
            MISSING_CATALOG,
        ),
        Vec::<String>::new()
    );
}

#[test]
fn a_workspace_of_extensions_only_is_silent() {
    let dir = tempfile::tempdir().expect("create fixture directory");
    let cfe = dir.path().join("cfe");
    write_extension(&cfe);
    assert_eq!(
        reported_in_workspace(
            dir.path(),
            vec![(Some("Расширение".to_string()), cfe)],
            None,
            "cfe/CommonModules/Тест/Ext/Module.bsl",
            MISSING_CATALOG,
        ),
        Vec::<String>::new()
    );
}

#[test]
fn a_directory_of_sources_without_a_configuration_is_silent() {
    let dir = tempfile::tempdir().expect("create fixture directory");
    let src = dir.path().join("src");
    assert_eq!(
        reported_in_workspace(
            dir.path(),
            vec![(None, src)],
            None,
            "src/Module.bsl",
            MISSING_CATALOG
        ),
        Vec::<String>::new()
    );
}

#[test]
fn nothing_is_judged_while_the_workspace_is_loading() {
    let source = r#"
Процедура Тест() Экспорт
    Т = Тип("СправочникСсылка.Товары");
    З = ПредопределенноеЗначение("Перечисление.Цвета.Красный");
    ОткрытьФорму("Обработка.Загрузка.Форма");
КонецПроцедуры
"#;
    assert_eq!(reported(source), Vec::<String>::new(), "loaded, every object exists");
    assert_eq!(
        findings_in(source, false, false),
        Vec::<(String, String)>::new(),
        "while loading, the configuration reads as empty"
    );
}

#[test]
fn a_report_spelled_with_yo_is_judged_like_one_spelled_with_ye() {
    let source = r#"
Процедура Тест() Экспорт
    ОткрытьФорму("Отчёт.Продажи.Форма");
    ОткрытьФорму("Отчет.Продажи.Форма");
КонецПроцедуры
"#;
    assert_eq!(
        reported(source),
        vec![
            "Объект метаданных \"Отчет.Продажи\" не найден в конфигурации",
            "Объект метаданных \"Отчет.Продажи\" не найден в конфигурации",
        ]
    );
}

#[test]
fn predefined_items_that_cannot_be_read_are_not_judged() {
    let source = r#"
Процедура Тест() Экспорт
    З = ПредопределенноеЗначение("Справочник.Товары.Основной");
КонецПроцедуры
"#;
    let dir = tempfile::tempdir().expect("create fixture directory");
    let cf = dir.path().join("cf");
    write_configuration(&cf);
    write(&cf, "Catalogs/Товары/Ext/Predefined.xml", "<PredefinedData");
    assert_eq!(
        reported_in_workspace(
            dir.path(),
            vec![(None, cf)],
            None,
            "cf/CommonModules/Тест/Ext/Module.bsl",
            source,
        ),
        Vec::<String>::new()
    );
}

#[test]
fn a_catalog_without_predefined_items_file_is_not_judged() {
    let source = r#"
Процедура Тест() Экспорт
    З = ПредопределенноеЗначение("Справочник.Товары.Нет");
КонецПроцедуры
"#;
    let dir = tempfile::tempdir().expect("create fixture directory");
    let cf = dir.path().join("cf");
    write_configuration(&cf);
    let judged = |cf: &Path| {
        reported_in_workspace(
            dir.path(),
            vec![(None, cf.to_path_buf())],
            None,
            "cf/CommonModules/Тест/Ext/Module.bsl",
            source,
        )
    };
    assert_eq!(judged(&cf).len(), 1, "with Predefined.xml the item set is known");
    std::fs::remove_file(cf.join("Catalogs/Товары/Ext/Predefined.xml"))
        .expect("remove predefined items");
    assert_eq!(judged(&cf), Vec::<String>::new());
}

#[test]
fn a_readable_empty_predefined_items_file_is_judged() {
    let source = r#"
Процедура Тест() Экспорт
    З = ПредопределенноеЗначение("Справочник.Товары.Основной");
КонецПроцедуры
"#;
    let dir = tempfile::tempdir().expect("create fixture directory");
    let cf = dir.path().join("cf");
    write_configuration(&cf);
    write(
        &cf,
        "Catalogs/Товары/Ext/Predefined.xml",
        r#"<PredefinedData xmlns="http://v8.1c.ru/8.3/xcf/predef"/>"#,
    );
    assert_eq!(
        reported_in_workspace(
            dir.path(),
            vec![(None, cf)],
            None,
            "cf/CommonModules/Тест/Ext/Module.bsl",
            source,
        ),
        vec![
            "Предопределенное значение \"Основной\" не найдено у \"Справочник.Товары\"".to_string()
        ]
    );
}

#[test]
fn predefined_items_whose_file_is_not_text_are_not_judged() {
    let source = r#"
Процедура Тест() Экспорт
    З = ПредопределенноеЗначение("Справочник.Товары.Основной");
КонецПроцедуры
"#;
    let dir = tempfile::tempdir().expect("create fixture directory");
    let cf = dir.path().join("cf");
    write_configuration(&cf);
    // «Основной» in windows-1251: not UTF-8, so the file cannot be read as text.
    std::fs::write(
        cf.join("Catalogs/Товары/Ext/Predefined.xml"),
        b"<Name>\xce\xf1\xed\xee\xe2\xed\xee\xe9</Name>",
    )
    .expect("write predefined items");
    assert_eq!(
        reported_in_workspace(
            dir.path(),
            vec![(None, cf)],
            None,
            "cf/CommonModules/Тест/Ext/Module.bsl",
            source,
        ),
        Vec::<String>::new()
    );
}

#[test]
fn every_kind_spelled_with_yo_is_judged() {
    let source = r#"
Процедура Тест() Экспорт
    Т1 = Тип("РегистрРасчётаНаборЗаписей.Нет");
    Т2 = Тип("ПланСчётовСсылка.Нет");
    ОткрытьФорму("РегистрРасчёта.Нет.Форма");
    З = ПредопределенноеЗначение("ПланВидовРасчёта.Нет.Значение");
КонецПроцедуры
"#;
    assert_eq!(
        reported(source),
        vec![
            "Объект метаданных \"РегистрРасчета.Нет\" не найден в конфигурации",
            "Объект метаданных \"ПланСчетов.Нет\" не найден в конфигурации",
            "Объект метаданных \"РегистрРасчета.Нет\" не найден в конфигурации",
            "Объект метаданных \"ПланВидовРасчета.Нет\" не найден в конфигурации",
        ]
    );
}
