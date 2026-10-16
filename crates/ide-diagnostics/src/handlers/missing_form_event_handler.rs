//! A handler bound in the managed form's `Form.xml` — a form or item event, or a
//! form command's action — that has no procedure or function of that name in the
//! form module. The designer's handler-existence check reports it as «Отсутствует
//! обработчик»; the module itself compiles, so nothing else flags it.

use crate::define_metadata;
use crate::metadata::*;
use crate::{Diagnostic, DiagnosticCode};
use hir::{ModuleMetadata, Name};
use ide_db::TextRange;
use rustc_hash::FxHashSet;
use stdx::case::CaseExt;

pub const METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: DiagnosticType::Error,
    severity: DiagnosticSeverityLevel::Critical,
    scope: DiagnosticScope::Bsl,
    modules: &[bsl_metadata::ModuleType::FormModule],
    minutes_to_fix: 5,
    activated_by_default: true,
    compatibility_mode: DiagnosticCompatibilityMode::Undefined,
    tags: &[MetadataTag::Error],
    can_locate_on_project: false,
    extra_min_for_complexity: 0.0,
    lsp_severity_override: "",
};

pub fn from_metadata(
    metadata: &ModuleMetadata,
    ctx: &crate::DiagnosticsContext,
) -> Vec<Diagnostic> {
    let code = DiagnosticCode::MissingFormEventHandler;

    if ctx.is_disabled_with_metadata(code) {
        return Vec::new();
    }

    if metadata.module_type != bsl_metadata::ModuleType::FormModule {
        return Vec::new();
    }

    let Some(form) = metadata.form.as_deref() else {
        return Vec::new();
    };

    // An ordinary form's bindings live in the binary `Form.bin`: the handler list
    // of such a form is a guess, not a declaration. A form borrowed by an
    // extension binds the base form's handlers too, and those live in the base
    // module, not in this one.
    if !form.is_managed() || form.has_base_form() {
        return Vec::new();
    }

    let declared = form
        .event_handlers()
        .iter()
        // An event named by an identifier is not one the form has, and the
        // designer's handler-existence check passes over such bindings.
        .filter(|h| !h.is_unnamed_event())
        .map(|h| (h.handler_name.as_str(), Binding::Event(h.event_type.as_str())))
        .chain(form.command_handlers().iter().map(|h| (h.as_str(), Binding::CommandAction)));

    let symbol_tree = ctx.symbol_tree();
    let range = first_line_range(&ctx.file_text());
    let mut reported = FxHashSet::default();
    let mut diagnostics = Vec::new();

    for (handler, binding) in declared {
        // One procedure can serve several bindings; it is one missing procedure.
        if !reported.insert(handler.fold_lower()) {
            continue;
        }
        if symbol_tree.find_method(&Name::new(handler)).is_some() {
            continue;
        }
        let message = match binding {
            Binding::Event(event) if !event.is_empty() => format!(
                "Создайте процедуру \"{handler}\" или исправьте обработчик события \"{event}\" в форме \"{}\"",
                form.name()
            ),
            Binding::Event(_) => format!(
                "Создайте процедуру \"{handler}\" или исправьте обработчик события в форме \"{}\"",
                form.name()
            ),
            Binding::CommandAction => format!(
                "Создайте процедуру \"{handler}\" или исправьте действие команды в форме \"{}\"",
                form.name()
            ),
        };
        diagnostics.push(Diagnostic {
            code,
            message,
            severity: ctx.severity(code),
            range,
            tags: ctx.tags(code),
            fixes: vec![],
        });
    }

    diagnostics
}

#[derive(Clone, Copy)]
enum Binding<'a> {
    Event(&'a str),
    CommandAction,
}

/// The missing procedure has no place in the module, so the finding goes on the
/// module as a whole: its first line, or an empty range at the start of an empty
/// module.
fn first_line_range(text: &str) -> TextRange {
    let end = text.find(['\r', '\n']).unwrap_or(text.len());
    TextRange::new(0.into(), (end as u32).into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::format_diags;
    use expect_test::expect;
    use std::sync::Arc;

    fn form_xml(body: &str) -> bsl_metadata::Form {
        let xml = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<Form xmlns="http://v8.1c.ru/8.3/xcf/logform" version="2.17">
{body}
</Form>"#
        );
        let mut form = bsl_metadata::xml_parser::parse_form_xml(&xml).expect("form XML parses");
        form.name = "ФормаЭлемента".to_string();
        form
    }

    fn check(form: bsl_metadata::Form, module: &str) -> Vec<Diagnostic> {
        let metadata = ModuleMetadata {
            module_type: bsl_metadata::ModuleType::FormModule,
            execution_context: None,
            common_module: None,
            mdo: None,
            register: None,
            http_service: None,
            web_service: None,
            integration_service: None,
            form: Some(Arc::new(form)),
        };
        crate::test_utils::check_metadata_diagnostic(metadata, module, from_metadata)
    }

    const BINDINGS: &str = r#"<Events>
    <Event name="OnOpen">ПриОткрытии</Event>
</Events>
<ChildItems>
    <InputField name="Поле" id="1">
        <DataPath>Реквизит</DataPath>
        <Events>
            <Event name="OnChange">ПолеПриИзменении</Event>
            <Event name="StartChoice">ПолеНачалоВыбора</Event>
        </Events>
    </InputField>
</ChildItems>
<Commands>
    <Command name="Выполнить" id="1">
        <Action>ВыполнитьКоманду</Action>
    </Command>
</Commands>"#;

    #[test]
    fn bound_handlers_declared_in_the_module_are_silent() {
        let module = "&НаКлиенте\nПроцедура ПриОткрытии(Отказ)\nКонецПроцедуры\n\n\
&НаКлиенте\nПроцедура ПолеПриИзменении(Элемент)\nКонецПроцедуры\n\n\
&НаКлиенте\nПроцедура ПолеНачалоВыбора(Элемент, ДанныеВыбора, СтандартнаяОбработка)\nКонецПроцедуры\n\n\
&НаКлиенте\nПроцедура ВыполнитьКоманду(Команда)\nКонецПроцедуры\n";
        assert_eq!(check(form_xml(BINDINGS), module), vec![]);
    }

    #[test]
    fn a_form_event_an_item_event_and_a_command_action_without_a_procedure_are_reported() {
        let module = "&НаКлиенте\nПроцедура ПолеНачалоВыбора(Элемент, ДанныеВыбора, СтандартнаяОбработка)\nКонецПроцедуры\n";
        let diagnostics = check(form_xml(BINDINGS), module);
        expect![[r#"
            MissingFormEventHandler @ 1:1..1:11
              message: Создайте процедуру "ВыполнитьКоманду" или исправьте действие команды в форме "ФормаЭлемента"
              severity: Critical
            MissingFormEventHandler @ 1:1..1:11
              message: Создайте процедуру "ПолеПриИзменении" или исправьте обработчик события "OnChange" в форме "ФормаЭлемента"
              severity: Critical
            MissingFormEventHandler @ 1:1..1:11
              message: Создайте процедуру "ПриОткрытии" или исправьте обработчик события "OnOpen" в форме "ФормаЭлемента"
              severity: Critical"#]]
        .assert_eq(&format_diags(module, &diagnostics));
    }

    #[test]
    fn a_handler_that_is_a_function_or_differs_only_in_case_is_found() {
        let module = "&НаКлиенте\nПроцедура приоткрытии(Отказ)\nКонецПроцедуры\n\n\
&НаКлиенте\nФункция ПОЛЕПРИИЗМЕНЕНИИ(Элемент)\n\tВозврат Неопределено;\nКонецФункции\n\n\
#Если Клиент Тогда\n&НаКлиенте\nПроцедура ПолеНачалоВыбора(Элемент, ДанныеВыбора, СтандартнаяОбработка)\nКонецПроцедуры\n#КонецЕсли\n\n\
&НаКлиенте\nАсинх Процедура ВыполнитьКоманду(Команда)\nКонецПроцедуры\n";
        assert_eq!(check(form_xml(BINDINGS), module), vec![]);
    }

    #[test]
    fn an_empty_module_reports_every_binding_at_its_start() {
        let diagnostics = check(form_xml(BINDINGS), "");
        assert_eq!(diagnostics.len(), 4, "{diagnostics:?}");
        assert!(diagnostics.iter().all(|d| d.range == TextRange::empty(0.into())));
    }

    #[test]
    fn one_procedure_bound_twice_is_reported_once() {
        let body = r#"<ChildItems>
    <InputField name="Первое" id="1">
        <Events><Event name="OnChange">ПолеПриИзменении</Event></Events>
    </InputField>
    <InputField name="Второе" id="2">
        <Events><Event name="OnChange">полеприизменении</Event></Events>
    </InputField>
</ChildItems>"#;
        let diagnostics = check(form_xml(body), "Процедура Иное()\nКонецПроцедуры\n");
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    }

    /// A binding to an event the form does not have is written with the event's
    /// identifier for a name; the designer's check passes over it.
    #[test]
    fn a_binding_to_an_event_named_by_an_identifier_is_skipped() {
        let body = r#"<Events>
    <Event name="00000000-1111-2222-3333-444444444444">ПередЗаписьюНаСервере</Event>
    <Event name="OnOpen">ПриОткрытии</Event>
</Events>"#;
        let diagnostics = check(form_xml(body), "Процедура Иное()\nКонецПроцедуры\n");
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert!(diagnostics[0].message.contains("\"ПриОткрытии\""), "{diagnostics:?}");
    }

    /// A form borrowed by an extension carries `<BaseForm>`, and its bindings
    /// include the base form's handlers, which live in the base module.
    #[test]
    fn a_form_borrowed_by_an_extension_is_skipped() {
        let body = format!(
            "{BINDINGS}\n<BaseForm version=\"2.17\">\n<Events><Event name=\"OnOpen\">ПриОткрытии</Event></Events>\n</BaseForm>"
        );
        assert_eq!(check(form_xml(&body), "Процедура Иное()\nКонецПроцедуры\n"), vec![]);
    }

    #[test]
    fn an_ordinary_form_is_skipped() {
        let form = bsl_metadata::Form::ordinary_without_dialog("Форма");
        assert!(!form.event_handlers().is_empty());
        assert_eq!(check(form, "Процедура Иное()\nКонецПроцедуры\n"), vec![]);
    }

    #[test]
    fn the_rule_can_be_switched_off() {
        let metadata = ModuleMetadata {
            module_type: bsl_metadata::ModuleType::FormModule,
            execution_context: None,
            common_module: None,
            mdo: None,
            register: None,
            http_service: None,
            web_service: None,
            integration_service: None,
            form: Some(Arc::new(form_xml(BINDINGS))),
        };
        let mut config = crate::DiagnosticsConfig::all_enabled();
        config.disabled.push(DiagnosticCode::MissingFormEventHandler);
        let diagnostics = crate::test_utils::check_metadata_diagnostic_with_config(
            metadata,
            "",
            config,
            from_metadata,
        );
        assert_eq!(diagnostics, vec![]);
    }
}
