//! Cancellation of one MCP request.
//!
//! `rmcp` hands every tool handler a per-request [`tokio_util::sync::CancellationToken`],
//! cancelled on `notifications/cancelled` and on transport shutdown. That token lives on
//! the async side; the work it has to reach runs on a blocking thread, often deep inside
//! Salsa queries. [`RequestCancel`] is the bridge between the two.
//!
//! It carries the signal in both forms a worker can observe:
//!
//! - a flag, for loops that are between queries and have nothing to unwind from;
//! - the Salsa cancellation tokens of the per-request database handles, so an in-flight
//!   query unwinds at its next query boundary through the `unwind_if_revision_cancelled`
//!   checkpoints the analysis layers already carry.
//!
//! Registration and cancellation share one mutex, so a worker registering after the
//! cancel request has its token cancelled on the spot — there is no window in which a
//! late worker runs to completion. Only the tokens handed to this request are cancelled:
//! a handle clone carries its own token, so the resident's master handle and every
//! concurrent request stay untouched.

use std::sync::Mutex;

use crate::diagnostics_state::lock_recover;

/// The cancellation state of one MCP request.
#[derive(Default)]
pub(crate) struct RequestCancel {
    inner: Mutex<RequestCancelInner>,
}

#[derive(Default)]
struct RequestCancelInner {
    cancel_requested: bool,
    tokens: Vec<salsa::CancellationToken>,
    disk_read: Option<ide_host_core::WorkspaceDiskRead>,
}

impl RequestCancel {
    pub(crate) fn with_disk_read(disk_read: ide_host_core::WorkspaceDiskRead) -> Self {
        Self {
            inner: Mutex::new(RequestCancelInner {
                disk_read: Some(disk_read),
                ..RequestCancelInner::default()
            }),
        }
    }

    /// Register a database handle's token; cancels it immediately when cancellation
    /// was already requested.
    pub(crate) fn register(&self, token: salsa::CancellationToken) {
        let mut inner = lock_recover(&self.inner);
        if let Some(disk_read) = &inner.disk_read {
            disk_read.register_cancel(token.clone());
        }
        if inner.cancel_requested {
            token.cancel();
        }
        inner.tokens.push(token);
    }

    /// Request cancellation: every registered token unwinds at its next salsa query
    /// boundary; handles not yet registered are cancelled by `register`.
    pub(crate) fn cancel_all(&self) {
        let mut inner = lock_recover(&self.inner);
        inner.cancel_requested = true;
        for token in &inner.tokens {
            token.cancel();
        }
    }

    /// Cheap check for loops between queries (nothing to unwind from).
    pub(crate) fn is_cancelled(&self) -> bool {
        lock_recover(&self.inner).cancel_requested
    }

    pub(crate) fn disk_superseded(&self) -> bool {
        lock_recover(&self.inner).disk_read.as_ref().is_some_and(|read| !read.is_current())
    }
}

/// Await a request's blocking task under the rmcp per-request token. Cancellation
/// wins: when `ct` fires (MCP `notifications/cancelled` or transport shutdown) —
/// including a token already cancelled before the first poll — `on_cancel` runs and
/// `None` is returned right away, WITHOUT waiting for the blocking task: it may still
/// be queued behind another call on the resident mutex, and once it runs it exits
/// early and logs on its own. `Some(join result)` when the task finishes first; a
/// completed call never cancels anything.
///
/// `on_cancel` is how the signal reaches a body that cannot see the token itself:
/// the resident door fans it out to its salsa tokens through [`RequestCancel`]. A body
/// that observes a clone of `ct` directly (the search door) has nothing to fan out and
/// passes a no-op.
pub(crate) async fn join_unless_cancelled<T>(
    ct: tokio_util::sync::CancellationToken,
    on_cancel: impl FnOnce(),
    mut join: tokio::task::JoinHandle<T>,
) -> Option<Result<T, tokio::task::JoinError>> {
    tokio::select! {
        // Biased so an already-cancelled token deterministically beats a completed
        // join — a cancelled request must never race into a normal response.
        biased;
        _ = ct.cancelled() => {
            on_cancel();
            None
        }
        joined = &mut join => Some(joined),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// The single registration/cancellation mutex closes the race with workers that
    /// start after the cancel request: their token is cancelled at registration.
    #[test]
    fn register_after_cancel_is_cancelled_on_the_spot() {
        let cancel = RequestCancel::default();

        let early = salsa::CancellationToken::default();
        cancel.register(early.clone());
        assert!(!early.is_cancelled(), "no cancel requested yet");
        assert!(!cancel.is_cancelled());

        cancel.cancel_all();
        assert!(early.is_cancelled(), "cancel_all cancels every registered token");
        assert!(cancel.is_cancelled());

        let late = salsa::CancellationToken::default();
        cancel.register(late.clone());
        assert!(late.is_cancelled(), "a late registration is cancelled immediately");
    }

    /// A token cancelled before the first poll deterministically wins over an
    /// already-completed join: the registry is cancelled and no normal response
    /// can race out.
    #[tokio::test]
    async fn pre_cancelled_token_beats_a_completed_join() {
        let ct = tokio_util::sync::CancellationToken::new();
        ct.cancel();
        let cancel = Arc::new(RequestCancel::default());
        let join = tokio::task::spawn_blocking(|| 42);
        let _ = join.is_finished();

        let out = join_unless_cancelled(ct, || cancel.cancel_all(), join).await;
        assert!(out.is_none(), "a cancelled request must never produce a normal response");
        assert!(cancel.is_cancelled(), "the cancel must fan out to the request registry");
    }

    /// A cancel arriving while the blocking task is stuck (queued on the resident
    /// mutex in production) answers immediately instead of waiting the task out.
    #[tokio::test]
    async fn mid_flight_cancel_answers_without_waiting_for_the_join() {
        let ct = tokio_util::sync::CancellationToken::new();
        let cancel = Arc::new(RequestCancel::default());
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let join = tokio::task::spawn_blocking(move || rx.recv());

        let guard = join_unless_cancelled(ct.clone(), || cancel.cancel_all(), join);
        let canceller = async {
            tokio::task::yield_now().await;
            ct.cancel();
        };
        // The guard can only resolve through the cancel arm: the blocking task
        // stays parked on the channel until we release it below.
        let (out, ()) = tokio::join!(guard, canceller);
        assert!(out.is_none(), "cancellation must not wait for the blocked task");
        assert!(cancel.is_cancelled());

        tx.send(()).expect("the detached task is still alive and picks up the release");
    }

    /// A call that completes first returns the join result untouched, and a late
    /// cancel is a no-op for the (finished) request.
    #[tokio::test]
    async fn completed_join_is_returned_and_a_late_cancel_is_a_noop() {
        let ct = tokio_util::sync::CancellationToken::new();
        let cancel = Arc::new(RequestCancel::default());
        let join = tokio::task::spawn_blocking(|| 7);

        let out = join_unless_cancelled(ct.clone(), || cancel.cancel_all(), join).await;
        let value = out.expect("uncancelled call yields the join").expect("no panic");
        assert_eq!(value, 7);
        assert!(!cancel.is_cancelled(), "a completed call must not cancel anything");

        ct.cancel();
        tokio::task::yield_now().await;
        assert!(!cancel.is_cancelled(), "a cancel after completion has nothing to reach");
    }
}
