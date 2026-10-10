//! Reference walk over a method called through a TYPED receiver — `Об.Метод()`,
//! `Набор.Метод()`, `ЭтотОбъект.Метод()` — as opposed to a syntactically
//! qualified name like `Справочники.Товары.Метод()`.
//!
//! Every input here has ONE config root and no extensions, and none of them sets
//! a multi-root `WorkspaceConfigsSnapshot`: whatever these gates catch, config
//! visibility can neither cause nor excuse it.
//!
//! Each gate names the input on which it must fail. A walk that answered an empty
//! list for everything would pass a suite of "no false hits" assertions, so the
//! required hit is asserted, not just the forbidden one — and the manager route,
//! which reaches the same method through a qualified name, is carried in the same
//! input as the control that the harness can count a cross-file call at all.

use hir::{HirDatabase, InferenceDiagnostic};
use ide::{
    find_references_by_name, ReferenceAnchor, ReferenceArea, ReferenceKind, ReferencesOutcome,
    ReferencesRequest,
};
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::metadata::{MdoEntry, MetadataListingData};
use ide_db::RootDatabaseImpl;
use std::path::PathBuf;
use vfs::{FileId, FileSet, VfsPath};

const MAX_FILES: usize = 2000;

fn designer_root() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-metadata/fixtures/designer"))
}

const CATALOG_XML: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../bsl-metadata/fixtures/designer/Catalogs/Справочник1.xml"
));

const INFORMATION_REGISTER_XML: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../bsl-metadata/fixtures/designer/InformationRegisters/РегистрСведений1.xml"
));

/// A db over the `designer` root wired the way the resident host does it:
/// config paths plus a metadata listing, so `Справочники.Справочник1` denotes a
/// metadata object and the receiver of the call has a type at all.
fn db_over_designer(
    files: &[(&str, &str)],
    entries: &[(bsl_metadata::MdoType, &str, usize)],
) -> (RootDatabaseImpl, Vec<FileId>) {
    let root = designer_root();
    let mut db = RootDatabaseImpl::new();
    let ids: Vec<FileId> = (0..files.len()).map(|i| FileId(i as u32)).collect();

    let mut file_set = FileSet::default();
    for (id, (rel, _)) in ids.iter().zip(files) {
        file_set.insert(*id, VfsPath::from(root.join(rel)));
    }
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    for (id, (_, text)) in ids.iter().zip(files) {
        db.set_file_source_root(*id, SourceRootId(0));
        db.set_file_text(*id, text);
    }

    db.set_all_config_paths(vec![(None, root.clone())]);
    db.set_metadata_listing(
        &root.to_string_lossy(),
        MetadataListingData {
            entries: entries
                .iter()
                .map(|(kind, name, idx)| MdoEntry {
                    kind: *kind,
                    name: (*name).to_string(),
                    main: ids[*idx],
                    predefined: None,
                })
                .collect(),
            ..MetadataListingData::default()
        },
    );

    (db, ids)
}

fn by_name(db: &RootDatabaseImpl, name: &str) -> ide::ReferencesResult {
    find_references_by_name(
        db,
        &ReferencesRequest {
            anchor: ReferenceAnchor::Name(name.to_string()),
            anchor_files: None,
            area: ReferenceArea::default(),
            kinds: None,
            include_declaration: true,
            max_files: MAX_FILES,
        },
        &[],
    )
}

fn hit_files(result: &ide::ReferencesResult) -> Vec<FileId> {
    result.hits.iter().map(|hit| hit.file_id).collect()
}

const CATALOG_ENTRY: (bsl_metadata::MdoType, &str, usize) =
    (bsl_metadata::MdoType::Catalog, "Справочник1", 1);

/// Gate I0 (object module) — a call reached through a receiver whose type was
/// INFERRED is a reference to the method it calls.
///
/// The control lives in the same input: the manager module is reached by a
/// qualified name instead, and its cross-file call must be counted. Without it a
/// walk that answers nothing for every route would satisfy the object assertion
/// by accident.
#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn an_object_module_method_called_through_a_typed_receiver_is_counted() {
    let caller = "\
Процедура Прогон() Экспорт
    Объект = Справочники.Справочник1.СоздатьЭлемент();
    Объект.ПодготовитьОбъект();
    Справочники.Справочник1.ПодготовитьМенеджер();
КонецПроцедуры
";
    let (db, ids) = db_over_designer(
        &[
            (
                "Catalogs/Справочник1/Ext/ObjectModule.bsl",
                "Процедура ПодготовитьОбъект() Экспорт\nКонецПроцедуры\n",
            ),
            ("Catalogs/Справочник1.xml", CATALOG_XML),
            ("CommonModules/Вызывающий/Ext/Module.bsl", caller),
            (
                "Catalogs/Справочник1/Ext/ManagerModule.bsl",
                "Процедура ПодготовитьМенеджер() Экспорт\nКонецПроцедуры\n",
            ),
        ],
        &[CATALOG_ENTRY],
    );
    let (object_module, caller_file, manager_module) = (ids[0], ids[2], ids[3]);

    let control = by_name(&db, "Справочник.Справочник1.ПодготовитьМенеджер");
    assert_eq!(control.outcome, ReferencesOutcome::Resolved, "{:?}", control.outcome);
    let control_files = hit_files(&control);
    assert!(
        control_files.contains(&manager_module) && control_files.contains(&caller_file),
        "control: the harness counts a cross-file call on the manager route: {control_files:?}",
    );

    let result = by_name(&db, "Справочник.Справочник1.ПодготовитьОбъект");
    assert_eq!(result.outcome, ReferencesOutcome::Resolved, "{:?}", result.outcome);
    let files = hit_files(&result);
    assert!(files.contains(&object_module), "the declaration itself: {files:?}");
    assert!(
        files.contains(&caller_file),
        "the call through the inferred receiver must be counted: {files:?}",
    );
}

/// Gate I0 (record-set module) — the same rule on the fourth class of names with
/// a reference walk. The control is the object route in the gate above: both are
/// reached through an inferred receiver, so a walk fixed for one and not the
/// other is visible.
#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn a_record_set_module_method_called_through_a_typed_receiver_is_counted() {
    let caller = "\
Процедура Прогон() Экспорт
    Набор = РегистрыСведений.РегистрСведений1.СоздатьНаборЗаписей();
    Набор.ПодготовитьНабор();
КонецПроцедуры
";
    let (db, ids) = db_over_designer(
        &[
            (
                "InformationRegisters/РегистрСведений1/Ext/RecordSetModule.bsl",
                "Процедура ПодготовитьНабор() Экспорт\nКонецПроцедуры\n",
            ),
            ("InformationRegisters/РегистрСведений1.xml", INFORMATION_REGISTER_XML),
            ("CommonModules/Вызывающий/Ext/Module.bsl", caller),
        ],
        &[(bsl_metadata::MdoType::InformationRegister, "РегистрСведений1", 1)],
    );
    let (record_set_module, caller_file) = (ids[0], ids[2]);

    let result = by_name(&db, "РегистрСведений.РегистрСведений1.ПодготовитьНабор");
    assert_eq!(result.outcome, ReferencesOutcome::Resolved, "{:?}", result.outcome);
    let files = hit_files(&result);
    assert!(files.contains(&record_set_module), "the declaration itself: {files:?}");
    assert!(
        files.contains(&caller_file),
        "the call through the inferred receiver must be counted: {files:?}",
    );
}

/// Gate I0b (object module) — the user method WINS over a platform method of the
/// same name.
///
/// `Записать` is a platform method of a catalog object, so this input is the one
/// the gates above cannot fail on: they use names no platform member claims, and
/// stay green whichever of the two surfaces is consulted first. Here the order is
/// the whole subject.
#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn an_object_method_shadowing_a_platform_name_is_still_the_one_called() {
    let caller = "\
Процедура Прогон() Экспорт
    Объект = Справочники.Справочник1.СоздатьЭлемент();
    Объект.Записать();
КонецПроцедуры
";
    let (db, ids) = db_over_designer(
        &[
            (
                "Catalogs/Справочник1/Ext/ObjectModule.bsl",
                "Процедура Записать() Экспорт\nКонецПроцедуры\n",
            ),
            ("Catalogs/Справочник1.xml", CATALOG_XML),
            ("CommonModules/Вызывающий/Ext/Module.bsl", caller),
        ],
        &[CATALOG_ENTRY],
    );
    let (object_module, caller_file) = (ids[0], ids[2]);

    let result = by_name(&db, "Справочник.Справочник1.Записать");
    assert_eq!(result.outcome, ReferencesOutcome::Resolved, "{:?}", result.outcome);
    let files = hit_files(&result);
    assert!(files.contains(&object_module), "the declaration itself: {files:?}");
    assert!(
        files.contains(&caller_file),
        "the user method shadows the platform one and the call is its reference: {files:?}",
    );
}

/// Gate I0b (manager module) — the same collision on the manager route.
///
/// The name must be one the manager surface actually claims: `Записать` is a
/// method of the OBJECT, not of the manager, so reusing it here would resolve
/// through the already-working qualified-name route and pass no matter what the
/// order is. `НайтиПоКоду` is a genuine manager collision.
#[test]
fn a_manager_method_shadowing_a_platform_name_is_still_the_one_called() {
    let caller = "\
Процедура Прогон() Экспорт
    Справочники.Справочник1.НайтиПоКоду(\"001\");
КонецПроцедуры
";
    let (db, ids) = db_over_designer(
        &[
            (
                "Catalogs/Справочник1/Ext/ManagerModule.bsl",
                "Процедура НайтиПоКоду(Код) Экспорт\nКонецПроцедуры\n",
            ),
            ("Catalogs/Справочник1.xml", CATALOG_XML),
            ("CommonModules/Вызывающий/Ext/Module.bsl", caller),
        ],
        &[CATALOG_ENTRY],
    );
    let (manager_module, caller_file) = (ids[0], ids[2]);

    let result = by_name(&db, "Справочник.Справочник1.НайтиПоКоду");
    assert_eq!(result.outcome, ReferencesOutcome::Resolved, "{:?}", result.outcome);
    let files = hit_files(&result);
    assert!(files.contains(&manager_module), "the declaration itself: {files:?}");
    assert!(
        files.contains(&caller_file),
        "the user method shadows the platform one and the call is its reference: {files:?}",
    );
}

/// Gate I0c — `ЭтотОбъект` is a receiver class of its own.
///
/// The gates above all reach the object through a variable, so none of them
/// exercises the coercion that turns `ЭтотОбъект` into the metadata reference the
/// user routes match on. The call and the declaration live in the SAME file here,
/// so the assertion is on the kind of the hit, not on the set of files: a walk
/// that returned the declaration alone would satisfy any file-level check.
#[test]
fn a_call_through_this_object_is_counted() {
    let module = "\
Процедура Записать() Экспорт
КонецПроцедуры

Процедура Прогон() Экспорт
    ЭтотОбъект.Записать();
КонецПроцедуры
";
    let (db, ids) = db_over_designer(
        &[
            ("Catalogs/Справочник1/Ext/ObjectModule.bsl", module),
            ("Catalogs/Справочник1.xml", CATALOG_XML),
        ],
        &[CATALOG_ENTRY],
    );
    let object_module = ids[0];

    let result = by_name(&db, "Справочник.Справочник1.Записать");
    assert_eq!(result.outcome, ReferencesOutcome::Resolved, "{:?}", result.outcome);
    assert!(
        result
            .hits
            .iter()
            .any(|hit| hit.file_id == object_module && hit.kind == ReferenceKind::Call),
        "the call through ЭтотОбъект must be counted, not just the declaration: {:?}",
        result.hits.iter().map(|hit| (hit.file_id, hit.kind)).collect::<Vec<_>>(),
    );
}

/// Gate P — inference and navigation name the SAME method on one input.
///
/// Both surfaces now ask `resolve_user_call` for the user half of the cascade, so
/// they cannot disagree about WHICH user method answers. What stays private to
/// each caller is the order relative to the platform surface, and that is what
/// this gate pins — on both sides at once, using a name the platform also claims.
///
/// The inference half is observable through arity: the user `Записать` requires
/// two arguments and the platform one requires none, so a call with no arguments
/// yields `MismatchedArgCount` exactly when inference chose the user method. The
/// navigation half is the reference walk reaching that call.
///
/// The mutant that must turn it red: consult the platform surface BEFORE
/// `resolve_user_call` in `resolve_method_call_to_definition`. The navigation half
/// goes red while the inference half stays green — which is precisely the drift a
/// gate on one surface alone would miss.
#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn inference_and_navigation_name_the_same_method() {
    let caller = "\
Процедура Прогон() Экспорт
    Объект = Справочники.Справочник1.СоздатьЭлемент();
    Объект.Записать();
КонецПроцедуры
";
    let (db, ids) = db_over_designer(
        &[
            (
                "Catalogs/Справочник1/Ext/ObjectModule.bsl",
                "Процедура Записать(Реквизит1, Реквизит2) Экспорт\nКонецПроцедуры\n",
            ),
            ("Catalogs/Справочник1.xml", CATALOG_XML),
            ("CommonModules/Вызывающий/Ext/Module.bsl", caller),
        ],
        &[CATALOG_ENTRY],
    );
    let (object_module, caller_file) = (ids[0], ids[2]);

    let arity: Vec<(usize, usize, usize)> = db
        .arg_diagnostics(caller_file)
        .iter()
        .filter_map(|(_, diagnostic)| match diagnostic {
            InferenceDiagnostic::MismatchedArgCount {
                required_count, total_count, found, ..
            } => Some((*required_count, *total_count, *found)),
            _ => None,
        })
        .collect();
    assert_eq!(
        arity,
        vec![(2, 2, 0)],
        "inference must bind the call to the USER Записать, whose arity the platform one \
         does not share — no diagnostic here means it chose the platform member",
    );

    let result = by_name(&db, "Справочник.Справочник1.Записать");
    assert_eq!(result.outcome, ReferencesOutcome::Resolved, "{:?}", result.outcome);
    let files = hit_files(&result);
    assert!(files.contains(&object_module), "the declaration itself: {files:?}");
    assert!(
        files.contains(&caller_file),
        "navigation must name the same method inference just bound: {files:?}",
    );
}

/// The same db, but with NO configured visibility at all — neither config paths nor
/// a metadata listing.
fn db_without_config(files: &[(&str, &str)]) -> (RootDatabaseImpl, Vec<FileId>) {
    let root = designer_root();
    let mut db = RootDatabaseImpl::new();
    let ids: Vec<FileId> = (0..files.len()).map(|i| FileId(i as u32)).collect();
    let mut file_set = FileSet::default();
    for (id, (rel, _)) in ids.iter().zip(files) {
        file_set.insert(*id, VfsPath::from(root.join(rel)));
    }
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    for (id, (_, text)) in ids.iter().zip(files) {
        db.set_file_source_root(*id, SourceRootId(0));
        db.set_file_text(*id, text);
    }
    (db, ids)
}

/// Gate I4 — with no configured visibility, resolution degrades to the path-derived
/// index and answers exactly as it did before visibility filtering existed.
///
/// The branch this pins is narrow and easy to lose: `visible_config_root_ranks`
/// returns `None` ONLY when the workspace snapshot holds no paths at all, so every
/// other gate in this file — each of which configures one root — exercises the
/// `Some` branch instead.
///
/// The existing config-less coverage reaches the MANAGER route only
/// (`infer_three_level::three_level_invalidates_on_config_change`); these two routes
/// had none.
#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn without_configured_visibility_the_walk_degrades_to_the_path_index() {
    let object_caller = "\
Процедура Прогон() Экспорт
    Объект = Справочники.Справочник1.СоздатьЭлемент();
    Объект.ПодготовитьОбъект();
КонецПроцедуры
";
    let (db, ids) = db_without_config(&[
        (
            "Catalogs/Справочник1/Ext/ObjectModule.bsl",
            "Процедура ПодготовитьОбъект() Экспорт\nКонецПроцедуры\n",
        ),
        ("Catalogs/Справочник1.xml", CATALOG_XML),
        ("CommonModules/Вызывающий/Ext/Module.bsl", object_caller),
    ]);
    let (object_module, caller_file) = (ids[0], ids[2]);

    let result = by_name(&db, "Справочник.Справочник1.ПодготовитьОбъект");
    assert_eq!(result.outcome, ReferencesOutcome::Resolved, "{:?}", result.outcome);
    let files = hit_files(&result);
    assert!(files.contains(&object_module), "the declaration itself: {files:?}");
    assert!(
        files.contains(&caller_file),
        "no configured visibility must not turn into 'no visible body': {files:?}",
    );

    let record_set_caller = "\
Процедура Прогон() Экспорт
    Набор = РегистрыСведений.РегистрСведений1.СоздатьНаборЗаписей();
    Набор.ПодготовитьНабор();
КонецПроцедуры
";
    let (db, ids) = db_without_config(&[
        (
            "InformationRegisters/РегистрСведений1/Ext/RecordSetModule.bsl",
            "Процедура ПодготовитьНабор() Экспорт\nКонецПроцедуры\n",
        ),
        ("InformationRegisters/РегистрСведений1.xml", INFORMATION_REGISTER_XML),
        ("CommonModules/Вызывающий/Ext/Module.bsl", record_set_caller),
    ]);
    let (record_set_module, caller_file) = (ids[0], ids[2]);

    let result = by_name(&db, "РегистрСведений.РегистрСведений1.ПодготовитьНабор");
    assert_eq!(result.outcome, ReferencesOutcome::Resolved, "{:?}", result.outcome);
    let files = hit_files(&result);
    assert!(files.contains(&record_set_module), "the declaration itself: {files:?}");
    assert!(
        files.contains(&caller_file),
        "no configured visibility must not turn into 'no visible body': {files:?}",
    );
}

/// Gate I4b — a candidate body that lies OUTSIDE every configured root is kept.
///
/// Root topology says nothing about such a file: it has no rank at all. The path
/// index hands it over today, so dropping it would be a second change of behaviour
/// riding along with the visibility fix. `effective_module_exports_query` does drop
/// it, and can afford to — it composes a surface, while this decides an answer.
///
/// This is the ONLY input in the suite where the two spellings of the filter differ.
/// With configured roots every candidate has a rank, and with no roots at all the
/// lookup returns before the filter runs — so `is_none_or` and `is_some_and` agree
/// everywhere else, and the decision would be defended by a comment alone.
#[test]
fn a_body_outside_every_configured_root_is_still_a_candidate() {
    let root = designer_root();
    let mut db = RootDatabaseImpl::new();
    let caller = FileId(0);
    let outside_manager = FileId(1);
    let xml = FileId(2);

    let mut file_set = FileSet::default();
    file_set.insert(caller, VfsPath::from(root.join("CommonModules/Вызывающий/Ext/Module.bsl")));
    // Deliberately not under the configured root.
    file_set.insert(
        outside_manager,
        VfsPath::new("/ws/вне-корней/Catalogs/Справочник1/Ext/ManagerModule.bsl"),
    );
    file_set.insert(xml, VfsPath::from(root.join("Catalogs/Справочник1.xml")));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));

    for (file_id, text) in [
        (
            caller,
            "Процедура Прогон() Экспорт\n    Справочники.Справочник1.ПодготовитьМенеджер();\nКонецПроцедуры\n",
        ),
        (outside_manager, "Процедура ПодготовитьМенеджер() Экспорт\nКонецПроцедуры\n"),
        (xml, CATALOG_XML),
    ] {
        db.set_file_source_root(file_id, SourceRootId(0));
        db.set_file_text(file_id, text);
    }

    db.set_all_config_paths(vec![(None, root.clone())]);
    db.set_metadata_listing(
        &root.to_string_lossy(),
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

    let result = by_name(&db, "Справочник.Справочник1.ПодготовитьМенеджер");
    assert_eq!(result.outcome, ReferencesOutcome::Resolved, "{:?}", result.outcome);
    let files = hit_files(&result);
    assert!(files.contains(&outside_manager), "the declaration itself: {files:?}");
    assert!(
        files.contains(&caller),
        "a rankless body is not an invisible one — the call still names it: {files:?}",
    );
}

/// Gate I0d — a user method the cascade FOUND but the export boundary bars does not
/// hand the call to the platform.
///
/// The barred case is not the same as "no user method": inference has already bound
/// this call to the user declaration and reports `MethodNotExport`, so answering a
/// platform member here would be the two surfaces disagreeing about what the call
/// names — the exact drift the shared entry exists to prevent.
///
/// Anchored by POSITION, not by qualified name: the name anchor cannot reach a
/// non-exported declaration at all, so it reports nothing for either behaviour and
/// tells the two apart from neither. The positional path does distinguish them —
/// a platform member answers `UnsupportedSymbol`.
///
/// The method name must be one the platform also claims, or the fallthrough has
/// nothing to answer with and the gate passes on a broken build.
#[test]
fn a_barred_user_method_does_not_hand_the_call_to_the_platform() {
    let caller = "\
Процедура Прогон() Экспорт
    Объект = Справочники.Справочник1.СоздатьЭлемент();
    Объект.Записать();
КонецПроцедуры
";
    let (db, ids) = db_over_designer(
        &[
            // No `Экспорт`: visible to its own module only.
            ("Catalogs/Справочник1/Ext/ObjectModule.bsl", "Процедура Записать()\nКонецПроцедуры\n"),
            ("Catalogs/Справочник1.xml", CATALOG_XML),
            ("CommonModules/Вызывающий/Ext/Module.bsl", caller),
        ],
        &[CATALOG_ENTRY],
    );
    let caller_file = ids[2];

    let result = find_references_by_name(
        &db,
        &ReferencesRequest {
            anchor: ReferenceAnchor::Position { file_id: caller_file, line: 2, column: 11 },
            anchor_files: None,
            area: ReferenceArea::default(),
            kinds: None,
            include_declaration: true,
            max_files: MAX_FILES,
        },
        &[],
    );
    assert!(
        !matches!(result.outcome, ReferencesOutcome::UnsupportedSymbol { .. }),
        "a call barred by the export boundary must not resolve to the platform member: {:?}",
        result.outcome,
    );
}

/// Gate I0e — inside its OWN module a non-exported method is a legitimate call, and
/// navigation names it.
///
/// The export boundary the gate above pins is a CROSS-module rule; applying it here
/// too would lose navigation for every private helper called through `ЭтотОбъект`,
/// while inference goes on binding those calls. The platform claims `Записать` for
/// this receiver, so a build that stopped at the boundary answers `UnsupportedSymbol`
/// or nothing — both visible here.
#[test]
fn a_private_method_called_within_its_own_module_is_still_named() {
    let module = "\
Процедура Записать()
КонецПроцедуры

Процедура Прогон() Экспорт
    ЭтотОбъект.Записать();
КонецПроцедуры
";
    let (db, ids) = db_over_designer(
        &[
            ("Catalogs/Справочник1/Ext/ObjectModule.bsl", module),
            ("Catalogs/Справочник1.xml", CATALOG_XML),
        ],
        &[CATALOG_ENTRY],
    );
    let object_module = ids[0];

    let result = find_references_by_name(
        &db,
        &ReferencesRequest {
            anchor: ReferenceAnchor::Position { file_id: object_module, line: 4, column: 15 },
            anchor_files: None,
            area: ReferenceArea::default(),
            kinds: None,
            include_declaration: true,
            max_files: MAX_FILES,
        },
        &[],
    );
    assert_eq!(
        result.outcome,
        ReferencesOutcome::Resolved,
        "a private method called from its own module is a user method, not a platform one: {:?}",
        result.outcome,
    );
    assert!(
        result.hits.iter().any(|hit| hit.kind == ReferenceKind::Declaration),
        "the walk reaches its declaration: {:?}",
        result.hits.iter().map(|hit| (hit.file_id, hit.kind)).collect::<Vec<_>>(),
    );
}
