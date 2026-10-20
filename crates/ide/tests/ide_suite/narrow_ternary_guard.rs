//! A guard written in a ternary condition must narrow the arm it guards.
//!
//! The CFG does not branch inside an expression, so both arms of `?(Усл, А, Б)` live in the vertex
//! that holds the condition and the dataflow state cannot tell them apart. These tests pin the
//! lexical refinement that covers the gap, next to the `Если` form it must agree with.

use hir::{HirDatabase, InferenceDiagnostic, TypeId};
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

fn mismatches(db: &RootDatabaseImpl, file_id: FileId) -> Vec<(TypeId, TypeId)> {
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

const MODULE: &str = r#"
//- /CommonModules/ПервыйОбщийМодуль/Ext/Module.bsl
// Возвращаемое значение:
//   - Число - индекс.
//   - Неопределено - не найден.
Функция Найти() Экспорт
    Возврат Неопределено;
КонецФункции

// Параметры:
//   Индекс - Число - индекс
Функция Удалить(Индекс) Экспорт
    Возврат Индекс;
КонецФункции
"#;

fn body(statements: &str) -> String {
    format!(
        "{MODULE}\n\
//- /test.bsl\n\
Процедура Тест()\n\
    Индекс = ПервыйОбщийМодуль.Найти();\n\
{statements}\
КонецПроцедуры\n"
    )
}

#[test]
fn ternary_false_arm_is_narrowed_by_is_undefined_condition() {
    let (db, file_id) = setup(&body(
        "    Значение = ?(Индекс = Неопределено, 0, ПервыйОбщийМодуль.Удалить(Индекс));\n",
    ));
    let got = mismatches(&db, file_id);
    assert!(got.is_empty(), "the false arm excludes Неопределено: {got:?}");
}

#[test]
fn ternary_true_arm_is_narrowed_by_is_not_undefined_condition() {
    let (db, file_id) = setup(&body(
        "    Значение = ?(Индекс <> Неопределено, ПервыйОбщийМодуль.Удалить(Индекс), 0);\n",
    ));
    let got = mismatches(&db, file_id);
    assert!(got.is_empty(), "the true arm excludes Неопределено: {got:?}");
}

#[test]
fn ternary_arm_on_the_guarded_side_still_reports() {
    // The arm reached when the value IS Неопределено must keep reporting: refining both arms
    // alike would make the check pass no matter which side the call sits on.
    let (db, file_id) = setup(&body(
        "    Значение = ?(Индекс = Неопределено, ПервыйОбщийМодуль.Удалить(Индекс), 0);\n",
    ));
    assert_eq!(mismatches(&db, file_id).len(), 1, "the guarded-away arm must still report");
}

#[test]
fn value_outside_any_ternary_still_reports() {
    let (db, file_id) = setup(&body("    ПервыйОбщийМодуль.Удалить(Индекс);\n"));
    assert_eq!(mismatches(&db, file_id).len(), 1, "an unguarded use must still report");
}
