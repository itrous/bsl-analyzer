use crate::define_metadata;
use crate::metadata::*;
use crate::{Diagnostic, DiagnosticCode, DiagnosticsContext};
use hir::{call_graph::CallerId, DefWithBodyId, ModuleId, Name, NotifyTarget};
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

    let summary = ctx.call_summary(ModuleId::new(ctx.file_id));
    if summary.notify_regs.is_empty() {
        return Vec::new();
    }

    // Whether the thick client demands `Экспорт` for these handlers has not been
    // measured (the rule is established on managed forms), so stay silent there.
    if ctx.is_ordinary_form_module() {
        return Vec::new();
    }

    // A borrowed form exposes its base module, which is not in this file's symbol tree.
    // Missing form metadata cannot disprove that the form is borrowed.
    let metadata = ctx.module_metadata();
    let local_unprovable = match metadata.form.as_deref() {
        Some(form) => form.has_base_form(),
        None if metadata.module_type == bsl_metadata::ModuleType::FormModule => {
            ctx.form_may_have_base().unwrap_or(true)
        }
        None => false,
    };

    let mut diagnostics = Vec::new();
    let mut module_verdicts = FxHashMap::default();
    for reg in &summary.notify_regs {
        let verdict = match &reg.target {
            NotifyTarget::ThisObject if local_unprovable => None,
            NotifyTarget::ThisObject => {
                local_verdict(ctx, &reg.callback_name).map(|v| (v, reg.callback_name.to_string()))
            }
            NotifyTarget::Module(module_name) => {
                let owner = match reg.caller {
                    CallerId::Method(key) => DefWithBodyId::Method(key),
                    CallerId::ModuleCode => DefWithBodyId::ModuleCode,
                };
                let key = (owner, module_name.clone(), reg.callback_name.clone());
                let verdict = *module_verdicts
                    .entry(key)
                    .or_insert_with(|| module_verdict(ctx, module_name, &reg.callback_name, owner));
                verdict.map(|v| {
                    (v, format!("{}.{}", module_name.as_str(), reg.callback_name.as_str()))
                })
            }
            NotifyTarget::Unsupported
            | NotifyTarget::BareName { .. }
            | NotifyTarget::Unresolved { .. }
            | NotifyTarget::NotCompiled { .. } => None,
        };
        let Some((verdict, detail)) = verdict else { continue };
        diagnostics.push(Diagnostic {
            code,
            message: message(verdict, &detail),
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

/// Handler named on `ЭтотОбъект`: it must be declared in this very module.
fn local_verdict(ctx: &DiagnosticsContext, name: &Name) -> Option<Verdict> {
    let tree = ctx.symbol_tree();
    let methods = tree.find_methods(name);
    if methods.is_empty() {
        Some(Verdict::NotFound)
    } else if methods.iter().any(|m| m.is_export) {
        None
    } else {
        Some(Verdict::NotExported)
    }
}

/// Handler named on a bare identifier. Only a receiver that is a known common module is
/// judged: any other identifier (a local variable, a module the analysis cannot see) says
/// nothing about where the handler lives, and neither does a body that could not be read.
fn module_verdict(
    ctx: &DiagnosticsContext,
    module_name: &Name,
    name: &Name,
    owner: DefWithBodyId,
) -> Option<Verdict> {
    if !ctx.nonlocal_common_module_receiver(owner, module_name)? {
        return None;
    }
    ctx.resolve_common_module(module_name.as_str())?;
    let bodies = ctx.common_module_bodies(module_name.as_str());
    if bodies.is_empty() || bodies.has_missing_expected_body() {
        return None;
    }

    match ctx.qualified_method_export(module_name, name)? {
        Ok(true) => None,
        Ok(false) => Some(Verdict::NotExported),
        Err(hir::QualifiedMethodError::NotFound) => Some(Verdict::NotFound),
        Err(hir::QualifiedMethodError::BodyUnread | hir::QualifiedMethodError::NotVisibleInConfigs) => {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::{check_cfe_at_with_unreadable, format_diags};
    use expect_test::expect;
    use test_fixture::CfeFixtureBuilder;

    const CALLER: &str = "CommonForms/Форма1/Ext/Form/Module.bsl";

    fn run(source: &str, fixture: test_fixture::CfeFixture) -> Vec<Diagnostic> {
        check_cfe_at_with_unreadable(CALLER, source, fixture, &[], check)
    }

    fn run_local(source: &str) -> Vec<Diagnostic> {
        run(source, CfeFixtureBuilder::new("").build())
    }

    fn with_module(source: &str, module_body: &str) -> Vec<Diagnostic> {
        let mut builder = CfeFixtureBuilder::new("");
        builder.add_base_module("МойМодульКлиент", module_body);
        run(source, builder.build())
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
            MissingNotifyHandler @ 2:15..2:75
              message: Добавьте "Экспорт" процедуре "ОбработчикЗавершения" - обработчик оповещения вызывается платформой только как экспортный
              severity: Blocker"#]]
        .assert_eq(&format_diags(&source, &diagnostics));
    }

    /// Control for the case above: the fixture differs by the single word `Экспорт`.
    #[test]
    fn the_same_local_handler_with_export_is_silent() {
        let source = local_source(" Экспорт");
        let diagnostics = run_local(&source);
        assert!(diagnostics.is_empty(), "an exported handler must pass: {diagnostics:#?}");
    }

    #[test]
    fn a_local_handler_that_does_not_exist_is_reported() {
        let source = "Процедура ПередЗакрытием()\n\tОповещение = Новый ОписаниеОповещения(\"НетТакогоОбработчика\", ЭтотОбъект);\nКонецПроцедуры\n";
        let diagnostics = run_local(source);
        expect![[r#"
            MissingNotifyHandler @ 2:15..2:75
              message: Создайте экспортную процедуру "НетТакогоОбработчика" или исправьте имя обработчика оповещения
              severity: Blocker"#]]
        .assert_eq(&format_diags(source, &diagnostics));
    }

    #[test]
    fn empty_literal_handler_names_are_reported() {
        let sources = [
            "Процедура Тест()\n    О = Новый ОписаниеОповещения(\"\", ЭтотОбъект);\nКонецПроцедуры\n",
            "Процедура Тест()\n    О = Новый ОписаниеОповещения(\"Готово\", ЭтотОбъект, Неопределено, \"\", ЭтотОбъект);\nКонецПроцедуры\nПроцедура Готово() Экспорт\nКонецПроцедуры\n",
            "Процедура Тест()\n    О = Новый(\"CallbackDescription\", \"\", ThisForm);\nКонецПроцедуры\n",
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
            MissingNotifyHandler @ 2:15..2:66
              message: Добавьте "Экспорт" процедуре "МойМодульКлиент.Готово" - обработчик оповещения вызывается платформой только как экспортный
              severity: Blocker"#]]
        .assert_eq(&format_diags(module_source(), &diagnostics));
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
        assert_eq!(count("&НаСервере"), 0);
        assert_eq!(count("&НаСервереБезКонтекста"), 1);

        let source = "&НаСервере\nПроцедура СКонтекстом()\n    О = Новый ОписаниеОповещения(\"Готово\", Модуль);\nКонецПроцедуры\n&НаСервереБезКонтекста\nПроцедура БезКонтекста()\n    О = Новый ОписаниеОповещения(\"Готово\", Модуль);\nКонецПроцедуры\n";
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
            check_cfe_at_with_db(
                CALLER,
                module_source(),
                builder.build(),
                &[],
                crate::DiagnosticsConfig::all_enabled(),
                |fixture| {
                    if missing_extension_body {
                        let ext_modules = fixture.extensions()[0].root().join("CommonModules");
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
        crate::test_utils::check_cfe_at_with_unreadable_config_and_setup(
            CALLER,
            source,
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
        let diagnostics = run_with_form_xml(module_source(), BASE_FORM_XML, builder.build());
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
