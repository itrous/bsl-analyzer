//! `Макс` / `Мин` return the type of their first argument.
//!
//! The HBK signature declares the return as the whole comparable family
//! (`Число, Строка, Дата, Булево`), because one function serves all four. Taken
//! literally, every `Макс(1, Х)` would carry `Строка` too, and the next
//! number-only consumer (`Формат(..., "ЧГ=0")`) would report a mismatch on
//! perfectly valid code. The platform picks the variant by the first argument,
//! so that argument's type is the result's type.

use hir::{HirDatabase, InferenceDiagnostic, TypeId, TypeKernelDb, TypeKind};
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use std::path::PathBuf;
use test_fixture::Fixture;
use vfs::{FileId, FileSet, VfsPath};

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

/// Type of the value last assigned to the implicit local `name_lower`. Read from the
/// implicit locals rather than `var_types`, which leaves out locals of unknown type.
fn local_ty(db: &RootDatabaseImpl, file_id: FileId, name_lower: &str) -> TypeId {
    db.infer(file_id)
        .implicit_locals_by_body
        .values()
        .find_map(|locals| locals.get(name_lower))
        .and_then(|info| info.assignments.last())
        .map(|assignment| assignment.ty)
        .unwrap_or_else(|| panic!("`{name_lower}` must be an inferred local"))
}

fn is_comparable(kind: &TypeKind) -> bool {
    matches!(
        kind,
        TypeKind::Number(_) | TypeKind::String(_) | TypeKind::Date(_) | TypeKind::Boolean
    )
}

fn mismatch_count(db: &RootDatabaseImpl, file_id: FileId) -> usize {
    db.infer(file_id)
        .diagnostics
        .iter()
        .chain(db.arg_diagnostics(file_id).iter())
        .filter(|(_, d)| matches!(d, InferenceDiagnostic::TypeMismatch { .. }))
        .count()
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn max_of_numbers_is_a_number_for_format() {
    let fixture = r#"
//- /test.bsl
Функция ПериодВМесяцах(Начало, Конец)
    Возврат Месяц(Конец) - Месяц(Начало);
КонецФункции

Процедура МаксМин(Начало, Конец)
    Месяцев = Макс(1, ПериодВМесяцах(Начало, Конец));
    Сообщить(Формат(Месяцев, "ЧГ=0"));
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    let ty = local_ty(&db, file_id, "месяцев");
    assert!(
        matches!(db.lookup_type(ty), TypeKind::Number(_)),
        "Макс(1, <Число>) must be a Число, got {:?}",
        db.lookup_type(ty)
    );
    assert_eq!(mismatch_count(&db, file_id), 0, "a Число is a valid Формат argument");
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn min_of_dates_is_a_date_for_format() {
    let fixture = r#"
//- /test.bsl
Процедура Тест()
    Начало = Мин(ТекущаяДата(), '20200101');
    Текст = Формат(Начало, "ДФ=dd.MM.yyyy");
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    let ty = local_ty(&db, file_id, "начало");
    assert!(
        matches!(db.lookup_type(ty), TypeKind::Date(_)),
        "Мин(<Дата>, <Дата>) must be a Дата, got {:?}",
        db.lookup_type(ty)
    );
    assert_eq!(mismatch_count(&db, file_id), 0, "a Дата is a valid Формат argument");
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn english_max_follows_first_argument_too() {
    let fixture = r#"
//- /test.bsl
Procedure Test()
    Count = Max(1, 2);
    Text = Format(Count, "NG=0");
EndProcedure
"#;
    let (db, file_id) = setup(fixture);
    let ty = local_ty(&db, file_id, "count");
    assert!(
        matches!(db.lookup_type(ty), TypeKind::Number(_)),
        "Max(1, 2) must be a Number, got {:?}",
        db.lookup_type(ty)
    );
    assert_eq!(mismatch_count(&db, file_id), 0, "a Number is a valid Format argument");
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn max_of_strings_is_still_rejected_by_format() {
    let fixture = r#"
//- /test.bsl
Процедура Тест()
    Имя = Макс("а", "б");
    Текст = Формат(Имя, "ЧГ=0");
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    let ty = local_ty(&db, file_id, "имя");
    assert!(
        matches!(db.lookup_type(ty), TypeKind::String(_)),
        "Макс(<Строка>, <Строка>) must be a Строка, got {:?}",
        db.lookup_type(ty)
    );
    assert_eq!(mismatch_count(&db, file_id), 1, "Формат does not accept a Строка");
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn max_of_a_comparable_union_keeps_that_union() {
    let fixture = r#"
//- /test.bsl
// Возвращаемое значение:
//   Число, Строка - значение
Функция Значение()
    Возврат 1;
КонецФункции

Процедура Тест()
    Результат = Макс(Значение(), 1);
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    let ty = local_ty(&db, file_id, "результат");
    let TypeKind::Union(arms) = db.lookup_type(ty) else {
        panic!("Макс(<Число | Строка>, 1) must be that union, got {:?}", db.lookup_type(ty));
    };
    let kinds: Vec<&TypeKind> = arms.iter().map(|arm| db.lookup_type(*arm)).collect();
    assert!(
        kinds.len() == 2
            && kinds.iter().any(|k| matches!(k, TypeKind::Number(_)))
            && kinds.iter().any(|k| matches!(k, TypeKind::String(_))),
        "the result is the first argument's Число | Строка, got {kinds:?}"
    );
}

#[test]
fn max_of_an_unknown_first_argument_is_unknown() {
    let fixture = r#"
//- /test.bsl
Процедура Тест(Количество)
    Месяцев = Макс(Количество, 1);
    Текст = Формат(Месяцев, "ЧГ=0");
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    let ty = local_ty(&db, file_id, "месяцев");
    assert!(
        matches!(db.lookup_type(ty), TypeKind::Unknown),
        "Макс(<unknown>, 1) must stay unknown, got {:?}",
        db.lookup_type(ty)
    );
    assert_eq!(mismatch_count(&db, file_id), 0, "an unknown value proves no Строка");
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn max_of_a_foreign_first_argument_is_not_narrowed_to_it() {
    let fixture = r#"
//- /test.bsl
Процедура Тест()
    Ссылка = Справочники.Справочник1.ПустаяСсылка();
    Результат = Макс(Ссылка, 1);
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    let first = local_ty(&db, file_id, "ссылка");
    assert!(
        matches!(db.lookup_type(first), TypeKind::MetadataRef(_)),
        "the first argument must be a catalog reference, got {:?}",
        db.lookup_type(first)
    );
    let ty = local_ty(&db, file_id, "результат");
    let within_family = match db.lookup_type(ty) {
        TypeKind::Unknown => true,
        TypeKind::Union(arms) => arms.iter().all(|arm| is_comparable(db.lookup_type(*arm))),
        kind => is_comparable(kind),
    };
    assert!(
        within_family,
        "a reference selects no comparison variant, so the result must not adopt it, got {:?}",
        db.lookup_type(ty)
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn a_module_method_named_max_is_not_narrowed() {
    let fixture = r#"
//- /test.bsl
Функция Макс(Значение)
    Возврат "текст";
КонецФункции

Процедура Тест()
    Результат = Макс(1);
    Текст = Формат(Результат, "ЧГ=0");
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    let ty = local_ty(&db, file_id, "результат");
    let admits_string = match db.lookup_type(ty) {
        TypeKind::Union(arms) => {
            arms.iter().any(|arm| matches!(db.lookup_type(*arm), TypeKind::String(_)))
        }
        kind => matches!(kind, TypeKind::String(_)),
    };
    assert!(
        admits_string,
        "the module's own `Макс` returns a Строка, so the platform rule must not apply, got {:?}",
        db.lookup_type(ty)
    );
    assert_eq!(mismatch_count(&db, file_id), 1, "Формат does not accept a Строка");
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn a_global_export_named_max_is_not_narrowed() {
    // The inline fixture format cannot carry the `<Global>` flag, so the global module
    // and the caller live at their paths in the on-disk `designer` configuration.
    let global_body = "Функция Макс(Значение) Экспорт\n    Возврат \"текст\";\nКонецФункции\n";
    let caller_body = "Процедура Тест() Экспорт\n    Результат = Макс(1);\n    \
                       Текст = Формат(Результат, \"ЧГ=0\");\nКонецПроцедуры\n";
    let caller_id = FileId::from_raw(1);
    let global_id = FileId::from_raw(2);
    let mut db = RootDatabaseImpl::new();
    let mut file_set = FileSet::default();
    for (id, rel) in [
        (caller_id, "CommonModules/ПервыйОбщийМодуль/Ext/Module.bsl"),
        (global_id, "CommonModules/ГлобальныйСерверныйМодуль/Ext/Module.bsl"),
    ] {
        let path = designer_fixture_path().join(rel).to_string_lossy().to_string();
        file_set.insert(id, VfsPath::new(path));
    }
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    for (id, text) in [(caller_id, caller_body), (global_id, global_body)] {
        db.set_file_source_root(id, SourceRootId(0));
        db.set_file_text(id, text);
    }
    db.set_all_config_paths(vec![(None, designer_fixture_path())]);

    let ty = local_ty(&db, caller_id, "результат");
    assert!(
        matches!(db.lookup_type(ty), TypeKind::String(_)),
        "the global export `Макс` returns a Строка, so the platform rule must not apply, got {:?}",
        db.lookup_type(ty)
    );
    assert_eq!(mismatch_count(&db, caller_id), 1, "Формат does not accept a Строка");
}

/// Methods and variables live apart: `Макс(...)` looks among methods only, so a variable
/// named `Макс` leaves the call to the platform function and the result is still the
/// first argument's type.
#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn a_module_variable_named_max_leaves_the_narrowing_in_place() {
    let fixture = r#"
//- /test.bsl
Перем Макс;

Функция ПериодВМесяцах(Начало, Конец)
    Возврат Месяц(Конец) - Месяц(Начало);
КонецФункции

Процедура МаксМин(Начало, Конец)
    Месяцев = Макс(1, ПериодВМесяцах(Начало, Конец));
    Сообщить(Формат(Месяцев, "ЧГ=0"));
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    let ty = local_ty(&db, file_id, "месяцев");
    assert!(
        matches!(db.lookup_type(ty), TypeKind::Number(_)),
        "a module variable does not own the call name, so the result is a Число, got {:?}",
        db.lookup_type(ty)
    );
    assert_eq!(mismatch_count(&db, file_id), 0, "a Число is a valid Формат argument");
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn a_local_variable_named_max_leaves_the_narrowing_in_place() {
    let fixture = r#"
//- /test.bsl
Процедура ЧерезПараметр(Макс)
    ЧерезПараметр = Макс(1, 2);
    Текст = Формат(ЧерезПараметр, "ЧГ=0");
КонецПроцедуры

Процедура ЧерезПерем()
    Перем Мин;
    ЧерезПерем = Мин(1, 2);
    Текст = Формат(ЧерезПерем, "ЧГ=0");
КонецПроцедуры

Процедура ЧерезЛокальную()
    Макс = "текст";
    ЧерезЛокальную = Макс(1, 2);
    Текст = Формат(ЧерезЛокальную, "ЧГ=0");
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    for local in ["черезпараметр", "черезперем", "черезлокальную"]
    {
        let ty = local_ty(&db, file_id, local);
        assert!(
            matches!(db.lookup_type(ty), TypeKind::Number(_)),
            "`{local}`: a variable does not own the call name, so the result is a Число, got {:?}",
            db.lookup_type(ty)
        );
    }
    assert_eq!(mismatch_count(&db, file_id), 0, "a Число is a valid Формат argument");
}
