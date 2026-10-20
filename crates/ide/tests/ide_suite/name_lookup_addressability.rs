//! The dictionary's one operational rule: a key is published only when the tool
//! it names accepts it.
//!
//! This is a round trip, not a table. A list of "which forms are addressable"
//! written beside the code would be a second source of truth about
//! `symbol_info`'s resolution, drifting from it in silence — and it did: the
//! first draft of this work called object-module methods unaddressable, which
//! `resolve_triple` disproves. So every published `symbol` is fed back here.

use ide::{
    lookup_names, symbol_info, NameCategory, NameQuery, SymbolInfoRequest, SymbolInfoSections,
};
use ide_db::base_db::{Locale, SourceDatabase, SourceRoot, SourceRootId};
use ide_db::metadata::{CommonModuleEntry, MdoEntry, MetadataListingData};
use ide_db::RootDatabaseImpl;
use vfs::{file_set::FileSet, FileId, VfsPath};

const CONFIG_ROOT: &str = "/ws/src/cf";

const COMMON_MODULE: &str = "\
Функция ЗначениеРеквизитаОбъекта(Ссылка, Реквизит) Экспорт
    Возврат Неопределено;
КонецФункции
";

const OBJECT_MODULE: &str = "\
Процедура ЗначениеПоУмолчанию() Экспорт
КонецПроцедуры
";

const MODULE_WITH_VARIABLE: &str = "\
Перем ЗначениеСчётчика Экспорт;
";

const CATALOG_XML: &str = "<MetaDataObject><Catalog><Properties><Name>Товары</Name></Properties></Catalog></MetaDataObject>";

/// A common module the listing knows and the sources do not: a protected module
/// ships without its `.bsl`. It is the one common module `symbol_info` cannot
/// resolve, because it resolves them through the path-derived module index.
const PROTECTED_XML: &str = "<MetaDataObject><CommonModule><Properties><Name>ЗначениеЗащищено</Name></Properties></CommonModule></MetaDataObject>";

/// A configuration that contains one representative of every class the rule has
/// to hold for: a common module and its method, an object module method (a
/// three-part name), a metadata object, and an exported module variable — the
/// one class that must come back WITHOUT a `symbol`.
fn stand() -> RootDatabaseImpl {
    let files: [(&str, &str); 5] = [
        ("/ws/src/cf/CommonModules/ОбщегоНазначения/Ext/Module.bsl", COMMON_MODULE),
        ("/ws/src/cf/Catalogs/Товары/Ext/ObjectModule.bsl", OBJECT_MODULE),
        ("/ws/src/cf/CommonModules/Счётчики/Ext/Module.bsl", MODULE_WITH_VARIABLE),
        ("/ws/src/cf/Catalogs/Товары.xml", CATALOG_XML),
        ("/ws/src/cf/CommonModules/ЗначениеЗащищено.xml", PROTECTED_XML),
    ];

    let mut db = RootDatabaseImpl::new();
    let mut file_set = FileSet::default();
    for (i, (path, _)) in files.iter().enumerate() {
        file_set.insert(FileId(i as u32), VfsPath::new(*path));
    }
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    for (i, (_, text)) in files.iter().enumerate() {
        let file_id = FileId(i as u32);
        db.set_file_source_root(file_id, SourceRootId(0));
        db.set_file_text(file_id, text);
    }

    db.set_all_config_paths(vec![(None, std::path::PathBuf::from(CONFIG_ROOT))], None);
    db.set_metadata_listing(
        CONFIG_ROOT,
        MetadataListingData {
            entries: vec![MdoEntry {
                kind: bsl_metadata::MdoType::Catalog,
                name: "Товары".to_string(),
                main: FileId(3),
                predefined: None,
            }],
            common_modules: vec![
                CommonModuleEntry {
                    name: "ОбщегоНазначения".to_string(),
                    main: FileId(0),
                    module_file: Some(FileId(0)),
                    unread_module_file: None,
                },
                CommonModuleEntry {
                    name: "ЗначениеЗащищено".to_string(),
                    main: FileId(4),
                    module_file: None,
                    unread_module_file: None,
                },
            ],
            ..MetadataListingData::default()
        },
    );
    db
}

fn card_for(db: &RootDatabaseImpl, symbol: &str) -> bool {
    let req = SymbolInfoRequest {
        symbol: Some(symbol.to_string()),
        position: None,
        locale: Locale::Ru,
        sections: SymbolInfoSections::all(),
        workspace_root: None,
    };
    symbol_info(db, &req).is_some()
}

/// Every `symbol` in the answer is fed straight back into the tool that owns the
/// parameter. A key that comes back empty is a dead end published as an address.
#[test]
fn every_published_symbol_is_accepted_back_by_symbol_info() {
    let db = stand();

    let mut checked = 0usize;
    let mut seen_triple = false;
    let mut seen_object = false;

    for needle in ["Значение", "Товары", "ОбщегоНазначения", "ЗначениеЗащищено"]
    {
        let query = NameQuery::new(needle, 200);
        for candidate in lookup_names(&db, &query, &[]).candidates {
            let Some(symbol) = candidate.symbol.as_deref() else { continue };
            // Platform members are a whole separate resolution path with its own
            // gate; this stand is about the workspace.
            if candidate.category == NameCategory::PlatformMember {
                continue;
            }
            assert!(
                card_for(&db, symbol),
                "`{symbol}` is published as a `symbol` but `symbol_info` answers nothing \
                 (category {:?}, provider {:?})",
                candidate.category,
                candidate.provider,
            );
            checked += 1;
            seen_triple |= symbol.matches('.').count() == 2;
            seen_object |= candidate.category == NameCategory::MetadataObject;
        }
    }

    // Without these the loop above is green on a stand that happens to contain
    // only the easy class.
    assert!(checked >= 3, "the stand published too few keys to prove anything: {checked}");
    assert!(seen_triple, "no object-module method was published — the hard class is missing");
    assert!(seen_object, "no metadata object was published — the XML class is missing");
}

/// The other half of the rule. An exported module variable is not a method and
/// not an object; `symbol_info` has no branch for it, so the dictionary must
/// hand back a place instead of a key that answers nothing.
#[test]
fn a_class_symbol_info_cannot_resolve_travels_by_location_only() {
    let db = stand();

    let found = lookup_names(&db, &NameQuery::new("ЗначениеСчётчика", 50), &[]);
    let variable = found
        .candidates
        .iter()
        .find(|c| c.category == NameCategory::ModuleVariable)
        .expect("the exported variable is in the dictionary");

    assert!(variable.symbol.is_none(), "{:?}", variable.symbol);
    assert!(variable.place.is_some());

    // The premise the assertion above rests on: were `symbol_info` to grow a
    // branch for module variables, this gate must be revisited rather than
    // quietly keep suppressing a now-valid key.
    assert!(!card_for(&db, "Счётчики.ЗначениеСчётчика"));
}

/// The same rule for a common module without a readable body. The listing knows
/// it and the module index does not, so its name is findable and openable but
/// never a key.
#[test]
fn a_module_without_a_body_is_found_by_place_and_not_by_name() {
    let db = stand();

    let found = lookup_names(&db, &NameQuery::new("ЗначениеЗащищено", 50), &[]);
    let module = found
        .candidates
        .iter()
        .find(|c| c.category == NameCategory::CommonModule)
        .expect("a protected module is still an answer to its own name");

    assert!(module.place.is_some(), "it is openable");
    assert!(module.symbol.is_none(), "{:?}", module.symbol);

    // The premise: the key withheld above is withheld because it does not work.
    assert!(!card_for(&db, "ЗначениеЗащищено"));
    // And the neighbouring module, whose body IS readable, still publishes one —
    // otherwise this gate would pass by suppressing the whole class.
    assert!(card_for(&db, "ОбщегоНазначения"));
}
