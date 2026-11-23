use crate::define_metadata;
use crate::metadata::*;
use crate::{BodyContext, Diagnostic, DiagnosticCode};
use hir::catch_class::{classify_catch_body, CatchBodyClass};
use hir::BodySourceMap;
use hir::LocalRange;
use hir::{Stmt, StmtId};
use syntax::{NodeOrToken, SyntaxKind};

pub const METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: DiagnosticType::Error,
    severity: DiagnosticSeverityLevel::Major,
    scope: DiagnosticScope::All,
    modules: &[],
    minutes_to_fix: 15,
    activated_by_default: true,
    compatibility_mode: DiagnosticCompatibilityMode::Undefined,
    tags: &[MetadataTag::Standard, MetadataTag::Badpractice],
    can_locate_on_project: false,
    extra_min_for_complexity: 0.0,
    lsp_severity_override: "warning",
    clean_code_attribute: CleanCodeAttribute::Intentional,
};

pub fn check_body(ctx: &BodyContext, acc: &mut Vec<Diagnostic<LocalRange>>) {
    let code = DiagnosticCode::MissingCodeTryCatchEx;

    if ctx.is_disabled_with_metadata(code) {
        return;
    }

    let comment_as_code = ctx
        .config
        .get_bool(DiagnosticCode::MissingCodeTryCatchEx, "commentAsCode")
        .unwrap_or(false);

    let body = ctx.body();
    let source_map = ctx.source_map();
    let mut catches: Vec<_> = body
        .stmts_iter()
        .filter_map(|(id, stmt)| match stmt {
            Stmt::Try { except, .. } => Some((id, classify_catch_body(body, except))),
            _ => None,
        })
        .collect();
    catches.sort_by_key(|(id, _)| source_map.stmt_range(*id).map(|range| range.in_root().start()));
    let observable = if catches
        .iter()
        .any(|(_, class)| matches!(class, CatchBodyClass::Silent | CatchBodyClass::RollbackOnly))
    {
        hir::dataflow::catch_outcome::observable_catch_results(
            body,
            ctx.cfg(),
            ctx.platform_call_effects(),
        )
    } else {
        Default::default()
    };
    for (stmt_id, class) in catches {
        let should_emit = match class {
            CatchBodyClass::Empty => {
                !(comment_as_code && except_clause_has_comments(stmt_id, source_map, ctx))
            }
            CatchBodyClass::Silent | CatchBodyClass::RollbackOnly => !observable.contains(&stmt_id),
            CatchBodyClass::RaisesOnly | CatchBodyClass::LogsOnly | CatchBodyClass::Mixed => false,
        };
        if should_emit {
            emit_at_except_keyword(stmt_id, source_map, code, ctx, acc, class);
        }
    }
}

fn except_clause_has_comments(
    stmt_id: StmtId,
    source_map: &BodySourceMap,
    ctx: &BodyContext,
) -> bool {
    let Some(stmt_range) = source_map.stmt_range(stmt_id) else { return false };
    let Some(try_node) = ctx
        .root()
        .descendants()
        .find(|n| n.kind() == SyntaxKind::TRY_STMT && n.text_range() == stmt_range.in_root())
    else {
        return false;
    };
    // Область блока задаётся ключевыми словами, а не узлом клаузы: тривия
    // принадлежит общему предку соседних значимых токенов, поэтому
    // комментарий пустого блока лежит уже не внутри `EXCEPT_CLAUSE`, а рядом
    // с ней. Вложенная `Попытка` границу не сдвигает — её `КонецПопытки`
    // лежит внутри своего узла и прямым ребёнком этого не является.
    try_node
        .children_with_tokens()
        .skip_while(|el| el.kind() != SyntaxKind::KW_EXCEPT)
        .take_while(|el| el.kind() != SyntaxKind::KW_END_TRY)
        .any(|el| match el {
            NodeOrToken::Token(token) => token.kind() == SyntaxKind::COMMENT,
            NodeOrToken::Node(node) => node
                .descendants_with_tokens()
                .filter_map(|el| el.into_token())
                .any(|token| token.kind() == SyntaxKind::COMMENT),
        })
}

fn emit_at_except_keyword(
    stmt_id: StmtId,
    source_map: &BodySourceMap,
    code: DiagnosticCode,
    ctx: &BodyContext,
    diagnostics: &mut Vec<Diagnostic<LocalRange>>,
    class: CatchBodyClass,
) {
    let message = match class {
        CatchBodyClass::Silent => "Блок исключения молча подавляет ошибку: добавьте \
             `ВызватьИсключение` или вызов журналирования"
            .to_string(),
        CatchBodyClass::RollbackOnly => "Блок исключения только откатывает транзакцию, \
             но не фиксирует ошибку: добавьте логирование или `ВызватьИсключение`"
            .to_string(),
        _ => "Отсутствует код в блоке исключения".to_string(),
    };

    let range = source_map
        .stmt_range(stmt_id)
        .and_then(|stmt_range| {
            let try_node = ctx.root().descendants().find(|n| {
                n.kind() == SyntaxKind::TRY_STMT && n.text_range() == stmt_range.in_root()
            })?;
            try_node
                .children_with_tokens()
                .filter_map(|el| el.into_token())
                .find(|tok| tok.kind() == SyntaxKind::KW_EXCEPT)
                .map(|tok| ctx.token_range(&tok))
        })
        .or_else(|| source_map.stmt_range(stmt_id));

    if let Some(range) = range {
        diagnostics.push(Diagnostic {
            code,
            message,
            severity: ctx.severity(code),
            range,
            tags: ctx.tags(code),
            fixes: vec![],
        });
    }
}

#[cfg(test)]
mod tests {
    use super::check_body;
    use crate::test_utils::{
        check_body_diagnostic, check_body_diagnostic_with_config, check_diagnostics_snapshot_for,
        format_diags,
    };
    use crate::{DiagnosticCode, DiagnosticsConfig};
    use expect_test::expect;

    fn check(code: &str, expected: expect_test::Expect) {
        check_diagnostics_snapshot_for(code, DiagnosticCode::MissingCodeTryCatchEx, expected);
    }

    fn configured_diagnostics(code: &str) -> Vec<crate::Diagnostic> {
        crate::test_utils::check_with_cfe(code, test_fixture::CfeFixtureBuilder::new("").build())
            .into_iter()
            .filter(|diagnostic| diagnostic.code == DiagnosticCode::MissingCodeTryCatchEx)
            .collect()
    }

    #[test]
    fn observable_exception_results_do_not_emit() {
        let code = include_str!("fixtures/observable_catch_results.bsl");
        let diagnostics = configured_diagnostics(code);
        assert!(diagnostics.is_empty(), "{diagnostics:#?}");
    }

    #[test]
    fn unindexed_platform_calls_do_not_prove_exception_observation() {
        let code = include_str!("fixtures/observable_catch_results.bsl");
        let diagnostics = check_body_diagnostic(code, check_body);
        assert_eq!(diagnostics.len(), 3, "{diagnostics:#?}");
    }

    #[test]
    fn unobserved_exception_results_still_emit() {
        let code = include_str!("fixtures/unobserved_catch_results.bsl");
        assert_eq!(configured_diagnostics(code).len(), 3);
    }

    #[test]
    fn discarded_exception_results_still_emit() {
        let code = include_str!("fixtures/discarded_catch_results.bsl");
        let diagnostics = configured_diagnostics(code);
        assert_eq!(diagnostics.len(), 10, "{diagnostics:#?}");
    }

    #[test]
    fn shadowed_formatters_do_not_prove_exception_observation() {
        let code = include_str!("fixtures/shadowed_catch_formatters.bsl");
        let diagnostics = configured_diagnostics(code);
        assert_eq!(diagnostics.len(), 2, "{diagnostics:#?}");
    }

    #[test]
    fn platform_calls_propagate_only_the_value_positions() {
        for (setup, caught, after, expected) in [
            ("", "Failure = ErrorDescription();", "Return XMLString(String(Failure));", 0),
            ("", "Ошибка = ОписаниеОшибки();", "Возврат XMLСтрока(Строка(Ошибка));", 0),
            (
                "",
                "Failure = ErrorDescription();",
                "Return New Structure(\"One,Two\", \"constant\", Failure);",
                0,
            ),
            ("", "Failure = New FixedArray(New Array);", "Return New Array(Failure);", 0),
            ("", "Size = 1;", "Return New Array(Size);", 1),
            ("", "Failure = \"Key\";", "Return New Structure(Failure, \"constant\");", 1),
            (
                "Values = New Array;",
                "Failure = ErrorDescription();",
                "Values.Insert(0, Failure);\nReturn StrConcat(Values, \",\");",
                0,
            ),
            (
                "Values = New Array;",
                "Position = 0;",
                "Values.Insert(Position, \"constant\");\nReturn Values.Count();",
                1,
            ),
            ("Values = New Array;", "Values.Add();", "Return Values.Count();", 0),
            (
                "Values = New Array;",
                "Values.Add(ErrorDescription());",
                "Values.Clear();\nReturn Values.Count();",
                1,
            ),
            ("", "Failure = ErrorDescription();", "Values.Add(Failure);\nReturn Values;", 1),
            (
                "If Flag Then\nValues = New Structure;\nElse\nValues = New Array;\nEndIf;",
                "Failure = ErrorDescription();",
                "Values.Add(Failure);\nReturn Values;",
                1,
            ),
        ] {
            let code = format!(
                "Function Test(Values, Flag)\n{setup}\nTry\n Work();\nExcept\n{caught}\nEndTry;\n{after}\nEndFunction"
            );
            let diagnostics = configured_diagnostics(&code);
            assert_eq!(diagnostics.len(), expected, "{code}\n{diagnostics:#?}");
        }
    }

    #[test]
    fn array_calls_preserve_the_failure_variable() {
        for (call, result, expected) in [
            ("Values.Add(Failure)", "Failure", 0),
            ("Values.Insert(Index, Failure)", "Failure", 0),
            ("Values.Insert(Index, Failure)", "Index", 0),
            ("UnknownValues.Add(Failure)", "Failure", 1),
            ("Add(Failure)", "Failure", 1),
        ] {
            let code = format!(
                r#"Procedure Add(Failure)
    Failure = "";
EndProcedure

Function Test(UnknownValues)
    Values = New Array;
    Try
        Work();
    Except
        Failure = ErrorDescription();
        Index = 0;
    EndTry;
    {call};
    Return {result};
EndFunction"#
            );
            let diagnostics = configured_diagnostics(&code);
            assert_eq!(diagnostics.len(), expected, "{call}\n{diagnostics:#?}");
        }
    }

    #[test]
    fn known_array_calls_preserve_the_type_of_collection_arguments() {
        for (setup, earlier_call, expected) in [
            ("Holder = New Array;", "Holder.Add(Values);", 0),
            (
                "Holder = New Array;\nOuter = New Array;",
                "Outer.Add(Holder);\nHolder.Insert(0, Values);",
                0,
            ),
            ("", "Holder.Add(Values);", 1),
            (
                "If Flag Then\nHolder = Unknown;\nElse\nHolder = New Array;\nEndIf;",
                "Holder.Add(Values);",
                1,
            ),
            (
                "Holder = New Array;",
                "While Flag Do\nHolder.Add(Values);\nValues.Add(Holder);\nHolder = Unknown;\nEndDo;",
                1,
            ),
            ("", "Change(Values);", 1),
        ] {
            let code = format!(
                r#"Procedure Change(Values)
    Values = Undefined;
EndProcedure

Function Test(Holder, Unknown, Flag)
    Values = New Array;
    {setup}
    {earlier_call}
    Try
        Work();
    Except
        Values.Add(ErrorDescription());
    EndTry;
    Return Values.Count();
EndFunction"#
            );
            let diagnostics = configured_diagnostics(&code);
            assert_eq!(diagnostics.len(), expected, "{setup}\n{earlier_call}\n{diagnostics:#?}");
        }
    }

    #[test]
    fn raises_only_does_not_emit() {
        let code = r#"Процедура ОтправитьВыгрузку(Файл)
	Попытка
		Транспорт.Передать(Файл);
	Исключение
		ВызватьИсключение;
	КонецПопытки;
КонецПроцедуры
"#;
        check(code, expect![[r#""#]]);
    }

    #[test]
    fn logs_only_does_not_emit() {
        let code = r#"Процедура ОтправитьВыгрузку(Файл)
	Попытка
		Транспорт.Передать(Файл);
	Исключение
		Сообщить(ОписаниеОшибки());
	КонецПопытки;
КонецПроцедуры
"#;
        check(code, expect![[r#""#]]);
    }

    #[test]
    fn test_logs_only_write_log_event_snapshot() {
        let code = r#"Процедура ОтправитьВыгрузку(Файл)
	Попытка
		Транспорт.Передать(Файл);
	Исключение
		ЗаписьЖурналаРегистрации("Выгрузка", УровеньЖурналаРегистрации.Ошибка, , , ОписаниеОшибки());
	КонецПопытки;
КонецПроцедуры
"#;
        check(code, expect![[r#""#]]);
    }

    #[test]
    fn issue_table_logging_calls_do_not_emit_in_either_language() {
        for (try_, except_, end_try) in
            [("Try", "Except", "EndTry"), ("Попытка", "Исключение", "КонецПопытки")]
        {
            for call in [
                "WriteErrorToLog(E, T)",
                "WriteErrorToEventLog(E, T)",
                "LogError(E, T)",
                "EventLogClient.AddMessageForEventLog(E, \"Error\", T)",
                "WriteLogEvent(\"E\", EventLogLevel.Error, , , T)",
                "ЗаписатьОшибкуВЖурнал(E, T)",
                "ЖурналРегистрацииКлиент.ДобавитьСообщениеДляЖурналаРегистрации(E, \"Ошибка\", T)",
            ] {
                let code = format!(
                    "Procedure P(E, T) Export\n\t{try_}\n\t\tA = 1 / 0;\n\t{except_}\n\t\t{call};\n\t{end_try};\nEndProcedure\n"
                );
                check(&code, expect![[r#""#]]);
            }
        }
    }

    #[test]
    fn log_inside_a_word_does_not_hide_the_swallow() {
        for call in ["AddToCatalog(E, T)", "ShowDialog(E, T)"] {
            let code = format!(
                "Procedure P(E, T) Export\n\tTry\n\t\tA = 1 / 0;\n\tExcept\n\t\t{call};\n\tEndTry;\nEndProcedure\n"
            );
            let diagnostics = check_body_diagnostic(&code, check_body);
            assert_eq!(diagnostics.len(), 1, "`{call}` must leave the swallow flagged");
        }
    }

    #[test]
    fn mixed_does_not_emit() {
        let code = r#"Процедура ОтправитьВыгрузку(Файл)
	Попытка
		Транспорт.Передать(Файл);
	Исключение
		Сообщить(ОписаниеОшибки());
		ВызватьИсключение;
	КонецПопытки;
КонецПроцедуры
"#;
        check(code, expect![[r#""#]]);
    }

    #[test]
    fn silent_swallow_emits() {
        let code = r#"Процедура ОтправитьВыгрузку(Файл)
	Попытка
		Транспорт.Передать(Файл);
	Исключение
		Файл.Пометить(Ложь);
	КонецПопытки;
КонецПроцедуры
"#;
        check(
            code,
            expect![[r#"
            MissingCodeTryCatchEx @ 4:2..4:12
              message: Блок исключения молча подавляет ошибку: добавьте `ВызватьИсключение` или вызов журналирования
              severity: Warning"#]],
        );

        let diagnostics = check_body_diagnostic(code, check_body);
        assert!(
            diagnostics[0].message.contains("молча"),
            "Silent message should mention the swallow, got: {}",
            diagnostics[0].message
        );
    }

    #[test]
    fn rollback_only_emits_with_rollback_message() {
        let code = r#"Процедура ЗаписатьОстатки(Набор)
	НачатьТранзакцию();
	Попытка
		Набор.Записать();
		ЗафиксироватьТранзакцию();
	Исключение
		ОтменитьТранзакцию();
	КонецПопытки;
КонецПроцедуры
"#;
        check(
            code,
            expect![[r#"
            MissingCodeTryCatchEx @ 6:2..6:12
              message: Блок исключения только откатывает транзакцию, но не фиксирует ошибку: добавьте логирование или `ВызватьИсключение`
              severity: Warning"#]],
        );

        let diagnostics = check_body_diagnostic(code, check_body);
        assert!(
            diagnostics[0].message.contains("откатывает"),
            "Rollback message should mention rollback, got: {}",
            diagnostics[0].message
        );
    }

    #[test]
    fn rollback_plus_log_does_not_emit() {
        let code = r#"Процедура ЗаписатьОстатки(Набор)
	НачатьТранзакцию();
	Попытка
		Набор.Записать();
		ЗафиксироватьТранзакцию();
	Исключение
		ОтменитьТранзакцию();
		Сообщить(ОписаниеОшибки());
	КонецПопытки;
КонецПроцедуры
"#;
        check(code, expect![[r#""#]]);
    }

    #[test]
    fn empty_and_comment_only_blocks_emit() {
        let code = r#"Процедура ПрочитатьКурсы()
	Попытка
		Курсы = Сервис.Курсы();
	Исключение

	КонецПопытки;
КонецПроцедуры

Функция КурсДоллара()
	Попытка
		Возврат Сервис.Курс("USD");
	Исключение
		// сервис бывает недоступен по ночам
	КонецПопытки;
	Возврат 0;
КонецФункции

Процедура ОбновитьКурсы()
	Попытка
		Сервис.Обновить();
	Исключение
		// сначала пишем причину, затем пробрасываем

		ВызватьИсключение;
	КонецПопытки;
КонецПроцедуры
"#;
        check(
            code,
            expect![[r#"
            MissingCodeTryCatchEx @ 4:2..4:12
              message: Отсутствует код в блоке исключения
              severity: Warning
            MissingCodeTryCatchEx @ 12:2..12:12
              message: Отсутствует код в блоке исключения
              severity: Warning"#]],
        );
    }

    #[test]
    fn test_comment_as_code() {
        let code = r#"Процедура ПрочитатьКурсы()
	Попытка
		Курсы = Сервис.Курсы();
	Исключение

	КонецПопытки;
КонецПроцедуры

Функция КурсДоллара()
	Попытка
		Возврат Сервис.Курс("USD");
	Исключение
		// сервис бывает недоступен по ночам
	КонецПопытки;
	Возврат 0;
КонецФункции
"#;
        let mut config = DiagnosticsConfig::default();
        config.parameters.insert(
            DiagnosticCode::MissingCodeTryCatchEx,
            serde_json::json!({"commentAsCode": true}),
        );

        let diagnostics = check_body_diagnostic_with_config(code, config, check_body);
        expect![[r#"
            MissingCodeTryCatchEx @ 4:2..4:12
              message: Отсутствует код в блоке исключения
              severity: Warning"#]]
        .assert_eq(&format_diags(code, &diagnostics));
    }

    #[test]
    fn nested_try_without_reraise_keeps_outer_silent() {
        // A raise inside the nested try would be caught by the nested handler, and that
        // handler is empty: nothing in the outer handler logs or rethrows.
        let code = r#"Процедура ПрочитатьКурсы()
	Попытка
		Курсы = Сервис.Курсы();
	Исключение
		Попытка
			Курсы = Кэш.Курсы();
		Исключение

		КонецПопытки;
	КонецПопытки;
КонецПроцедуры
"#;
        check(
            code,
            expect![[r#"
            MissingCodeTryCatchEx @ 4:2..4:12
              message: Блок исключения молча подавляет ошибку: добавьте `ВызватьИсключение` или вызов журналирования
              severity: Warning
            MissingCodeTryCatchEx @ 7:3..7:13
              message: Отсутствует код в блоке исключения
              severity: Warning"#]],
        );
    }

    #[test]
    fn conditional_reraise_does_not_emit() {
        // Suppressing one known error and rethrowing everything else is a
        // legitimate handler; the Raise lives on a nested path.
        let code = r#"Процедура ПрочитатьКурсы()
	Попытка
		Курсы = Сервис.Курсы();
	Исключение
		Если Не СтрНайти(ОписаниеОшибки(), "таймаут") > 0 Тогда
			ВызватьИсключение;
		КонецЕсли;
	КонецПопытки;
КонецПроцедуры
"#;
        check(code, expect![[r#""#]]);
    }

    #[test]
    fn conditional_swallow_still_emits() {
        let code = r#"Процедура ПрочитатьКурсы()
	Попытка
		Курсы = Сервис.Курсы();
	Исключение
		Если Курсы = Неопределено Тогда
			Курсы = Новый Соответствие;
		КонецЕсли;
	КонецПопытки;
КонецПроцедуры
"#;
        check(
            code,
            expect![[r#"
            MissingCodeTryCatchEx @ 4:2..4:12
              message: Блок исключения молча подавляет ошибку: добавьте `ВызватьИсключение` или вызов журналирования
              severity: Warning"#]],
        );
    }

    #[test]
    fn test_module_level_raises_only_snapshot() {
        let code = r#"Попытка
	ИнициализироватьКурсы();
Исключение
	ВызватьИсключение;
КонецПопытки;
"#;
        check(code, expect![[r#""#]]);
    }

    #[test]
    fn local_event_wrapper_does_not_emit() {
        let code = r#"Процедура РазобратьСписок(Файл)
	Попытка
		Транспорт.Передать(Файл);
	Исключение
		ЗаписатьСобытиеПодсистемы("Список не разобран: " + ПодробноеПредставлениеОшибки(ИнформацияОбОшибке()),
			УровеньЖурналаРегистрации.Ошибка);
		Возврат;
	КонецПопытки;
КонецПроцедуры
"#;
        check(code, expect![[r#""#]]);
    }
}
