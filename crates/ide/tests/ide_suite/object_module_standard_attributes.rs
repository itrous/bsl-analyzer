//! The standard attributes of an object are bare names in its own object module: the implicit
//! receiver is the object, and which standard attributes it has follows from its own metadata
//! (`Родитель`/`ЭтоГруппа` only for a hierarchical catalog, `Владелец` only for a catalog with
//! owners). Every positive case has a control that differs from it in exactly one thing.
//!
//! Configuration: `КассыККМ` is hierarchical and owned by `Организации`; `ПростойСправочник`
//! is neither; `ПереоценкаТоваров` is a business process.

use ide::{Analysis, DiagnosticsConfig};
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use std::path::Path;
use vfs::{FileId, FileSet, VfsPath};

const HEAD: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:v8="http://v8.1c.ru/8.1/data/core" xmlns:xr="http://v8.1c.ru/8.3/xcf/readable" xmlns:xs="http://www.w3.org/2001/XMLSchema" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xmlns:cfg="http://v8.1c.ru/8.1/data/enterprise/current-config" version="2.10">"#;

fn write(root: &Path, rel: &str, text: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn catalog(name: &str, uuid_tail: u32, properties: &str) -> String {
    format!(
        r#"{HEAD}
  <Catalog uuid="0ab00000-0000-0000-0000-0000000004{uuid_tail:02}">
    <Properties>
      <Name>{name}</Name>
      <CodeLength>9</CodeLength>
      <DescriptionLength>100</DescriptionLength>
      {properties}
    </Properties>
  </Catalog>
</MetaDataObject>"#
    )
}

fn configuration(root: &Path) {
    write(
        root,
        "Configuration.xml",
        &format!(
            r#"{HEAD}
  <Configuration uuid="0ab00000-0000-0000-0000-000000000401">
    <Properties><Name>СтандартныеРеквизиты</Name></Properties>
  </Configuration>
</MetaDataObject>"#
        ),
    );
    write(root, "Catalogs/Организации.xml", &catalog("Организации", 10, ""));
    write(
        root,
        "Catalogs/КассыККМ.xml",
        &catalog(
            "КассыККМ",
            11,
            "<Hierarchical>true</Hierarchical>
      <HierarchyType>HierarchyFoldersAndItems</HierarchyType>
      <Owners><xr:Item xsi:type=\"xr:MDObjectRef\">Catalog.Организации</xr:Item></Owners>",
        ),
    );
    write(root, "Catalogs/ПростойСправочник.xml", &catalog("ПростойСправочник", 12, ""));
    write(
        root,
        "BusinessProcesses/ПереоценкаТоваров.xml",
        &format!(
            r#"{HEAD}
  <BusinessProcess uuid="0ab00000-0000-0000-0000-000000000420">
    <Properties>
      <Name>ПереоценкаТоваров</Name>
      <NumberLength>9</NumberLength>
    </Properties>
  </BusinessProcess>
</MetaDataObject>"#
        ),
    );
}

/// The `UnresolvedName` messages that mention `name`, for `code` placed at `rel`.
fn unresolved(rel: &str, code: &str, name: &str) -> Vec<String> {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    configuration(root);
    write(root, rel, code);

    let file_id = FileId(0);
    let mut db = RootDatabaseImpl::new();
    db.set_all_config_paths(vec![(None, root.to_path_buf())], None);
    let mut file_set = FileSet::default();
    file_set.insert(file_id, VfsPath::new(root.join(rel).to_string_lossy().as_ref()));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    db.set_file_source_root(file_id, SourceRootId(0));
    db.set_file_text(file_id, code);

    Analysis::from_database(db)
        .diagnostics(file_id, &DiagnosticsConfig::all_enabled())
        .into_iter()
        .filter(|d| format!("{:?}", d.code).contains("UnresolvedName"))
        .map(|d| d.message)
        .filter(|m| m.contains(name))
        .collect()
}

fn read_of(name: &str) -> String {
    format!(
        "Процедура Тест()
    Значение = {name};
КонецПроцедуры
"
    )
}

const OBJECT_MODULE_OF_CASH_DESK: &str = "Catalogs/КассыККМ/Ext/ObjectModule.bsl";
const OBJECT_MODULE_OF_PLAIN: &str = "Catalogs/ПростойСправочник/Ext/ObjectModule.bsl";
const OBJECT_MODULE_OF_PROCESS: &str = "BusinessProcesses/ПереоценкаТоваров/Ext/ObjectModule.bsl";
const COMMON_MODULE: &str = "CommonModules/Модуль1/Ext/Module.bsl";

#[test]
fn hierarchical_owned_catalog_sees_its_standard_attributes() {
    let code = "Процедура ПередЗаписью(Отказ)
    Если НЕ ЭтоГруппа И Не ЗначениеЗаполнено(Владелец) Тогда
        Отказ = Ложь;
    КонецЕсли;
    Если ЗначениеЗаполнено(Родитель) Тогда
        Отказ = Ложь;
    КонецЕсли;
КонецПроцедуры
";
    for name in ["ЭтоГруппа", "Владелец", "Родитель"] {
        assert_eq!(
            unresolved(OBJECT_MODULE_OF_CASH_DESK, code, name),
            Vec::<String>::new(),
            "{name}"
        );
    }
}

#[test]
fn business_process_sees_its_head_task() {
    let code = "Процедура ПередЗаписью(Отказ)
    Если ЗначениеЗаполнено(ВедущаяЗадача) Тогда
        Отказ = Ложь;
    КонецЕсли;
КонецПроцедуры
";
    assert_eq!(unresolved(OBJECT_MODULE_OF_PROCESS, code, "ВедущаяЗадача"), Vec::<String>::new());
}

#[test]
fn business_process_does_not_accept_the_name_the_platform_does_not_have() {
    // The model used to carry `ГлавнаяЗадача`, which no platform version has.
    assert_eq!(
        unresolved(OBJECT_MODULE_OF_PROCESS, &read_of("ГлавнаяЗадача"), "ГлавнаяЗадача").len(),
        1
    );
}

#[test]
fn catalog_does_not_see_a_business_process_attribute() {
    // The control for the head-task test: same bare name, but the object is a catalog.
    assert_eq!(
        unresolved(OBJECT_MODULE_OF_CASH_DESK, &read_of("ВедущаяЗадача"), "ВедущаяЗадача").len(),
        1
    );
}

#[test]
fn name_that_is_no_attribute_is_still_reported() {
    assert_eq!(
        unresolved(
            OBJECT_MODULE_OF_CASH_DESK,
            &read_of("НесуществующийРеквизит"),
            "НесуществующийРеквизит"
        )
        .len(),
        1
    );
}

#[test]
fn common_module_still_lacks_the_attributes() {
    for name in ["ЭтоГруппа", "Владелец", "ВедущаяЗадача"] {
        assert_eq!(unresolved(COMMON_MODULE, &read_of(name), name).len(), 1, "{name}");
    }
}

#[test]
fn standard_attributes_that_always_resolve_stay_resolved() {
    for name in ["Код", "Наименование", "Ссылка"] {
        assert_eq!(
            unresolved(OBJECT_MODULE_OF_PLAIN, &read_of(name), name),
            Vec::<String>::new(),
            "{name}"
        );
    }
}

/// Every diagnostic message (any code) that mentions `name`, for `code` placed at `rel`.
fn reported(rel: &str, code: &str, name: &str) -> Vec<String> {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    configuration(root);
    write(root, rel, code);

    let file_id = FileId(0);
    let mut db = RootDatabaseImpl::new();
    db.set_all_config_paths(vec![(None, root.to_path_buf())], None);
    let mut file_set = FileSet::default();
    file_set.insert(file_id, VfsPath::new(root.join(rel).to_string_lossy().as_ref()));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    db.set_file_source_root(file_id, SourceRootId(0));
    db.set_file_text(file_id, code);

    Analysis::from_database(db)
        .diagnostics(file_id, &DiagnosticsConfig::all_enabled())
        .into_iter()
        .map(|d| format!("{:?}: {}", d.code, d.message))
        .filter(|m| m.contains(name))
        .collect()
}

#[test]
fn flat_catalog_without_owners_sees_the_attributes_of_the_implicit_receiver() {
    for name in ["ЭтоГруппа", "Родитель", "Владелец"] {
        assert_eq!(
            reported(OBJECT_MODULE_OF_PLAIN, &read_of(name), name),
            Vec::<String>::new(),
            "{name}"
        );
    }
}

#[test]
fn the_same_names_read_through_a_reference_are_still_reported() {
    // The control for the test above: the same flat catalog and names, but the read goes through
    // the object's own reference, so the leniency of the implicit receiver does not apply.
    for name in ["ЭтоГруппа", "Родитель", "Владелец"] {
        let code = format!(
            "Процедура Тест()
    Значение = Ссылка.{name};
КонецПроцедуры
"
        );
        assert!(
            !reported(OBJECT_MODULE_OF_PLAIN, &code, name).is_empty(),
            "{name} through a reference must stay reported"
        );
    }
}
