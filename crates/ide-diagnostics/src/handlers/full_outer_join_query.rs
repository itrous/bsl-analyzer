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
    if let sdbl_hir::SdblDiagnostic::FullOuterJoin { range } = diag {
        crate::sdbl_utils::dispatch_simple(config, DiagnosticCode::FullOuterJoinQuery, "Использование FULL OUTER JOIN значительно снижает производительность запроса. Рассмотрите возможность переписать с использованием UNION и LEFT JOIN", *range, mapper, query_text, diagnostics);
    }
}

pub fn check(ctx: &DiagnosticsContext) -> Vec<Diagnostic> {
    crate::sdbl_utils::collect_sdbl_via_dispatch(ctx, DiagnosticCode::FullOuterJoinQuery, dispatch)
}

#[cfg(test)]
mod tests {
    use crate::test_utils::check_diagnostics_snapshot_for;
    use crate::DiagnosticCode;
    use expect_test::expect;

    #[test]
    fn test_fixture_full_outer_join_detected_left_join_not() {
        let nested_full = r#"Функция СверкаРейсов()
    Сверка = Новый Запрос;
    Сверка.Текст = "ВЫБРАТЬ
                   |    Рейсы.Номер КАК Рейс,
                   |    ЕСТЬNULL(Погрузка.Вес, 0) КАК Погружено,
                   |    ЕСТЬNULL(Выгрузка.Вес, 0) КАК Выгружено
                   |ИЗ
                   |    Документ.Рейс КАК Рейсы
                   |        ЛЕВОЕ СОЕДИНЕНИЕ РегистрНакопления.Погрузка КАК Погрузка
                   |            ПОЛНОЕ ВНЕШНЕЕ СОЕДИНЕНИЕ РегистрНакопления.Выгрузка КАК Выгрузка
                   |            ПО Погрузка.Рейс = Выгрузка.Рейс
                   |        ПО Рейсы.Ссылка = Погрузка.Рейс";
    Возврат Сверка.Выполнить();
КонецФункции"#;
        check_diagnostics_snapshot_for(
            nested_full,
            DiagnosticCode::FullOuterJoinQuery,
            expect![[r#"
                FullOuterJoinQuery @ 10:33..11:65
                  message: Использование FULL OUTER JOIN значительно снижает производительность запроса. Рассмотрите возможность переписать с использованием UNION и LEFT JOIN
                  severity: Warning"#]],
        );

        let nested_left = r#"Функция СверкаРейсов()
    Сверка = Новый Запрос;
    Сверка.Текст = "ВЫБРАТЬ
                   |    Рейсы.Номер КАК Рейс
                   |ИЗ
                   |    Документ.Рейс КАК Рейсы
                   |        ЛЕВОЕ СОЕДИНЕНИЕ РегистрНакопления.Погрузка КАК Погрузка
                   |            ЛЕВОЕ СОЕДИНЕНИЕ РегистрНакопления.Выгрузка КАК Выгрузка
                   |            ПО Погрузка.Рейс = Выгрузка.Рейс
                   |        ПО Рейсы.Ссылка = Погрузка.Рейс";
    Возврат Сверка.Выполнить();
КонецФункции"#;
        check_diagnostics_snapshot_for(
            nested_left,
            DiagnosticCode::FullOuterJoinQuery,
            expect![[r#""#]],
        );
    }

    #[test]
    fn test_simple_english() {
        let code = r#"
Procedure Reconcile()
    Text = "SELECT * FROM Shipments AS S FULL JOIN Invoices AS I ON S.Ref = I.Shipment";
EndProcedure
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::FullOuterJoinQuery,
            expect![[r#"
            FullOuterJoinQuery @ 3:42..3:87
              message: Использование FULL OUTER JOIN значительно снижает производительность запроса. Рассмотрите возможность переписать с использованием UNION и LEFT JOIN
              severity: Warning"#]],
        );
    }

    #[test]
    fn test_simple_russian() {
        let code = r#"
Процедура Сверить()
    Текст = "ВЫБРАТЬ * ИЗ Заявки КАК З ПОЛНОЕ СОЕДИНЕНИЕ Отгрузки КАК О ПО З.Номер = О.Заявка";
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::FullOuterJoinQuery,
            expect![[r#"
            FullOuterJoinQuery @ 3:40..3:94
              message: Использование FULL OUTER JOIN значительно снижает производительность запроса. Рассмотрите возможность переписать с использованием UNION и LEFT JOIN
              severity: Warning"#]],
        );
    }

    #[test]
    fn test_no_false_positives_left_join() {
        let code = r#"
Процедура Сверить()
    Сверка = Новый Запрос;
    Сверка.Текст = "ВЫБРАТЬ
                   |    Водители.ФИО
                   |ИЗ
                   |    Справочник.Водители КАК Водители
                   |        ЛЕВОЕ СОЕДИНЕНИЕ Путевки";
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(code, DiagnosticCode::FullOuterJoinQuery, expect![[r#""#]]);
    }

    #[test]
    fn test_full_join_without_outer() {
        let code = r#"
Процедура Сверить()
    Text = "SELECT * FROM Shipments ПОЛНОЕ СОЕДИНЕНИЕ Invoices ПО Shipments.Ref = Invoices.Shipment";
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::FullOuterJoinQuery,
            expect![[r#"
            FullOuterJoinQuery @ 3:37..3:100
              message: Использование FULL OUTER JOIN значительно снижает производительность запроса. Рассмотрите возможность переписать с использованием UNION и LEFT JOIN
              severity: Warning"#]],
        );
    }

    #[test]
    fn test_multiple_full_joins() {
        let code = r#"
Процедура Сверить()
    Text = "SELECT * FROM Trips FULL OUTER JOIN Fuel ON Trips.Car = Fuel.Car FULL OUTER JOIN Repairs ON Trips.Car = Repairs.Car";
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::FullOuterJoinQuery,
            expect![[r#"
            FullOuterJoinQuery @ 3:33..3:77
              message: Использование FULL OUTER JOIN значительно снижает производительность запроса. Рассмотрите возможность переписать с использованием UNION и LEFT JOIN
              severity: Warning
            FullOuterJoinQuery @ 3:78..3:128
              message: Использование FULL OUTER JOIN значительно снижает производительность запроса. Рассмотрите возможность переписать с использованием UNION и LEFT JOIN
              severity: Warning"#]],
        );
    }

    #[test]
    fn test_multiline_simple() {
        let code = r#"
Процедура Сверить()
    Текст = "ВЫБРАТЬ *
            |ИЗ Путевки
            |    ПОЛНОЕ СОЕДИНЕНИЕ Заправки
            |    ПО Путевки.Машина = Заправки.Машина";
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::FullOuterJoinQuery,
            expect![[r#"
            FullOuterJoinQuery @ 5:18..6:53
              message: Использование FULL OUTER JOIN значительно снижает производительность запроса. Рассмотрите возможность переписать с использованием UNION и LEFT JOIN
              severity: Warning"#]],
        );
    }

    #[test]
    fn test_multiline_with_comment() {
        let code = r#"
Процедура Сверить()
    Текст = "ВЫБРАТЬ *
            |ИЗ Путевки
            |    ПОЛНОЕ СОЕДИНЕНИЕ Заправки // все машины обеих таблиц
            |    ПО Путевки.Машина = Заправки.Машина";
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::FullOuterJoinQuery,
            expect![[r#"
            FullOuterJoinQuery @ 5:18..6:53
              message: Использование FULL OUTER JOIN значительно снижает производительность запроса. Рассмотрите возможность переписать с использованием UNION и LEFT JOIN
              severity: Warning"#]],
        );
    }

    #[test]
    fn test_nested_joins_like_fixture() {
        let code = r#"
Процедура Сверить()
    Сверка = Новый Запрос;
    Сверка.Текст = "ВЫБРАТЬ
                   |    Склады.Наименование
                   |ИЗ
                   |    Справочник.Склады КАК Склады
                   |        ВНУТРЕННЕЕ СОЕДИНЕНИЕ РегистрНакопления.Приход КАК Приход
                   |            ПОЛНОЕ СОЕДИНЕНИЕ РегистрНакопления.Расход КАК Расход
                   |            ПО Приход.Склад = Расход.Склад
                   |        ПО Склады.Ссылка = Приход.Склад";
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::FullOuterJoinQuery,
            expect![[r#"
            FullOuterJoinQuery @ 9:33..10:63
              message: Использование FULL OUTER JOIN значительно снижает производительность запроса. Рассмотрите возможность переписать с использованием UNION и LEFT JOIN
              severity: Warning"#]],
        );
    }

    #[test]
    fn test_with_function_calls_in_select() {
        let code = r#"
Процедура Сверить()
    Сверка = Новый Запрос;
    Сверка.Текст = "ВЫБРАТЬ
                   |    ЕСТЬNULL(Пробег.Машина, Топливо.Машина) КАК Машина,
                   |    СУММА(Топливо.Литры) КАК Литры
                   |ИЗ
                   |    РегистрНакопления.Пробег КАК Пробег
                   |        ПОЛНОЕ ВНЕШНЕЕ СОЕДИНЕНИЕ РегистрНакопления.Топливо КАК Топливо
                   |        ПО Пробег.Машина = Топливо.Машина
                   |СГРУППИРОВАТЬ ПО
                   |    ЕСТЬNULL(Пробег.Машина, Топливо.Машина)";
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::FullOuterJoinQuery,
            expect![[r#"
            FullOuterJoinQuery @ 9:29..10:62
              message: Использование FULL OUTER JOIN значительно снижает производительность запроса. Рассмотрите возможность переписать с использованием UNION и LEFT JOIN
              severity: Warning"#]],
        );
    }
}
