//! Syntax-only callback protection for dynamic callback-description constructors.

#[path = "platform_help/common.rs"]
#[allow(dead_code, reason = "The shared fixture also provides helpers used by other test targets.")]
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
fn unrelated_branch_does_not_turn_payload_into_handler() {
    install_none();
    let (mut db, file_id, _fixture) = database(None);
    db.set_file_text(
        file_id,
        r#"Процедура Создать()
    Параметры = Новый Массив;
    Параметры.Добавить("Callback");
    Параметры.Добавить(ЭтотОбъект);
    Параметры.Добавить("Payload");
    Параметры.Добавить(Неопределено);
    Параметры.Добавить(Неопределено);
    Если Условие Тогда
        Сообщить("x");
    КонецЕсли;
    Описание = Новый("ОписаниеОповещения", Параметры);
КонецПроцедуры

Процедура Callback(Параметр)
КонецПроцедуры
Процедура Payload(НеИспользуется)
КонецПроцедуры
"#,
    );
    let summary = db.module_call_summary(hir::ModuleId::new(file_id));
    let names: Vec<_> =
        summary.notify_dynamic_signature_handlers.iter().map(|n| n.as_str()).collect();
    assert!(names.contains(&"Callback"), "{names:?}");
    assert!(!names.contains(&"Payload"), "payload is not a handler: {names:?}");
}

#[test]
fn indexed_write_before_constructor_preserves_real_callback() {
    install_none();
    let (mut db, file_id, _fixture) = database(None);
    db.set_file_text(
        file_id,
        r#"Процедура Создать()
    Параметры = Новый Массив;
    Параметры.Добавить("Резерв");
    Параметры.Добавить(ЭтотОбъект);
    Параметры[0] = "ПослеКоманды";
    Описание = Новый("ОписаниеОповещения", Параметры);
КонецПроцедуры

Процедура ПослеКоманды(Результат, ДопПараметры)
КонецПроцедуры
"#,
    );
    let summary = db.module_call_summary(hir::ModuleId::new(file_id));
    let names: Vec<_> =
        summary.notify_dynamic_signature_handlers.iter().map(|n| n.as_str()).collect();
    assert!(names.contains(&"ПослеКоманды"), "indexed write may supply callback: {names:?}");
    let diagnostics =
        Analysis::from_database(db).diagnostics(file_id, &DiagnosticsConfig::all_enabled());
    assert!(
        diagnostics.iter().all(|d| d.code != ide::DiagnosticCode::UnusedParameters),
        "{diagnostics:?}"
    );
}

#[test]
fn uncertain_write_channels_supply_possible_handler_names() {
    install_none();
    let (mut db, file_id, _fixture) = database(None);
    db.set_file_text(
        file_id,
        r#"Процедура Создать()
    Параметры = Новый Массив;
    Параметры.Добавить("Резерв");
    Псевдоним = Параметры;
    Псевдоним.Добавить("ЧерезПсевдоним");
    ЭтотОбъект.Параметры.Добавить("ЧерезСвойство");
    Чужой.Параметры.Добавить("ЧужойОбработчик");
    Параметры[0] = "ПослеИндекса";
    Описание = Новый("ОписаниеОповещения", Параметры);
КонецПроцедуры

Процедура ПослеИндекса(Результат, ДопПараметры)
КонецПроцедуры
"#,
    );
    let summary = db.module_call_summary(hir::ModuleId::new(file_id));
    let names: Vec<_> =
        summary.notify_dynamic_signature_handlers.iter().map(|n| n.as_str()).collect();
    // A write through an index, an alias or the object's own same-named member may
    // replace or extend any slot, so every string the array may hold stays protected;
    // a same-named member of another receiver is an unrelated object.
    for expected in ["Резерв", "ЧерезПсевдоним", "ЧерезСвойство", "ПослеИндекса"]
    {
        assert!(names.contains(&expected), "{names:?}");
    }
    assert!(!names.contains(&"ЧужойОбработчик"), "{names:?}");
}

#[test]
fn opaque_array_call_string_arguments_supply_possible_handler_names() {
    install_none();
    let (mut db, file_id, _fixture) = database(None);
    db.set_file_text(
        file_id,
        r#"Процедура Создать()
    Параметры = Новый Массив;
    Параметры.Добавить("Резерв");
    Параметры.Вставить(0, "ПослеВставки");
    Описание = Новый("ОписаниеОповещения", Параметры);
КонецПроцедуры

Процедура ПослеВставки(Результат, ДопПараметры)
КонецПроцедуры
"#,
    );
    let summary = db.module_call_summary(hir::ModuleId::new(file_id));
    let names: Vec<_> =
        summary.notify_dynamic_signature_handlers.iter().map(|n| n.as_str()).collect();
    // Any call on the array other than a recognized append makes the slot sequence
    // untrustworthy, and every identifier string it passes may end up in the array.
    for expected in ["Резерв", "ПослеВставки"] {
        assert!(names.contains(&expected), "{names:?}");
    }
    let diagnostics =
        Analysis::from_database(db).diagnostics(file_id, &DiagnosticsConfig::all_enabled());
    assert!(
        diagnostics.iter().all(|d| d.code != ide::DiagnosticCode::UnusedParameters),
        "{diagnostics:?}"
    );
}

#[test]
fn chained_alias_writes_supply_possible_handler_names() {
    install_none();
    let (mut db, file_id, _fixture) = database(None);
    db.set_file_text(
        file_id,
        r#"Процедура Создать()
    Параметры = Новый Массив;
    Параметры.Добавить("Резерв");
    Псевдоним = Параметры;
    Копия = Псевдоним;
    Копия.Добавить("ЧерезЦепочку");
    Описание = Новый("ОписаниеОповещения", Параметры);
КонецПроцедуры

Процедура ЧерезЦепочку(Параметр)
КонецПроцедуры
"#,
    );
    let summary = db.module_call_summary(hir::ModuleId::new(file_id));
    let names: Vec<_> =
        summary.notify_dynamic_signature_handlers.iter().map(|n| n.as_str()).collect();
    // An alias chain is followed to a fixed point, so a write through the last name
    // protects the possible handler it may add.
    for expected in ["Резерв", "ЧерезЦепочку"] {
        assert!(names.contains(&expected), "{names:?}");
    }
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
