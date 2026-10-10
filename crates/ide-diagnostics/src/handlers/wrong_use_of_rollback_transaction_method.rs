use crate::define_metadata;
use crate::metadata::*;
use crate::AnalysisContext;
use crate::{Diagnostic, DiagnosticCode};
use hir::LocalRange;

pub const METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: DiagnosticType::Error,
    severity: DiagnosticSeverityLevel::Critical,
    scope: DiagnosticScope::Bsl,
    modules: &[],
    minutes_to_fix: 1,
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
        DiagnosticCode::WrongUseOfRollbackTransactionMethod,
        "Вызов 'ОтменитьТранзакцию'/'RollbackTransaction' должен находиться в блоке обработки исключений первым оператором",
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
    fn test_valid_first_in_except() {
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
            DiagnosticCode::WrongUseOfRollbackTransactionMethod,
            expect![[r#""#]],
        );
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn test_not_first_in_except() {
        let code = r#"Процедура Тест()
    НачатьТранзакцию();
    Попытка
        ЗафиксироватьТранзакцию();
    Исключение
        Сообщить("Ошибка");
        ОтменитьТранзакцию();
    КонецПопытки;
КонецПроцедуры"#;

        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::WrongUseOfRollbackTransactionMethod,
            expect![[r#"
                WrongUseOfRollbackTransactionMethod @ 7:9..7:30
                  message: Вызов 'ОтменитьТранзакцию'/'RollbackTransaction' должен находиться в блоке обработки исключений первым оператором
                  severity: Critical"#]],
        );
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn test_outside_try_catch() {
        let code = r#"Процедура Тест()
    НачатьТранзакцию();
    ОтменитьТранзакцию();
КонецПроцедуры"#;

        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::WrongUseOfRollbackTransactionMethod,
            expect![[r#"
                WrongUseOfRollbackTransactionMethod @ 3:5..3:26
                  message: Вызов 'ОтменитьТранзакцию'/'RollbackTransaction' должен находиться в блоке обработки исключений первым оператором
                  severity: Critical"#]],
        );
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn test_in_try_body() {
        let code = r#"Процедура Тест()
    НачатьТранзакцию();
    Попытка
        ОтменитьТранзакцию();
        ЗафиксироватьТранзакцию();
    Исключение
        ОтменитьТранзакцию();
    КонецПопытки;
КонецПроцедуры"#;

        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::WrongUseOfRollbackTransactionMethod,
            expect![[r#"
                WrongUseOfRollbackTransactionMethod @ 4:9..4:30
                  message: Вызов 'ОтменитьТранзакцию'/'RollbackTransaction' должен находиться в блоке обработки исключений первым оператором
                  severity: Critical"#]],
        );
    }

    #[test]
    fn test_qualified_call_ignored() {
        let code = r#"Процедура Тест()
    Коннектор.ОтменитьТранзакцию();
КонецПроцедуры"#;

        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::WrongUseOfRollbackTransactionMethod,
            expect![[r#""#]],
        );
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn test_english_keyword() {
        let code = r#"Procedure Test()
    BeginTransaction();
    RollbackTransaction();
EndProcedure"#;

        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::WrongUseOfRollbackTransactionMethod,
            expect![[r#"
                WrongUseOfRollbackTransactionMethod @ 3:5..3:27
                  message: Вызов 'ОтменитьТранзакцию'/'RollbackTransaction' должен находиться в блоке обработки исключений первым оператором
                  severity: Critical"#]],
        );
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn test_comprehensive() {
        let code = r#"Функция Тест()
    НачатьТранзакцию();
    Попытка
        ЗафиксироватьТранзакцию();
    Исключение
        Сообщить("Сообщение");
        Сообщить("Сообщение");
        ОтменитьТранзакцию();  // Срабатывание здесь
    КонецПопытки;

    НачатьТранзакцию();
    ОтменитьТранзакцию();  // Срабатывание здесь
    Возврат;
КонецФункции

Function Test()

BeginTransaction();
Attempt
    DataLock = New DataLock;
    DataLockElement = DataLock.Add("Document.ReceiptNote");
    DataLockElement.SetValue("Reference", ReferenceForProcessing);

    DocumentObject.Record();

    CommitTransaction();
Exception
    DocumentObject.Record();
    DocumentObject.Record();
    RollbackTransaction();  // Срабатывание здесь

    Return;
EndFunction
"#;

        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::WrongUseOfRollbackTransactionMethod,
            expect![[r#"
                WrongUseOfRollbackTransactionMethod @ 8:9..8:30
                  message: Вызов 'ОтменитьТранзакцию'/'RollbackTransaction' должен находиться в блоке обработки исключений первым оператором
                  severity: Critical
                WrongUseOfRollbackTransactionMethod @ 12:5..12:26
                  message: Вызов 'ОтменитьТранзакцию'/'RollbackTransaction' должен находиться в блоке обработки исключений первым оператором
                  severity: Critical
                WrongUseOfRollbackTransactionMethod @ 30:5..30:27
                  message: Вызов 'ОтменитьТранзакцию'/'RollbackTransaction' должен находиться в блоке обработки исключений первым оператором
                  severity: Critical"#]],
        );
    }

    #[test]
    fn test_first_rollback_without_local_transaction_snapshot() {
        check_diagnostics_snapshot_for(
            r#"Процедура Тест()
    Попытка
        Действие();
    Исключение
        ОтменитьТранзакцию();
        ВызватьИсключение;
    КонецПопытки;
КонецПроцедуры"#,
            DiagnosticCode::WrongUseOfRollbackTransactionMethod,
            expect![[r#""#]],
        );
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn test_nested_try_body_rollback_snapshot() {
        check_diagnostics_snapshot_for(
            r#"Процедура Тест()
    НачатьТранзакцию();
    Попытка
        Попытка
            ОтменитьТранзакцию();
        Исключение
            ОтменитьТранзакцию();
        КонецПопытки;
        ЗафиксироватьТранзакцию();
    Исключение
        ОтменитьТранзакцию();
    КонецПопытки;
КонецПроцедуры"#,
            DiagnosticCode::WrongUseOfRollbackTransactionMethod,
            expect![[r#"
                WrongUseOfRollbackTransactionMethod @ 5:13..5:34
                  message: Вызов 'ОтменитьТранзакцию'/'RollbackTransaction' должен находиться в блоке обработки исключений первым оператором
                  severity: Critical
                WrongUseOfRollbackTransactionMethod @ 5:13..5:34
                  message: Вызов 'ОтменитьТранзакцию'/'RollbackTransaction' должен находиться в блоке обработки исключений первым оператором
                  severity: Critical
                WrongUseOfRollbackTransactionMethod @ 7:13..7:34
                  message: Вызов 'ОтменитьТранзакцию'/'RollbackTransaction' должен находиться в блоке обработки исключений первым оператором
                  severity: Critical"#]],
        );
    }
}
