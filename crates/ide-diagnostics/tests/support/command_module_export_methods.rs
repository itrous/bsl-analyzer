use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::{metadata::intern_configuration_path, RootDatabaseImpl, SalsaProvider};
use ide_diagnostics::{Diagnostic, DiagnosticsConfig, DiagnosticsContext};
use std::fs;
use vfs::{FileId, FileSet, VfsPath};

const SOURCE: &str = r#"&НаКлиенте
Процедура Зарегистрировать()
    Оповещение = Новый ОписаниеОповещения("ПослеКоманды", ЭтотОбъект);
КонецПроцедуры

&НаКлиенте
Процедура ПослеКоманды() Экспорт
КонецПроцедуры

Процедура ОбычныйЭкспорт() Экспорт
КонецПроцедуры
"#;

pub fn command_module_export_diagnostics() -> (Vec<Diagnostic>, String) {
    let dir = tempfile::tempdir().expect("temporary command configuration");
    let root = dir.path();
    let command_dir = root.join("CommonCommands/ПроверкаЭкспортов");
    fs::create_dir_all(command_dir.join("Ext")).unwrap();
    fs::write(
        root.join("Configuration.xml"),
        r#"<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <Configuration uuid="00000000-0000-0000-0000-000000000001">
        <Properties><Name>ПроверкаЭкспортов</Name></Properties>
    </Configuration>
</MetaDataObject>"#,
    )
    .unwrap();
    fs::write(
        root.join("CommonCommands/ПроверкаЭкспортов.xml"),
        r#"<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <CommonCommand uuid="00000000-0000-0000-0000-000000000002">
        <Properties><Name>ПроверкаЭкспортов</Name></Properties>
    </CommonCommand>
</MetaDataObject>"#,
    )
    .unwrap();
    let module_path = command_dir.join("Ext/CommandModule.bsl");
    fs::write(&module_path, SOURCE).unwrap();

    let mut db = RootDatabaseImpl::new();
    db.set_all_config_paths(
        vec![(None, root.to_path_buf())],
        Some(bsl_metadata::ExtensionScope::InfoBase),
    );
    let file_id = FileId::from_raw(1);
    let mut file_set = FileSet::default();
    file_set.insert(file_id, VfsPath::new(module_path.to_string_lossy().into_owned()));
    let source_root = SourceRoot::new_local(file_set);
    db.set_source_root(SourceRootId(0), source_root);
    db.set_file_source_root(file_id, SourceRootId(0));
    db.set_file_text(file_id, SOURCE);

    let root_string = root.to_string_lossy();
    let config_path = intern_configuration_path(&db, &root_string, 0);
    let provider = SalsaProvider::new(&db, Some(config_path));
    let config = DiagnosticsConfig::default();
    let ctx = DiagnosticsContext::new(&config, file_id, &provider);
    assert_eq!(
        ctx.module_metadata().module_type,
        bsl_metadata::ModuleType::CommandModule,
        "the test must exercise a real CommandModule"
    );
    let diagnostics = ide_diagnostics::handlers::command_module_export_methods::check(&ctx);
    (diagnostics, SOURCE.to_string())
}
