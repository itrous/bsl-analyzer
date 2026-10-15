use crate::BodyContext;
use crate::{Diagnostic, DiagnosticCode, Fix, TextEdit};
use bsl_platform::deprecation::{DeprecationEntry, LifecycleGroup};
use hir::LocalRange;

use super::deprecated_platform_api;
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
    if deprecated_platform_api::is_excluded(ctx.config, name) {
        return None;
    }

    let fact = global_function_fact(name, LifecycleGroup::StringSearch)?;
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

/// `СтрНайти(Строка, ПодстрокаПоиска)` behaves as `Найти(Строка, ПодстрокаПоиска)`:
/// the same two leading parameters, case-sensitive search, the same result (0 when
/// not found, 1 for an empty substring). Renaming the call is therefore safe.
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
    let label = format!("Заменить на \"{}\"", replacement);
    Some(Fix::safe(label, vec![TextEdit { range, new_text: replacement.to_string() }]))
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
    Позиция = Найти("Строка", "о");
КонецПроцедуры
"#;
        let diagnostics = check_hir_diagnostic(code);
        let deprecated_diags: Vec<_> = diagnostics
            .into_iter()
            .filter(|d| d.code == DiagnosticCode::DeprecatedPlatformApi)
            .collect();

        expect![[r#"
            DeprecatedPlatformApi @ 3:15..3:20
              message: Используйте "СтрНайти" вместо устаревшего "Найти"
              severity: Warning"#]]
        .assert_eq(&format_diags(code, &deprecated_diags));
        assert_eq!(deprecated_diags[0].severity, Severity::Warning);
        assert!(deprecated_diags[0].message.contains("СтрНайти"));
    }

    #[test]
    fn test_deprecated_english() {
        let code = r#"
Procedure Test()
    Position = Find("String", "S");
EndProcedure
"#;
        let diagnostics = check_hir_diagnostic(code);
        let deprecated_diags: Vec<_> = diagnostics
            .into_iter()
            .filter(|d| d.code == DiagnosticCode::DeprecatedPlatformApi)
            .collect();

        expect![[r#"
            DeprecatedPlatformApi @ 3:16..3:20
              message: Use "StrFind" instead of deprecated "Find"
              severity: Warning"#]]
        .assert_eq(&format_diags(code, &deprecated_diags));
        assert!(deprecated_diags[0].message.contains("StrFind"));
    }

    #[test]
    fn test_collection_method_excluded() {
        let code = r#"
Процедура Тест()
    Индекс = Массив.Найти("Элемент");
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
    Поз1 = НАЙТИ("A", "B");
    Поз2 = найти("C", "D");
    Поз3 = Найти("E", "F");
    Поз4 = НайтИ("G", "H");
КонецПроцедуры
"#;
        let diagnostics = check_hir_diagnostic(code);
        let deprecated_diags: Vec<_> = diagnostics
            .into_iter()
            .filter(|d| d.code == DiagnosticCode::DeprecatedPlatformApi)
            .collect();

        expect![[r#"
            DeprecatedPlatformApi @ 3:12..3:17
              message: Используйте "СтрНайти" вместо устаревшего "Найти"
              severity: Warning
            DeprecatedPlatformApi @ 4:12..4:17
              message: Используйте "СтрНайти" вместо устаревшего "Найти"
              severity: Warning
            DeprecatedPlatformApi @ 5:12..5:17
              message: Используйте "СтрНайти" вместо устаревшего "Найти"
              severity: Warning
            DeprecatedPlatformApi @ 6:12..6:17
              message: Используйте "СтрНайти" вместо устаревшего "Найти"
              severity: Warning"#]]
        .assert_eq(&format_diags(code, &deprecated_diags));
    }

    #[test]
    fn test_in_proc_and_toplevel() {
        let code = r#"
Процедура А()

   Если НайтИ(Сотрудник.Имя, "Борис") > 0 Тогда
       Представление = Сотрудник.Имя + " таб. №" + Сотрудник.Код;
   КонецЕсли;

КонецПроцедуры

If FinD("A", "B") Then
EndIf;"#;
        let diagnostics = check_hir_diagnostic(code);
        let deprecated_diags: Vec<_> = diagnostics
            .into_iter()
            .filter(|d| d.code == DiagnosticCode::DeprecatedPlatformApi)
            .collect();
        expect![[r#"
            DeprecatedPlatformApi @ 4:9..4:14
              message: Используйте "СтрНайти" вместо устаревшего "Найти"
              severity: Warning
            DeprecatedPlatformApi @ 10:4..10:8
              message: Use "StrFind" instead of deprecated "Find"
              severity: Warning"#]]
        .assert_eq(&format_diags(code, &deprecated_diags));
    }
}
