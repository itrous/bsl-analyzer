#[path = "support/partial_platform_help.rs"]
mod partial_platform_help;

use bsl_platform::{install_platform_help, InstallOutcome};
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::metadata::intern_configuration_path;
use ide_db::{RootDatabaseImpl, SalsaProvider};
use ide_diagnostics::{DiagnosticCode, DiagnosticsConfig, DiagnosticsContext};
use std::fs;
use vfs::{FileId, FileSet, VfsPath};

const SOURCE: &str =
    "Процедура Проверить()\n    Перем НастоящаяЛокаль;\n    НеизвестноеПоле = 1;\nКонецПроцедуры\n";

#[test]
fn partial_callback_facts_conservatively_suppress_implicit_context_assignments() {
    assert_eq!(
        install_platform_help(partial_platform_help::array_only_snapshot()),
        Ok(InstallOutcome::Installed)
    );

    let dir = tempfile::tempdir().expect("temporary catalog configuration");
    let root = dir.path();
    let module_dir = root.join("Catalogs/Товары/Ext");
    fs::create_dir_all(&module_dir).unwrap();
    fs::write(
        root.join("Configuration.xml"),
        r#"<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <Configuration uuid="00000000-0000-0000-0000-000000000001">
        <Properties><Name>ПроверкаПолей</Name></Properties>
    </Configuration>
</MetaDataObject>"#,
    )
    .unwrap();
    fs::write(
        root.join("Catalogs/Товары.xml"),
        r#"<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <Catalog uuid="00000000-0000-0000-0000-000000000002">
        <Properties><Name>Товары</Name></Properties>
    </Catalog>
</MetaDataObject>"#,
    )
    .unwrap();
    let module_path = module_dir.join("ObjectModule.bsl");
    fs::write(&module_path, SOURCE).unwrap();

    let mut db = RootDatabaseImpl::new();
    db.set_all_config_paths(
        vec![(None, root.to_path_buf())],
        Some(bsl_metadata::ExtensionScope::InfoBase),
    );
    let file_id = FileId::from_raw(1);
    let mut file_set = FileSet::default();
    file_set.insert(file_id, VfsPath::new(module_path.to_string_lossy().into_owned()));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    db.set_file_source_root(file_id, SourceRootId(0));
    db.set_file_text(file_id, SOURCE);

    let root_string = root.to_string_lossy();
    let config_path = intern_configuration_path(&db, &root_string, 0);
    let provider = SalsaProvider::new(&db, Some(config_path));
    let config = DiagnosticsConfig::default();
    let ctx = DiagnosticsContext::new(&config, file_id, &provider);
    assert_eq!(ctx.module_metadata().module_type, bsl_metadata::ModuleType::ObjectModule);

    let diagnostics = ide_diagnostics::handlers::unused_local_variable::check(&ctx);
    let unused_locals: Vec<_> = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.code == DiagnosticCode::UnusedLocalVariable)
        .collect();
    assert_eq!(
        unused_locals.len(),
        1,
        "the incomplete context cannot prove the implicit assignment is local, while explicit locals remain visible: {diagnostics:#?}"
    );
    assert!(
        unused_locals[0].message.contains("НастоящаяЛокаль"),
        "the real declared local remains diagnosable: {diagnostics:#?}"
    );
}
