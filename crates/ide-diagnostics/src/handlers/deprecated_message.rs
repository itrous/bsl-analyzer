use crate::AnalysisContext;
use crate::{Diagnostic, DiagnosticCode};
use bsl_platform::deprecation::{DeprecationEntry, LifecycleGroup};
use hir::LocalRange;

use super::deprecated_platform_api;
use super::deprecated_platform_facts::{
    canonical_name_for, global_function_fact, is_russian_alias,
    replacement_absent_from_configuration, replacement_for_name,
};

pub fn from_hir(
    name: &str,
    range: LocalRange,
    ctx: &AnalysisContext,
) -> Option<Diagnostic<LocalRange>> {
    let code = DiagnosticCode::DeprecatedPlatformApi;

    if ctx.is_disabled_with_metadata(code) {
        return None;
    }
    if deprecated_platform_api::is_excluded(ctx.config, name) {
        return None;
    }

    let fact = global_function_fact(name, LifecycleGroup::UserNotification)?;
    let message = get_message(name, fact, ctx)?;

    Some(Diagnostic {
        code,
        message,
        severity: ctx.severity(code),
        range,
        tags: ctx.tags(code),
        fixes: vec![],
    })
}

fn get_message(
    method_name: &str,
    fact: &DeprecationEntry,
    ctx: &AnalysisContext,
) -> Option<String> {
    let replacement = replacement_for_name(fact, method_name)?;
    let deprecated = canonical_name_for(fact, method_name)?;
    let russian = is_russian_alias(fact, method_name);
    // Do not advise a replacement this configuration provably does not have.
    if replacement_absent_from_configuration(replacement, ctx) {
        return Some(if russian {
            format!(
                "Устаревшее \"{}\": рекомендуемой замены \"{}\" в этой конфигурации нет",
                deprecated, replacement
            )
        } else {
            format!(
                "Deprecated \"{}\": the recommended replacement \"{}\" is absent from this configuration",
                deprecated, replacement
            )
        });
    }
    if russian {
        Some(format!("Используйте \"{}\" вместо устаревшего \"{}\"", replacement, deprecated))
    } else {
        Some(format!("Use \"{}\" instead of deprecated \"{}\"", replacement, deprecated))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::*;
    use crate::Severity;
    use expect_test::expect;
    #[test]
    fn test_deprecated_russian() {
        let code = r#"
Процедура Тест()
    Сообщить("Операция выполнена");
КонецПроцедуры
"#;
        let diagnostics = check_hir_diagnostic(code);
        let deprecated_diags: Vec<_> = diagnostics
            .into_iter()
            .filter(|d| d.code == DiagnosticCode::DeprecatedPlatformApi)
            .collect();

        expect![[r#"
            DeprecatedPlatformApi @ 3:5..3:13
              message: Используйте "ОбщегоНазначения.СообщитьПользователю" вместо устаревшего "Сообщить"
              severity: Warning"#]].assert_eq(&format_diags(code, &deprecated_diags));
        assert_eq!(deprecated_diags[0].severity, Severity::Warning);
        assert!(deprecated_diags[0].message.contains("СообщитьПользователю"));
    }

    #[test]
    fn test_deprecated_english() {
        let code = r#"
Procedure Test()
    Message("Operation completed");
EndProcedure
"#;
        let diagnostics = check_hir_diagnostic(code);
        let deprecated_diags: Vec<_> = diagnostics
            .into_iter()
            .filter(|d| d.code == DiagnosticCode::DeprecatedPlatformApi)
            .collect();

        expect![[r#"
            DeprecatedPlatformApi @ 3:5..3:12
              message: Use "CommonUse.MessageToUser" instead of deprecated "Message"
              severity: Warning"#]]
        .assert_eq(&format_diags(code, &deprecated_diags));
        assert!(deprecated_diags[0].message.contains("MessageToUser"));
    }

    #[test]
    fn test_object_method_excluded() {
        let code = r#"
Процедура Тест()
    Модуль.Сообщить("Операция выполнена");
КонецПроцедуры
"#;
        let diagnostics = check_hir_diagnostic(code);
        let deprecated_diags: Vec<_> = diagnostics
            .into_iter()
            .filter(|d| d.code == DiagnosticCode::DeprecatedPlatformApi)
            .collect();

        expect![[r#""#]].assert_eq(&format_diags(code, &deprecated_diags));
    }

    #[test]
    fn test_case_insensitive() {
        let code = r#"
Процедура Тест()
    СООБЩИТЬ("A");
    сообщить("B");
    Сообщить("C");
    СообЩить("D");
КонецПроцедуры
"#;
        let diagnostics = check_hir_diagnostic(code);
        let deprecated_diags: Vec<_> = diagnostics
            .into_iter()
            .filter(|d| d.code == DiagnosticCode::DeprecatedPlatformApi)
            .collect();

        expect![[r#"
            DeprecatedPlatformApi @ 3:5..3:13
              message: Используйте "ОбщегоНазначения.СообщитьПользователю" вместо устаревшего "Сообщить"
              severity: Warning
            DeprecatedPlatformApi @ 4:5..4:13
              message: Используйте "ОбщегоНазначения.СообщитьПользователю" вместо устаревшего "Сообщить"
              severity: Warning
            DeprecatedPlatformApi @ 5:5..5:13
              message: Используйте "ОбщегоНазначения.СообщитьПользователю" вместо устаревшего "Сообщить"
              severity: Warning
            DeprecatedPlatformApi @ 6:5..6:13
              message: Используйте "ОбщегоНазначения.СообщитьПользователю" вместо устаревшего "Сообщить"
              severity: Warning"#]].assert_eq(&format_diags(code, &deprecated_diags));
    }

    #[test]
    fn test_inside_if_block() {
        let code = r#"
Процедура А()
    Если Истина Тогда
        MessaGe("А");
        Модуль.Сообщить();
    КонецЕсли;
КонецПроцедуры
"#;
        let diagnostics = check_hir_diagnostic(code);
        let diags: Vec<_> = diagnostics
            .into_iter()
            .filter(|d| d.code == DiagnosticCode::DeprecatedPlatformApi)
            .collect();
        expect![[r#"
            DeprecatedPlatformApi @ 4:9..4:16
              message: Use "CommonUse.MessageToUser" instead of deprecated "Message"
              severity: Warning"#]]
        .assert_eq(&format_diags(code, &diags));
        assert!(diags[0].message.contains("MessageToUser"));
    }

    const CLIENT_SERVER_MODULE_XML: &str = r#"<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <CommonModule uuid="00000000-0000-0000-0000-000000000104">
        <Properties>
            <Name>ОбщегоНазначения</Name>
            <Server>true</Server>
            <ClientManagedApplication>true</ClientManagedApplication>
            <ServerCall>false</ServerCall>
        </Properties>
    </CommonModule>
</MetaDataObject>"#;

    const OTHER_EXPORT: &str = "Процедура ДругойМетод() Экспорт\nКонецПроцедуры\n";
    const REPLACEMENT_EXPORT: &str = "Процедура СообщитьПользователю() Экспорт\nКонецПроцедуры\n";
    const CALL_SITE: &str = "&НаКлиенте\nПроцедура Тест()\n    Сообщить(\"х\");\nКонецПроцедуры\n";

    const ABSENT_MESSAGE: &str = "Устаревшее \"Сообщить\": рекомендуемой замены \"ОбщегоНазначения.СообщитьПользователю\" в этой конфигурации нет";
    const NAMED_MESSAGE: &str =
        "Используйте \"ОбщегоНазначения.СообщитьПользователю\" вместо устаревшего \"Сообщить\"";

    fn deprecated_messages(modules: &[(&str, &str, &str)]) -> Vec<String> {
        check_form_with_common_modules(CALL_SITE, modules)
            .into_iter()
            .filter(|d| d.code == DiagnosticCode::DeprecatedPlatformApi)
            .map(|d| d.message)
            .collect()
    }

    #[test]
    fn replacement_absent_from_the_configuration_is_not_named() {
        let messages =
            deprecated_messages(&[("ОбщегоНазначения", OTHER_EXPORT, CLIENT_SERVER_MODULE_XML)]);
        assert_eq!(messages, vec![ABSENT_MESSAGE.to_string()]);
        assert!(!messages[0].contains("Используйте"));
    }

    #[test]
    fn replacement_present_in_the_configuration_is_still_named() {
        let messages = deprecated_messages(&[(
            "ОбщегоНазначения",
            REPLACEMENT_EXPORT,
            CLIENT_SERVER_MODULE_XML,
        )]);
        assert_eq!(messages, vec![NAMED_MESSAGE.to_string()]);
    }

    // Control: a receiver that is not a common module of the configuration keeps its
    // name. This stops the gate from stripping platform-spelled replacements (a chart
    // property path, say), whose absence from the configuration proves nothing.
    #[test]
    fn a_receiver_that_is_not_a_common_module_keeps_its_name() {
        let messages = deprecated_messages(&[]);
        assert_eq!(messages, vec![NAMED_MESSAGE.to_string()]);
    }

    #[test]
    fn test_module_level_call() {
        let code = r#"
Сообщить("А");
Модуль.Сообщить();
ДругойМетод();
"#;
        let diagnostics = check_hir_diagnostic(code);
        let diags: Vec<_> = diagnostics
            .into_iter()
            .filter(|d| d.code == DiagnosticCode::DeprecatedPlatformApi)
            .collect();
        expect![[r#"
            DeprecatedPlatformApi @ 2:1..2:9
              message: Используйте "ОбщегоНазначения.СообщитьПользователю" вместо устаревшего "Сообщить"
              severity: Warning"#]].assert_eq(&format_diags(code, &diags));
        assert!(diags[0].message.contains("СообщитьПользователю"));
    }
}
