use crate::native_check::{
    check, CheckContext, CheckRequest, CheckStatus, CleanupStatus, CompilationStatus, InputKind,
    ModuleOrigin, ModuleType, NativeProfile,
};
use check_contract as wire;
use tokio_util::sync::CancellationToken;

const BACKEND: &str = "bsl-native-check/0.2.85";

pub async fn check_wire(
    configured_profile: &NativeProfile,
    request: wire::CheckRequest,
    client: &onec_client::Client,
    cancel: CancellationToken,
) -> wire::CheckResponse {
    let Some(internal) = internal_request(&request) else {
        return failure(
            &request,
            "unsupported_module_context",
            "The request context is unsupported",
        );
    };
    let mut profile = configured_profile.clone();
    profile.max_input_bytes = request.limits.max_code_bytes as usize;
    profile.deadline_ms = request.limits.timeout_ms;
    profile.max_log_bytes = request.limits.max_stderr_bytes as usize;
    if !(500..=900_000).contains(&request.limits.stop_grace_ms) {
        return failure(
            &request,
            "unsupported_limits",
            "The requested cleanup grace is outside the native executor bounds",
        );
    }
    if let Err(error) = profile.validate() {
        return failure(&request, "profile_invalid", &error.to_string());
    }
    crate::native_check::configure_stop_grace(request.limits.stop_grace_ms);
    let result = check(client, &profile, &internal, &request.code, cancel).await;
    bounded(response(&request, result), request.limits.max_response_bytes as usize)
}

fn internal_request(request: &wire::CheckRequest) -> Option<CheckRequest> {
    let module_type = request.module_type.map(internal_type);
    let context = request.context.as_ref().map(|context| match context {
        wire::ModuleContext::Metadata { owner, origin } => CheckContext::Metadata {
            owner: owner.clone(),
            origin: match origin {
                wire::Origin::Configuration {} => ModuleOrigin::Configuration,
                wire::Origin::Extension { name } => ModuleOrigin::Extension { name: name.clone() },
            },
        },
        wire::ModuleContext::Synthetic { module_type } => {
            CheckContext::Synthetic { module_type: internal_type(*module_type) }
        }
    });
    if request.input_kind == wire::InputKind::Snippet {
        return Some(CheckRequest {
            input_kind: InputKind::Snippet,
            module_type: None,
            context: None,
        });
    }
    Some(CheckRequest { input_kind: InputKind::Module, module_type, context })
}

fn internal_type(value: wire::ModuleType) -> ModuleType {
    match value {
        wire::ModuleType::Object => ModuleType::Object,
        wire::ModuleType::ManagedForm => ModuleType::ManagedForm,
        wire::ModuleType::OrdinaryForm => ModuleType::OrdinaryForm,
        wire::ModuleType::Common => ModuleType::Common,
        wire::ModuleType::Manager => ModuleType::Manager,
    }
}

fn wire_type(value: ModuleType) -> wire::ModuleType {
    match value {
        ModuleType::Object => wire::ModuleType::Object,
        ModuleType::ManagedForm => wire::ModuleType::ManagedForm,
        ModuleType::OrdinaryForm => wire::ModuleType::OrdinaryForm,
        ModuleType::Common => wire::ModuleType::Common,
        ModuleType::Manager => wire::ModuleType::Manager,
    }
}

fn wire_context(value: CheckContext) -> wire::ModuleContext {
    match value {
        CheckContext::Metadata { owner, origin } => wire::ModuleContext::Metadata {
            owner,
            origin: match origin {
                ModuleOrigin::Configuration => wire::Origin::Configuration {},
                ModuleOrigin::Extension { name } => wire::Origin::Extension { name },
            },
        },
        CheckContext::Synthetic { module_type } => {
            wire::ModuleContext::Synthetic { module_type: wire_type(module_type) }
        }
    }
}

fn wire_contexts(values: Vec<String>) -> Vec<wire::CompilerContext> {
    values
        .into_iter()
        .filter_map(|value| match value.as_str() {
            "server" => Some(wire::CompilerContext::Server),
            "thin_client" => Some(wire::CompilerContext::ThinClient),
            "web_client" => Some(wire::CompilerContext::WebClient),
            "thick_client" => Some(wire::CompilerContext::ThickClientOrdinary),
            "thick_client_managed" => Some(wire::CompilerContext::ThickClientManaged),
            "external_connection" => Some(wire::CompilerContext::ExternalConnection),
            _ => None,
        })
        .collect()
}

fn response(
    request: &wire::CheckRequest,
    result: crate::native_check::CheckResult,
) -> wire::CheckResponse {
    wire::CheckResponse {
        schema_version: wire::SCHEMA_VERSION.to_owned(),
        request_id: request.request_id.clone(),
        connection: request.connection.clone(),
        backend: BACKEND.to_owned(),
        status: match result.status {
            CheckStatus::Valid => wire::Status::Valid,
            CheckStatus::Invalid => wire::Status::Invalid,
            CheckStatus::ContextRequired => wire::Status::ContextRequired,
            CheckStatus::Unsupported => wire::Status::Unsupported,
            CheckStatus::Error => wire::Status::Error,
        },
        valid: result.valid,
        issues: result
            .issues
            .into_iter()
            .map(|issue| wire::Issue {
                message: issue.message,
                line: issue.line,
                column: issue.column,
            })
            .collect(),
        contexts_required: wire_contexts(result.contexts_required),
        contexts_checked: wire_contexts(result.contexts_checked),
        truncated: result.truncated,
        failure: result.failure.map(|failure| wire::Failure {
            code: match failure.code.as_str() {
                "cancelled" | "deadline_exceeded" => "timeout".to_owned(),
                _ => failure.code,
            },
            message: failure.message,
        }),
        compilation_status: result.compilation_status.map(|status| match status {
            CompilationStatus::Valid => wire::CompilationStatus::Valid,
            CompilationStatus::Invalid => wire::CompilationStatus::Invalid,
        }),
        compiler: result.compiler.map(|compiler| wire::CompilerInfo {
            build: compiler.build,
            compatibility: compiler.compatibility,
            fingerprint: compiler.fingerprint,
        }),
        cleanup_status: match result.cleanup_status {
            CleanupStatus::Complete => Some(wire::CleanupStatus::Complete),
            CleanupStatus::Pending => Some(wire::CleanupStatus::Incomplete),
            CleanupStatus::NotStarted => None,
        },
        module_type: match request.input_kind {
            wire::InputKind::Snippet => None,
            wire::InputKind::Module => result.module_type.map(wire_type),
        },
        context: result.context.map(wire_context),
        backend_info: None,
    }
}

fn failure(request: &wire::CheckRequest, code: &str, message: &str) -> wire::CheckResponse {
    wire::CheckResponse {
        schema_version: wire::SCHEMA_VERSION.to_owned(),
        request_id: request.request_id.clone(),
        connection: request.connection.clone(),
        backend: BACKEND.to_owned(),
        status: wire::Status::Error,
        valid: None,
        issues: Vec::new(),
        contexts_required: Vec::new(),
        contexts_checked: Vec::new(),
        truncated: false,
        failure: Some(wire::Failure { code: code.to_owned(), message: message.to_owned() }),
        compilation_status: None,
        compiler: None,
        cleanup_status: None,
        module_type: request.module_type,
        context: request.context.clone(),
        backend_info: None,
    }
}

fn bounded(mut response: wire::CheckResponse, limit: usize) -> wire::CheckResponse {
    loop {
        let Ok(bytes) = serde_json::to_vec(&response) else { return response };
        if bytes.len() <= limit {
            return response;
        }
        response.truncated = true;
        if response.issues.len() > 1 {
            response.issues.pop();
            continue;
        }
        response.status = wire::Status::Error;
        response.valid = None;
        response.failure = Some(wire::Failure {
            code: "response_limit_too_small".to_owned(),
            message: "The response limit cannot hold a complete result".to_owned(),
        });
        response.compilation_status = None;
        response.issues.clear();
        response.contexts_checked.clear();
        response.contexts_required.clear();
        response.compiler = None;
        response.cleanup_status = None;
        return response;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> wire::CheckRequest {
        wire::CheckRequest {
            schema_version: wire::SCHEMA_VERSION.to_owned(),
            request_id: "wire-test-1".to_owned(),
            connection: "test".to_owned(),
            code: "x = 1;".to_owned(),
            limits: wire::WireLimits {
                timeout_ms: 120_000,
                max_code_bytes: 1024,
                max_response_bytes: 16 * 1024,
                max_stderr_bytes: 4096,
                stop_grace_ms: 2000,
            },
            input_kind: wire::InputKind::Module,
            module_type: Some(wire::ModuleType::Common),
            context: Some(wire::ModuleContext::Metadata {
                owner: "ОбщийМодуль.Тест".to_owned(),
                origin: wire::Origin::Configuration {},
            }),
        }
    }

    #[test]
    fn module_context_is_preserved_and_context_free_module_remains_context_required() {
        let mut request = request();
        let internal = internal_request(&request).unwrap();
        assert_eq!(internal.module_type, Some(ModuleType::Common));
        assert_eq!(
            internal.context,
            Some(CheckContext::Metadata {
                owner: "ОбщийМодуль.Тест".to_owned(),
                origin: ModuleOrigin::Configuration,
            })
        );

        request.context = None;
        assert!(internal_request(&request).unwrap().context.is_none());
    }

    #[test]
    fn response_limit_keeps_a_whole_truncated_invalid_result() {
        let request = request();
        let internal = internal_request(&request).unwrap();
        let mut result = crate::native_check::CheckResult::new(CheckStatus::Invalid);
        result.valid = Some(false);
        result.compilation_status = Some(CompilationStatus::Invalid);
        result.cleanup_status = CleanupStatus::Complete;
        result.module_type = Some(ModuleType::Common);
        result.context = internal.context;
        result.issues = (1..=2)
            .map(|line| crate::native_check::types::CheckIssue {
                message: "x".repeat(9_000),
                line: Some(line),
                column: Some(1),
            })
            .collect();

        let response = bounded(response(&request, result), 16 * 1024);
        assert_eq!(response.status, wire::Status::Invalid);
        assert!(response.truncated);
        assert_eq!(response.issues.len(), 1);
        assert!(serde_json::to_vec(&response).unwrap().len() <= 16 * 1024);
        response.validate_for(&request).unwrap();
    }

    #[test]
    fn snippet_responses_hide_the_internal_synthetic_module_type() {
        let mut request = request();
        request.input_kind = wire::InputKind::Snippet;
        request.module_type = None;
        request.context = None;

        for status in [CheckStatus::Valid, CheckStatus::Invalid] {
            let mut result = crate::native_check::CheckResult::new(status);
            result.valid = Some(status == CheckStatus::Valid);
            result.compilation_status = Some(match status {
                CheckStatus::Valid => CompilationStatus::Valid,
                CheckStatus::Invalid => CompilationStatus::Invalid,
                _ => unreachable!(),
            });
            result.cleanup_status = CleanupStatus::Complete;
            result.module_type = Some(ModuleType::Common);
            result.contexts_required = vec!["server".to_owned()];
            result.contexts_checked = vec!["server".to_owned()];
            if status == CheckStatus::Invalid {
                result.issues.push(crate::native_check::types::CheckIssue {
                    message: "syntax error".to_owned(),
                    line: Some(1),
                    column: Some(1),
                });
            }

            let response = response(&request, result);
            assert_eq!(response.module_type, None);
            response.validate_for(&request).unwrap();
        }
    }

    #[test]
    fn managed_thick_coverage_survives_the_wire_mapping() {
        let contexts = wire_contexts(vec![
            "thin_client".to_owned(),
            "thick_client_managed".to_owned(),
            "server".to_owned(),
            "web_client".to_owned(),
        ]);
        assert_eq!(
            contexts,
            [
                wire::CompilerContext::ThinClient,
                wire::CompilerContext::ThickClientManaged,
                wire::CompilerContext::Server,
                wire::CompilerContext::WebClient,
            ]
        );
    }
}
