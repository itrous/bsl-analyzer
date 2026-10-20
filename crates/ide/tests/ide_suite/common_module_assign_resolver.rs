use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::{RootDatabaseImpl, SalsaProvider};
use ide_diagnostics::{DiagnosticCode, DiagnosticsConfig, DiagnosticsContext};
use std::path::PathBuf;
use vfs::{FileId, FileSet, VfsPath};

fn designer_fixture_path() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-metadata/fixtures/designer"))
}

fn extension_common_module_path() -> PathBuf {
    PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../bsl-metadata/fixtures/extension_common_module"
    ))
}

fn common_module_path() -> PathBuf {
    designer_fixture_path().join("CommonModules/ПервыйОбщийМодуль/Ext/Module.bsl")
}

fn build_db_with_configs(
    text: &str,
    config_paths: Vec<(Option<String>, PathBuf)>,
) -> (RootDatabaseImpl, FileId) {
    let file_id = FileId::from_raw(1);
    let mut db = RootDatabaseImpl::new();

    let mut file_set = FileSet::default();
    file_set.insert(file_id, VfsPath::new(common_module_path().to_string_lossy().to_string()));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    db.set_file_source_root(file_id, SourceRootId(0));
    db.set_file_text(file_id, text);
    db.set_all_config_paths(config_paths, Some(bsl_metadata::ExtensionScope::InfoBase));

    (db, file_id)
}

fn setup_diagnostics_with_configs(
    text: &str,
    config_paths: Vec<(Option<String>, PathBuf)>,
) -> Vec<ide_diagnostics::Diagnostic> {
    let (db, file_id) = build_db_with_configs(text, config_paths);
    let config = DiagnosticsConfig::all_enabled();
    let provider = SalsaProvider::new(&db, None);
    let ctx = DiagnosticsContext::new(&config, file_id, &provider);

    ide_diagnostics::diagnostics(&ctx)
        .into_iter()
        .filter(|d| d.code == DiagnosticCode::CommonModuleAssign)
        .collect()
}

fn setup_diagnostics(text: &str) -> Vec<ide_diagnostics::Diagnostic> {
    setup_diagnostics_with_configs(text, vec![(None, designer_fixture_path())])
}

#[test]
fn common_module_assign_emits_for_unshadowed_assignment() {
    let text = r#"
Процедура Тест()
    ПервыйОбщийМодуль = 1;
КонецПроцедуры
"#;
    let diags = setup_diagnostics(text);
    assert_eq!(diags.len(), 1, "expected one CommonModuleAssign diagnostic");
    assert!(
        diags[0].message.contains("ПервыйОбщийМодуль"),
        "diagnostic must reference canonical CommonModule name, got: {}",
        diags[0].message
    );
}

#[test]
fn common_module_assign_suppressed_by_param_shadow() {
    let text = r#"
Процедура Тест(ПервыйОбщийМодуль)
    ПервыйОбщийМодуль = 1;
КонецПроцедуры
"#;
    let diags = setup_diagnostics(text);
    assert!(
        diags.is_empty(),
        "parameter shadow must suppress CommonModuleAssign, got {} diagnostic(s): {:?}",
        diags.len(),
        diags.iter().map(|d| &d.message).collect::<Vec<_>>(),
    );
}

#[test]
fn common_module_assign_suppressed_by_local_shadow() {
    let text = r#"
Процедура Тест()
    Перем ПервыйОбщийМодуль;
    ПервыйОбщийМодуль = 1;
КонецПроцедуры
"#;
    let diags = setup_diagnostics(text);
    assert!(
        diags.is_empty(),
        "local Перем shadow must suppress CommonModuleAssign, got {} diagnostic(s): {:?}",
        diags.len(),
        diags.iter().map(|d| &d.message).collect::<Vec<_>>(),
    );
}

#[test]
fn common_module_assign_suppressed_by_module_variable_shadow() {
    let text = r#"
Перем ПервыйОбщийМодуль;

Процедура Тест()
    ПервыйОбщийМодуль = 1;
КонецПроцедуры
"#;
    let diags = setup_diagnostics(text);
    assert!(
        diags.is_empty(),
        "module-level `Перем` must suppress CommonModuleAssign via the \
         resolver pass, got {} diagnostic(s): {:?}",
        diags.len(),
        diags.iter().map(|d| &d.message).collect::<Vec<_>>(),
    );
}

#[test]
fn common_module_assign_emits_for_cfe_only_module() {
    let text = r#"
Процедура Тест()
    РасширениеТолькоМодуль = 1;
КонецПроцедуры
"#;
    let config_paths = vec![
        (None, designer_fixture_path()),
        (Some("РасширениеОбщегоМодуля".to_string()), extension_common_module_path()),
    ];
    // Common modules are extension-private: an extension-only module is visible only
    // within that extension, so the analyzed file must live inside it (a base-config
    // file would correctly NOT see `РасширениеТолькоМодуль`).
    let file_id = FileId::from_raw(1);
    let mut db = RootDatabaseImpl::new();
    let mut file_set = FileSet::default();
    let caller_path =
        extension_common_module_path().join("CommonModules/Вызывающий/Ext/Module.bsl");
    file_set.insert(file_id, VfsPath::new(caller_path.to_string_lossy().to_string()));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    db.set_file_source_root(file_id, SourceRootId(0));
    db.set_file_text(file_id, text);
    db.set_all_config_paths(config_paths, Some(bsl_metadata::ExtensionScope::InfoBase));
    let config = DiagnosticsConfig::all_enabled();
    let provider = SalsaProvider::new(&db, None);
    let ctx = DiagnosticsContext::new(&config, file_id, &provider);

    let resolution = ctx.assignment_target_kind("РасширениеТолькоМодуль");
    assert!(
        matches!(resolution, hir::AssignmentResolution::CommonModule(_)),
        "resolver must classify CFE-only name as CommonModule (independently of \
         the `is_common_module_anywhere` fallback), got {:?}",
        resolution
    );

    let diags: Vec<_> = ide_diagnostics::diagnostics(&ctx)
        .into_iter()
        .filter(|d| d.code == DiagnosticCode::CommonModuleAssign)
        .collect();
    assert_eq!(
        diags.len(),
        1,
        "expected one CommonModuleAssign diagnostic for CFE-only name, got: {:?}",
        diags.iter().map(|d| &d.message).collect::<Vec<_>>(),
    );
    assert!(
        diags[0].message.contains("РасширениеТолькоМодуль"),
        "diagnostic must reference canonical CFE-declared CommonModule name, got: {}",
        diags[0].message
    );
}

#[test]
fn common_module_assign_quiet_for_unknown_name() {
    let text = r#"
Процедура Тест()
    СовершенноПроизвольноеИмя = 1;
КонецПроцедуры
"#;
    let diags = setup_diagnostics(text);
    assert!(
        diags.is_empty(),
        "unrelated identifier must not produce CommonModuleAssign, got {} diagnostic(s): {:?}",
        diags.len(),
        diags.iter().map(|d| &d.message).collect::<Vec<_>>(),
    );
}
