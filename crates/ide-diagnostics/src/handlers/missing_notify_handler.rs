use crate::define_metadata;
use crate::metadata::*;
use crate::{Diagnostic, DiagnosticCode, DiagnosticsContext};
#[cfg(test)]
use hir::ModuleId;
use hir::Name;
use rustc_hash::FxHashMap;

pub const METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: crate::metadata::DiagnosticType::Error,
    severity: DiagnosticSeverityLevel::Blocker,
    scope: DiagnosticScope::Bsl,
    modules: &[],
    minutes_to_fix: 5,
    activated_by_default: true,
    compatibility_mode: DiagnosticCompatibilityMode::Undefined,
    tags: &[MetadataTag::Error],
    can_locate_on_project: false,
    extra_min_for_complexity: 0.0,
    lsp_severity_override: "",
};

/// `Новый ОписаниеОповещения("Имя", Приёмник)` names its handler by a string, and the
/// platform calls it only as an exported method: a missing or non-exported handler
/// compiles and passes the module check, then fails at run time with
/// `Не найден экспортируемый метод`.
pub fn check(ctx: &DiagnosticsContext) -> Vec<Diagnostic> {
    let code = DiagnosticCode::MissingNotifyHandler;

    if ctx.is_disabled_with_metadata(code) {
        return Vec::new();
    }

    let notify_candidates = ctx.notify_candidates();
    if notify_candidates.is_empty() {
        return Vec::new();
    }

    let metadata = ctx.module_metadata();
    // The rule is established for managed forms only; missing metadata cannot
    // prove either the form kind or whether a base form supplies the handler.
    if metadata.module_type == bsl_metadata::ModuleType::FormModule
        && !metadata.form.as_deref().is_some_and(|form| form.is_managed())
    {
        return Vec::new();
    }

    // A borrowed form exposes its base module, which is not in this file's symbol tree.
    let local_unprovable = metadata.form.as_deref().is_some_and(|form| form.has_base_form());

    let mut diagnostics = Vec::new();
    let mut reported_slots: Vec<(syntax::TextRange, Option<Name>, Name)> = Vec::new();
    let mut module_verdicts = FxHashMap::default();
    for candidate in notify_candidates.iter() {
        let Some(reg) = candidate.registration.as_ref() else { continue };
        let verdict = match &candidate.receiver {
            hir::NotifyCandidateReceiver::ThisObject if local_unprovable => None,
            hir::NotifyCandidateReceiver::ThisObject => local_verdict(ctx, &reg.callback_name, reg)
                .map(|v| (v, reg.callback_name.to_string())),
            hir::NotifyCandidateReceiver::ForeignModule(module_name) => {
                let key = (module_name.clone(), reg.callback_name.clone(), reg.environments);
                let verdict = *module_verdicts
                    .entry(key)
                    .or_insert_with(|| module_verdict(ctx, module_name, &reg.callback_name, reg));
                verdict.map(|v| {
                    (v, format!("{}.{}", module_name.as_str(), reg.callback_name.as_str()))
                })
            }
            hir::NotifyCandidateReceiver::Unknown | hir::NotifyCandidateReceiver::NotCompiled => {
                None
            }
        };
        let Some((verdict, detail)) = verdict else { continue };
        let message = message(verdict, &detail);
        let module = match &candidate.receiver {
            hir::NotifyCandidateReceiver::ForeignModule(module) => Some(module.clone()),
            _ => None,
        };
        if reported_slots.iter().any(|(range, previous_module, previous_name)| {
            *range == reg.range
                && previous_name.eq_ignore_case(&reg.callback_name)
                && match (previous_module, &module) {
                    (Some(left), Some(right)) => left.eq_ignore_case(right),
                    (None, None) => true,
                    _ => false,
                }
        }) {
            continue;
        }
        reported_slots.push((reg.range, module, reg.callback_name.clone()));
        diagnostics.push(Diagnostic {
            code,
            message,
            severity: ctx.severity(code),
            range: reg.range,
            tags: ctx.tags(code),
            fixes: vec![],
        });
    }
    diagnostics
}

#[derive(Debug, Clone, Copy)]
enum Verdict {
    NotExported,
    NotFound,
}

fn message(verdict: Verdict, detail: &str) -> String {
    match verdict {
        Verdict::NotExported => format!(
            "Добавьте \"Экспорт\" процедуре \"{}\" - обработчик оповещения вызывается платформой только как экспортный",
            detail
        ),
        Verdict::NotFound => format!(
            "Создайте экспортную процедуру \"{}\" или исправьте имя обработчика оповещения",
            detail
        ),
    }
}

/// Handler named on `ЭтотОбъект`: only declarations available in the registration's
/// execution environments can satisfy the string reference.
fn local_verdict(
    ctx: &DiagnosticsContext,
    name: &Name,
    registration: &hir::call_graph::NotifyReg,
) -> Option<Verdict> {
    let methods = ctx
        .symbol_tree()
        .find_methods(name)
        .into_iter()
        .map(|method| method.id)
        .collect::<Vec<_>>();

    match ctx.notify_handler_availability_in_registration(registration, &methods) {
        hir::NotifyHandlerAvailability::Exported | hir::NotifyHandlerAvailability::Unknown => None,
        hir::NotifyHandlerAvailability::NotExported => Some(Verdict::NotExported),
        hir::NotifyHandlerAvailability::NotFound => Some(Verdict::NotFound),
    }
}

/// Handler named on a bare identifier. Only a receiver that is a known common module is
/// judged: any other identifier (a local variable, a module the analysis cannot see) says
/// nothing about where the handler lives. Registration and declaration environments are
/// compared by the semantic provider; incomplete expected bodies remain unknown.
fn module_verdict(
    ctx: &DiagnosticsContext,
    module_name: &Name,
    name: &Name,
    registration: &hir::call_graph::NotifyReg,
) -> Option<Verdict> {
    ctx.resolve_common_module(module_name.as_str())?;
    let (method_bodies, incomplete_tail) = match ctx
        .qualified_method_declarations(module_name, name)?
    {
        Ok(declarations) => (declarations.bodies, declarations.incomplete_tail),
        Err(hir::QualifiedMethodError::NotFound) => (Vec::new(), false),
        Err(
            hir::QualifiedMethodError::BodyUnread | hir::QualifiedMethodError::NotVisibleInConfigs,
        ) => return None,
    };
    let availability = ctx.notify_handler_availability_for_method_bodies_in_registration(
        registration,
        &method_bodies,
        incomplete_tail,
    );
    match availability {
        hir::NotifyHandlerAvailability::Exported | hir::NotifyHandlerAvailability::Unknown => None,
        hir::NotifyHandlerAvailability::NotExported => Some(Verdict::NotExported),
        hir::NotifyHandlerAvailability::NotFound => Some(Verdict::NotFound),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::format_diags;
    use expect_test::expect;
    use hir::call_graph::{EdgeKind, EdgeProvenance, ResolvedTarget};
    use hir::ConfigsDatabase;
    use test_fixture::CfeFixtureBuilder;

    const CALLER: &str = "CommonForms/Форма1/Ext/Form/Module.bsl";

    fn client_context_source(source: &str) -> String {
        let mut client_source = String::with_capacity(source.len() + 32);
        let mut previous_nonempty_line = None;
        let mut method_count = 0;
        for line in source.split_inclusive('\n') {
            let trimmed = line.trim_start();
            if trimmed.starts_with("Процедура ") || trimmed.starts_with("Асинх Процедура ")
            {
                method_count += 1;
                if !previous_nonempty_line.is_some_and(|line: &str| line.starts_with('&')) {
                    client_source.push_str("&НаКлиенте\n");
                }
            }
            client_source.push_str(line);
            if !trimmed.trim().is_empty() {
                previous_nonempty_line = Some(trimmed.trim());
            }
        }
        assert!(method_count > 0, "test source has no method declaration: {source}");
        client_source
    }

    fn run(source: &str, fixture: test_fixture::CfeFixture) -> Vec<Diagnostic> {
        run_with_form_xml(source, PLAIN_FORM_XML, fixture)
    }

    fn run_local(source: &str) -> Vec<Diagnostic> {
        run(source, CfeFixtureBuilder::new("").build())
    }

    fn run_local_with_string_resolved_notify_edges(
        source: &str,
        expected_edges: usize,
    ) -> Vec<Diagnostic> {
        let source = client_context_source(source);
        crate::test_utils::check_cfe_at_with_db(
            CALLER,
            &source,
            CfeFixtureBuilder::new("").build(),
            &[],
            crate::DiagnosticsConfig::all_enabled(),
            |fixture| {
                let form = fixture.root().join("CommonForms/Форма1/Ext/Form.xml");
                std::fs::create_dir_all(form.parent().unwrap()).unwrap();
                std::fs::write(&form, PLAIN_FORM_XML).unwrap();
            },
            |db, ctx| {
                let graph = db.resolved_module_summary(ModuleId::new(ctx.file_id));
                let edges: Vec<_> =
                    graph.edges.iter().filter(|edge| edge.kind == EdgeKind::NotifyRef).collect();
                assert_eq!(edges.len(), expected_edges, "{edges:#?}");
                assert!(
                    edges.iter().all(|edge| {
                        edge.provenance == EdgeProvenance::StringResolved
                            && matches!(edge.target, ResolvedTarget::Method(_))
                    }),
                    "the graph still resolves string callbacks independent of declaration availability: {edges:#?}"
                );
                check(ctx)
            },
        )
    }

    fn enable_managed_client_module(path: &std::path::Path) {
        let xml = std::fs::read_to_string(path).unwrap();
        let xml = xml.replace(
            "<ClientManagedApplication>false</ClientManagedApplication>",
            "<ClientManagedApplication>true</ClientManagedApplication>",
        );
        std::fs::write(path, xml).unwrap();
    }

    fn with_module(source: &str, module_body: &str) -> Vec<Diagnostic> {
        use crate::test_utils::check_cfe_at_with_db;

        let mut builder = CfeFixtureBuilder::new("");
        builder.add_base_module("МойМодульКлиент", module_body);
        check_cfe_at_with_db(
            CALLER,
            &client_context_source(source),
            builder.build(),
            &[],
            crate::DiagnosticsConfig::all_enabled(),
            |fixture| {
                let module_xml = fixture.root().join("CommonModules/МойМодульКлиент.xml");
                enable_managed_client_module(&module_xml);
                let form = fixture.root().join("CommonForms/Форма1/Ext/Form.xml");
                std::fs::create_dir_all(form.parent().unwrap()).unwrap();
                std::fs::write(form, PLAIN_FORM_XML).unwrap();
            },
            |_, ctx| check(ctx),
        )
    }

    fn local_source(export: &str) -> String {
        format!(
            "Процедура ПередЗакрытием()\n\tОповещение = Новый ОписаниеОповещения(\"ОбработчикЗавершения\", ЭтотОбъект);\nКонецПроцедуры\n\nПроцедура ОбработчикЗавершения(Результат, Параметры){export}\nКонецПроцедуры\n"
        )
    }

    #[test]
    fn a_local_handler_without_export_is_reported() {
        let source = local_source("");
        let diagnostics = run_local(&source);
        expect![[r#"
            MissingNotifyHandler @ 3:15..3:75
              message: Добавьте "Экспорт" процедуре "ОбработчикЗавершения" - обработчик оповещения вызывается платформой только как экспортный
              severity: Blocker"#]]
        .assert_eq(&format_diags(&client_context_source(&source), &diagnostics));
    }

    /// Control for the case above: the fixture differs by the single word `Экспорт`.
    #[test]
    fn the_same_local_handler_with_export_is_silent() {
        let source = local_source(" Экспорт");
        let diagnostics = run_local(&source);
        assert!(diagnostics.is_empty(), "an exported handler must pass: {diagnostics:#?}");
    }

    #[test]
    fn an_export_in_an_impossible_preprocessor_branch_does_not_satisfy_a_live_client_registration()
    {
        let source = "&НаКлиенте\nПроцедура ПередЗакрытием()\n    О = Новый ОписаниеОповещения(\"Готово\", ЭтотОбъект);\nКонецПроцедуры\n#Если Сервер И Не Сервер Тогда\n&НаКлиенте\nПроцедура Готово() Экспорт\nКонецПроцедуры\n#КонецЕсли\n";
        let diagnostics = run_local_with_string_resolved_notify_edges(source, 1);
        assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
        assert!(diagnostics[0].message.starts_with("Создайте экспортную процедуру"));
    }

    #[test]
    fn a_live_client_export_satisfies_a_client_registration() {
        let source = "&НаКлиенте\nПроцедура ПередЗакрытием()\n    О = Новый ОписаниеОповещения(\"Готово\", ЭтотОбъект);\nКонецПроцедуры\n&НаКлиенте\nПроцедура Готово() Экспорт\nКонецПроцедуры\n";
        let diagnostics = run_local_with_string_resolved_notify_edges(source, 1);
        assert!(diagnostics.is_empty(), "{diagnostics:#?}");
    }

    #[test]
    fn a_client_export_covers_all_constructor_available_environments() {
        let source = "&НаКлиентеНаСервере\nПроцедура ПередЗакрытием()\n    О = Новый ОписаниеОповещения(\"Готово\", ЭтотОбъект);\nКонецПроцедуры\n&НаКлиенте\nПроцедура Готово() Экспорт\nКонецПроцедуры\n";
        let diagnostics = run_local_with_string_resolved_notify_edges(source, 1);
        assert!(diagnostics.is_empty(), "{diagnostics:#?}");
    }

    #[test]
    fn complementary_exports_of_same_named_declarations_cover_the_registration_environments() {
        let source = "&НаКлиентеНаСервере\nПроцедура ПередЗакрытием()\n    О = Новый ОписаниеОповещения(\"Готово\", ЭтотОбъект);\nКонецПроцедуры\n#Если Клиент Тогда\n&НаКлиенте\nПроцедура Готово() Экспорт\nКонецПроцедуры\n#КонецЕсли\n#Если Сервер Тогда\n&НаСервере\nПроцедура Готово() Экспорт\nКонецПроцедуры\n#КонецЕсли\n";
        let diagnostics = run_local_with_string_resolved_notify_edges(source, 1);
        assert!(diagnostics.is_empty(), "{diagnostics:#?}");
    }

    #[test]
    fn server_only_registration_position_has_no_constructor_environment() {
        let source = "&НаКлиентеНаСервере\nПроцедура ПередЗакрытием()\n#Если Клиент Тогда\n    А = Новый ОписаниеОповещения(\"Готово\", ЭтотОбъект);\n#КонецЕсли\n#Если Сервер Тогда\n    Б = Новый ОписаниеОповещения(\"Готово\", ЭтотОбъект);\n#КонецЕсли\nКонецПроцедуры\n#Если Клиент Тогда\n&НаКлиенте\nПроцедура Готово() Экспорт\nКонецПроцедуры\n#КонецЕсли\n";
        let diagnostics = run_local_with_string_resolved_notify_edges(source, 2);
        assert!(diagnostics.is_empty(), "{diagnostics:#?}");
    }

    #[test]
    fn a_non_exported_declaration_outside_the_registration_environment_does_not_change_not_found() {
        let source = "&НаКлиенте\nПроцедура ПередЗакрытием()\n    О = Новый ОписаниеОповещения(\"Готово\", ЭтотОбъект);\nКонецПроцедуры\n#Если Сервер Тогда\n&НаСервере\nПроцедура Готово()\nКонецПроцедуры\n#КонецЕсли\n";
        let diagnostics = run_local_with_string_resolved_notify_edges(source, 1);
        assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
        assert!(diagnostics[0].message.starts_with("Создайте экспортную процедуру"));
    }

    #[test]
    fn an_undecidable_export_environment_does_not_produce_a_verdict() {
        let source = "&НаКлиенте\nПроцедура ПередЗакрытием()\n    О = Новый ОписаниеОповещения(\"Готово\", ЭтотОбъект);\nКонецПроцедуры\n#Если НеизвестныйСимвол Тогда\n&НаКлиенте\nПроцедура Готово() Экспорт\nКонецПроцедуры\n#КонецЕсли\n";
        let diagnostics = run_local(source);
        assert!(
            diagnostics.is_empty(),
            "unknown preprocessor availability is not proof of absence: {diagnostics:#?}"
        );
    }

    #[test]
    fn a_local_handler_that_does_not_exist_is_reported() {
        let source = "Процедура ПередЗакрытием()\n\tОповещение = Новый ОписаниеОповещения(\"НетТакогоОбработчика\", ЭтотОбъект);\nКонецПроцедуры\n";
        let diagnostics = run_local(source);
        expect![[r#"
            MissingNotifyHandler @ 3:15..3:75
              message: Создайте экспортную процедуру "НетТакогоОбработчика" или исправьте имя обработчика оповещения
              severity: Blocker"#]]
        .assert_eq(&format_diags(&client_context_source(source), &diagnostics));
    }

    #[test]
    fn empty_literal_handler_names_are_reported() {
        let sources = [
            "Процедура Тест()\n    О = Новый ОписаниеОповещения(\"\", ЭтотОбъект);\nКонецПроцедуры\n",
            "Процедура Тест()\n    О = Новый ОписаниеОповещения(\"Готово\", ЭтотОбъект, Неопределено, \"\", ЭтотОбъект);\nКонецПроцедуры\nПроцедура Готово() Экспорт\nКонецПроцедуры\n",
            "Процедура Тест()\n    О = Новый CallbackDescription(\"\", ThisForm);\nКонецПроцедуры\n",
        ];
        for source in sources {
            let diagnostics = run_local(source);
            assert_eq!(diagnostics.len(), 1, "{source}\n{diagnostics:#?}");
            assert_eq!(
                diagnostics[0].message,
                "Создайте экспортную процедуру \"\" или исправьте имя обработчика оповещения"
            );
        }

        let module = "Процедура Готово() Экспорт\nКонецПроцедуры\n";
        for source in [
            "Процедура Тест()\n    О = Новый ОписаниеОповещения(\"\", МойМодульКлиент);\nКонецПроцедуры\n",
            "Процедура Тест()\n    О = Новый ОписаниеОповещения(\"Готово\", МойМодульКлиент, Неопределено, \"\", МойМодульКлиент);\nКонецПроцедуры\n",
        ] {
            let diagnostics = with_module(source, module);
            assert_eq!(diagnostics.len(), 1, "{source}\n{diagnostics:#?}");
            assert_eq!(
                diagnostics[0].message,
                "Создайте экспортную процедуру \"МойМодульКлиент.\" или исправьте имя обработчика оповещения"
            );
        }
    }

    #[test]
    fn a_registration_inside_a_comment_is_ignored() {
        let source = "Процедура ПередЗакрытием()\n\t// Оповещение = Новый ОписаниеОповещения(\"Пример\", ЭтотОбъект);\nКонецПроцедуры\n\n// Процедура Пример(Результат, Параметры) Экспорт\n// КонецПроцедуры\n";
        let diagnostics = run_local(source);
        assert!(diagnostics.is_empty(), "a comment is not a registration: {diagnostics:#?}");
    }

    fn module_source() -> &'static str {
        "Процедура ПередЗакрытием()\n\tОповещение = Новый ОписаниеОповещения(\"Готово\", МойМодульКлиент);\nКонецПроцедуры\n"
    }

    #[test]
    fn an_exported_handler_of_a_common_module_is_silent() {
        let diagnostics = with_module(
            module_source(),
            "Процедура Готово(Результат, Параметры) Экспорт\nКонецПроцедуры\n",
        );
        assert!(diagnostics.is_empty(), "an exported handler must pass: {diagnostics:#?}");
    }

    /// Control for the case above: the module differs by the single word `Экспорт`.
    #[test]
    fn a_non_exported_handler_of_a_common_module_is_reported() {
        let diagnostics = with_module(
            module_source(),
            "Процедура Готово(Результат, Параметры)\nКонецПроцедуры\n",
        );
        expect![[r#"
            MissingNotifyHandler @ 3:15..3:66
              message: Добавьте "Экспорт" процедуре "МойМодульКлиент.Готово" - обработчик оповещения вызывается платформой только как экспортный
              severity: Blocker"#]]
        .assert_eq(&format_diags(&client_context_source(module_source()), &diagnostics));
    }

    #[test]
    fn a_missing_handler_of_a_known_common_module_is_reported() {
        let diagnostics = with_module(
            module_source(),
            "Процедура Другая(Результат, Параметры) Экспорт\nКонецПроцедуры\n",
        );
        assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
        assert_eq!(
            diagnostics[0].message,
            "Создайте экспортную процедуру \"МойМодульКлиент.Готово\" или исправьте имя обработчика оповещения"
        );
    }

    #[test]
    fn common_module_handler_is_checked_in_constructor_available_environments() {
        let diagnostics = with_module(
            module_source(),
            "#Если Клиент Тогда\n&НаКлиенте\nПроцедура Готово() Экспорт\nКонецПроцедуры\n#КонецЕсли\n",
        );
        assert!(
            diagnostics.is_empty(),
            "client handler should satisfy a client registration: {diagnostics:#?}"
        );

        let server_source = "&НаКлиентеНаСервере\nПроцедура ПередЗакрытием()\n    О = Новый ОписаниеОповещения(\"Готово\", МойМодульКлиент);\nКонецПроцедуры\n";
        let diagnostics = with_module(
            server_source,
            "#Если Клиент Тогда\n&НаКлиенте\nПроцедура Готово() Экспорт\nКонецПроцедуры\n#КонецЕсли\n",
        );
        assert!(
            diagnostics.is_empty(),
            "the server-incompatible constructor has no server registration: {diagnostics:#?}"
        );
    }

    #[test]
    fn common_module_conditional_declarations_can_jointly_cover_client_and_server() {
        let source = "&НаКлиентеНаСервере\nПроцедура ПередЗакрытием()\n#Если Клиент Тогда\n    А = Новый ОписаниеОповещения(\"Готово\", МойМодульКлиент);\n#КонецЕсли\n#Если Сервер Тогда\n    Б = Новый ОписаниеОповещения(\"Готово\", МойМодульКлиент);\n#КонецЕсли\nКонецПроцедуры\n";
        let module = "#Если Клиент Тогда\n&НаКлиенте\nПроцедура Готово() Экспорт\nКонецПроцедуры\n#КонецЕсли\n#Если Сервер Тогда\n&НаСервере\nПроцедура Готово() Экспорт\nКонецПроцедуры\n#КонецЕсли\n";
        let diagnostics = with_module(source, module);
        assert!(
            diagnostics.is_empty(),
            "complementary declarations should cover both registrations: {diagnostics:#?}"
        );
    }

    #[test]
    fn uncertain_common_module_declaration_keeps_the_verdict_unknown() {
        let module = "#Если НеизвестныйСимвол Тогда\n&НаКлиенте\nПроцедура Готово() Экспорт\nКонецПроцедуры\n#КонецЕсли\n";
        let diagnostics = with_module(module_source(), module);
        assert!(
            diagnostics.is_empty(),
            "an undecidable declaration cannot prove absence or failure: {diagnostics:#?}"
        );
    }

    #[test]
    fn expected_common_module_bodies_are_combined_by_environment_and_priority() {
        fn run_with_extension_body(
            source: &str,
            base_body: &str,
            extension_body: &str,
        ) -> Vec<Diagnostic> {
            use crate::test_utils::check_cfe_at_with_db;

            let mut builder = CfeFixtureBuilder::new("");
            builder
                .add_base_module("МойМодульКлиент", base_body)
                .add_extension("Расш", "")
                .add_extension_module("Расш", "МойМодульКлиент", extension_body);
            let source = client_context_source(source);
            check_cfe_at_with_db(
                CALLER,
                &source,
                builder.build(),
                &[],
                crate::DiagnosticsConfig::all_enabled(),
                |fixture| {
                    let base_module = fixture.root().join("CommonModules/МойМодульКлиент.xml");
                    enable_managed_client_module(&base_module);
                    let extension = fixture.extensions()[0].root();
                    let ext_modules = extension.join("CommonModules");
                    std::fs::create_dir_all(&ext_modules).unwrap();
                    std::fs::copy(&base_module, ext_modules.join("МойМодульКлиент.xml")).unwrap();
                    let ext_body = ext_modules.join("МойМодульКлиент/Ext/Module.bsl");
                    std::fs::create_dir_all(ext_body.parent().unwrap()).unwrap();
                    std::fs::write(ext_body, extension_body).unwrap();
                    let form = extension.join("CommonForms/Форма1/Ext/Form.xml");
                    std::fs::create_dir_all(form.parent().unwrap()).unwrap();
                    std::fs::write(form, PLAIN_FORM_XML).unwrap();
                },
                |_, ctx| check(ctx),
            )
        }

        let source = module_source();
        let client_only_base =
            "#Если Сервер Тогда\n&НаСервере\nПроцедура Готово()\nКонецПроцедуры\n#КонецЕсли\n";
        let client_export = "#Если Клиент Тогда\n&НаКлиенте\nПроцедура Готово() Экспорт\nКонецПроцедуры\n#КонецЕсли\n";
        assert!(
            run_with_extension_body(source, client_only_base, client_export).is_empty(),
            "the extension body supplies the client-only handler"
        );

        let client_non_export =
            "#Если Клиент Тогда\n&НаКлиенте\nПроцедура Готово()\nКонецПроцедуры\n#КонецЕсли\n";
        let diagnostics = run_with_extension_body(source, client_non_export, client_export);
        assert_eq!(
            diagnostics.len(),
            1,
            "base declaration takes priority where both exist: {diagnostics:#?}"
        );
        assert!(diagnostics[0].message.starts_with("Добавьте"));
    }

    #[test]
    fn server_preprocessor_position_is_excluded_when_constructor_is_unavailable() {
        let source = "&НаКлиентеНаСервере\nПроцедура ПередЗакрытием()\n#Если Клиент Тогда\n    А = Новый ОписаниеОповещения(\"Готово\", МойМодульКлиент);\n#КонецЕсли\n#Если Сервер Тогда\n    Б = Новый ОписаниеОповещения(\"Готово\", МойМодульКлиент);\n#КонецЕсли\nКонецПроцедуры\n";
        let module = "#Если Клиент Тогда\n&НаКлиенте\nПроцедура Готово() Экспорт\nКонецПроцедуры\n#КонецЕсли\n";
        let diagnostics = with_module(source, module);
        assert!(diagnostics.is_empty(), "{diagnostics:#?}");
    }

    fn async_local_source(export: &str) -> String {
        format!(
            "Процедура ПередЗакрытием()
	Оповещение = Новый ОписаниеОповещения(\"ОбработчикЗавершения\", ЭтотОбъект);
КонецПроцедуры

Асинх Процедура ОбработчикЗавершения(Результат, Параметры){export}
КонецПроцедуры
"
        )
    }

    #[test]
    fn an_async_exported_local_handler_is_silent() {
        let diagnostics = run_local(&async_local_source(" Экспорт"));
        assert!(diagnostics.is_empty(), "an async exported handler must pass: {diagnostics:#?}");
    }

    /// Control for the case above: only `Экспорт` is removed.
    #[test]
    fn an_async_local_handler_without_export_is_still_reported() {
        let diagnostics = run_local(&async_local_source(""));
        assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
        assert!(diagnostics[0].message.starts_with("Добавьте \"Экспорт\""), "{diagnostics:#?}");
    }

    #[test]
    fn an_async_exported_handler_of_a_common_module_is_silent() {
        let diagnostics = with_module(
            module_source(),
            "Асинх Процедура Готово(Результат, Параметры) Экспорт
КонецПроцедуры
",
        );
        assert!(diagnostics.is_empty(), "an async exported handler must pass: {diagnostics:#?}");
    }

    #[test]
    fn an_async_handler_of_a_common_module_without_export_is_still_reported() {
        let diagnostics = with_module(
            module_source(),
            "Асинх Процедура Готово(Результат, Параметры)
КонецПроцедуры
",
        );
        assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
        assert!(diagnostics[0].message.starts_with("Добавьте \"Экспорт\""), "{diagnostics:#?}");
    }

    #[test]
    fn a_parameter_named_like_a_common_module_is_not_that_module() {
        let source = "Процедура Тест(МойМодульКлиент)\n\tОповещение = Новый ОписаниеОповещения(\"Готово\", МойМодульКлиент);\nКонецПроцедуры\n";
        let diagnostics = with_module(source, "Процедура Другая() Экспорт\nКонецПроцедуры\n");
        assert!(diagnostics.is_empty(), "the parameter is the receiver: {diagnostics:#?}");
    }

    #[test]
    fn local_bindings_named_like_a_common_module_are_not_that_module() {
        let sources = [
            "Процедура Тест()\n    Перем МойМодульКлиент;\n    О = Новый ОписаниеОповещения(\"Готово\", моймодульклиент);\nКонецПроцедуры\n",
            "Процедура Тест()\n    МойМодульКлиент = Новый Структура;\n    О = Новый ОписаниеОповещения(\"Готово\", МойМодульКлиент);\nКонецПроцедуры\n",
            "Процедура Тест()\n    Для Каждого МойМодульКлиент Из Массив Цикл\n        О = Новый ОписаниеОповещения(\"Готово\", МойМодульКлиент);\n    КонецЦикла;\nКонецПроцедуры\n",
            "Перем МойМодульКлиент;\nПроцедура Тест()\n    О = Новый ОписаниеОповещения(\"Готово\", МойМодульКлиент);\nКонецПроцедуры\n",
        ];
        for source in sources {
            let diagnostics = with_module(source, "Процедура Другая() Экспорт\nКонецПроцедуры\n");
            assert!(diagnostics.is_empty(), "a nearer binding owns the receiver: {diagnostics:#?}");
        }
    }

    #[test]
    fn a_parameter_in_another_method_does_not_shadow_the_common_module() {
        let source = "Процедура Другая(МойМодульКлиент)\nКонецПроцедуры\nПроцедура Тест()\n    О = Новый ОписаниеОповещения(\"Готово\", МойМодульКлиент);\nКонецПроцедуры\n";
        let diagnostics = with_module(source, "Процедура Другая() Экспорт\nКонецПроцедуры\n");
        assert_eq!(diagnostics.len(), 1, "the common module remains visible: {diagnostics:#?}");
    }

    #[test]
    fn registrations_for_different_handlers_keep_their_own_verdicts() {
        let source = "Процедура Тест()\n    А = Новый ОписаниеОповещения(\"Готово\", МойМодульКлиент);\n    Б = Новый ОписаниеОповещения(\"Нет\", МойМодульКлиент);\n    В = Новый ОписаниеОповещения(\"БезЭкспорта\", МойМодульКлиент);\nКонецПроцедуры\n";
        let module =
            "Процедура Готово() Экспорт\nКонецПроцедуры\nПроцедура БезЭкспорта()\nКонецПроцедуры\n";
        let diagnostics = with_module(source, module);
        assert_eq!(diagnostics.len(), 2, "{diagnostics:#?}");
        assert!(diagnostics.iter().any(|d| d.message.starts_with("Создайте")));
        assert!(diagnostics.iter().any(|d| d.message.starts_with("Добавьте")));
    }

    #[test]
    fn a_local_assignment_shadows_both_earlier_and_later_registrations() {
        let source = "Процедура Тест()\n    О = Новый ОписаниеОповещения(\"Готово\", МойМодульКлиент);\n    МойМодульКлиент = Новый Структура;\n    О = Новый ОписаниеОповещения(\"Готово\", МойМодульКлиент);\nКонецПроцедуры\n";
        let diagnostics = with_module(source, "Процедура Другая() Экспорт\nКонецПроцедуры\n");
        assert!(diagnostics.is_empty(), "the receiver is a body-wide local: {diagnostics:#?}");
    }

    #[test]
    fn a_form_attribute_shadows_the_module_only_in_context() {
        let module_xml = r#"<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
<CommonModule uuid="00000000-0000-0000-0000-000000000101"><Properties>
<Name>Модуль</Name><Server>true</Server></Properties></CommonModule></MetaDataObject>"#;
        let form_xml = r#"<Form xmlns="http://v8.1c.ru/8.3/xcf/logform" xmlns:v8="http://v8.1c.ru/8.1/data/core" xmlns:xs="http://www.w3.org/2001/XMLSchema" version="2.10">
<Attributes><Attribute name="Модуль" id="2"><Type><v8:Type>xs:decimal</v8:Type></Type></Attribute></Attributes></Form>"#;
        let count = |directive: &str| {
            let source = format!(
                "{directive}\nПроцедура Тест()\n    О = Новый ОписаниеОповещения(\"Готово\", Модуль);\nКонецПроцедуры\n"
            );
            crate::test_utils::check_form_with_common_modules_and_form_xml(
                &source,
                &[("Модуль", "Процедура Другая() Экспорт\nКонецПроцедуры\n", module_xml)],
                form_xml,
            )
            .into_iter()
            .filter(|d| d.code == DiagnosticCode::MissingNotifyHandler)
            .count()
        };
        assert_eq!(count("&НаКлиентеНаСервере"), 0);
        assert_eq!(count("&НаКлиентеНаСервереБезКонтекста"), 1);

        let source = "&НаКлиентеНаСервере\nПроцедура СКонтекстом()\n    О = Новый ОписаниеОповещения(\"Готово\", Модуль);\nКонецПроцедуры\n&НаКлиентеНаСервереБезКонтекста\nПроцедура БезКонтекста()\n    О = Новый ОписаниеОповещения(\"Готово\", Модуль);\nКонецПроцедуры\n";
        let diagnostics = crate::test_utils::check_form_with_common_modules_and_form_xml(
            source,
            &[("Модуль", "Процедура Другая() Экспорт\nКонецПроцедуры\n", module_xml)],
            form_xml,
        )
        .into_iter()
        .filter(|d| d.code == DiagnosticCode::MissingNotifyHandler)
        .collect::<Vec<_>>();
        assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
        assert!(usize::from(diagnostics[0].range.start()) > source.find("БезКонтекста").unwrap());
    }

    #[test]
    fn an_unknown_receiver_module_proves_nothing() {
        let diagnostics = run_local(module_source());
        assert!(
            diagnostics.is_empty(),
            "absence in a module the analysis cannot see is not provable: {diagnostics:#?}"
        );
    }

    #[test]
    fn an_unmapped_module_body_prevents_a_verdict() {
        use crate::test_utils::check_cfe_at_with_db;

        fn run_with_body(base_body: &str, missing_extension_body: bool) -> Vec<Diagnostic> {
            let mut builder = CfeFixtureBuilder::new("");
            builder.add_base_module("МойМодульКлиент", base_body).add_extension("Расш", "");
            let source = client_context_source(module_source());
            check_cfe_at_with_db(
                CALLER,
                &source,
                builder.build(),
                &[],
                crate::DiagnosticsConfig::all_enabled(),
                |fixture| {
                    let extension = fixture.extensions()[0].root();
                    let form = extension.join("CommonForms/Форма1/Ext/Form.xml");
                    std::fs::create_dir_all(form.parent().unwrap()).unwrap();
                    std::fs::write(form, PLAIN_FORM_XML).unwrap();
                    if missing_extension_body {
                        let ext_modules = extension.join("CommonModules");
                        std::fs::create_dir_all(&ext_modules).unwrap();
                        std::fs::copy(
                            fixture.root().join("CommonModules/МойМодульКлиент.xml"),
                            ext_modules.join("МойМодульКлиент.xml"),
                        )
                        .unwrap();
                        let body_dir = ext_modules.join("МойМодульКлиент/Ext");
                        std::fs::create_dir_all(&body_dir).unwrap();
                        std::fs::write(
                            body_dir.join("Module.bsl"),
                            "Процедура Готово() Экспорт\nКонецПроцедуры\n",
                        )
                        .unwrap();
                    }
                },
                |_, ctx| {
                    let bodies = ctx.common_module_bodies("МойМодульКлиент");
                    assert!(!bodies.is_empty());
                    assert_eq!(bodies.has_missing_expected_body(), missing_extension_body);
                    check(ctx)
                },
            )
        }

        for body in
            ["Процедура Другая() Экспорт\nКонецПроцедуры\n", "Процедура Готово()\nКонецПроцедуры\n"]
        {
            assert_eq!(run_with_body(body, false).len(), 1);
            let diagnostics = run_with_body(body, true);
            assert!(
                diagnostics.is_empty(),
                "an unmapped body may define the handler: {diagnostics:#?}"
            );
        }
    }

    #[test]
    fn a_handler_registered_through_callback_description_is_reported_like_the_old_name() {
        let source = "Процедура ПередЗакрытием()
	Оповещение = Новый CallbackDescription(\"НетТакогоОбработчика\", ЭтотОбъект);
КонецПроцедуры
";
        let diagnostics = run_local(source);
        assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
        assert_eq!(
            diagnostics[0].message,
            "Создайте экспортную процедуру \"НетТакогоОбработчика\" или исправьте имя обработчика оповещения"
        );
    }

    /// Control for the case above: the same registration with an exported handler.
    #[test]
    fn an_exported_handler_registered_through_callback_description_is_silent() {
        let source = "Процедура ПередЗакрытием()
	Оповещение = Новый CallbackDescription(\"Готово\", ЭтотОбъект);
КонецПроцедуры

Процедура Готово(Результат, Параметры) Экспорт
КонецПроцедуры
";
        let diagnostics = run_local(source);
        assert!(diagnostics.is_empty(), "an exported handler must pass: {diagnostics:#?}");
    }

    #[test]
    fn a_parameter_named_like_this_form_is_not_the_form() {
        let source = "Процедура Тест(ЭтаФорма)
	Оповещение = Новый ОписаниеОповещения(\"НетТакогоОбработчика\", ЭтаФорма);
КонецПроцедуры
";
        let diagnostics = run_local(source);
        assert!(diagnostics.is_empty(), "the receiver is a parameter: {diagnostics:#?}");
    }

    #[test]
    fn self_receivers_shadowed_by_parameters_or_later_assignments_are_not_judged() {
        for receiver in ["ЭтотОбъект", "ЭтаФорма", "ThisObject", "ThisForm"] {
            let unshadowed = format!(
                "Процедура Тест()\n    О = Новый ОписаниеОповещения(\"Нет\", {receiver});\nКонецПроцедуры\n"
            );
            assert_eq!(run_local(&unshadowed).len(), 1, "{receiver}");
            let parameter = format!(
                "Процедура Тест({receiver})\n    О = Новый ОписаниеОповещения(\"Нет\", {receiver});\nКонецПроцедуры\n"
            );
            let assigned_later = format!(
                "Процедура Тест()\n    О = Новый ОписаниеОповещения(\"Нет\", {receiver});\n    {receiver} = Неопределено;\nКонецПроцедуры\n"
            );
            for source in [parameter, assigned_later] {
                let diagnostics = run_local(&source);
                assert!(diagnostics.is_empty(), "{receiver}: {source}\n{diagnostics:#?}");
            }
        }
    }

    #[test]
    fn self_receivers_shadowed_by_module_variables_are_not_judged() {
        for (declaration, receiver) in [
            ("ЭтотОбъект", "этотобъект"),
            ("ЭтаФорма", "этаформа"),
            ("ThisObject", "thisobject"),
            ("ThisForm", "thisform"),
        ] {
            for args in [
                format!("\"Готово\", {receiver}"),
                format!("\"НеВажно\", Неопределено, Неопределено, \"Готово\", {receiver}"),
            ] {
                let source = format!(
                    "Перем {declaration};\n{declaration} = ОбщегоНазначения.ОбщийМодуль(\"Приемник\");\nПроцедура Тест()\n    О = Новый ОписаниеОповещения({args});\nКонецПроцедуры\n"
                );
                let diagnostics = run_local(&source);
                assert!(diagnostics.is_empty(), "{source}\n{diagnostics:#?}");
            }
        }
    }

    /// Control for the case above: without the parameter `ЭтаФорма` is the form.
    #[test]
    fn this_form_without_a_shadowing_parameter_is_still_checked() {
        let source = "Процедура Тест()
	Оповещение = Новый ОписаниеОповещения(\"НетТакогоОбработчика\", ЭтаФорма);
КонецПроцедуры
";
        let diagnostics = run_local(source);
        assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    }

    #[test]
    fn a_local_variable_named_like_a_common_module_is_not_the_module() {
        let source = "Процедура ПередЗакрытием()
	МойМодульКлиент = Новый Структура;
	Оповещение = Новый ОписаниеОповещения(\"Готово\", МойМодульКлиент);
КонецПроцедуры
";
        let diagnostics = with_module(
            source,
            "Процедура Другая(Результат, Параметры) Экспорт
КонецПроцедуры
",
        );
        assert!(diagnostics.is_empty(), "the receiver is a local: {diagnostics:#?}");
    }

    #[test]
    fn a_parameter_named_like_a_common_module_is_not_the_module() {
        let source = "Процедура Тест(МойМодульКлиент)
	Оповещение = Новый ОписаниеОповещения(\"Готово\", МойМодульКлиент);
КонецПроцедуры
";
        let diagnostics = with_module(
            source,
            "Процедура Другая(Результат, Параметры) Экспорт
КонецПроцедуры
",
        );
        assert!(diagnostics.is_empty(), "the receiver is a parameter: {diagnostics:#?}");
    }

    const BASE_FORM_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<Form xmlns="http://v8.1c.ru/8.3/xcf/logform" version="2.20">
    <Events/>
    <BaseForm version="2.17">
        <Events/>
    </BaseForm>
</Form>"#;

    const PLAIN_FORM_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<Form xmlns="http://v8.1c.ru/8.3/xcf/logform" version="2.20">
    <Events/>
</Form>"#;

    fn run_with_form_xml(
        source: &str,
        form_xml: &str,
        fixture: test_fixture::CfeFixture,
    ) -> Vec<Diagnostic> {
        let source = client_context_source(source);
        crate::test_utils::check_cfe_at_with_unreadable_config_and_setup(
            CALLER,
            &source,
            fixture,
            &[],
            crate::DiagnosticsConfig::all_enabled(),
            |fixture| {
                let path = fixture.root().join("CommonForms/Форма1/Ext/Form.xml");
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, form_xml).unwrap();
            },
            check,
        )
    }

    #[test]
    fn a_form_with_unreadable_type_is_not_judged() {
        let source = "&НаКлиенте\nПроцедура Тест()\n    О = Новый ОписаниеОповещения(\"НетОбработчика\", ЭтотОбъект);\n    П = Новый ОписаниеОповещения(\"НетВМодуле\", МойМодульКлиент);\n    Л = Новый ОписаниеОповещения(\"ЛокальныйЭкспорт\", ЭтотОбъект);\nКонецПроцедуры\n&НаКлиенте\nПроцедура ЛокальныйЭкспорт() Экспорт\nКонецПроцедуры\n";
        for malformed in [false, true] {
            let mut builder = CfeFixtureBuilder::new("");
            builder
                .add_base_module("МойМодульКлиент", "Процедура Другая() Экспорт\nКонецПроцедуры\n");
            let diagnostics = crate::test_utils::check_cfe_at_with_db(
                CALLER,
                source,
                builder.build(),
                &[],
                crate::DiagnosticsConfig::all_enabled(),
                |fixture| {
                    let form_dir = fixture.root().join("CommonForms/Форма1/Ext");
                    std::fs::create_dir_all(form_dir.join("Form")).unwrap();
                    if malformed {
                        std::fs::write(form_dir.join("Form.xml"), "<").unwrap();
                    }
                },
                |_, ctx| {
                    let metadata = ctx.module_metadata();
                    assert_eq!(metadata.module_type, bsl_metadata::ModuleType::FormModule);
                    assert!(metadata.form.is_none());
                    check(ctx)
                },
            );
            assert!(diagnostics.is_empty(), "malformed={malformed}: {diagnostics:#?}");
        }
    }

    /// A handler exported in the BASE form's module is not in this file's tree.
    #[test]
    fn a_local_handler_of_a_form_with_a_base_form_is_not_judged() {
        let source = "Процедура ПередЗакрытием()
	Оповещение = Новый ОписаниеОповещения(\"ЕстьВБазовойФорме\", ЭтотОбъект);
КонецПроцедуры
";
        let diagnostics =
            run_with_form_xml(source, BASE_FORM_XML, CfeFixtureBuilder::new("").build());
        assert!(diagnostics.is_empty(), "the base form's module is unseen: {diagnostics:#?}");
    }

    /// Control for the case above: the same form without `<BaseForm>`.
    #[test]
    fn a_local_handler_of_a_form_without_a_base_form_is_still_judged() {
        let source = "Процедура ПередЗакрытием()
	Оповещение = Новый ОписаниеОповещения(\"ЕстьВБазовойФорме\", ЭтотОбъект);
КонецПроцедуры
";
        let diagnostics =
            run_with_form_xml(source, PLAIN_FORM_XML, CfeFixtureBuilder::new("").build());
        assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    }

    /// The common-module verdict does not depend on this file's tree.
    #[test]
    fn a_common_module_handler_is_still_judged_in_a_form_with_a_base_form() {
        let mut builder = CfeFixtureBuilder::new("");
        builder.add_base_module(
            "МойМодульКлиент",
            "Процедура Готово(Результат, Параметры)
КонецПроцедуры
",
        );
        use crate::test_utils::check_cfe_at_with_db;

        let diagnostics = check_cfe_at_with_db(
            CALLER,
            &client_context_source(module_source()),
            builder.build(),
            &[],
            crate::DiagnosticsConfig::all_enabled(),
            |fixture| {
                let module_xml = fixture.root().join("CommonModules/МойМодульКлиент.xml");
                enable_managed_client_module(&module_xml);
                let form = fixture.root().join("CommonForms/Форма1/Ext/Form.xml");
                std::fs::create_dir_all(form.parent().unwrap()).unwrap();
                std::fs::write(form, BASE_FORM_XML).unwrap();
            },
            |_, ctx| check(ctx),
        );
        assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
        assert!(diagnostics[0].message.starts_with("Добавьте \"Экспорт\""), "{diagnostics:#?}");
    }

    #[test]
    fn a_module_variable_named_like_a_common_module_is_not_the_module() {
        let source = "Перем МойМодульКлиент;

Процедура ПередЗакрытием()
	Оповещение = Новый ОписаниеОповещения(\"Готово\", МойМодульКлиент);
КонецПроцедуры
";
        let diagnostics = with_module(
            source,
            "Процедура Другая(Результат, Параметры) Экспорт
КонецПроцедуры
",
        );
        assert!(diagnostics.is_empty(), "the receiver is a module variable: {diagnostics:#?}");
    }

    /// Control for the case above: the same registration without the `Перем`.
    #[test]
    fn the_same_registration_without_a_module_variable_is_still_checked() {
        let source = "Процедура ПередЗакрытием()
	Оповещение = Новый ОписаниеОповещения(\"Готово\", МойМодульКлиент);
КонецПроцедуры
";
        let diagnostics = with_module(
            source,
            "Процедура Другая(Результат, Параметры) Экспорт
КонецПроцедуры
",
        );
        assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    }
}
