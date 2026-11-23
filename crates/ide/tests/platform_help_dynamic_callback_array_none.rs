//! Syntax-only callback protection for dynamic callback-description constructors.

#[path = "platform_help/common.rs"]
mod common;

use bsl_platform::{install_platform_help, PlatformData, PlatformHelp, PlatformHelpRequest};
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
fn dynamic_array_callback_uses_platform_handler_slot_without_help() {
    install_none();
    let (mut db, file_id, _fixture) = database(None);
    db.set_file_text(
        file_id,
        r#"&НаКлиенте
Процедура Вызов()
    Параметры = Новый Массив;
    Параметры.Добавить("Callback");
    Параметры.Добавить(ЭтотОбъект);
    Описание = Новый("ОписаниеОповещения", Параметры);
КонецПроцедуры

&НаКлиенте
Процедура Callback(Результат, ДопПараметры) Экспорт
    Вызов(Результат);
КонецПроцедуры
"#,
    );

    let summary = db.module_call_summary(hir::ModuleId::new(file_id));
    assert!(summary.notify_regs.is_empty(), "none help must not create semantic registrations");
    assert_eq!(
        summary
            .notify_dynamic_signature_handlers
            .iter()
            .map(|handler| handler.as_str())
            .collect::<Vec<_>>(),
        ["Callback"]
    );
    let analysis = Analysis::from_database(db);
    let unused = analysis
        .diagnostics(file_id, &DiagnosticsConfig::all_enabled())
        .into_iter()
        .filter(|diagnostic| diagnostic.code == ide::DiagnosticCode::UnusedParameters)
        .collect::<Vec<_>>();
    assert!(unused.is_empty(), "callback signature is fixed by the constructor: {unused:?}");
}

#[test]
fn dynamic_array_extra_parameters_slot_does_not_protect_handler() {
    install_none();
    let (mut db, file_id, _fixture) = database(None);
    db.set_file_text(
        file_id,
        r#"&НаКлиенте
Процедура Вызов()
    Параметры = Новый Массив;
    Параметры.Добавить("ДругойОбработчик");
    Параметры.Добавить(ЭтотОбъект);
    Параметры.Добавить("Callback");
    Параметры.Добавить(Неопределено);
    Параметры.Добавить(Неопределено);
    Описание = Новый("ОписаниеОповещения", Параметры);
КонецПроцедуры

&НаКлиенте
Процедура Callback(Результат, ДопПараметры) Экспорт
    Вызов(Результат);
КонецПроцедуры
"#,
    );

    let analysis = Analysis::from_database(db);
    let unused = analysis
        .diagnostics(file_id, &DiagnosticsConfig::all_enabled())
        .into_iter()
        .filter(|diagnostic| diagnostic.code == ide::DiagnosticCode::UnusedParameters)
        .collect::<Vec<_>>();
    assert_eq!(unused.len(), 1, "the extra-parameters slot is not a handler slot: {unused:?}");
    assert!(unused[0].message.contains("ДопПараметры"), "{unused:?}");
}

#[test]
fn dynamic_array_reinitialization_discards_old_additions() {
    install_none();
    let (mut db, file_id, _fixture) = database(None);
    db.set_file_text(
        file_id,
        r#"Процедура Вызов()
    Параметры = Новый Массив;
    Параметры.Добавить("StaleCallback");
    Параметры = Новый Массив;
    Параметры.Добавить("LiveCallback");
    Параметры.Добавить(ЭтотОбъект);
    Описание = Новый("ОписаниеОповещения", Параметры);
КонецПроцедуры

Процедура StaleCallback(StaleParameter)
    Сообщить("stale");
КонецПроцедуры

Процедура LiveCallback(LiveParameter)
    Сообщить("live");
КонецПроцедуры
"#,
    );

    let summary = db.module_call_summary(hir::ModuleId::new(file_id));
    assert_eq!(
        summary
            .notify_dynamic_signature_handlers
            .iter()
            .map(|handler| handler.as_str())
            .collect::<Vec<_>>(),
        ["LiveCallback"]
    );
    let analysis = Analysis::from_database(db);
    let unused = analysis
        .diagnostics(file_id, &DiagnosticsConfig::all_enabled())
        .into_iter()
        .filter(|diagnostic| diagnostic.code == ide::DiagnosticCode::UnusedParameters)
        .collect::<Vec<_>>();
    assert_eq!(unused.len(), 1, "only additions to the current array should bind: {unused:?}");
    assert!(unused[0].message.contains("StaleParameter"), "{unused:?}");
}

#[test]
fn conditional_array_reset_keeps_possible_handler_names() {
    install_none();
    let (mut db, file_id, _fixture) = database(None);
    db.set_file_text(
        file_id,
        r#"Процедура Вызов()
    Параметры = Новый Массив;
    Параметры.Добавить("Готово");
    Если Условие Тогда
        Параметры = Новый Массив;
    КонецЕсли;
    Описание = Новый("ОписаниеОповещения", Параметры);
КонецПроцедуры

Процедура Готово(Параметр)
КонецПроцедуры
"#,
    );

    let analysis = Analysis::from_database(db);
    let unused = analysis
        .diagnostics(file_id, &DiagnosticsConfig::all_enabled())
        .into_iter()
        .filter(|diagnostic| diagnostic.code == ide::DiagnosticCode::UnusedParameters)
        .collect::<Vec<_>>();
    assert!(unused.is_empty(), "the branch may leave the earlier handler in the array: {unused:?}");
}

#[test]
fn branching_dynamic_array_protects_every_possible_added_handler() {
    install_none();
    let (mut db, file_id, _fixture) = database(None);
    db.set_file_text(
        file_id,
        r#"Процедура Вызов()
    Параметры = Новый Массив;
    Если Условие Тогда
        Параметры.Добавить("FirstPossibleHandler");
    Иначе
        Параметры.Добавить("SecondPossibleHandler");
    КонецЕсли;
    Описание = Новый("ОписаниеОповещения", Параметры);
КонецПроцедуры

Процедура FirstPossibleHandler(Параметр)
КонецПроцедуры

Процедура SecondPossibleHandler(Параметр)
КонецПроцедуры
"#,
    );

    let summary = db.module_call_summary(hir::ModuleId::new(file_id));
    assert_eq!(
        summary
            .notify_dynamic_signature_handlers
            .iter()
            .map(|handler| handler.as_str())
            .collect::<Vec<_>>(),
        ["FirstPossibleHandler", "SecondPossibleHandler"]
    );
    let analysis = Analysis::from_database(db);
    let unused = analysis
        .diagnostics(file_id, &DiagnosticsConfig::all_enabled())
        .into_iter()
        .filter(|diagnostic| diagnostic.code == ide::DiagnosticCode::UnusedParameters)
        .collect::<Vec<_>>();
    assert!(unused.is_empty(), "each branch may supply a callback: {unused:?}");
}

#[test]
fn unrelated_dynamic_constructor_does_not_protect_added_string() {
    install_none();
    let (mut db, file_id, _fixture) = database(None);
    db.set_file_text(
        file_id,
        r#"Процедура Вызов()
    Параметры = Новый Массив;
    Параметры.Добавить("Callback");
    Данные = Новый("Структура", Параметры);
КонецПроцедуры

Процедура Callback(Параметр)
    Сообщить("ordinary");
КонецПроцедуры
"#,
    );

    let summary = db.module_call_summary(hir::ModuleId::new(file_id));
    assert!(summary.notify_dynamic_signature_handlers.is_empty());
    let analysis = Analysis::from_database(db);
    let unused = analysis
        .diagnostics(file_id, &DiagnosticsConfig::all_enabled())
        .into_iter()
        .filter(|diagnostic| diagnostic.code == ide::DiagnosticCode::UnusedParameters)
        .collect::<Vec<_>>();
    assert_eq!(unused.len(), 1, "ordinary data arrays must not protect methods: {unused:?}");
}
