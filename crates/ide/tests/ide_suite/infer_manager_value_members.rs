//! A member read on an enum, catalog or chart manager (`Перечисления.X.<значение>`,
//! `Справочники.X.<предопределённый>`) is checked against the values and predefined
//! items the configuration declares, the way the platform's extended check reports
//! «Возможно ошибочное свойство».

use hir::{HirDatabase, InferenceDiagnostic};
use ide::{Analysis, DiagnosticCode, DiagnosticsConfig};
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use std::path::Path;
use test_fixture::Fixture;
use vfs::FileId;

const CONFIGURATION: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <Configuration uuid="11111111-1111-1111-1111-111111111111">
        <Properties>
            <Name>ManagerMembers</Name>
        </Properties>
        <ChildObjects>
            <Catalog>ВидыКонтактов</Catalog>
            <Catalog>Хранилища</Catalog>
            <Enum>Состояния</Enum>
            <ChartOfCharacteristicTypes>ПараметрыРаботы</ChartOfCharacteristicTypes>
            <ChartOfAccounts>Учебный</ChartOfAccounts>
            <ChartOfCalculationTypes>ВидыОплат</ChartOfCalculationTypes>
        </ChildObjects>
    </Configuration>
</MetaDataObject>"#;

fn object_xml(tag: &str, uuid: &str, name: &str, children: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <{tag} uuid="{uuid}">
        <Properties>
            <Name>{name}</Name>
        </Properties>
        <ChildObjects>{children}</ChildObjects>
    </{tag}>
</MetaDataObject>"#
    )
}

fn predefined_xml(items: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<PredefinedData xmlns="http://v8.1c.ru/8.3/xcf/predef" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xsi:type="CatalogPredefinedItems" version="2.10">{items}</PredefinedData>"#
    )
}

fn item(uuid: &str, name: &str, children: &str) -> String {
    format!(r#"<Item id="{uuid}"><Name>{name}</Name><Code/><Description/>{children}</Item>"#)
}

/// One object of `dir` named `name`, with an `Ext/Predefined.xml` when `items` is given.
fn write_object(root: &Path, dir: &str, tag: &str, uuid: &str, name: &str, items: Option<&str>) {
    std::fs::create_dir_all(root.join(dir).join(name).join("Ext")).expect("object directory");
    std::fs::write(root.join(dir).join(format!("{name}.xml")), object_xml(tag, uuid, name, ""))
        .expect("object XML");
    if let Some(items) = items {
        std::fs::write(root.join(dir).join(name).join("Ext/Predefined.xml"), predefined_xml(items))
            .expect("Predefined.xml");
    }
}

fn write_configuration(root: &Path) {
    std::fs::write(root.join("Configuration.xml"), CONFIGURATION).expect("Configuration.xml");

    std::fs::create_dir_all(root.join("Enums")).expect("Enums directory");
    let values = ["Активно", "Закрыто"]
        .iter()
        .enumerate()
        .map(|(i, value)| {
            format!(
                r#"<EnumValue uuid="33333333-3333-3333-3333-33333333333{i}"><Properties><Name>{value}</Name></Properties></EnumValue>"#
            )
        })
        .collect::<String>();
    std::fs::write(
        root.join("Enums/Состояния.xml"),
        object_xml("Enum", "22222222-2222-2222-2222-222222222222", "Состояния", &values),
    )
    .expect("enum XML");

    // A folder with a nested item, and an item at the top level.
    let nested = item("44444444-4444-4444-4444-444444444442", "ТелефонРабочий", "");
    let contacts = item(
        "44444444-4444-4444-4444-444444444441",
        "Телефоны",
        &format!("<IsFolder>true</IsFolder><ChildItems>{nested}</ChildItems>"),
    ) + &item("44444444-4444-4444-4444-444444444443", "Почта", "");
    write_object(
        root,
        "Catalogs",
        "Catalog",
        "55555555-5555-5555-5555-555555555551",
        "ВидыКонтактов",
        Some(&contacts),
    );
    write_object(
        root,
        "Catalogs",
        "Catalog",
        "55555555-5555-5555-5555-555555555552",
        "Хранилища",
        None,
    );
    write_object(
        root,
        "ChartsOfCharacteristicTypes",
        "ChartOfCharacteristicTypes",
        "55555555-5555-5555-5555-555555555553",
        "ПараметрыРаботы",
        Some(&item("66666666-6666-6666-6666-666666666661", "ПоказыватьПодсказки", "")),
    );
    write_object(
        root,
        "ChartsOfAccounts",
        "ChartOfAccounts",
        "55555555-5555-5555-5555-555555555554",
        "Учебный",
        Some(&item("66666666-6666-6666-6666-666666666662", "Счет01", "")),
    );
    write_object(
        root,
        "ChartsOfCalculationTypes",
        "ChartOfCalculationTypes",
        "55555555-5555-5555-5555-555555555555",
        "ВидыОплат",
        Some(&item("66666666-6666-6666-6666-666666666663", "Оклад", "")),
    );
}

fn setup(code: &str, config: &Path) -> (RootDatabaseImpl, FileId) {
    let fixture = Fixture::parse(&format!("//- /test.bsl\n{code}"));
    let mut db = RootDatabaseImpl::new();
    let mut file_set = vfs::FileSet::default();
    for (file_id, file) in &fixture.files {
        file_set.insert(*file_id, file.path.clone());
    }
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    for (file_id, file) in &fixture.files {
        db.set_file_source_root(*file_id, SourceRootId(0));
        db.set_file_text(*file_id, &file.content);
    }
    db.set_all_config_paths(vec![(None, config.to_path_buf())], None);
    let file_id = *fixture.files.keys().next().expect("the test module");
    (db, file_id)
}

fn unresolved_fields(code: &str) -> Vec<String> {
    let config = tempfile::tempdir().expect("tempdir");
    write_configuration(config.path());
    let (db, file_id) = setup(code, config.path());
    db.infer(file_id)
        .diagnostics
        .iter()
        .filter_map(|(_, diag)| match diag {
            InferenceDiagnostic::UnresolvedField { field_name, .. } => {
                Some(field_name.as_str().to_string())
            }
            _ => None,
        })
        .collect()
}

#[test]
fn a_value_or_predefined_item_the_object_lacks_is_reported() {
    let found = unresolved_fields(
        r#"Процедура Тест()
    А = Перечисления.Состояния.Активно;
    Б = Перечисления.Состояния.Активна;
    В = Справочники.ВидыКонтактов.Почта;
    Г = Справочники.ВидыКонтактов.ТелефонДомашний;
    Д = ПланыВидовХарактеристик.ПараметрыРаботы.ПоказыватьПодсказки;
    Е = ПланыВидовХарактеристик.ПараметрыРаботы.ПоказыватьСовет;
    Ж = ПланыСчетов.Учебный.Счет01;
    З = ПланыСчетов.Учебный.Счет02;
    И = ПланыВидовРасчета.ВидыОплат.Оклад;
    К = ПланыВидовРасчета.ВидыОплат.Премия;
    Л = Enums.Состояния.Закрыт;
КонецПроцедуры
"#,
    );
    assert_eq!(
        found,
        ["Активна", "ТелефонДомашний", "ПоказыватьСовет", "Счет02", "Премия", "Закрыт"]
    );
}

#[test]
fn a_nested_predefined_item_and_its_folder_resolve() {
    let found = unresolved_fields(
        r#"Процедура Тест()
    А = Справочники.ВидыКонтактов.Телефоны;
    Б = Справочники.ВидыКонтактов.ТелефонРабочий;
    В = Справочники.ВидыКонтактов.телефонрабочий;
КонецПроцедуры
"#,
    );
    assert!(found.is_empty(), "nested items are members too: {found:?}");
}

#[test]
fn manager_methods_index_and_unread_predefined_data_stay_silent() {
    // `Хранилища` has no `Ext/Predefined.xml`: its predefined items are unknown, not absent.
    let found = unresolved_fields(
        r#"Процедура Тест(Ссылка)
    А = Перечисления.Состояния.ПустаяСсылка();
    Б = Справочники.ВидыКонтактов.ПолучитьСсылку(Новый УникальныйИдентификатор);
    В = Справочники.ВидыКонтактов.Выбрать();
    Г = Справочники.ВидыКонтактов.НайтиПоКоду("1");
    Д = ПланыСчетов.Учебный.ПустаяСсылка();
    Е = Перечисления.Состояния["Активна"];
    Ж = Перечисления.Состояния.Индекс(Ссылка);
    З = Справочники.Хранилища.Любой;
КонецПроцедуры
"#,
    );
    assert!(found.is_empty(), "these shapes must stay silent: {found:?}");
}

#[test]
fn the_rendered_diagnostic_names_the_manager() {
    let config = tempfile::tempdir().expect("tempdir");
    write_configuration(config.path());
    let (db, file_id) = setup(
        "Процедура Тест()\n    А = Перечисления.Состояния.Активна;\nКонецПроцедуры\n",
        config.path(),
    );
    let messages: Vec<String> = Analysis::from_database(db)
        .diagnostics(file_id, &DiagnosticsConfig::all_enabled())
        .into_iter()
        .filter(|diag| diag.code == DiagnosticCode::UnresolvedField)
        .map(|diag| diag.message)
        .collect();
    assert_eq!(messages.len(), 1, "{messages:?}");
    assert!(messages[0].contains("'Активна'"), "{messages:?}");
}
