use crate::define_metadata;
use crate::metadata::*;
use crate::{Diagnostic, DiagnosticCode, DiagnosticsContext};
use ide_db::TextRange;
use rustc_hash::FxHashMap;

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

    // Without a served platform snapshot no constructor knowledge can distinguish an
    // ordinary export from a handler the platform calls by name, so silence is the only
    // safe verdict. A partial snapshot still identifies ordinary exports.
    if ctx.is_disabled_with_metadata(code) || !ctx.platform_help_available() {
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

    let mut candidates_by_name: FxHashMap<hir::Name, Vec<&hir::NotifyCandidate>> =
        FxHashMap::default();
    let mut has_reachable_incomplete_candidate = false;
    for (index, candidate) in candidates.iter().enumerate() {
        if index & 0xff == 0 {
            ctx.check_cancellation();
        }
        has_reachable_incomplete_candidate |=
            hir::command_module_export_verdict(candidate).global_uncertainty;
        candidates_by_name
            .entry(hir::normalize_match_name(&candidate.callback_name))
            .or_default()
            .push(candidate);
    }
    // A reachable incomplete registration may name any local method, so no export can
    // safely be classified as unregistered.
    if has_reachable_incomplete_candidate {
        return Vec::new();
    }

    let mut methods_by_name: FxHashMap<hir::Name, Vec<hir::MethodId>> = FxHashMap::default();
    let mut exemptions_by_name: FxHashMap<hir::Name, LocalHandlerExemptions> = FxHashMap::default();
    for (index, (_, proc)) in item_tree.procedures().enumerate() {
        if index & 0xff == 0 {
            ctx.check_cancellation();
        }
        if proc.is_export {
            let key = hir::normalize_match_name(&proc.name);
            let methods = methods_by_name.entry(key.clone()).or_insert_with(|| {
                symbol_tree.find_methods(&proc.name).into_iter().map(|method| method.id).collect()
            });
            let exemptions = exemptions_by_name.entry(key.clone()).or_insert_with(|| {
                local_handler_exemptions(
                    ctx,
                    candidates_by_name.get(&key).map_or(&[], Vec::as_slice),
                    methods,
                )
            });
            if !exemptions.protects(hir::MethodId { module, local_id: proc.key }) {
                diagnostics.push(make_diagnostic(proc.name_range, code, ctx));
            }
        }
    }

    for (index, (_, func)) in item_tree.functions().enumerate() {
        if index & 0xff == 0 {
            ctx.check_cancellation();
        }
        if func.is_export {
            let key = hir::normalize_match_name(&func.name);
            let methods = methods_by_name.entry(key.clone()).or_insert_with(|| {
                symbol_tree.find_methods(&func.name).into_iter().map(|method| method.id).collect()
            });
            let exemptions = exemptions_by_name.entry(key.clone()).or_insert_with(|| {
                local_handler_exemptions(
                    ctx,
                    candidates_by_name.get(&key).map_or(&[], Vec::as_slice),
                    methods,
                )
            });
            if !exemptions.protects(hir::MethodId { module, local_id: func.key }) {
                diagnostics.push(make_diagnostic(func.name_range, code, ctx));
            }
        }
    }

    diagnostics
}

#[derive(Default)]
struct LocalHandlerExemptions {
    registered_methods: rustc_hash::FxHashSet<hir::MethodId>,
    syntax_protected: bool,
}

impl LocalHandlerExemptions {
    fn protects(&self, method: hir::MethodId) -> bool {
        self.syntax_protected || self.registered_methods.contains(&method)
    }
}

// Registration environments are combined here, but method export/environment semantics stay
// in HIR: the semantic facade computes the eligible method set in one pass per name.
fn local_handler_exemptions(
    ctx: &DiagnosticsContext,
    matching: &[&hir::NotifyCandidate],
    methods: &[hir::MethodId],
) -> LocalHandlerExemptions {
    if methods.is_empty() {
        return LocalHandlerExemptions::default();
    }

    let mut registrations = Vec::new();
    let mut syntax_protected = false;
    for (index, candidate) in matching.iter().enumerate() {
        if index & 0xff == 0 {
            ctx.check_cancellation();
        }
        let verdict = hir::command_module_export_verdict(candidate);
        if let Some(registration) = verdict.local_registration {
            registrations.push(registration);
        }
        // A resolved local registration must not fall back after its environment
        // verdict was negative; unresolved receiver protection is projected separately.
        syntax_protected |= verdict.protects_export;
    }

    LocalHandlerExemptions {
        registered_methods: ctx.notify_methods_exported_in_registrations(methods, &registrations),
        syntax_protected,
    }
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

    #[test]
    fn local_binding_receiver_stays_uncertain_about_the_local_handler() {
        use hir::NotifyCandidateReceiver;

        let code = r#"&НаКлиенте
Процедура ОбработкаКоманды(Команда, ПараметрыВыполнения) Экспорт
    Р = ЭтотОбъект;
    Оповещение = Новый ОписаниеОповещения("ПослеКоманды", Р);
КонецПроцедуры

Процедура ПослеКоманды(Параметр) Экспорт
КонецПроцедуры
"#;
        let diagnostics = check_command_at_with(code, |ctx| {
            let candidates = ctx.notify_candidates();
            let candidate = candidates
                .iter()
                .find(|candidate| candidate.callback_name.as_str() == "ПослеКоманды")
                .expect("the callback candidate is exposed");
            // The receiver is a local binding of unknown runtime value: uncertainty,
            // not a proven absence of a local callback.
            assert_eq!(candidate.receiver, NotifyCandidateReceiver::LocalBinding);
            assert!(candidate.protects_local_method);
            assert_ne!(
                candidate.signature_protection(),
                hir::NotifySignatureProtection::NotProtected,
                "uncertainty must not become a proven-foreign verdict"
            );
            assert!(
                hir::command_module_export_verdict(candidate).protects_export,
                "uncertainty must protect the export"
            );
            let mut diagnostics = super::check(ctx);
            diagnostics.extend(crate::handlers::unused_parameters::check(ctx));
            diagnostics
        });
        let handler_start = code.rfind("Процедура ПослеКоманды").unwrap();
        let false_findings: Vec<_> = diagnostics
            .iter()
            .filter(|diagnostic| usize::from(diagnostic.range.start()) >= handler_start)
            .filter(|diagnostic| {
                matches!(
                    diagnostic.code,
                    DiagnosticCode::UnusedParameters | DiagnosticCode::CommandModuleExportMethods
                )
            })
            .collect();
        assert!(
            false_findings.is_empty(),
            "`Р = ЭтотОбъект` must not produce false UnusedParameters or export findings for the callback: {false_findings:#?}"
        );
    }

    #[test]
    fn known_value_receiver_does_not_protect_a_command_handler() {
        let code = r#"&НаКлиенте
Процедура Регистрация()
    Запрос = Новый Запрос("ВЫБРАТЬ 1");
    Результат = Запрос.Выполнить();
    Оповещение = Новый ОписаниеОповещения("Обработчик", Результат);
КонецПроцедуры

Процедура Обработчик(Параметр) Экспорт
    Сообщить(1);
КонецПроцедуры
"#;
        let diagnostics = check_command_at_with(code, |ctx| {
            let candidates = ctx.notify_candidates();
            let candidate = candidates
                .iter()
                .find(|candidate| candidate.callback_name.as_str() == "Обработчик")
                .expect("the callback candidate is exposed");
            assert_eq!(
                candidate.signature_protection(),
                hir::NotifySignatureProtection::NotProtected,
                "{candidate:#?}"
            );
            assert!(!candidate.protects_local_method);
            assert!(!hir::command_module_export_verdict(candidate).protects_export);
            let mut diagnostics = super::check(ctx);
            diagnostics.extend(crate::handlers::unused_parameters::check(ctx));
            diagnostics
        });
        let handler_start = code.rfind("Процедура Обработчик").unwrap();
        for code in [DiagnosticCode::UnusedParameters, DiagnosticCode::CommandModuleExportMethods] {
            assert!(
                diagnostics.iter().any(|diagnostic| {
                    diagnostic.code == code
                        && usize::from(diagnostic.range.start()) >= handler_start
                }),
                "{code:?} must report the unrelated handler: {diagnostics:#?}"
            );
        }
    }

    fn check_command_with_unreadable_global(
        code: &str,
        run: impl FnOnce(&DiagnosticsContext<'_>) -> Vec<Diagnostic>,
    ) -> Vec<Diagnostic> {
        const GLOBAL: &str = "НедоступныйГлобальный";
        let mut builder = test_fixture::CfeFixtureBuilder::new("");
        builder.add_base_module_global(GLOBAL, "Процедура Чужая() Экспорт КонецПроцедуры");
        let fixture = builder.build();
        let unreadable = format!("CommonModules/{GLOBAL}/Ext/Module.bsl");
        crate::test_utils::check_cfe_at_with_db(
            COMMON_COMMAND,
            code,
            fixture,
            &[unreadable.as_str()],
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

    #[test]
    fn function_receiver_stays_uncertain_about_the_local_handler() {
        let code = r#"&НаКлиенте
Процедура Регистрация()
    Оповещение = Новый ОписаниеОповещения("Обработчик", ВернутьПриёмник());
КонецПроцедуры

Функция ВернутьПриёмник()
    Возврат ЭтотОбъект;
КонецФункции

Процедура Обработчик(Параметр) Экспорт
    Сообщить(1);
КонецПроцедуры
"#;
        let diagnostics = check_command_at_with(code, |ctx| {
            let candidates = ctx.notify_candidates();
            let candidate = candidates
                .iter()
                .find(|candidate| candidate.callback_name.as_str() == "Обработчик")
                .expect("the callback candidate is exposed");
            assert_eq!(
                candidate.receiver,
                hir::NotifyCandidateReceiver::LocalBinding,
                "{candidate:#?}"
            );
            assert!(candidate.protects_local_method, "{candidate:#?}");
            assert_eq!(
                candidate.signature_protection(),
                hir::NotifySignatureProtection::Protected,
                "{candidate:#?}"
            );
            let registration = candidate.registration.as_ref().expect("registration");
            assert_eq!(
                registration.target,
                hir::call_graph::NotifyTarget::LocalBinding,
                "an uncertain receiver must not become a callback edge"
            );
            assert!(
                crate::handlers::missing_notify_handler::check(ctx).is_empty(),
                "an uncertain receiver must not become a MissingNotifyHandler finding"
            );
            let mut diagnostics = super::check(ctx);
            diagnostics.extend(crate::handlers::unused_parameters::check(ctx));
            diagnostics
        });
        let handler_start = code.rfind("Процедура Обработчик").unwrap();
        let false_findings: Vec<_> = diagnostics
            .iter()
            .filter(|diagnostic| usize::from(diagnostic.range.start()) >= handler_start)
            .filter(|diagnostic| {
                matches!(
                    diagnostic.code,
                    DiagnosticCode::UnusedParameters | DiagnosticCode::CommandModuleExportMethods
                )
            })
            .collect();
        assert!(
            false_findings.is_empty(),
            "a call receiver must not prove the callback foreign: {false_findings:#?}"
        );
    }

    #[test]
    fn indeterminate_bare_receiver_stays_uncertain_about_the_local_handler() {
        let code = r#"&НаКлиенте
Процедура Регистрация()
    Оповещение = Новый ОписаниеОповещения("Обработчик", Приёмник);
КонецПроцедуры

Процедура Обработчик(Параметр) Экспорт
    Сообщить(1);
КонецПроцедуры
"#;
        let diagnostics = check_command_with_unreadable_global(code, |ctx| {
            let candidates = ctx.notify_candidates();
            let candidate = candidates
                .iter()
                .find(|candidate| candidate.callback_name.as_str() == "Обработчик")
                .expect("the callback candidate is exposed");
            // The unreadable global body leaves the bare receiver's miss indeterminate,
            // so the local callback is not proven foreign.
            assert_eq!(
                candidate.receiver,
                hir::NotifyCandidateReceiver::LocalBinding,
                "{candidate:#?}"
            );
            assert_eq!(
                candidate.signature_protection(),
                hir::NotifySignatureProtection::Protected,
                "{candidate:#?}"
            );
            let registration = candidate.registration.as_ref().expect("registration");
            assert_eq!(
                registration.target,
                hir::call_graph::NotifyTarget::LocalBinding,
                "an indeterminate receiver must not become a callback edge"
            );
            assert!(
                crate::handlers::missing_notify_handler::check(ctx).is_empty(),
                "an indeterminate receiver must not become a MissingNotifyHandler finding"
            );
            let mut diagnostics = super::check(ctx);
            diagnostics.extend(crate::handlers::unused_parameters::check(ctx));
            diagnostics
        });
        let handler_start = code.rfind("Процедура Обработчик").unwrap();
        let false_findings: Vec<_> = diagnostics
            .iter()
            .filter(|diagnostic| usize::from(diagnostic.range.start()) >= handler_start)
            .filter(|diagnostic| {
                matches!(
                    diagnostic.code,
                    DiagnosticCode::UnusedParameters | DiagnosticCode::CommandModuleExportMethods
                )
            })
            .collect();
        assert!(
            false_findings.is_empty(),
            "an indeterminate receiver must not prove the callback foreign: {false_findings:#?}"
        );
    }

    #[test]
    fn call_returning_a_known_value_does_not_protect_a_command_handler() {
        let code = r#"&НаКлиенте
Процедура Регистрация()
    Оповещение = Новый ОписаниеОповещения("Обработчик", ПолучитьМассив());
КонецПроцедуры

Функция ПолучитьМассив()
    Возврат Новый Массив;
КонецФункции

Процедура Обработчик(Параметр) Экспорт
    Сообщить(1);
КонецПроцедуры
"#;
        let diagnostics = check_command_at_with(code, |ctx| {
            let candidates = ctx.notify_candidates();
            let candidate = candidates
                .iter()
                .find(|candidate| candidate.callback_name.as_str() == "Обработчик")
                .expect("the callback candidate is exposed");
            // A call whose result is concrete data cannot be the current module object.
            assert_eq!(candidate.receiver, hir::NotifyCandidateReceiver::Unknown, "{candidate:#?}");
            assert!(!candidate.protects_local_method, "{candidate:#?}");
            super::check(ctx)
        });
        let handler_start = code.rfind("Процедура Обработчик").unwrap();
        assert!(
            diagnostics.iter().any(|diagnostic| {
                diagnostic.code == DiagnosticCode::CommandModuleExportMethods
                    && usize::from(diagnostic.range.start()) >= handler_start
            }),
            "a concrete call result proves the callback foreign: {diagnostics:#?}"
        );
    }

    #[test]
    fn absent_bare_receiver_does_not_protect_a_command_handler() {
        let code = r#"&НаКлиенте
Процедура Регистрация()
    Оповещение = Новый ОписаниеОповещения("Обработчик", ОтсутствующийМодуль);
КонецПроцедуры

Процедура Обработчик(Параметр) Экспорт
    Сообщить(1);
КонецПроцедуры
"#;
        let diagnostics = check_command_at_with(code, |ctx| {
            let candidates = ctx.notify_candidates();
            let candidate = candidates
                .iter()
                .find(|candidate| candidate.callback_name.as_str() == "Обработчик")
                .expect("the callback candidate is exposed");
            // A complete surface makes the missing name a proven absence, not uncertainty.
            assert_eq!(candidate.receiver, hir::NotifyCandidateReceiver::Unknown, "{candidate:#?}");
            assert!(!candidate.protects_local_method, "{candidate:#?}");
            let mut diagnostics = super::check(ctx);
            diagnostics.extend(crate::handlers::unused_parameters::check(ctx));
            diagnostics
        });
        let handler_start = code.rfind("Процедура Обработчик").unwrap();
        for code in [DiagnosticCode::UnusedParameters, DiagnosticCode::CommandModuleExportMethods] {
            assert!(
                diagnostics.iter().any(|diagnostic| {
                    diagnostic.code == code
                        && usize::from(diagnostic.range.start()) >= handler_start
                }),
                "{code:?} must report the unrelated handler: {diagnostics:#?}"
            );
        }
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
    fn many_distinct_callback_names_keep_their_own_export_exemption() {
        let mut code = String::new();
        for index in 0..64 {
            code.push_str(&format!(
                "&НаКлиенте\nПроцедура Зарегистрировать{index}()\n    О = Новый ОписаниеОповещения(\"Обработчик{index}\", ЭтотОбъект);\nКонецПроцедуры\n\n&НаКлиенте\nПроцедура Обработчик{index}() Экспорт\nКонецПроцедуры\n\n"
            ));
        }
        code.push_str("Процедура ОбычныйЭкспорт() Экспорт\nКонецПроцедуры\n");

        let diagnostics = check_command_at(&code);
        let reported = diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.code == DiagnosticCode::CommandModuleExportMethods)
            .collect::<Vec<_>>();
        assert_eq!(reported.len(), 1, "only the unrelated export is reported: {reported:#?}");
        let start: usize = reported[0].range.start().into();
        let end: usize = reported[0].range.end().into();
        assert_eq!(&code[start..end], "ОбычныйЭкспорт");
    }

    #[test]
    fn repeated_conditional_handlers_and_registrations_share_one_semantic_pass() {
        let mut code = String::new();
        for index in 0..48 {
            code.push_str(&format!(
                "&НаКлиенте\nПроцедура Регистрация{index}()\n    О = Новый ОписаниеОповещения(\"ПовторныйОбработчик\", ЭтотОбъект);\nКонецПроцедуры\n\n"
            ));
        }
        for _ in 0..48 {
            code.push_str(
                "#Если Сервер Тогда\n&НаСервере\nПроцедура ПовторныйОбработчик() Экспорт\nКонецПроцедуры\n#Иначе\n&НаКлиенте\nПроцедура ПовторныйОбработчик() Экспорт\nКонецПроцедуры\n#КонецЕсли\n\n",
            );
        }
        code.push_str("Процедура ОбычныйЭкспорт() Экспорт\nКонецПроцедуры\n");

        let diagnostics = check_command_at(&code);
        let reported = diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.code == DiagnosticCode::CommandModuleExportMethods)
            .collect::<Vec<_>>();
        let repeated_handlers = reported
            .iter()
            .filter(|diagnostic| {
                let start: usize = diagnostic.range.start().into();
                let end: usize = diagnostic.range.end().into();
                &code[start..end] == "ПовторныйОбработчик"
            })
            .count();
        assert_eq!(
            repeated_handlers, 48,
            "only the server-side conditional declarations are unregistered: {reported:#?}"
        );
        assert_eq!(reported.len(), 49, "the unrelated export is still reported");
    }

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
            let method_ids = methods.iter().map(|method| method.id).collect::<Vec<_>>();
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
            let unknown_verdict = hir::command_module_export_verdict(&unknown_candidate);
            assert!(!unknown_verdict.global_uncertainty);
            assert!(unknown_verdict.local_registration.is_none());
            assert!(unknown_verdict.protects_export);

            let mut unreachable_local_registration = foreign_candidate.clone();
            unreachable_local_registration.receiver = hir::NotifyCandidateReceiver::ThisObject;
            unreachable_local_registration.site_reachable = false;
            let unreachable_verdict =
                hir::command_module_export_verdict(&unreachable_local_registration);
            assert!(!unreachable_verdict.global_uncertainty);
            assert!(unreachable_verdict.local_registration.is_some());
            assert!(!unreachable_verdict.protects_export);

            let mut incomplete_candidate = unknown_candidate.clone();
            incomplete_candidate.analysis_incomplete = true;
            assert!(hir::command_module_export_verdict(&incomplete_candidate).global_uncertainty);
            incomplete_candidate.site_reachable = false;
            assert!(!hir::command_module_export_verdict(&incomplete_candidate).global_uncertainty);

            let site_is_not_disproved_by_other_slot = local_handler_exemptions(
                ctx,
                &[&foreign_candidate, &unknown_candidate],
                &method_ids,
            )
            .protects(method);
            assert!(
                site_is_not_disproved_by_other_slot,
                "a foreign receiver in slot 1 must not prove an unknown slot 2"
            );

            // The exact same foreign receiver does disprove that particular slot.
            assert!(
                !local_handler_exemptions(ctx, &[&foreign_candidate], &method_ids).protects(method)
            );
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
