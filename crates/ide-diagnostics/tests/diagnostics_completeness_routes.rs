//! Completeness must cover every semantic route used to assemble extension diagnostics.

use base_db::{SourceRoot, SourceRootId};
use ide_db::{base_db::SourceDatabase, RootDatabaseImpl};
use ide_diagnostics::{
    diagnostics_completeness, file_diagnostics_result, DiagnosticCode, DiagnosticsCompleteness,
    DiagnosticsConfig,
};
use vfs::{FileId, FileSet, VfsPath};

const STOPPING_BODY: &str = r#"
    Перем Параметры;
    Параметры = Новый Массив;
    Параметры.Добавить("Провал");
    Параметры.Добавить(ЭтотОбъект);
    О = Новый("CallbackDescription", Параметры);
"#;

fn extension_database(extension: &str) -> (tempfile::TempDir, RootDatabaseImpl, FileId, FileId) {
    let temp = tempfile::tempdir().expect("temporary project root");
    let base_root = temp.path().join("src/cf");
    let extension_root = temp.path().join("src/cfe/X");
    std::fs::create_dir_all(&base_root).expect("base root");
    std::fs::create_dir_all(&extension_root).expect("extension root");

    let base_file = FileId(0);
    let extension_file = FileId(1);
    let base_path = base_root.join("CommonModules/M/Ext/Module.bsl");
    let extension_path = extension_root.join("CommonModules/M/Ext/Module.bsl");
    let mut files = FileSet::default();
    files.insert(base_file, VfsPath::new(base_path.to_string_lossy().as_ref()));
    files.insert(extension_file, VfsPath::new(extension_path.to_string_lossy().as_ref()));

    let mut db = RootDatabaseImpl::new();
    db.set_all_config_paths(
        vec![(None, base_root), (Some("X".to_string()), extension_root)],
        Some(bsl_metadata::ExtensionScope::InfoBase),
    );
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(files));
    db.set_file_source_root(base_file, SourceRootId(0));
    db.set_file_source_root(extension_file, SourceRootId(0));
    db.set_file_text(base_file, "Процедура Цель() Экспорт\nКонецПроцедуры\n");
    db.set_file_text(extension_file, extension);

    (temp, db, base_file, extension_file)
}

fn incomplete_config() -> DiagnosticsConfig {
    DiagnosticsConfig {
        only_enabled: Some(vec![DiagnosticCode::MissingNotifyHandler]),
        ..DiagnosticsConfig::default()
    }
}

#[test]
fn cav_insertion_and_extension_method_routes_contribute_to_both_public_apis() {
    let cav = format!(
        "&ИзменениеИКонтроль(\"Цель\")\nПроцедура Расш_Цель()\n#Вставка\n{}\n#КонецВставки\nКонецПроцедуры\n",
        STOPPING_BODY
    );
    let (_cav_temp, cav_db, _, cav_file) = extension_database(&cav);
    let cav_config = incomplete_config();

    let (effective_stopped, cav_result, cav_completeness) =
        dataflow::reaching_defs::with_visit_budget(1, || {
            let effective = ide_db::effective_target(&cav_db, cav_file)
                .expect("CAV fixture has an effective route");
            let effective_stopped =
                hir::semantic_module_call_summary_effective_query(&cav_db, effective)
                    .notify_projection_incomplete;
            (
                effective_stopped,
                file_diagnostics_result(&cav_db, cav_file, &cav_config),
                diagnostics_completeness(&cav_db, cav_file, &cav_config),
            )
        });
    assert!(effective_stopped, "the reaching-definitions seam stopped the inserted body");
    assert_eq!(
        cav_result.completeness,
        DiagnosticsCompleteness::Incomplete {
            reason: ide_diagnostics::DiagnosticsIncompleteReason::NotifyProjection,
        },
        "the CAV insertion's effective route must contribute uncertainty"
    );
    assert_eq!(cav_completeness, cav_result.completeness);

    let own_method = format!("Процедура Расш_Обычная()\n{}\nКонецПроцедуры\n", STOPPING_BODY);
    let (_own_temp, own_db, _, own_file) = extension_database(&own_method);
    let own_config = incomplete_config();
    let (weaving_stopped, own_result, own_completeness) =
        dataflow::reaching_defs::with_visit_budget(1, || {
            let weaving = ide_db::weaving_target(&own_db, own_file)
                .expect("extension module has a weaving route");
            let weaving_stopped = hir::semantic_module_call_summary_weaving_query(&own_db, weaving)
                .notify_projection_incomplete;
            (
                weaving_stopped,
                file_diagnostics_result(&own_db, own_file, &own_config),
                diagnostics_completeness(&own_db, own_file, &own_config),
            )
        });
    assert!(weaving_stopped, "the reaching-definitions seam stopped the extension body");
    assert_eq!(
        own_result.completeness,
        DiagnosticsCompleteness::Incomplete {
            reason: ide_diagnostics::DiagnosticsIncompleteReason::NotifyProjection,
        },
        "the extension method's weaving route must contribute uncertainty"
    );
    assert_eq!(own_completeness, own_result.completeness);
}

#[test]
fn standalone_route_without_base_is_reported_and_disabled_consumers_stay_lazy() {
    let mut db = RootDatabaseImpl::new();
    let file = FileId(0);
    let mut files = FileSet::default();
    files.insert(file, VfsPath::new("/test/Standalone.bsl"));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(files));
    db.set_file_source_root(file, SourceRootId(0));
    db.set_file_text(file, &format!("Процедура Т()\n{}\nКонецПроцедуры", STOPPING_BODY));

    let config = incomplete_config();
    let (result, completeness) = dataflow::reaching_defs::with_visit_budget(1, || {
        (file_diagnostics_result(&db, file, &config), diagnostics_completeness(&db, file, &config))
    });
    assert!(matches!(result.completeness, DiagnosticsCompleteness::Incomplete { .. }));
    assert_eq!(completeness, result.completeness);

    let disabled = DiagnosticsConfig {
        disabled: vec![
            DiagnosticCode::MissingNotifyHandler,
            DiagnosticCode::UnusedParameters,
            DiagnosticCode::UnusedLocalMethod,
            DiagnosticCode::CommandModuleExportMethods,
        ],
        ..DiagnosticsConfig::default()
    };
    let (disabled_result, disabled_completeness) =
        dataflow::reaching_defs::with_visit_budget(1, || {
            (
                file_diagnostics_result(&db, file, &disabled),
                diagnostics_completeness(&db, file, &disabled),
            )
        });
    assert_eq!(disabled_result.completeness, DiagnosticsCompleteness::Complete);
    assert_eq!(disabled_completeness, DiagnosticsCompleteness::Complete);
}
