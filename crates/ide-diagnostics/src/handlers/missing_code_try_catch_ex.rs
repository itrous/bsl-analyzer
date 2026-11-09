use crate::define_metadata;
use crate::metadata::*;
use crate::{BodyContext, Diagnostic, DiagnosticCode};
use hir::catch_class::{classify_catch_body, CatchBodyClass};
use hir::BodySourceMap;
use hir::LocalRange;
use hir::{IdConversion, Stmt, StmtId};
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

    check_body_for_empty_except(ctx.body(), ctx.source_map(), comment_as_code, code, ctx, acc);
}

fn check_body_for_empty_except(
    body: &hir::Body,
    source_map: &BodySourceMap,
    comment_as_code: bool,
    code: DiagnosticCode,
    ctx: &BodyContext,
    diagnostics: &mut Vec<Diagnostic<LocalRange>>,
) {
    for stmt_id in body.body_stmts() {
        check_stmt_recursive(stmt_id, body, source_map, comment_as_code, code, ctx, diagnostics);
    }
}

fn check_stmt_recursive(
    stmt_id: StmtId,
    body: &hir::Body,
    source_map: &BodySourceMap,
    comment_as_code: bool,
    code: DiagnosticCode,
    ctx: &BodyContext,
    diagnostics: &mut Vec<Diagnostic<LocalRange>>,
) {
    let stmt = body.stmt(stmt_id);

    if let Stmt::Try { body: try_body, except } = stmt {
        let class = classify_catch_body(body, except);
        let should_emit = match class {
            CatchBodyClass::Empty => {
                !(comment_as_code && except_clause_has_comments(stmt_id, source_map, ctx))
            }
            CatchBodyClass::Silent => true,
            CatchBodyClass::RollbackOnly => true,
            CatchBodyClass::RaisesOnly | CatchBodyClass::LogsOnly | CatchBodyClass::Mixed => false,
        };

        if should_emit {
            emit_at_except_keyword(stmt_id, source_map, code, ctx, diagnostics, class);
        }

        for &nested_stmt_idx in try_body.iter() {
            check_stmt_recursive(
                StmtId::from_idx(nested_stmt_idx),
                body,
                source_map,
                comment_as_code,
                code,
                ctx,
                diagnostics,
            );
        }

        for &nested_stmt_idx in except.iter() {
            check_stmt_recursive(
                StmtId::from_idx(nested_stmt_idx),
                body,
                source_map,
                comment_as_code,
                code,
                ctx,
                diagnostics,
            );
        }
    } else {
        match stmt {
            Stmt::If(if_stmt) => {
                for &nested_idx in if_stmt.then_branch.iter() {
                    check_stmt_recursive(
                        StmtId::from_idx(nested_idx),
                        body,
                        source_map,
                        comment_as_code,
                        code,
                        ctx,
                        diagnostics,
                    );
                }
                for (_, branch) in if_stmt.elsif_branches.iter() {
                    for &nested_idx in branch.iter() {
                        check_stmt_recursive(
                            StmtId::from_idx(nested_idx),
                            body,
                            source_map,
                            comment_as_code,
                            code,
                            ctx,
                            diagnostics,
                        );
                    }
                }
                if let Some(ref branch) = if_stmt.else_branch {
                    for &nested_idx in branch.iter() {
                        check_stmt_recursive(
                            StmtId::from_idx(nested_idx),
                            body,
                            source_map,
                            comment_as_code,
                            code,
                            ctx,
                            diagnostics,
                        );
                    }
                }
            }
            Stmt::While { body: while_body, .. } => {
                for &nested_idx in while_body.iter() {
                    check_stmt_recursive(
                        StmtId::from_idx(nested_idx),
                        body,
                        source_map,
                        comment_as_code,
                        code,
                        ctx,
                        diagnostics,
                    );
                }
            }
            Stmt::For { body: for_body, .. } | Stmt::ForEach { body: for_body, .. } => {
                for &nested_idx in for_body.iter() {
                    check_stmt_recursive(
                        StmtId::from_idx(nested_idx),
                        body,
                        source_map,
                        comment_as_code,
                        code,
                        ctx,
                        diagnostics,
                    );
                }
            }
            _ => {}
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
