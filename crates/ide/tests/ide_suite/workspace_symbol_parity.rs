//! `workspace/symbol` keeps everything it used to find.
//!
//! The dictionary replaces a scan that had its own ranking and its own idea of
//! what a symbol is, so the risk is not that it finds too little in general —
//! it is that it drops a class quietly. The baseline below was taken from the
//! implementation being replaced, on a fixture built to contain the classes
//! most likely to be lost: two object modules that share a file stem, an
//! exported module variable, and a non-exported method that must stay out.

use ide::{lookup_names, NameCategory, NameQuery};
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::metadata::{MdoEntry, MetadataListingData};
use ide_db::RootDatabaseImpl;
use vfs::{file_set::FileSet, FileId, VfsPath};

const CONFIG_ROOT: &str = "/ws/src/cf";

const FILES: &[(&str, &str)] = &[
    (
        "/ws/src/cf/CommonModules/ОбщегоНазначения/Ext/Module.bsl",
        "Функция ЗначениеРеквизитаОбъекта() Экспорт\nКонецФункции\n\n\
         Функция ЗакрытоеЗначение()\nКонецФункции\n",
    ),
    (
        "/ws/src/cf/Catalogs/Товары/Ext/ObjectModule.bsl",
        "Процедура ЗначениеПоУмолчаниюТовара() Экспорт\nКонецПроцедуры\n",
    ),
    (
        "/ws/src/cf/Catalogs/Склады/Ext/ObjectModule.bsl",
        "Процедура ЗначениеПоУмолчаниюСклада() Экспорт\nКонецПроцедуры\n",
    ),
    (
        "/ws/src/cf/CommonModules/Счётчики/Ext/Module.bsl",
        "Перем ЗначениеСчётчика Экспорт;\n",
    ),
    (
        "/ws/src/cf/Catalogs/Товары.xml",
        "<MetaDataObject><Catalog><Properties><Name>Товары</Name></Properties></Catalog></MetaDataObject>",
    ),
];

/// What the replaced implementation returned for the query below, in its own
/// order. Recorded as data so it survives the deletion of the code that
/// produced it.
const BASELINE: &[&str] = &[
    "ЗначениеПоУмолчаниюСклада",
    "ЗначениеПоУмолчаниюТовара",
    "ЗначениеРеквизитаОбъекта",
    "ЗначениеСчётчика",
];

const QUERY: &str = "Значение";

fn stand() -> RootDatabaseImpl {
    let mut db = RootDatabaseImpl::new();
    let mut file_set = FileSet::default();
    for (i, (path, _)) in FILES.iter().enumerate() {
        file_set.insert(FileId(i as u32), VfsPath::new(*path));
    }
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    for (i, (_, text)) in FILES.iter().enumerate() {
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
                main: FileId(4),
                predefined: None,
            }],
            ..MetadataListingData::default()
        },
    );
    db
}

/// The baseline is a floor, not a ceiling: nothing it held may go missing, and
/// what is new has to belong to a category the replacement was meant to add.
#[test]
fn the_dictionary_keeps_everything_the_scan_used_to_find() {
    let db = stand();
    let found = lookup_names(&db, &NameQuery::new(QUERY, 256).requiring_location(), &[]);

    let names: Vec<&str> = found.candidates.iter().map(|c| c.display.as_str()).collect();
    for expected in BASELINE {
        assert!(names.contains(expected), "`{expected}` was lost; got {names:?}");
    }

    // Two object modules share the file stem `ObjectModule`; a table keyed by a
    // path-derived module name keeps one of them, and this is the query that
    // shows it.
    assert!(names.contains(&"ЗначениеПоУмолчаниюТовара"), "{names:?}");
    assert!(names.contains(&"ЗначениеПоУмолчаниюСклада"), "{names:?}");

    // Non-exported members were out before and stay out.
    assert!(!names.contains(&"ЗакрытоеЗначение"), "{names:?}");

    let allowed = [
        NameCategory::ModuleMethod,
        NameCategory::ModuleVariable,
        NameCategory::CommonModule,
        NameCategory::MetadataObject,
        NameCategory::MetadataMember,
        NameCategory::Form,
    ];
    for candidate in &found.candidates {
        assert!(
            allowed.contains(&candidate.category),
            "an unexpected category reached `workspace/symbol`: {candidate:?}",
        );
        assert!(candidate.place.is_some(), "a symbol with nowhere to jump: {candidate:?}");
    }
}

/// A platform member has no file, so the question `workspace/symbol` asks —
/// "what can I jump to" — excludes it at the source rather than downstream,
/// where a filter would drop rows without saying so.
///
/// And "at the source" is literal: the singleton is not walked at all. Asking a
/// source that cannot answer and discarding its work afterwards is the same
/// result at the cost of every platform type, method and property, on every
/// keystroke-sized query the editor sends.
#[test]
fn what_has_no_file_is_never_even_asked_for() {
    let db = stand();
    let found = lookup_names(&db, &NameQuery::new("СтрНайти", 256).requiring_location(), &[]);

    assert!(
        !found.candidates.iter().any(|c| c.category == ide::NameCategory::PlatformMember),
        "{:?}",
        found.candidates,
    );
    assert_eq!(
        found.state_of(ide::ProviderId::Platform),
        Some(ide::ProviderState::NotAsked),
        "the platform was walked and then discarded: {:?}",
        found.providers,
    );
    // Not a gap in the answer: the caller narrowed the question, and a narrowed
    // question is still completely answered.
    assert!(!found.is_partial());
}
