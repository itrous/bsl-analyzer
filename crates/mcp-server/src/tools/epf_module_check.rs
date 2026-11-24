use std::time::Duration;

use check_contract::{
    CheckRequest, CheckResponse, CleanupStatus, CompilationStatus, CompilerContext, Failure,
    InputKind, Issue, ModuleContext, ModuleType, Status, WireLimits, SCHEMA_VERSION,
};
use rmcp::model::{CallToolResult, ContentBlock};
use rmcp::ErrorData as McpError;
use tokio_util::sync::CancellationToken;

const BACKEND: &str = "http-epf/1";
const SCOPE_TEXT: &str = "synthetic server Object only";

pub(super) async fn check(
    client: &onec_client::Client,
    connection: &str,
    code: &str,
    module_type: Option<ModuleType>,
    context: Option<ModuleContext>,
    max_output_tokens: usize,
    cancel: CancellationToken,
) -> Result<CallToolResult, McpError> {
    let timeout = Duration::from_millis(onec_client::EPF_MODULE_TIMEOUT_MS);
    let deadline = tokio::time::Instant::now() + timeout;
    if cancel.is_cancelled() {
        return Err(McpError::internal_error("Request cancelled", None));
    }
    if code.len() > epf_carrier::MAX_MODULE_BYTES {
        return Err(McpError::invalid_params("BSL source exceeds the EPF module byte limit", None));
    }
    let request = CheckRequest {
        schema_version: SCHEMA_VERSION.to_owned(),
        request_id: super::check_executor::new_request_id(),
        connection: connection.to_owned(),
        code: code.to_owned(),
        limits: WireLimits {
            timeout_ms: onec_client::EPF_MODULE_TIMEOUT_MS,
            max_code_bytes: epf_carrier::MAX_MODULE_BYTES as u64,
            max_response_bytes: onec_client::EPF_MODULE_RESPONSE_BYTES as u64,
            max_stderr_bytes: 16 * 1024,
            stop_grace_ms: 2000,
        },
        input_kind: InputKind::Module,
        module_type,
        context,
    };
    if cancel.is_cancelled() {
        return Err(McpError::internal_error("Request cancelled", None));
    }
    let module = match epf_carrier::admit(&request) {
        Ok(module) => module,
        Err(error) => {
            let code = if error.status() == Status::ContextRequired {
                "epf_context_required"
            } else {
                "epf_unsupported"
            };
            return complete(
                local_failure(&request, code, &error.to_string(), error.status()),
                &request,
                max_output_tokens,
                &cancel,
            );
        }
    };
    if cancel.is_cancelled() {
        return Err(McpError::internal_error("Request cancelled", None));
    }
    if tokio::time::Instant::now() >= deadline {
        return complete(
            local_failure(
                &request,
                "epf_deadline_exceeded",
                "Module check deadline expired before HTTP",
                Status::Error,
            ),
            &request,
            max_output_tokens,
            &cancel,
        );
    }
    ensure_budget(&request, max_output_tokens)?;
    let peer = tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            return Err(McpError::internal_error("Request cancelled", None));
        }
        _ = tokio::time::sleep_until(deadline) => {
            return complete(remote_failure(&request, "epf_deadline_exceeded", "Module check deadline expired; remote completion and cleanup are unconfirmed"), &request, max_output_tokens, &cancel);
        }
        result = client.check_epf_module(&request.request_id, &module, deadline.into_std(), &cancel) => result,
    };
    if cancel.is_cancelled() {
        return Err(McpError::internal_error("Request cancelled", None));
    }
    let response = match peer {
        Ok(peer) => from_peer(&request, &module, peer),
        Err(onec_client::Error::ModuleCheckBeforeRequest { code }) => local_failure(
            &request,
            code,
            "Module check stopped before HTTP; no carrier was created remotely",
            Status::Error,
        ),
        Err(error) => remote_failure(&request, "epf_backend_error", &error.to_string()),
    };
    complete(response, &request, max_output_tokens, &cancel)
}

fn complete(
    response: CheckResponse,
    request: &CheckRequest,
    budget: usize,
    cancel: &CancellationToken,
) -> Result<CallToolResult, McpError> {
    if cancel.is_cancelled() {
        return Err(McpError::internal_error("Request cancelled", None));
    }
    let result = format_response(response, request, budget)?;
    if cancel.is_cancelled() {
        return Err(McpError::internal_error("Request cancelled", None));
    }
    Ok(result)
}

fn base_response(request: &CheckRequest) -> CheckResponse {
    CheckResponse {
        schema_version: SCHEMA_VERSION.to_owned(),
        request_id: request.request_id.clone(),
        connection: request.connection.clone(),
        backend: BACKEND.to_owned(),
        status: Status::Error,
        valid: None,
        issues: Vec::new(),
        contexts_required: vec![CompilerContext::Server],
        contexts_checked: Vec::new(),
        truncated: false,
        failure: None,
        compilation_status: None,
        compiler: None,
        cleanup_status: None,
        module_type: None,
        context: None,
        backend_info: Some(serde_json::json!({
            "scope":"synthetic_server_object",
            "limitations":["Only the internal external-processing object module on the server is checked", "Real metadata owners, configuration callables and client branches are not checked"],
            "template_reference_platform":"8.3.27.2130",
        }).as_object().unwrap().clone()),
    }
}

fn local_failure(
    request: &CheckRequest,
    code: &str,
    message: &str,
    status: Status,
) -> CheckResponse {
    let mut response = base_response(request);
    response.status = status;
    response.failure = Some(Failure { code: code.to_owned(), message: message.to_owned() });
    response.cleanup_status = Some(CleanupStatus::Complete);
    response
}

fn remote_failure(request: &CheckRequest, code: &str, message: &str) -> CheckResponse {
    let mut response = base_response(request);
    response.failure = Some(Failure { code: code.to_owned(), message: message.to_owned() });
    response.backend_info.as_mut().unwrap().insert("cleanup_unknown".into(), true.into());
    response
}

fn from_peer(
    request: &CheckRequest,
    module: &epf_carrier::AdmittedModule<'_>,
    peer: onec_client::EpfModuleCheckResult,
) -> CheckResponse {
    let mut response = base_response(request);
    response.cleanup_status = Some(if peer.cleanup_complete {
        CleanupStatus::Complete
    } else {
        CleanupStatus::Incomplete
    });
    if peer.create_attempted {
        response.module_type = Some(ModuleType::Object);
        response.context = request.context.clone();
    }
    response
        .backend_info
        .as_mut()
        .unwrap()
        .insert("safe_mode_requested".into(), peer.safe_mode_requested.into());
    response.backend_info.as_mut().unwrap().insert("duration_ms".into(), peer.duration_ms.into());
    if peer.created {
        response.compilation_status = Some(CompilationStatus::Valid);
    } else if let Some(position) = peer
        .owned_module_position
        .filter(|_| peer.create_attempted)
        .and_then(|position| original_position(module, position))
    {
        response.compilation_status = Some(CompilationStatus::Invalid);
        response.issues.push(Issue {
            message: peer
                .native_error
                .clone()
                .unwrap_or_else(|| "Native module compilation failed".to_owned()),
            line: Some(position.line),
            column: Some(position.column),
        });
    }
    if response.compilation_status.is_some() {
        response.contexts_checked.push(CompilerContext::Server);
    }
    if !peer.cleanup_complete {
        response.failure = Some(Failure {
            code: "epf_cleanup_failed".to_owned(),
            message: peer
                .cleanup_error
                .unwrap_or_else(|| "Owned EPF cleanup did not complete".to_owned()),
        });
    } else if let Some(code) = peer.failure_code {
        response.failure = Some(Failure {
            code: format!("epf_{code}"),
            message: peer
                .native_error
                .unwrap_or_else(|| "EPF backend could not complete within its bounds".to_owned()),
        });
    } else if peer.created {
        response.status = Status::Valid;
        response.valid = Some(true);
    } else if response.compilation_status == Some(CompilationStatus::Invalid) {
        response.status = Status::Invalid;
        response.valid = Some(false);
    } else {
        response.failure = Some(Failure {
            code: "epf_native_context_unconfirmed".to_owned(),
            message: peer.native_error.unwrap_or_else(|| {
                "Native creation failed without an attributable compiler position".to_owned()
            }),
        });
    }
    response
}

fn original_position(
    module: &epf_carrier::AdmittedModule<'_>,
    position: onec_client::EpfModulePosition,
) -> Option<epf_carrier::SourcePosition> {
    let line = position.line.checked_sub(1)?;
    let mut column = position.column.checked_sub(1)?;
    // The carrier's leading signature is encoding metadata; an original BOM belongs to its source coordinates.
    if line == 0 && module.source().starts_with('\u{feff}') {
        column = column.checked_add(1)?;
    }
    let index = line_index::LineIndex::new(module.source());
    let byte_column = index.utf16_col_to_byte_col(module.source(), line, column)?;
    if index.utf16_col(module.source(), line, byte_column) != column {
        return None;
    }
    let offset = index.offset(line_index::LineCol { line, col: byte_column })?;
    module.position(offset.into())
}

fn ensure_budget(request: &CheckRequest, max_output_tokens: usize) -> Result<(), McpError> {
    let mut minimum = base_response(request);
    minimum.status = Status::Invalid;
    minimum.valid = Some(false);
    minimum.failure = None;
    minimum.contexts_checked = vec![CompilerContext::Server];
    minimum.compilation_status = Some(CompilationStatus::Invalid);
    minimum.cleanup_status = Some(CleanupStatus::Complete);
    minimum.module_type = Some(ModuleType::Object);
    minimum.context = request.context.clone();
    minimum.issues.push(Issue {
        message: "Compilation error".to_owned(),
        line: Some(1),
        column: Some(1),
    });
    format_response(minimum, request, max_output_tokens).map(|_| ())
}

fn format_response(
    mut response: CheckResponse,
    request: &CheckRequest,
    max_output_tokens: usize,
) -> Result<CallToolResult, McpError> {
    let text = format!("BSL check: {:?}; {SCOPE_TEXT}.", response.status);
    let budget = max_output_tokens.saturating_mul(4);
    loop {
        response
            .validate_for(request)
            .map_err(|_| McpError::internal_error("Malformed internal EPF check response", None))?;
        let structured = serde_json::to_value(&response).expect("CheckResponse is serializable");
        let size = serde_json::to_vec(&structured)
            .expect("JSON is serializable")
            .len()
            .saturating_add(text.len());
        if size <= budget {
            let mut result = CallToolResult::success(vec![ContentBlock::text(text)]);
            result.structured_content = Some(structured);
            return Ok(result);
        }
        response.truncated = true;
        if response.backend_info.as_mut().is_some_and(|info| info.remove("limitations").is_some()) {
            continue;
        }
        let mut trimmed = false;
        for message in response
            .issues
            .iter_mut()
            .map(|issue| &mut issue.message)
            .chain(response.failure.iter_mut().map(|failure| &mut failure.message))
        {
            if message.len() > 32 {
                let mut end = (message.len() / 2).max(32);
                while !message.is_char_boundary(end) {
                    end -= 1;
                }
                message.truncate(end);
                trimmed = true;
            }
        }
        if !trimmed {
            return Err(McpError::invalid_params(
                "max_output_tokens is too small for the check-contract envelope",
                None,
            ));
        }
    }
}

#[cfg(test)]
mod tests;
