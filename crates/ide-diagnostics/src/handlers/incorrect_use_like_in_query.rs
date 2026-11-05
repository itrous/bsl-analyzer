use crate::define_metadata;
use crate::metadata::*;
use crate::{Diagnostic, DiagnosticCode, DiagnosticsConfig, DiagnosticsContext};

pub const METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: DiagnosticType::Error,
    severity: DiagnosticSeverityLevel::Major,
    scope: DiagnosticScope::Bsl,
    modules: &[],
    minutes_to_fix: 10,
    activated_by_default: true,
    compatibility_mode: DiagnosticCompatibilityMode::Undefined,
    tags: &[MetadataTag::Standard, MetadataTag::Sql, MetadataTag::Unpredictable],
    can_locate_on_project: false,
    extra_min_for_complexity: 0.0,
    lsp_severity_override: "warning",
};

pub(crate) fn dispatch(
    config: &DiagnosticsConfig,
    diag: &sdbl_hir::SdblDiagnostic,
    mapper: &crate::sdbl_utils::SdblPositionMapper,
    query_text: &str,
    diagnostics: &mut Vec<Diagnostic>,
) {
    if let sdbl_hir::SdblDiagnostic::LikeUsage { range, kind: sdbl_hir::LikeUsageKind::Incorrect } =
        diag
    {
        crate::sdbl_utils::dispatch_simple(
            config,
            DiagnosticCode::IncorrectUseLikeInQuery,
            "Нужно исправить выражение в соответствии со стандартом",
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
        DiagnosticCode::IncorrectUseLikeInQuery,
        dispatch,
    )
}

#[cfg(test)]
mod tests {
    use crate::test_utils::check_diagnostics_snapshot_for;
    use crate::DiagnosticCode;
    use expect_test::expect;

    fn check(code: &str, expected: expect_test::Expect) {
        check_diagnostics_snapshot_for(code, DiagnosticCode::IncorrectUseLikeInQuery, expected);
    }

    #[test]
    fn test_detects_invalid_like_patterns() {
        // Allowed patterns: a string literal or a query parameter (also as the right side
        // of a parameter); anything computed or a field is reported, in the selection, in a
        // nested query, in ON and in WHERE.
        let code = r#"Функция ЗапросПоиска()
	Возврат
	"ВЫБРАТЬ
	|	Книги.Название ПОДОБНО ""%роман%"" КАК ЭтоРоман,
	|	Книги.Название ПОДОБНО &Маска КАК ПоМаске,
	|	Книги.Название ПОДОБНО Книги.Подзаголовок КАК ПоПолю,
	|	&Маска ПОДОБНО (Книги.Автор) КАК МаскаПоАвтору,
	|	&Маска ПОДОБНО &ВтораяМаска КАК ДвеМаски,
	|	&Маска ПОДОБНО ПОДСТРОКА(""абв%"", 1, 2) КАК ПоФункции
	|ИЗ
	|	Справочник.Книги КАК Книги
	|	ВНУТРЕННЕЕ СОЕДИНЕНИЕ (
	|		ВЫБРАТЬ
	|			Отзывы.Книга КАК Книга,
	|			Отзывы.Текст ПОДОБНО Отзывы.Заголовок КАК Повтор
	|		ИЗ
	|			РегистрСведений.Отзывы КАК Отзывы) КАК Обзор
	|	ПО Книги.Ссылка = Обзор.Книга
	|		И Книги.Серия ПОДОБНО Обзор.Книга
	|		И Книги.Серия ПОДОБНО ""Классика%""
	|ГДЕ
	|	Книги.Автор ПОДОБНО &Маска
	|	И ""Классика"" ПОДОБНО Книги.Серия";
КонецФункции
"#;
        check(
            code,
            expect![[r#"
            IncorrectUseLikeInQuery @ 6:4..6:45
              message: Нужно исправить выражение в соответствии со стандартом
              severity: Warning
            IncorrectUseLikeInQuery @ 7:4..7:32
              message: Нужно исправить выражение в соответствии со стандартом
              severity: Warning
            IncorrectUseLikeInQuery @ 15:6..15:43
              message: Нужно исправить выражение в соответствии со стандартом
              severity: Warning
            IncorrectUseLikeInQuery @ 19:7..19:38
              message: Нужно исправить выражение в соответствии со стандартом
              severity: Warning
            IncorrectUseLikeInQuery @ 23:6..23:38
              message: Нужно исправить выражение в соответствии со стандартом
              severity: Warning"#]],
        );
    }

    #[test]
    fn test_correct_like_with_literal() {
        let code = r#"Функция ЗапросПоиска()
	Возврат "ВЫБРАТЬ Книги.Название ИЗ Справочник.Книги КАК Книги ГДЕ Книги.Название ПОДОБНО ""Сказ%""";
КонецФункции
"#;
        check(code, expect![[r#""#]]);
    }

    #[test]
    fn test_correct_like_with_parameter() {
        let code = r#"Функция ЗапросПоиска()
	Возврат "ВЫБРАТЬ Книги.Название ИЗ Справочник.Книги КАК Книги ГДЕ Книги.Название ПОДОБНО &Маска";
КонецФункции
"#;
        check(code, expect![[r#""#]]);
    }

    #[test]
    fn test_incorrect_like_with_column_ref() {
        let code = r#"Функция ЗапросПоиска()
	Возврат "ВЫБРАТЬ Книги.Название ИЗ Справочник.Книги КАК Книги ГДЕ Книги.Название ПОДОБНО Книги.Автор";
КонецФункции
"#;
        check(
            code,
            expect![[r#"
            IncorrectUseLikeInQuery @ 2:68..2:102
              message: Нужно исправить выражение в соответствии со стандартом
              severity: Warning"#]],
        );
    }

    #[test]
    fn test_correct_like_with_function_on_parameter_side() {
        let code = r#"Функция ЗапросПоиска()
	Возврат "ВЫБРАТЬ &Маска ПОДОБНО ПОДСТРОКА(""Сказки"", 1, 3) КАК Совпало ИЗ Справочник.Книги КАК Книги";
КонецФункции
"#;
        check(code, expect![[r#""#]]);
    }
}
