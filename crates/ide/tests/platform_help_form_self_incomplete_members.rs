#[path = "platform_help_self/support.rs"]
mod support;

use bsl_platform::{PlatformData, PlatformHelp, PlatformHelpRequest, install_platform_help};
use ide_db::base_db::SourceDatabase;
use support::*;

#[test]
fn present_form_type_without_member_does_not_prove_bare_member_absent() {
    let mut help = PlatformHelp::bundled(PlatformHelpRequest::Bundled, None);
    let form_type = help
        .snapshot
        .types
        .iter()
        .find(|ty| ty.name == "ФормаКлиентскогоПриложения")
        .expect("bundled platform has the managed form type")
        .english_name
        .clone();
    assert!(
        help.snapshot
            .methods
            .iter()
            .any(|method| { method.type_name == form_type && method.name == "Открыть" })
    );
    assert!(
        help.snapshot
            .methods
            .iter()
            .any(|method| { method.type_name == form_type && method.name == "Закрыть" })
    );
    help.snapshot
        .methods
        .retain(|method| !(method.type_name == form_type && method.name == "Закрыть"));
    install_platform_help(help).expect("fresh process owns the platform snapshot");

    let data = PlatformData::instance();
    assert!(data.get_type("ФормаКлиентскогоПриложения").is_some());
    assert!(data.get_method("ФормаКлиентскогоПриложения", "Открыть").is_some());
    assert!(data.get_method("ФормаКлиентскогоПриложения", "Закрыть").is_none());

    let (mut db, file_id, root) = database();
    assert_managed_form(root.path(), &db, file_id);
    db.set_file_text(
        file_id,
        "Процедура Тест()\n    Открыть();\n    Закрыть();\n    Результат = НеизвестноеИмяФормы;\nКонецПроцедуры\n",
    );
    let (calls, names) = unresolved(&db, file_id);
    assert!(!calls.iter().any(|name| name == "Открыть"), "{calls:?}");
    assert!(
        !calls.iter().any(|name| name == "Закрыть"),
        "a present type with an incomplete method surface cannot prove absence: {calls:?}"
    );
    assert!(
        !names.iter().any(|name| name == "НеизвестноеИмяФормы"),
        "a present type with an incomplete member surface makes ordinary reads indeterminate: {names:?}"
    );
}
