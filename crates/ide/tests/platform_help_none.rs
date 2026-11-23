//! No platform help (`source = "none"`): platform answers are empty, while local
//! resolution, completion, keyword docs and the EDT-backed absence diagnostics
//! keep working.

#[path = "platform_help/common.rs"]
mod common;

use bsl_platform::{
    install_platform_help, PlatformData, PlatformHelp, PlatformHelpRequest, PlatformHelpStatus,
};
use common::*;
use hir::DefDatabase;
use ide::{Analysis, DiagnosticsConfig};
use ide_db::base_db::SourceDatabase;

fn install_none() {
    let help = PlatformHelp::without_io(&PlatformHelpRequest::None).unwrap();
    let _ = install_platform_help(help);
    assert_eq!(PlatformData::instance().help_request(), &PlatformHelpRequest::None);
}

#[test]
fn platform_answers_are_empty_but_independent_features_remain() {
    install_none();
    assert_eq!(
        PlatformData::instance().help_status_for_target(Some("8.3.27")),
        PlatformHelpStatus::Missing
    );

    let (db, file_id, _fixture) = database(None);
    let analysis = Analysis::from_database(db);

    let method_hover = hover(&analysis, file_id, "Добавить").unwrap_or_default();
    assert!(
        !method_hover.contains("appends one element"),
        "no help, no method docs: {method_hover}"
    );
    let data = PlatformData::instance();
    assert!(data.all_types().is_empty() && data.all_methods().is_empty());
    assert!(data.get_method("Массив", "Добавить").is_none());
    let position = offset_of("Список.Добавить") + "Список.".len() as u32;
    let items = analysis.completions(file_id, position, None, ide::Locale::Ru);
    assert!(!items.iter().any(|item| item.label == "Добавить"), "no help completion: {items:?}");

    let keyword = hover(&analysis, file_id, "Если").expect("keyword hover stays");
    assert!(keyword.contains("Условный оператор"), "{keyword}");

    let empty_line = common::MODULE.find("    \nКонецПроцедуры").unwrap() as u32 + 4;
    let items = analysis.completions(file_id, empty_line, None, ide::Locale::Ru);
    assert!(items.iter().any(|i| i.label == "Локальная"), "local completion stays");

    let local = analysis.goto_definition(file_id, offset_of("Локальная = 1;") + 1);
    assert!(local.is_some(), "local name resolution stays");
}

#[test]
fn unused_parameter_protection_matches_b0_without_platform_help() {
    const SOURCE: &str = r#"&AtClient
Procedure Register()
#If MobileClient Then
    Notification = New NotifyDescription("SelfCallback", ThisObject);
    Notification = New("NotifyDescription", "DynamicCallback", ThisObject, Undefined, "DynamicErrorCallback", ThisObject);
    Notification = New ОписаниеОповещения(
        "RussianCallback", ЭтотОбъект, Неопределено, "RussianErrorCallback", ЭтотОбъект
    );
    Notification = New NotifyDescription("ForeignCallback", OtherModule);
    Notification = New CallbackDescription(
        "NotProtectedCallback", ThisObject, Undefined, "CallbackError", ThisObject
    );
    Notification = New("CallbackDescription", "DynamicAliasCallback", ThisObject);
#EndIf
EndProcedure

&AtClient
Procedure SelfCallback(UnusedSelfParameter)
    Message("self");
EndProcedure

&AtClient
Procedure DynamicCallback(UnusedDynamicParameter)
    Message("dynamic");
EndProcedure

&AtClient
Procedure DynamicErrorCallback(UnusedDynamicErrorParameter)
    Message("dynamic-error");
EndProcedure

&AtClient
Procedure RussianCallback(UnusedRussianParameter)
    Message("russian");
EndProcedure

&AtClient
Procedure RussianErrorCallback(UnusedRussianErrorParameter)
    Message("russian-error");
EndProcedure

&AtClient
Procedure ForeignCallback(UnusedForeignParameter)
    Message("foreign");
EndProcedure

&AtClient
Procedure NotProtectedCallback(UnusedAliasParameter)
    Message("alias");
EndProcedure

&AtClient
Procedure CallbackError(UnusedErrorParameter)
    Message("error");
EndProcedure

&AtClient
Procedure DynamicAliasCallback(UnusedDynamicAliasParameter)
    Message("dynamic-alias");
EndProcedure

&AtClient
Procedure DeadMethod()
EndProcedure

Register();
"#;

    install_none();
    let (mut db, file_id, _fixture) = database(None);
    db.set_file_text(file_id, SOURCE);
    let summary = db.module_call_summary(hir::ModuleId::new(file_id));
    assert!(summary.notify_regs.is_empty(), "none help should leave registrations unnormalized");
    assert_eq!(
        summary
            .notify_additional_signature_handlers
            .iter()
            .map(|handler| handler.as_str())
            .collect::<Vec<_>>(),
        ["NotProtectedCallback", "CallbackError", "DynamicAliasCallback"]
    );
    assert_eq!(
        summary
            .notify_signature_handlers
            .iter()
            .map(|handler| handler.as_str())
            .collect::<Vec<_>>(),
        [
            "SelfCallback",
            "DynamicCallback",
            "DynamicErrorCallback",
            "RussianCallback",
            "RussianErrorCallback",
        ]
    );

    let analysis = Analysis::from_database(db);
    let unused = analysis
        .diagnostics(file_id, &DiagnosticsConfig::all_enabled())
        .into_iter()
        .filter(|diagnostic| diagnostic.code == ide::DiagnosticCode::UnusedParameters)
        .map(|diagnostic| diagnostic.message)
        .collect::<Vec<_>>();
    assert_eq!(unused, ["Уберите неиспользуемый параметр \"UnusedForeignParameter\""]);

    let unused_methods = analysis
        .diagnostics(file_id, &DiagnosticsConfig::all_enabled())
        .into_iter()
        .filter(|diagnostic| diagnostic.code == ide::DiagnosticCode::UnusedLocalMethod)
        .map(|diagnostic| diagnostic.message)
        .collect::<Vec<_>>();
    for protected in [
        "SelfCallback",
        "DynamicCallback",
        "DynamicErrorCallback",
        "RussianCallback",
        "RussianErrorCallback",
        "NotProtectedCallback",
        "CallbackError",
        "DynamicAliasCallback",
    ] {
        assert!(
            !unused_methods.iter().any(|message| message.contains(protected)),
            "{protected} should remain protected without platform facts: {unused_methods:?}"
        );
    }
    assert!(unused_methods.iter().any(|message| message.contains("ForeignCallback")));
    assert!(unused_methods.iter().any(|message| message.contains("DeadMethod")));
}

#[test]
fn absence_diagnostics_follow_the_edt_catalog_not_help_status() {
    install_none();

    // Complete EDT catalog for the target: a misspelt global is still reported
    // although no help is served, and a known global is not.
    let (db, file_id, _fixture) = database(Some("8.3.27"));
    let unresolved = unresolved_bare_calls(&db, file_id);
    assert!(unresolved.iter().any(|n| n == "СтрДлинаНеСуществует"), "{unresolved:?}");
    assert!(!unresolved.iter().any(|n| n == "Сообщить" || n == "СтрДлина"), "{unresolved:?}");

    // Another release line: the EDT catalog no longer proves absence.
    let (db, file_id, _fixture) = database(Some("8.3.28"));
    assert!(unresolved_bare_calls(&db, file_id).is_empty());
}
