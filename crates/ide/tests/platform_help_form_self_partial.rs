#[path = "platform_help_self/support.rs"]
mod support;

use bsl_platform::{
    install_platform_help, MemberSurface, PlatformData, PlatformHelp, PlatformHelpRequest,
};
use hir::{Builders, DefDatabase, DefWithBodyId, Expr, ExprId, IdConversion, ModuleId, Stmt};
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
    help.snapshot.properties.retain(|property| {
        !(property.type_name == "ClientApplicationForm" && property.english_name == "UniqueKey")
    });
    help.snapshot
        .member_surface_attestations
        .retain(|attestation| !attestation.type_name.eq_ignore_ascii_case("ClientApplicationForm"));
    install_platform_help(help).expect("fresh process owns the platform snapshot");
    let data = PlatformData::instance();
    assert!(data.get_type("ФормаКлиентскогоПриложения").is_some());
    assert!(data.get_type("Расширение формы клиентского приложения для документа").is_none());
    assert!(data.get_property("ClientApplicationForm", "Заголовок").is_some());
    assert!(
        !data.platform_type_member_surface_verified("ClientApplicationForm", MemberSurface::Fields),
        "removing a sibling property leaves the form field surface incomplete"
    );

    let (mut db, file_id, root) = database();
    let form = root.path().join("DataProcessors/Обработка1/Forms/Форма/Ext/Form.xml");
    std::fs::write(form, r#"<Form xmlns="http://v8.1c.ru/8.3/xcf/logform" xmlns:v8="http://v8.1c.ru/8.1/data/core" xmlns:cfg="http://v8.1c.ru/8.1/data/enterprise/current-config" version="2.10"><Attributes><Attribute name="Объект" id="1"><Type><v8:Type>cfg:UnknownType.Неизвестный</v8:Type></Type><MainAttribute>true</MainAttribute></Attribute></Attributes></Form>"#).unwrap();
    db.set_file_text(
        file_id,
        "&НаКлиенте\nПроцедура Тест()\n    Заголовок = \"тест\";\n    ПолучитьНавигационнуюСсылкуОбъекта();\nКонецПроцедуры\n",
    );
    assert_managed_form(root.path(), &db, file_id);
    let module_bodies = db.module_bodies(ModuleId::new(file_id));
    let (method_id, body) = module_bodies.iter_bodies().next().expect("form method");
    let target = body
        .stmts_iter()
        .find_map(|(_, stmt)| {
            let Stmt::Assign { target, .. } = stmt else { return None };
            let target = ExprId::from_idx(*target);
            matches!(body.expr(target), Expr::Path(name) if name.as_str() == "Заголовок")
                .then_some(target)
        })
        .expect("the form property assignment target");
    let inference = hir::infer_owner(&db, file_id, DefWithBodyId::Method(method_id));
    assert!(
        matches!(
            inference.assignment_target_resolution(target),
            Some(hir::BareNameResolution::Resolved {
                ty,
                origin: hir::BareNameOrigin::FormMember,
                ..
            }) if *ty != db.unknown()
        ),
        "a listed property retains its semantic FormMember owner despite the incomplete surface: {:?}",
        inference.assignment_target_resolution(target)
    );
    let (calls, names) = unresolved(&db, file_id);
    assert!(!calls.iter().any(|name| name == "ПолучитьНавигационнуюСсылкуОбъекта"), "{calls:?}");
    assert!(!names.iter().any(|name| name == "ПолучитьНавигационнуюСсылкуОбъекта"), "{names:?}");
}
