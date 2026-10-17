//! The one door a search request goes through.
//!
//! Sibling of `resident_call`: the body runs on a blocking thread under the rmcp
//! per-request token, and the outcome is rendered by the same `cancellable_answer` as every
//! other tool. The difference is how the signal reaches the body. The resident door fans
//! it out to salsa tokens and lets an in-flight query unwind; a search body has no query to
//! unwind and a `std::sync::Mutex` guard that an unwind would poison, so it observes a clone
//! of the token itself and returns [`SearchFailure::Cancelled`] from its next cooperative
//! point — a value, not a panic.

use std::sync::Arc;

use rmcp::model::CallToolResult;
use rmcp::ErrorData as McpError;
use tokio_util::sync::CancellationToken;

use super::types::SearchFailure;
use crate::cancel::join_unless_cancelled;
use crate::diagnostics_state::CallOutcome;

/// Run one search body under the request's cancellation token.
///
/// Returns as soon as the token fires, WITHOUT waiting for the blocking task: the body
/// sees the same token and exits at its next cooperative point, releasing the engine
/// guard or its place in the actor queue by ordinary return. A body that finishes first
/// yields `Ready` with whatever it computed, error included; a panic is `Panicked` and
/// never dressed up as the client's cancel. `Superseded` is never produced here: no writer
/// moves the search engine out from under a reader.
pub(crate) async fn search_call<F>(
    ct: CancellationToken,
    body: F,
) -> CallOutcome<Result<CallToolResult, McpError>>
where
    F: FnOnce(&CancellationToken) -> Result<CallToolResult, SearchFailure> + Send + 'static,
{
    // The body's clone, not a child: `notifications/cancelled` cancels this very token,
    // and a child would add a level for nothing.
    let cancel = Arc::new(ct.clone());
    let worker = Arc::clone(&cancel);
    let join = tokio::task::spawn_blocking(move || body(&worker));

    match join_unless_cancelled(ct, || {}, join).await {
        // The client ignores any response after its `notifications/cancelled`; the
        // detached body observes the same token and exits on its own.
        None => CallOutcome::Cancelled,
        Some(Ok(Ok(answer))) => CallOutcome::Ready(Ok(answer)),
        Some(Ok(Err(SearchFailure::Error(error)))) => CallOutcome::Ready(Err(error)),
        Some(Ok(Err(SearchFailure::Cancelled))) => CallOutcome::Cancelled,
        Some(Err(error)) => {
            // A real panic. The client gets a fixed sentence, so the payload has to be
            // recorded here or it is lost.
            tracing::error!(%error, "search call panicked");
            CallOutcome::Panicked
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn answer() -> CallToolResult {
        CallToolResult::success(vec![])
    }

    /// A token cancelled before the first poll is answered as cancelled whatever the body
    /// does: it is already queued on the blocking pool and may well run to completion —
    /// asserting that it never started would be asserting a race, not a property.
    #[tokio::test]
    async fn a_pre_cancelled_token_is_answered_as_cancelled_whatever_the_body_does() {
        let ct = CancellationToken::new();
        ct.cancel();
        let out = search_call(ct, |_| Ok(answer())).await;
        assert!(matches!(out, CallOutcome::Cancelled));
    }

    /// A body that observed the cancellation itself and returned the value is rendered as
    /// the cancellation, never as a ready answer.
    #[tokio::test]
    async fn a_body_that_returns_cancelled_is_not_a_ready_answer() {
        let out = search_call(CancellationToken::new(), |_| Err(SearchFailure::Cancelled)).await;
        assert!(matches!(out, CallOutcome::Cancelled));
    }

    #[tokio::test]
    async fn an_error_from_the_body_is_a_ready_error() {
        let out = search_call(CancellationToken::new(), |_| {
            Err(SearchFailure::Error(McpError::invalid_params("bad", None)))
        })
        .await;
        assert!(matches!(out, CallOutcome::Ready(Err(error)) if error.message == "bad"));
    }

    /// The body sees the request's own token, not a token of the door's making: a cancel
    /// on the request is visible from inside the body.
    #[tokio::test]
    async fn the_body_observes_the_request_token() {
        let ct = CancellationToken::new();
        let seen = Arc::new(AtomicUsize::new(0));
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let (stored_tx, stored_rx) = std::sync::mpsc::channel::<()>();
        let probe = Arc::clone(&seen);
        let call = search_call(ct.clone(), move |cancel| {
            release_rx.recv().ok();
            probe.store(usize::from(cancel.is_cancelled()) + 1, Ordering::SeqCst);
            let _ = stored_tx.send(());
            Err(SearchFailure::Cancelled)
        });
        tokio::pin!(call);
        ct.cancel();
        let out = (&mut call).await;
        assert!(matches!(out, CallOutcome::Cancelled));
        release_tx.send(()).unwrap();
        // The detached body runs on; wait until it has recorded what it saw.
        stored_rx
            .recv_timeout(crate::change_hub::test_support::STATE_CEILING)
            .expect("the detached body never recorded what it saw");
        assert_eq!(seen.load(Ordering::SeqCst), 2, "the body must see the request's cancel");
    }

    #[tokio::test]
    async fn a_panic_in_the_body_is_panicked_not_cancelled() {
        let out =
            search_call(CancellationToken::new(), |_| -> Result<CallToolResult, SearchFailure> {
                panic!("boom")
            })
            .await;
        assert!(matches!(out, CallOutcome::Panicked));
    }
}
