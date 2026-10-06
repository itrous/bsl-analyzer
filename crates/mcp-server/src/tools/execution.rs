use crate::state::SharedState;
use crate::tools::response::text_within_budget;
use rmcp::model::{CallToolResult, ContentBlock};
use rmcp::ErrorData as McpError;
use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::native_check::types::{
    CheckContext, CheckFailure, CheckIssue, CheckRequest, CheckResult, CheckStatus, CleanupStatus,
    CompilationStatus, CompilerInfo, InputKind,
};

/// Platform output (a run's context block, a syntax-error listing, an evaluated value) has no
/// size of its own, so every body that carries it goes out through the output budget.
const BUDGET_NOTE: &str =
    "\n-- вывод усечён под max_output_tokens; повысьте бюджет или сократите код/выражение --\n";
const MIN_NATIVE_CHECK_OUTPUT_TOKENS: usize = 512;

fn require_onec_connection(
    state: &SharedState,
    connection: Option<&str>,
) -> Result<crate::OnecConnection, McpError> {
    state.onec_connection(connection).map_err(|e| McpError::invalid_params(e, None))
}

pub async fn check_syntax(
    state: &SharedState,
    request: &CheckRequest,
    code: &str,
    connection: Option<&str>,
    cancel: CancellationToken,
    max_output_tokens: usize,
) -> Result<CallToolResult, McpError> {
    if code.trim().is_empty() {
        return Err(McpError::invalid_params("Пустой код", None));
    }
    request.validate().map_err(|message| McpError::invalid_params(message, None))?;

    let minimum_output_tokens = minimum_native_check_output_tokens(request);
    if max_output_tokens < minimum_output_tokens {
        return Err(McpError::invalid_params(
            format!(
                "output_budget_too_small: max_output_tokens must be at least {minimum_output_tokens}"
            ),
            None,
        ));
    }

    let selected = require_onec_connection(state, connection)?;

    if request.input_kind == InputKind::Module
        && (request.module_type.is_none() || request.context.is_none())
    {
        let mut result = CheckResult::new(CheckStatus::ContextRequired);
        if request.module_type.is_none() {
            result.contexts_required.push("module_type".to_string());
        }
        if request.context.is_none() {
            result.contexts_required.push("context".to_string());
        }
        result.module_type = request.module_type;
        result.context = request.context.clone();
        result.owner = match request.context.as_ref() {
            Some(CheckContext::Metadata { owner, .. }) => Some(owner.clone()),
            _ => None,
        };
        return Ok(native_check_response(result, request.input_kind, max_output_tokens));
    }

    let Some(profile) = selected.native_check_profile() else {
        let mut result = CheckResult::new(CheckStatus::Unsupported);
        result.failure = Some(CheckFailure {
            code: "native_profile_required".to_string(),
            message: "This connection has no native module compiler profile".to_string(),
        });
        result.module_type = request.module_type;
        result.context = request.context.clone();
        result.owner = match request.context.as_ref() {
            Some(CheckContext::Metadata { owner, .. }) => Some(owner.clone()),
            _ => None,
        };
        return Ok(native_check_response(result, request.input_kind, max_output_tokens));
    };

    let result =
        crate::native_check::check(selected.client(), &profile, request, code, cancel).await;
    Ok(native_check_response(result, request.input_kind, max_output_tokens))
}

fn native_check_text(result: &CheckResult, input_kind: InputKind) -> String {
    if input_kind == InputKind::Snippet {
        match result.status {
            CheckStatus::Valid => return "✓ Синтаксис корректен".to_string(),
            CheckStatus::Invalid => {
                let mut text = format!(
                    "✗ Ошибка синтаксиса:\n{}",
                    result.issues.first().map_or("", |issue| issue.message.as_str())
                );
                if let Some(issue) = result.issues.first() {
                    match (issue.line, issue.column) {
                        (Some(line), Some(column)) => {
                            text.push_str(&format!("\nСтрока: {line}, колонка: {column}"));
                        }
                        (Some(line), None) => text.push_str(&format!("\nСтрока: {line}")),
                        (None, Some(column)) => {
                            text.push_str(&format!("\nКолонка: {column}"));
                        }
                        (None, None) => {}
                    }
                }
                return text;
            }
            _ => {}
        }
    }

    match result.status {
        CheckStatus::Valid => "✓ Native syntax check passed".to_string(),
        CheckStatus::Invalid => "✗ Native syntax check found an error".to_string(),
        CheckStatus::ContextRequired => "Native syntax check needs module context".to_string(),
        CheckStatus::Unsupported => {
            "Native syntax check is unavailable for this connection".to_string()
        }
        CheckStatus::Error => "Native syntax check failed".to_string(),
    }
}

fn native_check_response(
    mut result: CheckResult,
    input_kind: InputKind,
    max_output_tokens: usize,
) -> CallToolResult {
    let budget = max_output_tokens.saturating_mul(4);
    let text = loop {
        let text = native_check_text(&result, input_kind);
        if native_check_response_bytes(&result, &text) <= budget {
            break text;
        }
        result.truncated = true;
        if result.issues.len() > 1 {
            result.issues.pop();
            continue;
        }
        if let Some(issue) = result.issues.first_mut() {
            if !issue.message.is_empty() {
                issue
                    .message
                    .truncate(previous_char_boundary(&issue.message, issue.message.len() / 2));
                continue;
            }
        }
        if let Some(failure) = result.failure.as_mut() {
            if failure.message.len() > 24 {
                failure
                    .message
                    .truncate(previous_char_boundary(&failure.message, failure.message.len() / 2));
                continue;
            }
        }
        let compiler_shortened = result.compiler.as_mut().is_some_and(|compiler| {
            shorten_optional(&mut compiler.build)
                || shorten_optional(&mut compiler.compatibility)
                || shorten_optional(&mut compiler.fingerprint)
        });
        if compiler_shortened {
            continue;
        }
        // The fixed contract envelope itself must fit; the early floor keeps this unreachable.
        break text;
    };
    let body = serde_json::to_value(&result).unwrap_or_default();
    let mut response = CallToolResult::success(vec![ContentBlock::text(text)]);
    response.structured_content = Some(body);
    response
}

fn native_check_response_bytes<T: Serialize>(result: &T, text: &str) -> usize {
    serde_json::to_vec(result).map_or(usize::MAX, |body| body.len().saturating_add(text.len()))
}

fn minimum_native_check_output_tokens(request: &CheckRequest) -> usize {
    let mut result = CheckResult::new(CheckStatus::Invalid);
    result.valid = Some(false);
    result.compilation_status = Some(CompilationStatus::Invalid);
    result.contexts_required = vec!["x".repeat(32); 5];
    result.contexts_checked = vec!["x".repeat(32); 5];
    result.module_type = request.module_type;
    result.context = request.context.clone();
    result.owner = match request.context.as_ref() {
        Some(CheckContext::Metadata { owner, .. }) => Some(owner.clone()),
        _ => None,
    };
    result.compiler = Some(CompilerInfo {
        build: Some("x".repeat(48)),
        compatibility: Some("x".repeat(32)),
        fingerprint: Some("x".repeat(64)),
    });
    result.issues.push(CheckIssue {
        // Control bytes exercise JSON's worst-case escaping while the snippet text includes
        // the same bounded diagnostic once more for the legacy text contract.
        message: "\u{0001}".repeat(256),
        line: Some(u32::MAX),
        column: Some(u32::MAX),
    });
    result.cleanup_status = CleanupStatus::NotStarted;
    result.failure = Some(CheckFailure { code: "x".repeat(32), message: "x".repeat(256) });

    let bytes =
        native_check_response_bytes(&result, &native_check_text(&result, request.input_kind));
    bytes.div_ceil(4).saturating_add(32).max(MIN_NATIVE_CHECK_OUTPUT_TOKENS)
}

fn previous_char_boundary(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn shorten_optional(value: &mut Option<String>) -> bool {
    let Some(value) = value else { return false };
    if value.len() <= 24 {
        return false;
    }
    value.truncate(previous_char_boundary(value, value.len() / 2));
    true
}

pub async fn execute_code(
    state: &SharedState,
    code: &str,
    connection: Option<&str>,
    max_output_tokens: usize,
) -> Result<CallToolResult, McpError> {
    let selected = require_onec_connection(state, connection)?;
    if !selected.allow_execute() {
        return Err(McpError::invalid_params("BSL run is disabled for this 1C connection", None));
    }

    if code.trim().is_empty() {
        return Err(McpError::invalid_params("Пустой код", None));
    }

    let request = onec_client::ExecuteRequest { code: code.to_string() };

    let result =
        selected.client().execute_code(&request).await.map_err(|e| {
            McpError::internal_error(format!("Ошибка выполнения кода в 1С: {e}"), None)
        })?;

    let mut out = if result.success {
        "✓ Код выполнен успешно".to_string()
    } else {
        let error = result.error.unwrap_or_default();
        format!("✗ Ошибка выполнения:\n{error}")
    };

    if let Some(ms) = result.duration_ms {
        out.push_str(&format!("\nВремя: {ms} мс"));
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

    Ok(text_within_budget(out, max_output_tokens, BUDGET_NOTE))
}

pub async fn eval_expression(
    state: &SharedState,
    expression: &str,
    connection: Option<&str>,
    max_output_tokens: usize,
) -> Result<CallToolResult, McpError> {
    let selected = require_onec_connection(state, connection)?;
    if !selected.allow_execute() {
        return Err(McpError::invalid_params("BSL eval is disabled for this 1C connection", None));
    }

    if expression.trim().is_empty() {
        return Err(McpError::invalid_params("Пустое выражение", None));
    }

    let request = onec_client::EvalRequest { expression: expression.to_string() };

    let result = selected.client().eval_expression(&request).await.map_err(|e| {
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

    fn native_profile_for_execution_route_test() -> crate::native_check::NativeProfile {
        serde_json::from_value(serde_json::json!({
            "designer_path": "/unused/designer",
            "python_path": "/unused/python3",
            "expected_build": "test-build",
            "work_root": "/unused/work",
            "source_connection_env": "TEST_SOURCE",
            "user_env": "TEST_USER"
        }))
        .unwrap()
    }

    fn read_mock_http_request(stream: &mut std::net::TcpStream) -> (String, serde_json::Value) {
        use std::io::Read;

        let mut bytes = Vec::new();
        let header_end = loop {
            if let Some(position) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                break position;
            }
            let mut chunk = [0; 4096];
            let read = stream.read(&mut chunk).unwrap();
            assert_ne!(read, 0, "client closed before sending HTTP headers");
            bytes.extend_from_slice(&chunk[..read]);
        };
        let headers = String::from_utf8_lossy(&bytes[..header_end]).into_owned();
        let content_length = headers
            .lines()
            .find_map(|line| {
                line.split_once(':')
                    .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                    .and_then(|(_, value)| value.trim().parse::<usize>().ok())
            })
            .expect("JSON POST has a content length");
        let body_start = header_end + 4;
        while bytes.len() < body_start + content_length {
            let mut chunk = [0; 4096];
            let read = stream.read(&mut chunk).unwrap();
            assert_ne!(read, 0, "client closed before sending the JSON body");
            bytes.extend_from_slice(&chunk[..read]);
        }
        let path = headers
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .expect("HTTP request line has a path")
            .to_owned();
        let body = serde_json::from_slice(&bytes[body_start..body_start + content_length]).unwrap();
        (path, body)
    }

    #[tokio::test]
    async fn execute_and_eval_keep_http_routes_dtos_and_allow_execute_gate() {
        use std::io::Write;
        use std::net::TcpListener;
        use std::time::Duration;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for _ in 0..4 {
                let (mut stream, _) = listener.accept().unwrap();
                stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                let (path, body) = read_mock_http_request(&mut stream);
                let response = match path.as_str() {
                    "/execute" => {
                        serde_json::json!({"success": true, "error": null,
                            "context": {"route": "execute"}, "duration_ms": 2})
                    }
                    "/eval" => {
                        serde_json::json!({"success": true, "result": "42",
                            "type": "Number", "error": null})
                    }
                    unexpected => panic!("unexpected HTTP path: {unexpected}"),
                };
                let response = serde_json::to_vec(&response).unwrap();
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    response.len()
                )
                .unwrap();
                stream.write_all(&response).unwrap();
                requests.push((path, body));
            }
            requests
        });

        let mut state = test_shared_state();
        let legacy_client = onec_client::Client::new(&base_url, "", "");
        state.add_onec_connection(
            "legacy".to_owned(),
            crate::state::OnecConnection::new(legacy_client.clone(), true),
        );
        state.add_onec_connection(
            "native".to_owned(),
            crate::state::OnecConnection::new_with_native_check(
                onec_client::Client::new(&base_url, "", ""),
                true,
                native_profile_for_execution_route_test(),
            ),
        );
        state.add_onec_connection(
            "disabled-legacy".to_owned(),
            crate::state::OnecConnection::new(legacy_client.clone(), false),
        );
        state.add_onec_connection(
            "disabled-native".to_owned(),
            crate::state::OnecConnection::new_with_native_check(
                legacy_client,
                false,
                native_profile_for_execution_route_test(),
            ),
        );

        let native_code = "Сообщить(\"native\");";
        let legacy_code = "Сообщить(\"legacy\");";
        let native_expression = "42 + 1";
        let legacy_expression = "40 + 2";
        let native_run = execute_code(&state, native_code, Some("native"), 6000).await.unwrap();
        assert!(native_run
            .content
            .iter()
            .any(|block| { block.as_text().is_some_and(|text| text.text.contains("route")) }));
        let native_eval =
            eval_expression(&state, native_expression, Some("native"), 6000).await.unwrap();
        assert!(native_eval.content.iter().any(|block| {
            block.as_text().is_some_and(|text| text.text == "✓ Результат: 42\nТип: Number")
        }));
        execute_code(&state, legacy_code, Some("legacy"), 6000).await.unwrap();
        eval_expression(&state, legacy_expression, Some("legacy"), 6000).await.unwrap();

        for connection in ["disabled-legacy", "disabled-native"] {
            let run_error =
                execute_code(&state, "Сообщить(1);", Some(connection), 6000).await.unwrap_err();
            assert_eq!(run_error.message, "BSL run is disabled for this 1C connection");
            let eval_error =
                eval_expression(&state, "1 + 1", Some(connection), 6000).await.unwrap_err();
            assert_eq!(eval_error.message, "BSL eval is disabled for this 1C connection");
        }

        let requests = server.join().unwrap();
        assert_eq!(
            requests,
            vec![
                ("/execute".to_owned(), serde_json::json!({"code": native_code})),
                ("/eval".to_owned(), serde_json::json!({"expression": native_expression})),
                ("/execute".to_owned(), serde_json::json!({"code": legacy_code})),
                ("/eval".to_owned(), serde_json::json!({"expression": legacy_expression})),
            ]
        );
    }

    #[tokio::test]
    async fn test_check_syntax_empty_code() {
        let state = test_shared_state();
        let result = check_syntax(
            &state,
            &CheckRequest::default(),
            "",
            None,
            CancellationToken::new(),
            6000,
        )
        .await;
        assert_eq!(result.unwrap_err().message, "Пустой код");
    }

    #[tokio::test]
    async fn test_check_syntax_whitespace() {
        let state = test_shared_state();
        let result = check_syntax(
            &state,
            &CheckRequest::default(),
            "   ",
            None,
            CancellationToken::new(),
            6000,
        )
        .await;
        assert_eq!(result.unwrap_err().message, "Пустой код");
    }

    #[tokio::test]
    async fn test_check_syntax_no_client() {
        let state = test_shared_state();
        let result = check_syntax(
            &state,
            &CheckRequest::default(),
            "а = 1;",
            None,
            CancellationToken::new(),
            6000,
        )
        .await;
        assert!(result.is_err(), "should fail without onec client");
    }

    #[tokio::test]
    async fn legacy_http_only_connection_returns_unsupported_without_http_call() {
        let mut state = test_shared_state();
        state.set_onec_client(onec_client::Client::new("http://127.0.0.1/no-service", "", ""));
        let response = check_syntax(
            &state,
            &CheckRequest::default(),
            "Сообщить(1);",
            None,
            CancellationToken::new(),
            6000,
        )
        .await
        .unwrap();
        let body = response.structured_content.expect("structured native-check result");
        assert_eq!(body["status"], "unsupported");
        assert_eq!(body["failure"]["code"], "native_profile_required");
    }

    #[tokio::test]
    async fn unknown_connection_is_rejected_before_check() {
        let mut state = test_shared_state();
        state.set_onec_client(onec_client::Client::new("http://127.0.0.1/no-service", "", ""));
        let result = check_syntax(
            &state,
            &CheckRequest::default(),
            "Сообщить(1);",
            Some("missing"),
            CancellationToken::new(),
            6000,
        )
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn too_small_budget_is_rejected_before_native_check() {
        let mut state = test_shared_state();
        state.set_onec_client(onec_client::Client::new("http://127.0.0.1/no-service", "", ""));
        let error = check_syntax(
            &state,
            &CheckRequest::default(),
            "Сообщить(1);",
            None,
            CancellationToken::new(),
            511,
        )
        .await
        .unwrap_err();
        assert!(error.message.contains("output_budget_too_small"));
    }

    #[tokio::test]
    async fn module_without_context_returns_context_required() {
        let mut state = test_shared_state();
        state.set_onec_client(onec_client::Client::new("http://127.0.0.1/no-service", "", ""));
        let request = CheckRequest {
            input_kind: InputKind::Module,
            module_type: Some(crate::native_check::types::ModuleType::Common),
            context: None,
        };
        let response = check_syntax(
            &state,
            &request,
            "Процедура М()\nКонецПроцедуры",
            None,
            CancellationToken::new(),
            6000,
        )
        .await
        .unwrap();
        let body = response.structured_content.expect("structured result");
        assert_eq!(body["status"], "context_required");
        assert_eq!(body["contexts_required"][0], "context");
    }

    #[test]
    fn native_check_budget_keeps_verdict_and_first_issue_position() {
        let mut result = CheckResult::new(CheckStatus::Invalid);
        result.valid = Some(false);
        result.compilation_status = Some(crate::native_check::types::CompilationStatus::Invalid);
        result.issues.push(crate::native_check::types::CheckIssue {
            message: "ошибка компиляции ".repeat(100),
            line: Some(7),
            column: Some(13),
        });
        result.issues.push(crate::native_check::types::CheckIssue {
            message: "ещё одна ошибка".to_string(),
            line: Some(9),
            column: Some(2),
        });

        let response =
            native_check_response(result, InputKind::Snippet, MIN_NATIVE_CHECK_OUTPUT_TOKENS);
        let body = response.structured_content.expect("structured result");
        assert_eq!(body["status"], "invalid");
        assert_eq!(body["valid"], false);
        assert_eq!(body["issues"][0]["line"], 7);
        assert_eq!(body["issues"][0]["column"], 13);
        assert_eq!(body["issues"].as_array().unwrap().len(), 1);
        assert_eq!(body["truncated"], true);
        let text = response.content[0].as_text().unwrap().text.as_str();
        assert_eq!(
            text,
            format!(
                "✗ Ошибка синтаксиса:\n{}\nСтрока: 7, колонка: 13",
                body["issues"][0]["message"].as_str().unwrap()
            )
        );
        let text_len = response
            .content
            .iter()
            .filter_map(|item| item.as_text())
            .map(|item| item.text.len())
            .sum::<usize>();
        assert!(
            serde_json::to_vec(&body).unwrap().len() + text_len
                <= 4 * MIN_NATIVE_CHECK_OUTPUT_TOKENS
        );

        let mut valid = CheckResult::new(CheckStatus::Valid);
        valid.valid = Some(true);
        valid.compilation_status = Some(crate::native_check::types::CompilationStatus::Valid);
        let valid =
            native_check_response(valid, InputKind::Snippet, MIN_NATIVE_CHECK_OUTPUT_TOKENS);
        assert_eq!(valid.content[0].as_text().unwrap().text, "✓ Синтаксис корректен");
        assert_eq!(valid.structured_content.unwrap()["status"], "valid");
    }

    #[test]
    fn native_check_preflight_accounts_for_escaped_owner_echo() {
        let request = CheckRequest {
            input_kind: InputKind::Module,
            module_type: Some(crate::native_check::types::ModuleType::Common),
            context: Some(CheckContext::Metadata {
                owner: "\\".repeat(512),
                origin: crate::native_check::types::ModuleOrigin::Configuration,
            }),
        };
        assert!(request.validate().is_ok());
        assert!(minimum_native_check_output_tokens(&request) > MIN_NATIVE_CHECK_OUTPUT_TOKENS);
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
