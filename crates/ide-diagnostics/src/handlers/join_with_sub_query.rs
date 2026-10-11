use crate::define_metadata;
use crate::metadata::*;
use crate::{Diagnostic, DiagnosticCode, DiagnosticsConfig, DiagnosticsContext};

pub const METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: DiagnosticType::CodeSmell,
    severity: DiagnosticSeverityLevel::Major,
    scope: DiagnosticScope::Bsl,
    modules: &[],
    minutes_to_fix: 10,
    activated_by_default: true,
    compatibility_mode: DiagnosticCompatibilityMode::Undefined,
    tags: &[MetadataTag::Sql, MetadataTag::Standard, MetadataTag::Performance],
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
    if let sdbl_hir::SdblDiagnostic::JoinWithSubQuery { range } = diag {
        crate::sdbl_utils::dispatch_simple(config, DiagnosticCode::JoinWithSubQuery, "Не используйте соединение с подзапросами. Соединения с подзапросами вызывают серьезные проблемы с производительностью", *range, mapper, query_text, diagnostics);
    }
}

pub fn check(ctx: &DiagnosticsContext) -> Vec<Diagnostic> {
    crate::sdbl_utils::collect_sdbl_via_dispatch(ctx, DiagnosticCode::JoinWithSubQuery, dispatch)
}

#[cfg(test)]
mod tests {
    use crate::test_utils::check_diagnostics_snapshot_for;
    use crate::DiagnosticCode;
    use expect_test::expect;

    #[test]
    fn test_join_with_sub_query_multi_case() {
        let code = r#"Процедура Тест1()

    Запрос = Новый Запрос;
    Запрос.Текст = "Выбрать Заказы.Ссылка
    |Из Документ.ЗаказПокупателя КАК Заказы
    |Левое соединение (Выбрать Источник.Регистратор Из РегистрСведений.Лимиты КАК Источник Где Источник.Склад = &Склад) КАК Лимиты
    |По Заказы.Ссылка = Лимиты.Регистратор"; //<-- ошибка

КонецПроцедуры

Процедура Тест2()

    Запрос = Новый Запрос;
    Запрос.Текст = "Выбрать Продажи.Ссылка
    |Из Документ.РеализацияТоваровУслуг КАК Продажи
    |Левое соединение
    |(Выбрать Остатки.Номенклатура Из РегистрНакопления.ОстаткиТоваров.Остатки КАК Остатки Где Остатки.Склад = &Склад) КАК Остатки //<-- ошибка
    |По Продажи.Товар = Остатки.Номенклатура";

КонецПроцедуры

Процедура Тест3()

    Запрос = Новый Запрос;
    Запрос.Текст = "Выбрать Маршруты.Ссылка
    |Из Справочник.МаршрутыДоставки КАК Маршруты
    |Правое соединение
    |(Выбрать Зоны.Ссылка Из Справочник.ЗоныДоставки КАК Зоны Где Зоны.Город = &Город) КАК Зоны //<-- ошибка
    |По Маршруты.Зона = Зоны.Ссылка";

КонецПроцедуры

Процедура Тест4()

    Запрос = Новый Запрос;
    Запрос.Текст = "Выбрать Клиенты.Ссылка
    | Из (Выбрать СписокКлиентов.Ссылка Из Справочник.Клиенты КАК СписокКлиентов Где СписокКлиентов.Группа = &Группа) КАК Клиенты Левое соединение //<-- ошибка
    |
    |(Выбрать Договоры.Владелец Из Справочник.ДоговорыКонтрагентов КАК Договоры Где Договоры.ВидДоговора = &ВидДоговора) КАК Договоры //<-- ошибка
    |По Клиенты.Ссылка = Договоры.Владелец";

КонецПроцедуры

Процедура Тест5()

    Запрос = Новый Запрос;
    Запрос.Текст = "Выбрать Контрагенты.Ссылка
    |
    |Из(Выбрать ЧерныйСписок.Ссылка Из Справочник.Контрагенты КАК ЧерныйСписок Где ЧерныйСписок.ПометкаУдаления = Истина) КАК ЧерныйСписок //<-- ошибка
    |Правое соединение Справочник.Контрагенты КАК Контрагенты
    |По Контрагенты.Ссылка = ЧерныйСписок.Ссылка";

КонецПроцедуры

Процедура Тест6()

    Запрос = Новый Запрос;
    Запрос.Текст = "Выбрать *
    |Из (Выбрать Секции.Ссылка
    |Из(Выбрать Отбор.Секция Из РегистрСведений.НастройкиСклада КАК Отбор //<-- ошибка
    |Где Отбор.Склад = &Склад) КАК Отбор
    |Правое соединение Справочник.СекцииСклада КАК Секции
    |По Секции.Ссылка = Отбор.Секция) КАК Итог";

КонецПроцедуры

Процедура Тест7()

    Запрос = Новый Запрос;
    Запрос.Текст = "Выбрать Категории.Ссылка
    | Из (Выбрать СписокКатегорий.Ссылка Из Справочник.КатегорииНоменклатуры КАК СписокКатегорий Где СписокКатегорий.ЭтоГруппа = Ложь) КАК Категории,
    |(Выбрать Группы.Ссылка Из Справочник.КатегорииНоменклатуры КАК Группы Где Группы.ЭтоГруппа = Истина) КАК Группы";

КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::JoinWithSubQuery,
            expect![[r#"
                JoinWithSubQuery @ 6:24..6:119
                  message: Не используйте соединение с подзапросами. Соединения с подзапросами вызывают серьезные проблемы с производительностью
                  severity: Warning
                JoinWithSubQuery @ 17:7..17:118
                  message: Не используйте соединение с подзапросами. Соединения с подзапросами вызывают серьезные проблемы с производительностью
                  severity: Warning
                JoinWithSubQuery @ 28:7..28:86
                  message: Не используйте соединение с подзапросами. Соединения с подзапросами вызывают серьезные проблемы с производительностью
                  severity: Warning
                JoinWithSubQuery @ 37:11..37:117
                  message: Не используйте соединение с подзапросами. Соединения с подзапросами вызывают серьезные проблемы с производительностью
                  severity: Warning
                JoinWithSubQuery @ 39:7..39:120
                  message: Не используйте соединение с подзапросами. Соединения с подзапросами вызывают серьезные проблемы с производительностью
                  severity: Warning
                JoinWithSubQuery @ 49:9..49:121
                  message: Не используйте соединение с подзапросами. Соединения с подзапросами вызывают серьезные проблемы с производительностью
                  severity: Warning
                JoinWithSubQuery @ 60:9..61:30
                  message: Не используйте соединение с подзапросами. Соединения с подзапросами вызывают серьезные проблемы с производительностью
                  severity: Warning"#]],
        );
    }

    #[test]
    fn test_simple_left_join_with_subquery() {
        let code = r#"
Процедура Тест()
    Запрос = "ВЫБРАТЬ * ИЗ Т1 ЛЕВОЕ СОЕДИНЕНИЕ (ВЫБРАТЬ * ИЗ Т2) КАК С ПО Т1.ID = С.ID";
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::JoinWithSubQuery,
            expect![[r#"
                JoinWithSubQuery @ 3:49..3:64
                  message: Не используйте соединение с подзапросами. Соединения с подзапросами вызывают серьезные проблемы с производительностью
                  severity: Warning"#]],
        );
    }

    #[test]
    fn test_no_false_positive_table_join() {
        let code = r#"
Процедура Тест()
    Запрос = "ВЫБРАТЬ * ИЗ Справочник.Товары ЛЕВОЕ СОЕДИНЕНИЕ Справочник.Цены ПО ID";
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(code, DiagnosticCode::JoinWithSubQuery, expect![[r#""#]]);
    }

    #[test]
    fn test_no_false_positive_subquery_without_join() {
        let code = r#"
Процедура Тест7()
    Запрос = "ВЫБРАТЬ * ИЗ (ВЫБРАТЬ * ИЗ Т1) КАК С1, (ВЫБРАТЬ * ИЗ Т2) КАК С2";
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(code, DiagnosticCode::JoinWithSubQuery, expect![[r#""#]]);
    }

    #[test]
    fn test_right_join_with_subquery() {
        let code = r#"
Процедура Тест()
    Запрос = "ВЫБРАТЬ * ИЗ Т1 ПРАВОЕ СОЕДИНЕНИЕ (ВЫБРАТЬ * ИЗ Т2) КАК С ПО ID";
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::JoinWithSubQuery,
            expect![[r#"
                JoinWithSubQuery @ 3:50..3:65
                  message: Не используйте соединение с подзапросами. Соединения с подзапросами вызывают серьезные проблемы с производительностью
                  severity: Warning"#]],
        );
    }

    #[test]
    fn test_inner_join_with_subquery() {
        let code = r#"
Процедура Тест()
    Запрос = "ВЫБРАТЬ * ИЗ Т1 ВНУТРЕННЕЕ СОЕДИНЕНИЕ (ВЫБРАТЬ * ИЗ Т2) ПО ID";
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::JoinWithSubQuery,
            expect![[r#"
                JoinWithSubQuery @ 3:54..3:69
                  message: Не используйте соединение с подзапросами. Соединения с подзапросами вызывают серьезные проблемы с производительностью
                  severity: Warning"#]],
        );
    }

    #[test]
    fn test_subquery_in_from_with_joins() {
        let code = r#"
Процедура Тест()
    Запрос = "ВЫБРАТЬ * ИЗ (ВЫБРАТЬ * ИЗ Т1) КАК С ЛЕВОЕ СОЕДИНЕНИЕ Т2 ПО ID";
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::JoinWithSubQuery,
            expect![[r#"
            JoinWithSubQuery @ 3:29..3:44
              message: Не используйте соединение с подзапросами. Соединения с подзапросами вызывают серьезные проблемы с производительностью
              severity: Warning"#]],
        );
    }

    #[test]
    fn test_multiline_subquery_in_from_with_joins() {
        let code = r#"
Процедура СвободныеНомера()
    Текст = "ВЫБРАТЬ Брони.Номер КАК Номер
    | ИЗ (ВЫБРАТЬ Н.Ссылка КАК Номер ИЗ Справочник.Номера КАК Н ГДЕ Н.Этаж = &Этаж) КАК Этаж ЛЕВОЕ СОЕДИНЕНИЕ
    |(ВЫБРАТЬ Б.Номер КАК Номер ИЗ Документ.Бронь КАК Б ГДЕ Б.Дата = &Дата) КАК Брони
    |ПО Этаж.Номер = Брони.Номер";
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::JoinWithSubQuery,
            expect![[r#"
                JoinWithSubQuery @ 4:11..4:83
                  message: Не используйте соединение с подзапросами. Соединения с подзапросами вызывают серьезные проблемы с производительностью
                  severity: Warning
                JoinWithSubQuery @ 5:7..5:75
                  message: Не используйте соединение с подзапросами. Соединения с подзапросами вызывают серьезные проблемы с производительностью
                  severity: Warning"#]],
        );
    }

    #[test]
    fn test_join_with_sum_subquery_emits() {
        let code = r#"
Процедура Тест()
    Запрос = "ВЫБРАТЬ T1.Ссылка ИЗ Документ.Заказ КАК T1
              |ЛЕВОЕ СОЕДИНЕНИЕ (ВЫБРАТЬ Регистратор, СУММА(Сумма) КАК Итог
              |ИЗ РегистрНакопления.Х
              |СГРУППИРОВАТЬ ПО Регистратор) КАК Агрегат
              |ПО T1.Ссылка = Агрегат.Регистратор";
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::JoinWithSubQuery,
            expect![[r#"
                JoinWithSubQuery @ 4:34..6:44
                  message: Не используйте соединение с подзапросами. Соединения с подзапросами вызывают серьезные проблемы с производительностью
                  severity: Warning"#]],
        );
    }

    #[test]
    fn test_join_with_count_english_subquery_emits() {
        let code = r#"
Процедура Тест()
    Запрос = "SELECT * FROM Catalog.Products AS P
              |LEFT JOIN (SELECT COUNT(*) AS Total FROM Catalog.Suppliers) AS Counts
              |ON 1 = 1";
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::JoinWithSubQuery,
            expect![[r#"
                JoinWithSubQuery @ 4:27..4:74
                  message: Не используйте соединение с подзапросами. Соединения с подзапросами вызывают серьезные проблемы с производительностью
                  severity: Warning"#]],
        );
    }

    #[test]
    fn test_join_with_group_by_only_subquery_emits() {
        let code = r#"
Процедура Тест()
    Запрос = "ВЫБРАТЬ T1.Ссылка ИЗ Документ.Заказ КАК T1
              |ЛЕВОЕ СОЕДИНЕНИЕ (ВЫБРАТЬ Регистратор ИЗ РегистрНакопления.Х
              |СГРУППИРОВАТЬ ПО Регистратор) КАК Группы
              |ПО T1.Ссылка = Группы.Регистратор";
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::JoinWithSubQuery,
            expect![[r#"
                JoinWithSubQuery @ 4:34..5:44
                  message: Не используйте соединение с подзапросами. Соединения с подзапросами вызывают серьезные проблемы с производительностью
                  severity: Warning"#]],
        );
    }

    #[test]
    fn test_join_with_nested_isnull_around_sum_emits() {
        let code = r#"
Процедура Тест()
    Запрос = "ВЫБРАТЬ T1.Ссылка ИЗ Документ.Заказ КАК T1
              |ЛЕВОЕ СОЕДИНЕНИЕ (ВЫБРАТЬ Регистратор, ЕСТЬNULL(СУММА(Сумма), 0) КАК Итог
              |ИЗ РегистрНакопления.Х
              |СГРУППИРОВАТЬ ПО Регистратор) КАК Агрегат
              |ПО T1.Ссылка = Агрегат.Регистратор";
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::JoinWithSubQuery,
            expect![[r#"
                JoinWithSubQuery @ 4:34..6:44
                  message: Не используйте соединение с подзапросами. Соединения с подзапросами вызывают серьезные проблемы с производительностью
                  severity: Warning"#]],
        );
    }

    #[test]
    fn test_from_aggregating_subquery_with_join_emits() {
        let code = r#"
Процедура Тест()
    Запрос = "ВЫБРАТЬ Агрегат.Регистратор
              |ИЗ (ВЫБРАТЬ Регистратор, СУММА(Сумма) КАК Итог
              |ИЗ РегистрНакопления.Х
              |СГРУППИРОВАТЬ ПО Регистратор) КАК Агрегат
              |ЛЕВОЕ СОЕДИНЕНИЕ Документ.Заказ КАК T2
              |ПО Агрегат.Регистратор = T2.Ссылка";
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::JoinWithSubQuery,
            expect![[r#"
                JoinWithSubQuery @ 4:20..6:44
                  message: Не используйте соединение с подзапросами. Соединения с подзапросами вызывают серьезные проблемы с производительностью
                  severity: Warning"#]],
        );
    }

    #[test]
    fn test_join_with_subquery_column_named_summa_still_emits() {
        let code = r#"
Процедура Тест()
    Запрос = "ВЫБРАТЬ * ИЗ Т1 ЛЕВОЕ СОЕДИНЕНИЕ (ВЫБРАТЬ Сумма ИЗ Т2) КАК С ПО Т1.ID = С.ID";
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::JoinWithSubQuery,
            expect![[r#"
                JoinWithSubQuery @ 3:49..3:68
                  message: Не используйте соединение с подзапросами. Соединения с подзапросами вызывают серьезные проблемы с производительностью
                  severity: Warning"#]],
        );
    }

    #[test]
    fn test_join_with_subquery_alias_named_sum_still_emits() {
        let code = r#"
Процедура Тест()
    Запрос = "SELECT * FROM T1 LEFT JOIN (SELECT Field1 AS Sum FROM T2) AS S ON T1.ID = S.ID";
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::JoinWithSubQuery,
            expect![[r#"
                JoinWithSubQuery @ 3:43..3:71
                  message: Не используйте соединение с подзапросами. Соединения с подзапросами вызывают серьезные проблемы с производительностью
                  severity: Warning"#]],
        );
    }

    #[test]
    fn join_with_nested_inner_aggregation_emits() {
        check_diagnostics_snapshot_for(
            r#"Процедура Тест()
    Запрос = Новый Запрос;
    Запрос.Текст =
        "ВЫБРАТЬ Заказы.Ссылка КАК Ссылка
        |ИЗ Документ.ЗаказПокупателя КАК Заказы
        |ЛЕВОЕ СОЕДИНЕНИЕ (
        |   ВЫБРАТЬ Вложенный.Регистратор КАК Регистратор
        |   ИЗ (
        |       ВЫБРАТЬ Продажи.Регистратор КАК Регистратор,
        |              СУММА(Продажи.Сумма) КАК Сумма
        |       ИЗ РегистрНакопления.Продажи КАК Продажи
        |       СГРУППИРОВАТЬ ПО Продажи.Регистратор
        |   ) КАК Вложенный
        |) КАК Итоги
        |ПО Заказы.Ссылка = Итоги.Регистратор";
КонецПроцедуры"#,
            DiagnosticCode::JoinWithSubQuery,
            expect![[r#"
                JoinWithSubQuery @ 7:13..13:28
                  message: Не используйте соединение с подзапросами. Соединения с подзапросами вызывают серьезные проблемы с производительностью
                  severity: Warning"#]],
        );
    }

    #[test]
    fn join_with_having_only_aggregation_emits() {
        check_diagnostics_snapshot_for(
            r#"Процедура Тест()
    Запрос = Новый Запрос;
    Запрос.Текст =
        "ВЫБРАТЬ Заказы.Ссылка КАК Ссылка
        |ИЗ Документ.ЗаказПокупателя КАК Заказы
        |ЛЕВОЕ СОЕДИНЕНИЕ (
        |   ВЫБРАТЬ Продажи.Регистратор КАК Регистратор
        |   ИЗ РегистрНакопления.Продажи КАК Продажи
        |   ИМЕЮЩИЕ КОЛИЧЕСТВО(Продажи.Регистратор) > 0
        |) КАК Итоги
        |ПО Заказы.Ссылка = Итоги.Регистратор";
КонецПроцедуры"#,
            DiagnosticCode::JoinWithSubQuery,
            expect![[r#"
                JoinWithSubQuery @ 7:13..9:56
                  message: Не используйте соединение с подзапросами. Соединения с подзапросами вызывают серьезные проблемы с производительностью
                  severity: Warning"#]],
        );
    }

    #[test]
    fn join_with_totals_subquery_emits() {
        check_diagnostics_snapshot_for(
            r#"Процедура Тест()
    Запрос = Новый Запрос;
    Запрос.Текст =
        "ВЫБРАТЬ Заказы.Ссылка КАК Ссылка
        |ИЗ Документ.ЗаказПокупателя КАК Заказы
        |ЛЕВОЕ СОЕДИНЕНИЕ (
        |   ВЫБРАТЬ Продажи.Регистратор КАК Регистратор,
        |          Продажи.Сумма КАК Сумма
        |   ИЗ РегистрНакопления.Продажи КАК Продажи
        |   ИТОГИ СУММА(Сумма) ПО Регистратор
        |) КАК Итоги
        |ПО Заказы.Ссылка = Итоги.Регистратор";
КонецПроцедуры"#,
            DiagnosticCode::JoinWithSubQuery,
            expect![[r#"
                JoinWithSubQuery @ 7:13..9:53
                  message: Не используйте соединение с подзапросами. Соединения с подзапросами вызывают серьезные проблемы с производительностью
                  severity: Warning"#]],
        );
    }
}
