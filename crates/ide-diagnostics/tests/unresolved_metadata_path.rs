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
    ide_diagnostics::file_diagnostics(&db, file_id, &DiagnosticsConfig::all_enabled())
        .into_iter()
        .filter(|diag| diag.code.as_str() == CODE)
        .map(|diag| diag.message)
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
