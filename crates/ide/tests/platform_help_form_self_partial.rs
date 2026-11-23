#[path = "platform_help_self/support.rs"]
mod support;

use bsl_platform::{install_platform_help, PlatformData, PlatformHelp, PlatformHelpRequest};
use ide_db::base_db::SourceDatabase;
use std::collections::HashSet;
use support::*;

#[test]
fn partial_form_surface_does_not_prove_extension_method_absent() {
    let mut help = PlatformHelp::bundled(PlatformHelpRequest::Bundled, None);
    let removed: HashSet<_> = help
        .snapshot
        .types
        .iter()
        .filter(|ty| ty.name.starts_with("Расширение формы клиентского приложения"))
        .map(|ty| ty.english_name.clone())
        .collect();
    assert!(!removed.is_empty());
    help.snapshot.types.retain(|ty| !removed.contains(&ty.english_name));
    help.snapshot.methods.retain(|method| !removed.contains(&method.type_name));
    install_platform_help(help).expect("fresh process owns the platform snapshot");
    let data = PlatformData::instance();
    assert!(data.get_type("ФормаКлиентскогоПриложения").is_some());
    assert!(data.get_type("Расширение формы клиентского приложения для документа").is_none());

    let (mut db, file_id, root) = database();
    let form = root.path().join("DataProcessors/Обработка1/Forms/Форма/Ext/Form.xml");
    std::fs::write(form, r#"<Form xmlns="http://v8.1c.ru/8.3/xcf/logform" xmlns:v8="http://v8.1c.ru/8.1/data/core" xmlns:cfg="http://v8.1c.ru/8.1/data/enterprise/current-config" version="2.10"><Attributes><Attribute name="Объект" id="1"><Type><v8:Type>cfg:UnknownType.Неизвестный</v8:Type></Type><MainAttribute>true</MainAttribute></Attribute></Attributes></Form>"#).unwrap();
    db.set_file_text(
        file_id,
        "&НаКлиенте\nПроцедура Тест()\n    ПолучитьНавигационнуюСсылкуОбъекта();\nКонецПроцедуры\n",
    );
    assert_managed_form(root.path(), &db, file_id);
    let (calls, names) = unresolved(&db, file_id);
    assert!(!calls.iter().any(|name| name == "ПолучитьНавигационнуюСсылкуОбъекта"), "{calls:?}");
    assert!(!names.iter().any(|name| name == "ПолучитьНавигационнуюСсылкуОбъекта"), "{names:?}");
}
