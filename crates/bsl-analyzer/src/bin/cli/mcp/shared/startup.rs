use std::{future::Future, time::Duration};

use mcp_server::{InitializationFailure, McpServer};
use tokio::sync::{watch, Mutex};
use tokio_util::sync::CancellationToken;

pub(super) type OwnerReadiness = watch::Receiver<Option<Result<McpServer, ()>>>;
pub(super) type OwnerStartup = Mutex<Option<OwnerReadiness>>;
const OWNER_BUDGET: Duration = Duration::from_secs(30);

pub(super) async fn wait_for_mcp_owner(
    readiness: &OwnerStartup,
    initialize: impl Future<Output = anyhow::Result<McpServer>> + Send + 'static,
    stop: CancellationToken,
) -> Result<Option<McpServer>, InitializationFailure> {
    if stop.is_cancelled() {
        return Ok(None);
    }
    let mut receiver = {
        let mut current = readiness.lock().await;
        let pending_or_ready =
            current.as_ref().is_some_and(|receiver| match receiver.borrow().as_ref() {
                Some(Ok(_)) => true,
                Some(Err(())) => false,
                None => receiver.has_changed().is_ok(),
            });
        if !pending_or_ready {
            let (sender, receiver) = watch::channel(None);
            // A session deadline must not discard the shared owner or start a second build.
            tokio::spawn(async move {
                let result = initialize.await.map_err(|_error| {
                    tracing::warn!(
                        phase = "owner_initialization",
                        "shared workspace preparation failed before MCP initialization"
                    );
                });
                let _ = sender.send_replace(Some(result));
            });
            *current = Some(receiver);
        }
        current.as_ref().expect("owner readiness installed").clone()
    };
    let ready = async {
        loop {
            let result = receiver.borrow().clone();
            if let Some(result) = result {
                return result.map(Some).map_err(|()| InitializationFailure::Failed);
            }
            receiver.changed().await.map_err(|_| InitializationFailure::Failed)?;
        }
    };
    tokio::select! {
        biased;
        () = stop.cancelled() => Ok(None),
        result = tokio::time::timeout(OWNER_BUDGET, ready) => {
            result.unwrap_or(Err(InitializationFailure::TimedOut))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcp_server::{McpProfile, SharedState};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    #[tokio::test]
    async fn a_session_timeout_keeps_one_initializer_for_the_next_client() {
        let server = McpServer::new(McpProfile::Reference, SharedState::shared());
        let readiness = Arc::new(OwnerStartup::default());
        let calls = Arc::new(AtomicUsize::new(0));
        let (finish, gate) = tokio::sync::oneshot::channel();
        let first_cell = readiness.clone();
        let first_calls = calls.clone();
        tokio::time::pause();
        let first = tokio::spawn(async move {
            wait_for_mcp_owner(
                &first_cell,
                async move {
                    first_calls.fetch_add(1, Ordering::SeqCst);
                    gate.await.unwrap();
                    Ok(server)
                },
                CancellationToken::new(),
            )
            .await
        });
        tokio::task::yield_now().await;
        tokio::time::advance(OWNER_BUDGET + Duration::from_secs(1)).await;
        assert!(matches!(first.await.unwrap(), Err(InitializationFailure::TimedOut)));
        finish.send(()).unwrap();
        let next_calls = calls.clone();
        let next = wait_for_mcp_owner(
            &readiness,
            async move {
                next_calls.fetch_add(100, Ordering::SeqCst);
                anyhow::bail!("a second initializer must not run")
            },
            CancellationToken::new(),
        )
        .await;
        assert!(matches!(next, Ok(Some(_))));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_new_session_can_retry_a_failed_owner() {
        let readiness = OwnerStartup::default();
        let first = wait_for_mcp_owner(
            &readiness,
            async { anyhow::bail!("workspace is temporarily unavailable") },
            CancellationToken::new(),
        )
        .await;
        assert!(matches!(first, Err(InitializationFailure::Failed)));
        let server = McpServer::new(McpProfile::Reference, SharedState::shared());
        let retry =
            wait_for_mcp_owner(&readiness, async move { Ok(server) }, CancellationToken::new())
                .await;
        assert!(matches!(retry, Ok(Some(_))));
    }

    #[tokio::test]
    async fn owner_failure_is_terminal_and_cancelled_sessions_do_not_start_work() {
        let failure = wait_for_mcp_owner(
            &OwnerStartup::default(),
            async { anyhow::bail!("unavailable workspace") },
            CancellationToken::new(),
        )
        .await;
        assert!(matches!(failure, Err(InitializationFailure::Failed)));
        let stop = CancellationToken::new();
        stop.cancel();
        let cancelled = wait_for_mcp_owner(
            &OwnerStartup::default(),
            async { panic!("cancelled initialization must not run") },
            stop,
        )
        .await;
        assert!(matches!(cancelled, Ok(None)));
    }
}
