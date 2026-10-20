use hir::{Builders, HirDatabase, InferenceDiagnostic, TypeId};
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
    db.set_all_config_paths(vec![(None, designer_fixture_path())], None);

    let test_file = fixture
        .files
        .iter()
        .find(|(_, f)| f.path.as_path().to_string_lossy().ends_with("/test.bsl"))
        .map(|(id, _)| *id)
        .expect("fixture must contain /test.bsl");
    (db, test_file)
}

fn arg_mismatches(db: &RootDatabaseImpl, file_id: FileId) -> Vec<(TypeId, TypeId)> {
    db.arg_diagnostics(file_id)
        .iter()
        .filter_map(|(_, d)| match d {
            InferenceDiagnostic::TypeMismatch { expected, actual, .. } => {
                Some((*expected, *actual))
            }
            _ => None,
        })
        .collect()
}

const NARROW_FIXTURE_MODULE: &str = r#"
//- /CommonModules/ПервыйОбщийМодуль/Ext/Module.bsl
// Возвращаемое значение:
//   Число, Неопределено - индекс или Неопределено если не найден
Функция Найти() Экспорт
    Возврат Неопределено;
КонецФункции

// Параметры:
//   Индекс - Число - индекс
Процедура Удалить(Индекс) Экспорт
КонецПроцедуры
"#;

#[test]
fn narrowed_arg_inside_guard_does_not_fire_mismatch() {
    let fixture = format!(
        "{NARROW_FIXTURE_MODULE}\n\
//- /test.bsl\n\
Процедура Тест()\n\
    Индекс = ПервыйОбщийМодуль.Найти();\n\
    Если Индекс <> Неопределено Тогда\n\
        ПервыйОбщийМодуль.Удалить(Индекс);\n\
    КонецЕсли;\n\
КонецПроцедуры\n"
    );
    let (db, file_id) = setup(&fixture);

    assert!(
        arg_mismatches(&db, file_id).is_empty(),
        "narrowed `Number | Undefined → Number` arg in guard true-branch \
         must not emit TypeMismatch — got {:?}",
        arg_mismatches(&db, file_id),
    );
}

#[test]
fn unnarrowed_arg_outside_guard_still_fires_mismatch() {
    let fixture = format!(
        "{NARROW_FIXTURE_MODULE}\n\
//- /test.bsl\n\
Процедура Тест()\n\
    Индекс = ПервыйОбщийМодуль.Найти();\n\
    ПервыйОбщийМодуль.Удалить(Индекс);\n\
КонецПроцедуры\n"
    );
    let (db, file_id) = setup(&fixture);

    let mm = arg_mismatches(&db, file_id);
    assert_eq!(
        mm.len(),
        1,
        "no-guard call with `Number | Undefined` arg must still emit one TypeMismatch — \
         got {mm:?}"
    );
    let (expected, actual) = &mm[0];
    assert_eq!(*expected, db.number(None, None), "expected param type Number");
    assert!(
        *actual != db.number(None, None),
        "actual must be the unnarrowed union, not Number — got {actual:?}"
    );
}

#[test]
fn arg_mismatch_in_else_branch_still_fires() {
    let fixture = format!(
        "{NARROW_FIXTURE_MODULE}\n\
//- /test.bsl\n\
Процедура Тест()\n\
    Индекс = ПервыйОбщийМодуль.Найти();\n\
    Если Индекс <> Неопределено Тогда\n\
    Иначе\n\
        ПервыйОбщийМодуль.Удалить(Индекс);\n\
    КонецЕсли;\n\
КонецПроцедуры\n"
    );
    let (db, file_id) = setup(&fixture);

    let mm = arg_mismatches(&db, file_id);
    assert_eq!(
        mm.len(),
        1,
        "else-branch `Undefined` arg must still emit one TypeMismatch — got {mm:?}"
    );
}

#[test]
fn arity_mismatch_still_fires_from_arg_diagnostics() {
    let fixture = r#"
//- /CommonModules/ПервыйОбщийМодуль/Ext/Module.bsl
// Параметры:
//   А - Число
//   Б - Число
Процедура Двойной(А, Б) Экспорт
КонецПроцедуры

//- /test.bsl
Процедура Тест()
    ПервыйОбщийМодуль.Двойной(1);
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);

    let arity = db
        .arg_diagnostics(file_id)
        .iter()
        .filter(|(_, d)| matches!(d, InferenceDiagnostic::MismatchedArgCount { .. }))
        .count();
    assert_eq!(arity, 1, "exactly one MismatchedArgCount must continue to fire");
}

#[test]
fn feature_flag_off_disables_narrowing_overlay() {
    let fixture = format!(
        "{NARROW_FIXTURE_MODULE}\n\
//- /test.bsl\n\
Процедура Тест()\n\
    Индекс = ПервыйОбщийМодуль.Найти();\n\
    Если Индекс <> Неопределено Тогда\n\
        ПервыйОбщийМодуль.Удалить(Индекс);\n\
    КонецЕсли;\n\
КонецПроцедуры\n"
    );
    let (mut db, file_id) = setup(&fixture);
    db.set_type_narrowing_enabled(false);

    let mm = arg_mismatches(&db, file_id);
    assert_eq!(
        mm.len(),
        1,
        "with narrowing disabled, the guard must not suppress the mismatch — got {mm:?}"
    );
}

#[test]
fn flag_flip_invalidates_arg_diagnostics_cache() {
    let fixture = format!(
        "{NARROW_FIXTURE_MODULE}\n\
//- /test.bsl\n\
Процедура Тест()\n\
    Индекс = ПервыйОбщийМодуль.Найти();\n\
    Если Индекс <> Неопределено Тогда\n\
        ПервыйОбщийМодуль.Удалить(Индекс);\n\
    КонецЕсли;\n\
КонецПроцедуры\n"
    );
    let (mut db, file_id) = setup(&fixture);

    let on = arg_mismatches(&db, file_id);
    assert!(on.is_empty(), "with narrowing on: expected zero mismatches, got {on:?}");

    db.set_type_narrowing_enabled(false);
    let off = arg_mismatches(&db, file_id);
    assert_eq!(
        off.len(),
        1,
        "after flag flip to false: expected one mismatch, got {off:?} — \
         indicates stale arg_diagnostics cache",
    );

    db.set_type_narrowing_enabled(true);
    let on_again = arg_mismatches(&db, file_id);
    assert!(
        on_again.is_empty(),
        "after flag flip back to true: expected zero mismatches, got {on_again:?}"
    );
}
