//! A method of a managed form module declared with `&НаКлиентеНаСервере` is visible to
//! neither side. Measured on platform 8.3.27.2214 (external data processor with a managed
//! form, the probe method called from `&НаСервере`): `ПолучитьФорму` fails with
//! "Ошибка инициализации модуля: ... Процедура или функция с указанным именем не определена".
//! Without any call the form still opens, so the declaration alone is dead code, while the
//! context-free `&НаКлиентеНаСервереБезКонтекста` passes and stays legal. The directive is
//! valid in a command module (developer guide 4.8.1.3), hence the form-module gate.

use crate::define_metadata;
use crate::metadata::*;
use crate::{Diagnostic, DiagnosticCode, DiagnosticsContext};
use bsl_metadata::{FormType, ModuleType};
use hir::AnnotationKind;
use ide_db::TextRange;

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

pub fn check(ctx: &DiagnosticsContext) -> Vec<Diagnostic> {
    let code = DiagnosticCode::AtClientAtServerFormMethod;

    if ctx.is_disabled_with_metadata(code) {
        return Vec::new();
    }

    let metadata = ctx.module_metadata();

    if metadata.module_type != ModuleType::FormModule {
        return Vec::new();
    }

    match &metadata.form {
        Some(form) if form.form_type == FormType::Managed => {}
        _ => return Vec::new(),
    }

    let item_tree = ctx.item_tree();
    let mut diagnostics = Vec::new();

    for (_, proc) in item_tree.procedures() {
        if has_both_sides_directive(&proc.annotations) {
            diagnostics.push(make_diagnostic(proc.name_range, code, ctx));
        }
    }

    for (_, func) in item_tree.functions() {
        if has_both_sides_directive(&func.annotations) {
            diagnostics.push(make_diagnostic(func.name_range, code, ctx));
        }
    }

    diagnostics.sort_by_key(|d| d.range.start());
    diagnostics
}

// The no-context variant is a different kind and is deliberately not matched.
fn has_both_sides_directive(annotations: &[hir::Annotation]) -> bool {
    annotations.iter().any(|a| a.kind == AnnotationKind::AtClientAtServer)
}

fn make_diagnostic(range: TextRange, code: DiagnosticCode, ctx: &DiagnosticsContext) -> Diagnostic {
    Diagnostic {
        code,
        message: "Метод формы с директивой &НаКлиентеНаСервере недоступен ни клиенту, ни серверу — вызвать его нельзя ниоткуда".to_string(),
        severity: ctx.severity(code),
        range,
        tags: ctx.tags(code),
        fixes: vec![],
    }
}

#[cfg(test)]
mod tests {
    use crate::test_utils::format_diags;
    use expect_test::expect;
    use std::sync::Arc;

    fn managed_form_metadata() -> hir::ModuleMetadata {
        let form = bsl_metadata::xml_parser::parse_form_xml(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<Form xmlns="http://v8.1c.ru/8.3/xcf/logform" version="2.20"></Form>"#,
        )
        .unwrap();

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

    #[test]
    fn both_sides_procedure_and_function_are_flagged_on_name() {
        let code = r#"&НаКлиентеНаСервере
Процедура ОбщаяПроцедура()
КонецПроцедуры

&НаКлиентеНаСервере
Функция ОбщаяФункция()
	Возврат 1;
КонецФункции
"#;
        let diagnostics = run(managed_form_metadata(), code);
        assert_eq!(diagnostics.len(), 2);
        expect![[r#"
            AtClientAtServerFormMethod @ 2:11..2:25
              message: Метод формы с директивой &НаКлиентеНаСервере недоступен ни клиенту, ни серверу — вызвать его нельзя ниоткуда
              severity: Warning
            AtClientAtServerFormMethod @ 6:9..6:21
              message: Метод формы с директивой &НаКлиентеНаСервере недоступен ни клиенту, ни серверу — вызвать его нельзя ниоткуда
              severity: Warning"#]].assert_eq(&format_diags(code, &diagnostics));
    }

    #[test]
    fn no_context_variant_is_legal() {
        let code = r#"&НаКлиентеНаСервереБезКонтекста
Процедура ОбщаяПроцедура()
КонецПроцедуры

&НаКлиентеНаСервереБезКонтекста
Функция ОбщаяФункция()
	Возврат 1;
КонецФункции
"#;
        assert!(run(managed_form_metadata(), code).is_empty());
    }

    #[test]
    fn single_side_directives_are_not_flagged() {
        let code = r#"&НаКлиенте
Процедура КлиентскаяПроцедура()
КонецПроцедуры

&НаСервере
Функция СервернаяФункция()
	Возврат 1;
КонецФункции
"#;
        assert!(run(managed_form_metadata(), code).is_empty());
    }

    #[test]
    fn non_form_modules_are_not_checked() {
        let code = r#"&НаКлиентеНаСервере
Процедура ОбщаяПроцедура()
КонецПроцедуры
"#;
        let common = crate::test_utils::make_non_common_module_metadata(
            bsl_metadata::ModuleType::CommonModule,
        );
        assert!(run(common, code).is_empty(), "common module");

        let command = crate::test_utils::make_non_common_module_metadata(
            bsl_metadata::ModuleType::CommandModule,
        );
        assert!(run(command, code).is_empty(), "command module");
    }

    #[test]
    fn form_module_without_managed_form_data_is_not_checked() {
        let code = "&НаКлиентеНаСервере\nПроцедура ОбщаяПроцедура()\nКонецПроцедуры\n";
        let mut metadata = managed_form_metadata();
        metadata.form = None;
        assert!(run(metadata, code).is_empty());
    }

    #[test]
    fn managed_common_form_flags_the_name_with_and_without_a_call() {
        const MODULE: &str = "CommonForms/Общая/Ext/Form/Module.bsl";
        const CONFIG: &str = r#"<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <Configuration uuid="00000000-0000-0000-0000-000000000000">
        <Properties><Name>TestConfiguration</Name></Properties>
    </Configuration>
</MetaDataObject>"#;
        const DESCRIPTOR: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.20">
    <CommonForm uuid="00000000-0000-0000-0000-000000000040">
        <Properties>
            <Name>Общая</Name>
            <FormType>Managed</FormType>
        </Properties>
    </CommonForm>
</MetaDataObject>"#;
        const FORM_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<Form xmlns="http://v8.1c.ru/8.3/xcf/logform" version="2.20"></Form>"#;
        let called = "&НаСервере\nПроцедура Пересчитать()\n    Общая();\nКонецПроцедуры\n\n\
                      &НаКлиентеНаСервере\nПроцедура Общая()\nКонецПроцедуры\n";
        let not_called = "&НаКлиентеНаСервере\nФункция Сумма()\n    Возврат 1;\nКонецФункции\n";

        for (source, name) in [(called, "Общая"), (not_called, "Сумма")] {
            let found = crate::test_utils::check_config_files(
                &[
                    ("Configuration.xml", CONFIG),
                    ("CommonForms/Общая.xml", DESCRIPTOR),
                    ("CommonForms/Общая/Ext/Form.xml", FORM_XML),
                    (MODULE, source),
                ],
                MODULE,
            )
            .into_iter()
            .filter(|d| d.code == crate::DiagnosticCode::AtClientAtServerFormMethod)
            .collect::<Vec<_>>();
            assert_eq!(found.len(), 1, "{source}");
            assert_eq!(&source[found[0].range], name);
            assert_eq!(found[0].severity, crate::Severity::Warning);
        }
    }
}
