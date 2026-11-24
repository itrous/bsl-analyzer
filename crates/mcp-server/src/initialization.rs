use std::time::Duration;

use rmcp::model::{InitializeRequestParams, InitializeResult};
use rmcp::service::{RequestContext, ServerInitializeError};
use rmcp::{ErrorData, RoleServer, ServerHandler, ServiceExt};
use tokio_util::sync::CancellationToken;

pub(crate) const INITIALIZE_BUDGET: Duration = Duration::from_secs(5);

/// The workspace preparation phase that prevented an MCP session from starting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InitializationFailure {
    /// The owner did not become ready within its preparation budget.
    TimedOut,
    /// The owner failed before the MCP service could start.
    Failed,
}

impl InitializationFailure {
    fn error(self) -> ErrorData {
        let (message, state, code) = match self {
            Self::TimedOut => (
                "MCP workspace preparation timed out before initialize; retry after preparation",
                "loading",
                "owner_initialization_timeout",
            ),
            Self::Failed => (
                "MCP workspace preparation failed before initialize",
                "failed",
                "owner_initialization_failed",
            ),
        };
        ErrorData::internal_error(
            message,
            Some(serde_json::json!({
                "phase": "owner_initialization", "state": state,
                "reason_code": code, "retryable": self == Self::TimedOut,
            })),
        )
    }
}

struct RefusedInitialization(InitializationFailure);

impl ServerHandler for RefusedInitialization {
    async fn initialize(
        &self,
        _request: InitializeRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<InitializeResult, ErrorData> {
        Err(self.0.error())
    }
}

/// Answer a buffered initialize with its JSON-RPC error and request ID, then close it.
/// The SDK owns framing and negotiation, including malformed and silent clients.
pub async fn refuse_initialization<T, A>(
    transport: T,
    shutdown: CancellationToken,
    failure: InitializationFailure,
) -> anyhow::Result<()>
where
    T: rmcp::transport::IntoTransport<RoleServer, std::io::Error, A>,
{
    let result = tokio::time::timeout(
        INITIALIZE_BUDGET,
        RefusedInitialization(failure).serve_with_ct(transport, shutdown.child_token()),
    )
    .await
    .map_err(|_| anyhow::anyhow!("MCP initialize timed out while reporting workspace refusal"))?;
    match result {
        Err(ServerInitializeError::InitializeFailed(_) | ServerInitializeError::Cancelled) => {
            Ok(())
        }
        Err(error) => Err(anyhow::anyhow!("MCP initialization refusal: {error}")),
        Ok(session) => {
            session.cancel().await?;
            anyhow::bail!("MCP initialization refusal unexpectedly accepted a session")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn workspace_refusal_preserves_the_json_rpc_error_instead_of_a_silent_eof() {
        for failure in [InitializationFailure::TimedOut, InitializationFailure::Failed] {
            let (client, server) = tokio::io::duplex(4096);
            let refusal =
                tokio::spawn(refuse_initialization(server, CancellationToken::new(), failure));
            let error = match ().serve(client).await {
                Ok(_) => panic!("an unavailable workspace must not accept the client"),
                Err(error) => error,
            };
            let text = error.to_string();
            assert!(text.contains("MCP workspace preparation"), "{text}");
            assert!(refusal.await.unwrap().is_ok());
        }
    }
}
