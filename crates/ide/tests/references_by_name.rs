//! The `references` surface in the `ide` layer: which symbol an anchor names,
//! and what the answer says when it names none, or more than one.
//!
//! Every check names the input on which it must fail. A reference search that
//! answers an empty list for everything would pass a suite of "no false hits"
//! assertions, so each one is paired with an input where the hit is required.

use ide::{
    find_references_by_name, lookup_names, resolve_declarations, symbol_info, NameQuery,
    ReferenceAnchor, ReferenceArea, ReferenceKind, ReferencesOutcome, ReferencesRequest,
    SymbolInfoRequest, SymbolInfoSections,
};
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::metadata::{MdoEntry, MetadataListingData};
use ide_db::RootDatabaseImpl;
use rustc_hash::FxHashSet;
use std::path::PathBuf;
use vfs::{FileId, FileSet, VfsPath};

const MAX_FILES: usize = 2000;

fn db_with(files: &[(&str, &str)]) -> (RootDatabaseImpl, Vec<FileId>) {
    let mut db = RootDatabaseImpl::new();
    let mut file_set = FileSet::default();
    let ids: Vec<FileId> = (0..files.len()).map(|i| FileId(i as u32)).collect();
    for (id, (path, _)) in ids.iter().zip(files) {
        file_set.insert(*id, VfsPath::new(*path));
    }
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    for (id, (_, text)) in ids.iter().zip(files) {
        db.set_file_source_root(*id, SourceRootId(0));
        db.set_file_text(*id, text);
    }
    (db, ids)
}

fn request(anchor: ReferenceAnchor) -> ReferencesRequest {
    ReferencesRequest {
        anchor,
        anchor_files: None,
        area: ReferenceArea::default(),
        kinds: None,
        include_declaration: true,
        max_files: MAX_FILES,
    }
}

fn by_name(db: &RootDatabaseImpl, name: &str) -> ide::ReferencesResult {
    find_references_by_name(db, &request(ReferenceAnchor::Name(name.to_string())), &[])
}

fn dictionary_candidates(db: &RootDatabaseImpl, name: &str) -> usize {
    lookup_names(db, &NameQuery::new(name, 64), &[]).candidates.len()
}

fn anchored_dictionary_candidates(db: &RootDatabaseImpl, name: &str) -> usize {
    lookup_names(db, &NameQuery::new(name, 64), &[])
        .candidates
        .iter()
        .filter(|candidate| candidate.place.is_some_and(|place| place.range.is_some()))
        .count()
}

const SALES: &str = "\
Процедура Расчёт() Экспорт
КонецПроцедуры
";

const CLIENT: &str = "\
Процедура Вызвать() Экспорт
    Продажи.Расчёт();
КонецПроцедуры
";

/// A method whose name is also a platform member (`Массив.Добавить`).
const PLATFORM_TWIN: &str = "\
Процедура Добавить() Экспорт
КонецПроцедуры
";

fn two_common_modules() -> (RootDatabaseImpl, Vec<FileId>) {
    db_with(&[
        ("/ws/src/cf/CommonModules/Продажи/Ext/Module.bsl", SALES),
        ("/ws/src/cf/CommonModules/Клиент/Ext/Module.bsl", CLIENT),
    ])
}

/// Gate Q — a qualified name is a working anchor, and the name dictionary alone
/// could not have served it.
#[test]
fn qualified_common_module_method_resolves() {
    let (db, files) = two_common_modules();

    let result = by_name(&db, "Продажи.Расчёт");

    assert_eq!(result.outcome, ReferencesOutcome::Resolved);
    assert_eq!(result.hits.len(), 2, "declaration + one call: {:?}", result.hits);
    assert!(result
        .hits
        .iter()
        .any(|hit| hit.file_id == files[0] && hit.kind == ReferenceKind::Declaration));
    assert!(result
        .hits
        .iter()
        .any(|hit| hit.file_id == files[1] && hit.kind == ReferenceKind::Call));

    assert_eq!(
        dictionary_candidates(&db, "Продажи.Расчёт"),
        0,
        "the control: the dictionary matches a short member name against the whole \
         needle, so stage one is what makes this anchor work — without this the test \
         would pass with no stage one at all",
    );
}

/// Gate Q3 — the third segment is not decoration. `module_name` is `ObjectModule`
/// for the object module of every metadata object, so an implementation keyed on
/// that field resolves no triple at all.
#[test]
fn qualified_object_module_method_resolves() {
    let (db, files) = db_with(&[
        (
            "/ws/src/cf/Catalogs/Товары/Ext/ObjectModule.bsl",
            "Процедура ПересчитатьИтоги() Экспорт\nКонецПроцедуры\n",
        ),
        (
            "/ws/src/cf/Catalogs/Услуги/Ext/ObjectModule.bsl",
            "Процедура ПересчитатьИтоги() Экспорт\nКонецПроцедуры\n",
        ),
    ]);

    let declarations = resolve_declarations(&db, "Справочник.Товары.ПересчитатьИтоги");
    assert_eq!(
        declarations.len(),
        1,
        "the triple must name ONE object module, not both: {declarations:?}",
    );
    assert_eq!(declarations[0].file_id, files[0]);

    let result = by_name(&db, "Справочник.Товары.ПересчитатьИтоги");
    assert_eq!(result.outcome, ReferencesOutcome::Resolved);
    assert!(result.hits.iter().all(|hit| hit.file_id == files[0]));

    assert_eq!(
        dictionary_candidates(&db, "Справочник.Товары.ПересчитатьИтоги"),
        0,
        "control: the dictionary cannot serve a triple either",
    );
}

/// Gate Am — a qualified name declared in two roots is ambiguous, and the way
/// out is narrowing the ANCHOR. This is the input a first-wins resolver passes
/// silently.
#[test]
fn same_named_common_modules_are_ambiguous_until_the_anchor_is_narrowed() {
    let (db, files) = db_with(&[
        ("/ws/src/cf/CommonModules/Продажи/Ext/Module.bsl", SALES),
        ("/ws/src/cfe/CommonModules/Продажи/Ext/Module.bsl", SALES),
    ]);

    let wide = by_name(&db, "Продажи.Расчёт");
    assert_eq!(wide.outcome, ReferencesOutcome::Ambiguous);
    assert_eq!(wide.declarations.len(), 2, "both declarations are offered");
    assert!(wide.hits.is_empty(), "an ambiguous anchor counts nothing");

    let narrowed = find_references_by_name(
        &db,
        &ReferencesRequest {
            anchor_files: Some(FxHashSet::from_iter([files[1]])),
            ..request(ReferenceAnchor::Name("Продажи.Расчёт".to_string()))
        },
        &[],
    );
    assert_eq!(narrowed.outcome, ReferencesOutcome::Resolved);
    assert!(narrowed.hits.iter().all(|hit| hit.file_id == files[1]));
}

/// Gate A — the same for a short name, which the dictionary answers. Control
/// one: narrowing resolves it, so `ambiguous` is not a dead end. Control two: a
/// single declaration resolves without narrowing.
#[test]
fn short_name_ambiguity_is_visible_and_resolvable() {
    let (db, files) = db_with(&[
        ("/ws/src/cf/CommonModules/Первый/Ext/Module.bsl", SALES),
        ("/ws/src/cf/CommonModules/Второй/Ext/Module.bsl", SALES),
    ]);

    let wide = by_name(&db, "Расчёт");
    assert_eq!(wide.outcome, ReferencesOutcome::Ambiguous);

    let narrowed = find_references_by_name(
        &db,
        &ReferencesRequest {
            anchor_files: Some(FxHashSet::from_iter([files[0]])),
            ..request(ReferenceAnchor::Name("Расчёт".to_string()))
        },
        &[],
    );
    assert_eq!(narrowed.outcome, ReferencesOutcome::Resolved);
    assert!(narrowed.hits.iter().all(|hit| hit.file_id == files[0]));

    let (single_db, _) = db_with(&[("/ws/src/cf/CommonModules/Первый/Ext/Module.bsl", SALES)]);
    assert_eq!(by_name(&single_db, "Расчёт").outcome, ReferencesOutcome::Resolved);
}

/// Gate Q1 (module half) — a module as a whole is found, and is not a symbol
/// references can be walked for. Control: the dictionary DOES see the name, it
/// just cannot anchor on it, so "no candidates" would be the wrong control here.
#[test]
fn a_whole_module_is_unsupported_not_missing() {
    let (db, _) = two_common_modules();

    let result = by_name(&db, "Продажи");
    assert!(
        matches!(result.outcome, ReferencesOutcome::UnsupportedSymbol { .. }),
        "got {:?}",
        result.outcome,
    );

    assert!(dictionary_candidates(&db, "Продажи") > 0, "the dictionary sees the module name",);
    assert_eq!(
        anchored_dictionary_candidates(&db, "Продажи"),
        0,
        "…but has no range to anchor on, which is why the outcome is not `resolved`",
    );
}

/// Gate O (third input) — an exported method with no references at all is
/// `resolved` with an empty list. Without this input, an implementation that
/// answered `unsupported_symbol` for everything would pass every check above.
#[test]
fn a_method_without_references_is_resolved_and_empty() {
    let (db, files) = db_with(&[(
        "/ws/src/cf/CommonModules/Продажи/Ext/Module.bsl",
        "Процедура Расчёт() Экспорт\nКонецПроцедуры\n",
    )]);

    let result = find_references_by_name(
        &db,
        &ReferencesRequest {
            include_declaration: false,
            ..request(ReferenceAnchor::Name("Продажи.Расчёт".to_string()))
        },
        &[],
    );

    assert_eq!(result.outcome, ReferencesOutcome::Resolved);
    assert!(result.hits.is_empty(), "no call sites exist: {:?}", result.hits);
    assert_eq!(result.files_scanned, 1);
    assert_eq!(files.len(), 1);
}

/// Gate Pm — a position that names nothing is `not_found`, not "zero
/// references". Control: a position on a real symbol with zero references is
/// `resolved` with an empty list, so the two cannot be confused.
#[test]
fn a_position_that_names_nothing_is_not_found() {
    let source = "// комментарий\nПроцедура Расчёт() Экспорт\nКонецПроцедуры\n";
    let (db, files) = db_with(&[("/ws/src/cf/CommonModules/Продажи/Ext/Module.bsl", source)]);

    let on_comment = find_references_by_name(
        &db,
        &request(ReferenceAnchor::Position { file_id: files[0], line: 0, column: 3 }),
        &[],
    );
    assert_eq!(on_comment.outcome, ReferencesOutcome::NotFound);

    let on_name = find_references_by_name(
        &db,
        &ReferencesRequest {
            include_declaration: false,
            ..request(ReferenceAnchor::Position { file_id: files[0], line: 1, column: 10 })
        },
        &[],
    );
    assert_eq!(on_name.outcome, ReferencesOutcome::Resolved);
    assert!(on_name.hits.is_empty());
}

/// Gate O (second input) — a platform member has no reference walk, and the
/// positional path says so instead of returning an empty list.
#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn a_position_on_a_platform_member_is_unsupported() {
    let source = "Процедура Тест()\n    Сообщить(1);\nКонецПроцедуры\n";
    let (db, files) = db_with(&[("/ws/src/cf/CommonModules/Продажи/Ext/Module.bsl", source)]);

    let result = find_references_by_name(
        &db,
        &request(ReferenceAnchor::Position { file_id: files[0], line: 1, column: 4 }),
        &[],
    );

    assert!(
        matches!(result.outcome, ReferencesOutcome::UnsupportedSymbol { .. }),
        "got {:?}",
        result.outcome,
    );
}

/// Gate N — narrowing by area is a subset of what was computed, and the count
/// follows the filter. The control is the wide answer: a filter that silently
/// matched nothing would leave both counts at zero and pass.
#[test]
fn area_narrows_the_answer_and_the_total() {
    let (db, files) = db_with(&[
        ("/ws/src/cf/CommonModules/Продажи/Ext/Module.bsl", SALES),
        ("/ws/src/cf/CommonModules/Клиент/Ext/Module.bsl", CLIENT),
        ("/ws/src/cfe/CommonModules/Расширение/Ext/Module.bsl", CLIENT),
    ]);

    let wide = by_name(&db, "Продажи.Расчёт");
    assert_eq!(wide.outcome, ReferencesOutcome::Resolved);
    assert_eq!(wide.hits.len(), 3, "declaration + two calls: {:?}", wide.hits);

    let narrowed = find_references_by_name(
        &db,
        &ReferencesRequest {
            area: ReferenceArea { files: Some(FxHashSet::from_iter([files[2]])) },
            ..request(ReferenceAnchor::Name("Продажи.Расчёт".to_string()))
        },
        &[],
    );
    assert_eq!(narrowed.outcome, ReferencesOutcome::Resolved);
    assert_eq!(narrowed.hits.len(), 1);
    assert!(narrowed.hits.iter().all(|hit| hit.file_id == files[2]));
    assert!(narrowed.hits.len() < wide.hits.len(), "the filter must actually cut");
}

/// Gate M — a truncated walk says so, and the flag is not stuck on. Control: the
/// same query with room to finish clears it and finds everything.
#[test]
fn a_truncated_walk_is_declared() {
    let (db, _) = db_with(&[
        ("/ws/src/cf/CommonModules/Продажи/Ext/Module.bsl", SALES),
        ("/ws/src/cf/CommonModules/Клиент/Ext/Module.bsl", CLIENT),
        ("/ws/src/cfe/CommonModules/Расширение/Ext/Module.bsl", CLIENT),
    ]);

    let capped = find_references_by_name(
        &db,
        &ReferencesRequest {
            max_files: 1,
            ..request(ReferenceAnchor::Name("Продажи.Расчёт".to_string()))
        },
        &[],
    );
    assert!(capped.files_capped, "one file out of three walked");
    assert_eq!(capped.files_scanned, 1);
    assert!(capped.hits.len() < 3, "a capped walk cannot have seen everything");

    let full = by_name(&db, "Продажи.Расчёт");
    assert!(!full.files_capped, "control: with room, nothing is capped");
    assert_eq!(full.hits.len(), 3);
}

/// The kind filter selects, and `include_declaration` is a separate switch —
/// both against an input where each actually removes something.
#[test]
fn kinds_and_include_declaration_filter_independently() {
    let (db, _) = two_common_modules();

    let all = by_name(&db, "Продажи.Расчёт");
    assert_eq!(all.hits.len(), 2);

    let calls_only = find_references_by_name(
        &db,
        &ReferencesRequest {
            kinds: Some(vec![ReferenceKind::Call]),
            ..request(ReferenceAnchor::Name("Продажи.Расчёт".to_string()))
        },
        &[],
    );
    assert_eq!(calls_only.hits.len(), 1);
    assert!(calls_only.hits.iter().all(|hit| hit.kind == ReferenceKind::Call));

    let without_declaration = find_references_by_name(
        &db,
        &ReferencesRequest {
            include_declaration: false,
            ..request(ReferenceAnchor::Name("Продажи.Расчёт".to_string()))
        },
        &[],
    );
    assert_eq!(without_declaration.hits.len(), 1);
    assert!(without_declaration.hits.iter().all(|hit| hit.kind != ReferenceKind::Declaration));
}

// --- an implicit local is one variable ---------------------------------------------------

const IMPLICIT_LOCAL_MODULE: &str = "/ws/src/cf/CommonModules/Первый/Ext/Module.bsl";

const REASSIGNED: &str = r#"
Процедура Тест() Экспорт
    Набор = 1;
    Сообщить(Набор);
    Набор = 2;
    Сообщить(Набор);
КонецПроцедуры
"#;

/// A text anchor certifies its coordinates by quoting what the caller read, and the
/// quote is matched everywhere it stands in the file. A quote holding occurrences on
/// both sides of a reassignment therefore names one variable twice — and an identity
/// split into slices reports that as an ambiguity between two places.
///
/// The quote is `(Набор)` and not the whole line: a whole line carries `Сообщить` too,
/// and that third candidate keeps the answer ambiguous for a reason that has nothing to
/// do with the variable. A quote unique to one line — `Набор = 2;` — is no gate either:
/// it resolves today and would resolve after, only with a different total.
#[test]
fn a_quote_spanning_a_reassignment_names_one_variable() {
    let (db, files) = db_with(&[(IMPLICIT_LOCAL_MODULE, REASSIGNED)]);

    let result = find_references_by_name(
        &db,
        &request(ReferenceAnchor::Text {
            file_id: files[0],
            line: None,
            column: None,
            content: "(Набор)".to_string(),
        }),
        &[],
    );

    assert!(
        matches!(result.outcome, ReferencesOutcome::Resolved),
        "the quote names one variable, not several places to choose between: {:?}, sites {:?}",
        result.outcome,
        result.anchor_sites.len(),
    );
    assert_eq!(result.hits.len(), 4, "every occurrence of the variable: {:?}", result.hits);
}

// --- metadata substrate -----------------------------------------------------------------

fn designer_fixture_path() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-metadata/fixtures/designer"))
}

const DOCUMENT_XML: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../bsl-metadata/fixtures/designer/Documents/Документ1.xml"
));

const DOCUMENT_FORM_MODULE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../bsl-metadata/fixtures/designer/Documents/Документ1/Forms/ФормаДокумента/Ext/Form/Module.bsl"
));

const CATALOG_XML: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../bsl-metadata/fixtures/designer/Catalogs/Справочник1.xml"
));

/// A db with one catalog wired into the metadata substrate the way the resident
/// host does, plus one common module.
fn db_with_catalog() -> RootDatabaseImpl {
    let mut db = RootDatabaseImpl::new();
    let module = FileId(0);
    let xml = FileId(1);
    let designer = designer_fixture_path();
    let mut file_set = FileSet::new();
    file_set.insert(module, VfsPath::new("/ws/src/cf/CommonModules/Продажи/Ext/Module.bsl"));
    file_set.insert(xml, VfsPath::from(designer.join("Catalogs/Справочник1.xml")));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    db.set_file_source_root(module, SourceRootId(0));
    db.set_file_source_root(xml, SourceRootId(0));
    db.set_file_text(module, SALES);
    db.set_file_text(xml, CATALOG_XML);
    db.set_all_config_paths(vec![(None, designer.clone())]);
    db.set_metadata_listing(
        &designer.to_string_lossy(),
        MetadataListingData {
            entries: vec![MdoEntry {
                kind: bsl_metadata::MdoType::Catalog,
                name: "Справочник1".to_string(),
                main: xml,
                predefined: None,
            }],
            ..MetadataListingData::default()
        },
    );
    db
}

/// The type an occurrence carries is a property of the program point, and it stays one
/// when the variable's identity stops depending on the write: navigating to the TYPE of
/// a read lands on whatever the last preceding write produced.
///
/// `type_definition` is the only surface that reads `SemanticSymbol::ty` for an implicit
/// local, and it answers `None` for `Число` and `Строка` — so the gate needs two writes of
/// two METADATA types. Wiring: config paths alone are not enough; navigation to XML goes
/// through the metadata listing, so both objects are put in the VFS and registered.
///
/// Regression gate: green before and after the identity change, red under a mutant that
/// freezes the symbol type to the first write's.
#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn the_type_of_an_implicit_local_follows_the_occurrence() {
    let module = FileId(0);
    let catalog_xml = FileId(1);
    let document_xml = FileId(2);
    let designer = designer_fixture_path();

    let mut db = RootDatabaseImpl::new();
    let mut file_set = FileSet::new();
    file_set.insert(module, VfsPath::new(IMPLICIT_LOCAL_MODULE));
    file_set.insert(catalog_xml, VfsPath::from(designer.join("Catalogs/Справочник1.xml")));
    file_set.insert(document_xml, VfsPath::from(designer.join("Documents/Документ1.xml")));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    for file in [module, catalog_xml, document_xml] {
        db.set_file_source_root(file, SourceRootId(0));
    }

    const TWO_METADATA_WRITES: &str = r#"
Процедура Тест() Экспорт
    Набор = Справочники.Справочник1.СоздатьЭлемент();
    Сообщить(Набор);
    Набор = Документы.Документ1.СоздатьДокумент();
    Сообщить(Набор);
КонецПроцедуры
"#;
    db.set_file_text(module, TWO_METADATA_WRITES);
    db.set_file_text(catalog_xml, CATALOG_XML);
    db.set_file_text(document_xml, DOCUMENT_XML);
    db.set_all_config_paths(vec![(None, designer.clone())]);
    db.set_metadata_listing(
        &designer.to_string_lossy(),
        MetadataListingData {
            entries: vec![
                MdoEntry {
                    kind: bsl_metadata::MdoType::Catalog,
                    name: "Справочник1".to_string(),
                    main: catalog_xml,
                    predefined: None,
                },
                MdoEntry {
                    kind: bsl_metadata::MdoType::Document,
                    name: "Документ1".to_string(),
                    main: document_xml,
                    predefined: None,
                },
            ],
            ..MetadataListingData::default()
        },
    );

    let reads: Vec<usize> = {
        let mut found = Vec::new();
        let mut from = 0usize;
        while let Some(at) = TWO_METADATA_WRITES[from..].find("Сообщить(Набор)") {
            found.push(from + at + "Сообщить(".len());
            from += at + "Сообщить(Набор)".len();
        }
        found
    };
    assert_eq!(reads.len(), 2, "the input must carry one read after each write");

    let analysis = ide::Analysis::from_database(db);
    let target_of = |offset: usize| {
        analysis
            .type_definition(module, u32::try_from(offset).unwrap())
            .unwrap_or_else(|| panic!("no type target for the read at {offset}"))
            .file_id
    };

    assert_eq!(target_of(reads[0]), catalog_xml, "the read after the first write");
    assert_eq!(target_of(reads[1]), document_xml, "the read after the second write");
}

/// Gate Q1 (metadata half) and gate O (first input) — a metadata object is
/// found and is not referenceable. Control: `symbol_info` answers for the same
/// string, so the name is genuinely resolvable and `not_found` would be a lie.
#[test]
fn a_metadata_object_is_unsupported_not_missing() {
    let db = db_with_catalog();

    let card = symbol_info(
        &db,
        &SymbolInfoRequest {
            symbol: Some("Справочник.Справочник1".to_string()),
            position: None,
            locale: ide::Locale::default(),
            sections: SymbolInfoSections::all(),
            workspace_root: None,
        },
    );
    assert!(card.is_some(), "control: the same string resolves as a card");

    let result = by_name(&db, "Справочник.Справочник1");
    assert!(
        matches!(result.outcome, ReferencesOutcome::UnsupportedSymbol { .. }),
        "got {:?}",
        result.outcome,
    );
}

/// An attribute of a metadata object is `unsupported_symbol` too, and this input is what
/// pins the label: the classifier accepts the kinds `symbol_info` publishes, so a renamed
/// label would quietly turn this answer into `not_found` — a claim that the name does not
/// exist. Control: `symbol_info` resolves the same string.
#[test]
fn a_metadata_attribute_is_unsupported_not_missing() {
    let db = db_with_catalog();

    let card = symbol_info(
        &db,
        &SymbolInfoRequest {
            symbol: Some("Справочник.Справочник1.Реквизит1".to_string()),
            position: None,
            locale: ide::Locale::default(),
            sections: SymbolInfoSections::all(),
            workspace_root: None,
        },
    );
    assert!(card.is_some(), "control: the same string resolves as a card");

    let result = by_name(&db, "Справочник.Справочник1.Реквизит1");
    assert!(
        matches!(result.outcome, ReferencesOutcome::UnsupportedSymbol { .. }),
        "got {:?}",
        result.outcome,
    );
}

/// Gate Q1 (platform half) — a platform type member is found and is not
/// referenceable either.
#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn a_platform_member_is_unsupported_not_missing() {
    let (db, _) = two_common_modules();

    let result = by_name(&db, "Массив.Добавить");

    assert!(
        matches!(result.outcome, ReferencesOutcome::UnsupportedSymbol { .. }),
        "got {:?}",
        result.outcome,
    );
}

/// Narrowing the anchor to a set of files must not change the CLASS of the answer. A
/// platform member belongs to no root by construction, so a root filter that dropped it
/// would turn `unsupported_symbol` into `not_found` — "no such name" said about a name the
/// search did find. The control is the same call without the narrowing.
#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn narrowing_the_anchor_does_not_turn_unsupported_into_missing() {
    let (db, files) = two_common_modules();

    let wide = by_name(&db, "Добавить");
    assert!(
        matches!(wide.outcome, ReferencesOutcome::UnsupportedSymbol { .. }),
        "control: a platform member has no reference walk, got {:?}",
        wide.outcome,
    );

    let narrowed = find_references_by_name(
        &db,
        &ReferencesRequest {
            anchor_files: Some(FxHashSet::from_iter(files.iter().copied())),
            ..request(ReferenceAnchor::Name("Добавить".to_string()))
        },
        &[],
    );
    assert!(
        matches!(narrowed.outcome, ReferencesOutcome::UnsupportedSymbol { .. }),
        "narrowing selects among declarations; it must not reclassify the outcome, got {:?}",
        narrowed.outcome,
    );
}

/// Narrowing the anchor away from every declaration is `not_found` — "no anchor in this
/// file set" — and not `unsupported_symbol`, which claims no reference walk exists for the
/// symbol at all. The control is the same name without the narrowing: there it resolves,
/// so the difference is the file set and nothing else.
#[test]
fn narrowing_past_every_declaration_is_missing_and_not_unsupported() {
    let (db, files) = db_with(&[
        ("/ws/src/cf/CommonModules/Первый/Ext/Module.bsl", SALES),
        ("/ws/src/cf/CommonModules/Второй/Ext/Module.bsl", CLIENT),
    ]);

    let control = find_references_by_name(
        &db,
        &ReferencesRequest {
            anchor_files: Some(FxHashSet::from_iter([files[0]])),
            ..request(ReferenceAnchor::Name("Расчёт".to_string()))
        },
        &[],
    );
    assert_eq!(control.outcome, ReferencesOutcome::Resolved, "control: the declaration is here");

    let elsewhere = find_references_by_name(
        &db,
        &ReferencesRequest {
            anchor_files: Some(FxHashSet::from_iter([files[1]])),
            ..request(ReferenceAnchor::Name("Расчёт".to_string()))
        },
        &[],
    );
    assert_eq!(
        elsewhere.outcome,
        ReferencesOutcome::NotFound,
        "a method excluded by the file set is missing from it, not unwalkable",
    );
}

/// The two rules above meet on a short name that is BOTH — a module method and a
/// platform member spelled the same way, which is the common case for `Добавить`,
/// `Найти`, `Записать`. Whether a walk exists is settled by the method; the file
/// set only picks which declaration is taken, so excluding the method leaves the
/// name missing from THAT SET and never unwalkable. The control is the same name
/// narrowed to the file that declares it, where it resolves.
#[test]
fn a_method_sharing_a_platform_name_is_missing_when_the_file_set_excludes_it() {
    let (db, files) = db_with(&[
        ("/ws/src/cf/CommonModules/Первый/Ext/Module.bsl", PLATFORM_TWIN),
        ("/ws/src/cf/CommonModules/Второй/Ext/Module.bsl", CLIENT),
    ]);

    let control = find_references_by_name(
        &db,
        &ReferencesRequest {
            anchor_files: Some(FxHashSet::from_iter([files[0]])),
            ..request(ReferenceAnchor::Name("Добавить".to_string()))
        },
        &[],
    );
    assert_eq!(
        control.outcome,
        ReferencesOutcome::Resolved,
        "control: the method declares the name here, so a walk exists",
    );

    let elsewhere = find_references_by_name(
        &db,
        &ReferencesRequest {
            anchor_files: Some(FxHashSet::from_iter([files[1]])),
            ..request(ReferenceAnchor::Name("Добавить".to_string()))
        },
        &[],
    );
    assert_eq!(
        elsewhere.outcome,
        ReferencesOutcome::NotFound,
        "the platform twin of an excluded method must not answer `unsupported_symbol`: \
         the walk exists, this file set just does not hold its declaration",
    );
}

/// A name nothing knows is `not_found` — the outcome the three above must not
/// collapse into.
#[test]
fn an_unknown_name_is_not_found() {
    let (db, _) = two_common_modules();

    let result = by_name(&db, "СовершенноНеизвестноеИмя");

    assert_eq!(result.outcome, ReferencesOutcome::NotFound);
}

// --- extension visibility ---------------------------------------------------------------

fn cfe_root(name: &str) -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-metadata/fixtures/cfe_dependencies"))
        .join(name)
}

/// Gate V — a reference from an extension that cannot see the declaration is not
/// a reference to it. The control is in the same input: the extension that DOES
/// declare the dependency must contribute its call, otherwise an
/// always-empty walk would pass.
#[test]
fn references_follow_the_declared_dependency_matrix() {
    let mut db = RootDatabaseImpl::new();
    let unit = FileId(0);
    let tests = FileId(1);
    let independent = FileId(2);

    let module_path = |root: &str, module: &str| {
        VfsPath::new(
            cfe_root(root)
                .join(format!("CommonModules/{module}/Ext/Module.bsl"))
                .to_string_lossy()
                .to_string(),
        )
    };
    let mut file_set = FileSet::default();
    file_set.insert(unit, module_path("yaxunit", "МодульЮнит"));
    file_set.insert(tests, module_path("tests_ext", "МодульТестов"));
    file_set.insert(independent, module_path("independent", "МодульНезависимый"));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));

    let caller = "Процедура Прогон() Экспорт\n    МодульЮнит.ЗапуститьТест();\nКонецПроцедуры\n";
    for (file_id, text) in [
        (unit, "Процедура ЗапуститьТест() Экспорт\nКонецПроцедуры\n"),
        (tests, caller),
        (independent, caller),
    ] {
        db.set_file_source_root(file_id, SourceRootId(0));
        db.set_file_text(file_id, text);
    }

    let paths = vec![
        (None, cfe_root("base")),
        (Some("yaxunit".to_string()), cfe_root("yaxunit")),
        (Some("tests".to_string()), cfe_root("tests_ext")),
        (Some("independent".to_string()), cfe_root("independent")),
    ];
    let canonical_paths =
        paths.iter().map(|(_, p)| std::fs::canonicalize(p).unwrap_or_else(|_| p.clone())).collect();
    db.set_workspace_configs_snapshot(ide_db::metadata::WorkspaceConfigsSnapshot {
        kinds: ide_db::metadata::RootKind::from_labels(&paths),
        paths,
        canonical_paths,
        // `tests` (slot 2) declares a dependency on `yaxunit` (slot 1).
        closures: vec![Vec::new(), Vec::new(), vec![1], Vec::new()],
        topological_order: vec![0, 1, 2, 3],
        fingerprint: None,
        source_exclusions: Default::default(),
    });

    let result = by_name(&db, "МодульЮнит.ЗапуститьТест");

    assert_eq!(result.outcome, ReferencesOutcome::Resolved);
    let files: Vec<FileId> = result.hits.iter().map(|hit| hit.file_id).collect();
    assert!(files.contains(&unit), "the declaration itself: {files:?}");
    assert!(
        files.contains(&tests),
        "the extension that declares the dependency must contribute its call: {files:?}",
    );
    assert!(
        !files.contains(&independent),
        "an extension that cannot see the declaration calls something else: {files:?}",
    );
}

/// The same visibility rule on a MANAGER module, not a common one.
///
/// The gate above proves the rule for one class of names. `dependsOn` is not a property of
/// common modules, so a rule proven on them alone is proven for a quarter of the surface —
/// and the resolver reaches a manager module by a different route. The control is the same
/// as above and in the same input: the extension that DECLARES the dependency must
/// contribute its call, or an always-empty walk would pass.
#[test]
fn manager_module_references_follow_the_declared_dependency_matrix() {
    let mut db = RootDatabaseImpl::new();
    let unit = FileId(0);
    let tests = FileId(1);
    let independent = FileId(2);
    let unit_xml = FileId(3);
    let base_xml = FileId(4);

    let manager_path = |root: &str| {
        VfsPath::new(
            cfe_root(root)
                .join("Catalogs/Товары/Ext/ManagerModule.bsl")
                .to_string_lossy()
                .to_string(),
        )
    };
    let module_path = |root: &str, module: &str| {
        VfsPath::new(
            cfe_root(root)
                .join(format!("CommonModules/{module}/Ext/Module.bsl"))
                .to_string_lossy()
                .to_string(),
        )
    };
    let mut file_set = FileSet::default();
    file_set.insert(unit, manager_path("yaxunit"));
    file_set.insert(tests, module_path("tests_ext", "МодульТестов"));
    file_set.insert(independent, module_path("independent", "МодульНезависимый"));
    file_set.insert(
        unit_xml,
        VfsPath::new(cfe_root("yaxunit").join("Catalogs/Товары.xml").to_string_lossy().to_string()),
    );
    file_set.insert(
        base_xml,
        VfsPath::new(cfe_root("base").join("Catalogs/Товары.xml").to_string_lossy().to_string()),
    );
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));

    let caller =
        "Процедура Прогон() Экспорт\n    Справочники.Товары.ПодготовитьДанные();\nКонецПроцедуры\n";
    for (file_id, text) in [
        (unit, "Процедура ПодготовитьДанные() Экспорт\nКонецПроцедуры\n"),
        (tests, caller),
        (independent, caller),
    ] {
        db.set_file_source_root(file_id, SourceRootId(0));
        db.set_file_text(file_id, text);
    }
    for xml in [unit_xml, base_xml] {
        db.set_file_source_root(xml, SourceRootId(0));
        let path = if xml == unit_xml { cfe_root("yaxunit") } else { cfe_root("base") };
        let text = std::fs::read_to_string(path.join("Catalogs/Товары.xml"))
            .expect("фикстура справочника");
        db.set_file_text(xml, &text);
    }

    let paths = vec![
        (None, cfe_root("base")),
        (Some("yaxunit".to_string()), cfe_root("yaxunit")),
        (Some("tests".to_string()), cfe_root("tests_ext")),
        (Some("independent".to_string()), cfe_root("independent")),
    ];
    let canonical_paths =
        paths.iter().map(|(_, p)| std::fs::canonicalize(p).unwrap_or_else(|_| p.clone())).collect();
    db.set_workspace_configs_snapshot(ide_db::metadata::WorkspaceConfigsSnapshot {
        kinds: ide_db::metadata::RootKind::from_labels(&paths),
        paths,
        canonical_paths,
        closures: vec![Vec::new(), Vec::new(), vec![1], Vec::new()],
        topological_order: vec![0, 1, 2, 3],
        fingerprint: None,
        source_exclusions: Default::default(),
    });

    let result = by_name(&db, "Справочник.Товары.ПодготовитьДанные");
    assert_eq!(result.outcome, ReferencesOutcome::Resolved, "{:?}", result.outcome);
    let files: Vec<FileId> = result.hits.iter().map(|hit| hit.file_id).collect();
    assert!(files.contains(&unit), "the declaration itself: {files:?}");
    assert!(
        files.contains(&tests),
        "the extension that declares the dependency must contribute its call: {files:?}",
    );
    assert!(
        !files.contains(&independent),
        "an extension that cannot see the declaration calls something else: {files:?}",
    );
}

/// The same rule on an OBJECT module — the third of the four classes with a reference walk.
#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn object_module_references_follow_the_declared_dependency_matrix() {
    let mut db = RootDatabaseImpl::new();
    let unit = FileId(0);
    let tests = FileId(1);
    let independent = FileId(2);
    let unit_xml = FileId(3);
    let base_xml = FileId(4);

    let object_path = |root: &str| {
        VfsPath::new(
            cfe_root(root)
                .join("Catalogs/Товары/Ext/ObjectModule.bsl")
                .to_string_lossy()
                .to_string(),
        )
    };
    let module_path = |root: &str, module: &str| {
        VfsPath::new(
            cfe_root(root)
                .join(format!("CommonModules/{module}/Ext/Module.bsl"))
                .to_string_lossy()
                .to_string(),
        )
    };
    let mut file_set = FileSet::default();
    file_set.insert(unit, object_path("yaxunit"));
    file_set.insert(tests, module_path("tests_ext", "МодульТестов"));
    file_set.insert(independent, module_path("independent", "МодульНезависимый"));
    file_set.insert(
        unit_xml,
        VfsPath::new(cfe_root("yaxunit").join("Catalogs/Товары.xml").to_string_lossy().to_string()),
    );
    file_set.insert(
        base_xml,
        VfsPath::new(cfe_root("base").join("Catalogs/Товары.xml").to_string_lossy().to_string()),
    );
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));

    let caller = "Процедура Прогон() Экспорт\n    Объект = Справочники.Товары.СоздатьЭлемент();\n    Объект.ПодготовитьОбъект();\nКонецПроцедуры\n";
    for (file_id, text) in [
        (unit, "Процедура ПодготовитьОбъект() Экспорт\nКонецПроцедуры\n"),
        (tests, caller),
        (independent, caller),
    ] {
        db.set_file_source_root(file_id, SourceRootId(0));
        db.set_file_text(file_id, text);
    }
    for (xml, root) in [(unit_xml, "yaxunit"), (base_xml, "base")] {
        db.set_file_source_root(xml, SourceRootId(0));
        let text = std::fs::read_to_string(cfe_root(root).join("Catalogs/Товары.xml"))
            .expect("фикстура справочника");
        db.set_file_text(xml, &text);
    }

    let paths = vec![
        (None, cfe_root("base")),
        (Some("yaxunit".to_string()), cfe_root("yaxunit")),
        (Some("tests".to_string()), cfe_root("tests_ext")),
        (Some("independent".to_string()), cfe_root("independent")),
    ];
    let canonical_paths =
        paths.iter().map(|(_, p)| std::fs::canonicalize(p).unwrap_or_else(|_| p.clone())).collect();
    db.set_workspace_configs_snapshot(ide_db::metadata::WorkspaceConfigsSnapshot {
        kinds: ide_db::metadata::RootKind::from_labels(&paths),
        paths,
        canonical_paths,
        closures: vec![Vec::new(), Vec::new(), vec![1], Vec::new()],
        topological_order: vec![0, 1, 2, 3],
        fingerprint: None,
        source_exclusions: Default::default(),
    });

    let result = by_name(&db, "Справочник.Товары.ПодготовитьОбъект");
    assert_eq!(result.outcome, ReferencesOutcome::Resolved, "{:?}", result.outcome);
    let files: Vec<FileId> = result.hits.iter().map(|hit| hit.file_id).collect();
    assert!(files.contains(&unit), "the declaration itself: {files:?}");
    assert!(
        files.contains(&tests),
        "the extension that declares the dependency must contribute its call: {files:?}",
    );
    assert!(
        !files.contains(&independent),
        "an extension that cannot see the declaration calls something else: {files:?}",
    );
}

/// The same rule on a RECORD-SET module — the fourth and last class of names with a
/// reference walk, and the one the issue had no gate for at all because the fixture
/// held no registers. The control is the same as the other three: the extension that
/// DECLARES the dependency must contribute its call, or an always-empty walk passes.
#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn record_set_module_references_follow_the_declared_dependency_matrix() {
    let mut db = RootDatabaseImpl::new();
    let unit = FileId(0);
    let tests = FileId(1);
    let independent = FileId(2);
    let unit_xml = FileId(3);
    let base_xml = FileId(4);

    let record_set_path = |root: &str| {
        VfsPath::new(
            cfe_root(root)
                .join("InformationRegisters/ЦеныТоваров/Ext/RecordSetModule.bsl")
                .to_string_lossy()
                .to_string(),
        )
    };
    let module_path = |root: &str, module: &str| {
        VfsPath::new(
            cfe_root(root)
                .join(format!("CommonModules/{module}/Ext/Module.bsl"))
                .to_string_lossy()
                .to_string(),
        )
    };
    let mut file_set = FileSet::default();
    file_set.insert(unit, record_set_path("yaxunit"));
    file_set.insert(tests, module_path("tests_ext", "МодульТестов"));
    file_set.insert(independent, module_path("independent", "МодульНезависимый"));
    file_set.insert(
        unit_xml,
        VfsPath::new(
            cfe_root("yaxunit")
                .join("InformationRegisters/ЦеныТоваров.xml")
                .to_string_lossy()
                .to_string(),
        ),
    );
    file_set.insert(
        base_xml,
        VfsPath::new(
            cfe_root("base")
                .join("InformationRegisters/ЦеныТоваров.xml")
                .to_string_lossy()
                .to_string(),
        ),
    );
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));

    let caller = "Процедура Прогон() Экспорт\n    Набор =                   РегистрыСведений.ЦеныТоваров.СоздатьНаборЗаписей();\n                      Набор.ПодготовитьНабор();\nКонецПроцедуры\n";
    for (file_id, text) in [
        (unit, "Процедура ПодготовитьНабор() Экспорт\nКонецПроцедуры\n"),
        (tests, caller),
        (independent, caller),
    ] {
        db.set_file_source_root(file_id, SourceRootId(0));
        db.set_file_text(file_id, text);
    }
    for (xml, root) in [(unit_xml, "yaxunit"), (base_xml, "base")] {
        db.set_file_source_root(xml, SourceRootId(0));
        let text =
            std::fs::read_to_string(cfe_root(root).join("InformationRegisters/ЦеныТоваров.xml"))
                .expect("фикстура регистра");
        db.set_file_text(xml, &text);
    }

    let paths = vec![
        (None, cfe_root("base")),
        (Some("yaxunit".to_string()), cfe_root("yaxunit")),
        (Some("tests".to_string()), cfe_root("tests_ext")),
        (Some("independent".to_string()), cfe_root("independent")),
    ];
    let canonical_paths =
        paths.iter().map(|(_, p)| std::fs::canonicalize(p).unwrap_or_else(|_| p.clone())).collect();
    db.set_workspace_configs_snapshot(ide_db::metadata::WorkspaceConfigsSnapshot {
        kinds: ide_db::metadata::RootKind::from_labels(&paths),
        paths,
        canonical_paths,
        closures: vec![Vec::new(), Vec::new(), vec![1], Vec::new()],
        topological_order: vec![0, 1, 2, 3],
        fingerprint: None,
        source_exclusions: Default::default(),
    });

    let result = by_name(&db, "РегистрСведений.ЦеныТоваров.ПодготовитьНабор");
    assert_eq!(result.outcome, ReferencesOutcome::Resolved, "{:?}", result.outcome);
    let files: Vec<FileId> = result.hits.iter().map(|hit| hit.file_id).collect();
    assert!(files.contains(&unit), "the declaration itself: {files:?}");
    assert!(
        files.contains(&tests),
        "the extension that declares the dependency must contribute its call: {files:?}",
    );
    assert!(
        !files.contains(&independent),
        "an extension that cannot see the declaration calls something else: {files:?}",
    );
}

/// The input on which PATH order and ROOT topology disagree.
///
/// `adopter` sorts before `base`, so the path-derived index — which sorts by path and
/// keeps the first — answers with the extension's body and loses the base
/// declaration. Root topology says the opposite: the base declaration wins. Every
/// other gate in this file uses roots whose alphabetical order already agrees with
/// their rank, so none of them can tell the two orders apart.
fn adopter_first_input() -> (RootDatabaseImpl, FileId, FileId, FileId) {
    let mut db = RootDatabaseImpl::new();
    let adopter_manager = FileId(0);
    let base_manager = FileId(1);
    let caller = FileId(2);
    let adopter_xml = FileId(3);
    let base_xml = FileId(4);

    let manager_path = |root: &str| {
        VfsPath::new(
            cfe_root(root)
                .join("Catalogs/Товары/Ext/ManagerModule.bsl")
                .to_string_lossy()
                .to_string(),
        )
    };
    let mut file_set = FileSet::default();
    file_set.insert(adopter_manager, manager_path("adopter"));
    file_set.insert(base_manager, manager_path("base"));
    file_set.insert(
        caller,
        VfsPath::new(
            cfe_root("adopter")
                .join("CommonModules/МодульАдоптера/Ext/Module.bsl")
                .to_string_lossy()
                .to_string(),
        ),
    );
    for (id, root) in [(adopter_xml, "adopter"), (base_xml, "base")] {
        file_set.insert(
            id,
            VfsPath::new(cfe_root(root).join("Catalogs/Товары.xml").to_string_lossy().to_string()),
        );
    }
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));

    for (file_id, text) in [
        (adopter_manager, "Процедура ПодготовитьДанные() Экспорт\nКонецПроцедуры\n"),
        (base_manager, "Процедура ПодготовитьДанные() Экспорт\nКонецПроцедуры\n"),
        (
            caller,
            "Процедура Прогон() Экспорт\n    Справочники.Товары.ПодготовитьДанные();\nКонецПроцедуры\n",
        ),
    ] {
        db.set_file_source_root(file_id, SourceRootId(0));
        db.set_file_text(file_id, text);
    }
    for (xml, root) in [(adopter_xml, "adopter"), (base_xml, "base")] {
        db.set_file_source_root(xml, SourceRootId(0));
        let text = std::fs::read_to_string(cfe_root(root).join("Catalogs/Товары.xml"))
            .expect("фикстура справочника");
        db.set_file_text(xml, &text);
    }

    let paths = vec![(None, cfe_root("base")), (Some("adopter".to_string()), cfe_root("adopter"))];
    let canonical_paths =
        paths.iter().map(|(_, p)| std::fs::canonicalize(p).unwrap_or_else(|_| p.clone())).collect();
    db.set_workspace_configs_snapshot(ide_db::metadata::WorkspaceConfigsSnapshot {
        kinds: ide_db::metadata::RootKind::from_labels(&paths),
        paths,
        canonical_paths,
        closures: vec![Vec::new(), Vec::new()],
        topological_order: vec![0, 1],
        fingerprint: None,
        source_exclusions: Default::default(),
    });

    (db, base_manager, adopter_manager, caller)
}

fn references_from(db: &RootDatabaseImpl, name: &str, anchor_file: FileId) -> Vec<FileId> {
    find_references_by_name(
        db,
        &ReferencesRequest {
            anchor: ReferenceAnchor::Name(name.to_string()),
            anchor_files: Some(FxHashSet::from_iter([anchor_file])),
            area: ReferenceArea::default(),
            kinds: None,
            include_declaration: true,
            max_files: MAX_FILES,
        },
        &[],
    )
    .hits
    .iter()
    .map(|hit| hit.file_id)
    .collect()
}

/// Gate I2 — the BASE declaration wins even when an extension's directory sorts
/// before it.
///
/// Asserted from both sides, because either half alone passes on a broken build: a
/// walk that answered nothing would satisfy "the call is not adopter's", and a walk
/// that answered everything would satisfy "the call is base's".
#[test]
fn the_base_declaration_wins_over_an_extension_that_sorts_first_by_path() {
    let (db, base_manager, adopter_manager, caller) = adopter_first_input();

    let base_hits = references_from(&db, "Справочник.Товары.ПодготовитьДанные", base_manager);
    assert!(
        base_hits.contains(&caller),
        "the call resolves to the BASE declaration, which outranks the earlier path: {base_hits:?}",
    );

    let adopter_hits = references_from(&db, "Справочник.Товары.ПодготовитьДанные", adopter_manager);
    assert!(
        !adopter_hits.contains(&caller),
        "the extension body sorts first by path but does not win: {adopter_hits:?}",
    );
}

/// Gate I3 — an unreadable body of HIGHER priority stops the walk instead of letting
/// a lower-priority body answer in its place.
///
/// The input needs the same inverted path order as I2, and the unreadability on the
/// opposite side: the base body is the one that outranks, so it is the one whose
/// unreadability must bar the verdict. Marking the extension unreadable instead would
/// prove nothing — the base is readable, outranks it, and answers before the walk
/// ever reaches the extension.
#[test]
fn an_unreadable_body_of_higher_priority_bars_a_lower_one_from_answering() {
    let (mut db, base_manager, adopter_manager, caller) = adopter_first_input();

    // The base body no longer declares the method AND cannot be read: if the barrier
    // is missing, the walk falls through to the extension and answers with it.
    db.set_file_text(base_manager, "");
    db.set_file_unreadable(base_manager);

    let adopter_hits = references_from(&db, "Справочник.Товары.ПодготовитьДанные", adopter_manager);
    assert!(
        !adopter_hits.contains(&caller),
        "an unread body of higher priority bars the lower one from answering: {adopter_hits:?}",
    );
}

/// A form module's method is addressed by the same qualified name `symbol_info` accepts, and
/// answering `not_found` for it would deny a name the surface itself resolves. The form
/// table is not the path-derived module table, so this is a route of its own — and the
/// control is `symbol_info` on the very same string.
#[test]
fn a_form_module_method_is_a_working_anchor() {
    let mut db = RootDatabaseImpl::new();
    let form = FileId(0);
    let xml = FileId(1);
    let designer = designer_fixture_path();
    let mut file_set = FileSet::new();
    file_set.insert(
        form,
        VfsPath::from(
            designer.join("Documents/Документ1/Forms/ФормаДокумента/Ext/Form/Module.bsl"),
        ),
    );
    file_set.insert(xml, VfsPath::from(designer.join("Documents/Документ1.xml")));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    for (file_id, text) in [(form, DOCUMENT_FORM_MODULE), (xml, DOCUMENT_XML)] {
        db.set_file_source_root(file_id, SourceRootId(0));
        db.set_file_text(file_id, text);
    }
    db.set_all_config_paths(vec![(None, designer.clone())]);
    db.set_metadata_listing(
        &designer.to_string_lossy(),
        MetadataListingData {
            entries: vec![MdoEntry {
                kind: bsl_metadata::MdoType::Document,
                name: "Документ1".to_string(),
                main: xml,
                predefined: None,
            }],
            ..MetadataListingData::default()
        },
    );

    let symbol = "Документ.Документ1.Форма.ФормаДокумента.ПриЗаписиНаСервере";
    let card = symbol_info(
        &db,
        &SymbolInfoRequest {
            symbol: Some(symbol.to_string()),
            position: None,
            locale: ide::Locale::default(),
            sections: SymbolInfoSections::all(),
            workspace_root: Some(ide::StripRoot::resolve(&designer)),
        },
    );
    assert!(card.is_some(), "control: the same string resolves as a card");

    let result = by_name(&db, symbol);
    assert_eq!(
        result.outcome,
        ReferencesOutcome::Resolved,
        "a name the card surface resolves must not come back missing",
    );
    assert!(
        result.hits.iter().any(|hit| hit.kind == ReferenceKind::Declaration),
        "the declaration itself is among the hits: {:?}",
        result.hits,
    );
}

/// One string, one meaning. `symbol_info` reads `Тип.Объект.Член` as an attribute before it
/// reads it as a module method, and this surface must not read it the other way: a catalog
/// with both an attribute and an exported method of that name would otherwise answer about
/// two different entities depending on which tool was asked. The control is the same stand
/// with a method name that collides with nothing.
#[test]
fn an_attribute_outranks_a_module_method_of_the_same_name() {
    let mut db = RootDatabaseImpl::new();
    let module = FileId(0);
    let xml = FileId(1);
    let designer = designer_fixture_path();
    let mut file_set = FileSet::new();
    file_set
        .insert(module, VfsPath::from(designer.join("Catalogs/Справочник1/Ext/ObjectModule.bsl")));
    file_set.insert(xml, VfsPath::from(designer.join("Catalogs/Справочник1.xml")));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    db.set_file_source_root(module, SourceRootId(0));
    db.set_file_source_root(xml, SourceRootId(0));
    // `Реквизит1` is an attribute of this catalog in the fixture XML, and here it is also
    // an exported method of its object module.
    db.set_file_text(
        module,
        "Процедура Реквизит1() Экспорт\nКонецПроцедуры\n\nПроцедура Пересчитать() Экспорт\nКонецПроцедуры\n",
    );
    db.set_file_text(xml, CATALOG_XML);
    db.set_all_config_paths(vec![(None, designer.clone())]);
    db.set_metadata_listing(
        &designer.to_string_lossy(),
        MetadataListingData {
            entries: vec![MdoEntry {
                kind: bsl_metadata::MdoType::Catalog,
                name: "Справочник1".to_string(),
                main: xml,
                predefined: None,
            }],
            ..MetadataListingData::default()
        },
    );

    let card = symbol_info(
        &db,
        &SymbolInfoRequest {
            symbol: Some("Справочник.Справочник1.Реквизит1".to_string()),
            position: None,
            locale: ide::Locale::default(),
            sections: SymbolInfoSections::all(),
            workspace_root: None,
        },
    );
    assert_eq!(card.expect("a card").kind, "attribute", "the card reads it as an attribute");

    let collided = by_name(&db, "Справочник.Справочник1.Реквизит1");
    assert!(
        matches!(collided.outcome, ReferencesOutcome::UnsupportedSymbol { .. }),
        "the reference surface must read the string the same way, got {:?}",
        collided.outcome,
    );

    // The control: a method name that collides with no member still resolves, so the rule
    // above did not simply disable module methods on triples.
    let plain = by_name(&db, "Справочник.Справочник1.Пересчитать");
    assert_eq!(plain.outcome, ReferencesOutcome::Resolved, "{:?}", plain.outcome);
}

/// One string, one meaning — on the form route too. `symbol_info` reads a form member as an
/// attribute, then as an item, and only then as a handler, so a form carrying both an
/// attribute `Объект` and a procedure `Объект()` must not answer about the procedure here.
/// The control is a handler of the same form, which collides with nothing and still
/// resolves.
#[test]
fn a_form_attribute_outranks_a_form_module_method_of_the_same_name() {
    let mut db = RootDatabaseImpl::new();
    let form = FileId(0);
    let xml = FileId(1);
    let designer = designer_fixture_path();
    let mut file_set = FileSet::new();
    file_set.insert(
        form,
        VfsPath::from(
            designer.join("Documents/Документ1/Forms/ФормаДокумента/Ext/Form/Module.bsl"),
        ),
    );
    file_set.insert(xml, VfsPath::from(designer.join("Documents/Документ1.xml")));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    db.set_file_source_root(form, SourceRootId(0));
    db.set_file_source_root(xml, SourceRootId(0));
    // `Объект` is the form's main attribute in the fixture XML, and here it is also a
    // procedure of the same form module.
    db.set_file_text(
        form,
        "Процедура Объект()\nКонецПроцедуры\n\nПроцедура ПриЗаписиНаСервере()\nКонецПроцедуры\n",
    );
    db.set_file_text(xml, DOCUMENT_XML);
    db.set_all_config_paths(vec![(None, designer.clone())]);
    db.set_metadata_listing(
        &designer.to_string_lossy(),
        MetadataListingData {
            entries: vec![MdoEntry {
                kind: bsl_metadata::MdoType::Document,
                name: "Документ1".to_string(),
                main: xml,
                predefined: None,
            }],
            ..MetadataListingData::default()
        },
    );

    let symbol = "Документ.Документ1.Форма.ФормаДокумента.Объект";
    let card = symbol_info(
        &db,
        &SymbolInfoRequest {
            symbol: Some(symbol.to_string()),
            position: None,
            locale: ide::Locale::default(),
            sections: SymbolInfoSections::all(),
            workspace_root: Some(ide::StripRoot::resolve(&designer)),
        },
    );
    assert_eq!(
        card.expect("a card").kind,
        "form attribute",
        "control: the card surface reads this string as the attribute",
    );

    let collided = by_name(&db, symbol);
    assert!(
        matches!(collided.outcome, ReferencesOutcome::UnsupportedSymbol { .. }),
        "the reference surface must read the string the same way, got {:?}",
        collided.outcome,
    );

    let handler = by_name(&db, "Документ.Документ1.Форма.ФормаДокумента.ПриЗаписиНаСервере");
    assert_eq!(
        handler.outcome,
        ReferencesOutcome::Resolved,
        "the control: the rule above did not disable form methods, got {:?}",
        handler.outcome,
    );
}

/// A form an extension adopts exists twice, and both copies answer to one name. The form
/// table of `ModuleIndex` is first-wins, so an implementation resting on it reports one
/// declaration and a rename taken from that answer silently misses the other module. The
/// control is the same stand with only the base file registered, where the name resolves.
#[test]
fn an_adopted_form_is_ambiguous_and_not_first_wins() {
    let designer = designer_fixture_path();
    let base = designer.join("Documents/Документ1/Forms/ФормаДокумента/Ext/Form/Module.bsl");
    let adopted =
        PathBuf::from("/ws/src/cfe/Documents/Документ1/Forms/ФормаДокумента/Ext/Form/Module.bsl");
    let module = "Процедура ПриЗаписиНаСервере()\nКонецПроцедуры\n";
    let symbol = "Документ.Документ1.Форма.ФормаДокумента.ПриЗаписиНаСервере";

    let mut db = RootDatabaseImpl::new();
    let form = FileId(0);
    let mut file_set = FileSet::new();
    file_set.insert(form, VfsPath::from(base.clone()));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    db.set_file_source_root(form, SourceRootId(0));
    db.set_file_text(form, module);
    assert_eq!(
        by_name(&db, symbol).outcome,
        ReferencesOutcome::Resolved,
        "control: one copy of the form is one anchor",
    );

    let mut db = RootDatabaseImpl::new();
    let extension = FileId(1);
    let mut file_set = FileSet::new();
    file_set.insert(form, VfsPath::from(base));
    file_set.insert(extension, VfsPath::from(adopted));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    for file_id in [form, extension] {
        db.set_file_source_root(file_id, SourceRootId(0));
        db.set_file_text(file_id, module);
    }

    assert_eq!(
        resolve_declarations(&db, symbol).len(),
        2,
        "both modules declare the method the name addresses",
    );
    assert_eq!(
        by_name(&db, symbol).outcome,
        ReferencesOutcome::Ambiguous,
        "picking one of two copies silently is what the multiplicity exists to prevent",
    );
}
/// A module method may be named like a platform global — `Сообщить`, `Формат`, `Тип` are
/// ordinary wrapper names — and inside its own module that name means the method. Asking
/// the platform first made the declaration unreachable from its own uses, and the walk then
/// called a method it had just found "a symbol with no references". Both anchor routes are
/// checked, because they reach the walk by different paths, and `symbol_info` is the
/// control: the card and the walk must agree on what the string is.
#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn a_method_named_like_a_platform_global_is_walkable() {
    let (db, files) = db_with(&[(
        "/ws/src/cf/CommonModules/Продажи/Ext/Module.bsl",
        "Процедура Сообщить() Экспорт\nКонецПроцедуры\n\nПроцедура Вызвать() Экспорт\n    \
         Сообщить();\nКонецПроцедуры\n",
    )]);

    let card = symbol_info(
        &db,
        &SymbolInfoRequest {
            symbol: None,
            position: Some(ide::SymbolPosition { file_id: files[0], line: 0, column: 10 }),
            locale: ide::Locale::default(),
            sections: SymbolInfoSections::all(),
            workspace_root: None,
        },
    );
    assert_eq!(
        card.expect("a card").kind,
        "method",
        "control: the card surface reads the declaration as the module's own method",
    );

    for anchor in ["Продажи.Сообщить", "Сообщить"] {
        let result = by_name(&db, anchor);
        assert_eq!(
            result.outcome,
            ReferencesOutcome::Resolved,
            "`{anchor}` names a method that exists, got {:?}",
            result.outcome,
        );
        assert_eq!(
            result.hits.len(),
            2,
            "the declaration and the call inside the same module: {:?}",
            result.hits,
        );
    }

    // The control for the rule, not for the name: with no method of that name, the same
    // string is the platform's and has no walk — so the branch above is not simply
    // "never answer unsupported for a platform name".
    let (bare, _) = db_with(&[("/ws/src/cf/CommonModules/Продажи/Ext/Module.bsl", SALES)]);
    assert!(
        matches!(by_name(&bare, "Сообщить").outcome, ReferencesOutcome::UnsupportedSymbol { .. }),
        "with nothing declaring it, the name is the platform's",
    );
}

/// The anchor is chosen from a candidate list the dictionary caps, and past the cap the cap
/// decides the OUTCOME: a declaration that fell off it is one the answer could have been
/// ambiguous about. Unlike `max_files`, nothing about the hits shows it, so the flag is the
/// only way the caller can tell. The control is the same query below the cap, where the
/// flag is off.
#[test]
fn an_anchor_chosen_from_a_capped_candidate_list_says_so() {
    let mut files: Vec<(String, String)> = Vec::new();
    for i in 0..70 {
        files.push((
            format!("/ws/src/cf/CommonModules/Модуль{i}/Ext/Module.bsl"),
            "Процедура ПересчитатьИтоги() Экспорт\nКонецПроцедуры\n".to_string(),
        ));
    }
    let borrowed: Vec<(&str, &str)> =
        files.iter().map(|(path, text)| (path.as_str(), text.as_str())).collect();
    let (db, _) = db_with(&borrowed);

    let capped = by_name(&db, "ПересчитатьИтоги");
    assert!(
        capped.anchor_candidates_capped,
        "70 modules declare it and the dictionary caps at 64: {:?}",
        capped.outcome,
    );

    let (small, _) = db_with(&borrowed[..2]);
    let whole = by_name(&small, "ПересчитатьИтоги");
    assert!(
        !whole.anchor_candidates_capped,
        "control: two candidates fit, so nothing was capped: {:?}",
        whole.outcome,
    );
    assert_eq!(
        whole.outcome,
        ReferencesOutcome::Ambiguous,
        "control: the two declarations are what the flag would be hiding",
    );
}

/// One string, one meaning — for the three modules a `Тип.Объект.Метод` name can reach.
/// `symbol_info` reads the object module first and returns on the first hit, so a manager
/// module that happens to declare the same name does not make the string ambiguous. Merging
/// the three into one owner reported both and left the object module's own method
/// unreachable by name. The control is the same stand without the object module, where the
/// manager module answers.
#[test]
fn an_object_module_outranks_a_manager_module_of_the_same_object() {
    let method = "Процедура Пересчитать() Экспорт\nКонецПроцедуры\n";
    let (both, files) = db_with(&[
        ("/ws/src/cf/Catalogs/Товары/Ext/ObjectModule.bsl", method),
        ("/ws/src/cf/Catalogs/Товары/Ext/ManagerModule.bsl", method),
    ]);

    let declarations = resolve_declarations(&both, "Справочник.Товары.Пересчитать");
    assert_eq!(declarations.len(), 1, "the object module answers alone: {declarations:?}",);
    assert_eq!(declarations[0].file_id, files[0], "and it is the object module's method");
    assert_eq!(
        by_name(&both, "Справочник.Товары.Пересчитать").outcome,
        ReferencesOutcome::Resolved
    );

    let (manager_only, manager_files) =
        db_with(&[("/ws/src/cf/Catalogs/Товары/Ext/ManagerModule.bsl", method)]);
    let fallback = resolve_declarations(&manager_only, "Справочник.Товары.Пересчитать");
    assert_eq!(
        fallback.first().map(|declaration| declaration.file_id),
        Some(manager_files[0]),
        "control: with no object module the manager module is what the name reaches",
    );
}

/// The priority between an object module and a manager module holds INSIDE one object and
/// nowhere else. Ranking across the workspace let one root's object module delete another
/// root's manager module — the copy an `anchor_root_id` was meant to reach then vanished
/// into `not_found`. The control is the single-root stand next door, where the priority
/// does apply.
#[test]
fn the_module_priority_does_not_reach_across_roots() {
    let method = "Процедура Пересчитать() Экспорт\nКонецПроцедуры\n";
    let (db, files) = db_with(&[
        ("/ws/src/cf/Catalogs/Товары/Ext/ManagerModule.bsl", method),
        ("/ws/src/cfe/Catalogs/Товары/Ext/ObjectModule.bsl", method),
    ]);

    let declarations = resolve_declarations(&db, "Справочник.Товары.Пересчитать");
    assert_eq!(
        declarations.len(),
        2,
        "each root holds its own object, and both answer the name: {declarations:?}",
    );

    let narrowed = find_references_by_name(
        &db,
        &ReferencesRequest {
            anchor_files: Some(FxHashSet::from_iter([files[0]])),
            ..request(ReferenceAnchor::Name("Справочник.Товары.Пересчитать".to_string()))
        },
        &[],
    );
    assert_eq!(
        narrowed.outcome,
        ReferencesOutcome::Resolved,
        "the manager module IS the declaration of that root, so narrowing to it resolves",
    );
}

/// `symbol_info` reads a triple as a METHOD — it never looks for a variable there — so a
/// variable exported by the object module must not hide a manager-module method of the same
/// name. The control is the same pair with the object module declaring a method, where the
/// object module wins.
#[test]
fn an_exported_variable_does_not_hide_a_manager_method_of_the_same_name() {
    let (db, files) = db_with(&[
        ("/ws/src/cf/Catalogs/Товары/Ext/ObjectModule.bsl", "Перем Флаг Экспорт;\n"),
        (
            "/ws/src/cf/Catalogs/Товары/Ext/ManagerModule.bsl",
            "Процедура Флаг() Экспорт\nКонецПроцедуры\n",
        ),
    ]);

    let declarations = resolve_declarations(&db, "Справочник.Товары.Флаг");
    assert_eq!(declarations.len(), 1, "the method answers alone: {declarations:?}");
    assert_eq!(declarations[0].file_id, files[1], "and it is the manager module's method");
    assert_eq!(declarations[0].kind, ide::DeclarationKind::Method);

    let (methods, method_files) = db_with(&[
        (
            "/ws/src/cf/Catalogs/Товары/Ext/ObjectModule.bsl",
            "Процедура Флаг() Экспорт\nКонецПроцедуры\n",
        ),
        (
            "/ws/src/cf/Catalogs/Товары/Ext/ManagerModule.bsl",
            "Процедура Флаг() Экспорт\nКонецПроцедуры\n",
        ),
    ]);
    let control = resolve_declarations(&methods, "Справочник.Товары.Флаг");
    assert_eq!(
        control.first().map(|declaration| declaration.file_id),
        Some(method_files[0]),
        "control: between two methods the object module is the one the card surface reads",
    );
}

/// The service level `Ext` is optional in a module path — the parser accepts both layouts —
/// so the object a module belongs to cannot be "two segments up". Cutting a fixed two put
/// one object's two modules in different groups (priority lost) and two different objects of
/// the flat layout in one (a declaration silently dropped). Both halves are checked, because
/// the same wrong key produces opposite damage on them.
#[test]
fn the_object_of_a_module_is_found_in_both_path_layouts() {
    let method = "Процедура Пересчитать() Экспорт\nКонецПроцедуры\n";

    // One object, two layouts: the priority must still apply.
    let (mixed, mixed_files) = db_with(&[
        ("/ws/src/cf/Catalogs/Товары/Ext/ObjectModule.bsl", method),
        ("/ws/src/cf/Catalogs/Товары/ManagerModule.bsl", method),
    ]);
    let declarations = resolve_declarations(&mixed, "Справочник.Товары.Пересчитать");
    assert_eq!(
        declarations.len(),
        1,
        "one object holds both modules however they are laid out: {declarations:?}",
    );
    assert_eq!(declarations[0].file_id, mixed_files[0], "and the object module answers");

    // Two objects in the flat layout: neither may swallow the other's declaration.
    let (flat, flat_files) = db_with(&[
        ("/ws/src/cf/Catalogs/Товары/ObjectModule.bsl", method),
        ("/ws/src/cf/Catalogs/ТОВАРЫ/ManagerModule.bsl", method),
    ]);
    let both = resolve_declarations(&flat, "Справочник.Товары.Пересчитать");
    assert_eq!(both.len(), 2, "two spellings are two objects, and the name reaches both: {both:?}",);
    assert!(
        both.iter().any(|declaration| declaration.file_id == flat_files[1]),
        "the manager module of the second spelling must not be ranked away: {both:?}",
    );
}

// --- the self-certifying anchor -----------------------------------------------------------

fn by_text(
    db: &RootDatabaseImpl,
    file_id: FileId,
    line: Option<u32>,
    column: Option<u32>,
    content: &str,
) -> ide::ReferencesResult {
    find_references_by_name(
        db,
        &request(ReferenceAnchor::Text { file_id, line, column, content: content.to_owned() }),
        &[],
    )
}

/// The same module after someone renamed the method — the line the caller quoted is gone,
/// and its coordinates now name a different symbol.
const SALES_RENAMED: &str = "\
Процедура Пересчитать() Экспорт
КонецПроцедуры
";

/// Gate I6 — a quote the file no longer carries anchors nothing, and the coordinates alone
/// would have answered anyway.
#[test]
fn a_quote_the_file_does_not_carry_yields_no_references() {
    let (renamed, files) = db_with(&[
        ("/ws/src/cf/CommonModules/Продажи/Ext/Module.bsl", SALES_RENAMED),
        ("/ws/src/cf/CommonModules/Клиент/Ext/Module.bsl", CLIENT),
    ]);

    let quoted = by_text(&renamed, files[0], Some(0), None, "Процедура Расчёт() Экспорт");
    assert_eq!(
        quoted.outcome,
        ReferencesOutcome::AnchorStale {
            reason: ide::AnchorStaleReason::NotInFile,
            line_matches: 0,
        },
    );
    assert!(quoted.hits.is_empty(), "a stale anchor counts nothing: {:?}", quoted.hits);

    // What the anchor exists to close: the very same coordinates, taken on their own, answer
    // confidently about the symbol that moved into them.
    let positional = find_references_by_name(
        &renamed,
        &request(ReferenceAnchor::Position { file_id: files[0], line: 0, column: 10 }),
        &[],
    );
    assert_eq!(positional.outcome, ReferencesOutcome::Resolved);
    assert!(
        !positional.hits.is_empty(),
        "the control for the control: coordinates alone do answer, and about another symbol",
    );

    // The control: the file the caller actually read still answers the quote.
    let (intact, files) = two_common_modules();
    let quoted = by_text(&intact, files[0], Some(0), None, "Процедура Расчёт() Экспорт");
    assert_eq!(quoted.outcome, ReferencesOutcome::Resolved);
    assert_eq!(quoted.hits.len(), 2, "declaration + one call: {:?}", quoted.hits);
}

/// The module with three lines inserted above the declaration the caller quoted.
const SALES_SHIFTED: &str = "\
// Пересчёт продаж.
// Второй комментарий.
// Третий комментарий.
Процедура Расчёт() Экспорт
КонецПроцедуры
";

/// Gate I7 — lines inserted above the quote move the anchor with it, and the answer says so.
#[test]
fn lines_inserted_above_relocate_the_anchor_instead_of_changing_the_answer() {
    let (before, files) = two_common_modules();
    let original = by_text(&before, files[0], Some(0), None, "Процедура Расчёт() Экспорт");
    assert_eq!(original.outcome, ReferencesOutcome::Resolved);
    assert_eq!(original.anchor_line, Some(0));

    let (after, files) = db_with(&[
        ("/ws/src/cf/CommonModules/Продажи/Ext/Module.bsl", SALES_SHIFTED),
        ("/ws/src/cf/CommonModules/Клиент/Ext/Module.bsl", CLIENT),
    ]);
    // The caller sends the line number it read before the insertion.
    let relocated = by_text(&after, files[0], Some(0), None, "Процедура Расчёт() Экспорт");

    assert_eq!(relocated.outcome, ReferencesOutcome::Resolved);
    assert_eq!(relocated.hits.len(), original.hits.len(), "the same symbol, the same count");
    assert_eq!(
        relocated.anchor_line,
        Some(3),
        "the anchor stands where the quote went, not where it was sent",
    );
}

/// Two methods, each with a parameter of the same name: one quoted line, two different
/// symbols.
const TWO_LOCALS: &str = "\
Процедура Первая(Значение) Экспорт
    Значение = 1;
КонецПроцедуры

Процедура Вторая(Значение) Экспорт
    Значение = 2;
КонецПроцедуры
";

/// One method, one parameter, written on four lines: one quoted line, one symbol.
const ONE_LOCAL_MANY_LINES: &str = "\
Процедура Одна(Счётчик) Экспорт
    Счётчик = Счётчик + 1;
    Счётчик = Счётчик + 1;
    Счётчик = Счётчик + 1;
КонецПроцедуры
";

/// Gate I8 — a quote matching more than one SYMBOL is an ambiguity, and one matching one
/// symbol on many lines is not.
#[test]
fn a_quote_matching_two_symbols_is_ambiguous_and_one_matching_five_lines_is_not() {
    let (db, files) = db_with(&[("/ws/src/cf/CommonModules/Первый/Ext/Module.bsl", TWO_LOCALS)]);
    let ambiguous = by_text(&db, files[0], None, None, "Значение = ");

    assert_eq!(ambiguous.outcome, ReferencesOutcome::Ambiguous);
    assert_eq!(
        ambiguous.anchor_sites.len(),
        2,
        "an ambiguity without the places to choose from is the same silence, one level up: {:?}",
        ambiguous.anchor_sites,
    );
    assert!(ambiguous.hits.is_empty(), "an ambiguity counts nothing");

    // The control: the same shape of input — one quote, several matching lines — where the
    // lines all carry ONE symbol and there is nothing to choose between.
    let (db, files) =
        db_with(&[("/ws/src/cf/CommonModules/Первый/Ext/Module.bsl", ONE_LOCAL_MANY_LINES)]);
    let resolved = by_text(&db, files[0], None, None, "Счётчик");
    assert_eq!(resolved.outcome, ReferencesOutcome::Resolved);
    assert!(!resolved.hits.is_empty(), "one symbol on many lines is one answer");
}

/// A module carrying one line for every way a quote can fail to anchor.
const FOUR_REFUSALS: &str = "\
// просто комментарий
Процедура Считать() Экспорт
    Результат = Продажи.Расчёт(Параметр);
КонецПроцедуры
";

/// Gate I9 — every reachable refusal has a code of its own, and the four inputs tell them
/// apart on one file.
#[test]
fn each_way_a_quote_fails_to_anchor_has_its_own_code() {
    let (db, files) = db_with(&[
        ("/ws/src/cf/CommonModules/Считыватель/Ext/Module.bsl", FOUR_REFUSALS),
        ("/ws/src/cf/CommonModules/Продажи/Ext/Module.bsl", SALES),
    ]);
    let file = files[0];
    let reason = |result: &ide::ReferencesResult| match result.outcome {
        ReferencesOutcome::AnchorStale { reason, .. } => reason,
        ref other => panic!("expected a stale anchor, got {other:?}"),
    };

    assert_eq!(
        reason(&by_text(&db, file, None, None, "Процедура Отсутствует()")),
        ide::AnchorStaleReason::NotInFile,
    );
    assert_eq!(
        reason(&by_text(&db, file, Some(0), None, "// просто комментарий")),
        ide::AnchorStaleReason::NoNameInQuotedText,
    );
    assert_eq!(
        reason(&by_text(&db, file, Some(3), None, "КонецПроцедуры")),
        ide::AnchorStaleReason::NameNotResolved,
    );
    // `Результат` stands at column 4 of the quoted line and is not part of the quote: the
    // caller's aim and its quote disagree, which is a different mistake from either.
    assert_eq!(
        reason(&by_text(&db, file, Some(2), Some(4), "Продажи.Расчёт(Параметр)")),
        ide::AnchorStaleReason::ColumnOutsideQuotedText,
    );

    // The control: the same file, the same quote, aimed at a name the quote does carry.
    let resolved = by_text(&db, file, Some(2), Some(16), "Продажи.Расчёт(Параметр)");
    assert_eq!(resolved.outcome, ReferencesOutcome::Resolved);
}

/// A quote naming a property that semantics cannot resolve, on a line that also holds a
/// SHORTER identifier whose text is a substring of the quoted one.
const SHORTER_NAME_INSIDE: &str = "\
Процедура Тест(Номер) Экспорт
    Номер = Объект.НомерСтроки;
КонецПроцедуры
";

/// A name is quoted or it is not, and a name is not quoted by a longer word that happens to
/// contain it. Otherwise the anchor answers confidently about the neighbour — the silent
/// wrong answer it exists to close, one identifier over.
#[test]
fn a_longer_identifier_does_not_quote_the_shorter_one_inside_it() {
    let (db, files) =
        db_with(&[("/ws/src/cf/CommonModules/Первый/Ext/Module.bsl", SHORTER_NAME_INSIDE)]);

    let result = by_text(&db, files[0], None, None, "НомерСтроки");

    assert_eq!(
        result.outcome,
        ReferencesOutcome::AnchorStale {
            reason: ide::AnchorStaleReason::NameNotResolved,
            line_matches: 1,
        },
        "`Номер` is a substring of the quote, not a name in it: {result:?}",
    );
    assert!(result.hits.is_empty(), "nothing was anchored, so nothing is counted");

    // The control: the same file, the same reader, quoting the shorter name itself. The
    // assertion above is about which names the quote carries, not about a reader that
    // stopped matching.
    let control = by_text(&db, files[0], None, None, "Номер = ");
    assert_eq!(control.outcome, ReferencesOutcome::Resolved);
    assert!(!control.hits.is_empty(), "{control:?}");
}

/// Gate — the qualified name published beside an ambiguity is the one that addresses the
/// SYMBOL, and a method lives in its own module, not in the file the quote came from.
#[test]
fn an_anchor_site_is_named_by_the_module_that_declares_it() {
    let (db, files) = db_with(&[
        ("/ws/src/cf/CommonModules/Продажи/Ext/Module.bsl", SALES),
        ("/ws/src/cf/CommonModules/Склад/Ext/Module.bsl", SALES),
        (
            "/ws/src/cf/CommonModules/Клиент/Ext/Module.bsl",
            "Процедура Вызвать() Экспорт\n    Продажи.Расчёт();\n    Склад.Расчёт();\nКонецПроцедуры\n",
        ),
    ]);

    let result = by_text(&db, files[2], None, None, "Расчёт();");

    assert_eq!(result.outcome, ReferencesOutcome::Ambiguous, "{result:?}");
    let mut named: Vec<String> =
        result.anchor_sites.iter().filter_map(|site| site.symbol.clone()).collect();
    named.sort();
    assert_eq!(
        named,
        ["Продажи.Расчёт", "Склад.Расчёт"],
        "an address that resolves to a third method is worse than no address: {:?}",
        result.anchor_sites,
    );
}

/// Gate — the candidate a text anchor counts is a SYMBOL, so a name repeated past the cap
/// inside one method neither raises the cap flag nor hides a second symbol.
#[test]
fn the_anchor_cap_counts_symbols_and_not_the_tokens_that_name_them() {
    let mut source = String::from("Процедура Первая(Значение) Экспорт\n");
    for _ in 0..65 {
        source.push_str("    Значение = 1;\n");
    }
    source.push_str(
        "КонецПроцедуры\n\nПроцедура Вторая(Значение) Экспорт\n    Значение = 2;\nКонецПроцедуры\n",
    );
    let (db, files) =
        db_with(&[("/ws/src/cf/CommonModules/Первый/Ext/Module.bsl", source.as_str())]);

    let result = by_text(&db, files[0], None, None, "Значение = ");

    assert_eq!(
        result.outcome,
        ReferencesOutcome::Ambiguous,
        "a second symbol past the sixty-fourth TOKEN is still a second symbol: {result:?}",
    );
    assert_eq!(result.anchor_sites.len(), 2);
    assert!(
        !result.anchor_candidates_capped,
        "two candidates is not a capped list, however many tokens name them",
    );
}

/// Gate — an empty quote certifies nothing, and must not report that it was found on every
/// line of the file. `contains("")` is true everywhere, so the count would be the file's
/// length dressed up as evidence.
#[test]
fn an_empty_quote_is_found_nowhere_rather_than_everywhere() {
    let (db, files) = db_with(&[("/ws/src/cf/CommonModules/Продажи/Ext/Module.bsl", SALES)]);

    let result = by_text(&db, files[0], None, None, "   ");

    assert_eq!(
        result.outcome,
        ReferencesOutcome::AnchorStale {
            reason: ide::AnchorStaleReason::NotInFile,
            line_matches: 0,
        },
        "an empty quote matched every line: {result:?}",
    );
}

/// A structure built with a literal key, and its field read on two lines. The plainest BSL
/// there is that reaches a member through a TYPED receiver.
const TYPED_MEMBER_TWICE: &str = "\
Процедура Тест() Экспорт
    С = Новый Структура(\"Поле\", 1);
    Х = С.Поле;
    У = С.Поле;
КонецПроцедуры
";

/// A platform method called on two lines — a member whose key already identifies the SYMBOL.
const DEFINITION_MEMBER_TWICE: &str = "\
Процедура Тест() Экспорт
    М = Новый Массив;
    М.Добавить(1);
    М.Добавить(2);
КонецПроцедуры
";

/// Gate — two mentions of ONE member of a typed receiver are one symbol.
///
/// `SemanticSymbolKey::TypedMember` names the member by its receiver and its folded field
/// name, so both mentions share one key by construction. A key that varied per occurrence
/// would answer `ambiguous` with two clones of one place — against the promise that one
/// symbol on several lines is one answer, and against a hint (`narrow line_content`) that
/// cannot help, since any narrowing still holds the member.
#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn two_mentions_of_one_typed_member_are_not_an_ambiguity() {
    let (db, files) =
        db_with(&[("/ws/src/cf/CommonModules/Первый/Ext/Module.bsl", TYPED_MEMBER_TWICE)]);

    let unpinned = by_text(&db, files[0], None, None, "Поле");
    // The control: the same file with the line pinned leaves ONE candidate, so it never
    // reaches the dedup. Whatever it answers, the unpinned call has to answer the same.
    let pinned = by_text(&db, files[0], Some(2), None, "Поле");

    assert_eq!(
        unpinned.outcome, pinned.outcome,
        "two mentions of one member became a choice between clones: {:?}",
        unpinned.anchor_sites,
    );
    assert!(unpinned.anchor_sites.is_empty(), "{:?}", unpinned.anchor_sites);

    // The second control: the same shape over a member whose key already IS a symbol
    // identity. It collapsed before this fix and must keep collapsing after it, so the
    // change is about the member key and not about dedup at large.
    let (db, files) =
        db_with(&[("/ws/src/cf/CommonModules/Первый/Ext/Module.bsl", DEFINITION_MEMBER_TWICE)]);
    let calls = by_text(&db, files[0], None, None, "Добавить");
    assert_eq!(
        calls.outcome,
        ReferencesOutcome::UnsupportedSymbol { category: ide::UnsupportedCategory::UnknownScope },
    );
    assert!(calls.anchor_sites.is_empty());
}

/// Two same-named fields of two DIFFERENT locals are two members — the second structure is
/// no control for the first. Their anchor faces two symbols, and only the caller's own line
/// can choose between them: the answer is ambiguity with both sites, never one collapsed
/// "cannot walk" that hides the choice.
#[test]
fn two_members_of_different_receivers_are_two_anchors() {
    const TWO_STRUCTURES: &str = "\
Процедура Тест() Экспорт
    С = Новый Структура(\"Поле\", 1);
    Д = Новый Структура(\"Поле\", 2);
    Х = С.Поле;
    У = С.Поле;
    Т = Д.Поле;
КонецПроцедуры
";
    let (db, files) =
        db_with(&[("/ws/src/cf/CommonModules/Первый/Ext/Module.bsl", TWO_STRUCTURES)]);

    let result = by_text(&db, files[0], None, None, "Поле");

    assert_eq!(result.outcome, ReferencesOutcome::Ambiguous);
    assert_eq!(result.anchor_sites.len(), 2, "{:?}", result.anchor_sites);

    // The narrowing hint still chooses: naming the occurrence by position leaves one member,
    // and walking it stays gated by its unknown scope.
    let aimed = find_references_by_name(
        &db,
        &request(ReferenceAnchor::Position { file_id: files[0], line: 3, column: 10 }),
        &[],
    );
    assert_eq!(
        aimed.outcome,
        ReferencesOutcome::UnsupportedSymbol { category: ide::UnsupportedCategory::UnknownScope },
    );
}

/// Gate — the shared line reader excludes the whole terminator, and a CRLF terminator is
/// `\r\n`. Its doc-comment promises exactly that, and the first caller that trusts the
/// promise without trimming would publish the `\r`.
#[test]
fn the_line_reader_excludes_a_crlf_terminator_whole() {
    let (db, files) =
        db_with(&[("/ws/src/cf/CommonModules/Первый/Ext/Module.bsl", "А = 1;\r\nБ = 2;\r\n")]);

    assert_eq!(ide::line_text(&db, files[0], 0).as_deref(), Some("А = 1;"));
    // The control: the same reader over LF, where the two spellings must agree.
    let (db, files) =
        db_with(&[("/ws/src/cf/CommonModules/Первый/Ext/Module.bsl", "А = 1;\nБ = 2;\n")]);
    assert_eq!(ide::line_text(&db, files[0], 0).as_deref(), Some("А = 1;"));
}

/// The file the client read, and the same file after a method with an identical body was
/// inserted above. The quoted line now stands twice, and the coordinate the client kept
/// belongs to the NEW method.
const DUPLICATED_QUOTE_BEFORE: &str = "\
Процедура Первая(Значение) Экспорт
    Значение = 1;
КонецПроцедуры
";

const DUPLICATED_QUOTE_AFTER: &str = "\
Процедура Нулевая(Значение) Экспорт
    Значение = 1;
    Значение = Значение + 1;
КонецПроцедуры

Процедура Первая(Значение) Экспорт
    Значение = 1;
КонецПроцедуры
";

/// Gate — a coordinate is evidence only where it decides nothing.
///
/// Pinning to a `line` that still carries the quote looks safe and is not: when the text
/// repeats, "your line still carries it" is a coincidence. Without this the stale request
/// answers `resolved` about the NEW method's local, standing at exactly the line it asked
/// for — so no relocation is announced and the answer is shaped like a correct fresh one.
#[test]
fn a_line_that_still_carries_a_repeated_quote_does_not_pin_the_anchor() {
    let (before, files) =
        db_with(&[("/ws/src/cf/CommonModules/Первый/Ext/Module.bsl", DUPLICATED_QUOTE_BEFORE)]);
    let read = by_text(&before, files[0], Some(1), None, "Значение = 1;");
    assert_eq!(read.outcome, ReferencesOutcome::Resolved, "the client's own picture");
    assert_eq!(read.hits.len(), 2);

    let (after, files) =
        db_with(&[("/ws/src/cf/CommonModules/Первый/Ext/Module.bsl", DUPLICATED_QUOTE_AFTER)]);
    let stale = by_text(&after, files[0], Some(1), None, "Значение = 1;");

    assert_eq!(
        stale.outcome,
        ReferencesOutcome::Ambiguous,
        "a repeated quote plus a coordinate the caller distrusts is a choice, not an answer: \
         {stale:?}",
    );
    assert!(stale.hits.is_empty(), "an ambiguity counts nothing");
    let pointed: Vec<&ide::AnchorSite> =
        stale.anchor_sites.iter().filter(|site| site.pointed_by_line).collect();
    assert_eq!(
        pointed.len(),
        1,
        "the caller has to see which place its own line meant: {:?}",
        stale.anchor_sites,
    );

    // The control: the same file, the same quote, and the line the occurrence actually moved
    // to. One coordinate is not the axis here — the quote still names two symbols — so the
    // control is instead the UNIQUE quote, where a coordinate decides nothing and the answer
    // is the direct one.
    let unique = by_text(&after, files[0], Some(1), None, "Значение = Значение + 1;");
    assert_eq!(unique.outcome, ReferencesOutcome::Resolved, "{unique:?}");
    assert_eq!(unique.anchor_line, Some(2), "a unique quote relocates and says so");
}

/// Two modules declaring one method name, and a caller invoking both on ONE line.
const TWO_CALLS_ONE_LINE: &str = "\
Процедура Вызвать() Экспорт
    Продажи.Расчёт(); Склад.Расчёт();
КонецПроцедуры
";

/// Gate — a quote is a place on the line, not a bag of names.
///
/// The quote names one of two calls that share a method name. Matching by name alone would
/// pick up the other call's token — outside the quoted text entirely — and answer `ambiguous`
/// about a quote that named exactly one occurrence.
#[test]
fn a_partial_quote_does_not_reach_a_namesake_standing_outside_it() {
    let (db, files) = db_with(&[
        ("/ws/src/cf/CommonModules/Продажи/Ext/Module.bsl", SALES),
        ("/ws/src/cf/CommonModules/Склад/Ext/Module.bsl", SALES),
        ("/ws/src/cf/CommonModules/Клиент/Ext/Module.bsl", TWO_CALLS_ONE_LINE),
    ]);

    let one = by_text(&db, files[2], None, None, "Продажи.Расчёт()");
    assert_eq!(
        one.outcome,
        ReferencesOutcome::Resolved,
        "the quote stands over one call and names one symbol: {:?}",
        one.anchor_sites,
    );
    assert!(one.hits.iter().any(|hit| hit.file_id == files[0]), "and it is the quoted one");
    assert!(
        !one.hits.iter().any(|hit| hit.file_id == files[1]),
        "the neighbour's method is not in the answer: {:?}",
        one.hits,
    );

    // The control: quoting the WHOLE line does stand over both calls, and two symbols under
    // one quote are a genuine choice.
    let both = by_text(&db, files[2], None, None, "Продажи.Расчёт(); Склад.Расчёт();");
    assert_eq!(both.outcome, ReferencesOutcome::Ambiguous, "{both:?}");
    assert_eq!(both.anchor_sites.len(), 2);
}

/// One platform function called twice on one line, in two spellings.
const BUILTIN_TWO_CASES: &str = "\
Процедура Тест() Экспорт
    Сообщить(1); СООБЩИТЬ(2);
КонецПроцедуры
";

/// Gate — BSL does not distinguish spellings, so one function written twice is one symbol.
///
/// A definition keyed by a name carries the spelling of its occurrence, so derived equality
/// sees two entities where the language has one. The control is the same quote aimed by a
/// column: it reaches one token, cannot go through the dedup, and its answer is what the
/// unaimed one has to give too.
#[test]
fn one_platform_function_in_two_spellings_is_one_symbol() {
    let (db, files) =
        db_with(&[("/ws/src/cf/CommonModules/Первый/Ext/Module.bsl", BUILTIN_TWO_CASES)]);

    let aimed = by_text(&db, files[0], Some(1), Some(4), "Сообщить(1); СООБЩИТЬ(2);");
    let whole = by_text(&db, files[0], Some(1), None, "Сообщить(1); СООБЩИТЬ(2);");

    assert_eq!(
        whole.outcome, aimed.outcome,
        "two spellings of one function became a choice between clones: {:?}",
        whole.anchor_sites,
    );
    assert!(whole.anchor_sites.is_empty(), "{:?}", whole.anchor_sites);
}
