//! Versioned JSON types and validators for external native checks.
#![deny(missing_docs)]

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Current wire schema version.
pub const SCHEMA_VERSION: &str = "1";
/// Maximum `request_id` length in ASCII bytes.
pub const MAX_REQUEST_ID_BYTES: usize = 64;
/// Fixed bytes added to the request frame bound.
pub const FRAME_OVERHEAD_BYTES: u64 = 64 * 1024;

/// Request sent to a check backend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CheckRequest {
    /// Wire schema version.
    pub schema_version: String,
    /// Non-empty ASCII caller identifier, at most 64 bytes.
    pub request_id: String,
    /// Profile name echoed unchanged in the response.
    pub connection: String,
    /// UTF-8 source text limited by `limits.max_code_bytes`.
    pub code: String,
    /// Five positive bounds supplied by the caller.
    pub limits: WireLimits,
    /// Source shape; an omitted value means `snippet`.
    #[serde(default)]
    pub input_kind: InputKind,
    /// Expected module type, forbidden for snippets.
    pub module_type: Option<ModuleType>,
    /// Metadata owner or explicit synthetic context.
    pub context: Option<ModuleContext>,
}

impl CheckRequest {
    /// Validates version, limits, source bytes, request id, and context.
    pub fn validate(&self) -> Result<(), ContractError> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(ContractError::UnknownSchemaVersion);
        }
        self.limits.validate()?;
        if self.code.len() as u64 > self.limits.max_code_bytes {
            return Err(ContractError::CodeTooLarge);
        }
        if self.request_id.is_empty()
            || self.request_id.len() > MAX_REQUEST_ID_BYTES
            || !self.request_id.is_ascii()
        {
            return Err(ContractError::InvalidRequestId);
        }
        if self.input_kind == InputKind::Snippet
            && (self.module_type.is_some() || self.context.is_some())
        {
            return Err(ContractError::InvalidRequestContext);
        }
        if let Some(context) = &self.context {
            context.validate(self.module_type)?;
        }
        Ok(())
    }
}

/// Kind of source supplied to the checker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum InputKind {
    /// Procedure or function text without module metadata.
    #[default]
    Snippet,
    /// Complete module text.
    Module,
}

/// Supported module kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ModuleType {
    /// Object module.
    Object,
    /// Managed form module.
    ManagedForm,
    /// Ordinary form module.
    OrdinaryForm,
    /// Common module.
    Common,
    /// Manager module.
    Manager,
}

/// Real or synthetic module context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ModuleContext {
    /// Real owner and origin from configuration or an extension.
    Metadata {
        /// Full metadata owner name.
        owner: String,
        /// Configuration or extension where the owner is defined.
        origin: Origin,
    },
    /// Synthetic context that makes no claim about a real owner.
    Synthetic {
        /// Module type represented by the synthetic request.
        module_type: ModuleType,
    },
}

impl ModuleContext {
    fn validate(&self, requested_type: Option<ModuleType>) -> Result<(), ContractError> {
        match self {
            Self::Metadata { owner, origin } => {
                validate_name(owner, 512)?;
                if let Origin::Extension { name } = origin {
                    validate_name(name, 128)?;
                }
            }
            Self::Synthetic { module_type } => {
                if requested_type.is_some_and(|requested| requested != *module_type) {
                    return Err(ContractError::InvalidRequestContext);
                }
            }
        }
        Ok(())
    }
}

/// Origin of a metadata owner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[serde(deny_unknown_fields)]
pub enum Origin {
    /// Owner belongs to the base configuration.
    Configuration {},
    /// Owner belongs to the named extension.
    Extension {
        /// Extension name.
        name: String,
    },
}

/// 1C compiler contexts used to report required and checked coverage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CompilerContext {
    /// Server.
    Server,
    /// Thin client.
    ThinClient,
    /// Web client.
    WebClient,
    /// Managed thick client.
    ThickClientManaged,
    /// Ordinary thick client.
    ThickClientOrdinary,
    /// External connection.
    ExternalConnection,
    /// Mobile client.
    MobileClient,
    /// Mobile application client.
    MobileAppClient,
    /// Mobile application server.
    MobileAppServer,
}

/// Result category for the complete check request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// The check succeeded.
    Valid,
    /// A proven issue exists in the supplied source.
    Invalid,
    /// More module context is needed.
    ContextRequired,
    /// The selected backend cannot handle the request.
    Unsupported,
    /// The backend could not complete a trustworthy result.
    Error,
}

/// Result of the compiler step, independent of later cleanup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CompilationStatus {
    /// Compilation found no source issue.
    Valid,
    /// Compilation found a source issue.
    Invalid,
}

/// Whether owned temporary resources were cleaned up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CleanupStatus {
    /// Cleanup completed.
    Complete,
    /// Some owned resources remain.
    Incomplete,
}

/// Required positive wire bounds supplied by the caller.
/// Normalized response shared by callers and backends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WireLimits {
    /// Operation deadline in milliseconds.
    #[schemars(range(min = 1))]
    pub timeout_ms: u64,
    /// Maximum source size in UTF-8 bytes.
    #[schemars(range(min = 1))]
    pub max_code_bytes: u64,
    /// Maximum response size in bytes.
    #[schemars(range(min = 1))]
    pub max_response_bytes: u64,
    /// Maximum captured stderr size in bytes.
    #[schemars(range(min = 1))]
    pub max_stderr_bytes: u64,
    /// Soft-stop grace period in milliseconds.
    #[schemars(range(min = 1))]
    pub stop_grace_ms: u64,
}

impl WireLimits {
    /// Rejects zero limits and frame arithmetic overflow.
    pub fn validate(&self) -> Result<(), ContractError> {
        if [
            self.timeout_ms,
            self.max_code_bytes,
            self.max_response_bytes,
            self.max_stderr_bytes,
            self.stop_grace_ms,
        ]
        .contains(&0)
        {
            return Err(ContractError::InvalidLimits);
        }
        self.frame_bytes().ok_or(ContractError::FrameOverflow)?;
        Ok(())
    }

    /// Returns `6 * max_code_bytes + 64 KiB`, or `None` on overflow.
    pub fn frame_bytes(&self) -> Option<u64> {
        self.max_code_bytes.checked_mul(6)?.checked_add(FRAME_OVERHEAD_BYTES)
    }
}

/// Normalized response shared by callers and backends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CheckResponse {
    /// Wire schema version.
    pub schema_version: String,
    /// Caller identifier echoed from the request.
    pub request_id: String,
    /// Connection name echoed from the request.
    pub connection: String,
    /// Backend name and a non-empty version suffix separated by `/`.
    pub backend: String,
    /// Overall result category.
    pub status: Status,
    /// `true` for valid, `false` for invalid, or required explicit `null` otherwise.
    #[serde(deserialize_with = "required_nullable")]
    pub valid: Option<bool>,
    /// Findings and their original-source positions.
    pub issues: Vec<Issue>,
    /// Compiler contexts required for a valid result.
    pub contexts_required: Vec<CompilerContext>,
    /// Compiler contexts actually checked.
    pub contexts_checked: Vec<CompilerContext>,
    /// Whether some findings were omitted while the JSON stayed whole.
    pub truncated: bool,
    /// Domain failure, or required explicit `null` if absent.
    #[serde(deserialize_with = "required_nullable")]
    pub failure: Option<Failure>,
    /// Compilation result, or required explicit `null` if unknown.
    #[serde(deserialize_with = "required_nullable")]
    pub compilation_status: Option<CompilationStatus>,
    /// Compiler identity; each unknown member is explicit `null`.
    #[serde(deserialize_with = "required_nullable")]
    pub compiler: Option<CompilerInfo>,
    /// Cleanup result, or required explicit `null` if unknown.
    #[serde(deserialize_with = "required_nullable")]
    pub cleanup_status: Option<CleanupStatus>,
    /// Resolved module type, or required explicit `null` if unavailable.
    #[serde(deserialize_with = "required_nullable")]
    pub module_type: Option<ModuleType>,
    /// Checked owner and origin, or required explicit `null` if unavailable.
    #[serde(deserialize_with = "required_nullable")]
    pub context: Option<ModuleContext>,
    /// Open backend-specific object, or required explicit `null` if unused.
    #[serde(deserialize_with = "required_nullable")]
    pub backend_info: Option<Map<String, Value>>,
}

impl CheckResponse {
    /// Checks response identity and cross-field rules before accepting the result.
    pub fn validate_for(&self, request: &CheckRequest) -> Result<(), ResponseError> {
        if self.request_id != request.request_id {
            return Err(ResponseError::RequestIdMismatch);
        }
        if self.schema_version != SCHEMA_VERSION
            || self.connection != request.connection
            || !has_backend_version(&self.backend)
            || status_valid(self.status) != self.valid
            || self.failure.as_ref().is_some_and(|failure| !failure.is_domain_code())
            || (matches!(self.status, Status::Valid | Status::Invalid) && self.failure.is_some())
            || self.issues.iter().any(|issue| !issue.has_valid_coordinates())
            || (self.status == Status::Valid
                && (self
                    .contexts_required
                    .iter()
                    .any(|context| !self.contexts_checked.contains(context))
                    || self.contexts_checked.is_empty()
                    || self.compilation_status != Some(CompilationStatus::Valid)
                    || self.cleanup_status != Some(CleanupStatus::Complete)))
            || (self.status == Status::Invalid
                && !self.issues.iter().any(|issue| issue.line.is_some() && issue.column.is_some()))
            || (self.status == Status::Invalid
                && self.compilation_status != Some(CompilationStatus::Invalid))
        {
            return Err(ResponseError::Malformed);
        }
        if matches!(self.status, Status::Valid | Status::Invalid)
            && (request.context != self.context
                || match request.input_kind {
                    InputKind::Snippet => self.module_type.is_some(),
                    InputKind::Module => {
                        let expected = request.module_type.or(match &request.context {
                            Some(ModuleContext::Synthetic { module_type }) => Some(*module_type),
                            _ => None,
                        });
                        request.context.is_none()
                            || self.module_type.is_none()
                            || expected
                                .is_some_and(|module_type| self.module_type != Some(module_type))
                    }
                })
        {
            return Err(ResponseError::Malformed);
        }
        Ok(())
    }
}

fn status_valid(status: Status) -> Option<bool> {
    match status {
        Status::Valid => Some(true),
        Status::Invalid => Some(false),
        Status::ContextRequired | Status::Unsupported | Status::Error => None,
    }
}

fn has_backend_version(backend: &str) -> bool {
    backend
        .split_once('/')
        .is_some_and(|(name, version)| !name.trim().is_empty() && !version.trim().is_empty())
}

/// Compiler finding with a positive 1-based line and UTF-16 column pair, or two nulls.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Issue {
    /// Diagnostic text.
    pub message: String,
    /// 1-based source line, or required explicit `null` when unknown.
    #[serde(deserialize_with = "required_nullable")]
    #[schemars(range(min = 1))]
    pub line: Option<u32>,
    /// 1-based UTF-16 code-unit column, or required explicit `null` when unknown.
    #[serde(deserialize_with = "required_nullable")]
    #[schemars(range(min = 1))]
    pub column: Option<u32>,
}

impl Issue {
    fn has_valid_coordinates(&self) -> bool {
        match (self.line, self.column) {
            (Some(line), Some(column)) => line > 0 && column > 0,
            (None, None) => true,
            _ => false,
        }
    }
}

/// Domain failure returned by the backend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Failure {
    /// Lower snake-case domain code; caller transport codes are reserved.
    pub code: String,
    /// Human-readable explanation.
    pub message: String,
}

impl Failure {
    fn is_domain_code(&self) -> bool {
        let code = self.code.as_str();
        !is_caller_code(code)
            && code != "cancelled"
            && !code.is_empty()
            && code.split('_').all(|part| {
                part.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
                    && part.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
            })
    }
}

fn is_caller_code(code: &str) -> bool {
    matches!(
        code,
        "executor_not_found"
            | "spawn_failed"
            | "transport_timeout"
            | "transport_output_limit"
            | "transport_malformed_response"
            | "transport_request_id_mismatch"
            | "transport_exit_status"
    )
}

/// Compiler build and source identity values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CompilerInfo {
    /// Build string, or required explicit `null` when unknown.
    #[serde(deserialize_with = "required_nullable")]
    pub build: Option<String>,
    /// Compatibility mode, or required explicit `null` when unknown.
    #[serde(deserialize_with = "required_nullable")]
    pub compatibility: Option<String>,
    /// Compiler/source fingerprint, or required explicit `null` when unknown.
    #[serde(deserialize_with = "required_nullable")]
    pub fingerprint: Option<String>,
}

fn required_nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

/// Request parsing or validation error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContractError {
    /// Input exceeds the caller or request-derived frame limit.
    FrameTooLarge,
    /// Request uses an unsupported schema version.
    UnknownSchemaVersion,
    /// Request id is empty, non-ASCII, or longer than 64 bytes.
    InvalidRequestId,
    /// Source exceeds `max_code_bytes` in UTF-8 bytes.
    CodeTooLarge,
    /// At least one wire limit is zero.
    InvalidLimits,
    /// Frame-bound arithmetic overflowed.
    FrameOverflow,
    /// Request source kind and module context are inconsistent.
    InvalidRequestContext,
    /// Owner or extension name is blank or exceeds its byte limit.
    InvalidName,
    /// Input is not exactly one valid JSON request object.
    MalformedJson,
}

/// Response validation category reported to the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseError {
    /// Response request id differs from the request.
    RequestIdMismatch,
    /// Response violates a field or cross-field contract rule.
    Malformed,
}

/// Parses one bounded UTF-8 JSON request through EOF, then validates it.
pub fn parse_request(bytes: &[u8], max_frame_bytes: usize) -> Result<CheckRequest, ContractError> {
    if bytes.len() > max_frame_bytes {
        return Err(ContractError::FrameTooLarge);
    }
    let request: CheckRequest =
        serde_json::from_slice(bytes).map_err(|_| ContractError::MalformedJson)?;
    request.validate()?;
    if u64::try_from(bytes.len()).map_err(|_| ContractError::FrameOverflow)?
        > request.limits.frame_bytes().ok_or(ContractError::FrameOverflow)?
    {
        return Err(ContractError::FrameTooLarge);
    }
    Ok(request)
}

/// Generates the request JSON Schema from the serde DTOs.
pub fn request_schema() -> Value {
    let mut schema = strict_object_schema(schemars::schema_for!(CheckRequest), false);
    if let Some(required) = schema.get_mut("required").and_then(Value::as_array_mut) {
        required.retain(|field| field.as_str() != Some("input_kind"));
    }
    schema
}

/// Generates the response JSON Schema from the serde DTOs.
pub fn response_schema() -> Value {
    strict_object_schema(schemars::schema_for!(CheckResponse), true)
}

fn strict_object_schema(schema: schemars::Schema, force_required: bool) -> Value {
    fn visit(value: &mut Value, force_required: bool) {
        match value {
            Value::Object(object) => {
                if let Some(properties) = object.get("properties").and_then(Value::as_object) {
                    if force_required {
                        let required = properties.keys().cloned().map(Value::String).collect();
                        object.insert("required".to_owned(), Value::Array(required));
                    }
                    object.insert("additionalProperties".to_owned(), Value::Bool(false));
                }
                for child in object.values_mut() {
                    visit(child, force_required);
                }
            }
            Value::Array(items) => items.iter_mut().for_each(|item| visit(item, force_required)),
            _ => {}
        }
    }
    let mut value = serde_json::to_value(schema).expect("schema is serializable");
    visit(&mut value, force_required);
    value
}

fn validate_name(value: &str, max_bytes: usize) -> Result<(), ContractError> {
    let trimmed = value.trim();
    if trimmed.is_empty() || value.len() > max_bytes {
        return Err(ContractError::InvalidName);
    }
    Ok(())
}
