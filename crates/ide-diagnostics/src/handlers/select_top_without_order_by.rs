use crate::define_metadata;
use crate::metadata::*;
use crate::{Diagnostic, DiagnosticCode, DiagnosticsConfig, DiagnosticsContext};
use sdbl_hir;

pub const METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: DiagnosticType::CodeSmell,
    severity: DiagnosticSeverityLevel::Major,
    scope: DiagnosticScope::Bsl,
    modules: &[],
    minutes_to_fix: 5,
    activated_by_default: true,
    compatibility_mode: DiagnosticCompatibilityMode::Undefined,
    tags: &[MetadataTag::Standard, MetadataTag::Sql, MetadataTag::Suspicious],
    can_locate_on_project: false,
    extra_min_for_complexity: 0.0,
    lsp_severity_override: "",
    clean_code_attribute: CleanCodeAttribute::Intentional,
};

const DEFAULT_SKIP_SELECT_TOP_ONE: bool = true;

pub(crate) fn dispatch(
    config: &DiagnosticsConfig,
    diag: &sdbl_hir::SdblDiagnostic,
    mapper: &crate::sdbl_utils::SdblPositionMapper,
    query_text: &str,
    diagnostics: &mut Vec<Diagnostic>,
) {
    if let sdbl_hir::SdblDiagnostic::SelectTopWithoutOrderBy {
        top_value,
        in_union,
        has_where,
        range,
    } = diag
    {
        let skip_select_top_one = config
            .get_bool(DiagnosticCode::SelectTopWithoutOrderBy, "skipSelectTopOne")
            .unwrap_or(DEFAULT_SKIP_SELECT_TOP_ONE);

        let should_report = if *in_union {
            true
        } else if *top_value == 1 || *top_value == 0 {
            !skip_select_top_one && !*has_where
        } else {
            true
        };

        if should_report {
            let code = DiagnosticCode::SelectTopWithoutOrderBy;
            diagnostics.push(Diagnostic {
                code,
                message: "Измените запрос, добавив сортировку".to_string(),
                severity: config.severity(code),
                range: mapper.map_range(*range, query_text),
                tags: config.tags(code),
                fixes: vec![],
            });
        }
    }
}

pub fn check(ctx: &DiagnosticsContext) -> Vec<Diagnostic> {
    crate::sdbl_utils::collect_sdbl_via_dispatch(
        ctx,
        DiagnosticCode::SelectTopWithoutOrderBy,
        dispatch,
    )
}

#[cfg(test)]
mod tests {
    use crate::test_utils::check_diagnostics_snapshot_for;
    use crate::DiagnosticCode;
    use expect_test::expect;

    fn check_snapshot(code: &str, expected: expect_test::Expect) {
        check_diagnostics_snapshot_for(code, DiagnosticCode::SelectTopWithoutOrderBy, expected);
    }
    #[test]
    fn test_top_10_in_batch_order_by_in_other_query() {
        let code = r#"
Процедура СвежиеПоступления()
    Текст = "ВЫБРАТЬ ПЕРВЫЕ 15
            |   Книги.Ссылка КАК Книга
            |ИЗ
            |   Справочник.Книги КАК Книги
            |;
            |// второй запрос пакета
            |ВЫБРАТЬ
            |   Читатели.Ссылка КАК Читатель
            |ИЗ
            |   Справочник.Читатели КАК Читатели
            |УПОРЯДОЧИТЬ ПО
            |   Читатели.ДатаЗаписи";
КонецПроцедуры
"#;
        check_snapshot(
            code,
            expect![[r#"
            SelectTopWithoutOrderBy @ 3:22..3:31
              message: Измените запрос, добавив сортировку
              severity: Warning"#]],
        );
    }

    #[test]
    fn test_top_10_in_where_in_subquery() {
        let code = r#"
Процедура ДолжникиПоКнигам()
    Текст = "ВЫБРАТЬ
            |   Выдачи.Читатель КАК Читатель
            |ИЗ
            |   Документ.Выдача КАК Выдачи
            |ГДЕ
            |   Выдачи.Книга В (
            |       ВЫБРАТЬ ПЕРВЫЕ 20
            |           Книги.Ссылка
            |       ИЗ
            |           Справочник.Книги КАК Книги)";
КонецПроцедуры
"#;
        check_snapshot(
            code,
            expect![[r#"
            SelectTopWithoutOrderBy @ 9:29..9:38
              message: Измените запрос, добавив сортировку
              severity: Warning"#]],
        );
    }

    #[test]
    fn test_top_1_in_where_in_subquery_skipped_by_default() {
        let code = r#"
Процедура ДолжникиПоКнигам()
    Текст = "ВЫБРАТЬ
            |   Выдачи.Читатель КАК Читатель
            |ИЗ
            |   Документ.Выдача КАК Выдачи
            |ГДЕ
            |   Выдачи.Книга В (
            |       ВЫБРАТЬ ПЕРВЫЕ 1
            |           Книги.Ссылка
            |       ИЗ
            |           Справочник.Книги КАК Книги)";
КонецПроцедуры
"#;
        check_snapshot(code, expect![[r#""#]]);
    }

    #[test]
    fn test_top_10_in_nested_from_subquery() {
        let code = r#"
Процедура ПопулярныеАвторы()
    Текст = "ВЫБРАТЬ ПЕРВЫЕ 3
            |   Отбор.Автор КАК Автор
            |ИЗ
            |   (ВЫБРАТЬ ПЕРВЫЕ 50
            |       Книги.Автор КАК Автор
            |   ИЗ
            |       Справочник.Книги КАК Книги) КАК Отбор
            |УПОРЯДОЧИТЬ ПО
            |   Автор";
КонецПроцедуры
"#;
        check_snapshot(
            code,
            expect![[r#"
            SelectTopWithoutOrderBy @ 6:26..6:35
              message: Измените запрос, добавив сортировку
              severity: Warning"#]],
        );
    }

    #[test]
    fn test_top_1_in_nested_from_subquery_skipped_by_default() {
        let code = r#"
Процедура ПопулярныеАвторы()
    Текст = "ВЫБРАТЬ ПЕРВЫЕ 3
            |   Отбор.Автор КАК Автор
            |ИЗ
            |   (ВЫБРАТЬ ПЕРВЫЕ 1
            |       Книги.Автор КАК Автор
            |   ИЗ
            |       Справочник.Книги КАК Книги) КАК Отбор
            |УПОРЯДОЧИТЬ ПО
            |   Автор";
КонецПроцедуры
"#;
        check_snapshot(code, expect![[r#""#]]);
    }

    #[test]
    fn test_top_10_in_where_in_subquery_order_by_only_in_outer() {
        let code = r#"
Процедура ДолжникиПоКнигам()
    Текст = "ВЫБРАТЬ
            |   Выдачи.Читатель КАК Читатель
            |ИЗ
            |   Документ.Выдача КАК Выдачи
            |ГДЕ
            |   Выдачи.Книга В (
            |       ВЫБРАТЬ ПЕРВЫЕ 20
            |           Книги.Ссылка
            |       ИЗ
            |           Справочник.Книги КАК Книги)
            |УПОРЯДОЧИТЬ ПО
            |   Выдачи.Дата УБЫВ";
КонецПроцедуры
"#;
        check_snapshot(
            code,
            expect![[r#"
            SelectTopWithoutOrderBy @ 9:29..9:38
              message: Измените запрос, добавив сортировку
              severity: Warning"#]],
        );
    }

    #[test]
    fn test_union_with_where_subquery_and_union_members() {
        let code = r#"
Процедура СписокНаПолку()
    Текст = "ВЫБРАТЬ
            |   Выдачи.Книга КАК Книга
            |ИЗ
            |   Документ.Выдача КАК Выдачи
            |ГДЕ
            |   Выдачи.Читатель В (
            |       ВЫБРАТЬ ПЕРВЫЕ 5
            |           Читатели.Ссылка
            |       ИЗ
            |           Справочник.Читатели КАК Читатели)
            |
            |ОБЪЕДИНИТЬ ВСЕ
            |
            |ВЫБРАТЬ ПЕРВЫЕ 5
            |   Новинки.Книга
            |ИЗ
            |   Документ.Поступление КАК Новинки
            |
            |ОБЪЕДИНИТЬ ВСЕ
            |
            |ВЫБРАТЬ ПЕРВЫЕ 1
            |   Заказы.Книга
            |ИЗ
            |   Документ.ЗаказКниги КАК Заказы
            |УПОРЯДОЧИТЬ ПО
            |   Книга";
КонецПроцедуры
"#;
        check_snapshot(
            code,
            expect![[r#"
            SelectTopWithoutOrderBy @ 9:29..9:37
              message: Измените запрос, добавив сортировку
              severity: Warning
            SelectTopWithoutOrderBy @ 16:22..16:30
              message: Измените запрос, добавив сортировку
              severity: Warning
            SelectTopWithoutOrderBy @ 23:22..23:30
              message: Измените запрос, добавив сортировку
              severity: Warning"#]],
        );
    }

    #[test]
    fn test_complex_union_with_nested_subqueries() {
        let code = r#"
Процедура СписокНаПолку()
    Текст = "ВЫБРАТЬ ПЕРВЫЕ 1
            |   Сводка.Книга КАК Книга
            |ИЗ
            |   (ВЫБРАТЬ
            |       Выдачи.Книга КАК Книга
            |   ИЗ
            |       Документ.Выдача КАК Выдачи
            |   ГДЕ
            |       Выдачи.Читатель В (
            |           ВЫБРАТЬ ПЕРВЫЕ 5
            |               Читатели.Ссылка
            |           ИЗ
            |               Справочник.Читатели КАК Читатели)
            |
            |   ОБЪЕДИНИТЬ ВСЕ
            |
            |   ВЫБРАТЬ ПЕРВЫЕ 5
            |       Новинки.Книга
            |   ИЗ
            |       Документ.Поступление КАК Новинки) КАК Сводка
            |
            |УПОРЯДОЧИТЬ ПО
            |   Книга";
КонецПроцедуры
"#;
        check_snapshot(
            code,
            expect![[r#"
            SelectTopWithoutOrderBy @ 12:33..12:41
              message: Измените запрос, добавив сортировку
              severity: Warning
            SelectTopWithoutOrderBy @ 19:25..19:33
              message: Измените запрос, добавив сортировку
              severity: Warning"#]],
        );
    }

    #[test]
    fn test_parameter_substitution_in_union_no_top() {
        let code = r#"
Процедура ФондПоЗалам()
    Текст = "ВЫБРАТЬ
            |   Фонд.Книга КАК Книга
            |ПОМЕСТИТЬ ВТ_Фонд
            |ИЗ
            |   (ВЫБРАТЬ
            |       Наличие.Книга КАК Книга
            |   ИЗ
            |       РегистрНакопления.КнижныйФонд.Остатки() КАК Наличие
            |
            |   ОБЪЕДИНИТЬ ВСЕ
            |
            |   ВЫБРАТЬ &ТекстВыданныхКниг) КАК Фонд";
КонецПроцедуры
"#;
        check_snapshot(code, expect![[r#""#]]);
    }

    #[test]
    fn test_top_0_with_order_by() {
        let code = r#"
Процедура ПустаяВыборка()
    Текст = "ВЫБРАТЬ ПЕРВЫЕ 0
            |   0 КАК Количество
            |УПОРЯДОЧИТЬ ПО
            |   Количество";
КонецПроцедуры
"#;
        check_snapshot(code, expect![[r#""#]]);
    }

    #[test]
    fn test_simple_top_without_order_by_russian() {
        let code = r#"
Процедура Витрина()
    Текст = "ВЫБРАТЬ ПЕРВЫЕ 12 * ИЗ Справочник.Книги";
КонецПроцедуры
"#;
        check_snapshot(
            code,
            expect![[r#"
            SelectTopWithoutOrderBy @ 3:22..3:31
              message: Измените запрос, добавив сортировку
              severity: Warning"#]],
        );
    }

    #[test]
    fn test_simple_top_with_order_by() {
        let code = r#"
Процедура Витрина()
    Текст = "ВЫБРАТЬ ПЕРВЫЕ 12 * ИЗ Справочник.Книги УПОРЯДОЧИТЬ ПО ГодИздания УБЫВ";
КонецПроцедуры
"#;
        check_snapshot(code, expect![[r#""#]]);
    }

    #[test]
    fn test_top_1_with_where_clause() {
        let code = r#"
Процедура НайтиКнигу()
    Текст = "ВЫБРАТЬ ПЕРВЫЕ 1 * ИЗ Справочник.Книги ГДЕ ISBN = &ISBN";
КонецПроцедуры
"#;
        check_snapshot(code, expect![[r#""#]]);
    }

    #[test]
    fn test_top_1_without_where_skipped_by_default() {
        let code = r#"
Процедура ЛюбаяКнига()
    Текст = "ВЫБРАТЬ ПЕРВЫЕ 1 * ИЗ Справочник.Книги";
КонецПроцедуры
"#;
        check_snapshot(code, expect![[r#""#]]);
    }

    #[test]
    fn test_top_in_union() {
        let code = r#"
Процедура Витрина()
    Текст = "ВЫБРАТЬ ПЕРВЫЕ 4 * ИЗ Новинки
            |ОБЪЕДИНИТЬ ВСЕ
            |ВЫБРАТЬ * ИЗ Бестселлеры";
КонецПроцедуры
"#;
        check_snapshot(
            code,
            expect![[r#"
            SelectTopWithoutOrderBy @ 3:22..3:30
              message: Измените запрос, добавив сортировку
              severity: Warning"#]],
        );
    }

    #[test]
    fn test_english_keywords() {
        let code = r#"
Procedure Showcase()
    Text = "SELECT TOP 12 * FROM Catalog.Books";
EndProcedure
"#;
        check_snapshot(
            code,
            expect![[r#"
            SelectTopWithoutOrderBy @ 3:20..3:26
              message: Измените запрос, добавив сортировку
              severity: Warning"#]],
        );
    }
}
