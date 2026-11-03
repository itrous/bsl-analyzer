use crate::define_metadata;
use crate::metadata::*;
use crate::AnalysisContext;
use crate::{Diagnostic, DiagnosticCode};
use hir::LocalRange;

pub const METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: DiagnosticType::Error,
    severity: DiagnosticSeverityLevel::Major,
    scope: DiagnosticScope::Bsl,
    modules: &[],
    minutes_to_fix: 15,
    activated_by_default: true,
    compatibility_mode: DiagnosticCompatibilityMode::Undefined,
    tags: &[MetadataTag::Error],
    can_locate_on_project: false,
    extra_min_for_complexity: 0.0,
    lsp_severity_override: "",
};

pub fn from_hir(
    method_name: &str,
    range: LocalRange,
    ctx: &AnalysisContext,
) -> Option<Diagnostic<LocalRange>> {
    let code = DiagnosticCode::GetFormMethod;

    if ctx.is_disabled_with_metadata(code) {
        return None;
    }
    // In an ordinary form `ПолучитьФорму` is the way to get a form.
    if ctx.is_ordinary_form_module() {
        return None;
    }

    Some(Diagnostic {
        code,
        message: format!(
            "Получение формы методом '{}' перед открытием требует лишнего серверного вызова: для открытия формы используйте 'ОткрытьФорму()', она делает это за один вызов (стандарт 487, п. 4.1)",
            method_name
        ),
        severity: ctx.severity(code),
        range,
        tags: ctx.tags(code),
        fixes: vec![],
    })
}

#[cfg(test)]
mod tests {
    use crate::test_utils::*;
    use crate::DiagnosticCode;
    use expect_test::expect;
    #[test]
    fn test_no_get_form() {
        let code = r#"
Процедура ОткрытьСправочник()
    ОткрытьФорму("Справочник.Номенклатура.ФормаСписка");
КонецПроцедуры
"#;
        let diagnostics = check_hir_diagnostic(code);
        let get_form_diags: Vec<_> =
            diagnostics.into_iter().filter(|d| d.code == DiagnosticCode::GetFormMethod).collect();
        expect![[r#""#]].assert_eq(&format_diags(code, &get_form_diags));
    }

    #[test]
    fn test_global_get_form_russian() {
        let code = r#"
Процедура Тест2()
    ФормаРедактора = ПолучитьФорму("Обработка.УниверсальныйРедактор.Форма");
КонецПроцедуры
"#;
        let diagnostics = check_hir_diagnostic(code);
        let get_form_diags: Vec<_> =
            diagnostics.into_iter().filter(|d| d.code == DiagnosticCode::GetFormMethod).collect();
        expect![[r#"
            GetFormMethod @ 3:22..3:35
              message: Получение формы методом 'ПолучитьФорму' перед открытием требует лишнего серверного вызова: для открытия формы используйте 'ОткрытьФорму()', она делает это за один вызов (стандарт 487, п. 4.1)
              severity: Major"#]].assert_eq(&format_diags(code, &get_form_diags));
        assert!(get_form_diags[0].message.contains("ПолучитьФорму"));
    }

    #[test]
    fn test_global_get_form_english() {
        let code = r#"
Procedure Test2()
    Form = GetForm("Document.PlanOperation.Form");
EndProcedure
"#;
        let diagnostics = check_hir_diagnostic(code);
        let get_form_diags: Vec<_> =
            diagnostics.into_iter().filter(|d| d.code == DiagnosticCode::GetFormMethod).collect();
        expect![[r#"
            GetFormMethod @ 3:12..3:19
              message: Получение формы методом 'GetForm' перед открытием требует лишнего серверного вызова: для открытия формы используйте 'ОткрытьФорму()', она делает это за один вызов (стандарт 487, п. 4.1)
              severity: Major"#]].assert_eq(&format_diags(code, &get_form_diags));
        assert!(get_form_diags[0].message.contains("GetForm"));
    }

    #[test]
    fn test_object_method_get_form_russian() {
        let code = r#"
Процедура Тест()
    Док = Документы.ЗаявкаНаОперацию.СоздатьДокумент();
    Форма = Док.ПолучитьФорму("ФормаДокумента");
КонецПроцедуры
"#;
        let diagnostics = check_hir_diagnostic(code);
        let get_form_diags: Vec<_> =
            diagnostics.into_iter().filter(|d| d.code == DiagnosticCode::GetFormMethod).collect();
        expect![[r#"
            GetFormMethod @ 4:17..4:30
              message: Получение формы методом 'ПолучитьФорму' перед открытием требует лишнего серверного вызова: для открытия формы используйте 'ОткрытьФорму()', она делает это за один вызов (стандарт 487, п. 4.1)
              severity: Major"#]].assert_eq(&format_diags(code, &get_form_diags));
    }

    #[test]
    fn test_object_method_get_form_english() {
        let code = r#"
Procedure Test()
    Doc = Documents.PlanOperation.CreateDocument();
    Form = Doc.GetForm("DocumentForm");
EndProcedure
"#;
        let diagnostics = check_hir_diagnostic(code);
        let get_form_diags: Vec<_> =
            diagnostics.into_iter().filter(|d| d.code == DiagnosticCode::GetFormMethod).collect();
        expect![[r#"
            GetFormMethod @ 4:16..4:23
              message: Получение формы методом 'GetForm' перед открытием требует лишнего серверного вызова: для открытия формы используйте 'ОткрытьФорму()', она делает это за один вызов (стандарт 487, п. 4.1)
              severity: Major"#]].assert_eq(&format_diags(code, &get_form_diags));
    }

    #[test]
    fn test_case_insensitive() {
        let code = r#"
Процедура Тест()
    Форма1 = получитьформу("Форма1");
    Форма2 = ПОЛУЧИТЬФОРМУ("Форма2");
    Форма3 = ПолучитьФОРМУ("Форма3");
    Форма4 = getform("Form4");
    Форма5 = GETFORM("Form5");
КонецПроцедуры
"#;
        let diagnostics = check_hir_diagnostic(code);
        let get_form_diags: Vec<_> =
            diagnostics.into_iter().filter(|d| d.code == DiagnosticCode::GetFormMethod).collect();
        expect![[r#"
            GetFormMethod @ 3:14..3:27
              message: Получение формы методом 'получитьформу' перед открытием требует лишнего серверного вызова: для открытия формы используйте 'ОткрытьФорму()', она делает это за один вызов (стандарт 487, п. 4.1)
              severity: Major
            GetFormMethod @ 4:14..4:27
              message: Получение формы методом 'ПОЛУЧИТЬФОРМУ' перед открытием требует лишнего серверного вызова: для открытия формы используйте 'ОткрытьФорму()', она делает это за один вызов (стандарт 487, п. 4.1)
              severity: Major
            GetFormMethod @ 5:14..5:27
              message: Получение формы методом 'ПолучитьФОРМУ' перед открытием требует лишнего серверного вызова: для открытия формы используйте 'ОткрытьФорму()', она делает это за один вызов (стандарт 487, п. 4.1)
              severity: Major
            GetFormMethod @ 6:14..6:21
              message: Получение формы методом 'getform' перед открытием требует лишнего серверного вызова: для открытия формы используйте 'ОткрытьФорму()', она делает это за один вызов (стандарт 487, п. 4.1)
              severity: Major
            GetFormMethod @ 7:14..7:21
              message: Получение формы методом 'GETFORM' перед открытием требует лишнего серверного вызова: для открытия формы используйте 'ОткрытьФорму()', она делает это за один вызов (стандарт 487, п. 4.1)
              severity: Major"#]].assert_eq(&format_diags(code, &get_form_diags));
    }

    #[test]
    fn test_multiple_calls() {
        let code = r#"
Процедура Тест()
    Форма1 = ПолучитьФорму("Форма1");
    Форма2 = GetForm("Form2");
    Док = Документы.Документ.СоздатьДокумент();
    Форма3 = Док.ПолучитьФорму("Форма3");
    Форма4 = Док.GetForm("Form4");
КонецПроцедуры
"#;
        let diagnostics = check_hir_diagnostic(code);
        let get_form_diags: Vec<_> =
            diagnostics.into_iter().filter(|d| d.code == DiagnosticCode::GetFormMethod).collect();
        expect![[r#"
            GetFormMethod @ 3:14..3:27
              message: Получение формы методом 'ПолучитьФорму' перед открытием требует лишнего серверного вызова: для открытия формы используйте 'ОткрытьФорму()', она делает это за один вызов (стандарт 487, п. 4.1)
              severity: Major
            GetFormMethod @ 4:14..4:21
              message: Получение формы методом 'GetForm' перед открытием требует лишнего серверного вызова: для открытия формы используйте 'ОткрытьФорму()', она делает это за один вызов (стандарт 487, п. 4.1)
              severity: Major
            GetFormMethod @ 6:18..6:31
              message: Получение формы методом 'ПолучитьФорму' перед открытием требует лишнего серверного вызова: для открытия формы используйте 'ОткрытьФорму()', она делает это за один вызов (стандарт 487, п. 4.1)
              severity: Major
            GetFormMethod @ 7:18..7:25
              message: Получение формы методом 'GetForm' перед открытием требует лишнего серверного вызова: для открытия формы используйте 'ОткрытьФорму()', она делает это за один вызов (стандарт 487, п. 4.1)
              severity: Major"#]].assert_eq(&format_diags(code, &get_form_diags));
    }

    #[test]
    fn test_detects_mixed_get_form_calls() {
        let code = r#"Процедура Тест()
    Док=Документы.ЗаявкаНаОперацию.СоздатьДокумент();
    Форма=Док.ПолучитьФорму("ФормаДокумента"); // Срабатывание здесь
КонецПроцедуры

Процедура Тест2()
    ФормаРедактора = ПолучитьФорму("Обработка.УниверсальныйРедактор.Форма"); // срабатывание здесь
КонецПроцедуры

Procedure Test()
    Doc = Documents.PlanOperation.CreateDocument();
    Form = Doc.GetForm("DocumentForm"); // срабатывание здесь
EndProcedure

Procedure Test2()
    Form = GetForm("Document.PlanOperation.Form"); // срабатывание здесь
EndProcedure
"#;
        let diagnostics = check_hir_diagnostic(code);
        let get_form_diags: Vec<_> =
            diagnostics.into_iter().filter(|d| d.code == DiagnosticCode::GetFormMethod).collect();

        expect![[r#"
            GetFormMethod @ 3:15..3:28
              message: Получение формы методом 'ПолучитьФорму' перед открытием требует лишнего серверного вызова: для открытия формы используйте 'ОткрытьФорму()', она делает это за один вызов (стандарт 487, п. 4.1)
              severity: Major
            GetFormMethod @ 7:22..7:35
              message: Получение формы методом 'ПолучитьФорму' перед открытием требует лишнего серверного вызова: для открытия формы используйте 'ОткрытьФорму()', она делает это за один вызов (стандарт 487, п. 4.1)
              severity: Major
            GetFormMethod @ 12:16..12:23
              message: Получение формы методом 'GetForm' перед открытием требует лишнего серверного вызова: для открытия формы используйте 'ОткрытьФорму()', она делает это за один вызов (стандарт 487, п. 4.1)
              severity: Major
            GetFormMethod @ 16:12..16:19
              message: Получение формы методом 'GetForm' перед открытием требует лишнего серверного вызова: для открытия формы используйте 'ОткрытьФорму()', она делает это за один вызов (стандарт 487, п. 4.1)
              severity: Major"#]].assert_eq(&format_diags(code, &get_form_diags));
    }
}
