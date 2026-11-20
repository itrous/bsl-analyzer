use std::time::{Duration, Instant};

use base64::Engine;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::{Client, Error};

/// Maximum wall-clock budget of one synthetic EPF HTTP request.
pub const EPF_MODULE_TIMEOUT_MS: u64 = 30_000;
/// Maximum JSON response accepted from the internal module-check endpoint.
pub const EPF_MODULE_RESPONSE_BYTES: usize = 64 * 1024;
const CARRIER_FORMAT: &str = "bsl-analyzer-synthetic-object/1";

#[derive(Serialize)]
struct EpfModuleCheckRequest<'a> {
    carrier_format: &'static str,
    request_id: &'a str,
    epf_base64: String,
    timeout_ms: u64,
}

/// A native one-based UTF-16 position in the fixed carrier's own object module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EpfModulePosition {
    pub line: u32,
    pub column: u32,
}

/// Internal producer acknowledgement; native compilation and cleanup are independent.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EpfModuleCheckResult {
    pub carrier_format: String,
    pub request_id: String,
    pub create_attempted: bool,
    pub safe_mode_requested: bool,
    pub created: bool,
    pub native_error: Option<String>,
    pub failure_code: Option<String>,
    pub cleanup_complete: bool,
    pub cleanup_error: Option<String>,
    pub duration_ms: u64,
    /// Derived from the native frame before credentials/URLs are redacted; never trusted from JSON.
    #[serde(skip)]
    pub owned_module_position: Option<EpfModulePosition>,
}

impl EpfModuleCheckResult {
    fn validate(&self, request_id: &str) -> Result<(), Error> {
        if self.carrier_format != CARRIER_FORMAT
            || self.request_id != request_id
            || self.create_attempted != self.safe_mode_requested
            || (self.created && (!self.create_attempted || self.native_error.is_some()))
            || (!self.created && self.native_error.is_none() && self.failure_code.is_none())
            || (self.cleanup_complete && self.cleanup_error.is_some())
            || (!self.cleanup_complete
                && self.cleanup_error.as_ref().is_none_or(|text| text.trim().is_empty()))
            || self.failure_code.as_ref().is_some_and(|code| {
                code.is_empty()
                    || !code.split('_').all(|part| {
                        part.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
                            && part
                                .bytes()
                                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
                    })
            })
        {
            return Err(Error::ModuleCheckProtocol { code: "invalid_epf_acknowledgement" });
        }
        Ok(())
    }
}

fn before_request(code: &'static str) -> Error {
    Error::ModuleCheckBeforeRequest { code }
}

fn remaining(deadline: Instant, cancel: &CancellationToken) -> Result<Duration, Error> {
    if cancel.is_cancelled() {
        return Err(before_request("epf_cancelled"));
    }
    let remaining = deadline
        .saturating_duration_since(Instant::now())
        .min(Duration::from_millis(EPF_MODULE_TIMEOUT_MS));
    if remaining.as_millis() == 0 {
        return Err(before_request("epf_deadline_exceeded"));
    }
    Ok(remaining)
}

impl Client {
    /// Compile an admitted synthetic server object module through the internal HTTP endpoint.
    /// Only this method constructs the binary/base64 frame; no path or arbitrary EPF input is accepted.
    /// The absolute caller deadline includes local packaging, encoding and HTTP response reading.
    pub async fn check_epf_module(
        &self,
        request_id: &str,
        module: &epf_carrier::AdmittedModule<'_>,
        deadline: Instant,
        cancel: &CancellationToken,
    ) -> Result<EpfModuleCheckResult, Error> {
        if request_id.is_empty() || request_id.len() > 64 || !request_id.is_ascii() {
            return Err(before_request("invalid_epf_request_id"));
        }
        remaining(deadline, cancel)?;
        let carrier = module.pack().map_err(|_| before_request("epf_carrier_failed"))?;
        let encoded = base64::engine::general_purpose::STANDARD.encode(&carrier);
        let timeout = remaining(deadline, cancel)?;
        let frame = EpfModuleCheckRequest {
            carrier_format: CARRIER_FORMAT,
            request_id,
            epf_base64: encoded,
            timeout_ms: timeout.as_millis() as u64,
        };
        let body = serde_json::to_vec(&frame).map_err(|_| before_request("epf_frame_failed"))?;
        let timeout = remaining(deadline, cancel)?;
        let mut request = self
            .execute_http
            .post(format!("{}/check-module", self.base_url))
            .header(reqwest::header::CONTENT_TYPE, "application/json; charset=utf-8")
            .body(body)
            .timeout(timeout);
        if let Some(user) = &self.user {
            request = request.basic_auth(user, self.password.as_deref());
        }
        let operation = async {
            let transport = |error: reqwest::Error| Error::Request(error.without_url());
            let mut response = request.send().await.map_err(transport)?;
            let status = response.status();
            if response.content_length().is_some_and(|size| size > EPF_MODULE_RESPONSE_BYTES as u64)
            {
                return Err(Error::ModuleCheckProtocol { code: "epf_response_too_large" });
            }
            let mut bytes = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(transport)? {
                if bytes
                    .len()
                    .checked_add(chunk.len())
                    .is_none_or(|size| size > EPF_MODULE_RESPONSE_BYTES)
                {
                    return Err(Error::ModuleCheckProtocol { code: "epf_response_too_large" });
                }
                bytes.extend_from_slice(&chunk);
            }
            if !status.is_success() {
                let body = String::from_utf8_lossy(&bytes);
                if status.as_u16() == 404 && body.trim().is_empty() {
                    return Err(Error::EndpointUnavailable {
                        connection: self.connection_name.clone(),
                    });
                }
                let mut body = self.redact_error_body(body.into_owned());
                let mut end = body.len().min(4096);
                while !body.is_char_boundary(end) {
                    end -= 1;
                }
                body.truncate(end);
                return Err(Error::Status { status: status.as_u16(), body });
            }
            let mut result: EpfModuleCheckResult = crate::decode_json(&bytes)
                .map_err(|_| Error::ModuleCheckProtocol { code: "invalid_epf_json" })?;
            result.validate(request_id)?;
            result.owned_module_position = if result.create_attempted && !result.created {
                result.native_error.as_deref().and_then(owned_module_position)
            } else {
                None
            };
            for message in
                [&mut result.native_error, &mut result.cleanup_error].into_iter().flatten()
            {
                *message = self.redact_error_body(std::mem::take(message));
            }
            Ok(result)
        };
        let result = cancel.run_until_cancelled(operation).await.unwrap_or_else(|| {
            Err(Error::ModuleCheckProtocol { code: "epf_cancelled_after_request" })
        });
        if cancel.is_cancelled() {
            return Err(Error::ModuleCheckProtocol { code: "epf_cancelled_after_request" });
        }
        result
    }
}

fn owned_module_position(error: &str) -> Option<EpfModulePosition> {
    for (kind, module) in
        [("ВнешняяОбработка", "МодульОбъекта"), ("ExternalDataProcessor", "ObjectModule")]
    {
        let prefix = format!("{{{kind}.{}.{module}(", epf_carrier::INTERNAL_PROCESSOR_NAME);
        for (start, matched) in error.match_indices(prefix.as_str()) {
            let numbers = &error[start + matched.len()..];
            let Some(end) = numbers.find(")}") else { continue };
            let Some((line, column)) = numbers[..end].split_once(',') else { continue };
            let (Ok(line), Ok(column)) = (line.trim().parse::<u32>(), column.trim().parse::<u32>())
            else {
                continue;
            };
            if line > 0 && column > 0 {
                return Some(EpfModulePosition { line, column });
            }
        }
    }
    None
}

#[cfg(test)]
mod tests;
