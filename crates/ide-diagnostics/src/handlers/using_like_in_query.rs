use crate::define_metadata;
use crate::metadata::*;
use crate::{Diagnostic, DiagnosticCode, DiagnosticsConfig, DiagnosticsContext};

pub const METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: DiagnosticType::Error,
    severity: DiagnosticSeverityLevel::Major,
    scope: DiagnosticScope::Bsl,
    modules: &[],
    minutes_to_fix: 10,
    activated_by_default: false,
    compatibility_mode: DiagnosticCompatibilityMode::Undefined,
    tags: &[MetadataTag::Sql, MetadataTag::Unpredictable],
    can_locate_on_project: false,
    extra_min_for_complexity: 0.0,
    lsp_severity_override: "",
    clean_code_attribute: CleanCodeAttribute::Adaptable,
};

pub(crate) fn dispatch(
    config: &DiagnosticsConfig,
    diag: &sdbl_hir::SdblDiagnostic,
    mapper: &crate::sdbl_utils::SdblPositionMapper,
    query_text: &str,
    diagnostics: &mut Vec<Diagnostic>,
) {
    if let sdbl_hir::SdblDiagnostic::LikeUsage { range, .. } = diag {
        crate::sdbl_utils::dispatch_simple(
            config,
            DiagnosticCode::UsingLikeInQuery,
            "Измените выражение, чтобы не использовать 'ПОДОБНО'",
            *range,
            mapper,
            query_text,
            diagnostics,
        );
    }
}

pub fn check(ctx: &DiagnosticsContext) -> Vec<Diagnostic> {
    crate::sdbl_utils::collect_sdbl_via_dispatch(ctx, DiagnosticCode::UsingLikeInQuery, dispatch)
}

#[cfg(test)]
mod tests {
    use crate::test_utils::check_diagnostics_snapshot_for;
    use crate::DiagnosticCode;
    use expect_test::expect;

    #[test]
    fn test_detects_like_usages_in_query_fixture() {
        let code = r#"Функция НайтиКонтрагентов(Маска) Экспорт
    Запрос = Новый Запрос;
    Запрос.Текст =
    "ВЫБРАТЬ
    |   Партнёры.ИНН КАК ИНН,
    |   Партнёры.Наименование ПОДОБНО ""ООО%"" КАК Общество,
    |   Партнёры.Наименование ПОДОБНО &Маска КАК ПоМаске,
    |   Партнёры.Наименование ПОДОБНО Партнёры.КраткоеИмя КАК ПоКраткому,
    |   &Маска ПОДОБНО (Партнёры.Код) КАК ОбратнаяМаска,
    |   &Маска ПОДОБНО &ЗапаснаяМаска КАК ДвеМаски,
    |   Партнёры.Код ПОДОБНО ПОДСТРОКА(Партнёры.ИНН, 1, 4) КАК ПоРегиону
    |ИЗ
    |   Справочник.Контрагенты КАК Партнёры
    |   ВНУТРЕННЕЕ СОЕДИНЕНИЕ (
    |       ВЫБРАТЬ
    |           Договоры.Владелец КАК Владелец,
    |           Договоры.Номер ПОДОБНО ""Д-%"" КАК Типовой
    |       ИЗ
    |           Справочник.Договоры КАК Договоры
    |       ГДЕ
    |           Договоры.Номер ПОДОБНО &Маска) КАК Соглашения
    |   ПО Партнёры.Ссылка = Соглашения.Владелец
    |       И Партнёры.Регион ПОДОБНО Соглашения.Владелец.Регион
    |ГДЕ
    |   Партнёры.Наименование ПОДОБНО &Маска
    |   ИЛИ ""%"" + Партнёры.Код ПОДОБНО &Маска";
    Возврат Запрос.Выполнить().Выгрузить();
КонецФункции
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::UsingLikeInQuery,
            expect![[r#"
            UsingLikeInQuery @ 6:9..6:47
              message: Измените выражение, чтобы не использовать 'ПОДОБНО'
              severity: Major
            UsingLikeInQuery @ 7:9..7:45
              message: Измените выражение, чтобы не использовать 'ПОДОБНО'
              severity: Major
            UsingLikeInQuery @ 8:9..8:58
              message: Измените выражение, чтобы не использовать 'ПОДОБНО'
              severity: Major
            UsingLikeInQuery @ 9:9..9:38
              message: Измените выражение, чтобы не использовать 'ПОДОБНО'
              severity: Major
            UsingLikeInQuery @ 10:9..10:38
              message: Измените выражение, чтобы не использовать 'ПОДОБНО'
              severity: Major
            UsingLikeInQuery @ 11:9..11:59
              message: Измените выражение, чтобы не использовать 'ПОДОБНО'
              severity: Major
            UsingLikeInQuery @ 17:17..17:47
              message: Измените выражение, чтобы не использовать 'ПОДОБНО'
              severity: Major
            UsingLikeInQuery @ 21:17..21:46
              message: Измените выражение, чтобы не использовать 'ПОДОБНО'
              severity: Major
            UsingLikeInQuery @ 23:15..23:65
              message: Измените выражение, чтобы не использовать 'ПОДОБНО'
              severity: Major
            UsingLikeInQuery @ 25:9..25:45
              message: Измените выражение, чтобы не использовать 'ПОДОБНО'
              severity: Major
            UsingLikeInQuery @ 26:13..26:48
              message: Измените выражение, чтобы не использовать 'ПОДОБНО'
              severity: Major"#]],
        );
    }

    #[test]
    fn test_simple_like_english() {
        let code = r#"
Procedure FindItems()
    Text = "SELECT Items.Sku LIKE ""A-%"" AS Marked FROM Catalog.Items AS Items";
EndProcedure
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::UsingLikeInQuery,
            expect![[r#"
            UsingLikeInQuery @ 3:20..3:42
              message: Измените выражение, чтобы не использовать 'ПОДОБНО'
              severity: Major"#]],
        );
    }

    #[test]
    fn test_simple_like_russian() {
        let code = r#"
Процедура ОтметитьЧерновики()
    Текст = "ВЫБРАТЬ Заметки.Заголовок ПОДОБНО ""Черновик%"" КАК Черновик ИЗ Справочник.Заметки КАК Заметки";
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::UsingLikeInQuery,
            expect![[r#"
            UsingLikeInQuery @ 3:22..3:61
              message: Измените выражение, чтобы не использовать 'ПОДОБНО'
              severity: Major"#]],
        );
    }

    #[test]
    fn test_not_like() {
        let code = r#"
Процедура ОтметитьЧистовики()
    Текст = "ВЫБРАТЬ Заметки.Заголовок НЕ ПОДОБНО ""Черновик%"" КАК Чистовик ИЗ Справочник.Заметки КАК Заметки";
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::UsingLikeInQuery,
            expect![[r#"
            UsingLikeInQuery @ 3:22..3:64
              message: Измените выражение, чтобы не использовать 'ПОДОБНО'
              severity: Major"#]],
        );
    }

    #[test]
    fn test_like_in_where() {
        let code = r#"
Процедура ОтобратьЗвонки()
    Текст = "ВЫБРАТЬ Звонки.Номер КАК Номер ИЗ Документ.Звонок КАК Звонки ГДЕ Звонки.Номер ПОДОБНО ""+7%""";
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::UsingLikeInQuery,
            expect![[r#"
            UsingLikeInQuery @ 3:79..3:107
              message: Измените выражение, чтобы не использовать 'ПОДОБНО'
              severity: Major"#]],
        );
    }

    #[test]
    fn test_like_in_join() {
        let code = r#"
Процедура СопоставитьАдреса()
    Текст = "ВЫБРАТЬ Склады.Ссылка КАК Склад ИЗ Справочник.Склады КАК Склады ЛЕВОЕ СОЕДИНЕНИЕ Справочник.Адреса КАК Адреса ПО Склады.Адрес ПОДОБНО Адреса.Шаблон";
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::UsingLikeInQuery,
            expect![[r#"
            UsingLikeInQuery @ 3:127..3:161
              message: Измените выражение, чтобы не использовать 'ПОДОБНО'
              severity: Major"#]],
        );
    }

    #[test]
    fn test_multiple_likes() {
        let code = r#"
Процедура РазобратьАртикулы()
    Текст = "ВЫБРАТЬ
            |   Изделия.Артикул ПОДОБНО ""X%"" КАК Импорт,
            |   Изделия.Артикул ПОДОБНО ""Z%"" КАК Уценка
            |ИЗ Справочник.Изделия КАК Изделия
            |ГДЕ Изделия.Серия ПОДОБНО ""2026%""";
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::UsingLikeInQuery,
            expect![[r#"
            UsingLikeInQuery @ 4:17..4:47
              message: Измените выражение, чтобы не использовать 'ПОДОБНО'
              severity: Major
            UsingLikeInQuery @ 5:17..5:47
              message: Измените выражение, чтобы не использовать 'ПОДОБНО'
              severity: Major
            UsingLikeInQuery @ 7:18..7:49
              message: Измените выражение, чтобы не использовать 'ПОДОБНО'
              severity: Major"#]],
        );
    }
}
