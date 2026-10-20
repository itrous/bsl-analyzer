use hir::ConfigsDatabase;
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
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

fn file_inside_designer() -> PathBuf {
    designer_fixture_path().join("CommonModules/ПервыйОбщийМодуль/Ext/Module.bsl")
}

fn file_inside_extension() -> PathBuf {
    extension_common_module_path().join("CommonModules/РасширениеТолькоМодуль/Ext/Module.bsl")
}

/// A file in the base root and a file in the extension root over the same
/// registered config set. Base files see only the base; extension files see
/// base + their own extension.
fn setup_main_plus_cfe() -> (RootDatabaseImpl, FileId, FileId) {
    let base_file = FileId::from_raw(1);
    let ext_file = FileId::from_raw(2);
    let mut db = RootDatabaseImpl::new();

    let mut file_set = FileSet::default();
    file_set.insert(base_file, VfsPath::new(file_inside_designer().to_string_lossy().to_string()));
    file_set.insert(ext_file, VfsPath::new(file_inside_extension().to_string_lossy().to_string()));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    for file_id in [base_file, ext_file] {
        db.set_file_source_root(file_id, SourceRootId(0));
        db.set_file_text(file_id, "Процедура Тест() КонецПроцедуры");
    }

    db.set_all_config_paths(
        vec![
            (None, designer_fixture_path()),
            (Some("РасширениеОбщегоМодуля".to_string()), extension_common_module_path()),
        ],
        Some(bsl_metadata::ExtensionScope::InfoBase),
    );
    (db, base_file, ext_file)
}

#[test]
fn configurations_returns_main_then_own_cfe_in_registration_order() {
    let (db, base_file, ext_file) = setup_main_plus_cfe();

    let base_configs = db.configurations(base_file);
    assert_eq!(base_configs.len(), 1, "a base file must see only the base configuration");
    assert_eq!(base_configs[0].name, None, "main config must come first with name = None");

    let ext_configs = db.configurations(ext_file);
    assert_eq!(ext_configs.len(), 2, "an extension file sees main + its own CFE");
    assert_eq!(ext_configs[0].name, None, "main config must come first with name = None");
    assert_eq!(
        ext_configs[1].name.as_deref(),
        Some("РасширениеОбщегоМодуля"),
        "the file's own CFE must follow with its registered extension name",
    );
}

#[test]
fn cfe_only_common_module_resolves_through_extension_entry() {
    let (db, _base_file, ext_file) = setup_main_plus_cfe();
    let configs = db.configurations(ext_file);

    let main = &configs[0].configuration;
    assert!(
        main.find_common_module("РасширениеТолькоМодуль").is_none(),
        "the CFE-only module must NOT exist in the main configuration",
    );

    let cfe = &configs[1].configuration;
    let cm = cfe.find_common_module("РасширениеТолькоМодуль");
    assert!(cm.is_some(), "CFE-only CommonModule must be findable on the extension entry",);
}

#[test]
fn shared_common_module_lookup_finds_main_independently_of_cfe() {
    let (db, _base_file, ext_file) = setup_main_plus_cfe();
    let configs = db.configurations(ext_file);

    let main = &configs[0].configuration;
    assert!(
        main.find_common_module("ПервыйОбщийМодуль").is_some(),
        "main config must own its declared CommonModule",
    );

    let cfe = &configs[1].configuration;
    assert!(
        cfe.find_common_module("ПервыйОбщийМодуль").is_none(),
        "CFE must NOT inherit main config's CommonModules — visibility is set-union, \
         not metadata merge",
    );
}
