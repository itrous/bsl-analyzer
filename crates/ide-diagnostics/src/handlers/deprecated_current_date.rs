use crate::BodyContext;
use crate::{Diagnostic, DiagnosticCode, Fix, TextEdit};
use bsl_platform::deprecation::{DeprecationEntry, LifecycleGroup};
use hir::LocalRange;

use super::deprecated_platform_facts::{
    canonical_name_for, global_function_fact, is_russian_alias, module_owns_bare_call,
    replacement_compiles_here, replacement_for_name,
};

pub fn from_hir(
    name: &str,
    range: LocalRange,
    ctx: &BodyContext,
) -> Option<Diagnostic<LocalRange>> {
    let code = DiagnosticCode::DeprecatedPlatformApi;

    if ctx.is_disabled_with_metadata(code) {
        return None;
    }

    let fact = global_function_fact(name, LifecycleGroup::DateTime)?;
    let message = get_message(name, fact)?;

    Some(Diagnostic {
        code,
        message,
        severity: ctx.severity(code),
        range,
        tags: ctx.tags(code),
        fixes: get_fix(name, fact, range, ctx).into_iter().collect(),
    })
}

/// `ТекущаяДата()` returns the local time of the computer the code runs on,
/// `ТекущаяДатаСеанса()` the time in the session's time zone. The two agree only
/// while both zones do, so the replacement can change a stored or compared value: a
/// manual fix, never part of `source.fixAll`. Not offered where the replacement
/// does not compile — an older platform, or a client environment the session-date
/// function is absent from (thin, web and mobile clients).
fn get_fix(
    name: &str,
    fact: &DeprecationEntry,
    range: LocalRange,
    ctx: &BodyContext,
) -> Option<Fix<LocalRange>> {
    let replacement = replacement_for_name(fact, name)?;
    if module_owns_bare_call(name, ctx) || module_owns_bare_call(replacement, ctx) {
        return None;
    }
    if !replacement_compiles_here(replacement, ctx) {
        return None;
    }
    let label = format!("Заменить на \"{}\" (время в часовом поясе сеанса)", replacement);
    Some(Fix::manual(label, vec![TextEdit { range, new_text: replacement.to_string() }]))
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
    Дата = ТекущаяДата();
КонецПроцедуры
"#;
        let diagnostics = check_hir_diagnostic(code);
        let deprecated_diags: Vec<_> = diagnostics
            .into_iter()
            .filter(|d| d.code == DiagnosticCode::DeprecatedPlatformApi)
            .collect();

        expect![[r#"
            DeprecatedPlatformApi @ 3:12..3:23
              message: Используйте "ТекущаяДатаСеанса" вместо устаревшего "ТекущаяДата"
              severity: Warning"#]]
        .assert_eq(&format_diags(code, &deprecated_diags));
        assert_eq!(deprecated_diags[0].severity, Severity::Warning);
        assert!(deprecated_diags[0].message.contains("ТекущаяДатаСеанса"));
    }

    #[test]
    fn test_deprecated_english() {
        let code = r#"
Procedure Test()
    Date = CurrentDate();
EndProcedure
"#;
        let diagnostics = check_hir_diagnostic(code);
        let deprecated_diags: Vec<_> = diagnostics
            .into_iter()
            .filter(|d| d.code == DiagnosticCode::DeprecatedPlatformApi)
            .collect();

        expect![[r#"
            DeprecatedPlatformApi @ 3:12..3:23
              message: Use "CurrentSessionDate" instead of deprecated "CurrentDate"
              severity: Warning"#]]
        .assert_eq(&format_diags(code, &deprecated_diags));
        assert!(deprecated_diags[0].message.contains("CurrentSessionDate"));
    }

    #[test]
    fn test_object_method_excluded() {
        let code = r#"
Процедура Тест()
    Дата = Модуль.ТекущаяДата();
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
    Дата1 = ТЕКУЩАЯДАТА();
    Дата2 = текущаядата();
    Дата3 = ТекущаяДата();
КонецПроцедуры
"#;
        let diagnostics = check_hir_diagnostic(code);
        let deprecated_diags: Vec<_> = diagnostics
            .into_iter()
            .filter(|d| d.code == DiagnosticCode::DeprecatedPlatformApi)
            .collect();

        expect![[r#"
            DeprecatedPlatformApi @ 3:13..3:24
              message: Используйте "ТекущаяДатаСеанса" вместо устаревшего "ТекущаяДата"
              severity: Warning
            DeprecatedPlatformApi @ 4:13..4:24
              message: Используйте "ТекущаяДатаСеанса" вместо устаревшего "ТекущаяДата"
              severity: Warning
            DeprecatedPlatformApi @ 5:13..5:24
              message: Используйте "ТекущаяДатаСеанса" вместо устаревшего "ТекущаяДата"
              severity: Warning"#]]
        .assert_eq(&format_diags(code, &deprecated_diags));
    }

    #[test]
    fn test_two_procs_one_each_language() {
        let code = r#"
Процедура А()
    ДатаПроверки = ТекущаяДата();
КонецПроцедуры

Процедура Б()
    ДатаПроверки = ТекущаяДатаСеанса();
    Модуль.ТекущаяДата();
КонецПроцедуры

Procedure A()
    CheckDate = CurrentDate();
EndProcedure"#;
        let diagnostics = check_hir_diagnostic(code);
        let deprecated_diags: Vec<_> = diagnostics
            .into_iter()
            .filter(|d| d.code == DiagnosticCode::DeprecatedPlatformApi)
            .collect();
        expect![[r#"
            DeprecatedPlatformApi @ 3:20..3:31
              message: Используйте "ТекущаяДатаСеанса" вместо устаревшего "ТекущаяДата"
              severity: Warning
            DeprecatedPlatformApi @ 12:17..12:28
              message: Use "CurrentSessionDate" instead of deprecated "CurrentDate"
              severity: Warning"#]]
        .assert_eq(&format_diags(code, &deprecated_diags));
    }
}
