/// Version 3 keys a file by `(collection, root_id, path)` in EVERY carrier, semantic included.
///
/// Each bump is deliberate and its cost is a hard stop for older builds: a column added to a
/// table is invisible to them, but the DATA is not. Once a build that knows roots publishes an
/// extension, an older one on the same schema folds two files sharing a relative path into one
/// key and silently serves one of them. The choice was never "hard stop versus rolling upgrade"
/// — it was a hard stop versus losing a live file in silence. Version 2 brought the three
/// mandatory carriers, and 3 brings `serving_semantic`, where the same two files would otherwise
/// collide on one primary key.
pub const SCHEMA_VERSION_CURRENT: i32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmbeddingFailureCode {
    EmbeddingInvalidConfig,
    EmbeddingInputTooLarge,
    EmbeddingRequestTooLarge,
    EmbeddingResponseTooLarge,
    EmbeddingTimeout,
    EmbeddingTransportError,
    EmbeddingProviderError,
    EmbeddingInvalidResponse,
    EmbeddingFailed,
}

impl EmbeddingFailureCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::EmbeddingInvalidConfig => "embedding_invalid_config",
            Self::EmbeddingInputTooLarge => "embedding_input_too_large",
            Self::EmbeddingRequestTooLarge => "embedding_request_too_large",
            Self::EmbeddingResponseTooLarge => "embedding_response_too_large",
            Self::EmbeddingTimeout => "embedding_timeout",
            Self::EmbeddingTransportError => "embedding_transport_error",
            Self::EmbeddingProviderError => "embedding_provider_error",
            Self::EmbeddingInvalidResponse => "embedding_invalid_response",
            Self::EmbeddingFailed => "embedding_failed",
        }
    }
}

/// Safe diagnostic shared by native results and MCP. It never carries provider text.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, thiserror::Error,
)]
#[serde(deny_unknown_fields)]
#[error("{}", code.as_str())]
pub struct EmbeddingFailure {
    pub code: EmbeddingFailureCode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_bytes: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_request_bytes: Option<usize>,
}

impl EmbeddingFailure {
    pub fn new(code: EmbeddingFailureCode) -> Self {
        Self { code, request_bytes: None, max_request_bytes: None }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SearchError {
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("postgres error: {0}")]
    Postgres(#[from] postgres::Error),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("embedding_failed")]
    Embedder(String),

    #[error(transparent)]
    Embedding(#[from] EmbeddingFailure),

    #[error("index error: {0}")]
    Index(String),

    #[error("external baseline error: {0}")]
    ExternalBaseline(String),

    #[error("storage_not_initialized: PostgreSQL baseline storage has not been initialized (schema: {schema}); run `admin migrate` first")]
    StorageNotInitialized { schema: String },

    #[error(
        "schema_version_mismatch: schema {schema} is at version {}, and this build needs \
         {expected}; {}",
        .actual.map_or_else(|| "none".to_owned(), |version| version.to_string()),
        // The two directions need opposite advice, and only one of them is the common case.
        // A schema NEWER than the build cannot be migrated forward — `admin migrate` is exactly
        // the command that just refused it — so telling the operator to run it again sends them
        // in a loop.
        if .actual.is_some_and(|version| version > *.expected) {
            "upgrade this build to one that knows that version"
        } else {
            "run `admin migrate` to bring it forward"
        }
    )]
    SchemaVersionMismatch { expected: i32, actual: Option<i32>, schema: String },
}

impl SearchError {
    pub fn embedding_failure(&self) -> Option<EmbeddingFailure> {
        match self {
            Self::Embedding(failure) => Some(*failure),
            Self::Embedder(_) => Some(EmbeddingFailure::new(EmbeddingFailureCode::EmbeddingFailed)),
            _ => None,
        }
    }

    pub fn is_storage_not_initialized(&self) -> bool {
        matches!(self, Self::StorageNotInitialized { .. })
    }

    pub fn is_schema_version_mismatch(&self) -> bool {
        matches!(self, Self::SchemaVersionMismatch { .. })
    }

    pub fn reason_code(&self) -> Option<&'static str> {
        match self {
            Self::StorageNotInitialized { .. } => Some("storage_not_initialized"),
            Self::SchemaVersionMismatch { .. } => Some("schema_version_mismatch"),
            Self::ExternalBaseline(message) => external_baseline_reason_code(message),
            Self::Embedding(failure) => Some(failure.code.as_str()),
            Self::Embedder(_) => Some("embedding_failed"),
            _ => None,
        }
    }

    /// Builds a named refusal whose name the classifier is guaranteed to recognise.
    ///
    /// For refusals whose name is decided at run time. A name written as a literal at the
    /// construction site can be read there and checked against the vocabulary; one that arrives
    /// through a binding cannot, so it comes from a [`ReasonCode`] instead.
    pub(crate) fn named(reason: ReasonCode, detail: impl std::fmt::Display) -> Self {
        Self::ExternalBaseline(format!("{}: {detail}", reason.as_str()))
    }

    pub fn is_retryable(&self) -> bool {
        match self {
            Self::StorageNotInitialized { .. } | Self::SchemaVersionMismatch { .. } => false,
            Self::ExternalBaseline(message) => message_names(message, RETRYABLE_REASONS),
            Self::Postgres(error) => postgres_error_is_retryable(error),
            Self::Io(_) => true,
            Self::Embedder(_) | Self::Embedding(_) | Self::Index(_) => false,
            Self::Sqlite(_) => false,
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::StorageNotInitialized { .. }
                | Self::SchemaVersionMismatch { .. }
                | Self::Embedder(_)
                | Self::Embedding(_)
                | Self::Index(_)
        ) || matches!(self, Self::Postgres(error) if !postgres_error_is_retryable(error))
            || matches!(
                self,
                Self::ExternalBaseline(message) if !message_names(message, NON_TERMINAL_REASONS)
            )
    }
}

/// A refusal name the classifier is guaranteed to know.
///
/// The inner string is unreachable outside this module, so a helper cannot hand
/// [`SearchError::named`] a name of its own invention: the only names that exist are the
/// constants below. A code spelled twice — once where the refusal is built, once where it is
/// classified — is a code that can drift apart, and the drift is silent, because a refusal whose
/// name nobody recognises is simply an unnamed one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ReasonCode(&'static str);

impl ReasonCode {
    pub(crate) const fn as_str(self) -> &'static str {
        self.0
    }
}

pub(crate) mod reason {
    use super::ReasonCode;

    pub(crate) const HELPER_SPAWN_FAILED: ReasonCode = ReasonCode("helper_spawn_failed");
    pub(crate) const HELPER_TIMEOUT: ReasonCode = ReasonCode("helper_timeout");
    pub(crate) const HELPER_PROTOCOL_ERROR: ReasonCode = ReasonCode("helper_protocol_error");
    pub(crate) const HELPER_REJECTED: ReasonCode = ReasonCode("helper_rejected");
    pub(crate) const RESOLVED_TARGET_MISMATCH: ReasonCode = ReasonCode("resolved_target_mismatch");
    pub(crate) const MISSING_CONFIG: ReasonCode = ReasonCode("missing_config");
    pub(crate) const POSTGRES_CONNECT_FAILED: ReasonCode = ReasonCode("postgres_connect_failed");
    pub(crate) const POSTGRES_AUTH_FAILED: ReasonCode = ReasonCode("postgres_auth_failed");
    pub(crate) const REFRESH_RETRY_EXHAUSTED: ReasonCode = ReasonCode("refresh_retry_exhausted");
    pub(crate) const SERVING_LEXICAL_UNAVAILABLE: ReasonCode =
        ReasonCode("serving_lexical_unavailable");
    pub(crate) const SERVING_SEMANTIC_UNAVAILABLE: ReasonCode =
        ReasonCode("serving_semantic_unavailable");
    pub(crate) const ROOT_ID_NOT_PORTABLE: ReasonCode = ReasonCode("root_id_not_portable");
    pub(crate) const SNAPSHOT_REPUBLISHED_WHILE_PUBLISHING: ReasonCode =
        ReasonCode("snapshot_republished_while_publishing");
    pub(crate) const SCHEMA_NAME_TOO_LONG: ReasonCode = ReasonCode("schema_name_too_long");
    pub(crate) const TOKEN_LAYOUT_CLAIM_MISMATCH: ReasonCode =
        ReasonCode("token_layout_claim_mismatch");
    pub(crate) const TOKEN_LAYOUT_CLAIM_MISSING: ReasonCode =
        ReasonCode("token_layout_claim_missing");
    pub(crate) const TOKEN_LAYOUT_CLAIM_REQUIRED: ReasonCode =
        ReasonCode("token_layout_claim_required");
    pub(crate) const TOKEN_LAYOUT_PROVENANCE_MISSING: ReasonCode =
        ReasonCode("token_layout_provenance_missing");
    pub(crate) const SOURCE_SPAN_INVALID: ReasonCode = ReasonCode("source_span_invalid");
    pub(crate) const PARENT_SNAPSHOT_MISSING: ReasonCode = ReasonCode("parent_snapshot_missing");
    pub(crate) const SNAPSHOT_STILL_REFERENCED: ReasonCode =
        ReasonCode("snapshot_still_referenced");
    pub(crate) const PARENT_LINK_NOT_ENFORCED: ReasonCode = ReasonCode("parent_link_not_enforced");
    pub(crate) const SNAPSHOT_CHANGED_WHILE_COMPACTING: ReasonCode =
        ReasonCode("snapshot_changed_while_compacting");
}

/// The whole vocabulary. A code missing from here is not a named refusal at all.
const KNOWN_REASON_CODES: &[ReasonCode] = &[
    reason::HELPER_SPAWN_FAILED,
    reason::HELPER_TIMEOUT,
    reason::HELPER_PROTOCOL_ERROR,
    reason::HELPER_REJECTED,
    reason::RESOLVED_TARGET_MISMATCH,
    reason::MISSING_CONFIG,
    reason::POSTGRES_CONNECT_FAILED,
    reason::POSTGRES_AUTH_FAILED,
    reason::REFRESH_RETRY_EXHAUSTED,
    reason::SERVING_LEXICAL_UNAVAILABLE,
    reason::SERVING_SEMANTIC_UNAVAILABLE,
    reason::ROOT_ID_NOT_PORTABLE,
    reason::SNAPSHOT_REPUBLISHED_WHILE_PUBLISHING,
    reason::SCHEMA_NAME_TOO_LONG,
    reason::TOKEN_LAYOUT_CLAIM_MISMATCH,
    reason::TOKEN_LAYOUT_CLAIM_MISSING,
    reason::TOKEN_LAYOUT_CLAIM_REQUIRED,
    reason::TOKEN_LAYOUT_PROVENANCE_MISSING,
    reason::SOURCE_SPAN_INVALID,
    reason::PARENT_SNAPSHOT_MISSING,
    reason::SNAPSHOT_STILL_REFERENCED,
    reason::PARENT_LINK_NOT_ENFORCED,
    reason::SNAPSHOT_CHANGED_WHILE_COMPACTING,
];

/// Refusals the caller may retry: the corpus is intact, the way to it was not.
const RETRYABLE_REASONS: &[ReasonCode] =
    &[reason::POSTGRES_CONNECT_FAILED, reason::POSTGRES_AUTH_FAILED];

/// Refusals that may yet resolve on their own — serving tables are filled by a later publish.
const NON_TERMINAL_REASONS: &[ReasonCode] = &[
    reason::POSTGRES_CONNECT_FAILED,
    reason::POSTGRES_AUTH_FAILED,
    reason::SERVING_LEXICAL_UNAVAILABLE,
    reason::SERVING_SEMANTIC_UNAVAILABLE,
];

fn message_names(message: &str, codes: &[ReasonCode]) -> bool {
    external_baseline_reason_code(message)
        .is_some_and(|named| codes.iter().any(|code| code.as_str() == named))
}

fn external_baseline_reason_code(message: &str) -> Option<&'static str> {
    KNOWN_REASON_CODES
        .iter()
        .map(|code| code.as_str())
        .find(|code| message.strip_prefix(code).is_some_and(|rest| rest.starts_with(':')))
}

fn postgres_error_is_retryable(error: &postgres::Error) -> bool {
    let message = error.to_string().to_ascii_lowercase();
    [
        "connection refused",
        "connection reset",
        "connection closed",
        "could not connect",
        "broken pipe",
        "timed out",
        "timeout",
        "password authentication failed",
        "authentication failed",
        "no connection to the server",
        "server closed the connection",
        "eof",
        "tls",
        "ssl",
    ]
    .iter()
    .any(|needle| message.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_not_initialized_is_not_retryable() {
        let err = SearchError::StorageNotInitialized { schema: "bsl_search".to_owned() };
        assert!(err.is_storage_not_initialized());
        assert!(!err.is_retryable());
    }

    #[test]
    fn schema_version_mismatch_is_not_retryable() {
        let err = SearchError::SchemaVersionMismatch {
            expected: 1,
            actual: Some(0),
            schema: "bsl_search".to_owned(),
        };
        assert!(err.is_schema_version_mismatch());
        assert!(!err.is_retryable());
    }

    #[test]
    fn postgres_parse_error_is_terminal() {
        let result: Result<postgres::Config, _> = "invalid".parse();
        if let Err(pg_err) = result {
            let err: SearchError = pg_err.into();
            assert!(!err.is_retryable());
            assert!(err.is_terminal());
        }
    }

    /// A code that exists as a constant is a code the classifier knows.
    ///
    /// The other half of the same drift: a refusal built through `SearchError::named` can only
    /// name a `reason::` constant, but a constant left out of `KNOWN_REASON_CODES` classifies as
    /// nothing at all — the refusal comes out unnamed, and only a caller reading `reason_code()`
    /// would ever notice. Counted out of the source so declaring a code and registering it
    /// cannot become two separate acts.
    #[test]
    fn every_declared_reason_code_is_in_the_vocabulary() {
        // Spelled in halves so this needle does not count itself: the scan reads the very file
        // it lives in. Matched on the declaration's type, not on its initialiser, which rustfmt
        // is free to wrap onto the next line.
        let declared = include_str!("error.rs").matches(concat!(": ReasonCode", " =")).count();

        assert_eq!(
            declared,
            KNOWN_REASON_CODES.len(),
            "{declared} reason codes are declared but {} are registered; a code outside \
             KNOWN_REASON_CODES names nothing",
            KNOWN_REASON_CODES.len()
        );
    }

    /// Every named refusal in the storage adapter is recognised by the classifier.
    ///
    /// Read out of the SOURCE rather than from a list written here. The previous version of this
    /// gate was a third hand-written copy of the same names — it could not fail on the drift it
    /// was written for, because adding a refusal without touching `KNOWN_REASON_CODES` also
    /// leaves the test's own list untouched. A gate that restates the thing it guards is not a
    /// gate.
    #[test]
    fn every_named_refusal_in_the_adapter_has_a_reason_code() {
        let source = include_str!("external_baseline/postgres.rs");
        let production = source.split("\n#[cfg(test)]\nmod tests {").next().unwrap_or(source);

        let mut checked = 0usize;
        for occurrence in production.split("ExternalBaseline(").skip(1) {
            // The refusal's name is the prefix before the first colon of its message, whether the
            // message is a literal or a `format!`.
            let window: String = occurrence.chars().take(200).collect();
            let Some(quote) = window.find('"') else { continue };
            let rest = &window[quote + 1..];
            // A name that arrives through a binding is invisible to a reader AND to this scan,
            // which used to skip such a site in silence — leaving the drift it guards against
            // free to happen on exactly the refusals it could not read. Those go through
            // `SearchError::named`, where the name can only be a declared code.
            assert!(
                !rest.starts_with('{'),
                "this refusal interpolates its name, so nothing here can check it against the \
                 classifier; construct it with SearchError::named and a `reason::` constant: {rest}"
            );
            let Some(colon) = rest.find(':') else { continue };
            let name = &rest[..colon];
            if name.is_empty()
                || !name.chars().all(|c| c.is_ascii_lowercase() || c == '_')
                || name.contains('\n')
            {
                continue;
            }
            let error = SearchError::ExternalBaseline(format!("{name}: something went wrong"));
            assert_eq!(
                error.reason_code(),
                Some(name),
                "the adapter names this refusal, but the classifier does not know it"
            );
            checked += 1;
        }

        assert!(
            checked >= 4,
            "the scan found only {checked} named refusals; it stopped matching the source"
        );
    }

    /// The advice matches the DIRECTION of the mismatch.
    ///
    /// One message served both directions, and for a schema newer than the build it named the
    /// very command that had just refused it.
    #[test]
    fn the_version_mismatch_advises_by_direction() {
        let behind = SearchError::SchemaVersionMismatch {
            expected: 2,
            actual: Some(1),
            schema: "bsl_search".to_owned(),
        }
        .to_string();
        assert!(behind.contains("admin migrate"), "a schema behind the build migrates: {behind}");

        let ahead = SearchError::SchemaVersionMismatch {
            expected: 2,
            actual: Some(3),
            schema: "bsl_search".to_owned(),
        }
        .to_string();
        assert!(
            ahead.contains("upgrade this build"),
            "a schema ahead of the build cannot be migrated forward: {ahead}"
        );
        assert!(
            !ahead.contains("admin migrate"),
            "advising the command that just refused sends the operator in a loop: {ahead}"
        );
    }

    #[test]
    fn io_error_is_retryable() {
        let io_err = std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "refused");
        let err: SearchError = io_err.into();
        assert!(err.is_retryable());
    }

    #[test]
    fn external_baseline_error_is_not_retryable() {
        let err = SearchError::ExternalBaseline("credential refresh failed: resolve: …".to_owned());
        assert!(!err.is_retryable());
    }

    #[test]
    fn embedder_error_is_not_retryable() {
        let err = SearchError::Embedder("embedder timeout".to_owned());
        assert!(!err.is_retryable());
    }

    #[test]
    fn index_error_is_not_retryable() {
        let err = SearchError::Index("index corrupted".to_owned());
        assert!(!err.is_retryable());
    }

    #[test]
    fn external_baseline_reason_code_is_detected() {
        let err = SearchError::ExternalBaseline(
            "helper_protocol_error: credential helper returned invalid JSON".to_owned(),
        );
        assert_eq!(err.reason_code(), Some("helper_protocol_error"));
    }

    #[test]
    fn token_layout_and_source_span_refusals_are_registered() {
        for prefix in [
            "token_layout_claim_mismatch",
            "token_layout_claim_missing",
            "token_layout_claim_required",
            "token_layout_provenance_missing",
            "source_span_invalid",
        ] {
            let error = SearchError::ExternalBaseline(format!("{prefix}: invalid layout"));
            assert_eq!(error.reason_code(), Some(prefix));
        }
    }

    #[test]
    fn external_baseline_connectivity_error_is_retryable_and_non_terminal() {
        let err = SearchError::ExternalBaseline(
            "postgres_connect_failed: failed to get pooled connection".to_owned(),
        );
        assert!(err.is_retryable());
        assert!(!err.is_terminal());
        assert_eq!(err.reason_code(), Some("postgres_connect_failed"));
    }

    #[test]
    fn external_baseline_auth_error_is_retryable_and_non_terminal() {
        let err = SearchError::ExternalBaseline(
            "postgres_auth_failed: password authentication failed".to_owned(),
        );
        assert!(err.is_retryable());
        assert!(!err.is_terminal());
        assert_eq!(err.reason_code(), Some("postgres_auth_failed"));
    }

    #[test]
    fn external_baseline_retry_exhausted_error_is_terminal() {
        let err = SearchError::ExternalBaseline(
            "refresh_retry_exhausted: postgres error: connection refused".to_owned(),
        );
        assert!(!err.is_retryable());
        assert!(err.is_terminal());
        assert_eq!(err.reason_code(), Some("refresh_retry_exhausted"));
    }

    #[test]
    fn serving_lexical_unavailable_is_non_terminal() {
        let err = SearchError::ExternalBaseline(
            "serving_lexical_unavailable: snapshot has no serving rows".to_owned(),
        );
        assert!(!err.is_retryable());
        assert!(!err.is_terminal());
        assert_eq!(err.reason_code(), Some("serving_lexical_unavailable"));
    }

    #[test]
    fn serving_semantic_unavailable_is_non_terminal() {
        let err = SearchError::ExternalBaseline(
            "serving_semantic_unavailable: snapshot has no semantic serving rows".to_owned(),
        );
        assert!(!err.is_retryable());
        assert!(!err.is_terminal());
        assert_eq!(err.reason_code(), Some("serving_semantic_unavailable"));
    }

    #[test]
    fn storage_errors_are_terminal() {
        assert!(
            SearchError::StorageNotInitialized { schema: "bsl_search".to_owned() }.is_terminal()
        );
        assert!(SearchError::SchemaVersionMismatch {
            expected: 1,
            actual: Some(0),
            schema: "bsl_search".to_owned(),
        }
        .is_terminal());
        assert!(SearchError::ExternalBaseline("refresh failed".to_owned()).is_terminal());
        assert!(SearchError::Embedder("timeout".to_owned()).is_terminal());
        assert!(SearchError::Index("corrupted".to_owned()).is_terminal());
    }

    #[test]
    fn transient_errors_are_not_terminal() {
        let io_err = std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "refused");
        assert!(!SearchError::from(io_err).is_terminal());
        assert!(!SearchError::Sqlite(rusqlite::Error::InvalidQuery).is_terminal());
    }
}
