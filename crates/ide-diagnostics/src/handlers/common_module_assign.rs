use crate::define_metadata;
use crate::metadata::*;
use crate::AnalysisContext;
use crate::{Diagnostic, DiagnosticCode};
use bsl_metadata::traits::MdObject;
use hir::LocalRange;
use hir::{AssignmentResolution, DefWithBodyId, ExistingBindingKind, MethodId};
use stdx::case::fold_lower_per_char;

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

    let owner = method_id
        .map_or(DefWithBodyId::ModuleCode, |method_id| DefWithBodyId::Method(method_id.local_id));
    if names_a_context_attribute(variable_name, owner, ctx) {
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
    owner: DefWithBodyId,
    ctx: &AnalysisContext,
) -> bool {
    if !ctx.owner_has_form_context(owner) {
        return false;
    }
    let key = fold_lower_per_char(variable_name);
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

    #[test]
    fn mock_provider_preserves_missing_declaration_form_context() {
        let metadata = hir::ModuleMetadata::unknown(bsl_metadata::ModuleType::FormModule);
        crate::test_utils::check_metadata_diagnostic(
            metadata,
            "&НаСервереБезКонтекста\nПроцедура БезКонтекста() КонецПроцедуры\n&НаСервере\nПроцедура СКонтекстом() КонецПроцедуры",
            |_, ctx| {
                assert!(!ctx.owner_has_form_context(hir::DefWithBodyId::Method(
                    hir::MethodKey::first("БезКонтекста"),
                )));
                assert!(ctx.owner_has_form_context(hir::DefWithBodyId::Method(
                    hir::MethodKey::first("СКонтекстом"),
                )));
                assert!(ctx.owner_has_form_context(hir::DefWithBodyId::Method(
                    hir::MethodKey::first("Отсутствует"),
                )));
                Vec::new()
            },
        );
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
        assert_eq!(
            form_assign_diags("&НаКлиентеНаСервереБезКонтекста", Some("Модуль")),
            ["Недопустимо присваивание значения общему модулю 'Модуль'"]
        );
    }

    /// Case folding of the assigned name and the attribute names must not depend on
    /// where a letter stands: `str::to_lowercase` folds a word-final `Σ` to `ς`, so
    /// `ΟΔΟΣ` and the attribute `οδοσ` would get different keys though the platform
    /// treats them as one name.
    #[test]
    fn a_form_attribute_shadows_the_common_module_whatever_the_letter_position() {
        let source = "&НаСервере\nПроцедура Тест()\n    ΟΔΟΣ = 5;\nКонецПроцедуры\n";
        let module_xml = SERVER_MODULE_XML.replace("<Name>Модуль</Name>", "<Name>ΟΔΟΣ</Name>");
        let diags: Vec<_> = crate::test_utils::check_form_with_common_modules_and_form_xml(
            source,
            &[("ΟΔΟΣ", "Процедура П() Экспорт\nКонецПроцедуры\n", &module_xml)],
            &form_xml(Some("οδοσ")),
        )
        .into_iter()
        .filter(|d| d.code == DiagnosticCode::CommonModuleAssign)
        .map(|d| d.message)
        .collect();
        assert!(diags.is_empty(), "{diags:?}");
    }
}
