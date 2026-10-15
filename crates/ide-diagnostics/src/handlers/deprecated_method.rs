use crate::AnalysisContext;
use crate::{Diagnostic, DiagnosticCode};
use bsl_platform::deprecation::DeprecationEntry;
use hir::LocalRange;
use stdx::case::CaseExt;

use super::deprecated_platform_api;
use super::deprecated_platform_facts::{deprecated_method_fact, replacement_for_name};

pub fn from_hir(
    name: &str,
    range: LocalRange,
    ctx: &AnalysisContext,
) -> Option<Diagnostic<LocalRange>> {
    let (code, replacement) = get_diagnostic_code_and_replacement(name)?;

    if ctx.is_disabled_with_metadata(code) {
        return None;
    }
    if deprecated_platform_api::is_excluded(ctx.config, name) {
        return None;
    }

    let message = get_message(name, replacement);

    Some(Diagnostic {
        code,
        message,
        severity: ctx.severity(code),
        range,
        tags: ctx.tags(code),
        fixes: vec![],
    })
}

fn get_diagnostic_code_and_replacement(name: &str) -> Option<(DiagnosticCode, &'static str)> {
    let entry = deprecated_method_fact(name)?;
    Some((DiagnosticCode::DeprecatedPlatformApi, replacement_for_method(entry, name)?))
}

fn replacement_for_method(entry: &DeprecationEntry, name: &str) -> Option<&'static str> {
    replacement_for_name(entry, name)
}

fn get_message(method_name: &str, replacement: &str) -> String {
    let lower = method_name.fold_lower();
    let is_russian = lower.chars().any(|c| c as u32 > 127);

    if is_russian {
        format!("Метод \"{}\" устарел. Следует использовать \"{}\".", method_name, replacement)
    } else {
        format!("Method \"{}\" is deprecated. You should use \"{}\".", method_name, replacement)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::{check_hir_diagnostic, format_diags};
    use expect_test::expect;
    #[test]
    fn test_deprecated_8310_russian() {
        let code = r#"
Процедура Тест()
    УстановитьКраткийЗаголовокПриложения("Заголовок");
КонецПроцедуры
"#;
        let diagnostics = check_hir_diagnostic(code);
        let deprecated_diags: Vec<_> = diagnostics
            .into_iter()
            .filter(|d| d.code == DiagnosticCode::DeprecatedPlatformApi)
            .collect();

        expect![[r#"
            DeprecatedPlatformApi @ 3:5..3:54
              message: Метод "УстановитьКраткийЗаголовокПриложения" устарел. Следует использовать "КлиентскоеПриложение.УстановитьКраткийЗаголовок".
              severity: Warning"#]].assert_eq(&format_diags(code, &deprecated_diags));
        assert!(deprecated_diags[0].message.contains("КлиентскоеПриложение"));
    }

    #[test]
    fn test_deprecated_8310_english() {
        let code = r#"
Procedure Test()
    Caption = GetShortApplicationCaption();
EndProcedure
"#;
        let diagnostics = check_hir_diagnostic(code);
        let deprecated_diags: Vec<_> = diagnostics
            .into_iter()
            .filter(|d| d.code == DiagnosticCode::DeprecatedPlatformApi)
            .collect();

        expect![[r#"
            DeprecatedPlatformApi @ 3:15..3:43
              message: Method "GetShortApplicationCaption" is deprecated. You should use "ClientApplication.GetShortCaption".
              severity: Warning"#]].assert_eq(&format_diags(code, &deprecated_diags));
        assert!(deprecated_diags[0].message.contains("ClientApplication"));
    }

    #[test]
    fn test_deprecated_8317_russian() {
        let code = r#"
Процедура Тест()
    Описание = КраткоеПредставлениеОшибки(ИнформацияОбОшибке());
КонецПроцедуры
"#;
        let diagnostics = check_hir_diagnostic(code);
        let deprecated_diags: Vec<_> = diagnostics
            .into_iter()
            .filter(|d| d.code == DiagnosticCode::DeprecatedPlatformApi)
            .collect();

        expect![[r#"
            DeprecatedPlatformApi @ 3:16..3:64
              message: Метод "КраткоеПредставлениеОшибки" устарел. Следует использовать "ОбработкаОшибок.КраткоеПредставлениеОшибки".
              severity: Warning"#]].assert_eq(&format_diags(code, &deprecated_diags));
        let message = &deprecated_diags[0].message;
        assert!(message.contains("ОбработкаОшибок"));
    }

    #[test]
    fn test_not_triggered_for_method_calls() {
        let code = r#"
Процедура Тест()
    // Метод объекта - не должен триггериться
    Модуль.ПолучитьКраткийЗаголовокПриложения();
КонецПроцедуры
"#;
        let diagnostics = check_hir_diagnostic(code);
        let deprecated_diags: Vec<_> = diagnostics
            .into_iter()
            .filter(|d| d.code == DiagnosticCode::DeprecatedPlatformApi)
            .collect();

        expect![[r#""#]].assert_eq(&format_diags(code, &deprecated_diags));
    }
}
