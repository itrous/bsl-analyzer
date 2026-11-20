use crate::body::Body;
use crate::hir::{Expr, ExprIdx, Stmt, StmtIdx};
use bsl_platform::security::{registry, Category};
use stdx::case::CaseExt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatchBodyClass {
    Empty,
    RaisesOnly,
    LogsOnly,
    RollbackOnly,
    Mixed,
    Silent,
}

pub fn classify_catch_body(body: &Body, except: &[StmtIdx]) -> CatchBodyClass {
    if except.is_empty() {
        return CatchBodyClass::Empty;
    }

    let mut flags = RecoveryFlags::default();
    scan_stmts(body, except, true, &mut flags);
    let RecoveryFlags { raise: has_raise, log: has_log, rollback: has_rollback, other: has_other } =
        flags;

    if let Some(pure) = match (has_raise, has_log, has_rollback, has_other) {
        (true, false, false, false) => Some(CatchBodyClass::RaisesOnly),
        (false, true, false, false) => Some(CatchBodyClass::LogsOnly),
        (false, false, true, false) => Some(CatchBodyClass::RollbackOnly),
        (false, false, false, true) => Some(CatchBodyClass::Silent),
        _ => None,
    } {
        return pure;
    }

    if has_raise || has_log {
        CatchBodyClass::Mixed
    } else if has_rollback {
        CatchBodyClass::RollbackOnly
    } else {
        CatchBodyClass::Silent
    }
}

#[derive(Default)]
struct RecoveryFlags {
    raise: bool,
    log: bool,
    rollback: bool,
    other: bool,
}

/// Collects recovery actions on every control-flow path of the handler:
/// a `Raise` (or log/rollback call) inside an `Если`/loop branch is a real
/// conditional rethrow, so the branching statements themselves are
/// transparent. `raise_escapes` is false inside a nested `Попытка` body —
/// a `Raise` there is caught by the nested handler and never leaves the
/// outer one; only the nested handler's own statements rethrow outward.
fn scan_stmts(body: &Body, stmts: &[StmtIdx], raise_escapes: bool, flags: &mut RecoveryFlags) {
    for &stmt_idx in stmts.iter() {
        match body.stmt_idx(stmt_idx) {
            Stmt::Raise { .. } => {
                if raise_escapes {
                    flags.raise = true;
                } else {
                    flags.other = true;
                }
            }
            Stmt::Expr(expr_id) => match recovery_kind(body, *expr_id) {
                RecoveryKind::Log => flags.log = true,
                RecoveryKind::Rollback => flags.rollback = true,
                RecoveryKind::None => flags.other = true,
            },
            Stmt::If(if_stmt) => {
                scan_stmts(body, &if_stmt.then_branch, raise_escapes, flags);
                for (_, branch) in if_stmt.elsif_branches.iter() {
                    scan_stmts(body, branch, raise_escapes, flags);
                }
                if let Some(branch) = &if_stmt.else_branch {
                    scan_stmts(body, branch, raise_escapes, flags);
                }
            }
            Stmt::PreprocIf(preproc) => {
                scan_stmts(body, &preproc.then_branch, raise_escapes, flags);
                for (_, _, branch) in preproc.elsif_branches.iter() {
                    scan_stmts(body, branch, raise_escapes, flags);
                }
                if let Some(branch) = &preproc.else_branch {
                    scan_stmts(body, branch, raise_escapes, flags);
                }
            }
            Stmt::While { body: loop_body, .. }
            | Stmt::For { body: loop_body, .. }
            | Stmt::ForEach { body: loop_body, .. } => {
                scan_stmts(body, loop_body, raise_escapes, flags);
            }
            Stmt::Try { body: try_body, except } => {
                scan_stmts(body, try_body, false, flags);
                scan_stmts(body, except, raise_escapes, flags);
            }
            _ => flags.other = true,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RecoveryKind {
    Log,
    Rollback,
    None,
}

fn recovery_kind(body: &Body, expr_id: ExprIdx) -> RecoveryKind {
    let reg = registry();
    let classify = |entry: &bsl_platform::security::SecurityEntry| match entry.category {
        Category::Logging => RecoveryKind::Log,
        Category::Transaction => RecoveryKind::Rollback,
        _ => RecoveryKind::None,
    };
    let lookup = |name: &str| {
        if let Some(kind) = reg.lookup_global(name).map(classify) {
            if kind != RecoveryKind::None {
                return kind;
            }
        }
        if name_is_error_report_sink(name) {
            RecoveryKind::Log
        } else {
            RecoveryKind::None
        }
    };
    match body.expr_idx(expr_id) {
        Expr::Call { callee, .. } => match body.expr_idx(*callee) {
            Expr::Path(name) => lookup(name.as_str()),
            Expr::Field { field, .. } => lookup(field.as_str()),
            _ => RecoveryKind::None,
        },
        Expr::MethodCall { method, .. } => lookup(method.as_str()),
        _ => RecoveryKind::None,
    }
}

/// Recognizes user/BSP error-reporting helpers by name: a recording or
/// notifying verb combined with an error/log/problem/warning/event noun
/// (`ЗаписатьОшибкуВЖурналРегистрации`, `ДобавитьСообщениеДляЖурналаРегистрации`,
/// `СообщитьОПроблеме`, `ПоказатьПредупреждение`, `ЗаписатьСобытие…`,
/// `WriteErrorToLog`, `AddMessageForEventLog`, `LogError`, …).
///
/// The platform security registry only knows global logging primitives, but the
/// bulk of BSP/application handlers report through such named helpers. Pure
/// formatters and getters (`ПодробноеПредставлениеОшибки`, `ОписаниеОшибки`,
/// `ИнформацияОбОшибке`) carry the noun but no recording verb, so they are not
/// treated as logging and a handler that only formats the error stays `Silent`.
fn name_is_error_report_sink(name: &str) -> bool {
    russian_name_is_error_report_sink(name) || english_name_is_error_report_sink(name)
}

fn russian_name_is_error_report_sink(name: &str) -> bool {
    const VERBS: &[&str] = &[
        "запис",     // Записать… / Запись…
        "добав",     // ДобавитьОшибку / ДобавитьСообщение…
        "зафиксир",  // ЗафиксироватьОшибку
        "зарегистр", // ЗарегистрироватьОшибку
        "регистрац", // РегистрацияОшибки
        "вывест",    // ВывестиОшибку
        "показа",    // ПоказатьОшибку / ПоказатьПредупреждение
        "сообщи",    // СообщитьОбОшибке / СообщитьОПроблеме (но не `Сообщение…`)
        "оповест",   // Оповестить…
        "сохран",    // СохранитьОшибкуВЖурнал
    ];
    const NOUNS: &[&str] = &[
        "ошибк",      // ЗаписатьОшибку…
        "журнал",     // …ВЖурналРегистрации
        "проблем",    // СообщитьОПроблеме
        "предупрежд", // ПоказатьПредупреждение
        "событи",     // ЗаписатьСобытиеЖурнала / ДобавитьСобытие…
    ];

    let lower = name.fold_lower();
    VERBS.iter().any(|verb| lower.contains(verb)) && NOUNS.iter().any(|noun| lower.contains(noun))
}

/// English counterpart of the Russian table. English stems are short and sit
/// inside unrelated words (`log` in `Catalog`/`Dialog`, `add` in `Address`),
/// so each name is split into words (CamelCase, `_`, digits) and a stem must
/// equal a whole word; a verb and a noun must be two different words. `log`
/// is a verb only as the leading word (`LogError`): elsewhere it is the noun
/// (`ReadEventLog`, `EventLogLevel`).
fn english_name_is_error_report_sink(name: &str) -> bool {
    const VERBS: &[&str] =
        &["write", "add", "record", "register", "report", "notify", "show", "save"];
    // `EventLog` needs no entry of its own: it splits into `event` + `log`.
    const NOUNS: &[&str] = &["error", "log", "event", "problem", "warning"];

    let words = name_words(name);
    // `RegisterEventHandler` / `AddErrorHandler` wire a handler up; they do not report.
    if words.iter().any(|word| word == "handler" || word == "handlers") {
        return false;
    }
    let is_noun = |word: &str| {
        NOUNS.contains(&word) || word.strip_suffix('s').is_some_and(|stem| NOUNS.contains(&stem))
    };
    words.iter().enumerate().any(|(verb_at, verb)| {
        (VERBS.contains(&verb.as_str()) || (verb_at == 0 && verb == "log"))
            && words.iter().enumerate().any(|(noun_at, noun)| noun_at != verb_at && is_noun(noun))
    })
}

/// Lower-cased words of an identifier: splits at non-letters (`_`, digits),
/// at a lower→upper step (`WriteLog` → `write`, `log`) and at the end of an
/// acronym (`HTTPError` → `http`, `error`). An all-caps run without separators
/// stays one word: it carries no boundaries to split at.
fn name_words(name: &str) -> Vec<String> {
    let chars: Vec<char> = name.chars().collect();
    let mut words = Vec::new();
    let mut current = String::new();
    for (at, &ch) in chars.iter().enumerate() {
        if !ch.is_alphabetic() {
            if !current.is_empty() {
                words.push(std::mem::take(&mut current).fold_lower());
            }
            continue;
        }
        let prev = at.checked_sub(1).map(|i| chars[i]);
        let next = chars.get(at + 1).copied();
        let starts_word = ch.is_uppercase()
            && prev.is_some_and(|p| {
                p.is_lowercase() || (p.is_uppercase() && next.is_some_and(char::is_lowercase))
            });
        if starts_word && !current.is_empty() {
            words.push(std::mem::take(&mut current).fold_lower());
        }
        current.push(ch);
    }
    if !current.is_empty() {
        words.push(current.fold_lower());
    }
    words
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::body::lower_method;
    use syntax::SyntaxKind;

    fn parse_lower(code: &str) -> Body {
        let parse = parser::parse(code);
        let root = parse.syntax_node();
        let method_node = root
            .descendants()
            .find(|n| matches!(n.kind(), SyntaxKind::FUNCTION_DEF | SyntaxKind::PROCEDURE_DEF))
            .expect("Should have a method");
        let is_function = method_node.kind() == SyntaxKind::FUNCTION_DEF;
        std::sync::Arc::unwrap_or_clone(lower_method(&method_node, is_function).body)
    }

    fn first_try_except(body: &Body) -> &[StmtIdx] {
        for &stmt_idx in body.body_stmts.iter() {
            if let Stmt::Try { except, .. } = body.stmt_idx(stmt_idx) {
                return except;
            }
        }
        &[]
    }

    #[test]
    fn empty_except_is_empty() {
        let body = parse_lower(
            r#"
Процедура Тест()
    Попытка
        Действие();
    Исключение
    КонецПопытки;
КонецПроцедуры
"#,
        );
        let except = first_try_except(&body);
        assert_eq!(classify_catch_body(&body, except), CatchBodyClass::Empty);
    }

    #[test]
    fn raise_only_is_raises_only() {
        let body = parse_lower(
            r#"
Процедура Тест()
    Попытка
        Действие();
    Исключение
        ВызватьИсключение;
    КонецПопытки;
КонецПроцедуры
"#,
        );
        let except = first_try_except(&body);
        assert_eq!(classify_catch_body(&body, except), CatchBodyClass::RaisesOnly);
    }

    #[test]
    fn message_call_is_logs_only() {
        let body = parse_lower(
            r#"
Процедура Тест()
    Попытка
        Действие();
    Исключение
        Сообщить("error");
    КонецПопытки;
КонецПроцедуры
"#,
        );
        let except = first_try_except(&body);
        assert_eq!(classify_catch_body(&body, except), CatchBodyClass::LogsOnly);
    }

    #[test]
    fn raise_plus_log_is_mixed() {
        let body = parse_lower(
            r#"
Процедура Тест()
    Попытка
        Действие();
    Исключение
        Сообщить("error");
        ВызватьИсключение;
    КонецПопытки;
КонецПроцедуры
"#,
        );
        let except = first_try_except(&body);
        assert_eq!(classify_catch_body(&body, except), CatchBodyClass::Mixed);
    }

    #[test]
    fn silent_swallow_is_silent() {
        let body = parse_lower(
            r#"
Процедура Тест()
    Попытка
        Действие();
    Исключение
        Х = 1;
    КонецПопытки;
КонецПроцедуры
"#,
        );
        let except = first_try_except(&body);
        assert_eq!(classify_catch_body(&body, except), CatchBodyClass::Silent);
    }

    #[test]
    fn rollback_only_is_rollback_only() {
        let body = parse_lower(
            r#"
Процедура Тест()
    Попытка
        Действие();
    Исключение
        ОтменитьТранзакцию();
    КонецПопытки;
КонецПроцедуры
"#,
        );
        let except = first_try_except(&body);
        assert_eq!(classify_catch_body(&body, except), CatchBodyClass::RollbackOnly);
    }

    #[test]
    fn rollback_plus_log_is_mixed() {
        let body = parse_lower(
            r#"
Процедура Тест()
    Попытка
        Действие();
    Исключение
        ОтменитьТранзакцию();
        Сообщить("error");
    КонецПопытки;
КонецПроцедуры
"#,
        );
        let except = first_try_except(&body);
        assert_eq!(classify_catch_body(&body, except), CatchBodyClass::Mixed);
    }

    #[test]
    fn qualified_logging_call_is_logs_only() {
        let body = parse_lower(
            r#"
Процедура Тест()
    Попытка
        Действие();
    Исключение
        ОбщегоНазначения.СообщитьПользователю("error");
    КонецПопытки;
КонецПроцедуры
"#,
        );
        let except = first_try_except(&body);
        assert_eq!(classify_catch_body(&body, except), CatchBodyClass::LogsOnly);
    }

    #[test]
    fn conditional_raise_in_if_is_raises_only() {
        let body = parse_lower(
            r#"
Процедура Тест()
    Попытка
        Действие();
    Исключение
        Если ИнформацияОбОшибке().Описание <> ТекстИсключенияДублирование Тогда
            ВызватьИсключение;
        КонецЕсли;
    КонецПопытки;
КонецПроцедуры
"#,
        );
        let except = first_try_except(&body);
        assert_eq!(classify_catch_body(&body, except), CatchBodyClass::RaisesOnly);
    }

    #[test]
    fn raise_in_else_branch_counts() {
        let body = parse_lower(
            r#"
Процедура Тест()
    Попытка
        Действие();
    Исключение
        Если ИзвестнаяОшибка Тогда
            Лог = 1;
        Иначе
            ВызватьИсключение;
        КонецЕсли;
    КонецПопытки;
КонецПроцедуры
"#,
        );
        let except = first_try_except(&body);
        assert_eq!(classify_catch_body(&body, except), CatchBodyClass::Mixed);
    }

    #[test]
    fn conditional_log_is_logs_only() {
        let body = parse_lower(
            r#"
Процедура Тест()
    Попытка
        Действие();
    Исключение
        Если НужноЛогировать Тогда
            Сообщить("error");
        КонецЕсли;
    КонецПопытки;
КонецПроцедуры
"#,
        );
        let except = first_try_except(&body);
        assert_eq!(classify_catch_body(&body, except), CatchBodyClass::LogsOnly);
    }

    #[test]
    fn raise_in_loop_counts() {
        let body = parse_lower(
            r#"
Процедура Тест()
    Попытка
        Действие();
    Исключение
        Для Каждого Ошибка Из Ошибки Цикл
            ВызватьИсключение;
        КонецЦикла;
    КонецПопытки;
КонецПроцедуры
"#,
        );
        let except = first_try_except(&body);
        assert_eq!(classify_catch_body(&body, except), CatchBodyClass::RaisesOnly);
    }

    #[test]
    fn conditional_swallow_is_still_silent() {
        let body = parse_lower(
            r#"
Процедура Тест()
    Попытка
        Действие();
    Исключение
        Если Условие Тогда
            Х = 1;
        КонецЕсли;
    КонецПопытки;
КонецПроцедуры
"#,
        );
        let except = first_try_except(&body);
        assert_eq!(classify_catch_body(&body, except), CatchBodyClass::Silent);
    }

    #[test]
    fn raise_inside_nested_try_body_does_not_count() {
        // The nested handler catches it before it can leave the outer one.
        let body = parse_lower(
            r#"
Процедура Тест()
    Попытка
        Действие();
    Исключение
        Попытка
            ВызватьИсключение;
        Исключение
        КонецПопытки;
    КонецПопытки;
КонецПроцедуры
"#,
        );
        let except = first_try_except(&body);
        assert_eq!(classify_catch_body(&body, except), CatchBodyClass::Silent);
    }

    #[test]
    fn raise_inside_nested_try_handler_counts() {
        let body = parse_lower(
            r#"
Процедура Тест()
    Попытка
        Действие();
    Исключение
        Попытка
            ЗаписатьОшибку();
        Исключение
            ВызватьИсключение;
        КонецПопытки;
    КонецПопытки;
КонецПроцедуры
"#,
        );
        let except = first_try_except(&body);
        assert_eq!(classify_catch_body(&body, except), CatchBodyClass::Mixed);
    }

    #[test]
    fn unknown_call_is_silent() {
        let body = parse_lower(
            r#"
Процедура Тест()
    Попытка
        Действие();
    Исключение
        ОбработатьОшибку();
    КонецПопытки;
КонецПроцедуры
"#,
        );
        let except = first_try_except(&body);
        assert_eq!(classify_catch_body(&body, except), CatchBodyClass::Silent);
    }

    #[test]
    fn named_log_helper_is_logs_only() {
        for callee in [
            "ЗаписатьОшибкуВЖурналРегистрации",
            "ДобавитьСообщениеДляЖурналаРегистрации",
            "СообщитьОПроблеме",
            "ПоказатьПредупреждение",
            "ОбработкаОшибок.ЗаписатьОшибку",
        ] {
            let code = format!(
                r#"
Процедура Тест()
    Попытка
        Действие();
    Исключение
        {callee}(ИнформацияОбОшибке());
    КонецПопытки;
КонецПроцедуры
"#
            );
            let body = parse_lower(&code);
            let except = first_try_except(&body);
            assert_eq!(
                classify_catch_body(&body, except),
                CatchBodyClass::LogsOnly,
                "callee `{callee}` should be recognized as logging"
            );
        }
    }

    fn class_of_except_call(try_kw: (&str, &str, &str), call: &str) -> CatchBodyClass {
        let (try_, except_, end_try) = try_kw;
        let code = format!(
            "Procedure Test(E, T)\n    {try_}\n        A = 1 / 0;\n    {except_}\n        {call};\n    {end_try};\nEndProcedure\n"
        );
        let body = parse_lower(&code);
        classify_catch_body(&body, first_try_except(&body))
    }

    const ENGLISH_KEYWORDS: (&str, &str, &str) = ("Try", "Except", "EndTry");
    const RUSSIAN_KEYWORDS: (&str, &str, &str) = ("Попытка", "Исключение", "КонецПопытки");

    #[test]
    fn issue_table_calls_are_logs_only_in_both_keyword_languages() {
        for keywords in [ENGLISH_KEYWORDS, RUSSIAN_KEYWORDS] {
            for call in [
                "WriteErrorToLog(E, T)",
                "WriteErrorToEventLog(E, T)",
                "LogError(E, T)",
                "EventLogClient.AddMessageForEventLog(E, \"Error\", T)",
                "WriteLogEvent(\"E\", EventLogLevel.Error, , , T)",
                "ЗаписатьОшибкуВЖурнал(E, T)",
                "ЖурналРегистрацииКлиент.ДобавитьСообщениеДляЖурналаРегистрации(E, \"Ошибка\", T)",
            ] {
                assert_eq!(
                    class_of_except_call(keywords, call),
                    CatchBodyClass::LogsOnly,
                    "`{call}` should be recognized as logging"
                );
            }
        }
    }

    #[test]
    fn english_log_helper_names_are_logs_only() {
        for call in [
            "ErrorHandler.LogError(E)",
            "RecordError(E)",
            "RegisterProblem(E)",
            "ReportError(E)",
            "NotifyAboutError(E)",
            "ShowWarning(E)",
            "SaveErrorToLog(E)",
            "write_error_to_log(E)",
            "WRITE_ERROR_TO_LOG(E)",
            "LogEvent(E)",
            "Logger.LogWarning(E)",
            "AddEventLogRecord(E)",
            "Write2Error(E)",
            "WriteLog2(E)",
        ] {
            assert_eq!(
                class_of_except_call(ENGLISH_KEYWORDS, call),
                CatchBodyClass::LogsOnly,
                "`{call}` should be recognized as logging"
            );
        }
    }

    #[test]
    fn log_inside_a_word_is_not_logging() {
        for call in
            ["AddToCatalog(E)", "ShowDialog(E)", "WriteDialog(E)", "SaveBlog(E)", "ShowAnalog(E)"]
        {
            assert_eq!(
                class_of_except_call(ENGLISH_KEYWORDS, call),
                CatchBodyClass::Silent,
                "`{call}` must not be recognized as logging"
            );
        }
    }

    #[test]
    fn english_verb_or_noun_alone_is_not_logging() {
        for call in [
            "Write(E)",
            "WriteFile(E)",
            "AddItem(E)",
            "ShowForm(E)",
            "GetErrorText(E)",
            "FormatError(E)",
            "ErrorDescription(E)",
            "LogLevel(E)",
            "AddressError(E)",
            "RegisterEventHandler(E)",
            "AddErrorHandler(E)",
            "ReadEventLog(E)",
            "GetEventLogLevel(E)",
            "CatalogError(E)",
        ] {
            assert_eq!(
                class_of_except_call(ENGLISH_KEYWORDS, call),
                CatchBodyClass::Silent,
                "`{call}` must not be recognized as logging"
            );
        }
    }

    #[test]
    fn error_formatter_only_stays_silent() {
        // A getter/formatter carries the error noun but no recording verb: the
        // handler still does nothing with the formatted error.
        let body = parse_lower(
            r#"
Процедура Тест()
    Попытка
        Действие();
    Исключение
        ПодробноеПредставлениеОшибки(ИнформацияОбОшибке());
    КонецПопытки;
КонецПроцедуры
"#,
        );
        let except = first_try_except(&body);
        assert_eq!(classify_catch_body(&body, except), CatchBodyClass::Silent);
    }

    #[test]
    fn error_message_constructor_statement_stays_silent() {
        // `Сообщение…` is a noun (constructor/getter), not the `Сообщить` verb:
        // a bare constructor statement still does nothing with the error.
        let body = parse_lower(
            r#"
Процедура Тест()
    Попытка
        Действие();
    Исключение
        СообщениеОбОшибке(ИнформацияОбОшибке());
    КонецПопытки;
КонецПроцедуры
"#,
        );
        let except = first_try_except(&body);
        assert_eq!(classify_catch_body(&body, except), CatchBodyClass::Silent);
    }

    #[test]
    fn error_accumulator_helper_is_logs_only() {
        // The `СписокОшибок`/`ДобавитьОшибкуПользователю` pattern is the standard
        // BSP way to surface collected errors, so it counts as reporting.
        let body = parse_lower(
            r#"
Процедура Тест()
    Попытка
        Действие();
    Исключение
        ДобавитьОшибкуПользователю(СписокОшибок, "Поле", ОписаниеОшибки(), Неопределено);
    КонецПопытки;
КонецПроцедуры
"#,
        );
        let except = first_try_except(&body);
        assert_eq!(classify_catch_body(&body, except), CatchBodyClass::LogsOnly);
    }

    #[test]
    fn rollback_plus_named_log_is_mixed() {
        let body = parse_lower(
            r#"
Процедура Тест()
    Попытка
        Действие();
    Исключение
        ОтменитьТранзакцию();
        ЗаписатьОшибкуВЖурналРегистрации(ИнформацияОбОшибке());
    КонецПопытки;
КонецПроцедуры
"#,
        );
        let except = first_try_except(&body);
        assert_eq!(classify_catch_body(&body, except), CatchBodyClass::Mixed);
    }

    #[test]
    fn local_event_wrapper_with_return_is_mixed() {
        // A module-local wrapper around the journal write: `Записать… Событие…`.
        // The call plus `Возврат` is a log and an other statement, so Mixed.
        let body = parse_lower(
            r#"
Процедура Тест()
    Попытка
        Действие();
    Исключение
        ЗаписатьСобытиеПодсистемы("Не разобрано: " + ПодробноеПредставлениеОшибки(ИнформацияОбОшибке()), УровеньЖурналаРегистрации.Ошибка);
        Возврат;
    КонецПопытки;
КонецПроцедуры
"#,
        );
        let except = first_try_except(&body);
        assert_eq!(classify_catch_body(&body, except), CatchBodyClass::Mixed);
    }

    #[test]
    fn local_event_wrapper_alone_is_logs_only() {
        let body = parse_lower(
            r#"
Процедура Тест()
    Попытка
        Действие();
    Исключение
        ЗафиксироватьСобытие(ИнформацияОбОшибке());
    КонецПопытки;
КонецПроцедуры
"#,
        );
        let except = first_try_except(&body);
        assert_eq!(classify_catch_body(&body, except), CatchBodyClass::LogsOnly);
    }

    #[test]
    fn unnamed_call_with_return_stays_silent() {
        let body = parse_lower(
            r#"
Процедура Тест()
    Попытка
        Действие();
    Исключение
        ОбработатьДанные(ИнформацияОбОшибке());
        Возврат;
    КонецПопытки;
КонецПроцедуры
"#,
        );
        let except = first_try_except(&body);
        assert_eq!(classify_catch_body(&body, except), CatchBodyClass::Silent);
    }

    #[test]
    fn verb_with_foreign_noun_stays_silent() {
        let body = parse_lower(
            r#"
Процедура Тест()
    Попытка
        Действие();
    Исключение
        ЗаписатьФайл(ИнформацияОбОшибке());
        Возврат;
    КонецПопытки;
КонецПроцедуры
"#,
        );
        let except = first_try_except(&body);
        assert_eq!(classify_catch_body(&body, except), CatchBodyClass::Silent);
    }
}
