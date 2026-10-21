use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use base_db::SourceRootId;
use hir::CallHierarchyReverseIndex;
use vfs::FileId;

use super::{
    CallHierarchyIndexCompletion, CallHierarchyIndexPrepareAction, CallHierarchyIndexSnapshotId,
    CallHierarchyIndexState,
};

const HANG_WATCHDOG: Duration = test_utils::wait::hang_guard(Duration::from_secs(60));

fn root() -> SourceRootId {
    SourceRootId(7)
}

fn snapshot(generation: u64) -> CallHierarchyIndexSnapshotId {
    CallHierarchyIndexSnapshotId(generation)
}

fn index() -> Arc<CallHierarchyReverseIndex> {
    Arc::new(CallHierarchyReverseIndex::new())
}

#[test]
fn publishes_ready_index_when_idle_build_completes() {
    // Given: an idle source root.
    let state = CallHierarchyIndexState::default();
    let built = index();

    // When: its first build completes.
    assert!(state.start_build(root(), 1, snapshot(1)));
    assert!(state.publish(root(), 1, Arc::clone(&built)));

    // Then: readers receive the published Arc without copying the index.
    let current = state.current(root()).expect("completed build must be readable");
    assert!(Arc::ptr_eq(&current, &built));
}

#[test]
fn coalesces_idempotent_starts_and_allows_one_waiter() {
    // Given: an in-flight generation.
    let state = CallHierarchyIndexState::default();
    assert!(state.start_build(root(), 1, snapshot(1)));
    let cancellation = state.cancellation(root(), 1).expect("build owns cancellation");

    // When: concurrent callers ask for the same build and body edits arrive.
    assert!(!state.start_build(root(), 1, snapshot(1)));
    assert!(state.record_body_edit_or_supersede_ready(root(), 1, FileId(9)));
    let waiter = state.waiter(root(), 1).expect("first waiter parks");

    // Then: one build/generation is retained, body edits do not cancel it, and followers do not park.
    assert_eq!(state.generation(root()), Some(1));
    assert_eq!(state.frozen_snapshot(root(), 1), Some(snapshot(1)));
    assert!(!cancellation.is_cancelled());
    assert!(state.waiter(root(), 1).is_none());
    assert_eq!(state.drain_journal(root(), 1), Some(vec![FileId(9)]));
    assert!(state.publish(root(), 1, index()));
    assert!(matches!(
        waiter.recv_timeout(test_utils::wait::hang_guard(Duration::from_millis(50))),
        Ok(CallHierarchyIndexCompletion::Ready(_))
    ));
}

#[test]
fn wait_or_ready_observes_a_ready_index_after_a_waiter_is_released() {
    let state = CallHierarchyIndexState::default();
    let built = index();
    assert!(state.start_build(root(), 1, snapshot(1)));
    let waiter = state.wait_or_ready(root(), 1).expect("building generation must be waitable");
    assert!(matches!(waiter, super::CallHierarchyIndexWaitOrReady::Waiting(_)));
    drop(waiter);
    assert!(state.publish(root(), 1, Arc::clone(&built)));

    let ready = state.wait_or_ready(root(), 1).expect("ready generation must be observable");
    let super::CallHierarchyIndexWaitOrReady::Ready(index) = ready else {
        panic!("ready generation must not return a waiter");
    };
    assert!(Arc::ptr_eq(&index, &built));
}

#[test]
fn records_prepare_as_a_generation_watermark() {
    // Given: an untouched source root.
    let state = CallHierarchyIndexState::default();

    // When: the same prepare signal arrives twice, then a newer generation prepares.
    assert!(state.record_prepare(root(), 1));
    assert!(!state.record_prepare(root(), 1));
    assert!(state.record_prepare(root(), 2));

    // Then: the latest prepare authorizes current and older generations, but never a newer one.
    assert!(state.is_prepared(root(), 1));
    assert!(state.is_prepared(root(), 2));
    assert!(!state.is_prepared(root(), 3));
}

#[test]
fn prepare_authorization_reuses_the_current_ready_generation() {
    // Given: a completed first-generation index.
    let state = CallHierarchyIndexState::default();
    assert!(state.start_build(root(), 1, snapshot(1)));
    assert!(state.publish(root(), 1, index()));

    // When: another prepare is authorized.
    let authorization = state.prepare_authorization(root());

    // Then: it authorizes Ready generation one without scheduling generation two.
    assert_eq!(authorization, Some((1, CallHierarchyIndexPrepareAction::UseReady)));
    assert!(state.is_prepared(root(), 1));
    assert!(!state.is_prepared(root(), 2));
}

#[test]
fn dropping_waiter_releases_the_single_waiter_slot() {
    // Given: an active build with one registered incoming-call waiter.
    let state = CallHierarchyIndexState::default();
    assert!(state.start_build(root(), 1, snapshot(1)));
    let waiter = state.waiter(root(), 1).expect("first waiter parks");

    // When: the request abandons its wait before the build completes.
    drop(waiter);

    // Then: a subsequent request may become the sole waiter for the same generation.
    assert!(state.waiter(root(), 1).is_some());
}

#[test]
fn call_hierarchy_index_edit_journal_coalesces_and_blocks_stale_publication() {
    // Given: a frozen build with repeated edits to one module and one edit to another.
    let state = CallHierarchyIndexState::default();
    assert!(state.start_build(root(), 1, snapshot(1)));
    assert!(state.record_body_edit_or_supersede_ready(root(), 1, FileId(9)));
    assert!(state.record_body_edit_or_supersede_ready(root(), 1, FileId(3)));
    assert!(state.record_body_edit_or_supersede_ready(root(), 1, FileId(9)));

    // When: the worker inspects its journal before it has reconciled those edits.
    let edited = state.journal_files(root(), 1).expect("active build retains its edit journal");

    // Then: edits are FileId-coalesced and an index from the stale frozen input cannot publish.
    assert_eq!(edited, vec![FileId(3), FileId(9)]);
    assert!(!state.publish(root(), 1, index()));
}

#[test]
fn body_edit_after_worker_publication_invalidates_ready_index() {
    // Given: a worker that can publish a frozen generation while an edit is in flight.
    let state = CallHierarchyIndexState::default();
    let built = index();
    assert!(state.start_build(root(), 1, snapshot(1)));
    let (published_tx, published_rx) = std::sync::mpsc::sync_channel(0);
    let (edited_tx, edited_rx) = std::sync::mpsc::sync_channel(0);

    let worker_state = state.clone();
    let worker_index = Arc::clone(&built);
    let worker = thread::spawn(move || {
        assert!(worker_state.publish(root(), 1, worker_index));
        published_tx.send(()).expect("editor waits for publication");
    });
    let editor_state = state.clone();
    let editor = thread::spawn(move || {
        published_rx.recv_timeout(HANG_WATCHDOG).expect("worker publication completes");
        assert!(editor_state.record_body_edit_or_supersede_ready(root(), 1, FileId(9)));
        edited_tx.send(()).expect("test waits for edit processing");
    });

    // When: the edit reaches the lifecycle immediately after the worker publishes.
    edited_rx.recv_timeout(HANG_WATCHDOG).expect("body edit is processed");
    worker.join().expect("worker completes");
    editor.join().expect("editor completes");

    // Then: the old Ready value cannot be served to the next incoming request.
    assert!(!state.is_ready_generation(root(), 1));
    assert!(state.current(root()).is_none());
    assert!(!state.record_body_edit_or_supersede_ready(root(), 1, FileId(9)));
    assert!(!state.record_body_edit_or_supersede_ready(root(), 2, FileId(9)));
}

#[test]
fn transitions_to_failed_and_wakes_waiter() {
    // Given: a build with one waiting request.
    let state = CallHierarchyIndexState::default();
    assert!(state.start_build(root(), 1, snapshot(1)));
    let waiter = state.waiter(root(), 1).expect("first waiter parks");

    // When: the build fails.
    assert!(state.fail(root(), 1, "batch opener failed".to_owned()));

    // Then: no index is exposed and the waiter observes the terminal reason.
    assert!(state.current(root()).is_none());
    assert_eq!(state.failure_reason(root(), 1).as_deref(), Some("batch opener failed"));
    assert!(matches!(
        waiter.recv_timeout(test_utils::wait::hang_guard(Duration::from_millis(50))),
        Ok(CallHierarchyIndexCompletion::Failed(reason)) if reason == "batch opener failed"
    ));
}

#[test]
fn explicit_failure_after_ready_preserves_the_last_complete_index() {
    // Given: a successfully published index.
    let state = CallHierarchyIndexState::default();
    let ready = index();
    assert!(state.start_build(root(), 1, snapshot(1)));
    assert!(state.publish(root(), 1, Arc::clone(&ready)));

    // When: the active generation is explicitly marked failed.
    assert!(state.fail(root(), 1, "post-publication validation failed".to_owned()));

    // Then: readers retain the complete value while callers can observe the failed lifecycle.
    assert!(Arc::ptr_eq(
        &state.current(root()).expect("last complete value remains readable"),
        &ready
    ));
    assert_eq!(
        state.failure_reason(root(), 1).as_deref(),
        Some("post-publication validation failed")
    );
}

#[test]
fn rejects_stale_completion_after_newer_generation_starts() {
    // Given: generation one is building.
    let state = CallHierarchyIndexState::default();
    assert!(state.start_build(root(), 1, snapshot(1)));

    // When: generation two replaces it before generation one completes.
    assert!(state.start_build(root(), 2, snapshot(2)));

    // Then: the late generation-one result cannot become resident.
    assert!(!state.publish(root(), 1, index()));
    let current = index();
    assert!(state.publish(root(), 2, Arc::clone(&current)));
    assert!(Arc::ptr_eq(
        &state.current(root()).expect("new generation must be readable"),
        &current
    ));
}

#[test]
fn structural_supersession_cancels_and_discards_the_old_build() {
    // Given: an active build and its sole waiter.
    let state = CallHierarchyIndexState::default();
    assert!(state.start_build(root(), 1, snapshot(1)));
    let cancellation = state.cancellation(root(), 1).expect("build owns cancellation");
    let waiter = state.waiter(root(), 1).expect("first waiter parks");

    // When: a layout/file/config change structurally supersedes the snapshot.
    assert!(state.supersede(root()));

    // Then: the worker is cancelled, the old result is discarded, and a replacement may start.
    assert!(cancellation.is_cancelled());
    assert!(matches!(
        waiter.recv_timeout(test_utils::wait::hang_guard(Duration::from_millis(50))),
        Ok(CallHierarchyIndexCompletion::Superseded)
    ));
    assert!(!state.publish(root(), 1, index()));
    assert!(state.start_build(root(), 2, snapshot(2)));
}

#[test]
fn shutdown_cancels_builds_wakes_waiters_and_rejects_new_starts() {
    // Given: an active build and waiter.
    let state = CallHierarchyIndexState::default();
    assert!(state.start_build(root(), 1, snapshot(1)));
    let cancellation = state.cancellation(root(), 1).expect("build owns cancellation");
    let waiter = state.waiter(root(), 1).expect("first waiter parks");

    // When: the LSP server shuts down.
    state.shutdown();

    // Then: work is cancelled, the waiter is released, and no replacement can start.
    assert!(cancellation.is_cancelled());
    assert!(matches!(
        waiter.recv_timeout(test_utils::wait::hang_guard(Duration::from_millis(50))),
        Ok(CallHierarchyIndexCompletion::Shutdown)
    ));
    assert!(!state.start_build(root(), 2, snapshot(2)));
}

#[test]
fn concurrent_readers_observe_only_complete_old_or_new_indexes() {
    // Given: a published first generation.
    let state = CallHierarchyIndexState::default();
    let old = index();
    let new = index();
    assert!(state.start_build(root(), 1, snapshot(1)));
    assert!(state.publish(root(), 1, Arc::clone(&old)));

    // When: a second generation publishes while another thread reads.
    assert!(state.start_build(root(), 2, snapshot(2)));
    assert!(Arc::ptr_eq(
        &state.current(root()).expect("building keeps the prior ready value"),
        &old
    ));
    let done = Arc::new(AtomicBool::new(false));
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (start_tx, start_rx) = std::sync::mpsc::channel();
    let (reader_done_tx, reader_done_rx) = std::sync::mpsc::channel();
    let reader_state = state.clone();
    let reader_done = Arc::clone(&done);
    let reader_old = Arc::clone(&old);
    let reader_new = Arc::clone(&new);
    let reader = thread::spawn(move || {
        ready_tx.send(()).expect("test receives reader readiness");
        start_rx.recv_timeout(HANG_WATCHDOG).expect("test releases concurrent reader");
        while !reader_done.load(Ordering::Acquire) {
            let current = reader_state.current(root()).expect("a ready value remains available");
            assert!(Arc::ptr_eq(&current, &reader_old) || Arc::ptr_eq(&current, &reader_new));
        }
        reader_done_tx.send(()).expect("test receives reader completion");
    });
    ready_rx.recv_timeout(HANG_WATCHDOG).expect("reader is ready to observe publication");
    start_tx.send(()).expect("reader waits for the start signal");
    assert!(state.publish(root(), 2, Arc::clone(&new)));
    done.store(true, Ordering::Release);
    reader_done_rx.recv_timeout(HANG_WATCHDOG).expect("reader exits after publication");
    reader.join().expect("reader must not observe a partial publication");
}
