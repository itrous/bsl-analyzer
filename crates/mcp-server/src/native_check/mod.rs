mod commands;
mod compiler;
mod profile;
mod runtime;
mod source;
pub mod types;

pub use profile::{NativeProfile, NativeProfileError, SourceKind};
pub use types::{
    CheckContext, CheckFailure, CheckIssue, CheckRequest, CheckResult, CheckStatus, CleanupStatus,
    CompilationStatus, CompilerInfo, InputKind, ModuleOrigin, ModuleType,
};

/// Validate the local worker profile and prepare its private, owned job root before serving.
pub fn startup(profile: &NativeProfile) -> Result<(), NativeProfileError> {
    profile.validate()?;
    profile.validate_runtime_environment()?;
    profile.prepare_job_root()
}

use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

// ponytail: one process-wide native job; add per-profile locks only if measured throughput requires it.
static ACTIVE_CHECK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Run a compile-only module check using the local profile attached to the selected connection.
pub async fn check(
    client: &onec_client::Client,
    profile: &NativeProfile,
    request: &CheckRequest,
    code: &str,
    cancel: CancellationToken,
) -> CheckResult {
    if let Err(message) = request.validate() {
        let mut result = CheckResult::new(CheckStatus::Error);
        result.failure =
            Some(CheckFailure { code: "invalid_request".to_owned(), message: message.to_owned() });
        return result;
    }
    if request.input_kind == InputKind::Module && request.context.is_none() {
        let mut result = echoed_result(request, CheckStatus::ContextRequired);
        result.failure = Some(CheckFailure {
            code: "module_context_required".to_owned(),
            message: "A metadata owner or an explicit synthetic module context is required"
                .to_owned(),
        });
        return result;
    }
    if code.is_empty() || code.len() > profile.max_input_bytes {
        let mut result = echoed_result(request, CheckStatus::Error);
        result.failure = Some(CheckFailure {
            code: if code.is_empty() { "empty_input" } else { "input_too_large" }.to_owned(),
            message: if code.is_empty() {
                "The code input is empty".to_owned()
            } else {
                "The code input exceeds the configured byte limit".to_owned()
            },
        });
        return result;
    }

    let deadline = Instant::now() + Duration::from_millis(profile.deadline_ms);
    let lock = ACTIVE_CHECK.lock();
    let guard = tokio::select! {
        biased;
        _ = cancel.cancelled() => return failure(request, "cancelled", "Native module check was cancelled"),
        _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
            return failure(request, "deadline_exceeded", "Native module check exceeded its deadline");
        }
        guard = lock => guard,
    };
    let result = profile.run(client, request, code, cancel, deadline).await;
    drop(guard);
    result
}

fn failure(request: &CheckRequest, code: &str, message: &str) -> CheckResult {
    let mut result = echoed_result(request, CheckStatus::Error);
    result.failure = Some(CheckFailure { code: code.to_owned(), message: message.to_owned() });
    result
}

pub(super) fn echoed_result(request: &CheckRequest, status: CheckStatus) -> CheckResult {
    let mut result = CheckResult::new(status);
    result.module_type = request.module_type.or(match &request.context {
        Some(CheckContext::Synthetic { module_type }) => Some(*module_type),
        _ => None,
    });
    result.owner = match &request.context {
        Some(CheckContext::Metadata { owner, .. }) => Some(owner.clone()),
        _ => None,
    };
    result.context = request.context.clone();
    result
}

#[cfg(test)]
mod tests;
