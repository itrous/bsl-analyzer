use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use std::{path::Path, sync::Arc};
use vfs::{FileId, FileSet, VfsPath};

pub const SOURCE: &str = "Процедура Тест()\n    Закрыть();\n    Заголовок = Заголовок;\n    НеизвестнаяФормаФункция();\nКонецПроцедуры\n";

pub fn database() -> (RootDatabaseImpl, FileId, tempfile::TempDir) {
    let root = tempfile::tempdir().unwrap();
    let form = root.path().join("DataProcessors/Обработка1/Forms/Форма");
    let module = form.join("Ext/Form/Module.bsl");
    std::fs::create_dir_all(module.parent().unwrap()).unwrap();
    std::fs::write(&module, SOURCE).unwrap();
    std::fs::write(
        root.path().join("Configuration.xml"),
        r#"<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10"><Configuration><Properties><Name>Конфигурация1</Name></Properties><ChildObjects><DataProcessor>Обработка1</DataProcessor></ChildObjects></Configuration></MetaDataObject>"#,
    ).unwrap();
    std::fs::write(
        root.path().join("DataProcessors/Обработка1.xml"),
        r#"<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10"><DataProcessor><Properties><Name>Обработка1</Name></Properties><ChildObjects><Form>Форма</Form></ChildObjects></DataProcessor></MetaDataObject>"#,
    ).unwrap();
    std::fs::write(
        root.path().join("DataProcessors/Обработка1/Forms/Форма.xml"),
        r#"<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10"><Form><Properties><Name>Форма</Name><FormType>Managed</FormType></Properties></Form></MetaDataObject>"#,
    ).unwrap();
    std::fs::write(
        form.join("Ext/Form.xml"),
        r#"<Form xmlns="http://v8.1c.ru/8.3/xcf/logform" version="2.10"/>"#,
    )
    .unwrap();

    let mut db = RootDatabaseImpl::new();
    let file_id = FileId(0);
    let mut files = FileSet::default();
    files.insert(file_id, VfsPath::new(module.to_string_lossy().as_ref()));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(files));
    db.set_file_source_root(file_id, SourceRootId(0));
    db.set_file_text(file_id, SOURCE);
    db.set_all_config_paths(vec![(None, root.path().to_path_buf())], None);
    db.set_target_platform_version(Some(Arc::<str>::from("8.3.27")));
    (db, file_id, root)
}

pub fn unresolved(db: &RootDatabaseImpl, file_id: FileId) -> (Vec<String>, Vec<String>) {
    use hir::{HirDatabase, InferenceDiagnostic};
    let inference = db.infer(file_id);
    let calls = inference
        .diagnostics
        .iter()
        .filter_map(|(_, diag)| match diag {
            InferenceDiagnostic::UnresolvedBareCall { name, .. } => Some(name.as_str().to_owned()),
            _ => None,
        })
        .collect();
    let names = inference
        .diagnostics
        .iter()
        .filter_map(|(_, diag)| match diag {
            InferenceDiagnostic::UnresolvedName { name, .. } => Some(name.as_str().to_owned()),
            _ => None,
        })
        .collect();
    (calls, names)
}

pub fn assert_managed_form(root: &Path, db: &RootDatabaseImpl, file_id: FileId) {
    use hir::{DefDatabase, ModuleId};
    let metadata = db.module_metadata(ModuleId::new(file_id));
    assert_eq!(metadata.module_type, bsl_metadata::ModuleType::FormModule);
    assert!(metadata.form.as_deref().is_some_and(|form| form.is_managed()), "{}", root.display());
}
