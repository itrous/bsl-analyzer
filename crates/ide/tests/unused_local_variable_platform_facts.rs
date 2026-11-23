//! Platform context writes cannot be proven to be locals when help facts are absent.
//! This binary launches each platform selection in a fresh process because the
//! platform snapshot is process-wide.

use bsl_platform::{
    install_platform_help, PlatformData, PlatformDataInner, PlatformHelp, PlatformHelpRequest,
};
use hir::{DefDatabase, ModuleId};
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use ide_diagnostics::{DiagnosticCode, DiagnosticsConfig};
use std::path::PathBuf;
use std::process::Command;
use vfs::{FileId, FileSet, VfsPath};

fn designer_root() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-metadata/fixtures/designer"))
}

fn db_for(relative_path: &str, source: &str) -> (RootDatabaseImpl, FileId) {
    let root = designer_root();
    let path = root.join(relative_path);
    let file = FileId(0);
    let mut files = FileSet::default();
    files.insert(file, VfsPath::from(path));
    let mut db = RootDatabaseImpl::new();
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(files));
    db.set_file_source_root(file, SourceRootId(0));
    db.set_all_config_paths(vec![(None, root)], None);
    db.set_file_text(file, source);
    (db, file)
}

fn unused_messages(db: &RootDatabaseImpl, file: FileId) -> Vec<String> {
    ide_diagnostics::file_diagnostics(db, file, &DiagnosticsConfig::all_enabled())
        .into_iter()
        .filter(|diagnostic| diagnostic.code == DiagnosticCode::UnusedLocalVariable)
        .map(|diagnostic| diagnostic.message)
        .collect()
}

fn test_mode(mode: &str) {
    let request = match mode {
        "none" => PlatformHelpRequest::None,
        "bundled" => PlatformHelpRequest::Bundled,
        _ => panic!("unknown platform mode: {mode}"),
    };
    let help = PlatformHelp::without_io(&request).expect("selection requires no I/O");
    install_platform_help(help).expect("fresh child process owns the platform snapshot");

    let data = PlatformData::instance();
    let is_empty = mode == "none";
    assert_eq!(
        data.all_types().is_empty(),
        is_empty,
        "all_types().is_empty() is the conservative-mode condition"
    );
    assert_eq!(
        data.get_type_properties("ФормаКлиентскогоПриложения").is_empty(),
        is_empty,
        "the selected managed-form facts"
    );
    assert_eq!(
        PlatformDataInner::instance().form_extension_property_names().is_empty(),
        is_empty,
        "the selected form-extension facts"
    );

    const FORM_SOURCE: &str = "\
Процедура Тест()\n\
    Заголовок = \"x\";\n\
    ТекущийЭлемент = Неопределено;\n\
    КлючНазначенияИспользования = \"x\";\n\
    МертваяЛокаль = 1;\n\
КонецПроцедуры\n";
    for (relative_path, managed) in [
        ("Catalogs/Справочник1/Forms/ФормаЭлемента/Ext/Form/Module.bsl", true),
        ("Catalogs/Справочник1/Forms/ФормаОбычная/Ext/Form/Module.bsl", false),
    ] {
        let (db, file) = db_for(relative_path, FORM_SOURCE);
        let metadata = db.module_metadata(ModuleId::new(file));
        assert_eq!(metadata.module_type, bsl_metadata::ModuleType::FormModule);
        assert_eq!(metadata.form.as_ref().is_some_and(|form| form.is_managed()), managed);

        let messages = unused_messages(&db, file);
        assert!(
            !messages.iter().any(|message| {
                message.contains("Заголовок") || message.contains("ТекущийЭлемент")
            }),
            "form context assignments are not unused locals ({mode}, {relative_path}): {messages:?}"
        );
        assert_eq!(
            messages.iter().any(|message| message.contains("КлючНазначенияИспользования")),
            mode == "bundled" && !managed,
            "managed-only property membership is preserved with facts ({mode}, {relative_path}): {messages:?}"
        );
        assert_eq!(
            messages.iter().any(|message| message.contains("МертваяЛокаль")),
            mode == "bundled",
            "an implicit assignment is inconclusive without platform facts ({mode}, {relative_path}): {messages:?}"
        );

        let explicit_local =
            "Процедура Тест()\n    Перем Заголовок;\n    Заголовок = 1;\nКонецПроцедуры\n";
        let (db, file) = db_for(relative_path, explicit_local);
        let messages = unused_messages(&db, file);
        assert!(
            messages.iter().any(|message| message.contains("Заголовок")),
            "an explicit local is still diagnosed ({mode}, {relative_path}): {messages:?}"
        );
    }

    let (db, file) = db_for(
        "Reports/ТестовыйОтчёт/Forms/Форма/Ext/Form/Module.bsl",
        "Процедура Тест()\n    ВариантМодифицирован = Истина;\n    МертваяЛокаль = 1;\nКонецПроцедуры\n",
    );
    assert!(db.module_metadata(ModuleId::new(file)).form.is_some());
    let messages = unused_messages(&db, file);
    assert!(
        !messages.iter().any(|message| message.contains("ВариантМодифицирован")),
        "report-form extension assignment is not an unused local ({mode}): {messages:?}"
    );
    assert_eq!(
        messages.iter().any(|message| message.contains("МертваяЛокаль")),
        mode == "bundled",
        "an implicit assignment is inconclusive without platform facts ({mode}): {messages:?}"
    );

    let (db, file) = db_for(
        "Constants/СтрокаКонст/Ext/ValueManagerModule.bsl",
        "Процедура Тест()\n    ДополнительныеСвойства = 1;\n    МертваяЛокаль = 1;\nКонецПроцедуры\n",
    );
    assert_eq!(
        db.module_metadata(ModuleId::new(file)).module_type,
        bsl_metadata::ModuleType::ValueManagerModule
    );
    let value_manager_fields = hir::module_implicit_field_names(&db, file);
    if mode == "bundled" {
        assert!(
            value_manager_fields.iter().any(|name| name == "дополнительныесвойства"),
            "bundled facts expose value-manager implicit fields: {value_manager_fields:?}"
        );
    } else {
        assert!(
            !value_manager_fields.iter().any(|name| name == "дополнительныесвойства"),
            "none help removes platform-owned fields while retaining metadata fields: {value_manager_fields:?}"
        );
    }
    let messages = unused_messages(&db, file);
    assert!(
        !messages.iter().any(|message| message.contains("ДополнительныеСвойства")),
        "a value-manager context write is not an unused local ({mode}): {messages:?}"
    );
    assert_eq!(
        messages.iter().any(|message| message.contains("МертваяЛокаль")),
        mode == "bundled",
        "an implicit assignment is inconclusive without platform facts ({mode}): {messages:?}"
    );

    let (db, file) = db_for(
        "CommonModules/ПервыйОбщийМодуль/Ext/Module.bsl",
        "Перем Заголовок;\nПроцедура Тест()\n    Локальная = 1;\nКонецПроцедуры\n",
    );
    let messages = unused_messages(&db, file);
    assert!(
        messages.iter().any(|message| message.contains("Заголовок")),
        "explicit module variables remain diagnosable ({mode})"
    );
    assert!(
        messages.iter().any(|message| message.contains("Локальная")),
        "implicit assignments remain diagnosable outside context-bearing modules ({mode})"
    );

    let (db, file) = db_for(
        "Catalogs/Справочник1/Ext/ManagerModule.bsl",
        "Процедура Тест()\n    Локальная = 1;\nКонецПроцедуры\n",
    );
    assert_eq!(
        db.module_metadata(ModuleId::new(file)).module_type,
        bsl_metadata::ModuleType::ManagerModule
    );
    assert!(hir::module_implicit_field_names(&db, file).is_empty());
    assert!(
        unused_messages(&db, file).iter().any(|message| message.contains("Локальная")),
        "manager modules do not have bare implicit context fields ({mode})"
    );

    // Object and record-set modules consume platform fields through
    // module_implicit_fields/enumerate_fields_inner. Empty facts make the owner of
    // bare assignments inconclusive; bundled facts identify these context fields.
    for (relative_path, module_type, source) in [
        (
            "Catalogs/Справочник1/Ext/ObjectModule.bsl",
            bsl_metadata::ModuleType::ObjectModule,
            "Процедура Тест()\n    ДополнительныеСвойства = 1;\n    ОбменДанными = 1;\nКонецПроцедуры\n",
        ),
        (
            "InformationRegisters/РегистрСведений1/Ext/RecordSetModule.bsl",
            bsl_metadata::ModuleType::RecordSetModule,
            "Процедура Тест()\n    ДополнительныеСвойства = 1;\n    ОбменДанными = 1;\n    Отбор = 1;\nКонецПроцедуры\n",
        ),
    ] {
        let (db, file) = db_for(relative_path, source);
        assert_eq!(db.module_metadata(ModuleId::new(file)).module_type, module_type);
        let fields = hir::module_implicit_field_names(&db, file);
        if mode == "bundled" {
            assert!(
                fields.iter().any(|name| name == "дополнительныесвойства"),
                "bundled facts expose platform context fields for {module_type:?}: {fields:?}"
            );
        } else {
            assert!(
                !fields.iter().any(|name| name == "дополнительныесвойства" || name == "обменданными"),
                "none help removes platform-owned fields while retaining metadata fields: {fields:?}"
            );
        }
        assert!(
            unused_messages(&db, file).is_empty(),
            "module applicability must prevent a false local finding ({mode}, {module_type:?})"
        );
    }
}

#[test]
fn platform_modes_keep_form_context_assignments_out_of_unused_locals() {
    if let Ok(mode) = std::env::var("UNUSED_LOCAL_PLATFORM_MODE") {
        test_mode(&mode);
        return;
    }

    let executable = std::env::current_exe().expect("integration test executable");
    for mode in ["none", "bundled"] {
        let status = Command::new(&executable)
            .args([
                "--exact",
                "platform_modes_keep_form_context_assignments_out_of_unused_locals",
                "--nocapture",
            ])
            .env("UNUSED_LOCAL_PLATFORM_MODE", mode)
            .status()
            .expect("spawn isolated platform mode process");
        assert!(status.success(), "platform mode {mode} failed with {status}");
    }
}
