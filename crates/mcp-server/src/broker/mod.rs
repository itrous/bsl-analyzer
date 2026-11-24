//! Local-socket broker: one heavy backend per project, with thin auto-started
//! stdio proxies connecting to it instead of each client spawning its own ~3 GB
//! workspace server. This module is pure IPC/discovery infrastructure — no MCP
//! business logic lives here; serving reuses [`crate::serve_stream`].
//!
//! Layout: [`name`] is backend identity/naming; [`daemon`] is the shared backend
//! (accept loop + idle shutdown); [`proxy`] is the thin stdio relay that connects
//! to (or launches) a backend.

pub mod daemon;
mod name;
pub mod proxy;
pub(crate) mod security;
pub mod transport;

/// Internal log destination passed from a detached proxy to its daemon.
pub const DETACHED_LOG_PATH_ENV: &str = "BSL_MCP_DETACHED_LOG_PATH";

pub use name::{
    backend_name, embedding_config_fingerprint, embedding_config_fingerprint_with_prefixes,
    embedding_endpoint_identity, process_epoch, workspace_topology_fingerprint, BackendKey,
    EMBEDDING_DOCUMENT_PREFIX_ENV, EMBEDDING_MAX_INPUT_TOKENS_ENV, EMBEDDING_QUERY_PREFIX_ENV,
    EMBEDDING_TOKENIZER_FILE_ENV, EMBEDDING_TOKENIZER_SHA256_ENV, TOPOLOGY_FP_ENV,
};
pub use name::{SharedBackendIdentity, SharedBackendKey, SHARED_PROTOCOL_VERSION};
pub use security::{peer_pid_available, SUPERVISED_PID_PLATFORMS};
