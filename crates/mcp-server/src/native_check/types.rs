use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const CHECK_SCHEMA_VERSION: &str = "1";

#[derive(Debug, Clone, Copy, Deserialize, Serialize, JsonSchema, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum InputKind {
    #[default]
    Snippet,
    Module,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ModuleType {
    Object,
    ManagedForm,
    OrdinaryForm,
    Common,
    Manager,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub enum CheckContext {
    Metadata {
        #[schemars(length(min = 1, max = 512))]
        owner: String,
        origin: ModuleOrigin,
    },
    Synthetic {
        module_type: ModuleType,
    },
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub enum ModuleOrigin {
    Configuration,
    Extension {
        #[schemars(length(min = 1, max = 128))]
        name: String,
    },
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct CheckRequest {
    #[serde(default)]
    pub input_kind: InputKind,
    pub module_type: Option<ModuleType>,
    pub context: Option<CheckContext>,
}

impl CheckRequest {
    pub fn validate(&self) -> Result<(), &'static str> {
        match self.input_kind {
            InputKind::Snippet if self.module_type.is_some() || self.context.is_some() => {
                return Err("snippet does not accept module_type or context");
            }
            _ => {}
        }

        if let Some(context) = &self.context {
            match context {
                CheckContext::Metadata { owner, origin } => {
                    if owner.trim().is_empty() || owner.len() > 512 {
                        return Err("context.owner must contain 1 to 512 UTF-8 bytes");
                    }
                    if let ModuleOrigin::Extension { name } = origin {
                        if name.trim().is_empty() || name.len() > 128 {
                            return Err("context.origin.name must contain 1 to 128 UTF-8 bytes");
                        }
                    }
                }
                CheckContext::Synthetic { module_type } => {
                    if self.module_type.is_some_and(|requested| requested != *module_type) {
                        return Err("module_type must match context.module_type");
                    }
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Valid,
    Invalid,
    ContextRequired,
    Unsupported,
    Error,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CompilationStatus {
    Valid,
    Invalid,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CleanupStatus {
    NotStarted,
    Complete,
    Pending,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
pub struct CompilerInfo {
    pub build: Option<String>,
    pub compatibility: Option<String>,
    pub fingerprint: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
pub struct CheckIssue {
    pub message: String,
    pub line: Option<u32>,
    pub column: Option<u32>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
pub struct CheckFailure {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
pub struct CheckResult {
    pub schema_version: String,
    pub status: CheckStatus,
    pub valid: Option<bool>,
    pub compilation_status: Option<CompilationStatus>,
    pub contexts_required: Vec<String>,
    pub contexts_checked: Vec<String>,
    pub module_type: Option<ModuleType>,
    pub context: Option<CheckContext>,
    pub owner: Option<String>,
    pub compiler: Option<CompilerInfo>,
    pub issues: Vec<CheckIssue>,
    pub cleanup_status: CleanupStatus,
    pub failure: Option<CheckFailure>,
    pub truncated: bool,
}

impl CheckResult {
    pub fn new(status: CheckStatus) -> Self {
        Self {
            schema_version: CHECK_SCHEMA_VERSION.to_owned(),
            status,
            valid: None,
            compilation_status: None,
            contexts_required: Vec::new(),
            contexts_checked: Vec::new(),
            module_type: None,
            context: None,
            owner: None,
            compiler: None,
            issues: Vec::new(),
            cleanup_status: CleanupStatus::NotStarted,
            failure: None,
            truncated: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_allows_missing_context_and_rejects_synthetic_type_mismatch() {
        assert!(CheckRequest {
            input_kind: InputKind::Module,
            module_type: Some(ModuleType::Common),
            context: None,
        }
        .validate()
        .is_ok());

        assert!(CheckRequest {
            input_kind: InputKind::Module,
            module_type: Some(ModuleType::Object),
            context: Some(CheckContext::Synthetic { module_type: ModuleType::Manager }),
        }
        .validate()
        .is_err());
    }

    #[test]
    fn check_result_has_versioned_json_shape() {
        let value = serde_json::to_value(CheckResult::new(CheckStatus::Unsupported)).unwrap();
        assert_eq!(value["schema_version"], CHECK_SCHEMA_VERSION);
        assert_eq!(value["status"], "unsupported");
        assert_eq!(value["valid"], serde_json::Value::Null);
        assert_eq!(value["truncated"], false);
        for field in [
            "compilation_status",
            "contexts_required",
            "contexts_checked",
            "module_type",
            "context",
            "owner",
            "compiler",
            "issues",
            "cleanup_status",
            "failure",
        ] {
            assert!(value.get(field).is_some(), "missing result field {field}");
        }
    }

    #[test]
    fn request_rejects_unknown_fields_instead_of_silently_defaulting() {
        assert!(serde_json::from_value::<CheckRequest>(serde_json::json!({
            "input_knd": "module"
        }))
        .is_err());
        assert!(serde_json::from_value::<CheckContext>(serde_json::json!({
            "kind": "synthetic",
            "module_type": "common",
            "owner": "ignored"
        }))
        .is_err());
    }
}
