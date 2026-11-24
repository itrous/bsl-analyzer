use lsp_types::{Position, TextDocumentIdentifier, Url};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::documents::MetadataDocumentState;

pub(crate) const PREVIEW: &str = "bsl/metadataRenamePreview";
pub(crate) const COMMIT: &str = "bsl/metadataRenameCommit";
pub(crate) const ABORT: &str = "bsl/metadataRenameAbort";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct PreviewParams {
    pub text_document: TextDocumentIdentifier,
    pub position: Position,
    pub new_name: String,
    pub documents: Vec<MetadataDocumentState>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct CommitParams {
    pub session_id: String,
    pub exclusive_session: bool,
    pub documents: Vec<MetadataDocumentState>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct AbortParams {
    pub session_id: String,
    #[serde(default)]
    pub violation: bool,
    pub document_uri: Option<Url>,
    pub dirty: Option<bool>,
    pub event_kind: Option<DocumentEventKind>,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DocumentEventKind {
    Change,
    Open,
    Close,
    Save,
    Create,
    Delete,
    Rename,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PreviewFile {
    pub uri: Url,
    pub before_text: String,
    pub after_text: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PreviewResource {
    pub old_uri: Url,
    pub new_uri: Url,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PreviewRoot {
    pub source_uri: Url,
    pub canonical_uri: Url,
    pub case_sensitive: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ContextInput {
    pub uri: Url,
    pub canonical_uri: Option<Url>,
    pub case_sensitive: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PreviewResult {
    pub operation: &'static str,
    pub roots: Vec<PreviewRoot>,
    pub context_inputs: Vec<ContextInput>,
    pub session_id: Option<String>,
    pub expires_in_seconds: u64,
    pub can_apply: bool,
    pub problems: Vec<String>,
    pub files: Vec<PreviewFile>,
    pub resources: Vec<PreviewResource>,
    pub documents: Vec<MetadataDocumentState>,
}

pub(crate) fn supported(capabilities: &serde_json::Value) -> bool {
    capabilities.pointer("/experimental/metadataRename/protocolVersion")
        == Some(&serde_json::json!(1))
        && capabilities.pointer("/experimental/metadataRename/buffersOwnReloads")
            == Some(&Value::Bool(true))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_requires_the_exact_protocol_version() {
        for capabilities in [
            serde_json::json!({}),
            serde_json::json!({"experimental":{"metadataRename":true}}),
            serde_json::json!({"experimental":{"metadataRename":{"protocolVersion":2}}}),
        ] {
            assert!(!supported(&capabilities));
        }
        assert!(supported(&serde_json::json!({
            "experimental":{"metadataRename":{"protocolVersion":1,"buffersOwnReloads":true}}
        })));
    }

    #[test]
    fn commit_requires_an_explicit_session_and_fresh_document_manifest() {
        assert!(serde_json::from_value::<CommitParams>(serde_json::json!({
            "sessionId":"test", "documents":[]
        }))
        .is_err());
        assert!(serde_json::from_value::<CommitParams>(serde_json::json!({
            "sessionId":"test", "exclusiveSession":true
        }))
        .is_err());
    }
}
