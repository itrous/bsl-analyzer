//! A form event handler declared with a compilation directive that does not compile it into
//! the context the event requires is silently not bound. Measured on platform 8.3.27.2214
//! (managed form, handler named in `Form.xml`): there is no build error, no warning and no
//! runtime exception - the handler simply never runs. Only the combinations verified by a
//! live probe are reported; everything else is deliberately left alone. `&НаКлиентеНаСервере`
//! is covered by `AtClientAtServerFormMethod` and is not reported here a second time.

use crate::define_metadata;
use crate::metadata::*;
use crate::{Diagnostic, DiagnosticCode, DiagnosticsContext};
use bsl_metadata::{FormType, ModuleType};
use hir::AnnotationKind;
use ide_db::TextRange;
use std::collections::HashMap;
use stdx::case::CaseExt;

pub const METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: DiagnosticType::Error,
    severity: DiagnosticSeverityLevel::Critical,
    scope: DiagnosticScope::Bsl,
    modules: &[bsl_metadata::ModuleType::FormModule],
    minutes_to_fix: 5,
    activated_by_default: true,
    compatibility_mode: DiagnosticCompatibilityMode::Undefined,
    tags: &[MetadataTag::Error, MetadataTag::Suspicious],
    can_locate_on_project: false,
    extra_min_for_complexity: 0.0,
    lsp_severity_override: "warning",
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RequiredContext {
    Server,
    Client,
    ServerNoContext,
}

impl RequiredContext {
    fn directive_text(self) -> &'static str {
        match self {
            Self::Server => "&НаСервере",
            Self::Client => "&НаКлиенте",
            Self::ServerNoContext => "&НаСервереБезКонтекста",
        }
    }
}

/// The directive a method actually carries. `None` is a distinct case: no directive at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActualDirective {
    None,
    Server,
    Client,
    ServerNoContext,
    ClientServerNoContext,
    ClientServer,
}

impl ActualDirective {
    fn declared_text(self) -> &'static str {
        match self {
            Self::None => "без директивы компиляции",
            Self::Server => "с директивой &НаСервере",
            Self::Client => "с директивой &НаКлиенте",
            Self::ServerNoContext => "с директивой &НаСервереБезКонтекста",
            Self::ClientServerNoContext => "с директивой &НаКлиентеНаСервереБезКонтекста",
            Self::ClientServer => "с директивой &НаКлиентеНаСервере",
        }
    }
}

/// Event name (as written in `Form.xml`) -> context its handler must be compiled for.
static EVENT_TABLE: &[(&str, RequiredContext)] = &[
    ("AdditionalDetailProcessing", RequiredContext::Client),
    ("AfterDeleteRow", RequiredContext::Client),
    ("AfterWrite", RequiredContext::Client),
    ("AfterWriteAtServer", RequiredContext::Server),
    ("AutoComplete", RequiredContext::Client),
    ("BeforeAddRow", RequiredContext::Client),
    ("BeforeClose", RequiredContext::Client),
    ("BeforeCollapse", RequiredContext::Client),
    ("BeforeCreate", RequiredContext::Client),
    ("BeforeDelete", RequiredContext::Client),
    ("BeforeDeleteRow", RequiredContext::Client),
    ("BeforeEditEnd", RequiredContext::Client),
    ("BeforeExpand", RequiredContext::Client),
    ("BeforeLoadDataFromSettingsAtServer", RequiredContext::Server),
    ("BeforeLoadUserSettingsAtServer", RequiredContext::Server),
    ("BeforeLoadVariantAtServer", RequiredContext::Server),
    ("BeforeRowChange", RequiredContext::Client),
    ("BeforeStartEdit", RequiredContext::Client),
    ("BeforeStartQuickEdit", RequiredContext::Client),
    ("BeforeWrite", RequiredContext::Client),
    ("BeforeWriteAtServer", RequiredContext::Server),
    ("ChoiceProcessing", RequiredContext::Client),
    ("Clearing", RequiredContext::Client),
    ("Click", RequiredContext::Client),
    ("Creating", RequiredContext::Client),
    ("DetailProcessing", RequiredContext::Client),
    ("DocumentComplete", RequiredContext::Client),
    ("Drag", RequiredContext::Client),
    ("DragCheck", RequiredContext::Client),
    ("DragEnd", RequiredContext::Client),
    ("DragStart", RequiredContext::Client),
    ("EditTextChange", RequiredContext::Client),
    ("ExternalEvent", RequiredContext::Client),
    ("FillCheckProcessingAtServer", RequiredContext::Server),
    ("NavigationProcessing", RequiredContext::Client),
    ("NewWriteProcessing", RequiredContext::Client),
    ("NotificationProcessing", RequiredContext::Client),
    ("OnActivate", RequiredContext::Client),
    ("OnActivateCell", RequiredContext::Client),
    ("OnActivateDate", RequiredContext::Client),
    ("OnActivateField", RequiredContext::Client),
    ("OnActivateRow", RequiredContext::Client),
    ("OnChange", RequiredContext::Client),
    ("OnChangeAreaContent", RequiredContext::Client),
    ("OnChangeDisplaySettings", RequiredContext::Client),
    ("OnClick", RequiredContext::Client),
    ("OnClose", RequiredContext::Client),
    ("OnCreateAtServer", RequiredContext::Server),
    ("OnCurrentPageChange", RequiredContext::Client),
    ("OnCurrentRepresentationPeriodChange", RequiredContext::Client),
    ("OnEditEnd", RequiredContext::Client),
    ("OnGetDataAtServer", RequiredContext::ServerNoContext),
    ("OnLoadDataFromSettingsAtServer", RequiredContext::Server),
    ("OnLoadUserSettingsAtServer", RequiredContext::Server),
    ("OnLoadVariantAtServer", RequiredContext::Server),
    ("OnOpen", RequiredContext::Client),
    ("OnPeriodOutput", RequiredContext::Client),
    ("OnReadAtServer", RequiredContext::Server),
    ("OnReopen", RequiredContext::Client),
    ("OnSaveDataInSettingsAtServer", RequiredContext::Server),
    ("OnSaveUserSettingsAtServer", RequiredContext::Server),
    ("OnSaveVariantAtServer", RequiredContext::Server),
    ("OnStartEdit", RequiredContext::Client),
    ("OnUpdateUserSettingSetAtServer", RequiredContext::Server),
    ("OnWriteAtServer", RequiredContext::Server),
    ("Opening", RequiredContext::Client),
    ("RefreshRequestProcessing", RequiredContext::Client),
    ("Selection", RequiredContext::Client),
    ("StartChoice", RequiredContext::Client),
    ("StartListChoice", RequiredContext::Client),
    ("TextEditEnd", RequiredContext::Client),
    ("Tuning", RequiredContext::Client),
    ("URLProcessing", RequiredContext::Client),
    ("ValueChoice", RequiredContext::Client),
];

type BoundEvents<'a> = HashMap<String, Vec<(&'a str, RequiredContext)>>;

pub fn check(ctx: &DiagnosticsContext) -> Vec<Diagnostic> {
    let code = DiagnosticCode::FormEventHandlerDirective;

    if ctx.is_disabled_with_metadata(code) {
        return Vec::new();
    }

    let metadata = ctx.module_metadata();

    if metadata.module_type != ModuleType::FormModule {
        return Vec::new();
    }

    let form = match &metadata.form {
        Some(form) if form.form_type == FormType::Managed => form,
        _ => return Vec::new(),
    };

    // A borrowed form names handlers that live in the base form's module.
    if form.has_base_form {
        return Vec::new();
    }

    // Handler name (case-folded) -> every table event bound to it.
    let mut bound: BoundEvents = HashMap::new();
    for handler in &form.event_handlers {
        let Some((event, required)) =
            EVENT_TABLE.iter().find(|(name, _)| *name == handler.event_type)
        else {
            continue;
        };
        bound.entry(handler.handler_name.fold_lower()).or_default().push((event, *required));
    }

    if bound.is_empty() {
        return Vec::new();
    }

    let item_tree = ctx.item_tree();
    let mut diagnostics = Vec::new();

    for (_, proc) in item_tree.procedures() {
        check_method(
            proc.name.as_str(),
            proc.name_range,
            &proc.annotations,
            &bound,
            code,
            ctx,
            &mut diagnostics,
        );
    }

    for (_, func) in item_tree.functions() {
        check_method(
            func.name.as_str(),
            func.name_range,
            &func.annotations,
            &bound,
            code,
            ctx,
            &mut diagnostics,
        );
    }

    diagnostics.sort_by_key(|d| d.range.start());
    diagnostics
}

fn check_method(
    name: &str,
    name_range: TextRange,
    annotations: &[hir::Annotation],
    bound: &BoundEvents,
    code: DiagnosticCode,
    ctx: &DiagnosticsContext,
    out: &mut Vec<Diagnostic>,
) {
    let Some(events) = bound.get(&name.fold_lower()) else {
        return;
    };

    let (event, required) = events[0];
    // Contexts disagree between the events bound to one procedure: do not guess.
    if events.iter().any(|(_, r)| *r != required) {
        return;
    }

    let actual = actual_directive(annotations);
    if !is_dead(required, actual) {
        return;
    }

    out.push(Diagnostic {
        code,
        message: format!(
            "Обработчик события «{}» объявлен {}: платформа такой обработчик не вызывает, требуется {}",
            event,
            actual.declared_text(),
            required.directive_text()
        ),
        severity: ctx.severity(code),
        range: name_range,
        tags: ctx.tags(code),
        fixes: vec![],
    });
}

fn actual_directive(annotations: &[hir::Annotation]) -> ActualDirective {
    for a in annotations {
        match a.kind {
            AnnotationKind::AtServer => return ActualDirective::Server,
            AnnotationKind::AtClient => return ActualDirective::Client,
            AnnotationKind::AtServerNoContext => return ActualDirective::ServerNoContext,
            AnnotationKind::AtClientAtServerNoContext => {
                return ActualDirective::ClientServerNoContext;
            }
            AnnotationKind::AtClientAtServer => return ActualDirective::ClientServer,
            _ => {}
        }
    }
    ActualDirective::None
}

/// Only the combinations verified by a live probe. `&НаКлиентеНаСервере` is never reported:
/// `AtClientAtServerFormMethod` already covers it.
fn is_dead(required: RequiredContext, actual: ActualDirective) -> bool {
    use ActualDirective as A;
    match required {
        RequiredContext::Server => {
            matches!(actual, A::ServerNoContext | A::Client | A::ClientServerNoContext)
        }
        RequiredContext::Client => matches!(actual, A::Server | A::None),
        RequiredContext::ServerNoContext => matches!(actual, A::Server | A::Client | A::None),
    }
}

#[cfg(test)]
mod tests {
    use crate::test_utils::format_diags;
    use expect_test::expect;
    use std::sync::Arc;

    fn form_metadata(events: &[(&str, &str)]) -> hir::ModuleMetadata {
        let mut xml = String::from(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<Form xmlns="http://v8.1c.ru/8.3/xcf/logform" version="2.20">
    <Events>
"#,
        );
        for (event, handler) in events {
            xml.push_str(&format!("        <Event name=\"{event}\">{handler}</Event>\n"));
        }
        xml.push_str("    </Events>\n</Form>");
        let form = bsl_metadata::xml_parser::parse_form_xml(&xml).unwrap();

        hir::ModuleMetadata {
            module_type: bsl_metadata::ModuleType::FormModule,
            execution_context: None,
            common_module: None,
            mdo: None,
            register: None,
            command: None,
            form: Some(Arc::new(form)),
            http_service: None,
            web_service: None,
            integration_service: None,
        }
    }

    fn run(metadata: hir::ModuleMetadata, code: &str) -> Vec<crate::Diagnostic> {
        crate::test_utils::check_metadata_diagnostic(metadata, code, |_, ctx| super::check(ctx))
    }

    fn proc_with(directive: &str, name: &str) -> String {
        if directive.is_empty() {
            format!("Процедура {name}()\nКонецПроцедуры\n")
        } else {
            format!("{directive}\nПроцедура {name}()\nКонецПроцедуры\n")
        }
    }

    #[test]
    fn server_event_with_server_or_no_directive_is_clean() {
        let events = [("OnCreateAtServer", "ПриСозданииНаСервере")];
        for directive in ["&НаСервере", ""] {
            let code = proc_with(directive, "ПриСозданииНаСервере");
            assert!(run(form_metadata(&events), &code).is_empty(), "directive: {directive:?}");
        }
    }

    #[test]
    fn server_event_with_client_directive_is_flagged() {
        let events = [("OnCreateAtServer", "ПриСозданииНаСервере")];
        let code = proc_with("&НаКлиенте", "ПриСозданииНаСервере");
        let diagnostics = run(form_metadata(&events), &code);
        assert_eq!(diagnostics.len(), 1);
        expect![[r#"
            FormEventHandlerDirective @ 2:11..2:31
              message: Обработчик события «OnCreateAtServer» объявлен с директивой &НаКлиенте: платформа такой обработчик не вызывает, требуется &НаСервере
              severity: Warning"#]]
        .assert_eq(&format_diags(&code, &diagnostics));
    }

    #[test]
    fn server_event_with_no_context_directives_is_flagged() {
        let events = [("OnCreateAtServer", "ПриСозданииНаСервере")];
        for directive in ["&НаСервереБезКонтекста", "&НаКлиентеНаСервереБезКонтекста"]
        {
            let code = proc_with(directive, "ПриСозданииНаСервере");
            assert_eq!(run(form_metadata(&events), &code).len(), 1, "directive: {directive}");
        }
    }

    #[test]
    fn client_event_with_server_directive_is_flagged() {
        let events = [("OnOpen", "ПриОткрытии")];
        let code = proc_with("&НаСервере", "ПриОткрытии");
        let diagnostics = run(form_metadata(&events), &code);
        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].message.ends_with("требуется &НаКлиенте"));
        assert!(diagnostics[0].message.contains("с директивой &НаСервере:"));
    }

    #[test]
    fn client_event_without_directive_is_flagged() {
        let events = [("OnOpen", "ПриОткрытии")];
        let code = proc_with("", "ПриОткрытии");
        let diagnostics = run(form_metadata(&events), &code);
        assert_eq!(diagnostics.len(), 1);
        expect![[r#"
            FormEventHandlerDirective @ 1:11..1:22
              message: Обработчик события «OnOpen» объявлен без директивы компиляции: платформа такой обработчик не вызывает, требуется &НаКлиенте
              severity: Warning"#]]
        .assert_eq(&format_diags(&code, &diagnostics));
    }

    #[test]
    fn client_event_with_client_directive_is_clean() {
        let events = [("OnOpen", "ПриОткрытии")];
        let code = proc_with("&НаКлиенте", "ПриОткрытии");
        assert!(run(form_metadata(&events), &code).is_empty());
    }

    #[test]
    fn client_event_with_unmeasured_directives_is_not_flagged() {
        let events = [("OnOpen", "ПриОткрытии")];
        for directive in ["&НаСервереБезКонтекста", "&НаКлиентеНаСервереБезКонтекста"]
        {
            let code = proc_with(directive, "ПриОткрытии");
            assert!(run(form_metadata(&events), &code).is_empty(), "directive: {directive}");
        }
    }

    #[test]
    fn no_context_event_requires_no_context_directive() {
        let events = [("OnGetDataAtServer", "ПриПолученииДанныхНаСервере")];
        for directive in ["&НаСервере", "&НаКлиенте", ""] {
            let code = proc_with(directive, "ПриПолученииДанныхНаСервере");
            let diagnostics = run(form_metadata(&events), &code);
            assert_eq!(diagnostics.len(), 1, "directive: {directive:?}");
            assert!(diagnostics[0].message.ends_with("требуется &НаСервереБезКонтекста"));
        }
        let code = proc_with("&НаСервереБезКонтекста", "ПриПолученииДанныхНаСервере");
        assert!(run(form_metadata(&events), &code).is_empty());
    }

    #[test]
    fn both_sides_directive_is_left_to_the_other_diagnostic() {
        let events = [("OnCreateAtServer", "ПриСозданииНаСервере"), ("OnOpen", "ПриОткрытии")];
        for name in ["ПриСозданииНаСервере", "ПриОткрытии"] {
            let code = proc_with("&НаКлиентеНаСервере", name);
            assert!(run(form_metadata(&events), &code).is_empty(), "handler: {name}");
        }
    }

    #[test]
    fn unbound_procedure_is_not_flagged() {
        let events = [("OnCreateAtServer", "ПриСозданииНаСервере")];
        let code = proc_with("&НаКлиенте", "ПосторонняяПроцедура");
        assert!(run(form_metadata(&events), &code).is_empty());
    }

    #[test]
    fn unknown_event_is_not_flagged() {
        let events = [("SomethingUnmeasured", "МойОбработчик")];
        let code = proc_with("&НаКлиенте", "МойОбработчик");
        assert!(run(form_metadata(&events), &code).is_empty());
    }

    #[test]
    fn handler_name_is_compared_case_insensitively() {
        let events = [("OnCreateAtServer", "ПриСозданииНаСервере")];
        let code = proc_with("&НаКлиенте", "приСозданииНАсервере");
        assert_eq!(run(form_metadata(&events), &code).len(), 1);
    }

    #[test]
    fn function_handler_is_flagged_on_name() {
        let events = [("OnOpen", "ПриОткрытии")];
        let code = "&НаСервере\nФункция ПриОткрытии()\n\tВозврат 1;\nКонецФункции\n";
        assert_eq!(run(form_metadata(&events), code).len(), 1);
    }

    #[test]
    fn handler_bound_to_events_with_different_contexts_is_skipped() {
        let events = [("OnCreateAtServer", "Общий"), ("OnOpen", "Общий")];
        let code = proc_with("&НаКлиенте", "Общий");
        assert!(run(form_metadata(&events), &code).is_empty());
    }

    #[test]
    fn borrowed_form_is_skipped() {
        let events = [("OnCreateAtServer", "ПриСозданииНаСервере")];
        let mut metadata = form_metadata(&events);
        let mut form = (**metadata.form.as_ref().unwrap()).clone();
        form.has_base_form = true;
        metadata.form = Some(Arc::new(form));
        let code = proc_with("&НаКлиенте", "ПриСозданииНаСервере");
        assert!(run(metadata, &code).is_empty());
    }

    #[test]
    fn non_form_modules_are_not_checked() {
        let code = proc_with("&НаКлиенте", "ПриСозданииНаСервере");
        let common = crate::test_utils::make_non_common_module_metadata(
            bsl_metadata::ModuleType::CommonModule,
        );
        assert!(run(common, &code).is_empty());
    }

    #[test]
    fn form_module_without_managed_form_data_is_not_checked() {
        let code = proc_with("&НаКлиенте", "ПриСозданииНаСервере");
        let mut metadata = form_metadata(&[("OnCreateAtServer", "ПриСозданииНаСервере")]);
        metadata.form = None;
        assert!(run(metadata, &code).is_empty());
    }
}
