use crate::state::SharedState;
use crate::tools::response::text_within_budget;
use rmcp::model::{CallToolResult, ContentBlock};
use rmcp::ErrorData as McpError;

/// Platform output (a run's context block, a syntax-error listing, an evaluated value) has no
/// size of its own, so every body that carries it goes out through the output budget.
const BUDGET_NOTE: &str =
    "\n-- вывод усечён под max_output_tokens; повысьте бюджет или сократите код/выражение --\n";

fn require_onec_connection(
    state: &SharedState,
    connection: Option<&str>,
) -> Result<crate::OnecConnection, McpError> {
    state.onec_connection(connection).map_err(|e| McpError::invalid_params(e, None))
}

#[allow(
    clippy::too_many_arguments,
    reason = "The check route needs independent source, backend selection, module context, budget, and cancellation inputs"
)]
pub async fn check_syntax(
    state: &SharedState,
    code: &str,
    connection: Option<&str>,
    max_output_tokens: usize,
    input_kind: check_contract::InputKind,
    module_type: Option<check_contract::ModuleType>,
    context: Option<check_contract::ModuleContext>,
    ct: tokio_util::sync::CancellationToken,
) -> Result<CallToolResult, McpError> {
    if code.trim().is_empty() {
        return Err(McpError::invalid_params("Пустой код", None));
    }

    let (selected_name, selected) =
        state.onec_connection_named(connection).map_err(|e| McpError::invalid_params(e, None))?;
    if input_kind == check_contract::InputKind::Snippet {
        if module_type.is_some() || context.is_some() {
            return Err(McpError::invalid_params(
                "snippet cannot include module_type or context",
                None,
            ));
        }
        if let Some(profile) = selected.check_executor() {
            if profile.handles_snippet {
                return crate::tools::check_executor::check(
                    profile,
                    &selected_name,
                    code,
                    input_kind,
                    module_type,
                    context,
                    ct,
                )
                .await;
            }
        }
        if crate::tools::check_executor::has_module_syntax(code) {
            return Ok(crate::tools::check_executor::context_required(
                &selected_name,
                selected.check_executor().is_some()
                    || (selected.epf_module_check() && selected.http_client().is_some()),
            ));
        }
    }
    if input_kind == check_contract::InputKind::Module {
        if let Some(profile) = selected.check_executor() {
            return crate::tools::check_executor::check(
                profile,
                &selected_name,
                code,
                input_kind,
                module_type,
                context,
                ct,
            )
            .await;
        }
        if selected.epf_module_check() {
            if let Some(client) = selected.http_client() {
                return super::epf_module_check::check(
                    client,
                    &selected_name,
                    code,
                    module_type,
                    context,
                    max_output_tokens,
                    ct,
                )
                .await;
            }
        }
        return Ok(crate::tools::check_executor::unsupported(&selected_name));
    }
    let Some(client) = selected.http_client() else {
        return Ok(crate::tools::check_executor::unsupported(&selected_name));
    };

    let request = onec_client::CheckSyntaxRequest { code: code.to_string() };

    let result = client.check_syntax(&request).await.map_err(|e| {
        McpError::internal_error(format!("Ошибка проверки синтаксиса в 1С: {e}"), None)
    })?;

    let request_id = crate::tools::check_executor::new_request_id();
    if result.valid {
        Ok(crate::tools::check_executor::http_valid(&request_id, &selected_name))
    } else {
        let error = result.error.unwrap_or_default();
        let mut response = crate::tools::check_executor::http_position_unavailable(
            &request_id,
            &selected_name,
            &error,
        );
        let mut text = format!("✗ Ошибка синтаксиса:\n{error}");
        crate::tools::response::truncate_text_to_budget(&mut text, max_output_tokens, BUDGET_NOTE);
        response.content = vec![ContentBlock::text(text)];
        Ok(response)
    }
}

pub async fn execute_code(
    state: &SharedState,
    code: &str,
    connection: Option<&str>,
    max_output_tokens: usize,
) -> Result<CallToolResult, McpError> {
    let selected = require_onec_connection(state, connection)?;
    let client = selected
        .http_client()
        .ok_or_else(|| McpError::invalid_params("This 1C connection has no HTTP backend", None))?;
    if !selected.allow_execute() {
        return Err(McpError::invalid_params("BSL run is disabled for this 1C connection", None));
    }

    if code.trim().is_empty() {
        return Err(McpError::invalid_params("Пустой код", None));
    }

    let request = onec_client::ExecuteRequest { code: code.to_string() };

    let result = match client.execute_code(&request).await {
        Ok(result) => result,
        Err(onec_client::Error::UnknownExecutionState(error)) => {
            return Ok(unknown_execution_result(error.is_timeout(), max_output_tokens));
        }
        Err(error) => {
            return Err(McpError::internal_error(
                format!("Ошибка выполнения кода в 1С: {error}"),
                None,
            ))
        }
    };

    Ok(format_execute_result(result, max_output_tokens))
}

fn unknown_execution_result(timed_out: bool, max_output_tokens: usize) -> CallToolResult {
    let body = serde_json::json!({
        "status": "unknown_execution_state",
        "success": null,
        "failure": { "code": if timed_out { "timeout" } else { "transport_error" } },
        "retry_performed": false,
    });
    let mut text = "Исход выполнения неизвестен: HTTP-ожидание прервано. Операция могла продолжиться на сервере. Проверьте её результат перед повторным запуском; автоматический повтор не выполнялся.".to_owned();
    let text_tokens = max_output_tokens
        .saturating_sub(crate::tools::response::serialized_bytes(&body).div_ceil(4));
    crate::tools::response::truncate_text_to_budget(
        &mut text,
        text_tokens,
        "Исход неизвестен; автоматического повтора нет.",
    );
    let mut response = crate::tools::response::structured_with_text(text, body);
    response.is_error = Some(true);
    response
}

fn execute_text(result: &onec_client::ExecuteResult, truncated: bool) -> String {
    let mut out = if result.success {
        "✓ Код выполнен успешно".to_string()
    } else {
        let error = result.error.as_deref().unwrap_or_default();
        format!("✗ Ошибка выполнения:\n{error}")
    };

    if let Some(ms) = result.duration_ms {
        out.push_str(&format!("\nВремя: {ms} мс"));
    }

    match &result.messages {
        Some(messages) if !messages.is_empty() => {
            out.push_str("\n\nСообщения:\n");
            for message in messages {
                out.push_str(&message.text);
                out.push('\n');
            }
        }
        None => out.push_str("\nКанал сообщений недоступен в этой версии HTTP-сервиса."),
        _ => {}
    }

    if let Some(ref ctx) = result.context {
        if !ctx.is_empty() {
            out.push_str("\n\n## Контекст\n");
            for (key, value) in ctx {
                let v = match value {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                out.push_str(&format!("- **{key}**: {v}\n"));
            }
        }
    }

    if truncated {
        out.push_str(BUDGET_NOTE);
    }
    out
}

fn format_execute_result(
    mut result: onec_client::ExecuteResult,
    max_output_tokens: usize,
) -> CallToolResult {
    let mut truncated = result.messages_truncated;
    let budget = max_output_tokens.saturating_mul(4);
    loop {
        let text = execute_text(&result, truncated);
        let mut body = serde_json::to_value(&result).expect("execution DTO is serializable");
        body["truncated"] = truncated.into();
        if text.len() + crate::tools::response::serialized_bytes(&body) <= budget {
            let mut response = crate::tools::response::structured_with_text(text, body);
            response.is_error = Some(!result.success);
            return response;
        }
        truncated = true;
        if let Some(context) = result.context.as_mut().filter(|context| !context.is_empty()) {
            let last = context.keys().next_back().cloned().unwrap();
            context.remove(&last);
        } else if let Some(messages) =
            result.messages.as_mut().filter(|messages| !messages.is_empty())
        {
            result.messages_truncated = true;
            if messages.len() > 1 {
                messages.pop();
            } else if !messages[0].text.is_empty() {
                halve_text(&mut messages[0].text);
            } else {
                messages.clear();
            }
        } else if let Some(error) = result.error.as_mut().filter(|error| !error.is_empty()) {
            halve_text(error);
        } else {
            // The status and explicit truncation marker are the floor for very small budgets.
            let text = if result.success {
                "Выполнено; вывод усечён."
            } else {
                "Ошибка; вывод усечён."
            };
            let body = serde_json::json!({"success":result.success,"truncated":true});
            let mut response = crate::tools::response::structured_with_text(text.into(), body);
            response.is_error = Some(!result.success);
            return response;
        }
    }
}

fn halve_text(text: &mut String) {
    let mut end = text.len() / 2;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
}

pub async fn eval_expression(
    state: &SharedState,
    expression: &str,
    connection: Option<&str>,
    max_output_tokens: usize,
) -> Result<CallToolResult, McpError> {
    let selected = require_onec_connection(state, connection)?;
    let client = selected
        .http_client()
        .ok_or_else(|| McpError::invalid_params("This 1C connection has no HTTP backend", None))?;
    if !selected.allow_execute() {
        return Err(McpError::invalid_params("BSL eval is disabled for this 1C connection", None));
    }

    if expression.trim().is_empty() {
        return Err(McpError::invalid_params("Пустое выражение", None));
    }

    let request = onec_client::EvalRequest { expression: expression.to_string() };

    let result = client.eval_expression(&request).await.map_err(|e| {
        McpError::internal_error(format!("Ошибка вычисления выражения в 1С: {e}"), None)
    })?;

    if result.success {
        let value = match &result.result {
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(v) => v.to_string(),
            None => "Неопределено".to_string(),
        };
        let type_name = result.type_name.unwrap_or_default();
        Ok(CallToolResult::success(vec![ContentBlock::text(format_eval_result(
            &value,
            &type_name,
            max_output_tokens,
        ))]))
    } else {
        let error = result.error.unwrap_or_default();
        Ok(text_within_budget(
            format!("✗ Ошибка вычисления:\n{error}"),
            max_output_tokens,
            BUDGET_NOTE,
        ))
    }
}

/// A serialized value (a structure, a value table) has no bound of its own, so it goes out
/// through the budget. The value alone is clipped, with the type line and the note reserved
/// out of the budget first: a clipped value must not cost the agent the one line that says
/// what it was looking at.
fn format_eval_result(value: &str, type_name: &str, max_output_tokens: usize) -> String {
    let tail = format!("\nТип: {type_name}");
    // Everything but the value itself — the label, the type line, the note — is reserved out
    // of the budget, so the composed body stays inside it.
    let reserved = (format!("✓ Результат: {tail}").len() + BUDGET_NOTE.len()).div_ceil(4);
    let mut clipped = value.to_string();
    let cut = crate::tools::response::truncate_text_to_budget(
        &mut clipped,
        max_output_tokens.saturating_sub(reserved).max(1),
        " …",
    );
    let mut out = format!("✓ Результат: {clipped}{tail}");
    // Hard ceiling on the composed body: an absurdly long type name can blow the budget even
    // when the value itself fits.
    let ceiling_hit =
        crate::tools::response::truncate_text_to_budget(&mut out, max_output_tokens, BUDGET_NOTE);
    if cut && !ceiling_hit {
        out.push_str(BUDGET_NOTE);
    }
    out
}

#[cfg(test)]
fn test_shared_state() -> SharedState {
    SharedState::shared()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_producer_module_parses_with_message_collection() {
        let parsed = parser::parse(include_str!(
            "../../../../extension/src/HTTPServices/BSLAnalyzerService/Ext/Module.bsl"
        ));
        assert!(parsed.errors().is_empty(), "{:?}", parsed.errors());
    }

    #[test]
    fn run_transport_failure_distinguishes_timeout_from_unknown_execution_outcome() {
        for (timed_out, code) in [(true, "timeout"), (false, "transport_error")] {
            let response = unknown_execution_result(timed_out, 250);
            assert_eq!(response.is_error, Some(true));
            let body = response.structured_content.unwrap();
            assert_eq!(body["status"], "unknown_execution_state");
            assert_eq!(body["success"], serde_json::Value::Null);
            assert_eq!(body["failure"]["code"], code);
            assert_eq!(body["retry_performed"], false);
            assert!(
                response.content[0].as_text().unwrap().text.len()
                    + crate::tools::response::serialized_bytes(&body)
                    <= 1000
            );
        }
    }

    fn run_result(
        success: bool,
        messages: Option<Vec<onec_client::ExecuteMessage>>,
    ) -> onec_client::ExecuteResult {
        onec_client::ExecuteResult {
            success,
            error: (!success).then(|| "Ошибка после сообщения".into()),
            context: None,
            duration_ms: Some(1),
            messages,
            messages_truncated: false,
        }
    }

    #[test]
    fn run_messages_and_failure_remain_ordered_and_separate_in_both_outputs() {
        for success in [true, false] {
            let result = format_execute_result(
                run_result(
                    success,
                    Some(vec![
                        onec_client::ExecuteMessage { text: "Первое".into() },
                        onec_client::ExecuteMessage { text: "Второе".into() },
                    ]),
                ),
                6000,
            );
            assert_eq!(result.is_error, Some(!success));
            let body = result.structured_content.unwrap();
            assert_eq!(body["success"], success);
            assert_eq!(body["messages"][0]["text"], "Первое");
            assert_eq!(body["messages"][1]["text"], "Второе");
            let text = result.content[0].as_text().unwrap().text.as_str();
            assert!(text.find("Первое").unwrap() < text.find("Второе").unwrap());
            assert_eq!(text.contains("Ошибка после сообщения"), !success);
        }
    }

    #[test]
    fn run_output_fits_combined_budget_and_preserves_failure_and_truncation() {
        let result = format_execute_result(
            run_result(false, Some(vec![onec_client::ExecuteMessage { text: "я".repeat(10_000) }])),
            250,
        );
        assert_eq!(result.is_error, Some(true));
        let body = result.structured_content.unwrap();
        assert_eq!(body["success"], false);
        assert_eq!(body["messages_truncated"], true);
        assert_eq!(body["truncated"], true);
        let text = &result.content[0].as_text().unwrap().text;
        assert!(text.contains("Ошибка после сообщения"));
        assert!(text.ends_with(BUDGET_NOTE));
        assert!(text.len() + crate::tools::response::serialized_bytes(&body) <= 1000);
    }

    #[test]
    fn run_legacy_and_supported_empty_messages_are_distinct() {
        let legacy = format_execute_result(run_result(true, None), 6000);
        assert!(legacy.structured_content.unwrap()["messages"].is_null());
        assert!(legacy.content[0].as_text().unwrap().text.contains("недоступен"));
        let empty = format_execute_result(run_result(true, Some(vec![])), 6000);
        assert_eq!(empty.structured_content.unwrap()["messages"], serde_json::json!([]));
        assert!(!empty.content[0].as_text().unwrap().text.contains("недоступен"));
        let mut producer_cut = run_result(true, Some(vec![]));
        producer_cut.messages_truncated = true;
        let result = format_execute_result(producer_cut, 6000);
        assert_eq!(result.structured_content.unwrap()["truncated"], true);
    }

    #[tokio::test]
    async fn optional_http_only_connection_routes_preflight_and_unsupported_without_fallback() {
        let mut state = test_shared_state();
        state.add_onec_connection(
            "http-only".to_owned(),
            crate::state::OnecConnection::with_optional_http(None, false, None),
        );
        let preflight = check_syntax(
            &state,
            "&НаСервере\nПроцедура Проверка()\nКонецПроцедуры",
            Some("http-only"),
            6000,
            check_contract::InputKind::Snippet,
            None,
            None,
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .expect("preflight returns a structured caller response");
        let response: check_contract::CheckResponse =
            serde_json::from_value(preflight.structured_content.unwrap()).unwrap();
        assert_eq!(response.status, check_contract::Status::ContextRequired);
        let failure = response.failure.unwrap();
        assert_eq!(failure.code, "context_required");
        assert!(
            !failure.message.contains("retry with input_kind=module"),
            "a connection without an executor cannot take the module retry: {}",
            failure.message
        );

        for (code, kind) in [
            ("x = 1;", check_contract::InputKind::Snippet),
            ("Перем x;", check_contract::InputKind::Module),
        ] {
            let result = check_syntax(
                &state,
                code,
                Some("http-only"),
                6000,
                kind,
                None,
                None,
                tokio_util::sync::CancellationToken::new(),
            )
            .await
            .expect("missing backend is a structured result");
            let response: check_contract::CheckResponse =
                serde_json::from_value(result.structured_content.unwrap()).unwrap();
            assert_eq!(response.status, check_contract::Status::Unsupported);
            assert_eq!(response.failure.unwrap().code, "unsupported");
        }
    }

    #[tokio::test]
    async fn test_check_syntax_empty_code() {
        let state = test_shared_state();
        let result = check_syntax(
            &state,
            "",
            None,
            6000,
            check_contract::InputKind::Snippet,
            None,
            None,
            tokio_util::sync::CancellationToken::new(),
        )
        .await;
        assert!(result.is_err(), "empty code should fail");
    }

    #[tokio::test]
    async fn test_check_syntax_whitespace() {
        let state = test_shared_state();
        let result = check_syntax(
            &state,
            "   ",
            None,
            6000,
            check_contract::InputKind::Snippet,
            None,
            None,
            tokio_util::sync::CancellationToken::new(),
        )
        .await;
        assert!(result.is_err(), "whitespace-only code should fail");
    }

    #[tokio::test]
    async fn test_check_syntax_no_client() {
        let state = test_shared_state();
        let result = check_syntax(
            &state,
            "а = 1;",
            None,
            6000,
            check_contract::InputKind::Snippet,
            None,
            None,
            tokio_util::sync::CancellationToken::new(),
        )
        .await;
        assert!(result.is_err(), "should fail without onec client");
    }

    #[tokio::test]
    async fn test_execute_code_empty() {
        let state = test_shared_state();
        let result = execute_code(&state, "", None, 6000).await;
        assert!(result.is_err(), "empty code should fail");
    }

    #[tokio::test]
    async fn test_execute_code_no_client() {
        let state = test_shared_state();
        let result = execute_code(&state, "Сообщить(1);", None, 6000).await;
        assert!(result.is_err(), "should fail without onec client");
    }

    #[tokio::test]
    async fn test_eval_expression_empty() {
        let state = test_shared_state();
        let result = eval_expression(&state, "", None, 6000).await;
        assert!(result.is_err(), "empty expression should fail");
    }

    #[tokio::test]
    async fn test_eval_expression_no_client() {
        let state = test_shared_state();
        let result = eval_expression(&state, "1 + 1", None, 6000).await;
        assert!(result.is_err(), "should fail without onec client");
    }

    #[test]
    fn eval_result_within_budget_is_untouched() {
        let out = format_eval_result("42", "Число", 6000);
        assert_eq!(out, "✓ Результат: 42\nТип: Число");
    }

    #[test]
    fn eval_clips_a_huge_value_but_never_the_type_line() {
        let out = format_eval_result(&"я".repeat(10_000), "ТаблицаЗначений", 200);
        assert!(out.contains("\nТип: ТаблицаЗначений"), "type line must survive: {out}");
        assert!(out.ends_with(BUDGET_NOTE), "must say it clipped: {out}");
        assert!(out.len() <= 200 * 4, "must stay inside the budget: {}", out.len());
    }
}
