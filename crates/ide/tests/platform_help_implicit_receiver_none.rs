//! Missing platform help must not make metadata-backed implicit receivers look complete.

use bsl_platform::{
    install_platform_help, PlatformCatalogStatus, PlatformData, PlatformGlobalCatalog,
    PlatformHelp, PlatformHelpRequest,
};
use hir::{DefDatabase, HirDatabase, InferenceDiagnostic, ModuleId};
use ide_db::{
    base_db::{SourceDatabase, SourceRoot, SourceRootId},
    RootDatabaseImpl,
};
use std::{path::PathBuf, process::Command, sync::Arc};
use tempfile::TempDir;
use vfs::{FileId, FileSet, VfsPath};

const DESIGNER: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-metadata/fixtures/designer");
const PROBES: &[(&str, &str, &str, bsl_metadata::ModuleType)] = &[
    (
        "Reports/ТестовыйОтчёт/Ext/ObjectModule.bsl",
        "Процедура Тест()\n    ПолучитьМакет();\nКонецПроцедуры\n",
        "ПолучитьМакет",
        bsl_metadata::ModuleType::ObjectModule,
    ),
    (
        "Catalogs/Справочник1/Ext/ManagerModule.bsl",
        "Процедура Тест()\n    СоздатьЭлемент();\nКонецПроцедуры\n",
        "СоздатьЭлемент",
        bsl_metadata::ModuleType::ManagerModule,
    ),
    (
        "InformationRegisters/РегистрСведений1/Ext/RecordSetModule.bsl",
        "Процедура Тест()\n    Загрузить();\nКонецПроцедуры\n",
        "Загрузить",
        bsl_metadata::ModuleType::RecordSetModule,
    ),
    (
        "Constants/СтрокаКонст/Ext/ValueManagerModule.bsl",
        "Процедура Тест()\n    Метаданные();\nКонецПроцедуры\n",
        "Метаданные",
        bsl_metadata::ModuleType::ValueManagerModule,
    ),
];

fn write(path: PathBuf, content: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn configuration() -> TempDir {
    let root = tempfile::tempdir().unwrap();
    let ns = "http://v8.1c.ru/8.3/MDClasses";
    write(
        root.path().join("Configuration.xml"),
        &format!(
            r#"<MetaDataObject xmlns="{ns}" version="2.10"><Configuration><Properties><Name>Test</Name></Properties><ChildObjects><Catalog>Справочник1</Catalog><Report>ТестовыйОтчёт</Report><InformationRegister>РегистрСведений1</InformationRegister><Constant>СтрокаКонст</Constant><CommonModule>ПервыйОбщийМодуль</CommonModule></ChildObjects></Configuration></MetaDataObject>"#
        ),
    );
    for relative in [
        "Catalogs/Справочник1.xml",
        "Reports/ТестовыйОтчёт.xml",
        "InformationRegisters/РегистрСведений1.xml",
        "Constants/СтрокаКонст.xml",
        "CommonModules/ПервыйОбщийМодуль.xml",
    ] {
        let from = PathBuf::from(DESIGNER).join(relative);
        let to = root.path().join(relative);
        std::fs::create_dir_all(to.parent().unwrap()).unwrap();
        std::fs::copy(from, to).unwrap();
    }
    root
}

fn database(root: &TempDir, relative: &str, source: &str) -> (RootDatabaseImpl, FileId) {
    let path = root.path().join(relative);
    write(path.clone(), source);
    let file_id = FileId::from_raw(1);
    let mut db = RootDatabaseImpl::new();
    let mut files = FileSet::default();
    files.insert(file_id, VfsPath::new(path.to_string_lossy().into_owned()));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(files));
    db.set_file_source_root(file_id, SourceRootId(0));
    db.set_file_text(file_id, source);
    db.set_all_config_paths(
        vec![(None, root.path().to_path_buf())],
        Some(bsl_metadata::ExtensionScope::InfoBase),
    );
    db.set_target_platform_version(Some(Arc::<str>::from("8.3.27")));
    (db, file_id)
}

fn unresolved_names(db: &RootDatabaseImpl, file_id: FileId) -> (Vec<String>, Vec<String>) {
    let diagnostics = &db.infer(file_id).diagnostics;
    let calls = diagnostics
        .iter()
        .filter_map(|(_, diagnostic)| match diagnostic {
            InferenceDiagnostic::UnresolvedBareCall { name, .. } => Some(name.as_str().to_owned()),
            _ => None,
        })
        .collect();
    let names = diagnostics
        .iter()
        .filter_map(|(_, diagnostic)| match diagnostic {
            InferenceDiagnostic::UnresolvedName { name, .. } => Some(name.as_str().to_owned()),
            _ => None,
        })
        .collect();
    (calls, names)
}

#[test]
fn metadata_receivers_are_indeterminate_without_facts_but_common_module_misses_remain() {
    for mode in ["bundled", "none"] {
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "isolated_platform_help_probe", "--nocapture"])
            .env("PIO_PLATFORM_HELP_PROBE", mode)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{mode} probe failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn isolated_platform_help_probe() {
    let Ok(mode) = std::env::var("PIO_PLATFORM_HELP_PROBE") else {
        return;
    };
    let request = match mode.as_str() {
        "bundled" => PlatformHelpRequest::Bundled,
        "none" => PlatformHelpRequest::None,
        other => panic!("unexpected probe mode {other}"),
    };
    let help = PlatformHelp::without_io(&request).unwrap();
    let _ = install_platform_help(help);
    assert_eq!(PlatformData::instance().help_request(), &request);
    let root = configuration();

    if mode == "bundled" {
        assert!(!PlatformData::instance().all_types().is_empty());
    } else {
        assert!(PlatformData::instance().all_types().is_empty());
        assert!(PlatformData::instance().all_methods().is_empty());
        assert_eq!(
            PlatformGlobalCatalog::instance().status_for_target(Some("8.3.27")),
            PlatformCatalogStatus::Complete
        );
    }

    for (path, source, expected, module_type) in PROBES {
        let (db, file_id) = database(&root, path, source);
        let metadata = db.module_metadata(ModuleId::new(file_id));
        assert_eq!(metadata.module_type, *module_type, "{path}");
        assert!(
            metadata.mdo.is_some() || metadata.register.is_some(),
            "real MDO metadata required for {path}: {metadata:?}"
        );
        let (calls, names) = unresolved_names(&db, file_id);
        assert!(
            !calls.iter().any(|name| name == expected) && !names.iter().any(|name| name == expected),
            "implicit receiver method {expected} was diagnosed ({mode}, {path}): calls={calls:?}, names={names:?}"
        );
    }

    let (db, file_id) = database(
        &root,
        "CommonModules/ПервыйОбщийМодуль/Ext/Module.bsl",
        "Процедура Тест()\n    НесуществующийОбщийВызов();\nКонецПроцедуры\n",
    );
    assert_eq!(
        db.module_metadata(ModuleId::new(file_id)).module_type,
        bsl_metadata::ModuleType::CommonModule
    );
    let (calls, names) = unresolved_names(&db, file_id);
    assert!(
        calls.iter().any(|name| name == "НесуществующийОбщийВызов")
            || names.iter().any(|name| name == "НесуществующийОбщийВызов"),
        "complete EDT still diagnoses an absent common-module call ({mode}): calls={calls:?}, names={names:?}"
    );
}
