use crate::define_metadata;
use crate::metadata::*;
use crate::AnalysisContext;
use crate::{Diagnostic, DiagnosticCode};
use bsl_metadata::traits::MdObject;
use hir::LocalRange;
use hir::{AnnotationKind, AssignmentResolution, ExistingBindingKind, MethodId};
use stdx::case::CaseExt;

pub const METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: DiagnosticType::Error,
    severity: DiagnosticSeverityLevel::Blocker,
    scope: DiagnosticScope::All,
    modules: &[],
    minutes_to_fix: 2,
    activated_by_default: true,
    compatibility_mode: DiagnosticCompatibilityMode::Undefined,
    tags: &[MetadataTag::Error],
    can_locate_on_project: false,
    extra_min_for_complexity: 0.0,
    lsp_severity_override: "",
};

pub fn from_hir(
    variable_name: &str,
    range: LocalRange,
    existing_binding_kind: Option<ExistingBindingKind>,
    method_id: Option<MethodId>,
    ctx: &AnalysisContext,
) -> Option<Diagnostic<LocalRange>> {
    let code = DiagnosticCode::CommonModuleAssign;

    if ctx.is_disabled_with_metadata(code) {
        return None;
    }

    if existing_binding_kind.is_some() {
        return None;
    }

    match ctx.assignment_target_kind(variable_name) {
        AssignmentResolution::CommonModule(_) => {}
        AssignmentResolution::Local
        | AssignmentResolution::Param
        | AssignmentResolution::ModuleVariable(_) => return None,
        AssignmentResolution::Unknown => {
            if !ctx.is_common_module_anywhere(variable_name) {
                return None;
            }
        }
    }

    if names_a_context_attribute(variable_name, method_id, ctx) {
        return None;
    }

    let display_name = ctx
        .resolve_common_module(variable_name)
        .map(|common_module| common_module.name().to_string())
        .unwrap_or_else(|| variable_name.to_string());

    Some(Diagnostic {
        code,
        message: format!("Недопустимо присваивание значения общему модулю '{}'", display_name),
        severity: ctx.severity(code),
        range,
        tags: ctx.tags(code),
        fixes: vec![],
    })
}

/// An attribute of the module's own context — a form attribute, an object
/// attribute — is found before the global context, so the assignment writes the
/// attribute and never reaches the common module of the same name. Measured on
/// 8.3.17 and 8.3.27: `ИмяМодуля = 5;` in a form's `ПриСозданииНаСервере` sets the
/// form attribute `ИмяМодуля`; without the attribute the same line fails at run
/// time with "Поле объекта недоступно для записи".
///
/// A method without context (`&НаСервереБезКонтекста`,
/// `&НаКлиентеНаСервереБезКонтекста`) does not see the form, so there the name
/// still reaches the module.
fn names_a_context_attribute(
    variable_name: &str,
    method_id: Option<MethodId>,
    ctx: &AnalysisContext,
) -> bool {
    let without_context =
        method_id.and_then(|method_id| ctx.interface_method(method_id)).is_some_and(|method| {
            method.directives.iter().any(|directive| {
                matches!(
                    directive,
                    AnnotationKind::AtServerNoContext | AnnotationKind::AtClientAtServerNoContext
                )
            })
        });
    if without_context {
        return false;
    }
    let key = variable_name.fold_lower();
    ctx.module_implicit_field_names().contains(&key)
}
#[cfg(test)]
mod tests {
    use crate::test_utils::{check_hir_diagnostic, format_diags};
    use crate::DiagnosticCode;
    use expect_test::expect;
    #[test]
    fn test_no_metadata() {
        let code = r#"Процедура Тест()
    СвойМодуль = 1;
КонецПроцедуры"#;

        let diagnostics = check_hir_diagnostic(code);
        let common_module_diags: Vec<_> = diagnostics
            .into_iter()
            .filter(|d| d.code == DiagnosticCode::CommonModuleAssign)
            .collect();

        expect![[r#""#]].assert_eq(&format_diags(code, &common_module_diags));
    }

    #[test]
    fn test_property_access_no_diagnostic() {
        let code = r#"Процедура Тест()
    СвойМодуль.Свойство = 1;
КонецПроцедуры"#;

        let diagnostics = check_hir_diagnostic(code);
        let common_module_diags: Vec<_> = diagnostics
            .into_iter()
            .filter(|d| d.code == DiagnosticCode::CommonModuleAssign)
            .collect();

        expect![[r#""#]].assert_eq(&format_diags(code, &common_module_diags));
    }

    #[test]
    fn test_index_access_no_diagnostic() {
        let code = r#"Процедура Тест()
    Массив[0] = 1;
КонецПроцедуры"#;

        let diagnostics = check_hir_diagnostic(code);
        let common_module_diags: Vec<_> = diagnostics
            .into_iter()
            .filter(|d| d.code == DiagnosticCode::CommonModuleAssign)
            .collect();

        expect![[r#""#]].assert_eq(&format_diags(code, &common_module_diags));
    }

    #[test]
    fn test_simple_variable_emits_candidate() {
        let code = r#"Процедура Тест()
    А = 1;
КонецПроцедуры"#;

        let diagnostics = check_hir_diagnostic(code);
        let common_module_diags: Vec<_> = diagnostics
            .into_iter()
            .filter(|d| d.code == DiagnosticCode::CommonModuleAssign)
            .collect();
        expect![[r#""#]].assert_eq(&format_diags(code, &common_module_diags));
    }
    const SERVER_MODULE_XML: &str = r#"<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <CommonModule uuid="00000000-0000-0000-0000-000000000101">
        <Properties>
            <Name>Модуль</Name>
            <Server>true</Server>
            <ClientManagedApplication>false</ClientManagedApplication>
            <ServerCall>false</ServerCall>
        </Properties>
    </CommonModule>
</MetaDataObject>"#;

    fn form_xml(attribute: Option<&str>) -> String {
        let attribute = attribute.map_or(String::new(), |name| {
            format!(
                r#"
        <Attribute name="{name}" id="2">
            <Type>
                <v8:Type>xs:decimal</v8:Type>
            </Type>
        </Attribute>"#
            )
        });
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<Form xmlns="http://v8.1c.ru/8.3/xcf/logform" xmlns:v8="http://v8.1c.ru/8.1/data/core" xmlns:xs="http://www.w3.org/2001/XMLSchema" version="2.10">
    <Attributes>{attribute}
    </Attributes>
</Form>"#
        )
    }

    fn form_assign_diags(directive: &str, attribute: Option<&str>) -> Vec<String> {
        let source = format!("{directive}\nПроцедура Тест()\n    Модуль = 5;\nКонецПроцедуры\n");
        crate::test_utils::check_form_with_common_modules_and_form_xml(
            &source,
            &[("Модуль", "Процедура П() Экспорт\nКонецПроцедуры\n", SERVER_MODULE_XML)],
            &form_xml(attribute),
        )
        .into_iter()
        .filter(|d| d.code == DiagnosticCode::CommonModuleAssign)
        .map(|d| d.message)
        .collect()
    }

    /// A form attribute is found before the common module of the same name, so the
    /// assignment writes the attribute: on 8.3.17 and 8.3.27 it succeeds, while the
    /// same line in a form without the attribute fails with "Поле объекта
    /// недоступно для записи". A method without context does not see the attribute.
    #[test]
    fn a_form_attribute_shadows_the_common_module() {
        assert!(form_assign_diags("&НаСервере", Some("Модуль")).is_empty());
        assert_eq!(
            form_assign_diags("&НаСервере", None),
            ["Недопустимо присваивание значения общему модулю 'Модуль'"]
        );
        assert_eq!(
            form_assign_diags("&НаСервереБезКонтекста", Some("Модуль")),
            ["Недопустимо присваивание значения общему модулю 'Модуль'"]
        );
    }
}
