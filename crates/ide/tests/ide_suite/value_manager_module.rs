//! A constant's value-manager module (`Constants/<Имя>/Ext/ValueManagerModule.bsl`) is
//! addressable by name: `symbol_info`, the declaration walk, the name dictionary and the
//! graph all key a module by its path, and a path the module index does not know is a
//! module none of them can name.
//!
//! The layout mirrors a typical vendor configuration, where
//! `Константы.КаталогСообщенийОбменаДаннымиДляLinux.СоздатьМенеджерЗначения()
//! .ПриЗаполненииРазрешенийНаДоступКВнешнимРесурсам(...)` is called from
//! `ОбменДаннымиСервер`.

use ide::{
    graph::graph_id_of_method, resolve_declarations, symbol_info, Analysis, StripRoot,
    SymbolInfoRequest, SymbolInfoSections,
};
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::metadata::{MdoEntry, MetadataListingData};
use ide_db::RootDatabaseImpl;
use std::path::PathBuf;
use vfs::{FileId, FileSet, VfsPath};

fn designer_root() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-metadata/fixtures/designer"))
}

const CONSTANT_XML: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../bsl-metadata/fixtures/designer/Constants/СтрокаКонст.xml"
));

const VALUE_MANAGER_MODULE: &str = "\
Процедура ПриЗаполненииРазрешений(ЗапросыРазрешений) Экспорт
КонецПроцедуры
";

const MANAGER_MODULE: &str = "\
Процедура МетодМенеджера() Экспорт
КонецПроцедуры
";

const CALLER: &str = "\
Процедура Прогон(ЗапросыРазрешений) Экспорт
    Константы.СтрокаКонст.СоздатьМенеджерЗначения().ПриЗаполненииРазрешений(ЗапросыРазрешений);
    Константы.СтрокаКонст.ПриЗаполненииРазрешений(ЗапросыРазрешений);
КонецПроцедуры
";

const SYMBOL: &str = "Константа.СтрокаКонст.ПриЗаполненииРазрешений";

/// Files: 0 value-manager module, 1 constant XML, 2 caller, 3 manager module.
fn db() -> (RootDatabaseImpl, Vec<FileId>) {
    let files = [
        ("Constants/СтрокаКонст/Ext/ValueManagerModule.bsl", VALUE_MANAGER_MODULE),
        ("Constants/СтрокаКонст.xml", CONSTANT_XML),
        ("CommonModules/Вызывающий/Ext/Module.bsl", CALLER),
        ("Constants/СтрокаКонст/Ext/ManagerModule.bsl", MANAGER_MODULE),
    ];
    let root = designer_root();
    let mut db = RootDatabaseImpl::new();
    let ids: Vec<FileId> = (0..files.len()).map(|i| FileId(i as u32)).collect();
    let mut file_set = FileSet::default();
    for (id, (rel, _)) in ids.iter().zip(&files) {
        file_set.insert(*id, VfsPath::from(root.join(rel)));
    }
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    for (id, (_, text)) in ids.iter().zip(&files) {
        db.set_file_source_root(*id, SourceRootId(0));
        db.set_file_text(*id, text);
    }
    db.set_all_config_paths(vec![(None, root.clone())], None);
    db.set_metadata_listing(
        &root.to_string_lossy(),
        MetadataListingData {
            entries: vec![MdoEntry {
                kind: bsl_metadata::MdoType::Constant,
                name: "СтрокаКонст".to_string(),
                main: ids[1],
                predefined: None,
            }],
            ..MetadataListingData::default()
        },
    );
    (db, ids)
}

fn by_name(db: &RootDatabaseImpl, symbol: &str) -> Option<ide::SymbolInfoCard> {
    symbol_info(
        db,
        &SymbolInfoRequest {
            symbol: Some(symbol.to_string()),
            position: None,
            locale: ide::Locale::default(),
            sections: SymbolInfoSections::all(),
            workspace_root: Some(StripRoot::resolve(&designer_root())),
        },
    )
}

#[test]
fn a_value_manager_export_has_a_symbol_info_card() {
    let (db, _) = db();
    let card = by_name(&db, SYMBOL).expect("the value-manager export resolves by name");
    let path = card.definition.and_then(|d| d.path).expect("the card points at its source");
    assert!(path.ends_with("Constants/СтрокаКонст/Ext/ValueManagerModule.bsl"), "{path}");
    assert_eq!(
        card.graph_id.as_deref(),
        Some("method/object/Constant/СтрокаКонст/ПриЗаполненииРазрешений"),
    );
}

#[test]
fn a_value_manager_export_has_a_declaration_and_a_graph_id() {
    let (db, ids) = db();
    let files: Vec<FileId> = resolve_declarations(&db, SYMBOL).iter().map(|d| d.file_id).collect();
    assert_eq!(files, vec![ids[0]]);
    assert_eq!(
        graph_id_of_method(
            &db,
            ids[0],
            "ПриЗаполненииРазрешений",
            Some(&StripRoot::resolve(&designer_root()))
        )
        .as_deref(),
        Some("method/object/Constant/СтрокаКонст/ПриЗаполненииРазрешений"),
    );
}

#[test]
fn the_name_dictionary_publishes_an_addressable_symbol() {
    let (db, _) = db();
    let analysis = Analysis::from_database(db);
    let found = analysis.workspace_symbols("ПриЗаполненииРазрешений");
    let symbols: Vec<Option<String>> = found.candidates.iter().map(|c| c.symbol.clone()).collect();
    assert_eq!(symbols, vec![Some(SYMBOL.to_string())]);
}

/// The manager module keeps its own name and the value-manager export does not leak
/// into `Константы.СтрокаКонст.<Метод>()`: that spelling reaches the manager only.
#[test]
fn the_manager_route_does_not_reach_the_value_manager_module() {
    let (db, ids) = db();
    let manager = by_name(&db, "Константа.СтрокаКонст.МетодМенеджера")
        .expect("the manager export still resolves");
    let path = manager.definition.and_then(|d| d.path).unwrap();
    assert!(path.ends_with("Constants/СтрокаКонст/Ext/ManagerModule.bsl"), "{path}");

    let analysis = Analysis::from_database(db);
    let call = CALLER.find("Константы.СтрокаКонст.ПриЗаполненииРазрешений").unwrap()
        + "Константы.СтрокаКонст.".len()
        + 2;
    let target = analysis.goto_definition(ids[2], call as u32);
    assert!(target.is_none(), "`Константы.X.Метод()` must not reach the value manager: {target:?}");
}
