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
    use crate::test_utils::{check_diagnostics_for, check_diagnostics_snapshot_for};
    use crate::DiagnosticCode;
    use expect_test::{expect, Expect};

    fn check(code: &str, expected: expect_test::Expect) {
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::FieldsFromJoinsWithoutIsNull,
            expected,
        );
    }

    /// The fields under the diagnostics of `query` placed in a module, one per line.
    fn check_query(query: &str, expected: Expect) {
        expected.assert_eq(&reported(query).join("\n"));
    }

    fn reported(query: &str) -> Vec<String> {
        let source =
            format!("Процедура Выбрать(Запрос)\n\tЗапрос.Текст = \"{query}\";\nКонецПроцедуры\n");
        check_diagnostics_for(&source, DiagnosticCode::FieldsFromJoinsWithoutIsNull)
            .iter()
            .map(|d| source[d.range].to_string())
            .collect()
    }

    fn assert_silent(query: &str) {
        assert_eq!(reported(query), Vec::<String>::new(), "{query}");
    }

    /// `Ставки` is the optional side, `Товары` the preserved one.
    const RATES: &str = "ИЗ Документ.Заказ.Товары КАК Товары ЛЕВОЕ СОЕДИНЕНИЕ РегистрСведений.Ставки КАК Ставки ПО Товары.Номенклатура = Ставки.Номенклатура";

    const OWNER_OF_ITEMS: &str =
        "ИЗ Документ.Заказ.Товары КАК Товары ЛЕВОЕ СОЕДИНЕНИЕ Документ.Заказ КАК Заказ ПО";

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
        // A condition never lets NULL into the result: the row is filtered out instead.
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
        check(code, expect![[r#""#]]);
    }

    #[test]
    fn where_condition_does_not_hide_a_selected_field() {
        let code = r#"Процедура ВыбратьДоставки(Запрос)
	Запрос.Текст =
	"ВЫБРАТЬ Курьеры.Телефон
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
            FieldsFromJoinsWithoutIsNull @ 3:11..3:26
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

    #[test]
    fn issue_768_module_reports_only_the_unprotected_product() {
        check(
            include_str!("fixtures/fields_from_joins_issue_768.bsl"),
            expect![[r#"
            FieldsFromJoinsWithoutIsNull @ 54:19..54:32
              message: Для полей из ЛЕВОГО СОЕДИНЕНИЯ добавьте проверку через ЕСТЬ NULL или используйте функцию ЕСТЬNULL, либо замените на ВНУТРЕННЕЕ СОЕДИНЕНИЕ
              severity: Warning"#]],
        );
    }

    #[test]
    fn owner_of_a_tabular_section_joined_by_its_ref_is_not_reported() {
        for query in [
            "ВЫБРАТЬ Заказ.Дата ИЗ Документ.Заказ.Товары КАК Товары ЛЕВОЕ СОЕДИНЕНИЕ Документ.Заказ КАК Заказ ПО Товары.Ссылка = Заказ.Ссылка",
            "ВЫБРАТЬ Клиент.Наименование ИЗ Справочник.Клиенты.Телефоны КАК Телефоны ЛЕВОЕ СОЕДИНЕНИЕ Справочник.Клиенты КАК Клиент ПО Телефоны.Ссылка = Клиент.Ссылка",
            "SELECT Ord.Date FROM Document.Orders.Goods AS Goods LEFT JOIN Document.Orders AS Ord ON Goods.Ref = Ord.Ref",
            "SELECT Client.Description FROM Catalog.Clients.Phones AS Phones LEFT JOIN Catalog.Clients AS Client ON Phones.Ref = Client.Ref",
            "ВЫБРАТЬ Заказ.Дата ИЗ Document.ЗАКАЗ.Товары КАК Товары ЛЕВОЕ СОЕДИНЕНИЕ Документ.заказ КАК Заказ ПО Товары.Ref = Заказ.ССЫЛКА",
        ] {
            assert_silent(query);
        }
    }

    #[test]
    fn owner_proof_needs_the_own_object_and_its_ref() {
        check_query(
            "ВЫБРАТЬ Заказ.Дата ИЗ Документ.Заказ.Товары КАК Товары ЛЕВОЕ СОЕДИНЕНИЕ Документ.Счет КАК Заказ ПО Товары.Ссылка = Заказ.Ссылка",
            expect![[r#"Заказ.Дата"#]],
        );
        check_query(
            "ВЫБРАТЬ Заказ.Дата ИЗ Документ.Заказ.Товары КАК Товары ЛЕВОЕ СОЕДИНЕНИЕ Справочник.Заказ КАК Заказ ПО Товары.Ссылка = Заказ.Ссылка",
            expect![[r#"Заказ.Дата"#]],
        );
        check_query(
            &format!("ВЫБРАТЬ Заказ.Дата {OWNER_OF_ITEMS} Товары.ДругаяСсылка = Заказ.Ссылка"),
            expect![[r#"Заказ.Дата"#]],
        );
        check_query(
            &format!("ВЫБРАТЬ Заказ.Дата {OWNER_OF_ITEMS} Товары.Ссылка = Заказ.Контрагент"),
            expect![[r#"Заказ.Дата"#]],
        );
    }

    #[test]
    fn owner_proof_allows_only_the_ref_equality_in_the_condition() {
        for condition in [
            "Заказ.Ссылка = Товары.Ссылка",
            "(Товары.Ссылка = Заказ.Ссылка)",
            "Товары.Ссылка = Заказ.Ссылка И Заказ.Ссылка = Товары.Ссылка",
        ] {
            assert_silent(&format!("ВЫБРАТЬ Заказ.Дата {OWNER_OF_ITEMS} {condition}"));
        }
        check_query(
            &format!("ВЫБРАТЬ Заказ.Дата {OWNER_OF_ITEMS} Товары.Ссылка = Заказ.Ссылка И Заказ.Проведен = ИСТИНА"),
            expect![[r#"Заказ.Дата"#]],
        );
        check_query(
            &format!("ВЫБРАТЬ Заказ.Дата {OWNER_OF_ITEMS} Товары.Ссылка = Заказ.Ссылка ИЛИ Заказ.Проведен = ИСТИНА"),
            expect![[r#"Заказ.Дата"#]],
        );
    }

    #[test]
    fn owner_proof_follows_the_join_direction() {
        let select = "ВЫБРАТЬ Заказ.Дата, Товары.Количество";
        let on = "ПО Товары.Ссылка = Заказ.Ссылка";
        check_query(
            &format!("{select} ИЗ Документ.Заказ КАК Заказ ЛЕВОЕ СОЕДИНЕНИЕ Документ.Заказ.Товары КАК Товары {on}"),
            expect![[r#"Товары.Количество"#]],
        );
        check_query(
            &format!("{select} ИЗ Документ.Заказ КАК Заказ ПРАВОЕ СОЕДИНЕНИЕ Документ.Заказ.Товары КАК Товары {on}"),
            expect![[r#""#]],
        );
        check_query(
            &format!("{select} ИЗ Документ.Заказ.Товары КАК Товары ПРАВОЕ СОЕДИНЕНИЕ Документ.Заказ КАК Заказ {on}"),
            expect![[r#"Товары.Количество"#]],
        );
        check_query(
            &format!("{select} ИЗ Документ.Заказ.Товары КАК Товары ПОЛНОЕ СОЕДИНЕНИЕ Документ.Заказ КАК Заказ {on}"),
            expect![[r#"Товары.Количество"#]],
        );
        check_query(
            &format!("{select} ИЗ Документ.Заказ КАК Заказ ПОЛНОЕ СОЕДИНЕНИЕ Документ.Заказ.Товары КАК Товары {on}"),
            expect![[r#"Товары.Количество"#]],
        );
    }

    #[test]
    fn owner_proof_does_not_cover_a_chain_of_joins() {
        check_query(
            &format!("ВЫБРАТЬ Заказ.Дата, ЕСТЬNULL(Ставки.Ставка, 0) {OWNER_OF_ITEMS} Товары.Ссылка = Заказ.Ссылка ЛЕВОЕ СОЕДИНЕНИЕ РегистрСведений.Ставки КАК Ставки ПО Товары.Номенклатура = Ставки.Номенклатура"),
            expect![[r#"Заказ.Дата"#]],
        );
    }

    #[test]
    fn then_under_a_comparison_of_the_same_side_is_not_reported() {
        for op in ["=", "<>", "<", "<=", ">", ">="] {
            for other in [
                "0",
                "NULL",
                "&Ставка",
                "Ставки.Предел",
                "Товары.Ставка",
                "Товары.Ставка + 1",
                "ЕСТЬNULL(Товары.Ставка, 0)",
            ] {
                for condition in
                    [format!("Ставки.Ставка {op} {other}"), format!("{other} {op} Ставки.Ставка")]
                {
                    assert_silent(&format!(
                        "ВЫБРАТЬ ВЫБОР КОГДА {condition} ТОГДА Товары.Сумма / Ставки.Ставка ИНАЧЕ 0 КОНЕЦ КАК Сумма {RATES}"
                    ));
                }
            }
        }
    }

    #[test]
    fn comparison_guards_only_its_own_side() {
        check_query(
            &format!("ВЫБРАТЬ ВЫБОР КОГДА Товары.Ставка <> 0 ТОГДА Товары.Сумма / Ставки.Ставка ИНАЧЕ 0 КОНЕЦ КАК Сумма {RATES}"),
            expect![[r#"Ставки.Ставка"#]],
        );
        check_query(
            &format!("ВЫБРАТЬ ВЫБОР КОГДА Ставки.Ставка <> 0 ТОГДА Скидки.Скидка / Ставки.Ставка ИНАЧЕ 0 КОНЕЦ КАК Сумма {RATES} ЛЕВОЕ СОЕДИНЕНИЕ РегистрСведений.Скидки КАК Скидки ПО Товары.Номенклатура = Скидки.Номенклатура"),
            expect![[r#"Скидки.Скидка"#]],
        );
    }

    #[test]
    fn comparison_guard_holds_for_a_choice_inside_an_expression() {
        assert_silent(&format!(
            "ВЫБРАТЬ Товары.Сумма / ВЫБОР КОГДА Ставки.Ставка <> 0 ТОГДА Ставки.Ставка ИНАЧЕ 1 КОНЕЦ КАК Сумма {RATES}"
        ));
        check_query(
            &format!("ВЫБРАТЬ Товары.Сумма / ВЫБОР КОГДА Товары.Ставка <> 0 ТОГДА Ставки.Ставка ИНАЧЕ 1 КОНЕЦ КАК Сумма {RATES}"),
            expect![[r#"Ставки.Ставка"#]],
        );
    }

    #[test]
    fn comparison_guards_only_its_own_branch() {
        check_query(
            &format!("ВЫБРАТЬ ВЫБОР КОГДА Ставки.Ставка <> 0 ТОГДА Ставки.Код КОГДА Товары.Срочно ТОГДА Ставки.Наименование ИНАЧЕ Ставки.Вид КОНЕЦ КАК Вид {RATES}"),
            expect![[r#"
                Ставки.Наименование
                Ставки.Вид"#]],
        );
        check_query(
            &format!("ВЫБРАТЬ ВЫБОР КОГДА Ставки.Ставка ЕСТЬ NULL ТОГДА Ставки.Код ИНАЧЕ 0 КОНЕЦ КАК Вид {RATES}"),
            expect![[r#"Ставки.Код"#]],
        );
    }

    #[test]
    fn comparison_guard_needs_a_true_comparison_of_the_field_itself() {
        for condition in
            ["Ставки.Ставка <> 0 И Товары.Срочно", "Товары.Срочно И Ставки.Ставка <> 0"]
        {
            assert_silent(&format!(
                "ВЫБРАТЬ ВЫБОР КОГДА {condition} ТОГДА Ставки.Код ИНАЧЕ 0 КОНЕЦ КАК Код {RATES}"
            ));
        }
        for condition in [
            "Ставки.Ставка <> 0 ИЛИ Товары.Срочно",
            "НЕ (Ставки.Ставка = 0)",
            "Ставки.Действует",
            "Ставки.Ставка В (1, 2)",
            "Ставки.Ставка МЕЖДУ 1 И 2",
            "Ставки.Код ПОДОБНО &Шаблон",
            "(Ставки.Ставка + 1) > 0",
            "ЕСТЬNULL(Ставки.Ставка, 0) = 0",
        ] {
            assert_eq!(
                reported(&format!(
                    "ВЫБРАТЬ ВЫБОР КОГДА {condition} ТОГДА Ставки.Код ИНАЧЕ 0 КОНЕЦ КАК Код {RATES}"
                )),
                vec!["Ставки.Код"],
                "{condition}"
            );
        }
    }

    #[test]
    fn fields_used_only_in_conditions_are_not_reported() {
        for query in [
            format!("ВЫБРАТЬ Товары.Номенклатура {RATES} ГДЕ Ставки.Ставка = &Ставка"),
            format!("ВЫБРАТЬ ВЫБОР КОГДА Ставки.Ставка > 0 ТОГДА 1 ИНАЧЕ 0 КОНЕЦ КАК Есть {RATES}"),
            format!("ВЫБРАТЬ Товары.Номенклатура {RATES} И Ставки.Ставка > 0"),
            format!("ВЫБРАТЬ ВЫБОР Ставки.Ставка КОГДА 0 ТОГДА 1 ИНАЧЕ 2 КОНЕЦ КАК Вид {RATES}"),
            format!("ВЫБРАТЬ ВЫБОР Товары.Ставка КОГДА Ставки.Ставка ТОГДА 1 ИНАЧЕ 2 КОНЕЦ КАК Вид {RATES}"),
        ] {
            assert_silent(&query);
        }
    }

    #[test]
    fn a_selected_field_is_reported_next_to_its_conditions() {
        check_query(
            &format!("ВЫБРАТЬ Ставки.Код {RATES} ГДЕ Ставки.Ставка = &Ставка"),
            expect![[r#"Ставки.Код"#]],
        );
        check_query(
            &format!("ВЫБРАТЬ Ставки.Код {RATES} И Ставки.Ставка > 0"),
            expect![[r#"Ставки.Код"#]],
        );
        check_query(
            &format!(
                "ВЫБРАТЬ ВЫБОР КОГДА Товары.Срочно ТОГДА Ставки.Код ИНАЧЕ 0 КОНЕЦ КАК Код {RATES}"
            ),
            expect![[r#"Ставки.Код"#]],
        );
        check_query(
            &format!("ВЫБРАТЬ ВЫБОР Ставки.Ставка КОГДА 0 ТОГДА Ставки.Код ИНАЧЕ 0 КОНЕЦ КАК Код {RATES}"),
            expect![[r#"Ставки.Код"#]],
        );
        // A comparison with NULL is false, and false equals the operand ЛОЖЬ: the branch of a
        // simple ВЫБОР runs without the row.
        check_query(
            &format!("ВЫБРАТЬ ВЫБОР Товары.Срочно КОГДА Ставки.Ставка > 0 ТОГДА Ставки.Код ИНАЧЕ 0 КОНЕЦ КАК Код {RATES}"),
            expect![[r#"Ставки.Код"#]],
        );
    }

    #[test]
    fn where_presence_test_still_protects_the_selection() {
        let couriers = "ИЗ Документ.Заказ КАК Заказы ЛЕВОЕ СОЕДИНЕНИЕ Справочник.Курьеры КАК Курьеры ПО Заказы.Курьер = Курьеры.Ссылка";
        assert_silent(&format!(
            "ВЫБРАТЬ Курьеры.Телефон {couriers} ГДЕ НЕ (Курьеры.Транспорт ЕСТЬ NULL)"
        ));
        assert_silent(&format!(
            "ВЫБРАТЬ Курьеры.Телефон {couriers} ГДЕ Заказы.Срочный И (Курьеры.Транспорт ЕСТЬ НЕ NULL)"
        ));
        check_query(
            &format!("ВЫБРАТЬ Курьеры.Телефон {couriers} ГДЕ Курьеры.Транспорт = &Транспорт"),
            expect![[r#"Курьеры.Телефон"#]],
        );
        let english = "SELECT Couriers.Phone FROM Document.Orders AS Orders LEFT JOIN Catalog.Couriers AS Couriers ON Orders.Courier = Couriers.Ref";
        assert_silent(&format!("{english} WHERE Couriers.Transport IS NOT NULL"));
        // The presence test of the selection condition matches the alias as spelled.
        check_query(
            &format!("{english} WHERE couriers.Transport IS NOT NULL"),
            expect![[r#"Couriers.Phone"#]],
        );
    }

    #[test]
    fn each_raw_use_in_the_result_is_reported() {
        check_query(
            &format!("ВЫБРАТЬ Ставки.Ставка, Товары.Сумма * Ставки.Ставка, ЕСТЬNULL(Ставки.Ставка, 0) {RATES}"),
            expect![[r#"
                Ставки.Ставка
                Ставки.Ставка"#]],
        );
        check_query(
            "ВЫБРАТЬ Курьеры.Ссылка ИЗ Документ.Заказ КАК Заказы ЛЕВОЕ СОЕДИНЕНИЕ Справочник.Курьеры КАК Курьеры ПО Заказы.Курьер = Курьеры.Ссылка",
            expect![[r#"Курьеры.Ссылка"#]],
        );
    }

    #[test]
    fn null_handling_in_the_result_protects_the_field() {
        for field in [
            "ЕСТЬNULL(Ставки.Ставка, 0)",
            "Ставки.Ставка ЕСТЬ NULL",
            "Ставки.Ставка ЕСТЬ НЕ NULL",
            "Ставки.Код ЕСТЬ NULL ИЛИ Ставки.Ставка <> 0",
        ] {
            assert_silent(&format!("ВЫБРАТЬ {field} КАК Значение {RATES}"));
        }
        check_query(
            &format!("ВЫБРАТЬ Ставки.Ставка <> 0 КАК Значение {RATES}"),
            expect![[r#"Ставки.Ставка"#]],
        );
    }

    #[test]
    fn case_then_guarded_by_negated_is_null_silent() {
        let code = r#"Процедура ВыбратьДоставки(Запрос)
	Запрос.Текст =
	"ВЫБРАТЬ ВЫБОР
	|		КОГДА НЕ (Курьеры.Ссылка ЕСТЬ NULL)
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
    fn alias_matching_keeps_ascii_only_folding() {
        assert_silent(
            "SELECT CASE WHEN rates.Rate <> 0 THEN Rates.Code ELSE 0 END AS Code FROM Document.Orders.Goods AS Goods LEFT JOIN InformationRegister.Rates AS Rates ON Goods.Item = Rates.Item",
        );
        check_query(
            &format!("ВЫБРАТЬ ВЫБОР КОГДА ставки.Ставка <> 0 ТОГДА Ставки.Код ИНАЧЕ 0 КОНЕЦ КАК Код {RATES}"),
            expect![[r#"Ставки.Код"#]],
        );
        assert_silent(
            "SELECT Ord.Date FROM Document.Orders.Goods AS Goods LEFT JOIN Document.Orders AS Ord ON goods.Ref = ORD.Ref",
        );
        check_query(
            &format!("ВЫБРАТЬ Заказ.Дата {OWNER_OF_ITEMS} товары.Ссылка = Заказ.Ссылка"),
            expect![[r#"Заказ.Дата"#]],
        );
    }
}
