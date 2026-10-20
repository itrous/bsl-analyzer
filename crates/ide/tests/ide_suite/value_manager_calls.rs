//! A call on a constant's value manager — `Константы.X.СоздатьМенеджерЗначения().Метод()`,
//! or the same value held in a variable — reaches the exports of the constant's
//! `ValueManagerModule.bsl` the way a call on a catalog object reaches its object module:
//! navigation, references, the call hierarchy and the unresolved-call verdict all follow it.

use hir::{Definition, MethodCallPair, MethodId, Semantics};
use ide::{Analysis, DiagnosticsConfig};
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::metadata::{MdoEntry, MetadataListingData};
use ide_db::RootDatabaseImpl;
use std::path::PathBuf;
use std::sync::Arc;
use syntax::TextSize;
use vfs::{FileId, FileSet, VfsPath};

fn designer_root() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-metadata/fixtures/designer"))
}

const STRING_CONSTANT_XML: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../bsl-metadata/fixtures/designer/Constants/СтрокаКонст.xml"
));

const NUMBER_CONSTANT_XML: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../bsl-metadata/fixtures/designer/Constants/ЧислоКонст.xml"
));

const CATALOG_XML: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../bsl-metadata/fixtures/designer/Catalogs/Справочник1.xml"
));

const VALUE_MANAGER_MODULE: &str = "\
Процедура Метод1(Параметр1) Экспорт
КонецПроцедуры

Процедура Внутренняя()
КонецПроцедуры
";

const VMM: usize = 0;
const CALLER: usize = 1;

/// Files: 0 value-manager module of `СтрокаКонст`, 1 caller, then the XML of
/// `СтрокаКонст`, `ЧислоКонст` (no value-manager module) and `Справочник1` (no object
/// module).
fn db(caller: &str) -> (RootDatabaseImpl, Vec<FileId>) {
    db_with(caller, None)
}

/// As [`db`], plus file 5 — an object module of `Справочник1` — when one is given.
fn db_with(caller: &str, catalog_object_module: Option<&str>) -> (RootDatabaseImpl, Vec<FileId>) {
    db_with_modules(caller, VALUE_MANAGER_MODULE, catalog_object_module)
}

/// As [`db_with`], with `value_manager_module` as the text of file 0.
fn db_with_modules(
    caller: &str,
    value_manager_module: &str,
    catalog_object_module: Option<&str>,
) -> (RootDatabaseImpl, Vec<FileId>) {
    let mut files = vec![
        ("Constants/СтрокаКонст/Ext/ValueManagerModule.bsl", value_manager_module),
        ("CommonModules/Вызывающий/Ext/Module.bsl", caller),
        ("Constants/СтрокаКонст.xml", STRING_CONSTANT_XML),
        ("Constants/ЧислоКонст.xml", NUMBER_CONSTANT_XML),
        ("Catalogs/Справочник1.xml", CATALOG_XML),
    ];
    if let Some(text) = catalog_object_module {
        files.push(("Catalogs/Справочник1/Ext/ObjectModule.bsl", text));
    }
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
    let entry = |kind, name: &str, main: usize| MdoEntry {
        kind,
        name: name.to_string(),
        main: ids[main],
        predefined: None,
    };
    db.set_metadata_listing(
        &root.to_string_lossy(),
        MetadataListingData {
            entries: vec![
                entry(bsl_metadata::MdoType::Constant, "СтрокаКонст", 2),
                entry(bsl_metadata::MdoType::Constant, "ЧислоКонст", 3),
                entry(bsl_metadata::MdoType::Catalog, "Справочник1", 4),
            ],
            ..MetadataListingData::default()
        },
    );
    (db, ids)
}

/// Offset of the `nth` occurrence of `needle`, shifted into the word by `into`.
fn at(text: &str, needle: &str, nth: usize, into: usize) -> u32 {
    let start = text.match_indices(needle).nth(nth).expect("needle present").0;
    (start + into) as u32
}

fn method_at(analysis: &Analysis, file_id: FileId, offset: u32) -> MethodId {
    match Semantics::new(analysis.database())
        .symbol_at(file_id, TextSize::from(offset))
        .and_then(|symbol| symbol.definition)
    {
        Some(Definition::Method(id)) => id,
        other => panic!("not a method at {offset}: {other:?}"),
    }
}

/// `(code, covered text)` of every `Unresolved*` finding in the caller.
fn unresolved(analysis: &Analysis, caller: &str, file_id: FileId) -> Vec<(String, String)> {
    analysis
        .diagnostics(file_id, &DiagnosticsConfig::default())
        .into_iter()
        .filter(|d| format!("{:?}", d.code).starts_with("Unresolved"))
        .map(|d| {
            let range = std::ops::Range::<usize>::from(d.range);
            (format!("{:?}", d.code), caller[range].to_string())
        })
        .collect()
}

fn vmm_export_start() -> usize {
    VALUE_MANAGER_MODULE.find("Метод1").unwrap()
}

const CHAIN_CALLER: &str = "\
Процедура Прогон(Параметр1) Экспорт
    Константы.СтрокаКонст.СоздатьМенеджерЗначения().Метод1(Параметр1);
КонецПроцедуры
";

const LOCAL_CALLER: &str = "\
Процедура Прогон(Параметр1) Экспорт
    МенеджерЗначения = Константы.СтрокаКонст.СоздатьМенеджерЗначения();
    МенеджерЗначения.Метод1(Параметр1);
КонецПроцедуры
";

#[test]
fn goto_definition_from_the_call_result_reaches_the_value_manager_export() {
    let (db, ids) = db(CHAIN_CALLER);
    let analysis = Analysis::from_database(db);
    let target = analysis
        .goto_definition(ids[CALLER], at(CHAIN_CALLER, "Метод1", 0, 2))
        .expect("the call names the value-manager export");
    assert_eq!(target.file_id, ids[VMM]);
    assert_eq!(target.name, "Метод1");
}

#[test]
fn goto_definition_through_a_local_variable_reaches_the_value_manager_export() {
    let (db, ids) = db(LOCAL_CALLER);
    let analysis = Analysis::from_database(db);
    let target = analysis
        .goto_definition(ids[CALLER], at(LOCAL_CALLER, "Метод1", 0, 2))
        .expect("the call names the value-manager export");
    assert_eq!(target.file_id, ids[VMM]);
    assert_eq!(target.name, "Метод1");
}

#[test]
fn references_from_the_declaration_include_both_call_spellings() {
    let caller = "\
Процедура Прогон(Параметр1) Экспорт
    Константы.СтрокаКонст.СоздатьМенеджерЗначения().Метод1(Параметр1);
    МенеджерЗначения = Константы.СтрокаКонст.СоздатьМенеджерЗначения();
    МенеджерЗначения.Метод1(Параметр1);
КонецПроцедуры
";
    let (db, ids) = db(caller);
    let analysis = Analysis::from_database(db);
    let start = vmm_export_start();
    let mut found: Vec<(FileId, String)> = analysis
        .find_references(ids[VMM], start as u32 + 2)
        .into_iter()
        .map(|loc| {
            let text = if loc.file_id == ids[VMM] { VALUE_MANAGER_MODULE } else { caller };
            (loc.file_id, text[std::ops::Range::<usize>::from(loc.range)].to_string())
        })
        .collect();
    found.sort();
    let name = "Метод1".to_string();
    assert_eq!(
        found,
        vec![(ids[VMM], name.clone()), (ids[CALLER], name.clone()), (ids[CALLER], name)],
    );
}

/// Control: the call hierarchy is built from the syntactic call graph, which lowers no
/// typed receiver — a catalog object's export called through `СоздатьЭлемент()` is not an
/// edge either. The value-manager call keeps exactly that standing; the local call is the
/// control that the harness counts an edge at all.
#[test]
fn call_hierarchy_treats_the_value_manager_call_like_a_catalog_object_call() {
    let caller = "Процедура Помощник()
КонецПроцедуры

Процедура Прогон(Параметр1) Экспорт
    Помощник();
    Объект = Справочники.Справочник1.СоздатьЭлемент();
    Объект.МетодОбъекта();
    МенеджерЗначения = Константы.СтрокаКонст.СоздатьМенеджерЗначения();
    МенеджерЗначения.Метод1(Параметр1);
КонецПроцедуры
";
    let object_module = "Процедура МетодОбъекта() Экспорт
КонецПроцедуры
";
    let (db, ids) = db_with(caller, Some(object_module));
    let analysis = Analysis::from_database(db);

    // Navigation does reach both typed-receiver targets...
    let object_target =
        analysis.goto_definition(ids[CALLER], at(caller, "МетодОбъекта", 0, 2)).map(|t| t.file_id);
    assert_eq!(object_target, Some(ids[5]));
    let vmm_target =
        analysis.goto_definition(ids[CALLER], at(caller, "Метод1", 0, 2)).map(|t| t.file_id);
    assert_eq!(vmm_target, Some(ids[VMM]));

    // ...while the call hierarchy lists only the syntactic edge, for both alike.
    let outgoing = analysis.call_hierarchy_outgoing(ids[CALLER], at(caller, "Прогон", 0, 2));
    let callees: Vec<(FileId, String)> =
        outgoing.iter().map(|c| (c.item.file_id, c.item.name.clone())).collect();
    assert_eq!(callees, vec![(ids[CALLER], "Помощник".to_string())]);

    let prog = method_at(&analysis, ids[CALLER], at(caller, "Прогон", 0, 2));
    for (file, text, name) in
        [(ids[5], object_module, "МетодОбъекта"), (ids[VMM], VALUE_MANAGER_MODULE, "Метод1")]
    {
        let offset = at(text, name, 0, 2);
        let target = method_at(&analysis, file, offset);
        let mut index = hir::CallHierarchyReverseIndex::new();
        index.replace_module(prog.module, [MethodCallPair::new(prog, target)], 0);
        let incoming = analysis.call_hierarchy_incoming_from_index(file, offset, Arc::new(index));
        assert!(incoming.is_none(), "{name}: {incoming:?}");
    }
}

#[test]
fn an_absent_method_on_the_value_manager_is_reported() {
    let caller = "\
Процедура Прогон() Экспорт
    Константы.СтрокаКонст.СоздатьМенеджерЗначения().НетТакогоМетода();
    МенеджерЗначения = Константы.СтрокаКонст.СоздатьМенеджерЗначения();
    МенеджерЗначения.ИТакогоНет();
КонецПроцедуры
";
    let (db, ids) = db(caller);
    let analysis = Analysis::from_database(db);
    assert_eq!(
        unresolved(&analysis, caller, ids[CALLER]),
        vec![
            (
                "UnresolvedMethodCall".to_string(),
                "Константы.СтрокаКонст.СоздатьМенеджерЗначения().НетТакогоМетода".to_string()
            ),
            ("UnresolvedMethodCall".to_string(), "МенеджерЗначения.ИТакогоНет".to_string()),
        ],
    );
    // The receiver is named by the value-manager type, not by the `Константы` manager.
    let messages: Vec<String> = analysis
        .diagnostics(ids[CALLER], &DiagnosticsConfig::default())
        .into_iter()
        .filter(|d| d.code == ide::DiagnosticCode::UnresolvedMethodCall)
        .map(|d| d.message)
        .collect();
    assert_eq!(messages.len(), 2);
    for message in &messages {
        assert!(message.contains("КонстантаМенеджерЗначения.СтрокаКонст"), "{message}");
    }
}

/// The compiler rejects a call of a non-exported procedure from another module; the
/// call is reported and navigation does not name the declaration.
#[test]
fn a_non_exported_value_manager_procedure_does_not_resolve_from_outside() {
    let caller = "\
Процедура Прогон() Экспорт
    Константы.СтрокаКонст.СоздатьМенеджерЗначения().Внутренняя();
КонецПроцедуры
";
    let (db, ids) = db(caller);
    let analysis = Analysis::from_database(db);
    assert!(analysis.goto_definition(ids[CALLER], at(caller, "Внутренняя", 0, 2)).is_none());
    assert_eq!(
        unresolved(&analysis, caller, ids[CALLER]),
        vec![(
            "UnresolvedMethodCall".to_string(),
            "Константы.СтрокаКонст.СоздатьМенеджерЗначения().Внутренняя".to_string()
        )],
    );
}

/// Control: the platform members of `КонстантаМенеджерЗначения` are neither shadowed by
/// the module nor reported.
#[test]
fn platform_members_of_the_value_manager_stay_platform_members() {
    let caller = "\
Процедура Прогон() Экспорт
    МенеджерЗначения = Константы.СтрокаКонст.СоздатьМенеджерЗначения();
    МенеджерЗначения.Прочитать();
    МенеджерЗначения.Значение = \"\";
    Если МенеджерЗначения.ПроверитьЗаполнение() Тогда
        МенеджерЗначения.Записать();
    КонецЕсли;
    МенеджерЗначения.ДополнительныеСвойства.Вставить(\"Ключ\", МенеджерЗначения.ОбменДанными);
КонецПроцедуры
";
    let (db, ids) = db(caller);
    let analysis = Analysis::from_database(db);
    assert_eq!(unresolved(&analysis, caller, ids[CALLER]), vec![]);
    for member in ["Прочитать", "Записать"] {
        let target = analysis.goto_definition(ids[CALLER], at(caller, member, 0, 2));
        assert!(
            target.as_ref().is_none_or(|t| t.file_id != ids[VMM]),
            "`{member}` must not reach the value-manager module: {target:?}"
        );
    }
}

/// Control: the manager spelling `Константы.X.Метод()` still does not reach the
/// value-manager module, and its verdict is the one it had before.
#[test]
fn the_manager_spelling_stays_unresolved() {
    let caller = "\
Процедура Прогон(Параметр1) Экспорт
    Константы.СтрокаКонст.Метод1(Параметр1);
КонецПроцедуры
";
    let (db, ids) = db(caller);
    let analysis = Analysis::from_database(db);
    assert!(analysis.goto_definition(ids[CALLER], at(caller, "Метод1", 0, 2)).is_none());
    assert_eq!(
        unresolved(&analysis, caller, ids[CALLER]),
        vec![("UnresolvedMethodCall".to_string(), "Константы.СтрокаКонст.Метод1".to_string())],
    );
}

/// A constant without a value-manager module answers a call the way a catalog object
/// without an object module does: the platform surface is the whole surface, and a
/// name outside it is reported. The catalog line is the control that this is the
/// codebase's verdict for a module-less object, not something new to constants.
#[test]
fn a_constant_without_a_value_manager_module_is_judged_like_a_catalog_without_an_object_module() {
    let caller = "\
Процедура Прогон() Экспорт
    Справочники.Справочник1.СоздатьЭлемент().НетВСправочнике();
    Константы.ЧислоКонст.СоздатьМенеджерЗначения().НетВКонстанте();
    Константы.ЧислоКонст.СоздатьМенеджерЗначения().Прочитать();
КонецПроцедуры
";
    let (db, ids) = db(caller);
    let analysis = Analysis::from_database(db);
    assert_eq!(
        unresolved(&analysis, caller, ids[CALLER]),
        vec![
            (
                "UnresolvedMethodCall".to_string(),
                "Справочники.Справочник1.СоздатьЭлемент().НетВСправочнике".to_string()
            ),
            (
                "UnresolvedMethodCall".to_string(),
                "Константы.ЧислоКонст.СоздатьМенеджерЗначения().НетВКонстанте".to_string()
            ),
        ],
    );
}

/// An export variable of the value-manager module is a field of the value manager, the
/// way an object-module export variable is a field of a catalog object. The catalog
/// lines are the control that the harness resolves such a field at all.
#[test]
fn an_export_variable_of_the_value_manager_module_is_a_field() {
    let caller = "\
Процедура Прогон() Экспорт
    Объект = Справочники.Справочник1.СоздатьЭлемент();
    Х = Объект.СчетчикОбъекта;
    МенеджерЗначения = Константы.СтрокаКонст.СоздатьМенеджерЗначения();
    Х = МенеджерЗначения.Счетчик;
    МенеджерЗначения.Счетчик = 1;
    Х = Константы.СтрокаКонст.СоздатьМенеджерЗначения().Счетчик;
    Х = МенеджерЗначения.НетТакогоПоля;
КонецПроцедуры
";
    let value_manager_module = "\
Перем Счетчик Экспорт;
Перем Внутренний;
";
    let object_module = "Перем СчетчикОбъекта Экспорт;\n";
    let (db, ids) = db_with_modules(caller, value_manager_module, Some(object_module));
    let analysis = Analysis::from_database(db);
    assert_eq!(
        unresolved(&analysis, caller, ids[CALLER]),
        vec![("UnresolvedField".to_string(), "МенеджерЗначения.НетТакогоПоля".to_string())],
    );
}

/// The type of a value manager is the constant, so goto type definition lands on the
/// constant's XML. The catalog object is the control that the harness navigates a
/// metadata type at all.
#[test]
fn type_definition_of_a_value_manager_is_the_constant() {
    let caller = "\
Процедура Прогон() Экспорт
    Объект = Справочники.Справочник1.СоздатьЭлемент();
    Объект.Записать();
    МенеджерЗначения = Константы.СтрокаКонст.СоздатьМенеджерЗначения();
    МенеджерЗначения.Метод1(1);
КонецПроцедуры
";
    let (db, ids) = db(caller);
    let analysis = Analysis::from_database(db);
    let target_of = |needle: &str| {
        analysis
            .type_definition(ids[CALLER], at(caller, needle, 0, 2))
            .unwrap_or_else(|| panic!("no type target for `{needle}`"))
    };
    assert_eq!(target_of("Объект.Записать").file_id, ids[4], "the catalog control");
    let target = target_of("МенеджерЗначения.Метод1");
    assert_eq!(target.file_id, ids[2]);
    assert_eq!(target.name, "СтрокаКонст");
}
