use crate::define_metadata;
use crate::metadata::*;
use crate::{Diagnostic, DiagnosticCode, DiagnosticsContext};
use ide_db::TextRange;

pub const METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: DiagnosticType::CodeSmell,
    severity: DiagnosticSeverityLevel::Info,
    scope: DiagnosticScope::Bsl,
    modules: &[bsl_metadata::ModuleType::CommandModule],
    minutes_to_fix: 1,
    activated_by_default: true,
    compatibility_mode: DiagnosticCompatibilityMode::Undefined,
    tags: &[MetadataTag::Standard, MetadataTag::Clumsy],
    can_locate_on_project: false,
    extra_min_for_complexity: 0.0,
    lsp_severity_override: "",
};

pub fn check(ctx: &DiagnosticsContext) -> Vec<Diagnostic> {
    let code = DiagnosticCode::CommandModuleExportMethods;

    // Incomplete callback-constructor facts cannot establish that an export is not a callback.
    if ctx.is_disabled_with_metadata(code)
        || !bsl_platform::has_callback_description_constructor_facts(
            bsl_platform::PlatformData::instance(),
        )
    {
        return Vec::new();
    }

    let metadata = ctx.module_metadata();

    if metadata.module_type != bsl_metadata::ModuleType::CommandModule {
        return Vec::new();
    }

    let item_tree = ctx.item_tree();
    let module = hir::ModuleId::new(ctx.file_id);
    let candidates = ctx.notify_candidates();
    let symbol_tree = ctx.symbol_tree();
    let mut diagnostics = Vec::new();

    for (_, proc) in item_tree.procedures() {
        if proc.is_export
            && !registered_on_this_object(
                ctx,
                &candidates,
                &proc.name,
                hir::MethodId { module, local_id: proc.key },
                &symbol_tree,
            )
        {
            diagnostics.push(make_diagnostic(proc.name_range, code, ctx));
        }
    }

    for (_, func) in item_tree.functions() {
        if func.is_export
            && !registered_on_this_object(
                ctx,
                &candidates,
                &func.name,
                hir::MethodId { module, local_id: func.key },
                &symbol_tree,
            )
        {
            diagnostics.push(make_diagnostic(func.name_range, code, ctx));
        }
    }

    diagnostics
}

// A normalized ThisObject registration can prove a reference. An unresolved dynamic
// constructor also prevents a negative verdict when its syntax names this handler.
fn registered_on_this_object(
    ctx: &DiagnosticsContext,
    candidates: &[hir::NotifyCandidate],
    name: &hir::Name,
    method: hir::MethodId,
    symbol_tree: &hir::SymbolTree,
) -> bool {
    if candidates.iter().any(|candidate| candidate.site_reachable && candidate.analysis_incomplete)
    {
        return true;
    }
    let methods =
        symbol_tree.find_methods(name).into_iter().map(|method| method.id).collect::<Vec<_>>();
    if methods.is_empty() {
        return false;
    }

    let matching = candidates
        .iter()
        .filter(|candidate| candidate.callback_name.eq_ignore_case(name))
        .collect::<Vec<_>>();
    if matching.iter().any(|candidate| {
        candidate.receiver == hir::NotifyCandidateReceiver::ThisObject
            && candidate.registration.as_ref().is_some_and(|registration| {
                ctx.notify_method_is_exported_in_registration(registration, method, &methods)
            })
    }) {
        return true;
    }
    // Syntax-only candidates and unresolved dynamic arrays are conservative; only a proven
    // foreign receiver at this exact expression can rule them out. A resolved ThisObject
    // registration must not fall back after its environment verdict was negative.
    matching.iter().any(|candidate| {
        candidate.site_reachable
            && candidate.protects_local_method
            && (candidate.registration.is_none()
                || candidate.receiver == hir::NotifyCandidateReceiver::Unknown)
    })
}

fn make_diagnostic(range: TextRange, code: DiagnosticCode, ctx: &DiagnosticsContext) -> Diagnostic {
    Diagnostic {
        code,
        message: "Экспортные методы в модулях команд не имеют смысла".to_string(),
        severity: ctx.severity(code),
        range,
        tags: ctx.tags(code),
        fixes: vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::format_diags;
    use crate::DiagnosticsConfig;
    use expect_test::expect;
    use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
    use ide_db::RootDatabaseImpl;
    use std::rc::Rc;
    use vfs::{FileId, FileSet, VfsPath};

    const OBJECT_COMMAND: &str =
        "Documents/ПриходнаяНакладная/Commands/ПечатьЭтикеток/Ext/CommandModule.bsl";
    const COMMON_COMMAND: &str = "CommonCommands/ОбновитьОстатки/Ext/CommandModule.bsl";
    const REGULAR_MODULE: &str = "/СкладскойУчет.bsl";

    fn check_command_at(code: &str) -> Vec<Diagnostic> {
        check_command_at_with(code, check)
    }

    fn check_command_at_with(
        code: &str,
        run: impl FnOnce(&DiagnosticsContext<'_>) -> Vec<Diagnostic>,
    ) -> Vec<Diagnostic> {
        let fixture = test_fixture::CfeFixtureBuilder::new("").build();
        crate::test_utils::check_cfe_at_with_db(
            COMMON_COMMAND,
            code,
            fixture,
            &[],
            DiagnosticsConfig::default(),
            |fixture| {
                let command_dir = fixture.root().join("CommonCommands/ОбновитьОстатки");
                std::fs::create_dir_all(command_dir.join("Ext")).unwrap();
                std::fs::write(command_dir.join("Ext/CommandModule.bsl"), code).unwrap();
                std::fs::write(
                    fixture.root().join("CommonCommands/ОбновитьОстатки.xml"),
                    r#"<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <CommonCommand uuid="00000000-0000-0000-0000-000000000001">
        <Properties><Name>ОбновитьОстатки</Name></Properties>
    </CommonCommand>
</MetaDataObject>"#,
                )
                .unwrap();
            },
            |_, ctx| {
                assert_eq!(
                    ctx.module_metadata().module_type,
                    bsl_metadata::ModuleType::CommandModule
                );
                run(ctx)
            },
        )
    }

    fn check_at(path: &str, code: &str) -> Vec<Diagnostic> {
        let mut db = RootDatabaseImpl::new();
        let file_id = FileId::from_raw(1);

        let mut file_set = FileSet::default();
        file_set.insert(file_id, VfsPath::new(path));
        let source_root = SourceRoot::new_local(file_set);
        db.set_source_root(SourceRootId(0), source_root);
        db.set_file_source_root(file_id, SourceRootId(0));

        db.set_file_text(file_id, code);

        let config = Rc::new(DiagnosticsConfig::default());
        let provider = ide_db::SalsaProvider::new(&db, None);
        let ctx = crate::DiagnosticsContext::new(&config, file_id, &provider);

        check(&ctx)
    }

    const EXPORTED: &str = r#"&НаКлиенте
Процедура ОбработкаКоманды(Накладная, ПараметрыВыполнения) Экспорт
	НапечататьЭтикетки(Накладная);
КонецПроцедуры

&НаСервере
Функция ЧислоЭтикеток(Накладная) Экспорт
	Возврат Накладная.Товары.Количество();
КонецФункции

&НаКлиенте
Процедура НапечататьЭтикетки(Накладная)
КонецПроцедуры
"#;

    #[test]
    fn test_exported_procedure_and_function_detected() {
        let diagnostics = check_at(OBJECT_COMMAND, EXPORTED);
        expect![[r#"
            CommandModuleExportMethods @ 2:11..2:27
              message: Экспортные методы в модулях команд не имеют смысла
              severity: Hint
            CommandModuleExportMethods @ 7:9..7:22
              message: Экспортные методы в модулях команд не имеют смысла
              severity: Hint"#]]
        .assert_eq(&format_diags(EXPORTED, &diagnostics));
    }

    #[test]
    fn same_name_in_foreign_and_unknown_dynamic_slots_remains_uncertain() {
        let code = "&НаКлиенте\nПроцедура Callback() Экспорт\nКонецПроцедуры\n";
        check_command_at_with(code, |ctx| {
            let name = hir::Name::new("Callback");
            let symbol_tree = ctx.symbol_tree();
            let methods = symbol_tree.find_methods(&name);
            let method = methods.first().expect("exported callback method").id;
            let constructor_range = TextRange::new(0.into(), 10.into());
            let foreign_callback_expr = hir::ExprId::from_raw(0.into());
            let unknown_callback_expr = hir::ExprId::from_raw(1.into());
            let registration = hir::call_graph::NotifyReg {
                caller: hir::call_graph::CallerId::ModuleCode,
                callback_expr: foreign_callback_expr,
                callback_name: name.clone(),
                receiver_syntax: hir::call_graph::NotifyReceiverSyntax::BareName,
                target: hir::call_graph::NotifyTarget::Module(hir::Name::new("Foreign")),
                environments: hir::call_graph::NotifyRegistrationEnvironments::default(),
                range: constructor_range,
            };
            let constructor = hir::ExprId::from_raw(2.into());
            let foreign_candidate = hir::NotifyCandidate {
                caller: hir::call_graph::CallerId::ModuleCode,
                constructor,
                expression: foreign_callback_expr,
                slot: hir::NotifyCallbackSlot::Handler,
                kind: hir::NotifyCandidateKind::DynamicArray,
                callback_name: name.clone(),
                receiver_syntax: hir::call_graph::NotifyReceiverSyntax::BareName,
                receiver: hir::NotifyCandidateReceiver::ForeignModule(hir::Name::new("Foreign")),
                protects_local_method: false,
                site_reachable: true,
                analysis_incomplete: false,
                registration: Some(registration.clone()),
            };
            let unknown_candidate = hir::NotifyCandidate {
                caller: hir::call_graph::CallerId::ModuleCode,
                constructor,
                expression: unknown_callback_expr,
                slot: hir::NotifyCallbackSlot::ErrorHandler,
                kind: hir::NotifyCandidateKind::DynamicArray,
                callback_name: name.clone(),
                receiver_syntax: hir::call_graph::NotifyReceiverSyntax::Other,
                receiver: hir::NotifyCandidateReceiver::Unknown,
                protects_local_method: true,
                site_reachable: true,
                analysis_incomplete: false,
                registration: None,
            };
            let site_is_not_disproved_by_other_slot = registered_on_this_object(
                ctx,
                &[foreign_candidate.clone(), unknown_candidate],
                &name,
                method,
                &symbol_tree,
            );
            assert!(
                site_is_not_disproved_by_other_slot,
                "a foreign receiver in slot 1 must not prove an unknown slot 2"
            );

            // The exact same foreign receiver does disprove that particular slot.
            assert!(!registered_on_this_object(
                ctx,
                &[foreign_candidate],
                &name,
                method,
                &symbol_tree,
            ));
            Vec::new()
        });
    }

    #[test]
    fn registered_this_object_handler_is_not_reported() {
        let code = r#"&НаКлиенте
Процедура ОбработкаКоманды(Команда, Параметры) Экспорт
    Оповещение = Новый ОписаниеОповещения("послекоманды", ЭтотОбъект);
КонецПроцедуры

&НаКлиенте
Процедура ПослеКоманды() Экспорт
КонецПроцедуры

Процедура ОбычныйЭкспорт() Экспорт
КонецПроцедуры
"#;
        let diagnostics = check_command_at(code);
        expect![[r#"
            CommandModuleExportMethods @ 2:11..2:27
              message: Экспортные методы в модулях команд не имеют смысла
              severity: Hint
            CommandModuleExportMethods @ 10:11..10:25
              message: Экспортные методы в модулях команд не имеют смысла
              severity: Hint"#]]
        .assert_eq(&format_diags(code, &diagnostics));
    }

    #[test]
    fn dynamic_array_alias_protects_only_unproven_local_callback_registrations() {
        let code = r#"&НаКлиенте
Процедура RegisterCallbacks()
    Перем ПовторныеПараметры;
    ПовторныеПараметры = Новый Массив;
    ПовторныеПараметры.Добавить("SharedDynamicCallback");
    ПовторныеПараметры.Добавить(ЭтотОбъект);
    ПовторныеПараметры.Добавить(Неопределено);
    ПовторныеПараметры.Добавить("SharedDynamicCallback");
    ПовторныеПараметры.Добавить(ЭтотОбъект);
    Оповещение = Новый("CallbackDescription", ПовторныеПараметры);

    Параметры = Новый Массив;
    Параметры.Добавить("AliasCallback");
    Параметры.Добавить(ЭтотОбъект);
    Алиас = Параметры;
    Оповещение = Новый("CallbackDescription", Алиас);
    Оповещение = Новый CallbackDescription("AliasCallback", ДругойМодуль);

    ПараметрыЛокальногоАлиаса = Новый Массив;
    ПараметрыЛокальногоАлиаса.Добавить("LocalAliasCallback");
    ПараметрыЛокальногоАлиаса.Добавить(ЭтотОбъект);
    КопияЛокальногоАлиаса = ПараметрыЛокальногоАлиаса;
    Оповещение = Новый("CallbackDescription", КопияЛокальногоАлиаса);

    ВетвевыеПараметры = Новый Массив;
    Если Истина Тогда
        ВетвевыеПараметры.Добавить("BranchCallback");
        ВетвевыеПараметры.Добавить(ЭтотОбъект);
    КонецЕсли;
    Оповещение = Новый("CallbackDescription", ВетвевыеПараметры);

    ЧужиеПараметры = Новый Массив;
    ЧужиеПараметры.Добавить("ForeignCallback");
    ЧужиеПараметры.Добавить(ДругойМодуль);
    Оповещение = Новый("CallbackDescription", ЧужиеПараметры);
    Оповещение = Новый CallbackDescription("ForeignLocalCallback", ДругойМодуль);

    КлиентскиеПараметры = Новый Массив;
    КлиентскиеПараметры.Добавить("ServerCallback");
    КлиентскиеПараметры.Добавить(ЭтотОбъект);
    Оповещение = Новый("CallbackDescription", КлиентскиеПараметры);
    Оповещение = Новый CallbackDescription("ServerCallback", ЭтотОбъект);
КонецПроцедуры

&НаКлиенте
Процедура SharedDynamicCallback() Экспорт
КонецПроцедуры

&НаКлиенте
Процедура AliasCallback(AliasParameter) Экспорт
    Сообщить(1);
КонецПроцедуры

&НаКлиенте
Процедура BranchCallback(BranchParameter) Экспорт
    Сообщить(1);
КонецПроцедуры

&НаКлиенте
Процедура ForeignCallback(ForeignParameter) Экспорт
    Сообщить(1);
КонецПроцедуры

&НаКлиенте
Процедура ForeignLocalCallback(ForeignLocalParameter)
    Сообщить(1);
КонецПроцедуры

&НаКлиенте
Процедура LocalAliasCallback(LocalAliasParameter)
    Сообщить(1);
КонецПроцедуры

&НаСервере
Процедура ServerCallback() Экспорт
КонецПроцедуры
"#;
        let diagnostics = check_command_at_with(code, |ctx| {
            assert!(bsl_platform::has_callback_description_constructor_facts(
                bsl_platform::PlatformData::instance()
            ));
            let module = hir::ModuleId::new(ctx.file_id);
            let summary = ctx.call_summary(module);
            let candidates = ctx.notify_candidates();
            let duplicate_dynamic_slots = candidates
                .iter()
                .filter(|candidate| {
                    candidate.callback_name.eq_ignore_case(&hir::Name::new("SharedDynamicCallback"))
                })
                .collect::<Vec<_>>();
            assert_eq!(duplicate_dynamic_slots.len(), 2, "{duplicate_dynamic_slots:#?}");
            assert!(duplicate_dynamic_slots
                .iter()
                .all(|candidate| { candidate.kind == hir::NotifyCandidateKind::DynamicArray }));
            assert_eq!(duplicate_dynamic_slots[0].slot, hir::NotifyCallbackSlot::Handler);
            assert_eq!(duplicate_dynamic_slots[1].slot, hir::NotifyCallbackSlot::ErrorHandler);
            for name in ["AliasCallback", "BranchCallback"] {
                assert!(
                    summary
                        .notify_dynamic_signature_handlers
                        .iter()
                        .any(|handler| { handler.eq_ignore_case(&hir::Name::new(name)) }),
                    "{:#?}",
                    summary.notify_dynamic_signature_handlers
                );
                let site = summary
                    .notify_dynamic_signature_handler_sites
                    .iter()
                    .find(|site| site.handler_name.eq_ignore_case(&hir::Name::new(name)))
                    .expect("dynamic callback site");
                assert!(
                    !summary.notify_regs.iter().any(|registration| {
                        registration.range == site.constructor_range
                            && registration.callback_name.eq_ignore_case(&hir::Name::new(name))
                    }),
                    "{:#?}",
                    summary.notify_regs
                );
            }
            let unused_methods = crate::handlers::unused_local_method::check(ctx);
            let unused_parameters = crate::handlers::unused_parameters::check(ctx);
            let reports_name = |items: &[crate::Diagnostic], diagnostic_code, name: &str| {
                items.iter().any(|diagnostic| {
                    if diagnostic.code != diagnostic_code {
                        return false;
                    }
                    let start: usize = diagnostic.range.start().into();
                    let end: usize = diagnostic.range.end().into();
                    &code[start..end] == name
                })
            };
            assert!(reports_name(
                &unused_methods,
                crate::DiagnosticCode::UnusedLocalMethod,
                "ForeignLocalCallback"
            ));
            assert!(!reports_name(
                &unused_methods,
                crate::DiagnosticCode::UnusedLocalMethod,
                "LocalAliasCallback"
            ));
            for name in ["AliasParameter", "BranchParameter"] {
                assert!(
                    !reports_name(
                        &unused_parameters,
                        crate::DiagnosticCode::UnusedParameters,
                        name
                    ),
                    "{unused_parameters:#?}"
                );
            }
            assert!(
                reports_name(
                    &unused_parameters,
                    crate::DiagnosticCode::UnusedParameters,
                    "ForeignLocalParameter"
                ),
                "dynamic sites={:#?} names={:#?} regs={:#?} diagnostics={:#?}",
                summary.notify_dynamic_signature_handler_sites,
                summary.notify_dynamic_signature_handlers,
                summary.notify_regs,
                unused_parameters,
            );
            assert!(!reports_name(
                &unused_parameters,
                crate::DiagnosticCode::UnusedParameters,
                "LocalAliasParameter"
            ));
            check(ctx)
        });
        let reported = diagnostics
            .iter()
            .map(|diagnostic| {
                let start: usize = diagnostic.range.start().into();
                let end: usize = diagnostic.range.end().into();
                &code[start..end]
            })
            .collect::<Vec<_>>();
        assert_eq!(reported, ["ForeignCallback"], "{diagnostics:#?}");
    }

    #[test]
    fn client_registration_does_not_exempt_server_only_handler() {
        let code = r#"&НаКлиенте
Процедура ОбработкаКоманды(Команда, Параметры) Экспорт
    Оповещение = Новый ОписаниеОповещения("ПослеКоманды", ЭтотОбъект);
КонецПроцедуры

&НаСервере
Процедура ПослеКоманды() Экспорт
КонецПроцедуры
"#;
        let diagnostics = check_command_at(code);
        expect![[r#"
            CommandModuleExportMethods @ 2:11..2:27
              message: Экспортные методы в модулях команд не имеют смысла
              severity: Hint
            CommandModuleExportMethods @ 7:11..7:23
              message: Экспортные методы в модулях команд не имеют смысла
              severity: Hint"#]]
        .assert_eq(&format_diags(code, &diagnostics));
    }

    #[test]
    fn dynamic_client_registration_does_not_exempt_server_only_handler() {
        let code = r#"&НаКлиенте
Процедура ОбработкаКоманды(Команда, Параметры) Экспорт
    Перем МассивПараметров;
    МассивПараметров = Новый Массив;
    МассивПараметров.Добавить("ПослеКоманды");
    МассивПараметров.Добавить(ЭтотОбъект);
    Оповещение = Новый("CallbackDescription", МассивПараметров);
КонецПроцедуры

&НаСервере
Процедура ПослеКоманды() Экспорт
КонецПроцедуры
"#;
        let diagnostics = check_command_at_with(code, |ctx| {
            let candidates = ctx.notify_candidates();
            let candidate = candidates
                .iter()
                .find(|candidate| candidate.callback_name.as_str() == "ПослеКоманды")
                .expect("the dynamic callback candidate is exposed");
            assert_eq!(
                candidate.receiver,
                hir::NotifyCandidateReceiver::ThisObject,
                "the callback receiver must be semantically resolved: {candidates:#?}"
            );
            assert!(candidate.registration.is_some(), "{candidates:#?}");
            check(ctx)
        });
        expect![[r#"
            CommandModuleExportMethods @ 2:11..2:27
              message: Экспортные методы в модулях команд не имеют смысла
              severity: Hint
            CommandModuleExportMethods @ 11:11..11:23
              message: Экспортные методы в модулях команд не имеют смысла
              severity: Hint"#]]
        .assert_eq(&format_diags(code, &diagnostics));
    }

    #[test]
    fn complementary_conditional_declarations_cover_the_registration_environment() {
        let code = r#"&НаКлиенте
Процедура ОбработкаКоманды(Команда, Параметры) Экспорт
    Оповещение = Новый ОписаниеОповещения("ПослеКоманды", ЭтотОбъект);
КонецПроцедуры

#Если ТонкийКлиент Тогда
&НаКлиенте
Процедура ПослеКоманды() Экспорт
КонецПроцедуры
#Иначе
&НаКлиенте
Процедура ПослеКоманды() Экспорт
КонецПроцедуры
#КонецЕсли

#Если Сервер Тогда
&НаСервере
Процедура ПослеКоманды() Экспорт
КонецПроцедуры
#КонецЕсли
"#;
        let diagnostics = check_command_at(code);
        expect![[r#"
            CommandModuleExportMethods @ 2:11..2:27
              message: Экспортные методы в модулях команд не имеют смысла
              severity: Hint
            CommandModuleExportMethods @ 18:11..18:23
              message: Экспортные методы в модулях команд не имеют смысла
              severity: Hint"#]]
        .assert_eq(&format_diags(code, &diagnostics));
    }

    #[test]
    fn partially_non_exported_registration_does_not_recommend_removing_required_export() {
        let code = r#"&НаКлиенте
Процедура ОбработкаКоманды(Команда, Параметры) Экспорт
    Оповещение = Новый ОписаниеОповещения("ПослеКоманды", ЭтотОбъект);
КонецПроцедуры

#Если ТонкийКлиент Тогда
&НаКлиенте
Процедура ПослеКоманды() Экспорт
КонецПроцедуры
#Иначе
&НаКлиенте
Процедура ПослеКоманды()
КонецПроцедуры
#КонецЕсли
"#;
        let diagnostics = check_command_at_with(code, |ctx| {
            let mut diagnostics = check(ctx);
            diagnostics.extend(crate::handlers::missing_notify_handler::check(ctx));
            diagnostics
        });
        let export_method_diagnostics = diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.code == DiagnosticCode::CommandModuleExportMethods)
            .cloned()
            .collect::<Vec<_>>();
        expect![[r#"
            CommandModuleExportMethods @ 2:11..2:27
              message: Экспортные методы в модулях команд не имеют смысла
              severity: Hint"#]]
        .assert_eq(&format_diags(code, &export_method_diagnostics));
        assert!(diagnostics.iter().any(|diagnostic| {
            diagnostic.code == DiagnosticCode::MissingNotifyHandler
                && diagnostic.message.contains("ПослеКоманды")
        }));
    }

    #[test]
    fn external_receiver_does_not_hide_export_diagnostic() {
        let code = r#"&НаКлиенте
Процедура ОбработкаКоманды(Команда, Параметры) Экспорт
    Оповещение = Новый ОписаниеОповещения("ПослеКоманды", ДругойОбъект);
КонецПроцедуры

Процедура ПослеКоманды() Экспорт
КонецПроцедуры
"#;
        let diagnostics = check_command_at(code);
        expect![[r#"
            CommandModuleExportMethods @ 2:11..2:27
              message: Экспортные методы в модулях команд не имеют смысла
              severity: Hint
            CommandModuleExportMethods @ 6:11..6:23
              message: Экспортные методы в модулях команд не имеют смысла
              severity: Hint"#]]
        .assert_eq(&format_diags(code, &diagnostics));
    }

    #[test]
    fn unreachable_dynamic_registration_does_not_hide_export_diagnostic() {
        let code = r#"&НаКлиенте
Процедура Зарегистрировать()
#Если Сервер И Не Сервер Тогда
    Параметры = Новый Массив;
    Параметры.Добавить("ПослеКоманды");
    Параметры.Добавить(ЭтотОбъект);
    Оповещение = Новый("CallbackDescription", Параметры);
#КонецЕсли
КонецПроцедуры

Процедура ПослеКоманды() Экспорт
КонецПроцедуры
"#;
        let diagnostics = check_command_at(code);
        expect![[r#"
            CommandModuleExportMethods @ 11:11..11:23
              message: Экспортные методы в модулях команд не имеют смысла
              severity: Hint"#]]
        .assert_eq(&format_diags(code, &diagnostics));
    }

    #[test]
    fn unavailable_or_shadowed_registration_does_not_hide_export_diagnostic() {
        let code = r#"&НаКлиенте
Процедура ОбработкаКоманды(Команда, Параметры) Экспорт
#Если Сервер И Не Сервер Тогда
    Оповещение = Новый ОписаниеОповещения("ПослеНедоступнойКоманды", ЭтотОбъект);
#КонецЕсли
    ЭтотОбъект = Неопределено;
    Оповещение = Новый ОписаниеОповещения("ПослеЗатенения", ЭтотОбъект);
КонецПроцедуры

Процедура ПослеНедоступнойКоманды() Экспорт
КонецПроцедуры

Процедура ПослеЗатенения() Экспорт
КонецПроцедуры
"#;
        let diagnostics = check_command_at(code);
        expect![[r#"
            CommandModuleExportMethods @ 2:11..2:27
              message: Экспортные методы в модулях команд не имеют смысла
              severity: Hint
            CommandModuleExportMethods @ 10:11..10:34
              message: Экспортные методы в модулях команд не имеют смысла
              severity: Hint
            CommandModuleExportMethods @ 13:11..13:25
              message: Экспортные методы в модулях команд не имеют смысла
              severity: Hint"#]]
        .assert_eq(&format_diags(code, &diagnostics));
    }

    #[test]
    fn test_non_exported_ignored() {
        let code = r#"&НаКлиенте
Процедура ОбработкаКоманды(Накладная, ПараметрыВыполнения)
	НапечататьЭтикетки(Накладная);
КонецПроцедуры

&НаСервере
Функция ЧислоЭтикеток(Накладная)
	Возврат Накладная.Товары.Количество();
КонецФункции
"#;
        let diagnostics = check_at(OBJECT_COMMAND, code);
        expect![[r#""#]].assert_eq(&format_diags(code, &diagnostics));
    }

    #[test]
    fn test_common_command_module() {
        let code = r#"&НаКлиенте
Procedure CommandProcessing(CommandParameter, ExecutionParameters) Export
EndProcedure
"#;
        let diagnostics = check_at(COMMON_COMMAND, code);
        expect![[r#"
            CommandModuleExportMethods @ 2:11..2:28
              message: Экспортные методы в модулях команд не имеют смысла
              severity: Hint"#]]
        .assert_eq(&format_diags(code, &diagnostics));
    }

    #[test]
    fn test_regular_module_not_checked() {
        let diagnostics = check_at(REGULAR_MODULE, EXPORTED);
        expect![[r#""#]].assert_eq(&format_diags(EXPORTED, &diagnostics));
    }
}
