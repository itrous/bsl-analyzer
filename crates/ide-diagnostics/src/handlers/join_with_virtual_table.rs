use crate::define_metadata;
use crate::metadata::*;
use crate::{Diagnostic, DiagnosticCode, DiagnosticsConfig, DiagnosticsContext};
use sdbl_hir;

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
    if let sdbl_hir::SdblDiagnostic::JoinWithVirtualTable { range, .. } = diag {
        crate::sdbl_utils::dispatch_simple(
            config,
            DiagnosticCode::JoinWithVirtualTable,
            "Не следует использовать соединения с виртуальными таблицами",
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
        DiagnosticCode::JoinWithVirtualTable,
        dispatch,
    )
}

#[cfg(test)]
mod tests {
    use crate::test_utils::check_diagnostics_snapshot_for;
    use crate::DiagnosticCode;
    use expect_test::expect;

    fn check(code: &str, expected: expect_test::Expect) {
        check_diagnostics_snapshot_for(code, DiagnosticCode::JoinWithVirtualTable, expected);
    }

    #[test]
    fn test_join_with_virtual_table_single_line() {
        check(
            r#"Функция ДолгиКлиентов()
    Долги = Новый Запрос;
    Долги.Текст = "ВЫБРАТЬ Клиенты.Ссылка КАК Клиент ИЗ Справочник.Клиенты КАК Клиенты ЛЕВОЕ СОЕДИНЕНИЕ РегистрНакопления.Расчеты.Остатки КАК Расчеты ПО Клиенты.Ссылка = Расчеты.Клиент";
    Возврат Долги;
КонецФункции
"#,
            expect![[r#"
                JoinWithVirtualTable @ 3:105..3:138
                  message: Не следует использовать соединения с виртуальными таблицами
                  severity: Warning"#]],
        );
    }

    #[test]
    fn test_join_with_virtual_table_multiline_left() {
        check(
            r#"Функция ДолгиКлиентов()
    Долги = Новый Запрос;
    Долги.Текст = "ВЫБРАТЬ Клиенты.Ссылка КАК Клиент
    |ИЗ Справочник.Клиенты КАК Клиенты
    |ЛЕВОЕ СОЕДИНЕНИЕ
    |    РегистрНакопления.Расчеты.Обороты(&Начало, &Конец, , Договор.Валюта = &Валюта) КАК Обороты
    |ПО Клиенты.Ссылка = Обороты.Клиент";
    Возврат Долги;
КонецФункции
"#,
            expect![[r#"
                JoinWithVirtualTable @ 6:10..6:88
                  message: Не следует использовать соединения с виртуальными таблицами
                  severity: Warning"#]],
        );
    }

    #[test]
    fn test_join_with_virtual_table_multiline_right() {
        check(
            r#"Функция ДолгиКлиентов()
    Долги = Новый Запрос;
    Долги.Текст = "ВЫБРАТЬ Обороты.Клиент КАК Клиент
    |ИЗ Справочник.Клиенты КАК Клиенты
    |ПРАВОЕ СОЕДИНЕНИЕ
    |    РегистрНакопления.Расчеты.Обороты(&Начало, &Конец, , Договор.Валюта = &Валюта) КАК Обороты
    |ПО Клиенты.Ссылка = Обороты.Клиент";
    Возврат Долги;
КонецФункции
"#,
            expect![[r#"
                JoinWithVirtualTable @ 6:10..6:88
                  message: Не следует использовать соединения с виртуальными таблицами
                  severity: Warning"#]],
        );
    }

    #[test]
    fn test_join_with_two_virtual_tables() {
        check(
            r#"Функция ДолгиВВалюте()
    Долги = Новый Запрос;
    Долги.Текст = "ВЫБРАТЬ Курсы.Курс КАК Курс
    |ИЗ РегистрСведений.КурсыВалют.СрезПервых(&Начало) КАК Курсы
    |    ВНУТРЕННЕЕ СОЕДИНЕНИЕ РегистрНакопления.Расчеты.Остатки(&Конец) КАК Расчеты
    |    ПО Курсы.Валюта = Расчеты.Валюта";
    Возврат Долги;
КонецФункции
"#,
            expect![[r#"
                JoinWithVirtualTable @ 4:9..4:55
                  message: Не следует использовать соединения с виртуальными таблицами
                  severity: Warning
                JoinWithVirtualTable @ 5:32..5:73
                  message: Не следует использовать соединения с виртуальными таблицами
                  severity: Warning"#]],
        );
    }

    #[test]
    fn test_virtual_table_in_from_no_join_no_trigger() {
        check(
            r#"Функция ДолгиКлиентов()
    Долги = Новый Запрос;
    Долги.Текст = "ВЫБРАТЬ Расчеты.Клиент КАК Клиент
    |ИЗ РегистрНакопления.Расчеты.Остатки(&Конец) КАК Расчеты,
    |    (ВЫБРАТЬ Клиенты.Ссылка КАК Ссылка ИЗ Справочник.Клиенты КАК Клиенты ГДЕ Клиенты.Ссылка = &Клиент) КАК Отбор";
    Возврат Долги;
КонецФункции
"#,
            expect![[r#""#]],
        );
    }

    #[test]
    fn test_simple_join_with_virtual_table() {
        check(
            r#"
Процедура ПроверитьКурс()
    Текст = "ВЫБРАТЬ * ИЗ Платежи ВНУТРЕННЕЕ СОЕДИНЕНИЕ РегистрСведений.КурсыВалют.СрезПервых КАК Курс ПО Платежи.Валюта = Курс.Валюта";
КонецПроцедуры
"#,
            expect![[r#"
                JoinWithVirtualTable @ 3:57..3:94
                  message: Не следует использовать соединения с виртуальными таблицами
                  severity: Warning"#]],
        );
    }

    #[test]
    fn test_no_false_positive_regular_table() {
        check(
            r#"
Процедура ПроверитьКурс()
    Текст = "ВЫБРАТЬ * ИЗ Справочник.Валюты КАК В ЛЕВОЕ СОЕДИНЕНИЕ РегистрСведений.КурсыВалют КАК К ПО В.Ссылка = К.Валюта";
КонецПроцедуры
"#,
            expect![[r#""#]],
        );
    }

    #[test]
    fn test_no_false_positive_virtual_table_without_join() {
        check(
            r#"
Процедура ПроверитьКурс()
    Текст = "ВЫБРАТЬ * ИЗ РегистрСведений.КурсыВалют.СрезПервых(&Начало, Валюта = &Валюта) КАК К";
КонецПроцедуры
"#,
            expect![[r#""#]],
        );
    }

    #[test]
    fn test_virtual_table_in_from_with_join() {
        check(
            r#"
Процедура ПроверитьКурс()
    Текст = "ВЫБРАТЬ * ИЗ РегистрНакопления.Расчеты.Остатки(&Конец) КАК Р ЛЕВОЕ СОЕДИНЕНИЕ Справочник.Клиенты КАК К ПО Р.Клиент = К.Ссылка";
КонецПроцедуры
"#,
            expect![[r#"
                JoinWithVirtualTable @ 3:27..3:68
                  message: Не следует использовать соединения с виртуальными таблицами
                  severity: Warning"#]],
        );
    }

    #[test]
    fn test_multiple_virtual_tables_in_joins() {
        check(
            r#"
Процедура ПроверитьКурс()
    Текст = "ВЫБРАТЬ *
    |ИЗ Справочник.Клиенты КАК К
    |ЛЕВОЕ СОЕДИНЕНИЕ РегистрНакопления.Расчеты.Обороты КАК О ПО К.Ссылка = О.Клиент
    |ЛЕВОЕ СОЕДИНЕНИЕ РегистрСведений.КурсыВалют.СрезПервых КАК Курсы ПО О.Валюта = Курсы.Валюта";
КонецПроцедуры
"#,
            expect![[r#"
                JoinWithVirtualTable @ 5:23..5:56
                  message: Не следует использовать соединения с виртуальными таблицами
                  severity: Warning
                JoinWithVirtualTable @ 6:23..6:60
                  message: Не следует использовать соединения с виртуальными таблицами
                  severity: Warning"#]],
        );
    }
}
