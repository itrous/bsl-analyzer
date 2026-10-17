//! Waiting on behalf of one request, with the request's cancellation as a way out.
//!
//! Two things on the search path answer through a channel from another thread: the
//! baseline actor and the query embedder. Neither can be interrupted from outside —
//! the actor is mid-way through a synchronous PostgreSQL query, the embedder inside a
//! blocking HTTP call — but the CALLER can stop waiting: it drops its receiver, the
//! answer lands on nobody, and whatever the caller held (the engine guard, its place
//! in the request) is released by ordinary return. The producer finishes its current
//! work on its own and reads nothing back.

use std::sync::mpsc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

/// The caller stopped waiting because its request was cancelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Withdrawn;

/// How often a waiting request looks at its cancellation token. Bounds the latency of a
/// cancellation observed while waiting; a reply ends the wait the moment it arrives.
pub(crate) const REPLY_POLL: Duration = Duration::from_millis(25);

/// Wait for one reply unless the request is cancelled first.
///
/// `Err(Withdrawn)` means the caller gave up; the producer may still answer, into a
/// receiver that no longer exists. A disconnected sender (the producer died) is reported
/// as `Ok(None)`, distinct from cancellation: it is the producer's failure, not the
/// client's cancel, and the caller words it as such.
pub(crate) fn await_reply<R>(
    reply: &mpsc::Receiver<R>,
    cancel: &CancellationToken,
    poll: Duration,
) -> Result<Option<R>, Withdrawn> {
    loop {
        if cancel.is_cancelled() {
            return Err(Withdrawn);
        }
        match reply.recv_timeout(poll) {
            Ok(answer) => return Ok(Some(answer)),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(None),
        }
    }
}

/// Embed one query on a thread of its own, waiting for the vector unless the request is
/// cancelled first.
///
/// The embed is a blocking HTTP round-trip with a fixed timeout (`Embedder::INTERACTIVE_TIMEOUT`)
/// and no cancellation of its own. Running it here, off the caller's thread, lets a cancelled
/// search return at its next poll instead of after that timeout; the helper thread finishes
/// the call, finds its receiver gone, and exits. It captures nothing but the embedder clone
/// and the query text — no engine, no actor — so an abandoned embed holds nothing anyone
/// else waits for.
pub(crate) fn embed_unless_cancelled(
    embedder: bsl_search::Embedder,
    query: &str,
    cancel: &CancellationToken,
) -> Result<Result<Vec<f32>, bsl_search::SearchError>, Withdrawn> {
    let (reply_tx, reply_rx) = mpsc::channel();
    let query = query.to_owned();
    let spawned =
        std::thread::Builder::new().name("bsl-search-query-embed".to_owned()).spawn(move || {
            let _ = reply_tx.send(embedder.embed(&query));
        });
    if let Err(error) = spawned {
        return Ok(Err(bsl_search::SearchError::Embedder(format!(
            "could not start the query embed thread: {error}"
        ))));
    }
    match await_reply(&reply_rx, cancel, REPLY_POLL)? {
        Some(result) => Ok(result),
        None => Ok(Err(bsl_search::SearchError::Embedder(
            "the query embed thread exited without answering".to_owned(),
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::{await_reply, Withdrawn};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};
    use tokio_util::sync::CancellationToken;

    #[test]
    fn a_reply_ends_the_wait_at_once() {
        let (tx, rx) = mpsc::channel();
        tx.send(7).unwrap();
        let out = await_reply(&rx, &CancellationToken::new(), Duration::from_millis(10));
        assert_eq!(out, Ok(Some(7)));
    }

    #[test]
    fn a_dead_producer_is_not_a_cancellation() {
        let (tx, rx) = mpsc::channel::<u8>();
        drop(tx);
        let out = await_reply(&rx, &CancellationToken::new(), Duration::from_millis(10));
        assert_eq!(out, Ok(None));
    }

    /// The wait ends at the cancellation while the producer is still silent: the caller
    /// stops waiting on an answer nobody will read, and the producer is left alone.
    #[test]
    fn a_cancelled_request_stops_waiting_for_a_silent_producer() {
        let (tx, rx) = mpsc::channel::<u8>();
        let cancel = CancellationToken::new();
        // The pause that has the caller waiting before the cancellation is the timing
        // profile's; a cancellation that lands first must end the wait just the same.
        let canceller = {
            let cancel = cancel.clone();
            std::thread::spawn(move || {
                if cfg!(timing_contract) {
                    std::thread::sleep(Duration::from_millis(50));
                }
                cancel.cancel();
            })
        };
        let started = Instant::now();
        let out = await_reply(&rx, &cancel, Duration::from_millis(10));
        let waited = started.elapsed();
        canceller.join().unwrap();

        assert_eq!(out, Err(Withdrawn));
        if cfg!(timing_contract) {
            assert!(waited < Duration::from_millis(500), "waited {waited:?} past the cancellation");
        }
        // The producer is untouched: a late answer still goes through the channel.
        tx.send(1).expect("the receiver is still alive; only the waiting stopped");
    }
}
