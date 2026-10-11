use crate::define_metadata;
use crate::metadata::*;
use crate::{Diagnostic, DiagnosticCode, DiagnosticsConfig, DiagnosticsContext};

pub const METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: DiagnosticType::CodeSmell,
    severity: DiagnosticSeverityLevel::Major,
    scope: DiagnosticScope::All,
    modules: &[],
    minutes_to_fix: 10,
    activated_by_default: true,
    compatibility_mode: DiagnosticCompatibilityMode::Undefined,
    tags: &[MetadataTag::Standard, MetadataTag::Sql, MetadataTag::Performance],
    can_locate_on_project: false,
    extra_min_for_complexity: 0.0,
    lsp_severity_override: "",
};

const DEFAULT_MIN_PATH_DEPTH: i64 = 3;

pub(crate) fn dispatch(
    config: &DiagnosticsConfig,
    diag: &sdbl_hir::SdblDiagnostic,
    mapper: &crate::sdbl_utils::SdblPositionMapper,
    query_text: &str,
    diagnostics: &mut Vec<Diagnostic>,
) {
    if let sdbl_hir::SdblDiagnostic::QueryNestedFieldsByDot { range, parts_count } = diag {
        if let Some(n) = parts_count {
            let min = config
                .get_int(DiagnosticCode::QueryNestedFieldsByDot, "minPathDepth")
                .unwrap_or(DEFAULT_MIN_PATH_DEPTH);
            if (*n as i64) < min {
                return;
            }
        }
        crate::sdbl_utils::dispatch_simple(
            config,
            DiagnosticCode::QueryNestedFieldsByDot,
            "Обнаружено разыменование ссылочного поля",
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
        DiagnosticCode::QueryNestedFieldsByDot,
        dispatch,
    )
}

#[cfg(test)]
mod tests {
    use crate::test_utils::{
        check_diagnostics_snapshot_for, check_hir_diagnostic_with_config, format_diags,
    };
    use crate::DiagnosticCode;
    use expect_test::expect;
    #[test]
    fn test_query_nested_fields_by_dot() {
        let code = r#"Процедура СводкаУспеваемости(Начало, Конец)

	Сводка = Новый Запрос;
	Сводка.Текст =
	"ВЫБРАТЬ
	|	Оценки.Ссылка КАК Журнал,
	|	Оценки.Ученик КАК Ученик,
	|	Оценки.Балл * Оценки.Вес КАК ВзвешенныйБалл,
	|	Оценки.Ссылка.Класс КАК Класс,
	|	Оценки.Ссылка.Предмет КАК Предмет,
	|	Оценки.Ссылка.Учитель КАК Учитель
	|ПОМЕСТИТЬ ВТ_Оценки
	|ИЗ
	|	Документ.ЖурналОценок.Оценки КАК Оценки
	|ГДЕ
	|	Оценки.Ссылка.Дата >= &Начало
	|
	|ИНДЕКСИРОВАТЬ ПО
	|	Класс,
	|	Предмет
	|;
	|
	|ВЫБРАТЬ
	|	Посещения.Ученик КАК Ученик,
	|	Посещения.ПропускиОборот КАК Пропуски
	|ПОМЕСТИТЬ ВТ_Посещения
	|ИЗ
	|	РегистрНакопления.Посещаемость.Обороты(
	|			&Начало,
	|			&Конец,
	|			,
	|			(Ученик.Класс, Ученик.Наставник) В
	|				(ВЫБРАТЬ
	|					ВТ_Оценки.Класс КАК Класс,
	|					ВТ_Оценки.Учитель КАК Учитель
	|				ИЗ
	|					ВТ_Оценки КАК ВТ_Оценки)) КАК Посещения
	|;
	|
	|ВЫБРАТЬ
	|	ВТ_Посещения.Ученик КАК Ученик,
	|	МАКСИМУМ(ВТ_Оценки.Балл) КАК ЛучшийБалл,
	|	ЕСТЬNULL(ВТ_Посещения.Пропуски, 0) КАК Пропуски,
	|	ЗНАЧЕНИЕ(Документ.ЖурналОценок.ПустаяСсылка) КАК ПустойЖурнал
	|ИЗ
	|	ВТ_Оценки КАК ВТ_Оценки
	|		ЛЕВОЕ СОЕДИНЕНИЕ ВТ_Посещения КАК ВТ_Посещения
	|		ПО ВТ_Оценки.Ученик = ВТ_Посещения.Ученик
	|			И ВТ_Оценки.Класс.Параллель = ВТ_Посещения.Ученик.Параллель
	|СГРУППИРОВАТЬ ПО
	|	ВТ_Посещения.Ученик,
	|	ВТ_Посещения.Пропуски
	|;
	|
	|ВЫБРАТЬ
	|	ВЫРАЗИТЬ(ВТ_Оценки.Журнал КАК Документ.ЖурналОценок).Четверть КАК Четверть,
	|	ВЫРАЗИТЬ(ВТ_Оценки.Журнал КАК Документ.ЖурналОценок).Четверть.Начало КАК НачалоЧетверти
	|ИЗ
	|	ВТ_Оценки КАК ВТ_Оценки";

	Сводка.УстановитьПараметр("Начало", Начало);
	Сводка.УстановитьПараметр("Конец", Конец);
	Таблица = Сводка.Выполнить().Выгрузить();

	Нормы = Новый Запрос;
	Нормы.Текст =
		"ВЫБРАТЬ
		|	НормыСрез.МинимальныйБалл КАК МинимальныйБалл
		|ИЗ
		|	РегистрСведений.НормыОценок.СрезПоследних(&Конец, Предмет = &Предмет) КАК НормыСрез";
	Нормы.УстановитьПараметр("Конец", Конец);
	Выборка = Нормы.Выполнить().Выбрать();

КонецПроцедуры
"#;

        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::QueryNestedFieldsByDot,
            expect![[r#"
                QueryNestedFieldsByDot @ 9:4..9:23
                  message: Обнаружено разыменование ссылочного поля
                  severity: Warning
                QueryNestedFieldsByDot @ 10:4..10:25
                  message: Обнаружено разыменование ссылочного поля
                  severity: Warning
                QueryNestedFieldsByDot @ 11:4..11:25
                  message: Обнаружено разыменование ссылочного поля
                  severity: Warning
                QueryNestedFieldsByDot @ 16:4..16:22
                  message: Обнаружено разыменование ссылочного поля
                  severity: Warning
                QueryNestedFieldsByDot @ 32:7..32:19
                  message: Обнаружено разыменование ссылочного поля
                  severity: Warning
                QueryNestedFieldsByDot @ 32:21..32:37
                  message: Обнаружено разыменование ссылочного поля
                  severity: Warning
                QueryNestedFieldsByDot @ 49:8..49:33
                  message: Обнаружено разыменование ссылочного поля
                  severity: Warning
                QueryNestedFieldsByDot @ 49:36..49:65
                  message: Обнаружено разыменование ссылочного поля
                  severity: Warning
                QueryNestedFieldsByDot @ 57:4..57:72
                  message: Обнаружено разыменование ссылочного поля
                  severity: Warning"#]],
        );
    }

    #[test]
    fn test_no_false_positives_for_mdo_types() {
        let code = r#"
Процедура Тест()
    Запрос = Новый Запрос;
    Запрос.Текст = "ВЫБРАТЬ Справочник.Валюты.Код ИЗ Справочник.Валюты";
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::QueryNestedFieldsByDot,
            expect![[r#""#]],
        );
    }

    #[test]
    fn test_no_false_positives_for_two_parts() {
        let code = r#"
Процедура Тест()
    Запрос = Новый Запрос;
    Запрос.Текст = "ВЫБРАТЬ T.Ссылка ИЗ Документ.Заказ КАК T";
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::QueryNestedFieldsByDot,
            expect![[r#""#]],
        );
    }

    fn config_with_min_path_depth(depth: i64) -> crate::DiagnosticsConfig {
        let mut config = crate::DiagnosticsConfig::default();
        config.enabled.push(DiagnosticCode::QueryNestedFieldsByDot);
        config.parameters.insert(
            DiagnosticCode::QueryNestedFieldsByDot,
            serde_json::json!({ "minPathDepth": depth }),
        );
        config
    }

    #[test]
    fn test_min_path_depth_above_three_drops_three_part_normal_context() {
        let code = r#"
Процедура Тест()
    Запрос = Новый Запрос;
    Запрос.Текст = "ВЫБРАТЬ T.Ссылка.Контрагент ИЗ Документ.Заказ КАК T";
КонецПроцедуры
"#;
        let config = config_with_min_path_depth(4);
        let diagnostics = check_hir_diagnostic_with_config(code, config, crate::diagnostics);
        let diagnostics: Vec<_> = diagnostics
            .into_iter()
            .filter(|d| d.code == DiagnosticCode::QueryNestedFieldsByDot)
            .collect();
        expect![[r#""#]].assert_eq(&format_diags(code, &diagnostics));
    }

    #[test]
    fn test_min_path_depth_above_three_preserves_cast_member_chain() {
        let code = r#"
Процедура Тест()
    Запрос = Новый Запрос;
    Запрос.Текст =
    "ВЫБРАТЬ ВЫРАЗИТЬ(T.Ссылка КАК Документ.Заказ).Валюта.Курс
    |ИЗ Документ.Заказ КАК T";
КонецПроцедуры
"#;
        let config = config_with_min_path_depth(99);
        let diagnostics = check_hir_diagnostic_with_config(code, config, crate::diagnostics);
        let diagnostics: Vec<_> = diagnostics
            .into_iter()
            .filter(|d| d.code == DiagnosticCode::QueryNestedFieldsByDot)
            .collect();
        expect![[r#"
            QueryNestedFieldsByDot @ 5:14..5:63
              message: Обнаружено разыменование ссылочного поля
              severity: Warning"#]]
        .assert_eq(&format_diags(code, &diagnostics));
    }

    #[test]
    fn inline_tabular_fields_are_not_a_dereference_hop() {
        // `Т.ТЧ.(А, Б)` — выбор полей табличной части. Разыменования ссылки
        // здесь нет, глубина пути равна двум, и при пороге по умолчанию
        // находки быть не должно.
        let code = r#"
Процедура Тест()
    Запрос = Новый Запрос;
    Запрос.Текст = "ВЫБРАТЬ T.Товары.(Номенклатура, Количество) ИЗ Документ.Заказ КАК T";
КонецПроцедуры
"#;
        let config = config_with_min_path_depth(3);
        let diagnostics = check_hir_diagnostic_with_config(code, config, crate::diagnostics);
        let diagnostics: Vec<_> = diagnostics
            .into_iter()
            .filter(|d| d.code == DiagnosticCode::QueryNestedFieldsByDot)
            .collect();
        expect![[r#""#]].assert_eq(&format_diags(code, &diagnostics));
    }

    #[test]
    fn a_dereference_before_inline_tabular_fields_still_fires() {
        let code = r#"
Процедура Тест()
    Запрос = Новый Запрос;
    Запрос.Текст = "ВЫБРАТЬ T.Ссылка.Товары.(Номенклатура) ИЗ Документ.Заказ КАК T";
КонецПроцедуры
"#;
        let config = config_with_min_path_depth(3);
        let diagnostics = check_hir_diagnostic_with_config(code, config, crate::diagnostics);
        let diagnostics: Vec<_> = diagnostics
            .into_iter()
            .filter(|d| d.code == DiagnosticCode::QueryNestedFieldsByDot)
            .collect();
        expect![[r#"
            QueryNestedFieldsByDot @ 4:29..4:44
              message: Обнаружено разыменование ссылочного поля
              severity: Warning"#]]
        .assert_eq(&format_diags(code, &diagnostics));
    }

    #[test]
    fn test_min_path_depth_two_emits_two_part_normal_context() {
        let code = r#"
Процедура Тест()
    Запрос = Новый Запрос;
    Запрос.Текст = "ВЫБРАТЬ T.Поле ИЗ Документ.Заказ КАК T";
КонецПроцедуры
"#;
        let config = config_with_min_path_depth(2);
        let diagnostics = check_hir_diagnostic_with_config(code, config, crate::diagnostics);
        let diagnostics: Vec<_> = diagnostics
            .into_iter()
            .filter(|d| d.code == DiagnosticCode::QueryNestedFieldsByDot)
            .collect();
        expect![[r#"
            QueryNestedFieldsByDot @ 4:29..4:35
              message: Обнаружено разыменование ссылочного поля
              severity: Warning"#]]
        .assert_eq(&format_diags(code, &diagnostics));
    }
}
