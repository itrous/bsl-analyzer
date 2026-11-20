//! `XMLСтрока(<Значение>)` and the types it accepts.
//!
//! The extract keeps the first five entries of a parameter's type list, and the
//! help puts this parameter's admissible set in the prose of the description
//! rather than in its rubric, so the parameter arrived as
//! `Неопределено, Null, Булево, Число, Строка`. `Дата` — the type the help
//! documents first after the scalars, and the common argument here — was
//! reported as a type mismatch at Error severity. The curated overlay restores
//! the parameter to `Произвольный`; these checks read the generated data, so
//! they also prove the overlay reached the build.

use hir::{HirDatabase, InferenceDiagnostic, TypeId, TypeKernelDb, TypeKind};
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use std::path::PathBuf;
use test_fixture::Fixture;
use vfs::{FileId, FileSet};

fn designer_fixture_path() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-metadata/fixtures/designer"))
}

fn setup_loose(text: &str) -> (RootDatabaseImpl, FileId) {
    let fixture = Fixture::parse(&format!("//- /test.bsl\n{text}"));
    let mut db = RootDatabaseImpl::new();
    let mut file_set = FileSet::default();
    for (file_id, file) in &fixture.files {
        file_set.insert(*file_id, file.path.clone());
    }
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    for (file_id, file) in &fixture.files {
        db.set_file_source_root(*file_id, SourceRootId(0));
        db.set_file_text(*file_id, &file.content);
    }
    db.set_all_config_paths(vec![(None, designer_fixture_path())], None);
    let file_id = *fixture.files.keys().next().expect("one file");
    (db, file_id)
}

fn render(db: &RootDatabaseImpl, ty: TypeId) -> String {
    match db.lookup_type(ty) {
        TypeKind::Union(parts) => {
            parts.iter().map(|part| render(db, *part)).collect::<Vec<_>>().join(" | ")
        }
        TypeKind::PlatformObject(facet) => facet.name.to_string(),
        TypeKind::String(_) => "Строка".to_owned(),
        other => format!("{other:?}"),
    }
}

/// `(expected, actual)` of every reported mismatch, rendered.
fn mismatches(db: &RootDatabaseImpl, file_id: FileId) -> Vec<(String, String)> {
    db.infer(file_id)
        .diagnostics
        .iter()
        .chain(db.arg_diagnostics(file_id).iter())
        .filter_map(|(_, d)| match d {
            InferenceDiagnostic::TypeMismatch { expected, actual, .. } => {
                Some((render(db, *expected), render(db, *actual)))
            }
            _ => None,
        })
        .collect()
}

#[test]
fn xml_string_accepts_the_types_the_platform_converts() {
    // Every argument here returns a string on 8.3.17.1549 and 8.3.27.2214.
    let (db, file_id) = setup_loose(
        "Процедура Тест()
    Дата = XMLСтрока(ТекущаяДата());
    Идентификатор = XMLСтрока(Новый УникальныйИдентификатор);
    Данные = XMLСтрока(ПолучитьДвоичныеДанныеИзСтроки(\"x\"));
    Текст = XMLСтрока(\"abc\");
    Количество = XMLСтрока(42);
    Признак = XMLСтрока(Истина);
    Пусто = XMLСтрока(Неопределено);
КонецПроцедуры
",
    );
    assert_eq!(mismatches(&db, file_id), Vec::<(String, String)>::new());
}

#[test]
fn an_argument_with_a_real_constraint_is_still_a_mismatch() {
    // A positive control: an empty report is easy to get by accident, so a
    // parameter the platform does constrain must stay reported in the same
    // harness. `ПравоДоступа(<Право>, <ОбъектМетаданных>)` rejects a string:
    // «Несоответствие типов (параметр номер '2')».
    let (db, file_id) = setup_loose(
        "Процедура Тест()
    Право = ПравоДоступа(\"Вывод\", \"Метаданные\");
КонецПроцедуры
",
    );
    let found = mismatches(&db, file_id);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].1, "Строка", "{found:?}");
}
