use crate::define_metadata;
use crate::metadata::*;
use crate::AnalysisContext;
use crate::{Diagnostic, DiagnosticCode};
use hir::LocalRange;
use hir::Name;

pub const METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: DiagnosticType::Error,
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

pub fn from_hir(
    _module: &str,
    _method: &str,
    _range: LocalRange,
    _ctx: &AnalysisContext,
) -> Option<Diagnostic<LocalRange>> {
    let _ = (Name::new(""), DiagnosticCode::MissingCommonModuleMethod);
    None
}

#[cfg(test)]
mod tests {
    use crate::test_utils::{check_diagnostics_snapshot_for, check_hir_diagnostic};
    use crate::DiagnosticCode;
    use expect_test::expect;

    #[test]
    fn test_missing_common_module_method() {
        let code = r#"
Процедура Тест()
    ПервыйОбщийМодуль.МетодНесуществующий(1, 2);
КонецПроцедуры
"#;

        let diagnostics = check_hir_diagnostic(code);
        let umc: Vec<_> = diagnostics
            .iter()
            .filter(|d| d.code == crate::DiagnosticCode::UnresolvedMethodCall)
            .collect();
        assert_eq!(
            umc.len(),
            1,
            "Expected one UnresolvedMethodCall for the unresolved receiver, got: {:?}",
            diagnostics.iter().map(|d| (&d.code, &d.message)).collect::<Vec<_>>()
        );
        assert!(
            umc[0].message.contains("ПервыйОбщийМодуль"),
            "message must name the unresolved receiver, got: {}",
            umc[0].message
        );
    }

    #[test]
    #[ignore]
    fn registered_common_module_with_missing_method_emits_method_not_found() {
        use crate::test_utils::check_hir_diagnostic_with_fixtures;
        let fixture = r#"
//- /CommonModules/ПервыйОбщийМодуль/Module.bsl
Процедура ДругойМетод() Экспорт
КонецПроцедуры

//- /test.bsl
Процедура Тест()
    ПервыйОбщийМодуль.МетодНесуществующий(1, 2);
КонецПроцедуры
"#;

        let diagnostics = check_hir_diagnostic_with_fixtures(fixture);
        let umc: Vec<_> = diagnostics
            .iter()
            .filter(|d| d.code == crate::DiagnosticCode::UnresolvedMethodCall)
            .collect();

        assert_eq!(umc.len(), 1, "Expected one UnresolvedMethodCall, got: {:?}", diagnostics);
        assert!(umc[0].message.contains("МетодНесуществующий"));
        assert!(umc[0].message.contains("ПервыйОбщийМодуль"));
    }

    #[test]
    fn test_local_variable_shadowing() {
        let code = r#"
Процедура Тест(ПервыйОбщийМодуль)  // Parameter shadows module name
    ПервыйОбщийМодуль.Method();  // Should NOT trigger - parameter
КонецПроцедуры

Функция ДругойТест()
    Перем ПервыйОбщийМодуль;  // Local variable
    Возврат ПервыйОбщийМодуль.SomeMethod();  // Should NOT trigger - local variable
КонецФункции
"#;

        check_diagnostics_snapshot_for(
            code,
            crate::DiagnosticCode::MissingCommonModuleMethod,
            expect![[r#""#]],
        );
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn test_platform_global_member_call_suppresses_diagnostic() {
        let code = r#"
Процедура Тест()
    ОбработкаОшибок.КраткоеПредставлениеОшибки(ИнформацияОбОшибке());
КонецПроцедуры
"#;
        let diagnostics = check_hir_diagnostic(code);
        let mcm: Vec<_> = diagnostics
            .iter()
            .filter(|d| d.code == crate::DiagnosticCode::MissingCommonModuleMethod)
            .collect();
        assert!(
            mcm.is_empty(),
            "platform global member calls must not raise MissingCommonModuleMethod, got: {:?}",
            mcm.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
        let umc: Vec<_> = diagnostics
            .iter()
            .filter(|d| d.code == crate::DiagnosticCode::UnresolvedMethodCall)
            .collect();
        assert!(
            umc.is_empty(),
            "valid platform-global member call must not regress to UnresolvedMethodCall, got: {:?}",
            umc.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
    }

    #[test]
    #[ignore]
    fn user_common_module_shadows_platform_global() {
        use crate::test_utils::check_hir_diagnostic_with_fixtures;
        let fixture = r#"
//- /CommonModules/Метаданные/Module.bsl
Процедура ОдинЭкспортируемыйМетод() Экспорт
КонецПроцедуры

//- /test.bsl
Процедура Тест()
    Метаданные.Найти("КакаяТоТаблица");
КонецПроцедуры
"#;
        let diagnostics = check_hir_diagnostic_with_fixtures(fixture);
        let umc: Vec<_> = diagnostics
            .iter()
            .filter(|d| d.code == crate::DiagnosticCode::UnresolvedMethodCall)
            .collect();
        assert_eq!(
            umc.len(),
            1,
            "user-shadows-platform must surface a UMC; if this is silent the cascade fell \
             through to gate 4 (platform `Найти` Resolved). got: {:?}",
            diagnostics
        );
        assert!(
            umc[0].message.contains("Метаданные") && umc[0].message.contains("Найти"),
            "UMC must name the user's missing method, got: {}",
            umc[0].message
        );
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn test_platform_global_unknown_member_keeps_falling_through() {
        let code = r#"
Процедура Тест()
    ОбработкаОшибок.СовершенноНесуществующийМетод();
КонецПроцедуры
"#;
        let diagnostics = check_hir_diagnostic(code);
        let umc: Vec<_> = diagnostics
            .iter()
            .filter(|d| d.code == crate::DiagnosticCode::UnresolvedMethodCall)
            .collect();
        assert_eq!(
            umc.len(),
            1,
            "unknown method on a known platform global must raise UnresolvedMethodCall, got: {:?}",
            diagnostics.iter().map(|d| (&d.code, &d.message)).collect::<Vec<_>>()
        );
        assert!(
            umc[0].message.contains("СовершенноНесуществующийМетод"),
            "message must name the missing method, got: {}",
            umc[0].message
        );
        let mcm: Vec<_> = diagnostics
            .iter()
            .filter(|d| d.code == crate::DiagnosticCode::MissingCommonModuleMethod)
            .collect();
        assert!(
            mcm.is_empty(),
            "deprecated MissingCommonModuleMethod must not surface anymore, got: {:?}",
            mcm.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
    }

    fn managed_form_metadata(form_xml: &str) -> hir::ModuleMetadata {
        use std::sync::Arc;
        let form = bsl_metadata::xml_parser::parse_form_xml(form_xml).unwrap();
        assert!(form.is_managed(), "fixture must produce a managed form");
        hir::ModuleMetadata {
            module_type: bsl_metadata::ModuleType::FormModule,
            execution_context: None,
            common_module: None,
            mdo: None,
            register: None,
            form: Some(Arc::new(form)),
            http_service: None,
            web_service: None,
            integration_service: None,
        }
    }

    const MANAGED_FORM_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<Form xmlns="http://v8.1c.ru/8.3/xcf/logform" version="2.20"></Form>"#;

    #[test]
    fn form_self_method_call_in_managed_form_does_not_emit_missing_common_module_method() {
        let code = r#"
&НаКлиенте
Процедура Тест()
    Элементы.Найти("ТЗШкалы");
КонецПроцедуры
"#;
        let metadata = managed_form_metadata(MANAGED_FORM_XML);
        let diagnostics =
            crate::test_utils::check_metadata_diagnostic(metadata, code, |_metadata, ctx| {
                crate::diagnostics(ctx)
            });
        let bad: Vec<_> = diagnostics
            .iter()
            .filter(|d| d.code == DiagnosticCode::MissingCommonModuleMethod)
            .collect();
        assert!(
            bad.is_empty(),
            "form-self property `Элементы` must not fire MissingCommonModuleMethod, got: {:?}",
            bad.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
    }

    #[test]
    #[ignore]
    fn unknown_module_in_managed_form_still_reports_unresolved_receiver() {
        let code = r#"
&НаКлиенте
Процедура Тест()
    ЗаведомоНесуществующийМодуль.КакойТоМетод();
КонецПроцедуры
"#;
        let metadata = managed_form_metadata(MANAGED_FORM_XML);
        let diagnostics =
            crate::test_utils::check_metadata_diagnostic(metadata, code, |_metadata, ctx| {
                crate::diagnostics(ctx)
            });
        let umc: Vec<_> =
            diagnostics.iter().filter(|d| d.code == DiagnosticCode::UnresolvedMethodCall).collect();
        assert_eq!(
            umc.len(),
            1,
            "unknown module call must still surface as UnresolvedMethodCall, got: {:?}",
            diagnostics.iter().map(|d| (&d.code, &d.message)).collect::<Vec<_>>()
        );
        assert!(
            umc[0].message.contains("ЗаведомоНесуществующийМодуль"),
            "message must name the unresolved receiver, got: {}",
            umc[0].message
        );
        let mcm: Vec<_> = diagnostics
            .iter()
            .filter(|d| d.code == DiagnosticCode::MissingCommonModuleMethod)
            .collect();
        assert!(
            mcm.is_empty(),
            "deprecated MissingCommonModuleMethod must stay silent, got: {:?}",
            mcm.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
    }

    #[test]
    fn form_self_name_in_common_module_reports_unresolved_receiver() {
        let code = r#"
Процедура Тест()
    Элементы.Найти("Х");
КонецПроцедуры
"#;
        let diagnostics = check_hir_diagnostic(code);
        let umc: Vec<_> =
            diagnostics.iter().filter(|d| d.code == DiagnosticCode::UnresolvedMethodCall).collect();
        assert_eq!(
            umc.len(),
            1,
            "form-self property name in non-form module must still be reported, got: {:?}",
            diagnostics.iter().map(|d| (&d.code, &d.message)).collect::<Vec<_>>()
        );
        assert!(
            umc[0].message.contains("Элементы"),
            "message must name the unresolved receiver, got: {}",
            umc[0].message
        );
    }

    #[test]
    fn test_mixed_local_and_common_module() {
        let code = r#"
Процедура Тест()
    Перем ПервыйОбщийМодуль;
    ПервыйОбщийМодуль.Method();
КонецПроцедуры

Процедура ДругойТест()
    ВторойОбщийМодуль.Method();
КонецПроцедуры
"#;

        let diagnostics = check_hir_diagnostic(code);
        let umc: Vec<_> = diagnostics
            .iter()
            .filter(|d| d.code == crate::DiagnosticCode::UnresolvedMethodCall)
            .collect();
        assert_eq!(
            umc.len(),
            1,
            "Expected exactly one UnresolvedMethodCall (for ВторойОбщийМодуль), got: {:?}",
            diagnostics.iter().map(|d| (&d.code, &d.message)).collect::<Vec<_>>()
        );
        assert!(
            umc[0].message.contains("ВторойОбщийМодуль"),
            "diagnostic must point at the unresolved receiver, got: {}",
            umc[0].message
        );
        assert!(
            !umc.iter().any(|d| d.message.contains("ПервыйОбщийМодуль")),
            "shadowed local must not surface a diagnostic, got: {:?}",
            umc.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
    }
}
