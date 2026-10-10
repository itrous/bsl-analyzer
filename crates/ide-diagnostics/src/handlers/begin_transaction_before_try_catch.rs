use crate::define_metadata;
use crate::metadata::*;
use crate::AnalysisContext;
use crate::{Diagnostic, DiagnosticCode};
use hir::LocalRange;

pub const METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: DiagnosticType::Error,
    severity: DiagnosticSeverityLevel::Major,
    scope: DiagnosticScope::All,
    modules: &[],
    minutes_to_fix: 10,
    activated_by_default: true,
    compatibility_mode: DiagnosticCompatibilityMode::Undefined,
    tags: &[MetadataTag::Standard],
    can_locate_on_project: false,
    extra_min_for_complexity: 0.0,
    lsp_severity_override: "",
    clean_code_attribute: CleanCodeAttribute::Intentional,
};

pub fn from_hir(range: LocalRange, ctx: &AnalysisContext) -> Option<Diagnostic<LocalRange>> {
    crate::simple_hir_diagnostic(
        DiagnosticCode::BeginTransactionBeforeTryCatch,
        "Метод 'НачатьТранзакцию' должен быть за пределами блока 'Попытка-Исключение' непосредственно перед оператором 'Попытка'",
        range,
        ctx,
    )
}

#[cfg(test)]
mod tests {
    use crate::test_utils::*;
    use crate::DiagnosticCode;
    use expect_test::expect;

    #[test]
    fn test_valid_before_try() {
        let code = r#"Процедура Тест()
    НачатьТранзакцию();
    Попытка
        ЗаписатьДанные();
        ЗафиксироватьТранзакцию();
    Исключение
        ОтменитьТранзакцию();
        ВызватьИсключение;
    КонецПопытки;
КонецПроцедуры"#;

        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::BeginTransactionBeforeTryCatch,
            expect![[r#""#]],
        );
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn test_code_between() {
        let code = r#"Процедура Тест()
    НачатьТранзакцию();
    Метод();
    Попытка
        ЗаписатьДанные();
    Исключение
        ОтменитьТранзакцию();
    КонецПопытки;
КонецПроцедуры"#;

        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::BeginTransactionBeforeTryCatch,
            expect![[r#"
                BeginTransactionBeforeTryCatch @ 2:5..2:24
                  message: Метод 'НачатьТранзакцию' должен быть за пределами блока 'Попытка-Исключение' непосредственно перед оператором 'Попытка'
                  severity: Major"#]],
        );
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn test_inside_try() {
        let code = r#"Процедура Тест()
    Попытка
        НачатьТранзакцию();
        ЗаписатьДанные();
        ЗафиксироватьТранзакцию();
    Исключение
        ОтменитьТранзакцию();
    КонецПопытки;
КонецПроцедуры"#;

        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::BeginTransactionBeforeTryCatch,
            expect![[r#"
                BeginTransactionBeforeTryCatch @ 3:9..3:28
                  message: Метод 'НачатьТранзакцию' должен быть за пределами блока 'Попытка-Исключение' непосредственно перед оператором 'Попытка'
                  severity: Major"#]],
        );
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn test_no_try_after() {
        let code = r#"Процедура Тест()
    НачатьТранзакцию();
    ЗаписатьДанные();
    ЗафиксироватьТранзакцию();
КонецПроцедуры"#;

        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::BeginTransactionBeforeTryCatch,
            expect![[r#"
                BeginTransactionBeforeTryCatch @ 2:5..2:24
                  message: Метод 'НачатьТранзакцию' должен быть за пределами блока 'Попытка-Исключение' непосредственно перед оператором 'Попытка'
                  severity: Major"#]],
        );
    }

    #[test]
    fn test_qualified_call_ignored() {
        let code = r#"Процедура Тест()
    Коннектор.НачатьТранзакцию();
    ЗаписатьДанные();
КонецПроцедуры"#;

        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::BeginTransactionBeforeTryCatch,
            expect![[r#""#]],
        );
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn test_english_keyword() {
        let code = r#"Procedure Test()
    BeginTransaction();
    SaveData();
EndProcedure"#;

        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::BeginTransactionBeforeTryCatch,
            expect![[r#"
                BeginTransactionBeforeTryCatch @ 2:5..2:24
                  message: Метод 'НачатьТранзакцию' должен быть за пределами блока 'Попытка-Исключение' непосредственно перед оператором 'Попытка'
                  severity: Major"#]],
        );
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn test_case_insensitive() {
        let code = r#"Процедура Тест()
    НАЧАТЬТРАНЗАКЦИЮ();
    Данные();
КонецПроцедуры"#;

        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::BeginTransactionBeforeTryCatch,
            expect![[r#"
                BeginTransactionBeforeTryCatch @ 2:5..2:24
                  message: Метод 'НачатьТранзакцию' должен быть за пределами блока 'Попытка-Исключение' непосредственно перед оператором 'Попытка'
                  severity: Major"#]],
        );
    }

    #[test]
    fn begin_in_preproc_then_try_outside() {
        let code = r#"Процедура Тест()
    НачатьТранзакцию();
    #Если Сервер Тогда
    Попытка
        ЗаписатьДанные();
        ЗафиксироватьТранзакцию();
    Исключение
        ОтменитьТранзакцию();
        ВызватьИсключение;
    КонецПопытки;
    #Иначе
    Попытка
        ЗаписатьДанныеКлиента();
        ЗафиксироватьТранзакцию();
    Исключение
        ОтменитьТранзакцию();
        ВызватьИсключение;
    КонецПопытки;
    #КонецЕсли
КонецПроцедуры"#;

        let diagnostics = check_hir_diagnostic(code);
        let diags: Vec<_> = diagnostics
            .iter()
            .filter(|d| d.code == DiagnosticCode::BeginTransactionBeforeTryCatch)
            .collect();
        assert_eq!(
            diags.len(),
            0,
            "BeginTransaction immediately followed by a preprocessor block \
             where every active branch starts with `Попытка` should be valid \
             after the preproc-aware path lands."
        );
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn begin_in_preproc_asymmetric_then_try_else_missing() {
        let code = r#"Процедура Тест()
    НачатьТранзакцию();
    #Если Сервер Тогда
        Попытка
            ЗаписатьДанные();
            ЗафиксироватьТранзакцию();
        Исключение
            ОтменитьТранзакцию();
            ВызватьИсключение;
        КонецПопытки;
    #Иначе
        ЗаписатьБезТранзакции();
    #КонецЕсли
КонецПроцедуры"#;

        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::BeginTransactionBeforeTryCatch,
            expect![[r#"
                BeginTransactionBeforeTryCatch @ 2:5..2:24
                  message: Метод 'НачатьТранзакцию' должен быть за пределами блока 'Попытка-Исключение' непосредственно перед оператором 'Попытка'
                  severity: Major"#]],
        );
    }

    #[test]
    fn begin_inside_preproc_unchanged_behavior() {
        let code = r#"Процедура Тест()
    #Если Сервер Тогда
        НачатьТранзакцию();
        Попытка
            ЗаписатьДанные();
        Исключение
            ОтменитьТранзакцию();
        КонецПопытки;
    #КонецЕсли
КонецПроцедуры"#;

        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::BeginTransactionBeforeTryCatch,
            expect![[r#""#]],
        );
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn begin_in_preproc_no_else_branch_still_flagged() {
        let code = r#"Процедура Тест()
    НачатьТранзакцию();
    #Если Сервер Тогда
        Попытка
            ЗаписатьДанные();
            ЗафиксироватьТранзакцию();
        Исключение
            ОтменитьТранзакцию();
            ВызватьИсключение;
        КонецПопытки;
    #КонецЕсли
КонецПроцедуры"#;

        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::BeginTransactionBeforeTryCatch,
            expect![[r#"
                BeginTransactionBeforeTryCatch @ 2:5..2:24
                  message: Метод 'НачатьТранзакцию' должен быть за пределами блока 'Попытка-Исключение' непосредственно перед оператором 'Попытка'
                  severity: Major"#]],
        );
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn test_multiple_violations_in_one_module() {
        let code = r#"Процедура ПровестиДокумент()
    НачатьТранзакцию();
    ПодготовитьДанные();
    Попытка
        ЗаписатьДвижения();
    Исключение
        ОтменитьТранзакцию();
    КонецПопытки;
КонецПроцедуры

Функция ОбновитьОстатки()
    Попытка
        НачатьТранзакцию();
        ПересчитатьОстатки();
    Исключение
        ОтменитьТранзакцию();
    КонецПопытки;
    Возврат Истина;
КонецФункции

Процедура СинхронизироватьСправочник()
    НачатьТранзакцию();
    ОбновитьКэш();
    ЗафиксироватьТранзакцию();
КонецПроцедуры

Для каждого СтрокаТовара Из ТаблицаТоваров Цикл
    НачатьТранзакцию();
    ЛогироватьСтроку(СтрокаТовара);
    Попытка
        ОбновитьСтроку(СтрокаТовара);
        ЗафиксироватьТранзакцию();
    Исключение
        ОтменитьТранзакцию();
    КонецПопытки;
КонецЦикла;"#;

        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::BeginTransactionBeforeTryCatch,
            expect![[r#"
                BeginTransactionBeforeTryCatch @ 2:5..2:24
                  message: Метод 'НачатьТранзакцию' должен быть за пределами блока 'Попытка-Исключение' непосредственно перед оператором 'Попытка'
                  severity: Major
                BeginTransactionBeforeTryCatch @ 13:9..13:28
                  message: Метод 'НачатьТранзакцию' должен быть за пределами блока 'Попытка-Исключение' непосредственно перед оператором 'Попытка'
                  severity: Major
                BeginTransactionBeforeTryCatch @ 22:5..22:24
                  message: Метод 'НачатьТранзакцию' должен быть за пределами блока 'Попытка-Исключение' непосредственно перед оператором 'Попытка'
                  severity: Major
                BeginTransactionBeforeTryCatch @ 28:5..28:24
                  message: Метод 'НачатьТранзакцию' должен быть за пределами блока 'Попытка-Исключение' непосредственно перед оператором 'Попытка'
                  severity: Major"#]],
        );
    }
}
