#[path = "platform_help_self/support.rs"]
mod support;

use bsl_platform::{install_platform_help, PlatformData, PlatformHelp, PlatformHelpRequest};
use ide_db::base_db::SourceDatabase;
use support::*;

#[test]
fn absent_form_surface_does_not_prove_bare_form_names_absent() {
    let help = PlatformHelp::without_io(&PlatformHelpRequest::None).unwrap();
    let _ = install_platform_help(help);
    assert_eq!(PlatformData::instance().help_request(), &PlatformHelpRequest::None);

    let (mut db, file_id, root) = database();
    assert_managed_form(root.path(), &db, file_id);
    assert!(PlatformData::instance().all_types().is_empty());
    db.set_file_text(
        file_id,
        "Процедура Тест()\n    Результат = НеизвестноеИмяФормы;\nКонецПроцедуры\n",
    );
    let (calls, names) = unresolved(&db, file_id);
    assert!(!calls.iter().any(|name| name == "Закрыть"), "{calls:?}");
    assert!(!names.iter().any(|name| name == "НеизвестноеИмяФормы"), "{names:?}");
}

#[test]
fn unreadable_form_surface_does_not_prove_bare_reads_absent() {
    use hir::{DefDatabase, ModuleId};

    let help = PlatformHelp::without_io(&PlatformHelpRequest::None).unwrap();
    let _ = install_platform_help(help);
    let (mut db, file_id, root) = database();
    std::fs::remove_file(root.path().join("DataProcessors/Обработка1/Forms/Форма/Ext/Form.xml"))
        .unwrap();
    let metadata = db.module_metadata(ModuleId::new(file_id));
    assert_eq!(metadata.module_type, bsl_metadata::ModuleType::FormModule);
    assert!(metadata.form.as_deref().is_none_or(|form| !form.is_managed()));
    db.set_file_text(file_id, "Процедура Тест()\n    Значение = Заголовок;\nКонецПроцедуры\n");
    let (_, names) = unresolved(&db, file_id);
    assert!(
        !names.iter().any(|name| name == "Заголовок"),
        "an unreadable implicit receiver makes an ordinary read indeterminate: {names:?}"
    );
}
