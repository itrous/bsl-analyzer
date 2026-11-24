use hir::{DefDatabase, HirDatabase, InferenceDiagnostic, ModuleId, UnresolvedMethodKind};
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use test_fixture::Fixture;
use vfs::FileId;

pub(super) fn setup(fixture_text: &str) -> (RootDatabaseImpl, FileId) {
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
        .find(|(_, file)| file.path.as_path().ends_with("test.bsl"))
        .map(|(file_id, _)| *file_id)
        .expect("fixture must contain /test.bsl");
    let _ = db.module_bodies(ModuleId::new(test_file));
    (db, test_file)
}

pub(super) fn setup_with_designer_config(
    fixture_text: &str,
    target_path: &str,
) -> (RootDatabaseImpl, FileId) {
    let designer = std::path::PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../bsl-metadata/fixtures/designer"
    ));
    let fixture = Fixture::parse(fixture_text);
    let mut db = RootDatabaseImpl::new();
    let mut file_set = vfs::FileSet::default();
    let mapped = fixture
        .files
        .iter()
        .map(|(file_id, file)| {
            let relative = file.path.as_path().to_string_lossy();
            let path = designer.join(relative.trim_start_matches('/'));
            (*file_id, vfs::VfsPath::new(path.to_string_lossy().into_owned()))
        })
        .collect::<Vec<_>>();
    for (file_id, path) in &mapped {
        file_set.insert(*file_id, path.clone());
    }
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    for (file_id, file) in &fixture.files {
        db.set_file_source_root(*file_id, SourceRootId(0));
        db.set_file_text(*file_id, &file.content);
    }
    db.set_all_config_paths(vec![(None, designer)], None);
    let target_file = mapped
        .iter()
        .find(|(_, path)| path.as_path().ends_with(target_path.trim_start_matches('/')))
        .map(|(file_id, _)| *file_id)
        .unwrap_or_else(|| panic!("fixture must contain {target_path}"));
    let _ = db.module_bodies(ModuleId::new(target_file));
    (db, target_file)
}

pub(super) fn unresolved_kinds(
    db: &RootDatabaseImpl,
    file_id: FileId,
) -> Vec<UnresolvedMethodKind> {
    db.infer(file_id)
        .diagnostics
        .iter()
        .filter_map(|(_, diagnostic)| match diagnostic {
            InferenceDiagnostic::UnresolvedMethodCall { kind, .. } => Some(*kind),
            _ => None,
        })
        .collect()
}

/// Имена полей, по которым инференс не нашёл члена. Промах по СРЕДНЕМУ звену
/// трёхзвенной цепочки — это именно промах поля коллекции, а не метода.
pub(super) fn unresolved_fields(db: &RootDatabaseImpl, file_id: FileId) -> Vec<String> {
    db.infer(file_id)
        .diagnostics
        .iter()
        .filter_map(|(_, diagnostic)| match diagnostic {
            InferenceDiagnostic::UnresolvedField { field_name, .. } => {
                Some(field_name.as_str().to_string())
            }
            _ => None,
        })
        .collect()
}

/// Пары «plural, объект» из вердиктов об избыточном обращении к объекту.
pub(super) fn redundant_three_level(
    db: &RootDatabaseImpl,
    file_id: FileId,
) -> Vec<(String, String)> {
    db.infer(file_id)
        .diagnostics
        .iter()
        .filter_map(|(_, diagnostic)| match diagnostic {
            InferenceDiagnostic::RedundantAccessToObjectThreeLevel {
                mdo_type, mdo_name, ..
            } => Some((mdo_type.as_str().to_string(), mdo_name.as_str().to_string())),
            _ => None,
        })
        .collect()
}

/// Пропущенные обязательные параметры менеджерного вызова: `(метод, plural, объект)`.
pub(super) fn missed_manager_params(
    db: &RootDatabaseImpl,
    file_id: FileId,
) -> Vec<(String, String, String)> {
    db.infer(file_id)
        .diagnostics
        .iter()
        .filter_map(|(_, diagnostic)| match diagnostic {
            InferenceDiagnostic::MissedRequiredParameterManagerModule {
                callee,
                mdo_type,
                mdo_name,
                ..
            } => Some((
                callee.as_str().to_string(),
                mdo_type.as_str().to_string(),
                mdo_name.as_str().to_string(),
            )),
            _ => None,
        })
        .collect()
}

pub(super) fn mismatched_arg_counts(
    db: &RootDatabaseImpl,
    file_id: FileId,
) -> Vec<(usize, usize, usize)> {
    db.arg_diagnostics(file_id)
        .iter()
        .filter_map(|(_, diagnostic)| match diagnostic {
            InferenceDiagnostic::MismatchedArgCount {
                required_count, total_count, found, ..
            } => Some((*required_count, *total_count, *found)),
            _ => None,
        })
        .collect()
}

/// Имена получателей из вердиктов об избыточном двухзвенном обращении.
pub(super) fn redundant_two_level(db: &RootDatabaseImpl, file_id: FileId) -> Vec<String> {
    db.infer(file_id)
        .diagnostics
        .iter()
        .filter_map(|(_, diagnostic)| match diagnostic {
            InferenceDiagnostic::RedundantAccessToObjectTwoLevel { module, .. } => {
                Some(module.as_str().to_string())
            }
            _ => None,
        })
        .collect()
}
