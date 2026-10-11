use crate::define_metadata;
use crate::metadata::*;
use crate::{Diagnostic, DiagnosticCode, DiagnosticsConfig, DiagnosticsContext};

pub const METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: DiagnosticType::CodeSmell,
    severity: DiagnosticSeverityLevel::Major,
    scope: DiagnosticScope::All,
    modules: &[],
    minutes_to_fix: 15,
    activated_by_default: true,
    compatibility_mode: DiagnosticCompatibilityMode::Undefined,
    tags: &[MetadataTag::Sql, MetadataTag::Performance, MetadataTag::Unpredictable],
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
    if let sdbl_hir::SdblDiagnostic::LogicalOrInJoin { range } = diag {
        crate::sdbl_utils::dispatch_simple(
            config,
            DiagnosticCode::LogicalOrInJoinQuerySection,
            "ИЛИ в условии соединения мешает СУБД использовать индекс, если не сводится к В; разбивать запрос на части через ОБЪЕДИНИТЬ ВСЕ можно, только если результат не изменится",
            *range,
            mapper,
            query_text,
            diagnostics,
        );
    }
}

pub fn check(ctx: &DiagnosticsContext) -> Vec<Diagnostic> {
    crate::sdbl_utils::collect_sdbl_via_dispatch(
        ctx,
        DiagnosticCode::LogicalOrInJoinQuerySection,
        dispatch,
    )
}

#[cfg(test)]
mod tests {
    use crate::test_utils::check_diagnostics_snapshot_for;
    use crate::DiagnosticCode;
    use expect_test::expect;

    fn check(code: &str, expected: expect_test::Expect) {
        check_diagnostics_snapshot_for(code, DiagnosticCode::LogicalOrInJoinQuerySection, expected);
    }

    #[test]
    fn test_logical_or_in_join_query_section() {
        // ИЛИ over one field is left to the planner (it becomes IN); ИЛИ over different
        // fields in an ON condition is reported once per operator, also in a join nested
        // inside another join. ИЛИ in the selection list is not a join condition.
        let code = r#"Функция ЗапросРейсов()
	Возврат
	"ВЫБРАТЬ
	|	Рейсы.Номер КАК Номер,
	|	Рейсы.Задержка > 0
	|		ИЛИ Рейсы.Отменен КАК Проблемный
	|ИЗ
	|	Справочник.Рейсы КАК Рейсы
	|		ВНУТРЕННЕЕ СОЕДИНЕНИЕ Справочник.Маршруты КАК Маршруты
	|		ПО Рейсы.Маршрут = Маршруты.Ссылка
	|			И (Маршруты.Дальность > 500 ИЛИ Рейсы.Ночной ИЛИ Маршруты.Международный)
	|		ЛЕВОЕ СОЕДИНЕНИЕ Справочник.Перроны КАК Перроны
	|			ЛЕВОЕ СОЕДИНЕНИЕ Справочник.Вокзалы КАК Вокзалы
	|			ПО Перроны.Вокзал = Вокзалы.Ссылка
	|				И (Перроны.Длина > 200
	|					ИЛИ Перроны.Длина < 50)
	|				И (Перроны.Крытый
	|					ИЛИ Вокзалы.Отапливаемый)
	|		ПО Рейсы.Перрон = Перроны.Ссылка
	|			И (Перроны.Код = ""П1""
	|				ИЛИ Перроны.Код = ""П2""
	|				ИЛИ Перроны.Код = ""П3"")
	|			И (Перроны.Код = ""П4""
	|				ИЛИ Рейсы.РезервныйПеррон = Перроны.Ссылка)";
КонецФункции
"#;
        check(
            code,
            expect![[r#"
                LogicalOrInJoinQuerySection @ 11:34..11:37
                  message: ИЛИ в условии соединения мешает СУБД использовать индекс, если не сводится к В; разбивать запрос на части через ОБЪЕДИНИТЬ ВСЕ можно, только если результат не изменится
                  severity: Warning
                LogicalOrInJoinQuerySection @ 11:51..11:54
                  message: ИЛИ в условии соединения мешает СУБД использовать индекс, если не сводится к В; разбивать запрос на части через ОБЪЕДИНИТЬ ВСЕ можно, только если результат не изменится
                  severity: Warning
                LogicalOrInJoinQuerySection @ 18:8..18:11
                  message: ИЛИ в условии соединения мешает СУБД использовать индекс, если не сводится к В; разбивать запрос на части через ОБЪЕДИНИТЬ ВСЕ можно, только если результат не изменится
                  severity: Warning
                LogicalOrInJoinQuerySection @ 24:7..24:10
                  message: ИЛИ в условии соединения мешает СУБД использовать индекс, если не сводится к В; разбивать запрос на части через ОБЪЕДИНИТЬ ВСЕ можно, только если результат не изменится
                  severity: Warning"#]],
        );
    }

    #[test]
    fn test_same_field_no_trigger() {
        let code = r#"Процедура Табло(Запрос)
	Запрос.Текст = "SELECT * FROM Flights AS F
	|LEFT JOIN Platforms AS P ON F.Platform = P.Ref
	|	AND (P.Code = 1 OR P.Code = 2)";
КонецПроцедуры
"#;
        check(code, expect![[r#""#]]);
    }

    #[test]
    fn test_or_in_select_no_trigger() {
        let code = r#"Процедура Табло(Запрос)
	Запрос.Текст = "SELECT F.Delay > 0 OR F.Cancelled AS Bad FROM Flights AS F";
КонецПроцедуры
"#;
        check(code, expect![[r#""#]]);
    }

    #[test]
    fn test_multiple_fields_trigger() {
        let code = r#"Процедура Табло(Запрос)
	Запрос.Текст = "SELECT * FROM Flights AS F INNER JOIN Routes AS R ON F.Route = R.Ref AND (F.Delay > 10 OR R.Length > 900)";
КонецПроцедуры
"#;
        check(
            code,
            expect![[r#"
                LogicalOrInJoinQuerySection @ 2:105..2:107
                  message: ИЛИ в условии соединения мешает СУБД использовать индекс, если не сводится к В; разбивать запрос на части через ОБЪЕДИНИТЬ ВСЕ можно, только если результат не изменится
                  severity: Warning"#]],
        );
    }

    #[test]
    fn test_bilingual_english() {
        let code = r#"Procedure Board(Query)
	Query.Text = "SELECT * FROM Flights AS F
	|INNER JOIN Routes AS R ON F.Route = R.Ref
	|	AND (F.Night = TRUE OR R.Length = 2)";
EndProcedure
"#;
        check(
            code,
            expect![[r#"
                LogicalOrInJoinQuerySection @ 4:24..4:26
                  message: ИЛИ в условии соединения мешает СУБД использовать индекс, если не сводится к В; разбивать запрос на части через ОБЪЕДИНИТЬ ВСЕ можно, только если результат не изменится
                  severity: Warning"#]],
        );
    }

    #[test]
    fn test_bilingual_russian() {
        let code = r#"Процедура Табло(Запрос)
	Запрос.Текст = "ВЫБРАТЬ * ИЗ Рейсы КАК Р
	|ВНУТРЕННЕЕ СОЕДИНЕНИЕ Маршруты КАК М ПО Р.Маршрут = М.Ссылка
	|	И (Р.Ночной = ИСТИНА ИЛИ М.Дальность = 2)";
КонецПроцедуры
"#;
        check(
            code,
            expect![[r#"
                LogicalOrInJoinQuerySection @ 4:25..4:28
                  message: ИЛИ в условии соединения мешает СУБД использовать индекс, если не сводится к В; разбивать запрос на части через ОБЪЕДИНИТЬ ВСЕ можно, только если результат не изменится
                  severity: Warning"#]],
        );
    }

    #[test]
    fn test_three_part_field_path_no_leak() {
        let code = r#"Процедура Табло(Запрос)
	Запрос.Текст = "ВЫБРАТЬ * ИЗ Рейсы КАК Р
	|ВНУТРЕННЕЕ СОЕДИНЕНИЕ Маршруты КАК М ПО Р.Маршрут = М.Ссылка
	|	И (М.Начало.Город = 1 ИЛИ М.Начало.Город = 2)";
КонецПроцедуры
"#;
        check(code, expect![[r#""#]]);
    }

    #[test]
    fn test_undefined_literal_in_or_is_not_field() {
        let code = r#"Процедура Табло(Запрос)
	Запрос.Текст = "ВЫБРАТЬ * ИЗ Рейсы КАК Р
	|ВНУТРЕННЕЕ СОЕДИНЕНИЕ Маршруты КАК М ПО Р.Маршрут = М.Ссылка
	|	И (М.Оператор = НЕОПРЕДЕЛЕНО ИЛИ М.Оператор = &Оператор)";
КонецПроцедуры
"#;
        check(code, expect![[r#""#]]);
    }
}
