//! Shared setup for the per-process platform help tests: each test binary
//! installs its own help selection, since the snapshot is process-wide.

use ide::Analysis;
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use std::sync::Arc;
use vfs::{FileId, FileSet, VfsPath};

pub const MODULE: &str = "\
Процедура Тест()
    Перем Локальная;
    Локальная = 1;
    Если Локальная = 1 Тогда
    КонецЕсли;
    Список = Новый Массив;
    Список.Добавить(1);
    Длина = СтрДлина(\"x\");
    СтрДлинаНеСуществует(\"x\");
    Сообщить(\"x\");
    
КонецПроцедуры
";

/// A server common module of a one-module configuration; the fixture directory
/// lives as long as the returned guard.
pub fn database(target: Option<&str>) -> (RootDatabaseImpl, FileId, test_fixture::CfeFixture) {
    let fixture = test_fixture::CfeFixtureBuilder::new("").build();
    let body_dir = fixture.root().join("CommonModules").join("Тест").join("Ext");
    std::fs::create_dir_all(&body_dir).unwrap();
    std::fs::write(
        fixture.root().join("CommonModules").join("Тест.xml"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <CommonModule uuid="00000000-0000-0000-0000-000000000002">
        <Properties><Name>Тест</Name><Server>true</Server><Global>false</Global></Properties>
    </CommonModule>
</MetaDataObject>"#,
    )
    .unwrap();
    let path = body_dir.join("Module.bsl");
    std::fs::write(&path, MODULE).unwrap();

    let mut db = RootDatabaseImpl::new();
    let file_id = FileId(0);
    let mut file_set = FileSet::default();
    file_set.insert(file_id, VfsPath::new(path.to_string_lossy().as_ref()));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    db.set_file_source_root(file_id, SourceRootId(0));
    db.set_file_text(file_id, MODULE);
    db.set_all_config_paths(fixture.config_paths());
    db.set_target_platform_version(target.map(Arc::<str>::from));
    (db, file_id, fixture)
}

pub fn offset_of(needle: &str) -> u32 {
    MODULE.find(needle).unwrap_or_else(|| panic!("{needle} not in module")) as u32
}

pub fn hover(analysis: &Analysis, file_id: FileId, needle: &str) -> Option<String> {
    analysis.hover(file_id, offset_of(needle) + 1, ide::Locale::Ru).map(|h| h.markup)
}

/// Names the inference reports as unresolved bare calls.
pub fn unresolved_bare_calls(db: &RootDatabaseImpl, file_id: FileId) -> Vec<String> {
    use hir::HirDatabase;
    db.infer(file_id)
        .diagnostics
        .iter()
        .filter_map(|(_, d)| match d {
            hir::InferenceDiagnostic::UnresolvedBareCall { name, .. } => {
                Some(name.as_str().to_owned())
            }
            _ => None,
        })
        .collect()
}
