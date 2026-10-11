use crate::define_metadata;
use crate::metadata::*;
use crate::{Diagnostic, DiagnosticCode, DiagnosticsConfig, DiagnosticsContext};
use sdbl_hir;

pub const METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: DiagnosticType::Error,
    severity: DiagnosticSeverityLevel::Major,
    scope: DiagnosticScope::Bsl,
    modules: &[],
    minutes_to_fix: 5,
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
    if let sdbl_hir::SdblDiagnostic::VirtualTableCallWithoutParameters { range, .. } = diag {
        crate::sdbl_utils::dispatch_simple(
            config,
            DiagnosticCode::VirtualTableCallWithoutParameters,
            "Не следует использовать виртуальные таблицы без параметров",
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
        DiagnosticCode::VirtualTableCallWithoutParameters,
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
            DiagnosticCode::VirtualTableCallWithoutParameters,
            expected,
        );
    }

    #[test]
    fn test_detects_virtual_table_calls_without_parameters() {
        check(
            r#"Функция ЦеныБезОтбора()
    Цены = Новый Запрос;
    Цены.Текст = "ВЫБРАТЬ Прайс.Цена КАК Цена
    |ИЗ РегистрСведений.Прайс.СрезПервых КАК Прайс";
    Возврат Цены;
КонецФункции

Функция ЗапасыСклада()
    Запасы = Новый Запрос;
    Запасы.Текст = "ВЫБРАТЬ Товары.Ссылка КАК Товар
    |ИЗ Справочник.Товары КАК Товары
    |ВНУТРЕННЕЕ СОЕДИНЕНИЕ РегистрНакопления.Запасы.Остатки(, Склад = &Склад) КАК Запасы
    |ПО Товары.Ссылка = Запасы.Товар";
    Возврат Запасы;
КонецФункции

Функция ПродажиЗаПериод()
    Продажи = Новый Запрос;
    Продажи.Текст = "ВЫБРАТЬ Обороты.Товар КАК Товар
    |ИЗ РегистрНакопления.Продажи.Обороты(&Начало, &Конец) КАК Обороты
    |ЛЕВОЕ СОЕДИНЕНИЕ РегистрСведений.Прайс.СрезПервых(&Начало) КАК Прайс
    |ПО Обороты.Товар = Прайс.Товар";
    Возврат Продажи;
КонецФункции

Функция ВсеОбороты()
    Продажи = Новый Запрос;
    Продажи.Текст = "ВЫБРАТЬ Обороты.Товар КАК Товар
    |ИЗ РегистрНакопления.Продажи.Обороты() КАК Обороты";
    Возврат Продажи;
КонецФункции

Функция ВсеЗапасы()
    Запасы = Новый Запрос;
    Запасы.Текст = "ВЫБРАТЬ Запасы.Товар КАК Товар
    |ИЗ РегистрНакопления.Запасы.Остатки( , ) КАК Запасы";
    Возврат Запасы;
КонецФункции

Функция ЗапасыНаДату()
    Запасы = Новый Запрос;
    Запасы.Текст = "ВЫБРАТЬ Запасы.Товар КАК Товар
    |ИЗ РегистрНакопления.Запасы.Остатки(&Дата, ) КАК Запасы";
    Возврат Запасы;
КонецФункции
"#,
            expect![[r#"
                VirtualTableCallWithoutParameters @ 4:9..4:41
                  message: Не следует использовать виртуальные таблицы без параметров
                  severity: Major
                VirtualTableCallWithoutParameters @ 29:9..29:44
                  message: Не следует использовать виртуальные таблицы без параметров
                  severity: Major
                VirtualTableCallWithoutParameters @ 36:9..36:46
                  message: Не следует использовать виртуальные таблицы без параметров
                  severity: Major"#]],
        );
    }

    #[test]
    fn test_virtual_table_with_params_ok() {
        check(
            r#"
Процедура Запасы()
    Текст = "ВЫБРАТЬ * ИЗ РегистрНакопления.Запасы.Остатки(Склад = &Склад)";
КонецПроцедуры
"#,
            expect![[r#""#]],
        );
    }

    #[test]
    fn test_virtual_table_period_only_ok() {
        check(
            r#"
Процедура Цены()
    Текст = "ВЫБРАТЬ * ИЗ РегистрСведений.Прайс.СрезПервых(&Начало)";
КонецПроцедуры
"#,
            expect![[r#""#]],
        );
    }

    #[test]
    fn test_virtual_table_empty_period_with_condition_ok() {
        check(
            r#"
Процедура Запасы()
    Текст = "ВЫБРАТЬ * ИЗ РегистрНакопления.Запасы.Остатки(, Товар В (&Товары))";
КонецПроцедуры
"#,
            expect![[r#""#]],
        );
    }

    #[test]
    fn test_virtual_table_without_parens() {
        check(
            r#"
Процедура Обороты()
    Текст = "ВЫБРАТЬ * ИЗ РегистрНакопления.Продажи.Обороты";
КонецПроцедуры
"#,
            expect![[r#"
                VirtualTableCallWithoutParameters @ 3:27..3:60
                  message: Не следует использовать виртуальные таблицы без параметров
                  severity: Major"#]],
        );
    }

    #[test]
    fn test_virtual_table_empty_parens() {
        check(
            r#"
Процедура Цены()
    Текст = "ВЫБРАТЬ * ИЗ РегистрСведений.Прайс.СрезПервых()";
КонецПроцедуры
"#,
            expect![[r#"
                VirtualTableCallWithoutParameters @ 3:27..3:61
                  message: Не следует использовать виртуальные таблицы без параметров
                  severity: Major"#]],
        );
    }

    #[test]
    fn test_virtual_table_period_with_trailing_empty_param_ok() {
        check(
            r#"
Процедура Цены()
    Текст = "ВЫБРАТЬ * ИЗ РегистрСведений.Прайс.СрезПервых(&Начало, )";
КонецПроцедуры
"#,
            expect![[r#""#]],
        );
    }

    #[test]
    fn test_virtual_table_both_empty() {
        check(
            r#"
Процедура Цены()
    Текст = "ВЫБРАТЬ * ИЗ РегистрСведений.Прайс.СрезПервых(, )";
КонецПроцедуры
"#,
            expect![[r#"
                VirtualTableCallWithoutParameters @ 3:27..3:63
                  message: Не следует использовать виртуальные таблицы без параметров
                  severity: Major"#]],
        );
    }
}
