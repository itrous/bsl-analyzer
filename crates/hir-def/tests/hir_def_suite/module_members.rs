//! The exported-member table backing both `workspace/symbol` and the MCP name
//! dictionary. Lives outside the crate because building it needs a real
//! `DefDatabase`, which only `ide-db` provides.

use base_db::{SourceDatabase, SourceRoot, SourceRootId};
use hir_def::DefDatabase;
use ide_db::RootDatabaseImpl;
use vfs::{file_set::FileSet, FileId, VfsPath};

fn db_with_files(files: &[(&str, &str)]) -> RootDatabaseImpl {
    let mut db = RootDatabaseImpl::default();
    let mut file_set = FileSet::new();
    for (i, (path, _)) in files.iter().enumerate() {
        file_set.insert(FileId(i as u32), VfsPath::new(*path));
    }
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    for (i, (_, text)) in files.iter().enumerate() {
        let file_id = FileId(i as u32);
        db.set_file_source_root(file_id, SourceRootId(0));
        db.set_file_text(file_id, text);
    }
    db
}

/// Object modules of different metadata objects all spell their file
/// `ObjectModule.bsl`; keying the table by a path-derived module name keeps
/// exactly one of them and silently drops the rest of the configuration.
#[test]
fn modules_sharing_a_file_stem_keep_both_method_sets() {
    let files = [
        (
            "/ws/Справочники/Товары/Ext/ObjectModule.bsl",
            "Процедура ЗаписатьТовар() Экспорт\nКонецПроцедуры\n",
        ),
        (
            "/ws/Справочники/Склады/Ext/ObjectModule.bsl",
            "Процедура ЗаписатьСклад() Экспорт\nКонецПроцедуры\n",
        ),
    ];
    let db = db_with_files(&files);

    let found = db.module_members(SourceRootId(0));
    let names: Vec<&str> = found
        .modules
        .values()
        .flat_map(|info| info.methods.iter())
        .map(|m| m.name.as_str())
        .collect();

    assert!(names.contains(&"ЗаписатьТовар"), "{names:?}");
    assert!(names.contains(&"ЗаписатьСклад"), "{names:?}");
}

/// `workspace/symbol` offers exported module variables, so the table that
/// replaces its own scan has to carry them.
#[test]
fn exported_module_variables_reach_the_table() {
    let db = db_with_files(&[(
        "/ws/CommonModules/Настройки/Ext/Module.bsl",
        "Перем СчётчикЗапросов Экспорт;\n\nПроцедура Сбросить() Экспорт\nКонецПроцедуры\n",
    )]);

    let found = db.module_members(SourceRootId(0));
    let variables: Vec<&str> = found
        .modules
        .values()
        .flat_map(|info| info.variables.iter())
        .map(|v| v.name.as_str())
        .collect();

    assert_eq!(variables, vec!["СчётчикЗапросов"]);
}

/// A non-exported member cannot be called from outside its module and
/// `symbol_info` refuses to resolve it, so a search hit naming one would be a
/// candidate nothing accepts back. The filter lives here, in the single
/// provider, rather than in each consumer.
#[test]
fn non_exported_members_stay_out_of_the_table() {
    let db = db_with_files(&[(
        "/ws/CommonModules/Настройки/Ext/Module.bsl",
        "Перем Публичная Экспорт;\nПерем Приватная;\n\n\
         Процедура Открытая() Экспорт\nКонецПроцедуры\n\n\
         Процедура Закрытая()\nКонецПроцедуры\n",
    )]);

    let found = db.module_members(SourceRootId(0));
    let module = found.modules.values().next().expect("the module is indexed");

    let methods: Vec<&str> = module.methods.iter().map(|m| m.name.as_str()).collect();
    let variables: Vec<&str> = module.variables.iter().map(|v| v.name.as_str()).collect();

    assert_eq!(methods, vec!["Открытая"]);
    assert_eq!(variables, vec!["Публичная"]);
}

/// The module name is ambiguous across files and is therefore a field, but it
/// still has to be the *right* name — the dictionary spells `symbol` with it.
#[test]
fn each_module_carries_its_own_name() {
    let db = db_with_files(&[
        (
            "/ws/CommonModules/ОбщегоНазначения/Ext/Module.bsl",
            "Процедура Общая() Экспорт\nКонецПроцедуры\n",
        ),
        (
            "/ws/Справочники/Товары/Ext/ObjectModule.bsl",
            "Процедура Товарная() Экспорт\nКонецПроцедуры\n",
        ),
    ]);

    let found = db.module_members(SourceRootId(0));
    let mut named: Vec<(&str, &str)> = found
        .modules
        .values()
        .map(|info| (info.module_name.as_str(), info.methods[0].name.as_str()))
        .collect();
    named.sort_unstable();

    assert_eq!(named, vec![("ObjectModule", "Товарная"), ("ОбщегоНазначения", "Общая")]);
}
