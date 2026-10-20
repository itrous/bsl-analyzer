use hir::{HirDatabase, InferenceDiagnostic, UnresolvedMethodKind};
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use test_fixture::Fixture;
use vfs::FileId;

const FIXTURE: &str = r#"
//- /CommonModules/ОбщегоНазначения/Ext/Module.bsl
Функция ЗначениеРеквизитаОбъекта(Объект, ИмяРеквизита) Экспорт
    Возврат Неопределено;
КонецФункции

//- /test.bsl
Процедура Тест()
    Результат = ОбщегоНазначения.ЗначениеРеквизитаОбъекта();
КонецПроцедуры
"#;

fn setup_fixture(fixture_text: &str) -> (RootDatabaseImpl, FileId) {
    let fixture = Fixture::parse(fixture_text);
    let mut db = RootDatabaseImpl::new();

    let mut file_set = vfs::FileSet::default();
    for (file_id, file) in &fixture.files {
        file_set.insert(*file_id, file.path.clone());
        db.set_file_text(*file_id, &file.content);
    }
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    for file_id in fixture.files.keys() {
        db.set_file_source_root(*file_id, SourceRootId(0));
    }

    let test_file = fixture
        .files
        .iter()
        .find(|(_, f)| f.path.as_path().to_string_lossy().ends_with("/test.bsl"))
        .map(|(id, _)| *id)
        .expect("fixture must contain /test.bsl");
    (db, test_file)
}

fn unresolved_kinds(db: &RootDatabaseImpl, file_id: FileId) -> Vec<UnresolvedMethodKind> {
    db.infer(file_id)
        .diagnostics
        .iter()
        .filter_map(|(_, d)| match d {
            InferenceDiagnostic::UnresolvedMethodCall { kind, .. } => Some(*kind),
            _ => None,
        })
        .collect()
}

fn mismatched_arg_counts(db: &RootDatabaseImpl, file_id: FileId) -> Vec<(usize, usize, usize)> {
    db.arg_diagnostics(file_id)
        .iter()
        .filter_map(|(_, d)| match d {
            InferenceDiagnostic::MismatchedArgCount {
                required_count, total_count, found, ..
            } => Some((*required_count, *total_count, *found)),
            _ => None,
        })
        .collect()
}

#[test]
fn infer_invalidates_when_config_set_changes() {
    let (mut db, file_id) = setup_fixture(FIXTURE);

    assert_eq!(
        mismatched_arg_counts(&db, file_id),
        vec![(2, 2, 0)],
        "baseline: without configs, resolution must succeed and emit \
         MismatchedArgCount — if this fails, the regression is outside the \
         invalidation path (likely module_index or symbol_tree)"
    );
    assert!(
        unresolved_kinds(&db, file_id).is_empty(),
        "baseline: no UnresolvedMethodCall expected, got {:?}",
        unresolved_kinds(&db, file_id)
    );

    db.set_all_config_paths(vec![(None, std::path::PathBuf::from("/does-not-exist"))], None);

    let kinds = unresolved_kinds(&db, file_id);
    assert_eq!(
        kinds,
        vec![UnresolvedMethodKind::ReceiverNameAbsent],
        "after registering bogus config: the visibility gate must flip \
         resolution to an absent receiver root; if this fails, `db.infer` \
         is not invalidated by `set_all_config_paths` — the metadata bridge \
         is not wired through Salsa. ReceiverNameAbsent preserves the legacy \
         ReceiverNotResolved diagnostic while UnresolvedName is default-off, \
         and lets the IDE deduplicate it when the root diagnostic is enabled."
    );
    assert!(
        mismatched_arg_counts(&db, file_id).is_empty(),
        "after registering bogus config: resolution failed before arity \
         check, MismatchedArgCount must NOT be emitted, got {:?}",
        mismatched_arg_counts(&db, file_id)
    );

    db.set_all_config_paths(vec![], None);

    assert_eq!(
        mismatched_arg_counts(&db, file_id),
        vec![(2, 2, 0)],
        "after resetting configs to empty: baseline resolution must be \
         restored — proves invalidation fires on input removal too"
    );
    assert!(
        unresolved_kinds(&db, file_id).is_empty(),
        "after resetting configs: no UnresolvedMethodCall expected, got {:?}",
        unresolved_kinds(&db, file_id)
    );
}
