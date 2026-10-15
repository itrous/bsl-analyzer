use crate::AnalysisContext;
use crate::{Diagnostic, DiagnosticCode};
use bsl_platform::deprecation::{DeprecationEntry, LifecycleGroup};
use hir::LocalRange;

use super::deprecated_platform_api;
use super::deprecated_platform_facts::{
    canonical_name_for, global_function_fact, is_russian_alias, replacement_for_name,
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
    let message = get_message(name, fact)?;

    Some(Diagnostic {
        code,
        message,
        severity: ctx.severity(code),
        range,
        tags: ctx.tags(code),
        fixes: vec![],
    })
}

fn get_message(method_name: &str, fact: &DeprecationEntry) -> Option<String> {
    let replacement = replacement_for_name(fact, method_name)?;
    let deprecated = canonical_name_for(fact, method_name)?;
    if is_russian_alias(fact, method_name) {
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
