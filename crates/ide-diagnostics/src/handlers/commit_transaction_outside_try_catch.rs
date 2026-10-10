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
        DiagnosticCode::CommitTransactionOutsideTryCatch,
        "Вызов 'ЗафиксироватьТранзакцию'/'CommitTransaction' должен быть размещен в блоке 'Попытка' с обработчиком 'Исключение'",
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
    fn test_valid_inside_try() {
        let code = r#"Процедура Пример1()
    НачатьТранзакцию();
    Попытка
        БлокировкаДанных = Новый БлокировкаДанных;
        ДокументОбъект.Записать();
        ЗафиксироватьТранзакцию();
    Исключение
        ОтменитьТранзакцию();
        ВызватьИсключение;
    КонецПопытки;
КонецПроцедуры"#;

        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::CommitTransactionOutsideTryCatch,
            expect![[r#""#]],
        );
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn test_outside_try() {
        let code = r#"Процедура Пример2()
    НачатьТранзакцию();
    Метод();
    ЗафиксироватьТранзакцию();
КонецПроцедуры"#;

        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::CommitTransactionOutsideTryCatch,
            expect![[r#"
                CommitTransactionOutsideTryCatch @ 4:5..4:31
                  message: Вызов 'ЗафиксироватьТранзакцию'/'CommitTransaction' должен быть размещен в блоке 'Попытка' с обработчиком 'Исключение'
                  severity: Major"#]],
        );
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn test_in_exception_handler() {
        let code = r#"Процедура Пример3()
    НачатьТранзакцию();
    Попытка
        Метод();
    Исключение
        Если ТранзакцияАктивна() Тогда
            ЗафиксироватьТранзакцию();
        Иначе
            ОтменитьТранзакцию();
        КонецЕсли;
    КонецПопытки;
КонецПроцедуры"#;

        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::CommitTransactionOutsideTryCatch,
            expect![[r#"
                CommitTransactionOutsideTryCatch @ 7:13..7:39
                  message: Вызов 'ЗафиксироватьТранзакцию'/'CommitTransaction' должен быть размещен в блоке 'Попытка' с обработчиком 'Исключение'
                  severity: Major"#]],
        );
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn test_code_after_commit() {
        let code = r#"Процедура Пример6()
    НачатьТранзакцию();
    Попытка
        Метод();
        ЗафиксироватьТранзакцию();
        Метод2();
    Исключение
        ОтменитьТранзакцию();
        Возврат;
    КонецПопытки;
КонецПроцедуры"#;

        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::CommitTransactionOutsideTryCatch,
            expect![[r#"
                CommitTransactionOutsideTryCatch @ 5:9..5:35
                  message: Вызов 'ЗафиксироватьТранзакцию'/'CommitTransaction' должен быть размещен в блоке 'Попытка' с обработчиком 'Исключение'
                  severity: Major"#]],
        );
    }

    #[test]
    fn test_qualified_call_ignored() {
        let code = r#"Процедура Тест()
    Коннектор.ЗафиксироватьТранзакцию();
КонецПроцедуры"#;

        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::CommitTransactionOutsideTryCatch,
            expect![[r#""#]],
        );
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn test_english_keyword() {
        let code = r#"Procedure Test()
    BeginTransaction();
    Method();
    CommitTransaction();
EndProcedure"#;

        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::CommitTransactionOutsideTryCatch,
            expect![[r#"
                CommitTransactionOutsideTryCatch @ 4:5..4:25
                  message: Вызов 'ЗафиксироватьТранзакцию'/'CommitTransaction' должен быть размещен в блоке 'Попытка' с обработчиком 'Исключение'
                  severity: Major"#]],
        );
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn test_case_insensitive() {
        let code = r#"Процедура Тест()
    НачатьТранзакцию();
    Метод();
    ЗАФИКСИРОВАТЬТРАНЗАКЦИЮ();
КонецПроцедуры"#;

        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::CommitTransactionOutsideTryCatch,
            expect![[r#"
                CommitTransactionOutsideTryCatch @ 4:5..4:31
                  message: Вызов 'ЗафиксироватьТранзакцию'/'CommitTransaction' должен быть размещен в блоке 'Попытка' с обработчиком 'Исключение'
                  severity: Major"#]],
        );
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn test_multiple_procedures_seven_errors() {
        let code = r#"Процедура Пример1()
    НачатьТранзакцию();
    Попытка
        БлокировкаДанных = Новый БлокировкаДанных;
        ДокументОбъект.Записать();
        ЗафиксироватьТранзакцию();
    Исключение
        ОтменитьТранзакцию();
        ВызватьИсключение;
    КонецПопытки;
КонецПроцедуры

Процедура Пример2()
    НачатьТранзакцию();
    Попытка
        Метод();
    Исключение
        ОтменитьТранзакцию();
        Возврат;
    КонецПопытки;
    ЗафиксироватьТранзакцию();
КонецПроцедуры

Процедура Пример3()
    НачатьТранзакцию();
    Попытка
        Метод();
    Исключение
        Если ТранзакцияАктивна() Тогда
            ЗафиксироватьТранзакцию();
        Иначе
            ОтменитьТранзакцию();
        КонецЕсли;
        Возврат;
    КонецПопытки;
КонецПроцедуры

Процедура Пример4()
    НачатьТранзакцию();
    Метод();
    Если ТранзакцияАктивна() Тогда
        ЗафиксироватьТранзакцию();
    Иначе
        ОтменитьТранзакцию();
    КонецЕсли;
КонецПроцедуры

Функция Пример5()
    НачатьТранзакцию();
    Метод();
    ЗафиксироватьТранзакцию();
    Возврат 1;
КонецФункции

Процедура Пример6()
    НачатьТранзакцию();
    Попытка
        Метод();
        ЗафиксироватьТранзакцию();
        Метод2();
    Исключение
        ОтменитьТранзакцию();
        Возврат;
    КонецПопытки;
КонецПроцедуры

Процедура Пример7()
    НачатьТранзакцию();
    Попытка
        Метод();
        ЗафиксироватьТранзакцию();
        Возврат;
    Исключение
        ОтменитьТранзакцию();
        Возврат;
    КонецПопытки;
КонецПроцедуры"#;

        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::CommitTransactionOutsideTryCatch,
            expect![[r#"
                CommitTransactionOutsideTryCatch @ 21:5..21:31
                  message: Вызов 'ЗафиксироватьТранзакцию'/'CommitTransaction' должен быть размещен в блоке 'Попытка' с обработчиком 'Исключение'
                  severity: Major
                CommitTransactionOutsideTryCatch @ 30:13..30:39
                  message: Вызов 'ЗафиксироватьТранзакцию'/'CommitTransaction' должен быть размещен в блоке 'Попытка' с обработчиком 'Исключение'
                  severity: Major
                CommitTransactionOutsideTryCatch @ 42:9..42:35
                  message: Вызов 'ЗафиксироватьТранзакцию'/'CommitTransaction' должен быть размещен в блоке 'Попытка' с обработчиком 'Исключение'
                  severity: Major
                CommitTransactionOutsideTryCatch @ 51:5..51:31
                  message: Вызов 'ЗафиксироватьТранзакцию'/'CommitTransaction' должен быть размещен в блоке 'Попытка' с обработчиком 'Исключение'
                  severity: Major
                CommitTransactionOutsideTryCatch @ 59:9..59:35
                  message: Вызов 'ЗафиксироватьТранзакцию'/'CommitTransaction' должен быть размещен в блоке 'Попытка' с обработчиком 'Исключение'
                  severity: Major
                CommitTransactionOutsideTryCatch @ 71:9..71:35
                  message: Вызов 'ЗафиксироватьТранзакцию'/'CommitTransaction' должен быть размещен в блоке 'Попытка' с обработчиком 'Исключение'
                  severity: Major"#]],
        );
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn test_commit_in_loop_after_code() {
        let code = r#"Процедура Тест()
    Для каждого Элемент Из Коллекция Цикл
        НачатьТранзакцию();
        Попытка
            Метод();
            ЗафиксироватьТранзакцию();
            Продолжить;
        Исключение
            ОтменитьТранзакцию();
            Возврат;
        КонецПопытки;
    КонецЦикла;
КонецПроцедуры"#;

        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::CommitTransactionOutsideTryCatch,
            expect![[r#"
                CommitTransactionOutsideTryCatch @ 6:13..6:39
                  message: Вызов 'ЗафиксироватьТранзакцию'/'CommitTransaction' должен быть размещен в блоке 'Попытка' с обработчиком 'Исключение'
                  severity: Major"#]],
        );
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn test_single_sub_outside_try() {
        let code = r#"Процедура Тестовая()
    НачатьТранзакцию();
    А = 1;
    ЗафиксироватьТранзакцию();
КонецПроцедуры"#;

        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::CommitTransactionOutsideTryCatch,
            expect![[r#"
                CommitTransactionOutsideTryCatch @ 4:5..4:31
                  message: Вызов 'ЗафиксироватьТранзакцию'/'CommitTransaction' должен быть размещен в блоке 'Попытка' с обработчиком 'Исключение'
                  severity: Major"#]],
        );
    }

    #[test]
    fn test_nested_if_commit_in_try_snapshot() {
        check_diagnostics_snapshot_for(
            r#"Процедура Тест(Условие)
    НачатьТранзакцию();
    Попытка
        Если Условие Тогда
            ЗафиксироватьТранзакцию();
        КонецЕсли;
        ДействиеПослеФиксации();
    Исключение
        ОтменитьТранзакцию();
        ВызватьИсключение;
    КонецПопытки;
КонецПроцедуры"#,
            DiagnosticCode::CommitTransactionOutsideTryCatch,
            expect![[r#""#]],
        );
    }
}
