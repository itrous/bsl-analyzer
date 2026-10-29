use crate::define_metadata;
use crate::metadata::*;
use crate::{Diagnostic, DiagnosticCode, DiagnosticsConfig, DiagnosticsContext};
use sdbl_hir;

// CodeSmell, not Error: an unguarded outer-join field is a standard-conformance
// recommendation, and the vendor codebase legitimately mass-produces the
// pattern (self-joins where NULL is impossible by construction). Warning-level
// keeps the signal without drowning real errors.
pub const METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: DiagnosticType::CodeSmell,
    severity: DiagnosticSeverityLevel::Critical,
    scope: DiagnosticScope::All,
    modules: &[],
    minutes_to_fix: 2,
    activated_by_default: true,
    compatibility_mode: DiagnosticCompatibilityMode::Undefined,
    tags: &[MetadataTag::Sql, MetadataTag::Suspicious, MetadataTag::Unpredictable],
    can_locate_on_project: false,
    extra_min_for_complexity: 0.0,
    lsp_severity_override: "",
};

pub(crate) fn dispatch(
    config: &DiagnosticsConfig,
    diag: &sdbl_hir::SdblDiagnostic,
    mapper: &crate::sdbl_utils::SdblPositionMapper,
    query_text: &str,
    diagnostics: &mut Vec<Diagnostic>,
) {
    if let sdbl_hir::SdblDiagnostic::FieldsFromJoinWithoutNullCheck {
        join_type,
        unprotected_fields,
        ..
    } = diag
    {
        let code = DiagnosticCode::FieldsFromJoinsWithoutIsNull;
        let join_type_str = match join_type {
            sdbl_hir::JoinType::Left => "ЛЕВОГО СОЕДИНЕНИЯ",
            sdbl_hir::JoinType::Right => "ПРАВОГО СОЕДИНЕНИЯ",
            sdbl_hir::JoinType::Full => "ПОЛНОГО СОЕДИНЕНИЯ",
            _ => "СОЕДИНЕНИЯ",
        };
        let message = format!(
            "Для полей из {} добавьте проверку через ЕСТЬ NULL или используйте функцию ЕСТЬNULL, либо замените на ВНУТРЕННЕЕ СОЕДИНЕНИЕ",
            join_type_str
        );
        for field_ref in unprotected_fields {
            let bsl_range = mapper.map_range(field_ref.range, query_text);
            diagnostics.push(Diagnostic {
                code,
                message: message.clone(),
                severity: config.severity(code),
                range: bsl_range,
                tags: config.tags(code),
                fixes: vec![],
            });
        }
    } else if let sdbl_hir::SdblDiagnostic::ScheduleDataFieldWithoutNullCheck {
        unprotected_fields,
        ..
    } = diag
    {
        let code = DiagnosticCode::FieldsFromJoinsWithoutIsNull;
        for field_ref in unprotected_fields {
            diagnostics.push(Diagnostic {
                code,
                message: sdbl_hir::SCHEDULE_DATA_NULL_MESSAGE.to_string(),
                severity: config.severity(code),
                range: mapper.map_range(field_ref.range, query_text),
                tags: config.tags(code),
                fixes: vec![],
            });
        }
    }
}

pub fn check(ctx: &DiagnosticsContext) -> Vec<Diagnostic> {
    crate::sdbl_utils::collect_sdbl_via_dispatch(
        ctx,
        DiagnosticCode::FieldsFromJoinsWithoutIsNull,
        dispatch,
    )
}

#[cfg(test)]
mod tests {
    use crate::test_utils::check_diagnostics_snapshot_for;
    use crate::DiagnosticCode;
    use expect_test::expect;

    fn check(code: &str, expected: expect_test::Expect) {
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::FieldsFromJoinsWithoutIsNull,
            expected,
        );
    }

    #[test]
    fn test_left_join_unprotected_field() {
        let code = r#"Процедура ВыбратьДоставки(Запрос)
	Запрос.Текст =
	"ВЫБРАТЬ Курьеры.Телефон
	|ИЗ Документ.Заказ КАК Заказы
	|ЛЕВОЕ СОЕДИНЕНИЕ Справочник.Курьеры КАК Курьеры
	|ПО Заказы.Курьер = Курьеры.Ссылка";
КонецПроцедуры
"#;
        check(
            code,
            expect![[r#"
            FieldsFromJoinsWithoutIsNull @ 3:11..3:26
              message: Для полей из ЛЕВОГО СОЕДИНЕНИЯ добавьте проверку через ЕСТЬ NULL или используйте функцию ЕСТЬNULL, либо замените на ВНУТРЕННЕЕ СОЕДИНЕНИЕ
              severity: Warning"#]],
        );
    }

    #[test]
    fn test_left_join_with_isnull_protected() {
        // Only the raw occurrence is reported; the ЕСТЬNULL-wrapped one is protected.
        let code = r#"Процедура ВыбратьДоставки(Запрос)
	Запрос.Текст =
	"ВЫБРАТЬ Курьеры.Телефон,
	|	ЕСТЬNULL(Курьеры.Телефон, """")
	|ИЗ Документ.Заказ КАК Заказы
	|ЛЕВОЕ СОЕДИНЕНИЕ Справочник.Курьеры КАК Курьеры
	|ПО Заказы.Курьер = Курьеры.Ссылка";
КонецПроцедуры
"#;
        check(
            code,
            expect![[r#"
            FieldsFromJoinsWithoutIsNull @ 3:11..3:26
              message: Для полей из ЛЕВОГО СОЕДИНЕНИЯ добавьте проверку через ЕСТЬ NULL или используйте функцию ЕСТЬNULL, либо замените на ВНУТРЕННЕЕ СОЕДИНЕНИЕ
              severity: Warning"#]],
        );
    }

    #[test]
    fn test_left_join_where_clause_unprotected() {
        let code = r#"Процедура ВыбратьДоставки(Запрос)
	Запрос.Текст =
	"ВЫБРАТЬ Заказы.Номер
	|ИЗ Документ.Заказ КАК Заказы
	|ЛЕВОЕ СОЕДИНЕНИЕ Справочник.Курьеры КАК Курьеры
	|ПО Заказы.Курьер = Курьеры.Ссылка
	|ГДЕ Курьеры.НаСмене
	|И ЕСТЬNULL(Курьеры.НаСмене, Ложь)";
КонецПроцедуры
"#;
        check(
            code,
            expect![[r#"
            FieldsFromJoinsWithoutIsNull @ 7:7..7:22
              message: Для полей из ЛЕВОГО СОЕДИНЕНИЯ добавьте проверку через ЕСТЬ NULL или используйте функцию ЕСТЬNULL, либо замените на ВНУТРЕННЕЕ СОЕДИНЕНИЕ
              severity: Warning"#]],
        );
    }

    #[test]
    fn test_right_join_unprotected_field() {
        let code = r#"Процедура ВыбратьДоставки(Запрос)
	Запрос.Текст =
	"ВЫБРАТЬ Заказы.Номер,
	|	ЕСТЬNULL(Заказы.Сумма, 0)
	|ИЗ Документ.Заказ КАК Заказы
	|ПРАВОЕ СОЕДИНЕНИЕ Справочник.Курьеры КАК Курьеры
	|ПО Заказы.Курьер = Курьеры.Ссылка";
КонецПроцедуры
"#;
        check(
            code,
            expect![[r#"
            FieldsFromJoinsWithoutIsNull @ 3:11..3:23
              message: Для полей из ПРАВОГО СОЕДИНЕНИЯ добавьте проверку через ЕСТЬ NULL или используйте функцию ЕСТЬNULL, либо замените на ВНУТРЕННЕЕ СОЕДИНЕНИЕ
              severity: Warning"#]],
        );
    }

    #[test]
    fn test_inner_join_no_diagnostic() {
        let code = r#"Процедура ВыбратьДоставки(Запрос)
	Запрос.Текст =
	"ВЫБРАТЬ Курьеры.Телефон,
	|	Заказы.Номер
	|ИЗ Документ.Заказ КАК Заказы
	|ВНУТРЕННЕЕ СОЕДИНЕНИЕ Справочник.Курьеры КАК Курьеры
	|ПО Заказы.Курьер = Курьеры.Ссылка";
КонецПроцедуры
"#;
        check(code, expect![[r#""#]]);
    }

    #[test]
    fn test_full_join_multiple_unprotected_fields() {
        let code = r#"Процедура ВыбратьДоставки(Запрос)
	Запрос.Текст =
	"ВЫБРАТЬ Курьеры.Телефон,
	|	Заказы.Номер,
	|	Курьеры.Транспорт,
	|	ЕСТЬNULL(Курьеры.Телефон, """"),
	|	ЕСТЬNULL(Заказы.Номер, """")
	|ИЗ Документ.Заказ КАК Заказы
	|ПОЛНОЕ СОЕДИНЕНИЕ Справочник.Курьеры КАК Курьеры
	|ПО Заказы.Курьер = Курьеры.Ссылка";
КонецПроцедуры
"#;
        check(
            code,
            expect![[r#"
            FieldsFromJoinsWithoutIsNull @ 3:11..3:26
              message: Для полей из ПОЛНОГО СОЕДИНЕНИЯ добавьте проверку через ЕСТЬ NULL или используйте функцию ЕСТЬNULL, либо замените на ВНУТРЕННЕЕ СОЕДИНЕНИЕ
              severity: Warning
            FieldsFromJoinsWithoutIsNull @ 4:4..4:16
              message: Для полей из ПОЛНОГО СОЕДИНЕНИЯ добавьте проверку через ЕСТЬ NULL или используйте функцию ЕСТЬNULL, либо замените на ВНУТРЕННЕЕ СОЕДИНЕНИЕ
              severity: Warning
            FieldsFromJoinsWithoutIsNull @ 5:4..5:21
              message: Для полей из ПОЛНОГО СОЕДИНЕНИЯ добавьте проверку через ЕСТЬ NULL или используйте функцию ЕСТЬNULL, либо замените на ВНУТРЕННЕЕ СОЕДИНЕНИЕ
              severity: Warning"#]],
        );
    }

    #[test]
    fn test_left_join_is_not_null_in_where_exempts() {
        let code = r#"Процедура ВыбратьДоставки(Запрос)
	Запрос.Текст =
	"ВЫБРАТЬ Курьеры.Телефон
	|ИЗ Документ.Заказ КАК Заказы
	|ЛЕВОЕ СОЕДИНЕНИЕ Справочник.Курьеры КАК Курьеры
	|ПО Заказы.Курьер = Курьеры.Ссылка
	|ГДЕ (Курьеры.Транспорт ЕСТЬ НЕ NULL)";
КонецПроцедуры
"#;
        check(code, expect![[r#""#]]);
    }

    #[test]
    fn test_is_null_in_where_does_not_exempt_select() {
        let code = r#"Процедура ВыбратьДоставки(Запрос)
	Запрос.Текст =
	"ВЫБРАТЬ Курьеры.Телефон
	|ИЗ Документ.Заказ КАК Заказы
	|ЛЕВОЕ СОЕДИНЕНИЕ Справочник.Курьеры КАК Курьеры
	|ПО Заказы.Курьер = Курьеры.Ссылка
	|ГДЕ Курьеры.Транспорт ЕСТЬ NULL";
КонецПроцедуры
"#;
        check(
            code,
            expect![[r#"
            FieldsFromJoinsWithoutIsNull @ 3:11..3:26
              message: Для полей из ЛЕВОГО СОЕДИНЕНИЯ добавьте проверку через ЕСТЬ NULL или используйте функцию ЕСТЬNULL, либо замените на ВНУТРЕННЕЕ СОЕДИНЕНИЕ
              severity: Warning"#]],
        );
    }

    #[test]
    fn case_else_guarded_by_when_is_null_silent() {
        let code = r#"Процедура ВыбратьДоставки(Запрос)
	Запрос.Текст =
	"ВЫБРАТЬ ВЫБОР
	|		КОГДА Курьеры.Ссылка ЕСТЬ NULL
	|			ТОГДА ""самовывоз""
	|		ИНАЧЕ Курьеры.Телефон
	|	КОНЕЦ КАК Контакт
	|ИЗ Документ.Заказ КАК Заказы
	|ЛЕВОЕ СОЕДИНЕНИЕ Справочник.Курьеры КАК Курьеры
	|ПО Заказы.Курьер = Курьеры.Ссылка";
КонецПроцедуры
"#;
        check(code, expect![[r#""#]]);
    }

    #[test]
    fn case_then_guarded_by_is_not_null_silent() {
        let code = r#"Процедура ВыбратьДоставки(Запрос)
	Запрос.Текст =
	"ВЫБРАТЬ ВЫБОР
	|		КОГДА Курьеры.Ссылка ЕСТЬ НЕ NULL
	|			ТОГДА Курьеры.Телефон
	|		ИНАЧЕ """"
	|	КОНЕЦ КАК Контакт
	|ИЗ Документ.Заказ КАК Заказы
	|ЛЕВОЕ СОЕДИНЕНИЕ Справочник.Курьеры КАК Курьеры
	|ПО Заказы.Курьер = Курьеры.Ссылка";
КонецПроцедуры
"#;
        check(code, expect![[r#""#]]);
    }

    #[test]
    fn case_then_guarded_by_isnull_comparison_silent() {
        let code = r#"Процедура ВыбратьДоставки(Запрос)
	Запрос.Текст =
	"ВЫБРАТЬ ВЫБОР
	|		КОГДА ЕСТЬNULL(Курьеры.НаСмене, ЛОЖЬ) <> ЛОЖЬ
	|			ТОГДА Курьеры.Телефон
	|		ИНАЧЕ """"
	|	КОНЕЦ КАК Контакт
	|ИЗ Документ.Заказ КАК Заказы
	|ЛЕВОЕ СОЕДИНЕНИЕ Справочник.Курьеры КАК Курьеры
	|ПО Заказы.Курьер = Курьеры.Ссылка";
КонецПроцедуры
"#;
        check(code, expect![[r#""#]]);
    }

    #[test]
    fn case_following_branches_guarded_by_first_is_null_silent() {
        let code = r#"Процедура ВыбратьДоставки(Запрос)
	Запрос.Текст =
	"ВЫБРАТЬ ВЫБОР
	|		КОГДА Курьеры.Ссылка ЕСТЬ NULL
	|			ТОГДА """"
	|		КОГДА Курьеры.НаСмене
	|			ТОГДА Курьеры.Телефон
	|		ИНАЧЕ Курьеры.Транспорт
	|	КОНЕЦ КАК Контакт
	|ИЗ Документ.Заказ КАК Заказы
	|ЛЕВОЕ СОЕДИНЕНИЕ Справочник.Курьеры КАК Курьеры
	|ПО Заказы.Курьер = Курьеры.Ссылка";
КонецПроцедуры
"#;
        check(code, expect![[r#""#]]);
    }

    #[test]
    fn where_disjunction_with_is_null_guards_other_operand() {
        let code = r#"Процедура ВыбратьДоставки(Запрос)
	Запрос.Текст =
	"ВЫБРАТЬ Заказы.Номер
	|ИЗ Документ.Заказ КАК Заказы
	|ЛЕВОЕ СОЕДИНЕНИЕ Справочник.Курьеры КАК Курьеры
	|ПО Заказы.Курьер = Курьеры.Ссылка
	|ГДЕ (Курьеры.Ссылка ЕСТЬ NULL ИЛИ Курьеры.Район <> Заказы.Район)";
КонецПроцедуры
"#;
        check(code, expect![[r#""#]]);
    }

    #[test]
    fn case_else_after_is_null_conjunction_still_fires() {
        let code = r#"Процедура ВыбратьДоставки(Запрос)
	Запрос.Текст =
	"ВЫБРАТЬ ВЫБОР
	|		КОГДА Курьеры.Ссылка ЕСТЬ NULL И Заказы.Срочный
	|			ТОГДА """"
	|		ИНАЧЕ Курьеры.Телефон
	|	КОНЕЦ КАК Контакт
	|ИЗ Документ.Заказ КАК Заказы
	|ЛЕВОЕ СОЕДИНЕНИЕ Справочник.Курьеры КАК Курьеры
	|ПО Заказы.Курьер = Курьеры.Ссылка";
КонецПроцедуры
"#;
        check(
            code,
            expect![[r#"
            FieldsFromJoinsWithoutIsNull @ 6:11..6:26
              message: Для полей из ЛЕВОГО СОЕДИНЕНИЯ добавьте проверку через ЕСТЬ NULL или используйте функцию ЕСТЬNULL, либо замените на ВНУТРЕННЕЕ СОЕДИНЕНИЕ
              severity: Warning"#]],
        );
    }

    #[test]
    fn case_guard_for_other_table_still_fires() {
        let code = r#"Процедура ВыбратьДоставки(Запрос)
	Запрос.Текст =
	"ВЫБРАТЬ ВЫБОР
	|		КОГДА Заказы.Ссылка ЕСТЬ NULL
	|			ТОГДА """"
	|		ИНАЧЕ Курьеры.Телефон
	|	КОНЕЦ КАК Контакт
	|ИЗ Документ.Заказ КАК Заказы
	|ЛЕВОЕ СОЕДИНЕНИЕ Справочник.Курьеры КАК Курьеры
	|ПО Заказы.Курьер = Курьеры.Ссылка";
КонецПроцедуры
"#;
        check(
            code,
            expect![[r#"
            FieldsFromJoinsWithoutIsNull @ 6:11..6:26
              message: Для полей из ЛЕВОГО СОЕДИНЕНИЯ добавьте проверку через ЕСТЬ NULL или используйте функцию ЕСТЬNULL, либо замените на ВНУТРЕННЕЕ СОЕДИНЕНИЕ
              severity: Warning"#]],
        );
    }

    #[test]
    fn case_then_after_is_not_null_disjunction_still_fires() {
        let code = r#"Процедура ВыбратьДоставки(Запрос)
	Запрос.Текст =
	"ВЫБРАТЬ ВЫБОР
	|		КОГДА Курьеры.Ссылка ЕСТЬ НЕ NULL ИЛИ Заказы.Срочный
	|			ТОГДА Курьеры.Телефон
	|		ИНАЧЕ """"
	|	КОНЕЦ КАК Контакт
	|ИЗ Документ.Заказ КАК Заказы
	|ЛЕВОЕ СОЕДИНЕНИЕ Справочник.Курьеры КАК Курьеры
	|ПО Заказы.Курьер = Курьеры.Ссылка";
КонецПроцедуры
"#;
        check(
            code,
            expect![[r#"
            FieldsFromJoinsWithoutIsNull @ 5:12..5:27
              message: Для полей из ЛЕВОГО СОЕДИНЕНИЯ добавьте проверку через ЕСТЬ NULL или используйте функцию ЕСТЬNULL, либо замените на ВНУТРЕННЕЕ СОЕДИНЕНИЕ
              severity: Warning"#]],
        );
    }

    #[test]
    fn case_then_after_isnull_equality_still_fires() {
        // ЕСТЬNULL(Поле, ЛОЖЬ) = ЛОЖЬ selects ТОГДА exactly when the row is absent (the fallback
        // satisfies the equality), so the raw field in ТОГДА is a genuine NULL hazard.
        let code = r#"Процедура ВыбратьДоставки(Запрос)
	Запрос.Текст =
	"ВЫБРАТЬ ВЫБОР
	|		КОГДА ЕСТЬNULL(Курьеры.НаСмене, ЛОЖЬ) = ЛОЖЬ
	|			ТОГДА Курьеры.Телефон
	|		ИНАЧЕ """"
	|	КОНЕЦ КАК Контакт
	|ИЗ Документ.Заказ КАК Заказы
	|ЛЕВОЕ СОЕДИНЕНИЕ Справочник.Курьеры КАК Курьеры
	|ПО Заказы.Курьер = Курьеры.Ссылка";
КонецПроцедуры
"#;
        check(
            code,
            expect![[r#"
            FieldsFromJoinsWithoutIsNull @ 5:12..5:27
              message: Для полей из ЛЕВОГО СОЕДИНЕНИЯ добавьте проверку через ЕСТЬ NULL или используйте функцию ЕСТЬNULL, либо замените на ВНУТРЕННЕЕ СОЕДИНЕНИЕ
              severity: Warning"#]],
        );
    }

    #[test]
    fn case_then_after_isnull_inequality_with_other_literal_still_fires() {
        // ЕСТЬNULL(Поле, ЛОЖЬ) <> ИСТИНА is TRUE on an absent row (fallback ЛОЖЬ differs from
        // ИСТИНА), so ТОГДА executes with NULL fields.
        let code = r#"Процедура ВыбратьДоставки(Запрос)
	Запрос.Текст =
	"ВЫБРАТЬ ВЫБОР
	|		КОГДА ЕСТЬNULL(Курьеры.НаСмене, ЛОЖЬ) <> ИСТИНА
	|			ТОГДА Курьеры.Телефон
	|		ИНАЧЕ """"
	|	КОНЕЦ КАК Контакт
	|ИЗ Документ.Заказ КАК Заказы
	|ЛЕВОЕ СОЕДИНЕНИЕ Справочник.Курьеры КАК Курьеры
	|ПО Заказы.Курьер = Курьеры.Ссылка";
КонецПроцедуры
"#;
        check(
            code,
            expect![[r#"
            FieldsFromJoinsWithoutIsNull @ 5:12..5:27
              message: Для полей из ЛЕВОГО СОЕДИНЕНИЯ добавьте проверку через ЕСТЬ NULL или используйте функцию ЕСТЬNULL, либо замените на ВНУТРЕННЕЕ СОЕДИНЕНИЕ
              severity: Warning"#]],
        );
    }

    #[test]
    fn case_else_after_wrapped_is_null_still_fires() {
        // ЕСТЬNULL(…) never yields NULL, so `ЕСТЬNULL(…) ЕСТЬ NULL` is constant-false: ИНАЧЕ
        // always executes, including on absent rows.
        let code = r#"Процедура ВыбратьДоставки(Запрос)
	Запрос.Текст =
	"ВЫБРАТЬ ВЫБОР
	|		КОГДА ЕСТЬNULL(Курьеры.Стаж, 0) ЕСТЬ NULL
	|			ТОГДА """"
	|		ИНАЧЕ Курьеры.Телефон
	|	КОНЕЦ КАК Контакт
	|ИЗ Документ.Заказ КАК Заказы
	|ЛЕВОЕ СОЕДИНЕНИЕ Справочник.Курьеры КАК Курьеры
	|ПО Заказы.Курьер = Курьеры.Ссылка";
КонецПроцедуры
"#;
        check(
            code,
            expect![[r#"
            FieldsFromJoinsWithoutIsNull @ 6:11..6:26
              message: Для полей из ЛЕВОГО СОЕДИНЕНИЯ добавьте проверку через ЕСТЬ NULL или используйте функцию ЕСТЬNULL, либо замените на ВНУТРЕННЕЕ СОЕДИНЕНИЕ
              severity: Warning"#]],
        );
    }

    #[test]
    fn case_then_after_wrapped_is_not_null_still_fires() {
        // ЕСТЬNULL(…) never yields NULL, so `ЕСТЬNULL(…) ЕСТЬ НЕ NULL` is constant-true: ТОГДА
        // executes on absent rows too.
        let code = r#"Процедура ВыбратьДоставки(Запрос)
	Запрос.Текст =
	"ВЫБРАТЬ ВЫБОР
	|		КОГДА ЕСТЬNULL(Курьеры.Стаж, 0) ЕСТЬ НЕ NULL
	|			ТОГДА Курьеры.Телефон
	|		ИНАЧЕ """"
	|	КОНЕЦ КАК Контакт
	|ИЗ Документ.Заказ КАК Заказы
	|ЛЕВОЕ СОЕДИНЕНИЕ Справочник.Курьеры КАК Курьеры
	|ПО Заказы.Курьер = Курьеры.Ссылка";
КонецПроцедуры
"#;
        check(
            code,
            expect![[r#"
            FieldsFromJoinsWithoutIsNull @ 5:12..5:27
              message: Для полей из ЛЕВОГО СОЕДИНЕНИЯ добавьте проверку через ЕСТЬ NULL или используйте функцию ЕСТЬNULL, либо замените на ВНУТРЕННЕЕ СОЕДИНЕНИЕ
              severity: Warning"#]],
        );
    }

    #[test]
    fn left_outer_join_isnull_wrapped_field() {
        let code = r#"Процедура ВыбратьДоставки(Запрос)
	Запрос.Текст =
	"ВЫБРАТЬ ЕСТЬNULL(Курьеры.Ссылка, ЗНАЧЕНИЕ(Справочник.Курьеры.ПустаяСсылка)) КАК Курьер
	|ИЗ Документ.Заказ КАК Заказы
	|ЛЕВОЕ ВНЕШНЕЕ СОЕДИНЕНИЕ Справочник.Курьеры КАК Курьеры
	|ПО Заказы.Курьер = Курьеры.Ссылка";
КонецПроцедуры
"#;
        check(code, expect![[r#""#]]);
    }

    #[test]
    fn right_outer_join_classification() {
        let code = r#"Процедура ВыбратьДоставки(Запрос)
	Запрос.Текст =
	"ВЫБРАТЬ Заказы.Номер КАК Номер
	|ИЗ Документ.Заказ КАК Заказы
	|ПРАВОЕ ВНЕШНЕЕ СОЕДИНЕНИЕ Справочник.Курьеры КАК Курьеры
	|ПО Заказы.Курьер = Курьеры.Ссылка";
КонецПроцедуры
"#;
        check(
            code,
            expect![[r#"
            FieldsFromJoinsWithoutIsNull @ 3:11..3:23
              message: Для полей из ПРАВОГО СОЕДИНЕНИЯ добавьте проверку через ЕСТЬ NULL или используйте функцию ЕСТЬNULL, либо замените на ВНУТРЕННЕЕ СОЕДИНЕНИЕ
              severity: Warning"#]],
        );
    }

    #[test]
    fn test_no_diagnostic_for_fields_in_join_conditions() {
        // Fields of the nullable side used only in ПО conditions, inside ЕСТЬNULL and
        // under a ВЫБОР guard are not reported; nested joins chain the nullable side.
        let code = r#"Процедура ВыбратьКлиентов(Запрос)
	Запрос.Текст = "ВЫБРАТЬ РАЗЛИЧНЫЕ
	|	Клиенты.Ссылка КАК Клиент,
	|	ЕСТЬNULL(Телефоны.Номер, """") КАК Телефон,
	|	ВЫБОР
	|		КОГДА Скидки.Ссылка ЕСТЬ NULL
	|			ТОГДА 0
	|		ИНАЧЕ 1
	|	КОНЕЦ КАК ЕстьСкидка
	|ПОМЕСТИТЬ ВТКлиенты
	|ИЗ
	|	Справочник.Клиенты КАК Клиенты
	|		ЛЕВОЕ СОЕДИНЕНИЕ Справочник.Клиенты.Телефоны КАК Телефоны
	|		ПО Клиенты.Ссылка = Телефоны.Ссылка
	|		ЛЕВОЕ СОЕДИНЕНИЕ РегистрСведений.ПроверкаТелефонов КАК Проверки
	|		ПО (Телефоны.Ссылка = Проверки.Клиент)
	|			И (Телефоны.Номер = Проверки.Номер)
	|		ЛЕВОЕ СОЕДИНЕНИЕ Справочник.Скидки КАК Скидки
	|		ПО Клиенты.Ссылка = Скидки.Клиент";
КонецПроцедуры
"#;
        check(code, expect![[r#""#]]);
    }

    #[test]
    fn test_diagnostic_highlights_field_not_join() {
        let code = r#"Процедура ВыбратьДоставки()
	Запрос = Новый Запрос("ВЫБРАТЬ
	|	Маршруты.Водитель,
	|	Остановки.Адрес КАК Адрес
	|ИЗ
	|	Документ.Доставка.Остановки КАК Остановки
	|		ЛЕВОЕ СОЕДИНЕНИЕ Справочник.Маршруты КАК Маршруты
	|		ПО Остановки.Маршрут = Маршруты.Ссылка
	|ГДЕ
	|	Остановки.Ссылка = &Доставка");
КонецПроцедуры
"#;
        check(
            code,
            expect![[r#"
            FieldsFromJoinsWithoutIsNull @ 3:4..3:21
              message: Для полей из ЛЕВОГО СОЕДИНЕНИЯ добавьте проверку через ЕСТЬ NULL или используйте функцию ЕСТЬNULL, либо замените на ВНУТРЕННЕЕ СОЕДИНЕНИЕ
              severity: Warning"#]],
        );
    }

    #[test]
    fn full_outer_join_classification_english() {
        let code = r#"Процедура ВыбратьДоставки(Запрос)
	Запрос.Текст =
	"SELECT Couriers.Phone AS Phone,
	|	Orders.Number AS Number
	|FROM Document.Shipment AS Orders
	|FULL OUTER JOIN Catalog.Couriers AS Couriers
	|ON Orders.Courier = Couriers.Ref";
КонецПроцедуры
"#;
        check(
            code,
            expect![[r#"
            FieldsFromJoinsWithoutIsNull @ 3:10..3:24
              message: Для полей из ПОЛНОГО СОЕДИНЕНИЯ добавьте проверку через ЕСТЬ NULL или используйте функцию ЕСТЬNULL, либо замените на ВНУТРЕННЕЕ СОЕДИНЕНИЕ
              severity: Warning
            FieldsFromJoinsWithoutIsNull @ 4:4..4:17
              message: Для полей из ПОЛНОГО СОЕДИНЕНИЯ добавьте проверку через ЕСТЬ NULL или используйте функцию ЕСТЬNULL, либо замените на ВНУТРЕННЕЕ СОЕДИНЕНИЕ
              severity: Warning"#]],
        );
    }
}
