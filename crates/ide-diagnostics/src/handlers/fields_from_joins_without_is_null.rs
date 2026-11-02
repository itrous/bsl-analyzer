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

    /// A condition of `ГДЕ` keeps a row only if it is true, and for the row without a pair it
    /// is not: the conjunct on the field itself hides the selected field, the one that
    /// admits the row (`ЕСТЬNULL` of a field of unknown type is not decided) does not.
    #[test]
    fn where_condition_hides_a_selected_field_only_if_it_excludes_the_missing_row() {
        let couriers = "ИЗ Документ.Заказ КАК Заказы ЛЕВОЕ СОЕДИНЕНИЕ Справочник.Курьеры КАК Курьеры ПО Заказы.Курьер = Курьеры.Ссылка";
        assert_silent(&format!(
            "ВЫБРАТЬ Курьеры.Телефон {couriers} ГДЕ Курьеры.НаСмене И ЕСТЬNULL(Курьеры.НаСмене, Ложь)"
        ));
        check_query(
            &format!("ВЫБРАТЬ Курьеры.Телефон {couriers} ГДЕ ЕСТЬNULL(Курьеры.НаСмене, Ложь)"),
            expect![[r#"Курьеры.Телефон"#]],
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

    /// Without the type of the field the replacement of ЕСТЬNULL is not known to survive the
    /// conversion, so the comparison with it is not decided and the branch is reported. The
    /// silent counterpart with the field typed is
    /// `isnull_replacement_is_computed_only_for_a_known_type` in `sdbl-hir`.
    #[test]
    fn case_then_under_an_isnull_comparison_is_reported_without_the_field_type() {
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
        check(
            code,
            expect![[r#"
            FieldsFromJoinsWithoutIsNull @ 5:12..5:27
              message: Для полей из ЛЕВОГО СОЕДИНЕНИЯ добавьте проверку через ЕСТЬ NULL или используйте функцию ЕСТЬNULL, либо замените на ВНУТРЕННЕЕ СОЕДИНЕНИЕ
              severity: Warning"#]],
        );
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

    /// A third table joined to the section does not take the owner's row away, while the
    /// third table's own field is still checked: the owner is silent, the price is not.
    #[test]
    fn owner_proof_holds_in_a_chain_of_joins() {
        let chain = format!("{OWNER_OF_ITEMS} Товары.Ссылка = Заказ.Ссылка ЛЕВОЕ СОЕДИНЕНИЕ РегистрСведений.Ставки КАК Ставки ПО Товары.Номенклатура = Ставки.Номенклатура");
        assert_silent(&format!("ВЫБРАТЬ Заказ.Дата, ЕСТЬNULL(Ставки.Ставка, 0) {chain}"));
        check_query(
            &format!("ВЫБРАТЬ Заказ.Дата, Ставки.Ставка {chain}"),
            expect![[r#"Ставки.Ставка"#]],
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

    /// The branch is safe when its condition cannot be true for the missing row. A
    /// disjunction with a condition on another source can be, and so can `ЕСТЬNULL` of a
    /// field of unknown type, whose replacement is not known to survive the conversion.
    #[test]
    fn comparison_guard_needs_a_condition_that_cannot_be_true_for_the_missing_row() {
        for condition in [
            "Ставки.Ставка <> 0 И Товары.Срочно",
            "Товары.Срочно И Ставки.Ставка <> 0",
            "НЕ (Ставки.Ставка = 0)",
            "Ставки.Действует",
            "Ставки.Ставка В (1, 2)",
            "Ставки.Ставка МЕЖДУ 1 И 2",
            "Ставки.Код ПОДОБНО &Шаблон",
            "(Ставки.Ставка + 1) > 0",
        ] {
            assert_silent(&format!(
                "ВЫБРАТЬ ВЫБОР КОГДА {condition} ТОГДА Ставки.Код ИНАЧЕ 0 КОНЕЦ КАК Код {RATES}"
            ));
        }
        for condition in [
            "Ставки.Ставка <> 0 ИЛИ Товары.Срочно",
            "Товары.Срочно ИЛИ НЕ (Ставки.Ставка = 0)",
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
        // A condition of `ГДЕ` on another source does not exclude the missing row.
        check_query(
            &format!("ВЫБРАТЬ Ставки.Код {RATES} ГДЕ Товары.Номенклатура = &Номенклатура"),
            expect![[r#"Ставки.Код"#]],
        );
        // The condition of the join is not a condition of the selection.
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
        // The operand of a simple ВЫБОР is NULL for the missing row and equals nothing, but the
        // branch after the one that cannot run is not safe for it.
        check_query(
            &format!("ВЫБРАТЬ ВЫБОР Ставки.Ставка КОГДА 0 ТОГДА 0 ИНАЧЕ Ставки.Код КОНЕЦ КАК Код {RATES}"),
            expect![[r#"Ставки.Код"#]],
        );
        // A comparison used as a value is not evaluated, so the branch is reported.
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
            &format!("ВЫБРАТЬ Курьеры.Телефон {couriers} ГДЕ Заказы.Срочный ИЛИ Курьеры.Транспорт ЕСТЬ НЕ NULL"),
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

    // What the NULL of a missing row reaches: the fields of the optional side are NULL, the
    // rest of the query is unknown, and a branch or a `ГДЕ` that cannot be true then hides the
    // fields behind it.

    /// The fields reported for `query`. A query that does not parse would make every
    /// "silent" expectation hold for the wrong reason, so it is rejected here; a quote is
    /// doubled the way the BSL literal holding the query needs it.
    fn checked(query: &str) -> Vec<String> {
        let parse = parser::parse_sdbl(query);
        assert!(parse.errors().is_empty(), "{query}: {:?}", parse.errors());
        reported(&query.replace('"', "\"\""))
    }

    fn sorted(mut fields: Vec<String>) -> Vec<String> {
        fields.sort();
        fields
    }

    /// The field behind `ТОГДА` and the one behind `ИНАЧЕ` of the choice under test.
    const THEN: &str = "Ставки.Код";
    const ELSE: &str = "Ставки.Вид";
    const BOTH: [&str; 2] = [THEN, ELSE];
    const NOTHING: [&str; 0] = [];

    /// What `ВЫБОР КОГДА <condition> ТОГДА Ставки.Код ИНАЧЕ Ставки.Вид` reports.
    fn branches(condition: &str) -> Vec<String> {
        checked(&format!(
            "ВЫБРАТЬ ВЫБОР КОГДА {condition} ТОГДА {THEN} ИНАЧЕ {ELSE} КОНЕЦ КАК Р {RATES}"
        ))
    }

    /// What `ВЫБРАТЬ Ставки.Код ... ГДЕ <condition>` reports.
    fn filtered(condition: &str) -> Vec<String> {
        checked(&format!("ВЫБРАТЬ {THEN} {RATES} ГДЕ {condition}"))
    }

    #[track_caller]
    fn assert_branches<const N: usize>(condition: &str, expected: [&str; N]) {
        assert_eq!(branches(condition), expected, "ВЫБОР КОГДА {condition}");
    }

    #[track_caller]
    fn assert_filtered<const N: usize>(condition: &str, expected: [&str; N]) {
        assert_eq!(filtered(condition), expected, "ГДЕ {condition}");
    }

    #[test]
    fn a_branch_is_safe_when_its_condition_cannot_be_true_for_the_missing_row() {
        // Compared with NULL the condition is undefined, which is not true.
        assert_branches("Ставки.Ставка = &П", [ELSE]);
        assert_branches("Ставки.Ставка ЕСТЬ НЕ NULL", [ELSE]);
        // The test for NULL is true for the missing row: its branch is reached, and what comes
        // after a condition that is surely true is not.
        assert_branches("Ставки.Ставка ЕСТЬ NULL", [THEN]);
        // A condition on another source says nothing about the row.
        assert_branches("Товары.Ставка = &П", BOTH);
    }

    #[test]
    fn what_follows_a_condition_that_is_surely_true_is_safe() {
        let after = |first: &str| {
            checked(&format!(
                "ВЫБРАТЬ ВЫБОР КОГДА {first} ТОГДА 0 КОГДА &П ТОГДА {THEN} ИНАЧЕ {ELSE} КОНЕЦ КАК Р {RATES}"
            ))
        };
        assert_eq!(after("Ставки.Ставка ЕСТЬ NULL"), NOTHING);
        // Possibly true is not surely true: with a parameter beside it the test for NULL may
        // be false or undefined, and a disjunction whose first operand may be false or
        // undefined may be undefined while its second operand is.
        assert_eq!(after("Ставки.Ставка ЕСТЬ NULL И &П"), BOTH);
        assert_eq!(after("&П ИЛИ Ставки.Ставка = 0"), BOTH);
        // Undefined is not true: the row goes on to the next branch.
        assert_eq!(after("Ставки.Ставка = 0"), BOTH);
        assert_eq!(
            checked(&format!(
                "ВЫБРАТЬ ВЫБОР КОГДА Ставки.Ставка = 0 ТОГДА 0 ИНАЧЕ {ELSE} КОНЕЦ КАК Р {RATES}"
            )),
            [ELSE]
        );
    }

    #[test]
    fn undefined_stays_undefined_under_negation_and_is_not_false() {
        // NOT of undefined is undefined, not true.
        assert_branches("НЕ (Ставки.Ставка = 0)", [ELSE]);
        // Undefined and anything is not true; true or anything is true.
        assert_branches("Ставки.Ставка = 0 И &П", [ELSE]);
        assert_branches("Ставки.Ставка ЕСТЬ NULL ИЛИ &П", [THEN]);
        assert_branches("Ставки.Ставка = 0 ИЛИ &П", BOTH);
        assert_branches("Ставки.Ставка ЕСТЬ NULL И &П", BOTH);
        // The control that tells undefined from false: NOT of a condition that is false for
        // the missing row is true.
        assert_branches("НЕ (Ставки.Ставка ЕСТЬ НЕ NULL)", [THEN]);
    }

    #[test]
    fn arithmetic_and_datediff_of_the_missing_row_are_undefined() {
        for condition in [
            "РАЗНОСТЬДАТ(Ставки.Дата, &Дата, МЕСЯЦ) >= 0",
            "DATEDIFF(Ставки.Дата, &Дата, MONTH) >= 0",
            "РАЗНОСТЬДАТ(&Дата, Ставки.Дата, МЕСЯЦ) >= 0",
            "Ставки.Число + 1 > 0",
            "Ставки.Число - 1 > 0",
            "Ставки.Число * 2 > 0",
            "Ставки.Число / 2 > 0",
            "Ставки.Число % 2 > 0",
            "+Ставки.Число > 0",
            "-Ставки.Число > 0",
        ] {
            assert_branches(condition, [ELSE]);
            assert_branches(&format!("НЕ ({condition})"), [ELSE]);
            assert_filtered(condition, NOTHING);
            assert_filtered(&format!("НЕ ({condition})"), NOTHING);

            // The same condition on another source proves nothing.
            let other = condition.replace("Ставки.", "Товары.");
            assert_branches(&other, BOTH);
            assert_filtered(&other, [THEN]);
        }
        // A wrapper that turns NULL into a value takes the proof away.
        assert_branches("ЕСТЬNULL(Ставки.Число + 1, 0) > 0", BOTH);
        assert_branches("(ВЫБОР КОГДА Ставки.Число ЕСТЬ NULL ТОГДА 1 ИНАЧЕ 0 КОНЕЦ) + 1 > 0", BOTH);
    }

    #[test]
    fn only_the_minimal_date_constructor_is_a_constant() {
        // The same constant on both sides is decided by the constants alone.
        assert_branches("ДАТАВРЕМЯ(1, 1, 1) <> ДАТАВРЕМЯ(1, 1, 1)", [ELSE]);
        assert_branches("DATETIME(1, 1, 1) <> ДАТАВРЕМЯ(1, 1, 1)", [ELSE]);
        assert_branches("ДАТАВРЕМЯ(1, 1, 1) = ДАТАВРЕМЯ(1, 1, 1)", [THEN]);
        // Any other constructor is unknown, even if both sides read the same.
        assert_branches("ДАТАВРЕМЯ(&Год, 1, 1) <> ДАТАВРЕМЯ(&Год, 1, 1)", BOTH);
        assert_branches("ДАТАВРЕМЯ(2024, 1, 1) <> ДАТАВРЕМЯ(2024, 1, 1)", BOTH);
        assert_branches("ДАТАВРЕМЯ(1, 1, 1, 0, 0, 0) <> ДАТАВРЕМЯ(1, 1, 1, 0, 0, 0)", BOTH);
    }

    #[test]
    fn a_function_that_turns_null_into_a_value_does_not_make_the_condition_undefined() {
        for condition in [
            "ПРЕДСТАВЛЕНИЕ(Ставки.Ставка) = \"\"",
            "PRESENTATION(Ставки.Ставка) = \"\"",
            "КОЛИЧЕСТВО(Ставки.Ставка) = 0",
            "COUNT(Ставки.Ставка) = 0",
            "НеизвестнаяФункция(Ставки.Ставка) = 0",
            "(ВЫБОР КОГДА Ставки.Ставка ЕСТЬ NULL ТОГДА 1 ИНАЧЕ 0 КОНЕЦ) = 1",
            "ЕСТЬNULL(Ставки.Ставка, 0) = 0",
            "ISNULL(Ставки.Ставка, 0) = 0",
        ] {
            assert_branches(condition, BOTH);
        }
        // The control: the field itself, compared the same way.
        assert_branches("Ставки.Ставка = 0", [ELSE]);
    }

    #[test]
    fn the_operand_of_a_simple_choice_is_compared_with_every_value() {
        let simple = |operand: &str| {
            checked(&format!(
                "ВЫБРАТЬ ВЫБОР {operand} КОГДА 0 ТОГДА {THEN} ИНАЧЕ {ELSE} КОНЕЦ КАК Р {RATES}"
            ))
        };
        // NULL equals no value, so the branch is safe; the row still goes to `ИНАЧЕ`.
        assert_eq!(simple("Ставки.Ставка"), [ELSE]);
        assert_eq!(simple("Товары.Ставка"), BOTH);
    }

    #[test]
    fn positive_membership_never_admits_the_missing_row() {
        for operator in ["В", "В ИЕРАРХИИ", "IN", "IN HIERARCHY"] {
            for values in ["1", "&П", "ВЫБРАТЬ С.Код ИЗ Справочник.С КАК С"]
            {
                let member = |source: &str| format!("{source}.Группа {operator} ({values})");
                assert_branches(&member("Ставки"), [ELSE]);
                assert_filtered(&member("Ставки"), NOTHING);
                // On another source the same membership proves nothing.
                assert_branches(&member("Товары"), BOTH);
                assert_filtered(&member("Товары"), [THEN]);
            }
        }
    }

    #[test]
    fn negated_membership_admits_the_missing_row_unless_the_list_surely_holds_a_value() {
        let empty_ref = "ЗНАЧЕНИЕ(Справочник.Группы.ПустаяСсылка)";
        let nested = "ВЫБРАТЬ С.Код ИЗ Справочник.С КАК С";
        for operator in ["В", "В ИЕРАРХИИ"] {
            let negations = |values: &str| {
                [
                    format!("НЕ (Ставки.Группа {operator} ({values}))"),
                    format!("Ставки.Группа НЕ {operator} ({values})"),
                ]
            };
            // A literal, NULL or one of them next to a parameter is a value for certain.
            for values in ["1", "NULL", "1, &П", empty_ref] {
                for condition in negations(values) {
                    assert_branches(&condition, [ELSE]);
                    assert_filtered(&condition, NOTHING);
                }
            }
            // A parameter or a nested query may hold nothing, and then NOT turns false into true.
            for values in ["&П", "&П, &Q", nested] {
                for condition in negations(values) {
                    assert_branches(&condition, BOTH);
                    assert_filtered(&condition, [THEN]);
                    // Another conjunct that excludes the missing row hides the field again.
                    assert_filtered(&format!("{condition} И Ставки.Ставка > 0"), NOTHING);
                }
            }
        }
    }

    const PRICES_BY_ITEM: &str =
        "ЛЕВОЕ СОЕДИНЕНИЕ РегистрСведений.Цены КАК Цены ПО Товары.Номенклатура = Цены.Номенклатура";

    /// A tabular section and its owner, for a document and for a catalog.
    const OWNERS: [(&str, &str); 2] = [
        ("Документ.Заказ.Товары", "Документ.Заказ"),
        ("Справочник.Заказ.Товары", "Справочник.Заказ"),
    ];

    #[test]
    fn the_owner_is_silent_beside_a_third_table_in_each_direction_of_the_join() {
        for (section, owner) in OWNERS {
            let on = "ПО Товары.Ссылка = Заказ.Ссылка";
            let select = "ВЫБРАТЬ Заказ.Дата, Товары.Количество, Цены.Цена";
            // Section left-joins its owner: the preserved section is silent, the price is not.
            assert_eq!(
                checked(&format!(
                    "{select} ИЗ {section} КАК Товары ЛЕВОЕ СОЕДИНЕНИЕ {owner} КАК Заказ {on} {PRICES_BY_ITEM}"
                )),
                ["Цены.Цена"]
            );
            // The owner is the optional side of a right join.
            assert_eq!(
                checked(&format!(
                    "{select} ИЗ {owner} КАК Заказ ПРАВОЕ СОЕДИНЕНИЕ {section} КАК Товары {on} {PRICES_BY_ITEM}"
                )),
                ["Цены.Цена"]
            );
            // In a full join both are optional, but the section stays reported while the owner
            // of its own join does not.
            assert_eq!(
                sorted(checked(&format!(
                    "{select} ИЗ {section} КАК Товары ПОЛНОЕ СОЕДИНЕНИЕ {owner} КАК Заказ {on} {PRICES_BY_ITEM}"
                ))),
                ["Товары.Количество", "Цены.Цена"]
            );
            // Without the third table the owner is silent as before, and the control field is
            // still the section's.
            assert_eq!(
                checked(&format!(
                    "ВЫБРАТЬ Заказ.Дата, Товары.Количество ИЗ {section} КАК Товары ПОЛНОЕ СОЕДИНЕНИЕ {owner} КАК Заказ {on}"
                )),
                ["Товары.Количество"]
            );
        }
    }

    #[test]
    fn the_owner_is_reported_when_its_section_is_optional_in_another_join() {
        let section = "Документ.Заказ.Товары";
        let owner = "Документ.Заказ";
        let on = "ПО Товары.Ссылка = Заказ.Ссылка";

        // The third table is joined on the optional side of a left join to the section.
        assert_eq!(
            checked(&format!(
                "ВЫБРАТЬ Заказ.Дата ИЗ {section} КАК Товары ЛЕВОЕ СОЕДИНЕНИЕ {owner} КАК Заказ {on} {PRICES_BY_ITEM}"
            )),
            NOTHING
        );

        // The section with its owner hangs on the optional side of an outer left join.
        let nested = |outer: &str| {
            checked(&format!(
                "ВЫБРАТЬ Заказ.Дата ИЗ Документ.Основа КАК Основа {outer} {section} КАК Товары ЛЕВОЕ СОЕДИНЕНИЕ {owner} КАК Заказ {on} ПО Основа.Ссылка = Товары.Основа"
            ))
        };
        assert_eq!(nested("ЛЕВОЕ СОЕДИНЕНИЕ"), ["Заказ.Дата"]);
        // The control: with the outer join inner the section is not optional.
        assert_eq!(nested("ВНУТРЕННЕЕ СОЕДИНЕНИЕ"), NOTHING);

        // The section is made optional by a later right or full join of the pair.
        let later = |join: &str| {
            checked(&format!(
                "ВЫБРАТЬ Заказ.Дата ИЗ {section} КАК Товары ЛЕВОЕ СОЕДИНЕНИЕ {owner} КАК Заказ {on} {join} РегистрСведений.Цены КАК Цены ПО Товары.Номенклатура = Цены.Номенклатура"
            ))
        };
        assert_eq!(later("ПРАВОЕ СОЕДИНЕНИЕ"), ["Заказ.Дата"]);
        assert_eq!(later("ПОЛНОЕ СОЕДИНЕНИЕ"), ["Заказ.Дата"]);
        assert_eq!(later("ЛЕВОЕ СОЕДИНЕНИЕ"), NOTHING);
    }

    #[test]
    fn the_condition_of_the_owner_join_is_judged_with_a_third_table_too() {
        let with_prices = |condition: &str| {
            checked(&format!("ВЫБРАТЬ Заказ.Дата {OWNER_OF_ITEMS} {condition} {PRICES_BY_ITEM}"))
        };
        for condition in [
            "Товары.Ссылка = Заказ.Ссылка",
            "Заказ.Ссылка = Товары.Ссылка",
            "(Товары.Ссылка = Заказ.Ссылка)",
            "Товары.Ссылка = Заказ.Ссылка И Заказ.Ссылка = Товары.Ссылка",
        ] {
            assert_eq!(with_prices(condition), NOTHING, "{condition}");
        }
        for condition in [
            "Товары.Ссылка = Заказ.Ссылка И Заказ.Проведен = ИСТИНА",
            "Товары.Ссылка = Заказ.Ссылка ИЛИ Заказ.Проведен = ИСТИНА",
            "Товары.ДругаяСсылка = Заказ.Ссылка",
            "Товары.Ссылка = Заказ.Контрагент",
        ] {
            assert_eq!(with_prices(condition), ["Заказ.Дата"], "{condition}");
        }
        // Another object, or a catalog where a document is the owner, is not the owner.
        for other in ["Документ.Счет", "Справочник.Заказ"] {
            assert_eq!(
                checked(&format!(
                    "ВЫБРАТЬ Заказ.Дата ИЗ Документ.Заказ.Товары КАК Товары ЛЕВОЕ СОЕДИНЕНИЕ {other} КАК Заказ ПО Товары.Ссылка = Заказ.Ссылка {PRICES_BY_ITEM}"
                )),
                ["Заказ.Дата"],
                "{other}"
            );
        }
    }

    #[test]
    fn only_direct_fields_of_a_present_row_are_safe() {
        let chain = format!("{OWNER_OF_ITEMS} Товары.Ссылка = Заказ.Ссылка {PRICES_BY_ITEM}");
        // The owner's own attribute is silent; one read through its reference may still be NULL.
        assert_eq!(checked(&format!("ВЫБРАТЬ Заказ.Дата {chain}")), NOTHING);
        assert_eq!(
            checked(&format!("ВЫБРАТЬ Заказ.Дата, Заказ.Родитель.Родитель {chain}")),
            ["Заказ.Родитель.Родитель"]
        );
        // A branch that cannot run without the row, and the selection condition, are no
        // different: the direct field is hidden, the field behind a reference is not.
        assert_eq!(
            checked(&format!(
                "ВЫБРАТЬ ВЫБОР КОГДА Ставки.Ставка = 0 ТОГДА Ставки.Код ИНАЧЕ 0 КОНЕЦ, ВЫБОР КОГДА Ставки.Ставка = 0 ТОГДА Ставки.Ссылка.Имя ИНАЧЕ 0 КОНЕЦ {RATES}"
            )),
            ["Ставки.Ссылка.Имя"]
        );
        assert_eq!(
            checked(&format!(
                "ВЫБРАТЬ Ставки.Код, Ставки.Ссылка.Имя {RATES} ГДЕ Ставки.Ставка > 0"
            )),
            ["Ставки.Ссылка.Имя"]
        );
        assert_eq!(
            checked(&format!(
                "ВЫБРАТЬ Ставки.Код, ВЫРАЗИТЬ(Ставки.Код КАК Справочник.Группы).Наименование {RATES} ГДЕ Ставки.Ставка > 0"
            )),
            ["Ставки.Код"]
        );
        // A condition read through a reference proves nothing about the row.
        assert_branches("Ставки.Ссылка.Имя = 0", BOTH);
        assert_filtered("Ставки.Ссылка.Имя > 0", [THEN]);
    }

    #[test]
    fn a_top_level_conjunct_that_cannot_be_true_hides_the_fields_of_its_side() {
        let predicates = [
            "Ставки.Ставка = &П",
            "Ставки.Ставка <> &П",
            "Ставки.Ставка < &П",
            "Ставки.Ставка <= &П",
            "Ставки.Ставка > &П",
            "Ставки.Ставка >= &П",
            "Ставки.Ставка МЕЖДУ &А И &Б",
            "Ставки.Действует",
            "Ставки.Код ПОДОБНО &Шаблон",
            "Ставки.Ссылка ССЫЛКА Справочник.Ставки",
        ];
        for predicate in predicates {
            for condition in [
                format!("Товары.Срочно И {predicate}"),
                format!("{predicate} И Товары.Срочно"),
                format!("Товары.Срочно И НЕ ({predicate})"),
            ] {
                assert_filtered(&condition, NOTHING);
            }
            // The same predicate on another source does not hide the field.
            assert_filtered(
                &format!("Товары.Срочно И {}", predicate.replace("Ставки.", "Товары.")),
                [THEN],
            );
        }
        for negated in ["Ставки.Ставка НЕ МЕЖДУ &А И &Б", "Ставки.Код НЕ ПОДОБНО &Шаблон"]
        {
            assert_filtered(&format!("Товары.Срочно И {negated}"), NOTHING);
        }
        // The controls: without the excluding conjunct, or with the test that is true for the
        // missing row in its place, the field is reported.
        assert_filtered("Товары.Срочно", [THEN]);
        assert_filtered("Товары.Срочно И Ставки.Ставка ЕСТЬ NULL", [THEN]);
    }

    #[test]
    fn a_presence_test_hides_the_field_only_where_the_whole_conjunct_excludes_the_row() {
        assert_filtered("Ставки.Ставка ЕСТЬ НЕ NULL И &П", NOTHING);
        assert_filtered("НЕ (Ставки.Ставка ЕСТЬ NULL)", NOTHING);
        // A disjunction with an operand that admits the missing row excludes nothing.
        assert_filtered("Ставки.Ставка ЕСТЬ НЕ NULL ИЛИ &П", [THEN]);
        assert_filtered("Ставки.Ставка = 0 ИЛИ &П", [THEN]);
        // A disjunction whose every operand is undefined for the row excludes it as a whole.
        assert_filtered("Ставки.Ставка = 0 ИЛИ Ставки.Вид = 1", NOTHING);
    }

    #[test]
    fn a_condition_hides_only_the_side_it_is_about() {
        let full = "ИЗ Документ.Основа КАК Основа ПОЛНОЕ СОЕДИНЕНИЕ РегистрСведений.Допы КАК Допы ПО Основа.Ключ = Допы.Ключ";
        let both_sides = "Основа.Код, Допы.Код";
        assert_eq!(
            checked(&format!("ВЫБРАТЬ {both_sides} {full} ГДЕ Основа.Число > 0")),
            ["Допы.Код"]
        );
        assert_eq!(
            checked(&format!("ВЫБРАТЬ {both_sides} {full} ГДЕ Допы.Число > 0")),
            ["Основа.Код"]
        );
        assert_eq!(
            checked(&format!("ВЫБРАТЬ {both_sides} {full} ГДЕ Основа.Число > 0 И Допы.Число > 0")),
            NOTHING
        );
        assert_eq!(
            sorted(checked(&format!(
                "ВЫБРАТЬ {both_sides} {full} ГДЕ Основа.Число > 0 ИЛИ Допы.Число > 0"
            ))),
            ["Допы.Код", "Основа.Код"]
        );
        // A choice follows the same rule per side.
        assert_eq!(
            checked(&format!(
                "ВЫБРАТЬ ВЫБОР КОГДА Основа.Число > 0 ТОГДА Основа.Код ИНАЧЕ 0 КОНЕЦ, ВЫБОР КОГДА Основа.Число > 0 ТОГДА Допы.Код ИНАЧЕ 0 КОНЕЦ {full}"
            )),
            ["Допы.Код"]
        );
        // The condition of a joined table that is not the one reported does not hide it.
        let chain = "ИЗ Документ.Основа КАК Основа ЛЕВОЕ СОЕДИНЕНИЕ РегистрСведений.Допы КАК Допы ПО Основа.Ключ = Допы.Ключ ЛЕВОЕ СОЕДИНЕНИЕ РегистрСведений.Прочее КАК Прочее ПО Основа.Ключ = Прочее.Ключ";
        assert_eq!(
            checked(&format!("ВЫБРАТЬ Допы.Код, Прочее.Код {chain} ГДЕ Прочее.Число > 0")),
            ["Допы.Код"]
        );
    }

    #[test]
    fn a_condition_outside_the_selection_of_this_query_is_no_proof() {
        // The same condition in the selection hides the field.
        assert_filtered("Ставки.Ставка > 0", NOTHING);
        // Grouping condition and join condition: the field stays reported.
        assert_eq!(
            checked(&format!(
                "ВЫБРАТЬ Ставки.Код {RATES} СГРУППИРОВАТЬ ПО Ставки.Код ИМЕЮЩИЕ Ставки.Код > 0"
            )),
            [THEN]
        );
        assert_eq!(checked(&format!("ВЫБРАТЬ {THEN} {RATES} И Ставки.Ставка > 0")), [THEN]);
        // A condition on the alias of a nested query is a condition of that query.
        assert_eq!(
            checked(&format!(
                "ВЫБРАТЬ {THEN} {RATES} ГДЕ Товары.Номенклатура В (ВЫБРАТЬ Ставки.Номенклатура ИЗ РегистрСведений.Ставки КАК Ставки ГДЕ Ставки.Ставка > 0)"
            )),
            [THEN]
        );
        // The nested query is checked by its own rule: its condition hides its own field and
        // its unprotected field is reported.
        let inner = |condition: &str| {
            checked(&format!(
                "ВЫБРАТЬ Товары.Номенклатура {RATES} ГДЕ Товары.Номенклатура В (ВЫБРАТЬ Допы.Код ИЗ Документ.Основа КАК Основа ЛЕВОЕ СОЕДИНЕНИЕ РегистрСведений.Допы КАК Допы ПО Основа.Ключ = Допы.Ключ{condition})"
            ))
        };
        assert_eq!(inner(""), ["Допы.Код"]);
        assert_eq!(inner(" ГДЕ Допы.Число > 0"), NOTHING);
    }

    #[test]
    fn a_value_reaching_the_result_is_reported_whatever_its_type_would_be() {
        for field in ["Ссылка", "Наименование", "Дата", "Количество", "Действует"]
        {
            assert_eq!(
                checked(&format!("ВЫБРАТЬ Ставки.{field} {RATES}")),
                [format!("Ставки.{field}")]
            );
        }
        // The branch that is reached exactly when the field is NULL yields NULL.
        assert_eq!(
            checked(&format!(
                "ВЫБРАТЬ ВЫБОР КОГДА Ставки.Сотрудник ЕСТЬ NULL ТОГДА Ставки.ВидРасчета КОНЕЦ {RATES}"
            )),
            ["Ставки.ВидРасчета"]
        );
        // Wrapping a use removes only that use.
        assert_eq!(
            checked(&format!(
                "ВЫБРАТЬ ЕСТЬNULL(Ставки.Ставка, 0), Ставки.Ставка, Ставки.Код ЕСТЬ NULL {RATES}"
            )),
            ["Ставки.Ставка"]
        );
    }

    #[test]
    fn russian_and_english_spellings_report_the_same_fields() {
        let ru = "ИЗ Документ.Заказ КАК Doc ЛЕВОЕ СОЕДИНЕНИЕ РегистрСведений.Ставки КАК Rates ПО Doc.Item = Rates.Item";
        let en = "FROM Document.Orders AS Doc LEFT JOIN InformationRegister.Rates AS Rates ON Doc.Item = Rates.Item";
        // (russian select list and condition, english select list and condition, reported).
        let pairs: [(&str, &str, &str, &str, &[&str]); 9] = [
            (
                "ВЫБОР КОГДА Rates.Rate = 0 ТОГДА Rates.Code ИНАЧЕ Rates.Name КОНЕЦ", "",
                "CASE WHEN Rates.Rate = 0 THEN Rates.Code ELSE Rates.Name END", "",
                &["Rates.Name"],
            ),
            (
                "ВЫБОР КОГДА ЕСТЬNULL(Rates.Rate, 0) = 0 ТОГДА Rates.Code КОНЕЦ", "",
                "CASE WHEN ISNULL(Rates.Rate, 0) = 0 THEN Rates.Code END", "",
                &["Rates.Code"],
            ),
            (
                "ВЫБОР КОГДА РАЗНОСТЬДАТ(Rates.Moment, &Date, МЕСЯЦ) >= 0 ТОГДА Rates.Code КОНЕЦ", "",
                "CASE WHEN DATEDIFF(Rates.Moment, &Date, MONTH) >= 0 THEN Rates.Code END", "",
                &[],
            ),
            (
                "ВЫБОР КОГДА ДАТАВРЕМЯ(1, 1, 1) <> ДАТАВРЕМЯ(1, 1, 1) ТОГДА Rates.Code КОНЕЦ", "",
                "CASE WHEN DATETIME(1, 1, 1) <> DATETIME(1, 1, 1) THEN Rates.Code END", "",
                &[],
            ),
            ("Rates.Code", " ГДЕ Rates.Parent В (1)", "Rates.Code", " WHERE Rates.Parent IN (1)", &[]),
            (
                "Rates.Code", " ГДЕ Rates.Parent НЕ В ИЕРАРХИИ (&Parent)",
                "Rates.Code", " WHERE Rates.Parent NOT IN HIERARCHY (&Parent)",
                &["Rates.Code"],
            ),
            (
                "Rates.Code",
                " ГДЕ Rates.Rate МЕЖДУ 1 И 2 И Rates.Name ПОДОБНО &P И Rates.Link ССЫЛКА Справочник.Ставки",
                "Rates.Code",
                " WHERE Rates.Rate BETWEEN 1 AND 2 AND Rates.Name LIKE &P AND Rates.Link REFS Catalog.Rates",
                &[],
            ),
            (
                "Rates.Code", " ГДЕ Rates.Rate ЕСТЬ NULL ИЛИ НЕ Doc.Flag",
                "Rates.Code", " WHERE Rates.Rate IS NULL OR NOT Doc.Flag",
                &["Rates.Code"],
            ),
            (
                "Rates.Code", " ГДЕ НЕ (Rates.Rate ЕСТЬ NULL)",
                "Rates.Code", " WHERE NOT (Rates.Rate IS NULL)",
                &[],
            ),
        ];
        for (ru_select, ru_where, en_select, en_where, expected) in pairs {
            let ru_found = checked(&format!("ВЫБРАТЬ {ru_select} {ru}{ru_where}"));
            let en_found = checked(&format!("SELECT {en_select} {en}{en_where}"));
            assert_eq!(ru_found, expected, "{ru_select}{ru_where}");
            assert_eq!(en_found, expected, "{en_select}{en_where}");
        }

        // The kind of the join selects the side that is reported, in either language.
        let sources = [("ЛЕВОЕ", "LEFT", "Rates.Code"), ("ПРАВОЕ", "RIGHT", "Doc.Code")];
        for (ru_kind, en_kind, reported_field) in sources {
            let select = "Doc.Code, Rates.Code";
            let selected =
                |found: Vec<String>| found.into_iter().filter(|f| f == reported_field).count();
            assert_eq!(
                selected(checked(&format!("ВЫБРАТЬ {select} ИЗ Документ.Заказ КАК Doc {ru_kind} СОЕДИНЕНИЕ РегистрСведений.Ставки КАК Rates ПО Doc.Item = Rates.Item"))),
                1
            );
            assert_eq!(
                selected(checked(&format!("SELECT {select} FROM Document.Orders AS Doc {en_kind} JOIN InformationRegister.Rates AS Rates ON Doc.Item = Rates.Item"))),
                1
            );
        }
        let full_ru = checked("ВЫБРАТЬ Doc.Code, Rates.Code ИЗ Документ.Заказ КАК Doc ПОЛНОЕ СОЕДИНЕНИЕ РегистрСведений.Ставки КАК Rates ПО Doc.Item = Rates.Item ГДЕ Doc.Num > 0");
        let full_en = checked("SELECT Doc.Code, Rates.Code FROM Document.Orders AS Doc FULL JOIN InformationRegister.Rates AS Rates ON Doc.Item = Rates.Item WHERE Doc.Num > 0");
        assert_eq!(full_ru, ["Rates.Code"]);
        assert_eq!(full_en, ["Rates.Code"]);
    }

    #[test]
    fn the_condition_of_a_selection_matches_the_alias_as_written() {
        // The choice and the owner fold Latin letters; the selection condition and any
        // Cyrillic alias do not.
        let english = |alias: &str| {
            checked(&format!(
                "SELECT Rates.Code FROM Document.Orders AS Doc LEFT JOIN InformationRegister.Rates AS Rates ON Doc.Item = Rates.Item WHERE {alias}.Rate > 0"
            ))
        };
        assert_eq!(english("Rates"), NOTHING);
        assert_eq!(english("rates"), ["Rates.Code"]);
        assert_eq!(filtered("Ставки.Ставка > 0"), NOTHING);
        assert_eq!(filtered("ставки.Ставка > 0"), [THEN]);
        assert_branches("ставки.Ставка = 0", BOTH);
    }

    #[test]
    fn the_reported_range_is_exactly_the_field_in_the_bsl_string() {
        let code = r#"Процедура Выбрать(Запрос)
	Запрос.Текст =
	"ВЫБРАТЬ
	|	ВЫБОР
	|		КОГДА Ставки.Ставка = 0
	|			ТОГДА Ставки.Код
	|		ИНАЧЕ Ставки.Вид
	|	КОНЕЦ КАК Р
	|ИЗ Документ.Заказ КАК Заказы
	|ЛЕВОЕ СОЕДИНЕНИЕ РегистрСведений.Ставки КАК Ставки
	|ПО Заказы.Курс = Ставки.Курс";
КонецПроцедуры
"#;
        check(
            code,
            expect![[r#"
            FieldsFromJoinsWithoutIsNull @ 7:11..7:21
              message: Для полей из ЛЕВОГО СОЕДИНЕНИЯ добавьте проверку через ЕСТЬ NULL или используйте функцию ЕСТЬNULL, либо замените на ВНУТРЕННЕЕ СОЕДИНЕНИЕ
              severity: Warning"#]],
        );
    }
}
