use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// Bound any test wait that depends on another task or thread making progress.
const WAIT_BOUND: Duration = test_utils::wait::hang_guard(Duration::from_secs(60));

pub(crate) fn recv<T>(receiver: &Receiver<T>, what: &str) -> T {
    receiver.recv_timeout(WAIT_BOUND).unwrap_or_else(|error| match error {
        RecvTimeoutError::Timeout => panic!("timed out waiting for {what}"),
        RecvTimeoutError::Disconnected => panic!("{what} sender disconnected"),
    })
}

pub(crate) fn wait_until(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT_BOUND;
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(10));
    }
}

pub(crate) fn join<T>(handle: JoinHandle<T>, what: &str) -> thread::Result<T> {
    wait_until(what, || handle.is_finished());
    handle.join()
}
