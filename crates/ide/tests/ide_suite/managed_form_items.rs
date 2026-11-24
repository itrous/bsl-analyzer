//! `Элементы.<Имя>` in a managed form module: an item the form's `Form.xml` does not
//! describe is reported, unless the module itself may change the set of items, or the
//! name may be one the platform generates for a command. Without the form nothing is
//! judged.

use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::metadata::{RootKind, WorkspaceConfigsSnapshot};
use ide_db::RootDatabaseImpl;
use ide_diagnostics::{DiagnosticCode, DiagnosticsConfig};
use std::path::{Path, PathBuf};
use tempfile::TempDir;
use vfs::{FileId, FileSet, VfsPath};

/// A synthetic catalog with one managed form holding `Группа`, `Поле`, `Кнопка`, a
/// context menu and the form's own auto command bar with a button of its own, and
/// returns the path of its module.
fn form_config(root: &Path) -> PathBuf {
    let form_dir = root.join("Catalogs/Справочник1/Forms/ФормаЭлемента/Ext");
    std::fs::create_dir_all(form_dir.join("Form")).expect("create form directory");
    std::fs::write(
        root.join("Configuration.xml"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <Configuration uuid="11111111-1111-1111-1111-111111111111">
        <Properties>
            <Name>Конфигурация1</Name>
        </Properties>
        <ChildObjects>
            <Catalog>Справочник1</Catalog>
        </ChildObjects>
    </Configuration>
</MetaDataObject>"#,
    )
    .expect("write Configuration.xml");
    std::fs::write(
        root.join("Catalogs/Справочник1.xml"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <Catalog uuid="22222222-2222-2222-2222-222222222222">
        <Properties>
            <Name>Справочник1</Name>
            <CodeLength>9</CodeLength>
            <DescriptionLength>25</DescriptionLength>
        </Properties>
        <ChildObjects>
            <Form>ФормаЭлемента</Form>
        </ChildObjects>
    </Catalog>
</MetaDataObject>"#,
    )
    .expect("write catalog XML");
    std::fs::write(
        root.join("Catalogs/Справочник1/Forms/ФормаЭлемента.xml"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <Form uuid="33333333-3333-3333-3333-333333333333">
        <Properties>
            <Name>ФормаЭлемента</Name>
            <FormType>Managed</FormType>
        </Properties>
    </Form>
</MetaDataObject>"#,
    )
    .expect("write form descriptor");
    std::fs::write(
        form_dir.join("Form.xml"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<Form xmlns="http://v8.1c.ru/8.3/xcf/logform" xmlns:v8="http://v8.1c.ru/8.1/data/core" version="2.10">
    <AutoCommandBar name="ФормаКоманднаяПанель" id="-1">
        <ChildItems>
            <Button name="Перезаписать" id="7"/>
        </ChildItems>
    </AutoCommandBar>
    <ChildItems>
        <UsualGroup name="Группа" id="2">
            <ChildItems>
                <InputField name="Поле" id="5">
                    <ContextMenu name="ПолеКонтекстноеМеню" id="51"/>
                </InputField>
                <Button name="Кнопка" id="6"/>
            </ChildItems>
        </UsualGroup>
    </ChildItems>
</Form>"#,
    )
    .expect("write Form.xml");
    root.join("Catalogs/Справочник1/Forms/ФормаЭлемента/Ext/Form/Module.bsl")
}

fn setup(module: &Path, root: &Path, text: &str) -> (RootDatabaseImpl, FileId) {
    std::fs::create_dir_all(module.parent().expect("parent")).expect("create module directory");
    std::fs::write(module, text).expect("write module");
    let mut db = RootDatabaseImpl::new();
    let file_id = FileId(0);
    let mut file_set = FileSet::default();
    file_set.insert(file_id, VfsPath::new(module.to_string_lossy().as_ref()));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    db.set_file_source_root(file_id, SourceRootId(0));
    db.set_file_text(file_id, text);
    db.set_all_config_paths(vec![(None, root.to_path_buf())], None);
    (db, file_id)
}

/// The unresolved-field findings of a module of the synthetic form, as `(line, message)`
/// with 0-based lines.
fn findings(text: &str) -> (TempDir, Vec<(u32, String)>) {
    let temp = tempfile::tempdir().expect("tempdir");
    let module = form_config(temp.path());
    let (db, file_id) = setup(&module, temp.path(), text);
    let found = ide_diagnostics::file_diagnostics(&db, file_id, &DiagnosticsConfig::all_enabled())
        .into_iter()
        .filter(|d| d.code == DiagnosticCode::UnresolvedField)
        .map(|d| {
            let start = usize::from(d.range.start());
            (u32::try_from(text[..start].matches('\n').count()).unwrap(), d.message)
        })
        .collect();
    (temp, found)
}

fn procedure(body: &str) -> String {
    format!("&НаСервере\nПроцедура Тест()\n{body}КонецПроцедуры\n")
}

#[test]
fn an_item_the_form_does_not_have_is_reported() {
    let text = procedure("\tЭлементы.НетТакогоЭлемента.Доступность = Истина;\n");
    let (_temp, found) = findings(&text);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].0, 2, "{found:?}");
    assert!(found[0].1.contains("НетТакогоЭлемента"), "{found:?}");
}

#[test]
fn items_the_form_has_are_not_reported() {
    let text = procedure(
        "\tЭлементы.Поле.Доступность = Истина;
\tЭлементы.Группа.Видимость = Ложь;
\tЭлементы.Кнопка.Заголовок = \"Да\";
\tЭлементы.ПолеКонтекстноеМеню.Видимость = Ложь;
\tЭлементы.ФормаКоманднаяПанель.Видимость = Ложь;
\tЭлементы.Перезаписать.Видимость = Ложь;
",
    );
    let (_temp, found) = findings(&text);
    assert!(found.is_empty(), "{found:?}");
}

#[test]
fn a_module_that_may_change_the_items_is_not_judged() {
    for hatch in [
        "\tЭлементы.Добавить(\"Новый\", Тип(\"ПолеФормы\"), Элементы.Группа);\n",
        "\tЭлементы.Удалить(Элементы.Поле);\n",
        "\tПостроить(ЭтаФорма);\n",
        "\tПостроить(ЭтотОбъект);\n",
        "\tПостроить(Элементы);\n",
        "\tВыполнить(\"Элементы.Добавить()\");\n",
    ] {
        let text =
            procedure(&format!("{hatch}\tЭлементы.НетТакогоЭлемента.Доступность = Истина;\n"));
        let (_temp, found) = findings(&text);
        assert!(found.is_empty(), "{hatch}: {found:?}");
    }
}

#[test]
fn a_name_the_platform_may_generate_is_not_judged() {
    // A command placed into a command bar or a context menu becomes an item named after
    // its owner followed by the command, and the dump describes none of them.
    let text = procedure(
        "\tЭлементы.ФормаЗаписатьИЗакрыть.Видимость = Ложь;
\tЭлементы.ПолеКонтекстноеМенюОчистить.Видимость = Ложь;
",
    );
    let (_temp, found) = findings(&text);
    assert!(found.is_empty(), "{found:?}");
}

/// A form borrowed by an extension dumps only the items it adds. The base
/// dialog's items are absent from that list, so a reference to one of them is
/// not a miss.
#[test]
fn a_borrowed_form_does_not_judge_items_the_base_describes() {
    let root = PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../bsl-metadata/fixtures/extension_metadata"
    ));
    let base = root.join("base");
    let extension = root.join("extension");
    let ext_module = extension.join("Documents/Заказ/Forms/ФормаДокумента/Ext/Form/Module.bsl");
    let base_module = base.join("Documents/Заказ/Forms/ФормаДокумента/Ext/Form/Module.bsl");
    let text =
        "&НаСервере\nПроцедура Тест()\n\tЭлементы.БазовоеПоле.Видимость = Ложь;\nКонецПроцедуры\n";
    let mut db = RootDatabaseImpl::new();
    let mut file_set = FileSet::default();
    file_set.insert(FileId(0), VfsPath::new(ext_module.to_string_lossy().as_ref()));
    file_set.insert(FileId(1), VfsPath::new(base_module.to_string_lossy().as_ref()));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    db.set_file_source_root(FileId(0), SourceRootId(0));
    db.set_file_source_root(FileId(1), SourceRootId(0));
    db.set_file_text(FileId(0), text);
    db.set_file_text(FileId(1), &std::fs::read_to_string(&base_module).unwrap());
    let paths = vec![(None, base.clone()), (Some("Расширение".to_string()), extension.clone())];
    db.set_workspace_configs_snapshot(WorkspaceConfigsSnapshot {
        external_owners: Vec::new(),
        compilation_overrides: Default::default(),
        extension_scopes: vec![None, Some(bsl_metadata::ExtensionScope::InfoBase)],
        canonical_paths: paths
            .iter()
            .map(|(_, path)| std::fs::canonicalize(path).unwrap_or_else(|_| path.clone()))
            .collect(),
        kinds: vec![RootKind::Base, RootKind::Extension],
        paths,
        closures: vec![vec![], vec![]],
        topological_order: vec![0, 1],
        fingerprint: None,
        source_exclusions: Default::default(),
    });
    let found: Vec<_> =
        ide_diagnostics::file_diagnostics(&db, FileId(0), &DiagnosticsConfig::all_enabled())
            .into_iter()
            .filter(|d| d.code == DiagnosticCode::UnresolvedField)
            .map(|d| d.message)
            .collect();
    assert!(found.is_empty(), "{found:?}");
}

/// `&ИзменениеИКонтроль` is inferred against the base form. An item the
/// extension's own `Form.xml` adds is still not a miss; a name neither form
/// lists still is.
#[test]
fn a_change_and_validate_insert_does_not_judge_an_extension_item() {
    let root = PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../bsl-metadata/fixtures/extension_metadata"
    ));
    let base = root.join("base");
    let extension = root.join("extension");
    let ext_module = extension.join("Documents/Заказ/Forms/ФормаДокумента/Ext/Form/Module.bsl");
    let base_module = base.join("Documents/Заказ/Forms/ФормаДокумента/Ext/Form/Module.bsl");
    let ext_text = "&НаКлиенте\n&ИзменениеИКонтроль(\"Проверка\")\nПроцедура Расш_Проверка()\n#Вставка\n\tЭлементы.ПолеРасширения.Видимость = Ложь;\n\tЭлементы.НетТакого.Видимость = Ложь;\n#КонецВставки\nКонецПроцедуры\n";
    let base_text = "&НаКлиенте\nПроцедура Проверка()\n\tДанные = Объект;\nКонецПроцедуры\n";
    let mut db = RootDatabaseImpl::new();
    let mut file_set = FileSet::default();
    file_set.insert(FileId(0), VfsPath::new(ext_module.to_string_lossy().as_ref()));
    file_set.insert(FileId(1), VfsPath::new(base_module.to_string_lossy().as_ref()));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    db.set_file_source_root(FileId(0), SourceRootId(0));
    db.set_file_source_root(FileId(1), SourceRootId(0));
    db.set_file_text(FileId(0), ext_text);
    db.set_file_text(FileId(1), base_text);
    let paths = vec![(None, base.clone()), (Some("Расширение".to_string()), extension.clone())];
    db.set_workspace_configs_snapshot(WorkspaceConfigsSnapshot {
        external_owners: Vec::new(),
        compilation_overrides: Default::default(),
        extension_scopes: vec![None, Some(bsl_metadata::ExtensionScope::InfoBase)],
        canonical_paths: paths
            .iter()
            .map(|(_, path)| std::fs::canonicalize(path).unwrap_or_else(|_| path.clone()))
            .collect(),
        kinds: vec![RootKind::Base, RootKind::Extension],
        paths,
        closures: vec![vec![], vec![]],
        topological_order: vec![0, 1],
        fingerprint: None,
        source_exclusions: Default::default(),
    });
    let found: Vec<_> =
        ide_diagnostics::file_diagnostics(&db, FileId(0), &DiagnosticsConfig::all_enabled())
            .into_iter()
            .filter(|d| d.code == DiagnosticCode::UnresolvedField)
            .map(|d| d.message)
            .collect();
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].contains("НетТакого"), "{found:?}");
    assert!(!found.iter().any(|message| message.contains("ПолеРасширения")), "{found:?}");
}

#[test]
fn without_the_form_nothing_is_judged() {
    let temp = tempfile::tempdir().expect("tempdir");
    let module = temp.path().join("CommonModules/Модуль1/Ext/Module.bsl");
    let text = procedure("\tЭлементы.НетТакогоЭлемента.Доступность = Истина;\n");
    let (db, file_id) = setup(&module, temp.path(), &text);
    let found: Vec<_> =
        ide_diagnostics::file_diagnostics(&db, file_id, &DiagnosticsConfig::all_enabled())
            .into_iter()
            .filter(|d| d.code == DiagnosticCode::UnresolvedField)
            .map(|d| d.message)
            .collect();
    assert!(found.is_empty(), "{found:?}");
}
