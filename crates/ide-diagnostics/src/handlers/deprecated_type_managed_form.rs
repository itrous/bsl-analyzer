use crate::AnalysisContext;
use crate::{Diagnostic, DiagnosticCode};
use bsl_platform::deprecation::DeprecationEntry;
use hir::LocalRange;

use super::deprecated_platform_facts::{
    canonical_name_for, is_russian_alias, managed_form_type_fact, replacement_for_name,
};

pub fn from_hir(
    type_name: &str,
    range: LocalRange,
    ctx: &AnalysisContext,
) -> Option<Diagnostic<LocalRange>> {
    let code = DiagnosticCode::DeprecatedPlatformApi;

    if ctx.is_disabled_with_metadata(code) {
        return None;
    }

    let fact = managed_form_type_fact(type_name)?;
    let message = get_message(type_name, fact)?;

    Some(Diagnostic {
        code,
        message,
        severity: ctx.severity(code),
        range,
        tags: ctx.tags(code),
        fixes: vec![],
    })
}

fn get_message(arg_value: &str, fact: &DeprecationEntry) -> Option<String> {
    let replacement = replacement_for_name(fact, arg_value)?;
    let deprecated = canonical_name_for(fact, arg_value)?;
    if is_russian_alias(fact, arg_value) {
        Some(format!(
            "Использование устаревшего типа \"{}\". Рекомендуется использовать \"{}\"",
            deprecated, replacement
        ))
    } else {
        Some(format!(
            "Usage of deprecated type \"{}\". Recommended to use \"{}\"",
            deprecated, replacement
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::*;
    use crate::Severity;
    use expect_test::expect;

    fn deprecated(code: &str) -> Vec<Diagnostic> {
        check_hir_diagnostic(code)
            .into_iter()
            .filter(|d| d.code == DiagnosticCode::DeprecatedPlatformApi)
            .collect()
    }

    #[test]
    fn test_current_form_type_and_plain_string_are_silent() {
        let code = r#"Функция ЭтоФормаПриложения(Окно)
	ИмяТипа = "УправляемаяФорма";
	Возврат ТипЗнч(Окно) = Тип("ФормаКлиентскогоПриложения");
КонецФункции
"#;
        let diagnostics = deprecated(code);
        expect![[r#""#]].assert_eq(&format_diags(code, &diagnostics));
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn test_deprecated_type_russian() {
        let code = r#"Функция ЭтоФормаПриложения(Окно)
	ИмяТипа = "ФормаКлиентскогоПриложения";
	Возврат ТипЗнч(Окно) = Тип("УправляемаяФорма");
КонецФункции
"#;
        let diagnostics = deprecated(code);
        expect![[r#"
            DeprecatedPlatformApi @ 3:29..3:47
              message: Использование устаревшего типа "УправляемаяФорма". Рекомендуется использовать "ФормаКлиентскогоПриложения"
              severity: Warning"#]].assert_eq(&format_diags(code, &diagnostics));
        assert_eq!(diagnostics[0].severity, Severity::Warning);
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn test_deprecated_type_english() {
        let code = r#"Function IsAppForm(Window)
	Return TypeOf(Window) = Type("ManagedForm");
EndFunction
"#;
        let diagnostics = deprecated(code);
        expect![[r#"
            DeprecatedPlatformApi @ 2:31..2:44
              message: Usage of deprecated type "ManagedForm". Recommended to use "ClientApplicationForm"
              severity: Warning"#]].assert_eq(&format_diags(code, &diagnostics));
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn test_case_insensitive() {
        let code = r#"Процедура СобратьТипы(Типы)
	Типы.Добавить(тип("управляемаяФОРМА"));
	Типы.Добавить(ТИП("УПРАВЛЯЕМАЯФОРМА"));
	Типы.Добавить(Type("managedform"));
	Типы.Добавить(TYPE("ManagedFORM"));
КонецПроцедуры
"#;
        let diagnostics = deprecated(code);
        expect![[r#"
            DeprecatedPlatformApi @ 2:20..2:38
              message: Использование устаревшего типа "УправляемаяФорма". Рекомендуется использовать "ФормаКлиентскогоПриложения"
              severity: Warning
            DeprecatedPlatformApi @ 3:20..3:38
              message: Использование устаревшего типа "УправляемаяФорма". Рекомендуется использовать "ФормаКлиентскогоПриложения"
              severity: Warning
            DeprecatedPlatformApi @ 4:21..4:34
              message: Usage of deprecated type "ManagedForm". Recommended to use "ClientApplicationForm"
              severity: Warning
            DeprecatedPlatformApi @ 5:21..5:34
              message: Usage of deprecated type "ManagedForm". Recommended to use "ClientApplicationForm"
              severity: Warning"#]].assert_eq(&format_diags(code, &diagnostics));
    }
}
