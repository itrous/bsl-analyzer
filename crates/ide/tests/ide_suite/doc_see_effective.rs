//! Seeding a documented parameter is for ordinary modules only.
//!
//! Effective (`&ИзменениеИКонтроль`) inference works on a merged symbol tree, and a local id in
//! it does not name the same method in the file's own tree. Seeding there would read one method's
//! documentation into another method's parameter.

use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use stdx::case::CaseExt;
use vfs::{FileId, FileSet, VfsPath};

/// A common module's metadata object; the fixture builder writes bodies and `Configuration.xml`.
fn common_module_xml(name: &str, index: usize) -> String {
    format!(
        r#"<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <CommonModule uuid="00000000-0000-0000-0000-{:012}">
        <Properties>
            <Name>{name}</Name>
            <Global>false</Global>
            <Server>true</Server>
        </Properties>
    </CommonModule>
</MetaDataObject>"#,
        index + 1
    )
}

#[test]
fn an_effective_module_does_not_seed_a_documented_parameter() {
    const TARGET: &str = "// Возвращаемое значение:\n//   Структура:\n//    * Таймаут - Число - секунды.\nФункция Создать() Экспорт\n\tВозврат Новый Структура;\nКонецФункции\n";
    const BASE: &str = "// Параметры:\n//   Данные - см. База.Создать\nПроцедура Обработать(Данные) Экспорт\nКонецПроцедуры\n";
    // The extension's own documentation names a DIFFERENT structure. Seeding from the file's own
    // symbol tree would put this one into the merged method's parameter.
    const EXT: &str = "// Параметры:\n//   Данные - Структура:\n//    * Расширение - Строка - поле.\n&ИзменениеИКонтроль(\"Обработать\")\nПроцедура Расш1_Обработать(Данные)\n#Вставка\n\tЗначение = Данные;\n#КонецВставки\nКонецПроцедуры\n";

    let mut builder = test_fixture::CfeFixtureBuilder::new("");
    builder.add_base_module("База", TARGET);
    builder.add_base_module("М", BASE);
    builder.add_extension("Расш", "");
    builder.add_extension_module("Расш", "М", EXT);
    let fixture = builder.build();

    let mut bodies = Vec::new();
    for (index, module) in fixture.base_modules().iter().enumerate() {
        std::fs::write(
            fixture.root().join(format!("CommonModules/{}.xml", module.name())),
            common_module_xml(module.name(), index),
        )
        .unwrap();
        let path = fixture.root().join(format!("CommonModules/{}/Ext/Module.bsl", module.name()));
        bodies.push((path, module.source().to_string()));
    }
    let extension = &fixture.extensions()[0];
    for (index, module) in extension.modules().iter().enumerate() {
        std::fs::write(
            extension.root().join(format!("CommonModules/{}.xml", module.name())),
            common_module_xml(module.name(), index + 100),
        )
        .unwrap();
        let path = extension.root().join(format!("CommonModules/{}/Ext/Module.bsl", module.name()));
        bodies.push((path, module.source().to_string()));
    }

    let mut db = RootDatabaseImpl::new();
    db.set_all_config_paths(fixture.config_paths(), Some(bsl_metadata::ExtensionScope::InfoBase));
    let mut file_set = FileSet::default();
    for (index, (path, _)) in bodies.iter().enumerate() {
        file_set.insert(FileId(index as u32), VfsPath::new(path.to_string_lossy().into_owned()));
    }
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    let mut ext_file = FileId(0);
    for (index, (path, body)) in bodies.iter().enumerate() {
        let file_id = FileId(index as u32);
        db.set_file_source_root(file_id, SourceRootId(0));
        db.set_file_text(file_id, body);
        if path.starts_with(extension.root()) {
            ext_file = file_id;
        }
    }

    let target = ide_db::effective_target(&db, ext_file)
        .expect("модуль расширения с &ИзменениеИКонтроль обязан спариваться с базовым");
    let effective = hir::infer_effective(&db, target);

    assert_eq!(
        effective.var_types.get(&"Значение".fold_lower()).copied(),
        None,
        "эффективный вывод засеял параметр документацией своего файла: {:?}",
        effective.var_types,
    );
}
