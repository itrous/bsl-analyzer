use crate::define_metadata;
use crate::metadata::*;
use crate::{Diagnostic, DiagnosticCode, DiagnosticsContext};
use hir::{ModuleId, Name, NotifyTarget};

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

    let mut diagnostics = Vec::new();
    for reg in &summary.notify_regs {
        if reg.callback_name.as_str().is_empty() {
            continue;
        }
        let verdict = match &reg.target {
            NotifyTarget::ThisObject => {
                local_verdict(ctx, &reg.callback_name).map(|v| (v, reg.callback_name.to_string()))
            }
            NotifyTarget::Module(module_name) => {
                module_verdict(ctx, module_name, &reg.callback_name).map(|v| {
                    (v, format!("{}.{}", module_name.as_str(), reg.callback_name.as_str()))
                })
            }
            NotifyTarget::Unsupported => None,
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
fn module_verdict(ctx: &DiagnosticsContext, module_name: &Name, name: &Name) -> Option<Verdict> {
    ctx.resolve_common_module(module_name.as_str())?;

    let bodies = ctx.common_module_bodies(module_name.as_str());
    if bodies.is_empty() {
        return None;
    }

    let mut saw_non_export = false;
    match bodies.search_merged_surface(|module_file_id| {
        let tree = ctx.symbol_tree_for(ModuleId::new(module_file_id));
        let methods = tree.find_methods(name);
        if methods.iter().any(|m| m.is_export) {
            return Some(());
        }
        if !methods.is_empty() {
            saw_non_export = true;
        }
        None
    }) {
        hir::BodySearch::Found(()) => None,
        hir::BodySearch::Unread => None,
        hir::BodySearch::Absent => {
            Some(if saw_non_export { Verdict::NotExported } else { Verdict::NotFound })
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

    #[test]
    fn an_unknown_receiver_module_proves_nothing() {
        let diagnostics = run_local(module_source());
        assert!(
            diagnostics.is_empty(),
            "absence in a module the analysis cannot see is not provable: {diagnostics:#?}"
        );
    }
}
