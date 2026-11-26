use super::missing_notify_handler;
use crate::{Diagnostic, DiagnosticsConfig, DiagnosticsContext};
use hir::call_graph::{
    CallerId, EdgeKind, EdgeProvenance, NotifyReceiverSyntax, NotifyTarget, ResolvedTarget,
};
use hir::{ConfigsDatabase, DefDatabase, ModuleId};
use test_fixture::CfeFixtureBuilder;

const COMMAND_MODULE_CALLER: &str = "CommonCommands/ПроверкаОповещений/Ext/CommandModule.bsl";

fn run_in_command_module(
    source: &str,
    run: impl FnOnce(&ide_db::RootDatabaseImpl, &DiagnosticsContext<'_>) -> Vec<Diagnostic>,
) -> Vec<Diagnostic> {
    crate::test_utils::check_cfe_at_with_db(
        COMMAND_MODULE_CALLER,
        source,
        CfeFixtureBuilder::new("").build(),
        &[],
        DiagnosticsConfig::all_enabled(),
        |fixture| {
            let command_dir = fixture.root().join("CommonCommands/ПроверкаОповещений");
            std::fs::create_dir_all(command_dir.join("Ext")).unwrap();
            std::fs::write(command_dir.join("Ext/CommandModule.bsl"), source).unwrap();
            std::fs::write(
                fixture.root().join("CommonCommands/ПроверкаОповещений.xml"),
                r#"<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <CommonCommand uuid="5b9f7d02-c0da-4e02-b48a-1a5f2a6d7222">
        <Properties><Name>ПроверкаОповещений</Name></Properties>
    </CommonCommand>
</MetaDataObject>"#,
            )
            .unwrap();
        },
        |db, ctx| {
            assert_eq!(
                ctx.module_metadata().module_type,
                bsl_metadata::ModuleType::CommandModule,
                "test must load real CommandModule metadata"
            );
            run(db, ctx)
        },
    )
}

#[test]
fn command_module_this_object_is_a_callback_receiver_for_both_rules() {
    const SOURCE: &str = r#"&НаКлиенте
Процедура Зарегистрировать()
    О = Новый ОписаниеОповещения("ЕстьЭкспорт", ЭтотОбъект);
    О = Новый ОписаниеОповещения("НетОбработчика", ЭтотОбъект);
    О = Новый ОписаниеОповещения("НеЭкспорт", ЭтотОбъект);
КонецПроцедуры

&НаКлиенте
Процедура ЕстьЭкспорт() Экспорт
КонецПроцедуры

&НаКлиенте
Процедура НеЭкспорт()
КонецПроцедуры
"#;

    let diagnostics = run_in_command_module(SOURCE, |db, ctx| {
        let module = ModuleId::new(ctx.file_id);
        let summary = db.module_call_summary(module);
        assert_eq!(summary.notify_regs.len(), 3, "{:#?}", summary.notify_regs);
        assert!(
            summary
                .notify_regs
                .iter()
                .all(|registration| matches!(registration.target, NotifyTarget::ThisObject)),
            "command-module receivers must normalize semantically: {:#?}",
            summary.notify_regs
        );

        let mut diagnostics = missing_notify_handler::check(ctx);
        diagnostics.extend(super::unused_local_method::check(ctx));
        diagnostics
    });

    let missing = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.code == crate::DiagnosticCode::MissingNotifyHandler)
        .collect::<Vec<_>>();
    assert_eq!(missing.len(), 2, "only missing and non-exported handlers: {missing:#?}");
    assert!(missing.iter().any(|diagnostic| diagnostic.message.contains("НетОбработчика")));
    assert!(missing.iter().any(|diagnostic| diagnostic.message.contains("НеЭкспорт")));
    let unused = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.code == crate::DiagnosticCode::UnusedLocalMethod)
        .collect::<Vec<_>>();
    assert!(
        unused.iter().all(|diagnostic| {
            !diagnostic.message.contains("ЕстьЭкспорт") && !diagnostic.message.contains("НеЭкспорт")
        }),
        "a normalized ThisObject registration protects local handlers: {unused:#?}"
    );
}

#[test]
fn command_callback_greek_sigma_protects_parameter_and_local_method() {
    for export in ["", " Экспорт"] {
        let source = format!(
            r#"&НаКлиенте
Процедура Зарегистрировать()
    О = Новый CallbackDescription("ΟΣ", ЭтотОбъект);
КонецПроцедуры

&НаКлиенте
Процедура οσ(ФиксированныйПараметр){export}
    Сообщить("обработчик");
КонецПроцедуры

&НаКлиенте
Процедура Обычный(ОбычныйПараметр)
    Сообщить("обычный");
КонецПроцедуры
"#
        );
        let diagnostics = run_in_command_module(&source, |db, ctx| {
            let summary = db.module_call_summary(ModuleId::new(ctx.file_id));
            assert_eq!(summary.notify_regs.len(), 1, "{:#?}", summary.notify_regs);
            assert_eq!(summary.notify_regs[0].callback_name.as_str(), "ΟΣ");
            assert!(matches!(summary.notify_regs[0].target, NotifyTarget::ThisObject));
            let mut diagnostics = super::unused_parameters::check(ctx);
            diagnostics.extend(super::unused_local_method::check(ctx));
            diagnostics
        });
        assert!(
            diagnostics.iter().all(|d| !d.message.contains("ФиксированныйПараметр")),
            "{diagnostics:#?}"
        );
        assert!(diagnostics.iter().all(|d| !d.message.contains("οσ")), "{diagnostics:#?}");
        assert!(
            diagnostics.iter().any(|d| d.message.contains("ОбычныйПараметр")),
            "{diagnostics:#?}"
        );
        assert!(
            diagnostics.iter().any(|d| d.message.contains("Обычный")
                && d.code == crate::DiagnosticCode::UnusedLocalMethod),
            "{diagnostics:#?}"
        );
    }
}

#[test]
fn invalid_constructor_arity_keeps_only_the_syntax_signature_protection() {
    const SOURCE: &str = r#"&НаКлиенте
Процедура Зарегистрировать()
    А = Новый ОписаниеОповещения("Защищённый", ЭтотОбъект, Неопределено, Неопределено, Неопределено, Неопределено);
    Б = Новый ОписаниеОповещения("Живой", ЭтотОбъект);
КонецПроцедуры

&НаКлиенте
Процедура Защищённый(ПараметрЗащищённого) Экспорт
    Сообщить("защищённый");
КонецПроцедуры

&НаКлиенте
Процедура Живой(ПараметрЖивого) Экспорт
    Сообщить("живой");
КонецПроцедуры

&НаКлиенте
Процедура Обычный(ОбычныйПараметр)
    Сообщить("обычный");
КонецПроцедуры
"#;
    let diagnostics = run_in_command_module(SOURCE, |db, ctx| {
        let summary = db.module_call_summary(ModuleId::new(ctx.file_id));
        assert_eq!(summary.notify_regs.len(), 1, "{:#?}", summary.notify_regs);
        assert_eq!(summary.notify_regs[0].callback_name.as_str(), "Живой");
        assert!(summary.notify_signature_handlers.iter().any(|name| name.as_str() == "Защищённый"));
        super::unused_parameters::check(ctx)
    });
    let unused: Vec<_> = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.code == crate::DiagnosticCode::UnusedParameters)
        .collect();
    assert_eq!(unused.len(), 1, "{diagnostics:#?}");
    assert!(unused[0].message.contains("ОбычныйПараметр"), "{diagnostics:#?}");
}

#[test]
fn command_module_this_object_shadowed_by_parameter_or_local_is_not_a_receiver() {
    const SOURCE: &str = r#"&НаКлиенте
Процедура Параметр(ЭтотОбъект)
    О = Новый ОписаниеОповещения("ПараметрОбработчик", ЭтотОбъект);
КонецПроцедуры

&НаКлиенте
Процедура ЛокальнаяПеременная()
    ЭтотОбъект = Новый Структура;
    О = Новый ОписаниеОповещения("ЛокальныйОбработчик", ЭтотОбъект);
КонецПроцедуры

&НаКлиенте
Процедура ПараметрОбработчик()
КонецПроцедуры

&НаКлиенте
Процедура ЛокальныйОбработчик()
КонецПроцедуры
"#;

    let diagnostics = run_in_command_module(SOURCE, |db, ctx| {
        let summary = db.module_call_summary(ModuleId::new(ctx.file_id));
        assert_eq!(summary.notify_regs.len(), 2, "{:#?}", summary.notify_regs);
        assert!(
            summary
                .notify_regs
                .iter()
                .all(|registration| !matches!(registration.target, NotifyTarget::ThisObject)),
            "shadowed identifiers must not normalize as self: {:#?}",
            summary.notify_regs
        );
        let mut diagnostics = missing_notify_handler::check(ctx);
        diagnostics.extend(super::unused_local_method::check(ctx));
        diagnostics
    });
    assert!(
        diagnostics
            .iter()
            .all(|diagnostic| diagnostic.code != crate::DiagnosticCode::MissingNotifyHandler),
        "unknown receivers are not judged as command-local handlers: {diagnostics:#?}"
    );
    // A parameter receiver of unknown runtime value is uncertainty: it may hold
    // `ЭтотОбъект`, so the same-named method stays a possible callback and is not
    // reported unused.
    assert!(
        diagnostics
            .iter()
            .all(|diagnostic| !(diagnostic.code == crate::DiagnosticCode::UnusedLocalMethod
                && diagnostic.message.contains("ПараметрОбработчик"))),
        "the parameter-shadowed registration must keep its same-named method protected: {diagnostics:#?}"
    );
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == crate::DiagnosticCode::UnusedLocalMethod
                && diagnostic.message.contains("ЛокальныйОбработчик")),
        "the known-structure local registration must not protect the same-named method: {diagnostics:#?}"
    );
}

#[test]
fn unavailable_command_module_registration_does_not_judge_or_protect_handler() {
    const SOURCE: &str = r#"&НаКлиенте
Процедура Зарегистрировать()
#Если Сервер И Не Сервер Тогда
    О = Новый ОписаниеОповещения("Недоступный", ЭтотОбъект);
#КонецЕсли
КонецПроцедуры

&НаКлиенте
Процедура Недоступный()
КонецПроцедуры
"#;

    let diagnostics = run_in_command_module(SOURCE, |db, ctx| {
        let summary = db.module_call_summary(ModuleId::new(ctx.file_id));
        assert_eq!(summary.notify_regs.len(), 1, "{:#?}", summary.notify_regs);
        assert!(
            matches!(summary.notify_regs[0].target, NotifyTarget::NotCompiled { .. }),
            "registration in an impossible environment must remain unavailable: {:#?}",
            summary.notify_regs
        );
        let mut diagnostics = missing_notify_handler::check(ctx);
        diagnostics.extend(super::unused_local_method::check(ctx));
        diagnostics
    });
    assert!(
        diagnostics
            .iter()
            .all(|diagnostic| diagnostic.code != crate::DiagnosticCode::MissingNotifyHandler),
        "an unavailable registration must not be checked: {diagnostics:#?}"
    );
    assert!(
        diagnostics.iter().any(|diagnostic| {
            diagnostic.code == crate::DiagnosticCode::UnusedLocalMethod
                && diagnostic.message.contains("Недоступный")
        }),
        "an unavailable registration must not protect its method: {diagnostics:#?}"
    );
}

#[test]
fn constructor_after_return_is_not_compiled() {
    const SOURCE: &str = r#"&НаКлиенте
Процедура Зарегистрировать()
    Возврат;
    О = Новый ОписаниеОповещения("Недостижимый", ЭтотОбъект);
КонецПроцедуры

&НаКлиенте
Процедура Недостижимый()
КонецПроцедуры
"#;

    run_in_command_module(SOURCE, |db, ctx| {
        let summary = db.module_call_summary(ModuleId::new(ctx.file_id));
        assert_eq!(summary.notify_regs.len(), 1, "{:#?}", summary.notify_regs);
        assert!(
            matches!(summary.notify_regs[0].target, NotifyTarget::NotCompiled { .. }),
            "a constructor after Возврат cannot be compiled: {:#?}",
            summary.notify_regs
        );
        Vec::new()
    });
}

#[test]
fn constructor_in_if_condition_is_reachable() {
    const SOURCE: &str = r#"&НаКлиенте
Процедура Зарегистрировать()
    Если Новый ОписаниеОповещения("Готово", ЭтотОбъект) Тогда
    КонецЕсли;
КонецПроцедуры

&НаКлиенте
Процедура Готово() Экспорт
КонецПроцедуры
"#;

    run_in_command_module(SOURCE, |db, ctx| {
        let summary = db.module_call_summary(ModuleId::new(ctx.file_id));
        assert_eq!(summary.notify_regs.len(), 1, "{:#?}", summary.notify_regs);
        assert!(
            matches!(summary.notify_regs[0].target, NotifyTarget::ThisObject),
            "the CFG conditional vertex represents a reachable statement: {:#?}",
            summary.notify_regs
        );
        Vec::new()
    });
}

#[test]
fn common_module_handler_selection_matches_the_graph() {
    const CALLER: &str = "CommonForms/Форма1/Ext/Form/Module.bsl";
    const SOURCE: &str = "&НаКлиенте\nПроцедура Тест()\n    О = Новый ОписаниеОповещения(\"Готово\", Модуль);\nКонецПроцедуры\n";

    for (base_export, extension_export, expected) in [(false, true, false), (true, false, true)] {
        let method = |export| {
            format!("Процедура Готово(){}\nКонецПроцедуры\n", if export { " Экспорт" } else { "" })
        };
        let mut builder = CfeFixtureBuilder::new("");
        builder
            .add_base_module("Модуль", &method(base_export))
            .add_extension("Расш", "")
            .add_extension_module("Расш", "Модуль", &method(extension_export));
        let diagnostics = crate::test_utils::check_cfe_at_with_db(
            CALLER,
            SOURCE,
            builder.build(),
            &[],
            DiagnosticsConfig::all_enabled(),
            |fixture| {
                for (_, root) in fixture.config_paths() {
                    let form = root.join("CommonForms/Форма1/Ext/Form.xml");
                    std::fs::create_dir_all(form.parent().unwrap()).unwrap();
                    std::fs::write(
                        form,
                        r#"<Form xmlns="http://v8.1c.ru/8.3/xcf/logform" version="2.20"><Events/></Form>"#,
                    )
                    .unwrap();
                    let path = root.join("CommonModules/Модуль.xml");
                    let xml = std::fs::read_to_string(&path).unwrap().replace(
                        "<ClientManagedApplication>false</ClientManagedApplication>",
                        "<ClientManagedApplication>true</ClientManagedApplication>",
                    );
                    std::fs::write(path, xml).unwrap();
                }
            },
            |db, ctx| {
                let summary = db.resolved_module_summary(ModuleId::new(ctx.file_id));
                let edges: Vec<_> =
                    summary.edges.iter().filter(|edge| edge.kind == EdgeKind::NotifyRef).collect();
                assert_eq!(edges.len(), 1, "{edges:#?}");
                if expected {
                    assert_eq!(edges[0].provenance, EdgeProvenance::StringResolved);
                    assert!(matches!(edges[0].target, ResolvedTarget::Method(_)));
                } else {
                    assert_eq!(edges[0].provenance, EdgeProvenance::VisibilityBlocked);
                    assert!(matches!(edges[0].target, ResolvedTarget::Unresolved(_)));
                }
                missing_notify_handler::check(ctx)
            },
        );
        if expected {
            assert!(diagnostics.is_empty(), "{diagnostics:#?}");
        } else {
            assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
            assert!(diagnostics[0].message.contains("Добавьте \"Экспорт\""));
        }
    }
}

#[test]
fn adopted_cav_module_callback_uses_base_and_extension_declarations() {
    const CALLER: &str = "CommonModules/ОбщийМодуль/Ext/Module.bsl";
    const BASE: &str = "&НаКлиенте\nПроцедура Проверить()\nКонецПроцедуры\n\
        &НаКлиенте\nПроцедура Callback() Экспорт\nКонецПроцедуры\n";
    let extension_source = |handler: &str| {
        format!(
            "&ИзменениеИКонтроль(\"Проверить\")\nПроцедура Расш1_Проверить()\n\
             #Вставка\n    Значение = 1;\n#КонецВставки\n\
             О = Новый ОписаниеОповещения(\"{handler}\", ОбщийМодуль);\nКонецПроцедуры\n"
        )
    };

    for (handler, expected) in [("Callback", 0), ("Callbak", 1)] {
        let mut builder = CfeFixtureBuilder::new("");
        builder
            .add_base_module("ОбщийМодуль", BASE)
            .add_extension("Расш", "")
            .add_extension_module("Расш", "ОбщийМодуль", &extension_source(handler));
        let fixture = builder.build();
        let source = extension_source(handler);
        let diagnostics = crate::test_utils::check_cfe_at_with_db(
            CALLER,
            &source,
            fixture,
            &[],
            DiagnosticsConfig::all_enabled(),
            |fixture| {
                for (_, root) in fixture.config_paths() {
                    let path = root.join("CommonModules/ОбщийМодуль.xml");
                    let xml = std::fs::read_to_string(&path).unwrap();
                    let xml = xml.replace(
                        "<ClientManagedApplication>false</ClientManagedApplication>",
                        "<ClientManagedApplication>true</ClientManagedApplication>",
                    );
                    assert_ne!(xml, std::fs::read_to_string(&path).unwrap());
                    std::fs::write(path, xml).unwrap();
                }
            },
            |db, ctx| {
                assert!(ide_db::effective_target(db, ctx.file_id).is_some());
                assert!(ide_db::weaving_target(db, ctx.file_id).is_some());
                let candidates = ctx.notify_candidates();
                let candidate = candidates
                    .iter()
                    .find(|candidate| candidate.callback_name.as_str() == handler)
                    .unwrap_or_else(|| {
                        panic!(
                            "CAV insertion must preserve its notify candidate; candidates={candidates:#?}; summary={:#?}",
                            db.module_call_summary(ModuleId::new(ctx.file_id))
                        )
                    });
                assert_eq!(candidate.receiver, hir::NotifyCandidateReceiver::ThisObject);
                assert!(matches!(
                    candidate.registration.as_ref().map(|reg| &reg.target),
                    Some(hir::call_graph::NotifyTarget::Module(name))
                        if name.as_str() == "ОбщийМодуль"
                ));
                crate::file_diagnostics(db, ctx.file_id, ctx.config)
            },
        );
        let missing = diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.code == crate::DiagnosticCode::MissingNotifyHandler)
            .collect::<Vec<_>>();
        assert_eq!(missing.len(), expected, "handler={handler}: {diagnostics:#?}");
        if expected == 1 {
            assert!(missing[0].message.contains("Callbak"), "{missing:#?}");
        }
    }
}

/// The main and the error-handler slot naming the same procedure on the same receiver
/// describe one platform invocation: one registration, one verdict. Different names or
/// different receivers stay separate registrations — those are the dedup controls.
#[test]
fn identical_main_and_error_handler_slots_register_once() {
    const CALLER: &str = "CommonCommands/ПроверкаОповещений/Ext/CommandModule.bsl";
    let registration_line = |slots: &str| {
        format!(
            r#"&НаКлиенте
Процедура Зарегистрировать()
    О = Новый ОписаниеОповещения({slots});
КонецПроцедуры

&НаКлиенте
Процедура Готово(ЛишнийПараметр) Экспорт
    Сообщить("обработчик");
КонецПроцедуры

&НаКлиенте
Процедура Обычный(ОбычныйПараметр)
    Сообщить("обычный");
КонецПроцедуры
"#
        )
    };
    let cases = [
        // Identical slots, missing handler: one registration, one diagnostic.
        ("Провал", "ЭтотОбъект", "Провал", "ЭтотОбъект", (1, 1, &["Провал"][..])),
        // Case-insensitive spelling of the same handler stays one invocation target.
        ("Провал", "ЭтотОбъект", "провал", "ЭтотОбъект", (1, 1, &["Провал"][..])),
        // Control: distinct handler names are two registrations and two verdicts.
        ("Провал", "ЭтотОбъект", "ИнойПровал", "ЭтотОбъект", (2, 2, &["Провал", "ИнойПровал"][..])),
        // Control: same name on a different receiver is not deduplicated; the unknown
        // receiver is not judged, so only the ThisObject slot gets a verdict.
        ("Провал", "ЭтотОбъект", "Провал", "ДругойМодуль", (2, 1, &["Провал"][..])),
        // Control: two unresolvable receivers are both Unsupported — equally unknown, not
        // proven identical, so both registrations stay and neither slot is judged.
        ("Провал", "Форма1", "Провал", "Форма2", (2, 0, &[][..])),
    ];
    for (main, main_receiver, error, error_receiver, (regs, verdicts, names)) in cases {
        let slots = format!("\"{main}\", {main_receiver}, , \"{error}\", {error_receiver}");
        let source = registration_line(&slots);
        let fixture = CfeFixtureBuilder::new("").build();
        let diagnostics = crate::test_utils::check_cfe_at_with_db(
            CALLER,
            &source,
            fixture,
            &[],
            DiagnosticsConfig::all_enabled(),
            |_| {},
            |db, ctx| {
                let summary = db.module_call_summary(ModuleId::new(ctx.file_id));
                assert_eq!(summary.notify_regs.len(), regs, "{slots}: {:#?}", summary.notify_regs);
                if main.eq_ignore_ascii_case(error) && main_receiver == error_receiver {
                    let candidates = ctx.notify_candidates();
                    let same_constructor = candidates
                        .iter()
                        .filter(|candidate| {
                            candidate.callback_name.eq_ignore_case(&hir::Name::new(main))
                        })
                        .collect::<Vec<_>>();
                    assert_eq!(same_constructor.len(), 2, "slot provenance: {same_constructor:#?}");
                    assert_eq!(same_constructor[0].slot, hir::NotifyCallbackSlot::Handler);
                    assert_eq!(same_constructor[1].slot, hir::NotifyCallbackSlot::ErrorHandler);
                }
                missing_notify_handler::check(ctx)
            },
        );
        let missing: Vec<_> = diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.code == crate::DiagnosticCode::MissingNotifyHandler)
            .collect();
        assert_eq!(missing.len(), verdicts, "{slots}: {diagnostics:#?}");
        for name in names {
            assert!(
                missing.iter().any(|diagnostic| diagnostic.message.contains(name)),
                "{slots}: {diagnostics:#?}"
            );
        }
    }
}

/// Identical slots inside an unreachable branch produce two NotCompiled registrations.
/// They cannot be told apart by callback_expr (each slot carries its own literal), and the
/// unavailability proves nothing about the receiver — so neither registration is dropped.
#[test]
fn not_compiled_identical_slots_keep_both_registrations() {
    const CALLER: &str = "CommonCommands/ПроверкаОповещений/Ext/CommandModule.bsl";
    const SOURCE: &str = r#"&НаКлиенте
Процедура Зарегистрировать()
#Если Сервер И Не Сервер Тогда
    О = Новый ОписаниеОповещения("Провал", ЭтотОбъект, , "Провал", ЭтотОбъект);
#КонецЕсли
КонецПроцедуры

&НаКлиенте
Процедура Обычный(ОбычныйПараметр)
    Сообщить("обычный");
КонецПроцедуры
"#;
    let fixture = CfeFixtureBuilder::new("").build();
    let diagnostics = crate::test_utils::check_cfe_at_with_db(
        CALLER,
        SOURCE,
        fixture,
        &[],
        DiagnosticsConfig::all_enabled(),
        |_| {},
        |db, ctx| {
            let summary = db.module_call_summary(ModuleId::new(ctx.file_id));
            assert_eq!(summary.notify_regs.len(), 2, "{:#?}", summary.notify_regs);
            assert!(
                summary.notify_regs.iter().all(|registration| matches!(
                    &registration.target,
                    hir::call_graph::NotifyTarget::NotCompiled {
                        receiver_syntax: NotifyReceiverSyntax::SelfName,
                        ..
                    }
                )),
                "{:#?}",
                summary.notify_regs
            );
            missing_notify_handler::check(ctx)
        },
    );
    assert!(
        diagnostics
            .iter()
            .all(|diagnostic| diagnostic.code != crate::DiagnosticCode::MissingNotifyHandler),
        "an unavailable registration is not judged: {diagnostics:#?}"
    );
}

/// The dynamic-array path dedups the same way: identical slots on a proven receiver
/// collapse to one registration, unproven receivers keep both.
#[test]
fn identical_dynamic_array_slots_follow_the_same_dedup_rule() {
    const CALLER: &str = "CommonCommands/ПроверкаОповещений/Ext/CommandModule.bsl";
    let source = |second_receiver: &str| {
        format!(
            r#"&НаКлиенте
Процедура Зарегистрировать()
    Перем Параметры;
    Параметры = Новый Массив;
    Параметры.Добавить("Провал");
    Параметры.Добавить(ЭтотОбъект);
    Параметры.Добавить(Неопределено);
    Параметры.Добавить("Провал");
    Параметры.Добавить({second_receiver});
    О = Новый("CallbackDescription", Параметры);
КонецПроцедуры

&НаКлиенте
Процедура Обычный(ОбычныйПараметр)
    Сообщить("обычный");
КонецПроцедуры
"#
        )
    };
    // The second case keeps the unsupported error slot as its own registration while the
    // proven ThisObject main slot still gets its verdict.
    for (second_receiver, regs, verdicts) in [("ЭтотОбъект", 1, 1), ("Форма2", 2, 1)]
    {
        let source = source(second_receiver);
        let fixture = CfeFixtureBuilder::new("").build();
        let diagnostics = crate::test_utils::check_cfe_at_with_db(
            CALLER,
            &source,
            fixture,
            &[],
            DiagnosticsConfig::all_enabled(),
            |_| {},
            |db, ctx| {
                let summary = db.module_call_summary(ModuleId::new(ctx.file_id));
                assert_eq!(
                    summary.notify_regs.len(),
                    regs,
                    "receiver {second_receiver}: {:#?}",
                    summary.notify_regs
                );
                missing_notify_handler::check(ctx)
            },
        );
        let missing: Vec<_> = diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.code == crate::DiagnosticCode::MissingNotifyHandler)
            .collect();
        assert_eq!(missing.len(), verdicts, "receiver {second_receiver}: {diagnostics:#?}");
    }
}

#[test]
fn identical_main_and_error_handler_slots_still_protect_the_handler() {
    const CALLER: &str = "CommonCommands/ПроверкаОповещений/Ext/CommandModule.bsl";
    const SOURCE: &str = r#"&НаКлиенте
Процедура Зарегистрировать()
    О = Новый ОписаниеОповещения("Готово", ЭтотОбъект, , "Готово", ЭтотОбъект);
КонецПроцедуры

&НаКлиенте
Процедура Готово(ЛишнийПараметр) Экспорт
    Сообщить("обработчик");
КонецПроцедуры

&НаКлиенте
Процедура Обычный(ОбычныйПараметр)
    Сообщить("обычный");
КонецПроцедуры
"#;
    let fixture = CfeFixtureBuilder::new("").build();
    let diagnostics = crate::test_utils::check_cfe_at_with_db(
        CALLER,
        SOURCE,
        fixture,
        &[],
        DiagnosticsConfig::all_enabled(),
        |_| {},
        |db, ctx| {
            let module = ModuleId::new(ctx.file_id);
            let summary = db.module_call_summary(module);
            assert_eq!(summary.notify_regs.len(), 1, "{:#?}", summary.notify_regs);
            assert!(missing_notify_handler::check(ctx).is_empty());
            let mut diagnostics = super::unused_parameters::check(ctx);
            diagnostics.extend(super::unused_local_method::check(ctx));
            diagnostics
        },
    );
    assert!(
        diagnostics.iter().all(|diagnostic| !diagnostic.message.contains("ЛишнийПараметр")),
        "the deduplicated registration still protects the handler signature: {diagnostics:#?}"
    );
    assert!(
        diagnostics.iter().any(|diagnostic| diagnostic.message.contains("ОбычныйПараметр")),
        "an unregistered method stays unprotected: {diagnostics:#?}"
    );
}

fn write_client_common_module_metadata(root: &std::path::Path, name: &str) {
    let path = root.join(format!("CommonModules/{name}.xml"));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        path,
        format!(
            r#"<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <CommonModule uuid="00000000-0000-0000-0000-000000000001">
        <Properties>
            <Name>{name}</Name>
            <Global>false</Global>
            <ClientManagedApplication>true</ClientManagedApplication>
            <ClientOrdinaryApplication>false</ClientOrdinaryApplication>
            <Server>true</Server>
            <ExternalConnection>false</ExternalConnection>
            <ServerCall>false</ServerCall>
            <Privileged>false</Privileged>
            <ReturnValuesReuse>DontUse</ReturnValuesReuse>
        </Properties>
    </CommonModule>
</MetaDataObject>"#
        ),
    )
    .unwrap();
}

fn enable_client_common_module(fixture: &test_fixture::CfeFixture, name: &str) {
    let mut found = false;
    for (_, root) in fixture.config_paths() {
        let path = root.join(format!("CommonModules/{name}.xml"));
        if !path.exists() {
            continue;
        }
        let xml = std::fs::read_to_string(&path).unwrap();
        let updated = xml.replace(
            "<ClientManagedApplication>false</ClientManagedApplication>",
            "<ClientManagedApplication>true</ClientManagedApplication>",
        );
        assert_ne!(updated, xml, "{name} fixture must start client-disabled");
        std::fs::write(path, updated).unwrap();
        found = true;
    }
    assert!(found, "no CFE metadata for common module {name}");
}

#[test]
fn this_form_receiver_can_resolve_to_a_client_common_module() {
    const CALLER: &str = "CommonModules/Caller/Ext/Module.bsl";
    const SOURCE: &str = "&НаКлиенте\nПроцедура Тест()\n    О = Новый ОписаниеОповещения(\"Готово\", ThisForm);\nКонецПроцедуры\n\nПроцедура Готово(НеИспользуется)\n    Сообщить(\"локальный метод\");\nКонецПроцедуры\n";
    let mut builder = CfeFixtureBuilder::new("");
    builder.add_base_module("ThisForm", "Процедура Готово() Экспорт\nКонецПроцедуры\n");
    let fixture = builder.build();

    let diagnostics = crate::test_utils::check_cfe_at_with_db(
        CALLER,
        SOURCE,
        fixture,
        &[],
        DiagnosticsConfig::all_enabled(),
        |fixture| {
            enable_client_common_module(fixture, "ThisForm");
            write_client_common_module_metadata(fixture.root(), "Caller");
        },
        |db, ctx| {
            let module = ModuleId::new(ctx.file_id);
            let summary = db.module_call_summary(module);
            assert!(matches!(
                &summary.notify_regs[0].target,
                NotifyTarget::Module(name) if name.as_str() == "ThisForm"
            ));
            let graph = db.resolved_module_summary(module);
            let edges: Vec<_> =
                graph.edges.iter().filter(|edge| edge.kind == EdgeKind::NotifyRef).collect();
            assert_eq!(edges.len(), 1, "{edges:#?}");
            let ResolvedTarget::Method(method) = &edges[0].target else {
                panic!("expected the callback edge to resolve: {edges:#?}");
            };
            assert_ne!(method.module.file_id, ctx.file_id, "edge must target ThisForm");

            let mut diagnostics = missing_notify_handler::check(ctx);
            diagnostics.extend(super::unused_parameters::check(ctx));
            diagnostics
        },
    );

    assert!(
        diagnostics
            .iter()
            .all(|diagnostic| diagnostic.code != crate::DiagnosticCode::MissingNotifyHandler),
        "external ThisForm handler should satisfy the registration: {diagnostics:#?}"
    );
    assert!(
        diagnostics
            .iter()
            .all(|diagnostic| diagnostic.code != crate::DiagnosticCode::UnusedParameters),
        "a self-form callback reference keeps the same-named local signature protected even when ThisForm resolves externally: {diagnostics:#?}"
    );
}

#[test]
fn notify_registrations_use_live_branches_and_type_availability() {
    const CALLER: &str = "CommonModules/Caller/Ext/Module.bsl";
    const SOURCE: &str = r#"
&НаКлиентеНаСервере
Процедура Тест()
    О = Новый ОписаниеОповещения("Mixed", Handlers);
#Если Сервер Тогда
    О = Новый ОписаниеОповещения("ServerBranch", Handlers);
#КонецЕсли
#Если Сервер И Не ТолстыйКлиентУправляемоеПриложение Тогда
    О = Новый ОписаниеОповещения("ServerOnlyTypeBranch", Handlers);
#КонецЕсли
#Если Клиент Тогда
    О = Новый ОписаниеОповещения("ClientBranch", Handlers);
#КонецЕсли
#Если Сервер И Не Сервер Тогда
    Handlers = Неопределено;
    О = Новый ОписаниеОповещения("DeadBranch", Handlers);
    О = Новый ОписаниеОповещения("DeadNonBareBranch", ПолучитьОбъект());
#КонецЕсли
#Если Сервер И Не Сервер Тогда
    Handlers = Неопределено;
#КонецЕсли
    О = Новый ОписаниеОповещения("AfterDeadAssignment", Handlers);
#Если Клиент Тогда
    LocalHandlers = Неопределено;
#КонецЕсли
    О = Новый ОписаниеОповещения("AfterLiveAssignment", LocalHandlers);
КонецПроцедуры

&НаСервере
Процедура Сервер()
    О = Новый ОписаниеОповещения("ServerOnly", Handlers);
КонецПроцедуры

&НаСервереБезКонтекста
Процедура СерверБезКонтекста()
    О = Новый ОписаниеОповещения("Contextless", Handlers);
КонецПроцедуры
"#;
    let handlers = [
        "Mixed",
        "ServerBranch",
        "ServerOnlyTypeBranch",
        "ClientBranch",
        "DeadBranch",
        "DeadNonBareBranch",
        "AfterDeadAssignment",
        "AfterLiveAssignment",
        "ServerOnly",
        "Contextless",
    ]
    .into_iter()
    .map(|name| format!("Процедура {name}() Экспорт\nКонецПроцедуры\n"))
    .collect::<String>();
    let mut builder = CfeFixtureBuilder::new("");
    builder.add_base_module("Handlers", &handlers);
    let fixture = builder.build();

    let diagnostics = crate::test_utils::check_cfe_at_with_db(
        CALLER,
        SOURCE,
        fixture,
        &[],
        DiagnosticsConfig::all_enabled(),
        |fixture| {
            enable_client_common_module(fixture, "Handlers");
            write_client_common_module_metadata(fixture.root(), "Caller");
        },
        |db, ctx| {
            let metadata = ctx.module_metadata();
            assert_eq!(metadata.module_type, bsl_metadata::ModuleType::CommonModule);
            let common_module =
                metadata.common_module.as_ref().expect("caller common module metadata");
            assert!(common_module.is_client_managed_application());
            assert!(common_module.is_server());
            let module = ModuleId::new(ctx.file_id);
            let summary = db.module_call_summary(module);
            let caller = summary.notify_regs[0].caller;
            let CallerId::Method(local_id) = caller else { panic!("expected method caller") };
            let inferred = hir::infer_owner(db, ctx.file_id, hir::DefWithBodyId::Method(local_id));
            assert!(
                !inferred.dead_stmts().is_empty(),
                "dead branch was not recorded: {:?}",
                inferred.dead_stmts()
            );
            let target_for = |name: &str| {
                summary
                    .notify_regs
                    .iter()
                    .find(|reg| reg.callback_name.as_str() == name)
                    .map(|reg| &reg.target)
                    .unwrap_or_else(|| {
                        panic!("missing registration for {name}: {:#?}", summary.notify_regs)
                    })
            };
            for name in ["Mixed", "ServerBranch", "ClientBranch", "AfterDeadAssignment"] {
                assert!(
                    matches!(target_for(name), NotifyTarget::Module(module) if module.as_str() == "Handlers"),
                    "{name}: {:#?}",
                    target_for(name)
                );
            }
            for name in [
                "DeadBranch",
                "DeadNonBareBranch",
                "ServerOnly",
                "Contextless",
                "ServerOnlyTypeBranch",
            ] {
                assert!(
                    matches!(target_for(name), NotifyTarget::NotCompiled { .. }),
                    "{name}: {:#?}",
                    target_for(name)
                );
            }
            // The receiver is a live local assigned `Неопределено`: a known data value,
            // so the target is proven foreign rather than uncertain.
            assert_eq!(target_for("AfterLiveAssignment"), &NotifyTarget::Unsupported);

            let graph = db.resolved_module_summary(module);
            let edges: Vec<_> =
                graph.edges.iter().filter(|edge| edge.kind == EdgeKind::NotifyRef).collect();
            assert_eq!(edges.len(), 4, "{edges:#?}");
            for edge in edges {
                let ResolvedTarget::Method(method) = &edge.target else {
                    panic!("valid client registration must resolve: {edge:#?}");
                };
                assert_ne!(method.module.file_id, ctx.file_id, "{edge:#?}");
            }
            assert!(missing_notify_handler::check(ctx).is_empty());
            Vec::new()
        },
    );
    assert!(diagnostics.is_empty());
}

#[test]
fn notify_receiver_assignments_shadow_only_overlapping_environments() {
    const CALLER: &str = "CommonModules/Caller/Ext/Module.bsl";
    let cases = [
        (
            "no assignment",
            r#"#Если ТонкийКлиент Тогда
    О = Новый ОписаниеОповещения("Один", Handlers);
#КонецЕсли"#,
            1,
        ),
        (
            "server assignment and thin-client registration",
            r#"#Если Сервер И Не ТолстыйКлиентУправляемоеПриложение Тогда
    Handlers = Неопределено;
#КонецЕсли
#Если ТонкийКлиент Тогда
    О = Новый ОписаниеОповещения("Один", Handlers);
#КонецЕсли"#,
            1,
        ),
        (
            "overlapping assignment and registration",
            r#"#Если ТонкийКлиент Тогда
    Handlers = Неопределено;
    О = Новый ОписаниеОповещения("Один", Handlers);
#КонецЕсли"#,
            0,
        ),
        (
            "assignment in then branch does not shadow else registration",
            r#"#Если ТонкийКлиент Тогда
    Handlers = Неопределено;
    О = Новый ОписаниеОповещения("ВТонком", Handlers);
#Иначе
    О = Новый ОписаниеОповещения("ВДругом", Handlers);
#КонецЕсли"#,
            1,
        ),
        (
            "assignment in else branch does not shadow then registration",
            r#"#Если ТонкийКлиент Тогда
    О = Новый ОписаниеОповещения("ВТонком", Handlers);
#Иначе
    Handlers = Неопределено;
    О = Новый ОписаниеОповещения("ВДругом", Handlers);
#КонецЕсли"#,
            1,
        ),
        (
            "dead assignment does not shadow registration",
            r#"#Если Сервер И Не Сервер Тогда
    Handlers = Неопределено;
#КонецЕсли
#Если ТонкийКлиент Тогда
    О = Новый ОписаниеОповещения("Один", Handlers);
#КонецЕсли"#,
            1,
        ),
        (
            "unknown assignment environment conservatively shadows",
            r#"#Если НеизвестнаяПлатформа Тогда
    Handlers = Неопределено;
#КонецЕсли
#Если ТонкийКлиент Тогда
    О = Новый ОписаниеОповещения("Один", Handlers);
#КонецЕсли"#,
            0,
        ),
        (
            "English directives and dynamic CallbackDescription array",
            r#"#If ThinClient Then
    Параметры = ПолучитьПараметры();
    О = Новый("CallbackDescription", Параметры);
#EndIf"#,
            0,
        ),
    ];

    for (case, body, expected_module_targets) in cases {
        let source = format!("&НаКлиентеНаСервере\nПроцедура Тест()\n{body}\nКонецПроцедуры\n");
        let mut builder = CfeFixtureBuilder::new("");
        builder.add_base_module("Handlers", "Процедура Другая() Экспорт\nКонецПроцедуры\n");
        let diagnostics = crate::test_utils::check_cfe_at_with_db(
            CALLER,
            &source,
            builder.build(),
            &[],
            DiagnosticsConfig::all_enabled(),
            |fixture| {
                enable_client_common_module(fixture, "Handlers");
                write_client_common_module_metadata(fixture.root(), "Caller");
            },
            |db, ctx| {
                let summary = db.module_call_summary(ModuleId::new(ctx.file_id));
                let module_targets: Vec<_> = summary
                    .notify_regs
                    .iter()
                    .filter_map(|reg| match &reg.target {
                        NotifyTarget::Module(module) => Some(module),
                        _ => None,
                    })
                    .collect();
                assert_eq!(
                    module_targets.len(),
                    expected_module_targets,
                    "{case}: {:#?}",
                    summary.notify_regs
                );
                assert!(module_targets.iter().all(|module| module.as_str() == "Handlers"));
                missing_notify_handler::check(ctx)
            },
        );
        let missing: Vec<_> = diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.code == crate::DiagnosticCode::MissingNotifyHandler)
            .collect();
        assert_eq!(missing.len(), expected_module_targets, "{case}: {diagnostics:#?}");
    }
}

#[test]
fn client_server_method_in_client_only_module_checks_notify_handler_in_client_environment() {
    const CALLER: &str = "CommonModules/Caller/Ext/Module.bsl";
    let cases = [
        (
            "client export is available",
            "&НаКлиенте\nПроцедура Готово() Экспорт\nКонецПроцедуры\n",
            0,
        ),
        (
            "client non-export remains invalid",
            "&НаКлиенте\nПроцедура Готово()\nКонецПроцедуры\n",
            1,
        ),
    ];

    for (case, handler, expected) in cases {
        let source = format!(
            "&НаКлиентеНаСервере\nПроцедура Тест()\n    О = Новый ОписаниеОповещения(\"Готово\", Caller);\nКонецПроцедуры\n{handler}"
        );
        let fixture = CfeFixtureBuilder::new("").build();
        let diagnostics = crate::test_utils::check_cfe_at_with_db(
            CALLER,
            &source,
            fixture,
            &[],
            DiagnosticsConfig::all_enabled(),
            |fixture| {
                write_client_common_module_metadata(fixture.root(), "Caller");
                let path = fixture.root().join("CommonModules/Caller.xml");
                let xml = std::fs::read_to_string(&path).unwrap();
                std::fs::write(
                    path,
                    xml.replace("<Server>true</Server>", "<Server>false</Server>"),
                )
                .unwrap();
            },
            |db, ctx| {
                let module = ModuleId::new(ctx.file_id);
                let registration = &db.module_call_summary(module).notify_regs[0];
                assert!(
                    matches!(&registration.target, NotifyTarget::Module(module) if module.as_str() == "Caller"),
                    "{case}: expected self common-module receiver, got {registration:#?}"
                );
                assert!(
                    !registration.environments.definite.is_empty(),
                    "{case}: expected a non-empty definite environment"
                );
                assert!(registration
                    .environments
                    .definite
                    .intersects(hir::execution_env::EnvFlags::MANAGED_CLIENTS));
                assert!(!registration
                    .environments
                    .definite
                    .intersects(hir::execution_env::EnvFlags::SERVER));
                let symbol_tree = ctx.symbol_tree();
                let methods = symbol_tree.find_methods(&hir::Name::new("Готово"));
                let availability = ctx.notify_handler_availability_in_registration(
                    registration,
                    &methods.iter().map(|method| method.id).collect::<Vec<_>>(),
                );
                assert_eq!(
                    availability,
                    if expected == 0 {
                        hir::NotifyHandlerAvailability::Exported
                    } else {
                        hir::NotifyHandlerAvailability::NotExported
                    },
                    "{case}: registration={registration:#?}, methods={methods:#?}"
                );
                missing_notify_handler::check(ctx)
            },
        );
        let missing: Vec<_> = diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.code == crate::DiagnosticCode::MissingNotifyHandler)
            .collect();
        assert_eq!(missing.len(), expected, "{case}: {diagnostics:#?}");
    }
}

#[test]
fn an_undecidable_registration_environment_remains_unknown() {
    const CALLER: &str = "CommonForms/Форма1/Ext/Form/Module.bsl";
    const SOURCE: &str = r#"
&НаКлиентеНаСервере
Процедура Тест()
#Если НеизвестныйСимвол Тогда
    О = Новый ОписаниеОповещения("Готово", ЭтотОбъект);
#КонецЕсли
КонецПроцедуры
"#;
    let fixture = CfeFixtureBuilder::new("").build();
    let diagnostics = crate::test_utils::check_cfe_at_with_db(
        CALLER,
        SOURCE,
        fixture,
        &[],
        DiagnosticsConfig::all_enabled(),
        |fixture| {
            let form = fixture.root().join("CommonForms/Форма1/Ext/Form.xml");
            std::fs::create_dir_all(form.parent().unwrap()).unwrap();
            std::fs::write(
                form,
                r#"<Form xmlns="http://v8.1c.ru/8.3/xcf/logform" version="2.20"><Events/></Form>"#,
            )
            .unwrap();
        },
        |db, ctx| {
            let summary = db.module_call_summary(ModuleId::new(ctx.file_id));
            assert_eq!(summary.notify_regs.len(), 1, "{:#?}", summary.notify_regs);
            let environments = summary.notify_regs[0].environments;
            assert!(environments.definite.is_empty(), "{environments:#?}");
            assert!(
                !environments.possible.is_empty(),
                "an undecidable registration must retain possible environments: {environments:#?}"
            );
            missing_notify_handler::check(ctx)
        },
    );
    assert!(
        diagnostics
            .iter()
            .all(|diagnostic| diagnostic.code != crate::DiagnosticCode::MissingNotifyHandler),
        "possible-only registration must not produce a definitive verdict: {diagnostics:#?}"
    );
}

#[test]
fn server_only_notify_constructors_are_not_diagnosed() {
    const CALLER: &str = "CommonForms/Форма1/Ext/Form/Module.bsl";
    const SOURCE: &str = r#"
&НаСервере
Процедура Тест()
    О = Новый ОписаниеОповещения("Статический", ЭтотОбъект);
    О = Новый NotifyDescription("LegacyEnglishAlias", ЭтотОбъект);
КонецПроцедуры
"#;
    let fixture = CfeFixtureBuilder::new("").build();
    crate::test_utils::check_cfe_at_with_db(
        CALLER,
        SOURCE,
        fixture,
        &[],
        DiagnosticsConfig::all_enabled(),
        |_| {},
        |db, ctx| {
            let module = ModuleId::new(ctx.file_id);
            let summary = db.module_call_summary(module);
            assert_eq!(summary.notify_regs.len(), 2, "{:#?}", summary.notify_regs);
            assert!(
                summary
                    .notify_regs
                    .iter()
                    .all(|reg| { matches!(reg.target, NotifyTarget::NotCompiled { .. }) }),
                "{:#?}",
                summary.notify_regs
            );
            assert!(db
                .resolved_module_summary(module)
                .edges
                .iter()
                .all(|edge| edge.kind != EdgeKind::NotifyRef));
            assert!(missing_notify_handler::check(ctx).is_empty());
            Vec::new()
        },
    );
}

#[test]
fn dynamic_notify_function_form_uses_only_proven_local_array_state() {
    const CALLER: &str = "CommonForms/Форма1/Ext/Form/Module.bsl";
    const SOURCE: &str = r#"
&НаКлиенте
Процедура Тест(Параметры)
    Перем ТочныеПараметры;
    О = Новый("CallbackDescription", Параметры);
    Параметры.Добавить("Callback");
    Параметры.Добавить(ЭтотОбъект);
    Параметры = ПолучитьПараметры();
    О = Новый("CallbackDescription", Параметры);
    ТочныеПараметры = Новый Массив;
    ТочныеПараметры.Добавить("Callback");
    ТочныеПараметры.Добавить(ЭтотОбъект);
    О = Новый("CallbackDescription", ТочныеПараметры);
КонецПроцедуры

&НаКлиенте
Процедура Callback() Экспорт
КонецПроцедуры
"#;
    crate::test_utils::check_cfe_at_with_db(
        CALLER,
        SOURCE,
        CfeFixtureBuilder::new("").build(),
        &[],
        DiagnosticsConfig::all_enabled(),
        |fixture| {
            let path = fixture.root().join("CommonForms/Форма1/Ext/Form.xml");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(
                path,
                r#"<Form xmlns="http://v8.1c.ru/8.3/xcf/logform" version="2.20"><Events/></Form>"#,
            )
            .unwrap();
        },
        |db, ctx| {
            let summary = db.module_call_summary(ModuleId::new(ctx.file_id));
            assert_eq!(summary.notify_regs.len(), 1, "{:#?}", summary.notify_regs);
            assert_eq!(summary.notify_regs[0].callback_name.as_str(), "Callback");
            assert!(matches!(summary.notify_regs[0].target, NotifyTarget::ThisObject));
            assert!(missing_notify_handler::check(ctx).is_empty());
            Vec::new()
        },
    );
}

#[test]
fn impossible_notify_regs_do_not_protect_signatures_but_other_builds_do() {
    const CALLER: &str = "CommonModules/Caller/Ext/Module.bsl";
    const SOURCE: &str = r#"
&НаКлиентеНаСервере
Процедура Тест() Экспорт
#Если Сервер И Не Сервер Тогда
    О = Новый ОписаниеОповещения("DeadHandler", ПолучитьОбъект());
#КонецЕсли
#Если Клиент Тогда
    О = Новый ОписаниеОповещения("LiveHandler", ПолучитьОбъект());
#КонецЕсли
#Если МобильныйКлиент Тогда
    О = Новый ОписаниеОповещения("mObIlEhAnDlEr", ЭтотОбъект);
    О = Новый ОписаниеОповещения("ForeignMobileHandler", ДругойМодуль);
    О = Новый ОписаниеОповещения("OmittedReceiver");
    О = Новый ОписаниеОповещения("PrimaryHandler", ЭтотОбъект,, "ForeignErrorHandler", ДругойМодуль);
#КонецЕсли
КонецПроцедуры

&НаКлиентеНаСервере
Процедура DeadHandler(МёртвыйПараметр)
    Сообщить("handler");
КонецПроцедуры

&НаКлиентеНаСервере
Процедура LiveHandler(ЖивойПараметр)
    Сообщить("handler");
КонецПроцедуры

&НаКлиентеНаСервере
Процедура MobileHandler(МобильныйПараметр)
    Сообщить("handler");
КонецПроцедуры

&НаКлиентеНаСервере
Процедура ForeignMobileHandler(ЧужойПараметр)
    Сообщить("handler");
КонецПроцедуры

&НаКлиентеНаСервере
Процедура OmittedReceiver(ПропущенныйПриёмник)
    Сообщить("handler");
КонецПроцедуры

&НаКлиентеНаСервере
Процедура PrimaryHandler(ОсновнойПараметр)
    Сообщить("handler");
КонецПроцедуры

&НаКлиентеНаСервере
Процедура ForeignErrorHandler(ЧужойПараметрОшибки)
    Сообщить("handler");
КонецПроцедуры

&НаКлиентеНаСервере
Процедура Unrelated(ОбычныйПараметр)
    Сообщить("unrelated");
КонецПроцедуры
"#;
    let mut builder = CfeFixtureBuilder::new("");
    builder.add_base_module("Caller", "");
    let fixture = builder.build();
    let diagnostics = crate::test_utils::check_cfe_at_with_db(
        CALLER,
        SOURCE,
        fixture,
        &[],
        DiagnosticsConfig::all_enabled(),
        |fixture| write_client_common_module_metadata(fixture.root(), "Caller"),
        |db, ctx| {
            let summary = db.module_call_summary(ModuleId::new(ctx.file_id));
            let target_for = |name: &str| {
                summary
                    .notify_regs
                    .iter()
                    .find(|reg| reg.callback_name.as_str() == name)
                    .map(|reg| &reg.target)
                    .unwrap_or_else(|| panic!("missing {name}: {:#?}", summary.notify_regs))
            };
            assert!(matches!(
                target_for("DeadHandler"),
                NotifyTarget::NotCompiled { receiver_syntax: NotifyReceiverSyntax::Other, .. }
            ));
            assert_eq!(target_for("LiveHandler"), &NotifyTarget::Unsupported);
            assert!(matches!(
                target_for("mObIlEhAnDlEr"),
                NotifyTarget::NotCompiled { receiver_syntax: NotifyReceiverSyntax::SelfName, .. }
            ));
            assert!(matches!(
                target_for("ForeignMobileHandler"),
                NotifyTarget::NotCompiled { receiver_syntax: NotifyReceiverSyntax::BareName, .. }
            ));
            assert!(matches!(
                target_for("OmittedReceiver"),
                NotifyTarget::NotCompiled { receiver_syntax: NotifyReceiverSyntax::Other, .. }
            ));
            assert!(matches!(
                target_for("PrimaryHandler"),
                NotifyTarget::NotCompiled { receiver_syntax: NotifyReceiverSyntax::SelfName, .. }
            ));
            assert!(matches!(
                target_for("ForeignErrorHandler"),
                NotifyTarget::NotCompiled { receiver_syntax: NotifyReceiverSyntax::BareName, .. }
            ));
            super::unused_parameters::check(ctx)
        },
    );
    let unused_parameters: Vec<_> = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.code == crate::DiagnosticCode::UnusedParameters)
        .collect();
    assert_eq!(unused_parameters.len(), 4, "{diagnostics:#?}");
    for name in ["МёртвыйПараметр", "ЧужойПараметр", "ЧужойПараметрОшибки", "ОбычныйПараметр"]
    {
        assert!(
            unused_parameters.iter().any(|d| d.message.contains(&format!("\"{name}\""))),
            "{diagnostics:#?}"
        );
    }
}

#[test]
fn unavailable_notify_name_preserves_form_string_reference() {
    const CALLER: &str = "CommonForms/Форма1/Ext/Form/Module.bsl";
    const SOURCE: &str = r#"
&НаКлиенте
Процедура Тест()
#Если МобильныйКлиент Тогда
    О = Новый ОписаниеОповещения("ForeignCallback", ДругойМодуль);
#КонецЕсли
КонецПроцедуры

&НаКлиенте
Процедура ForeignCallback(ПараметрОповещения)
    Сообщить("handler");
КонецПроцедуры

&НаКлиенте
Процедура Unrelated(ОбычныйПараметр)
    Сообщить("ordinary text");
КонецПроцедуры
"#;
    let diagnostics = crate::test_utils::check_cfe_at_with_db(
        CALLER,
        SOURCE,
        CfeFixtureBuilder::new("").build(),
        &[],
        DiagnosticsConfig::all_enabled(),
        |fixture| {
            let path = fixture.root().join("CommonForms/Форма1/Ext/Form.xml");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(
                path,
                r#"<Form xmlns="http://v8.1c.ru/8.3/xcf/logform" version="2.20"><Events/></Form>"#,
            )
            .unwrap();
        },
        |db, ctx| {
            assert_eq!(ctx.module_metadata().module_type, bsl_metadata::ModuleType::FormModule);
            let summary = db.module_call_summary(ModuleId::new(ctx.file_id));
            assert!(matches!(
                summary.notify_regs[0].target,
                NotifyTarget::NotCompiled { receiver_syntax: NotifyReceiverSyntax::BareName, .. }
            ));
            assert!(summary.name_literal_refs.is_empty());
            assert!(!summary.name_literal_ref_sites.is_empty());
            super::unused_parameters::check(ctx)
        },
    );
    let unused: Vec<_> = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.code == crate::DiagnosticCode::UnusedParameters)
        .collect();
    assert_eq!(unused.len(), 1, "{diagnostics:#?}");
    assert!(unused[0].message.contains("ОбычныйПараметр"), "{diagnostics:#?}");
}

fn form_with_unread_dialog(extension: bool, base_mapped: bool, malformed: bool) -> Vec<Diagnostic> {
    use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
    use vfs::{FileId, FileSet, VfsPath};

    const REL: &str = "Documents/Заказ/Forms/Форма/Ext/Form/Module.bsl";
    const SOURCE: &str = "&НаКлиенте\nПроцедура Тест()\n    О = Новый ОписаниеОповещения(\"Готово\", ЭтотОбъект);\nКонецПроцедуры\n";
    const BASE: &str = "Процедура Готово() Экспорт\nКонецПроцедуры\n";

    let mut builder = CfeFixtureBuilder::new("");
    if extension {
        builder.add_extension("Расш", "");
    }
    let fixture = builder.build();
    let caller_path =
        if extension { fixture.extensions()[0].root().join(REL) } else { fixture.root().join(REL) };
    let base_path = fixture.root().join(REL);
    std::fs::create_dir_all(caller_path.parent().unwrap()).unwrap();
    std::fs::write(&caller_path, SOURCE).unwrap();
    if base_mapped {
        std::fs::create_dir_all(base_path.parent().unwrap()).unwrap();
        std::fs::write(&base_path, BASE).unwrap();
    }
    if malformed {
        std::fs::write(caller_path.parent().unwrap().parent().unwrap().join("Form.xml"), "<")
            .unwrap();
    }

    let mut db = ide_db::RootDatabaseImpl::new();
    db.set_all_config_paths(fixture.config_paths(), Some(bsl_metadata::ExtensionScope::InfoBase));
    let caller = FileId(0);
    let mut files = FileSet::default();
    files.insert(caller, VfsPath::new(caller_path.to_string_lossy().into_owned()));
    if base_mapped {
        files.insert(FileId(1), VfsPath::new(base_path.to_string_lossy().into_owned()));
    }
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(files));
    db.set_file_source_root(caller, SourceRootId(0));
    db.set_file_text(caller, SOURCE);
    if base_mapped {
        db.set_file_source_root(FileId(1), SourceRootId(0));
        db.set_file_text(FileId(1), BASE);
    }
    assert_eq!(ide_db::weaving_target(&db, caller).is_some(), base_mapped);
    let provider = ide_db::SalsaProvider::new(&db, None);
    let config = DiagnosticsConfig::all_enabled();
    let ctx = DiagnosticsContext::new(&config, caller, &provider);
    assert!(ctx.module_metadata().form.is_none());
    missing_notify_handler::check(&ctx)
}

#[test]
fn unread_form_metadata_never_proves_the_form_is_managed() {
    for malformed in [false, true] {
        for base_mapped in [true, false] {
            let diagnostics = form_with_unread_dialog(true, base_mapped, malformed);
            assert!(diagnostics.is_empty(), "a base form can own the handler: {diagnostics:#?}");
        }
        let diagnostics = form_with_unread_dialog(false, false, malformed);
        assert!(diagnostics.is_empty(), "the base form kind is also unknown: {diagnostics:#?}");
    }
}

/// Repeated same-name same-environment local registrations reuse one verdict instead of
/// repeating the declaration-environment pass per candidate, while every registration
/// site keeps its own spelling in the message: a case-variant spelling shares the
/// verdict and still reports under its own name.
#[test]
fn repeated_local_registrations_share_one_verdict_and_keep_site_spellings() {
    let mut source = String::new();
    for index in 0..48 {
        source.push_str(&format!(
            "&НаКлиенте\nПроцедура Регистрация{index}()\n    О{index} = Новый ОписаниеОповещения(\"Провал\", ЭтотОбъект);\nКонецПроцедуры\n\n"
        ));
    }
    source.push_str("&НаКлиенте\nПроцедура Последняя()\n    О = Новый ОписаниеОповещения(\"провал\", ЭтотОбъект);\nКонецПроцедуры\n\n&НаКлиенте\nПроцедура Провал()\nКонецПроцедуры\n");

    let diagnostics = run_in_command_module(&source, |_, ctx| missing_notify_handler::check(ctx));
    let missing = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.code == crate::DiagnosticCode::MissingNotifyHandler)
        .collect::<Vec<_>>();
    assert_eq!(missing.len(), 49, "one verdict per registration site: {missing:#?}");
    assert!(missing.iter().all(|diagnostic| diagnostic.message.starts_with("Добавьте")));
    let canonical = missing.iter().filter(|d| d.message.contains("\"Провал\"")).count();
    let lowercase = missing.iter().filter(|d| d.message.contains("\"провал\"")).count();
    assert_eq!(
        (canonical, lowercase),
        (48, 1),
        "site spellings survive the shared verdict: {missing:#?}"
    );
}
