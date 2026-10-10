//! A parameter the platform documents as any metadata object accepts a
//! description of any kind.
//!
//! `Метаданные.НайтиПоТипу` and `НайтиПоПолномуИмени` return a description of
//! some kind, typed as the union of every `ОбъектМетаданных: <Вид>`. The help
//! extract cut the "any metadata object" parameter of `КоллекцияОбъектовМетаданных.Содержит`,
//! `ПравоДоступа`, `ЗаписьЖурналаРегистрации` and others to five kinds, so the
//! union failed the argument check although the call is the usual way to ask
//! which collection an object belongs to.

use hir::{HirDatabase, InferenceDiagnostic, TypeId, TypeKernelDb, TypeKind};
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use std::path::PathBuf;
use test_fixture::Fixture;
use vfs::FileId;

fn designer_fixture_path() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-metadata/fixtures/designer"))
}

fn setup(fixture_text: &str) -> (RootDatabaseImpl, FileId) {
    let fixture = Fixture::parse(fixture_text);
    let mut db = RootDatabaseImpl::new();
    let mut file_set = vfs::FileSet::default();
    for (file_id, file) in &fixture.files {
        file_set.insert(*file_id, file.path.clone());
    }
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    for (file_id, file) in &fixture.files {
        db.set_file_source_root(*file_id, SourceRootId(0));
        db.set_file_text(*file_id, &file.content);
    }
    db.set_all_config_paths(vec![(None, designer_fixture_path())]);
    let test_file = fixture
        .files
        .iter()
        .find(|(_, f)| f.path.as_path().to_string_lossy().ends_with("/test.bsl"))
        .map(|(id, _)| *id)
        .expect("fixture must contain /test.bsl");
    (db, test_file)
}

fn local_ty(db: &RootDatabaseImpl, file_id: FileId, name_lower: &str) -> TypeId {
    db.infer(file_id)
        .implicit_locals_by_body
        .values()
        .find_map(|locals| locals.get(name_lower))
        .and_then(|info| info.assignments.last())
        .map(|assignment| assignment.ty)
        .unwrap_or_else(|| panic!("`{name_lower}` must be an inferred local"))
}

fn description_kind(db: &RootDatabaseImpl, ty: TypeId) -> Option<String> {
    match db.lookup_type(ty) {
        TypeKind::PlatformObject(facet) => facet
            .name
            .split_once(':')
            .filter(|(family, _)| family.trim() == "ОбъектМетаданных")
            .map(|(_, kind)| kind.trim().to_owned()),
        _ => None,
    }
}

/// The description kinds of a union, and whether it may be `Неопределено`.
fn description_union_kinds(db: &RootDatabaseImpl, ty: TypeId) -> (Vec<String>, bool) {
    let TypeKind::Union(parts) = db.lookup_type(ty) else {
        panic!("expected a union of descriptions, got {:?}", db.lookup_type(ty));
    };
    let mut maybe_undefined = false;
    let kinds = parts
        .iter()
        .filter_map(|part| {
            if matches!(db.lookup_type(*part), TypeKind::Undefined) {
                maybe_undefined = true;
                return None;
            }
            Some(description_kind(db, *part).unwrap_or_else(|| {
                panic!("every member must be a description, got {:?}", db.lookup_type(*part))
            }))
        })
        .collect();
    (kinds, maybe_undefined)
}

/// The expected type of every reported mismatch, rendered by kind.
fn mismatch_messages(db: &RootDatabaseImpl, file_id: FileId) -> Vec<String> {
    db.infer(file_id)
        .diagnostics
        .iter()
        .chain(db.arg_diagnostics(file_id).iter())
        .filter_map(|(_, d)| match d {
            InferenceDiagnostic::TypeMismatch { expected, .. } => Some(render(db, *expected)),
            _ => None,
        })
        .collect()
}

fn render(db: &RootDatabaseImpl, ty: TypeId) -> String {
    match db.lookup_type(ty) {
        TypeKind::Union(parts) => {
            parts.iter().map(|part| render(db, *part)).collect::<Vec<_>>().join(" | ")
        }
        TypeKind::PlatformObject(facet) => facet.name.to_string(),
        other => format!("{other:?}"),
    }
}

/// A description of any kind: the kinds outside the five the extract kept are
/// exactly the ones the old check rejected.
fn assert_any_kind(db: &RootDatabaseImpl, file_id: FileId, local: &str) -> bool {
    let (kinds, maybe_undefined) = description_union_kinds(db, local_ty(db, file_id, local));
    for kind in ["Справочник", "Документ", "РегистрСведений", "HTTPСервис"]
    {
        assert!(kinds.iter().any(|k| k == kind), "`{local}` must admit {kind}, got {kinds:?}");
    }
    maybe_undefined
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn collection_contains_a_description_found_by_full_name() {
    let fixture = r#"
//- /test.bsl
Функция КейсСодержит(ПолноеИмя) Экспорт
    МетаОбъект = Метаданные.НайтиПоПолномуИмени(ПолноеИмя);
    Возврат Метаданные.Справочники.Содержит(МетаОбъект);
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    assert!(!assert_any_kind(&db, file_id, "метаобъект"));
    assert_eq!(mismatch_messages(&db, file_id), Vec::<String>::new());
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn collection_index_of_a_description_found_by_full_name() {
    let fixture = r#"
//- /test.bsl
Процедура Тест(ПолноеИмя)
    МетаОбъект = Метаданные.НайтиПоПолномуИмени(ПолноеИмя);
    Индекс = Метаданные.Документы.Индекс(МетаОбъект);
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    assert!(!assert_any_kind(&db, file_id, "метаобъект"));
    assert_eq!(mismatch_messages(&db, file_id), Vec::<String>::new());
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn global_functions_take_a_description_of_any_kind() {
    let fixture = r#"
//- /test.bsl
Процедура Тест(ПолноеИмя)
    МетаОбъект = Метаданные.НайтиПоПолномуИмени(ПолноеИмя);
    Если ПравоДоступа("Чтение", МетаОбъект) Тогда
        ВыполнитьПроверкуПравДоступа("Чтение", МетаОбъект);
    КонецЕсли;
    ЗаписьЖурналаРегистрации("Событие", УровеньЖурналаРегистрации.Ошибка, МетаОбъект);
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    assert!(!assert_any_kind(&db, file_id, "метаобъект"));
    assert_eq!(mismatch_messages(&db, file_id), Vec::<String>::new());
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn a_single_kind_outside_the_cut_five_is_accepted() {
    let fixture = r#"
//- /test.bsl
Процедура Тест()
    МетаОбъект = Справочники.Справочник1.ПустаяСсылка().Метаданные();
    ЗаписьЖурналаРегистрации("Событие", УровеньЖурналаРегистрации.Ошибка, МетаОбъект);
    Содержит = Метаданные.Справочники.Содержит(МетаОбъект);
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    let ty = local_ty(&db, file_id, "метаобъект");
    assert_eq!(
        description_kind(&db, ty).as_deref(),
        Some("Справочник"),
        "`Ссылка.Метаданные()` must be the catalog description, got {:?}",
        db.lookup_type(ty)
    );
    assert_eq!(mismatch_messages(&db, file_id), Vec::<String>::new());
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn a_value_that_is_no_metadata_object_is_still_rejected() {
    // The platform answers each of these with "Несоответствие типов (параметр номер ...)".
    let fixture = r#"
//- /test.bsl
Процедура Тест()
    ПоЧислу = Метаданные.Справочники.Содержит(1);
    ПоИмени = Метаданные.Справочники.Содержит("Справочник.Справочник1");
    Право = ПравоДоступа("Чтение", 1);
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    let messages = mismatch_messages(&db, file_id);
    assert_eq!(messages.len(), 3, "{messages:?}");
    for message in &messages {
        let expected: Vec<&str> = message.split(" | ").collect();
        assert!(
            expected.len() > 60
                && expected.iter().all(|kind| kind.starts_with("ОбъектМетаданных: ")),
            "the whole family is expected, got {expected:?}"
        );
    }
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn a_documented_narrowing_still_rejects_other_kinds() {
    // The user/role parameter of `ПравоДоступа` admits a role description only; a
    // description of any kind may be something else.
    let fixture = r#"
//- /test.bsl
Процедура Тест(ПолноеИмя)
    МетаОбъект = Метаданные.НайтиПоПолномуИмени(ПолноеИмя);
    Право = ПравоДоступа("Чтение", МетаОбъект, МетаОбъект);
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    assert!(!assert_any_kind(&db, file_id, "метаобъект"));
    let messages = mismatch_messages(&db, file_id);
    assert_eq!(messages.len(), 1, "{messages:?}");
    assert!(
        messages[0].contains("Роль"),
        "the third argument must be the one reported: {messages:?}"
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn a_description_that_may_be_absent_is_reported_for_the_absence_only() {
    // `НайтиПоТипу` answers `Неопределено` for a type no metadata object stands
    // behind, and the platform rejects `Содержит(Неопределено)` with
    // "Несоответствие типов (параметр номер '1')". The report stays; what it
    // expects is now the whole family, so the absence is the only reason left.
    let fixture = r#"
//- /test.bsl
Функция КейсСодержит(ТипСсылки) Экспорт
    МетаОбъект = Метаданные.НайтиПоТипу(ТипСсылки);
    Возврат Метаданные.Справочники.Содержит(МетаОбъект);
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    assert!(assert_any_kind(&db, file_id, "метаобъект"), "`НайтиПоТипу` may answer Неопределено");
    let messages = mismatch_messages(&db, file_id);
    assert_eq!(messages.len(), 1, "{messages:?}");
    let expected: Vec<&str> = messages[0].split(" | ").collect();
    assert!(expected.len() > 60, "the whole family is expected, got {expected:?}");
    assert!(!expected.contains(&"Undefined"), "{expected:?}");
}
