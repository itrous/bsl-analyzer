use hir::{HirDatabase, InferenceDiagnostic};
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use test_fixture::Fixture;
use vfs::FileId;

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

    let test_file = fixture
        .files
        .iter()
        .find(|(_, f)| f.path.as_path().to_string_lossy().ends_with("/test.bsl"))
        .map(|(id, _)| *id)
        .expect("fixture must contain /test.bsl");
    (db, test_file)
}

#[test]
fn recovered_field_access_does_not_emit_unresolved_field() {
    let (db, file_id) = setup(
        r#"//- /test.bsl
Процедура Тест()
    Сп = Новый Массив;
    Сп.В
КонецПроцедуры
"#,
    );

    let infer = db.infer(file_id);
    let unresolved: Vec<_> = infer
        .diagnostics
        .iter()
        .filter_map(|(_, d)| match d {
            InferenceDiagnostic::UnresolvedField { field_name, .. } => {
                Some(field_name.as_str().to_string())
            }
            _ => None,
        })
        .collect();
    assert!(
        unresolved.is_empty(),
        "recovered Сп.В must not fire UnresolvedField; got: {:?}",
        unresolved,
    );
}

#[test]
fn well_formed_call_next_to_recovered_is_not_silenced() {
    let (db, file_id) = setup(
        r#"//- /test.bsl
Процедура Тест()
    Сп = Новый Массив;
    Сп.Добавить(1);
    Сп.В
КонецПроцедуры
"#,
    );

    let _infer = db.infer(file_id);
}
