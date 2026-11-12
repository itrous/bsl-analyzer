//! A procedure of a managed form module named like a form-level event handler, but not
//! bound to any event in the form's `Ext/Form.xml`, is dead code. Measured on platform
//! 8.3.27.2214: a managed form binds a handler only through `<Event>`, never by name, so the
//! platform simply never calls such a procedure - no error, no warning, nothing in the event
//! log, only the absence of an effect.

use crate::define_metadata;
use crate::metadata::*;
use crate::{Diagnostic, DiagnosticCode, DiagnosticsContext};
use bsl_metadata::ModuleType;
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

/// Canonical (Russian) names of form-level event handlers, lowercase.
static FORM_EVENT_HANDLER_NAMES: &[&str] = &[
    "присозданиинасервере",
    "причтениинасервере",
    "передзаписью",
    "передзаписьюнасервере",
    "призаписинасервере",
    "послезаписинасервере",
    "послезаписи",
    "приоткрытии",
    "передзакрытием",
    "призакрытии",
    "обработкаоповещения",
    "обработкавыбора",
    "обработказаписинового",
    "обработкапроверкизаполнениянасервере",
    "приповторномоткрытии",
    "призагрузкеданныхизнастроекнасервере",
    "присохраненииданныхвнастройкахнасервере",
    "передзагрузкойданныхизнастроекнасервере",
    "обновлениеотображенияданных",
    "выборзначения",
];

pub fn check(ctx: &DiagnosticsContext) -> Vec<Diagnostic> {
    let code = DiagnosticCode::UnboundFormEventHandler;

    if ctx.is_disabled_with_metadata(code) {
        return Vec::new();
    }

    let metadata = ctx.module_metadata();

    if metadata.module_type != ModuleType::FormModule {
        return Vec::new();
    }

    let Some(form) = metadata.form.as_deref() else {
        return Vec::new();
    };

    // A borrowed form names handlers that live in the base form's module.
    if !form.is_managed() || form.has_base_form() {
        return Vec::new();
    }

    let item_tree = ctx.item_tree();

    let bound: Vec<String> = form
        .event_handlers()
        .iter()
        .map(|h| h.handler_name.fold_lower())
        .chain(form.command_handlers().iter().map(|h| h.fold_lower()))
        .collect();

    // The form's own bindings settle most candidates; what follows walks the module and
    // reads a fold over the whole source root, so it runs only for the rest.
    let candidates: Vec<_> = item_tree
        .procedures()
        .filter(|(_, p)| !p.is_export)
        .map(|(_, p)| (p.name.as_str(), p.name_range))
        .chain(
            item_tree
                .functions()
                .filter(|(_, f)| !f.is_export)
                .map(|(_, f)| (f.name.as_str(), f.name_range)),
        )
        .map(|(name, range)| (name, name.fold_lower(), range))
        .filter(|(_, lower, _)| {
            FORM_EVENT_HANDLER_NAMES.contains(&lower.as_str()) && !bound.contains(lower)
        })
        .collect();

    if candidates.is_empty() {
        return Vec::new();
    }

    // Calls, wait and notification handlers, bindings by name: the same set
    // `UnusedLocalMethod` works from.
    let Some(called) = super::unused_local_method::collect_called_methods(ctx, false) else {
        return Vec::new();
    };

    let candidates: Vec<_> =
        candidates.into_iter().filter(|(_, lower, _)| !called.contains(lower)).collect();

    if candidates.is_empty() {
        return Vec::new();
    }

    let root = ctx.parse().syntax_node();
    let literals = super::unused_local_method::string_literal_texts(&root);

    let mut diagnostics = Vec::new();

    for (name, lower, name_range) in candidates {
        if literals.iter().any(|text| super::unused_local_method::mentions_identifier(text, &lower))
        {
            continue;
        }

        diagnostics.push(Diagnostic {
            code,
            message: format!(
                "Обработчик «{name}» не привязан ни к одному событию формы: платформа его не вызывает. Добавьте <Event> в Ext/Form.xml или удалите процедуру"
            ),
            severity: ctx.severity(code),
            range: name_range,
            tags: ctx.tags(code),
            fixes: vec![],
        });
    }

    diagnostics.sort_by_key(|d| d.range.start());
    diagnostics
}

#[cfg(test)]
mod tests {
    use crate::test_utils::format_diags;
    use crate::DiagnosticCode;
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

    fn with_form(
        mut metadata: hir::ModuleMetadata,
        edit: impl FnOnce(&mut bsl_metadata::Form),
    ) -> hir::ModuleMetadata {
        let mut form = (**metadata.form.as_ref().unwrap()).clone();
        edit(&mut form);
        metadata.form = Some(Arc::new(form));
        metadata
    }

    fn run(metadata: hir::ModuleMetadata, code: &str) -> Vec<crate::Diagnostic> {
        crate::test_utils::check_metadata_diagnostic(metadata, code, |_, ctx| super::check(ctx))
    }

    const UNBOUND: &str = "&НаКлиенте\nПроцедура ПриОткрытии()\nКонецПроцедуры\n";

    #[test]
    fn bound_handler_is_clean() {
        let events = [("OnOpen", "ПриОткрытии")];
        assert!(run(form_metadata(&events), UNBOUND).is_empty());
    }

    #[test]
    fn bound_handler_is_matched_case_insensitively() {
        let events = [("OnOpen", "приоткрытии")];
        assert!(run(form_metadata(&events), UNBOUND).is_empty());
    }

    #[test]
    fn unbound_handler_is_reported() {
        let diagnostics = run(form_metadata(&[]), UNBOUND);
        assert_eq!(diagnostics.len(), 1);
        expect![[r#"
            UnboundFormEventHandler @ 2:11..2:22
              message: Обработчик «ПриОткрытии» не привязан ни к одному событию формы: платформа его не вызывает. Добавьте <Event> в Ext/Form.xml или удалите процедуру
              severity: Warning"#]]
        .assert_eq(&format_diags(UNBOUND, &diagnostics));
    }

    #[test]
    fn unbound_function_is_reported() {
        let code = "&НаСервере\nФункция ПередЗаписью()\n\tВозврат 1;\nКонецФункции\n";
        assert_eq!(run(form_metadata(&[]), code).len(), 1);
    }

    #[test]
    fn handler_bound_to_another_event_name_is_still_bound() {
        let events = [("OnClose", "ПриОткрытии")];
        assert!(run(form_metadata(&events), UNBOUND).is_empty());
    }

    #[test]
    fn command_handler_is_not_reported() {
        let metadata =
            with_form(form_metadata(&[]), |form| form.command_handlers.push("ПриОткрытии".into()));
        assert!(run(metadata, UNBOUND).is_empty());
    }

    #[test]
    fn exported_method_is_not_reported() {
        let code = "&НаКлиенте\nПроцедура ПриОткрытии() Экспорт\nКонецПроцедуры\n";
        assert!(run(form_metadata(&[]), code).is_empty());
    }

    #[test]
    fn locally_called_method_is_not_reported() {
        let code = "&НаКлиенте\nПроцедура ПриОткрытии()\nКонецПроцедуры\n\n&НаКлиенте\nПроцедура Другая()\n\tПриОткрытии();\nКонецПроцедуры\n";
        let diagnostics = run(form_metadata(&[]), code);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    // A same-name call on another module is not a local call, the handler stays dead.
    #[test]
    fn same_name_call_on_another_module_still_reported() {
        let code = "&НаКлиенте
Процедура ПриОткрытии(Отказ)
	РаботаСФайламиКлиент.ПриОткрытии(ЭтотОбъект, Отказ);
КонецПроцедуры
";
        let events = [("OnCreateAtServer", "ПриСозданииНаСервере")];
        let diagnostics = run(form_metadata(&events), code);
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert!(diagnostics[0].message.contains("ПриОткрытии"), "{diagnostics:?}");
    }

    #[test]
    fn name_in_string_literal_is_not_reported() {
        let code = "&НаКлиенте\nПроцедура ПриОткрытии()\nКонецПроцедуры\n\n&НаКлиенте\nПроцедура Другая()\n\tПодключитьОбработчикОжидания(\"ПриОткрытии\", 1, Истина);\nКонецПроцедуры\n";
        let diagnostics = run(form_metadata(&[]), code);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    #[test]
    fn name_inside_longer_string_literal_is_not_reported() {
        let code = "&НаКлиенте\nПроцедура ПриОткрытии()\nКонецПроцедуры\n\n&НаКлиенте\nПроцедура Другая()\n\tВыполнить(\"Результат = ПриОткрытии();\");\nКонецПроцедуры\n";
        let diagnostics = run(form_metadata(&[]), code);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    #[test]
    fn form_with_base_form_is_skipped() {
        let metadata = with_form(form_metadata(&[]), |form| form.has_base_form = true);
        assert!(run(metadata, UNBOUND).is_empty());
    }

    #[test]
    fn name_outside_the_table_is_not_reported() {
        let code = "&НаКлиенте\nПроцедура МояПроцедура()\nКонецПроцедуры\n";
        assert!(run(form_metadata(&[]), code).is_empty());
    }

    #[test]
    fn ordinary_form_is_skipped() {
        let metadata = with_form(form_metadata(&[]), |form| {
            form.form_type = bsl_metadata::FormType::Ordinary;
        });
        assert!(run(metadata, UNBOUND).is_empty());
    }

    #[test]
    fn non_form_modules_are_not_checked() {
        let common = crate::test_utils::make_non_common_module_metadata(
            bsl_metadata::ModuleType::CommonModule,
        );
        assert!(run(common, UNBOUND).is_empty());
    }

    #[test]
    fn form_module_without_form_data_is_not_checked() {
        let mut metadata = form_metadata(&[]);
        metadata.form = None;
        assert!(run(metadata, UNBOUND).is_empty());
    }

    #[test]
    fn every_name_of_the_table_is_reported_when_unbound() {
        for name in super::FORM_EVENT_HANDLER_NAMES {
            let code = format!("&НаКлиенте\nПроцедура {name}()\nКонецПроцедуры\n");
            assert_eq!(run(form_metadata(&[]), &code).len(), 1, "name: {name}");
        }
    }

    fn both(metadata: hir::ModuleMetadata, code: &str) -> Vec<crate::Diagnostic> {
        crate::test_utils::check_metadata_diagnostic(metadata, code, |_, ctx| {
            let mut all = super::check(ctx);
            all.extend(crate::handlers::unused_local_method::check(ctx));
            all
        })
    }

    #[test]
    fn unused_local_method_does_not_duplicate_the_finding() {
        let diagnostics = both(form_metadata(&[]), UNBOUND);
        let codes: Vec<_> = diagnostics.iter().map(|d| d.code).collect();
        assert_eq!(codes, [DiagnosticCode::UnboundFormEventHandler], "{diagnostics:?}");
    }

    #[test]
    fn unused_local_method_still_reports_names_outside_the_table() {
        let code = "&НаКлиенте\nПроцедура МояПроцедура()\nКонецПроцедуры\n";
        let diagnostics = both(form_metadata(&[]), code);
        let codes: Vec<_> = diagnostics.iter().map(|d| d.code).collect();
        assert_eq!(codes, [DiagnosticCode::UnusedLocalMethod], "{diagnostics:?}");
    }
}
