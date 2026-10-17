use super::*;

use base_db::{SourceDatabase as _, SourceRoot, BSL_SOURCE_ROOT};
use crossbeam_channel::{Receiver, Sender};
use lsp_types::request::GotoDefinition;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Bound of a wait for an event a regression would never deliver: the shared sixty-second
/// ceiling under `timing_contract`, none otherwise (see [`test_utils::wait`]).
const HANG_GUARD: Duration = test_utils::wait::ceiling(Duration::from_secs(60));

#[derive(Clone, Copy)]
enum OwnerExit {
    PendingWrite,
    Panic,
}

struct SharedQueryGate {
    exit: OwnerExit,
    entered: Sender<()>,
    release: Receiver<()>,
    calls: AtomicUsize,
    waiter_observed_propagated: AtomicBool,
}

struct ClearQueryGateOnDrop;

impl Drop for ClearQueryGateOnDrop {
    fn drop(&mut self) {
        shared_query_gate().lock().unwrap_or_else(|poisoned| poisoned.into_inner()).take();
    }
}

static SHARED_QUERY_GATE: OnceLock<Mutex<Option<Arc<SharedQueryGate>>>> = OnceLock::new();

fn shared_query_gate() -> &'static Mutex<Option<Arc<SharedQueryGate>>> {
    SHARED_QUERY_GATE.get_or_init(|| Mutex::new(None))
}

#[salsa::tracked(returns(copy))]
fn shared_blocking_query<'db>(
    db: &'db dyn salsa::Database,
    _key: base_db::FileIdInput<'db>,
) -> u32 {
    let gate = shared_query_gate()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .as_ref()
        .cloned()
        .expect("controlled query gate installed");

    if gate.calls.fetch_add(1, Ordering::AcqRel) == 0 {
        gate.entered.send(()).expect("test is waiting for query owner");
        gate.release.recv().expect("test releases query owner");
        match gate.exit {
            OwnerExit::PendingWrite => db.unwind_if_revision_cancelled(),
            OwnerExit::Panic => panic!("controlled query owner panic"),
        }
    }

    42
}

fn shared_query_definition(
    ctx: &crate::frozen_context::LatencyRequestContext,
    params: lsp_types::GotoDefinitionParams,
) -> anyhow::Result<Option<lsp_types::GotoDefinitionResponse>> {
    let db = ctx.analysis.database();
    let file_id = ctx.file_id_for_url(&params.text_document_position_params.text_document.uri)?;
    let key = base_db::FileIdInput::new(db, file_id);
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| shared_blocking_query(db, key)))
    {
        Ok(42) => {}
        Ok(other) => panic!("unexpected shared query value {other}"),
        Err(payload) => {
            if matches!(
                payload.downcast_ref::<salsa::Cancelled>(),
                Some(salsa::Cancelled::PropagatedPanic)
            ) {
                let gate = shared_query_gate()
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .as_ref()
                    .cloned()
                    .expect("controlled query gate remains installed through retry");
                gate.waiter_observed_propagated.store(true, Ordering::Release);
            }
            std::panic::resume_unwind(payload);
        }
    }
    g1_definition(ctx, params)
}

fn install_gate(exit: OwnerExit) -> (Arc<SharedQueryGate>, Receiver<()>, Sender<()>) {
    let (entered_tx, entered_rx) = crossbeam_channel::bounded(1);
    let (release_tx, release_rx) = crossbeam_channel::bounded(1);
    let gate = Arc::new(SharedQueryGate {
        exit,
        entered: entered_tx,
        release: release_rx,
        calls: AtomicUsize::new(0),
        waiter_observed_propagated: AtomicBool::new(false),
    });
    let previous = shared_query_gate()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .replace(Arc::clone(&gate));
    assert!(previous.is_none(), "only one controlled query may be installed");
    (gate, entered_rx, release_tx)
}

fn remove_gate(gate: &Arc<SharedQueryGate>) {
    let removed = shared_query_gate()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take()
        .expect("controlled query gate remains installed");
    assert!(Arc::ptr_eq(&removed, gate));
}

fn live_definition_fixture() -> (
    tempfile::TempDir,
    crate::global_state::GlobalState,
    Receiver<Message>,
    std::path::PathBuf,
    lsp_types::Url,
    serde_json::Value,
) {
    let dir = tempfile::tempdir().expect("create live VFS test workspace");
    let (sender, client_receiver) = crossbeam_channel::unbounded();
    let mut state = crate::global_state::GlobalState::new(sender);
    *state.analysis_host.raw_database_mut() = ide_db::RootDatabaseImpl::new_with_salsa_events();
    state.init_empty_source_root();

    state.workspace_root = Some(dir.path().to_path_buf());
    state.project = Some(project_model::Project::new(dir.path()).expect("load fixture project"));
    let caller_dir = dir.path().join("Editor");
    std::fs::create_dir_all(&caller_dir).expect("create open-document subtree");
    let caller = caller_dir.join("Caller.bsl");
    let changed = dir.path().join("Other.bsl");
    let caller_text = "Процедура TargetProcedure()\nКонецПроцедуры\n\nПроцедура Caller()\n    TargetProcedure();\nКонецПроцедуры\n";
    let changed_text = "Процедура Other()\nКонецПроцедуры\n";
    std::fs::write(&caller, caller_text).expect("write caller fixture");
    std::fs::write(&changed, changed_text).expect("write unrelated fixture");
    let abs = |path: &std::path::Path| paths::AbsPathBuf::assert_utf8(path.to_path_buf());
    let caller_uri = lsp_types::Url::from_file_path(&caller).expect("caller file URI");

    handle_loader_msg(
        &mut state,
        vfs::loader::Message::Loaded {
            files: vec![
                (abs(&caller), Some(caller_text.as_bytes().to_vec())),
                (abs(&changed), Some(changed_text.as_bytes().to_vec())),
            ],
        },
    )
    .expect("load semantic fixture through the VFS handler");

    let mut file_set = vfs::file_set::FileSet::new();
    let file_ids: Vec<_> = [&caller, &changed]
        .into_iter()
        .map(|path| {
            let vfs_path = vfs::VfsPath::new(path.as_path());
            let file_id = state.vfs.read().file_id(&vfs_path).expect("loaded VFS file id");
            file_set.insert(file_id, vfs_path);
            file_id
        })
        .collect();
    {
        let db = state.analysis_host.raw_database_mut();
        db.set_source_root(BSL_SOURCE_ROOT, SourceRoot::new_local(file_set));
        for file_id in file_ids {
            db.set_file_source_root(file_id, BSL_SOURCE_ROOT);
        }
    }
    handle_loader_msg(
        &mut state,
        vfs::loader::Message::Progress {
            n_total: 2,
            n_done: vfs::loader::LoadingProgress::Finished,
            dir: None,
            config_version: 0,
        },
    )
    .expect("finish project loading through the VFS handler");
    assert!(state.vfs_done);
    state.mem_docs.insert(caller_uri.clone(), caller_text.to_owned(), 1);

    let task_receiver = state.task_pool.receiver.clone();
    RequestDispatcher {
        req: Some(g1_definition_request(730, caller_uri.clone())),
        global_state: &mut state,
    }
    .on_latency::<GotoDefinition>(g1_definition)
    .finish();
    let control = g1_wait_for_result(
        &mut state,
        &task_receiver,
        &client_receiver,
        lsp_server::RequestId::from(730),
    );
    assert!(control.error.is_none(), "baseline definition succeeds: {control:?}");
    let result = control.result.expect("baseline definition result");
    let result = if result.is_array() { result[0].clone() } else { result };
    assert_eq!(result["uri"], caller_uri.as_str());
    assert_eq!(result["range"]["start"]["line"], 0);
    let caller_file_id = state
        .vfs
        .read()
        .file_id(&vfs::VfsPath::new(caller.clone()))
        .expect("loaded caller VFS identity");
    state.open_files.insert(caller_file_id);

    (dir, state, client_receiver, changed, caller_uri, result)
}

fn wait_until_blocked(db: &ide_db::RootDatabaseImpl, baseline: u64) {
    let deadline = Instant::now() + HANG_GUARD;
    loop {
        let blocked = db
            .salsa_event_report()
            .expect("test database records Salsa events")
            .into_iter()
            .any(|row| row.name.contains("shared_blocking_query") && row.block_on > baseline);
        if blocked {
            return;
        }
        assert!(Instant::now() < deadline, "LSP reader never blocked on shared Salsa query");
        std::thread::yield_now();
    }
}

#[test]
fn g1_removed_recursive_invalidates_queued_origin_and_preserves_open_buffer() {
    g1_with_tracing(|| {
        let (_dir, mut state, client_receiver, changed, caller_uri, control) =
            live_definition_fixture();
        let task_receiver = state.task_pool.receiver.clone();
        let request_id = lsp_server::RequestId::from(734);
        let baseline = state
            .analysis_host
            .raw_database()
            .salsa_event_global()
            .expect("test database records Salsa cancellation events")
            .set_cancellation;
        G1_READER_ENTERED.store(false, Ordering::Release);
        G1_READER_SAW_WRITE.store(false, Ordering::Release);
        G1_CANCEL_FLAG_BASELINE.store(baseline, Ordering::Release);
        RequestDispatcher {
            req: Some(g1_definition_request(734, caller_uri.clone())),
            global_state: &mut state,
        }
        .on_retryable_latency::<GotoDefinition>(g1_definition_waiting_for_vfs_write)
        .finish();

        let deadline = Instant::now() + HANG_GUARD;
        while !G1_READER_ENTERED.load(Ordering::Acquire) {
            assert!(Instant::now() < deadline, "semantic reader did not enter");
            std::thread::yield_now();
        }
        let writer_text = "Процедура Other()\n    Значение = 1;\nКонецПроцедуры\n";
        std::fs::write(&changed, writer_text).expect("write actual VFS-change contents");
        handle_loader_msg(
            &mut state,
            vfs::loader::Message::Changed {
                files: vec![(
                    paths::AbsPathBuf::assert_utf8(changed.clone()),
                    Some(writer_text.as_bytes().to_vec()),
                )],
            },
        )
        .expect("apply production Changed event before retry admission");
        assert!(G1_READER_SAW_WRITE.load(Ordering::Acquire));
        assert!(
            state.analysis_host.raw_database().salsa_event_global().unwrap().set_cancellation
                > baseline,
            "the actual VFS writer sets Salsa cancellation"
        );

        let completion = loop {
            let task = task_receiver
                .recv_timeout(HANG_GUARD)
                .expect("writer-cancelled semantic attempt completes");
            if matches!(&task, Task::LatencyRequestFinished { key, .. } if key.id == request_id) {
                break task;
            }
            handle_task(&mut state, task).expect("apply unrelated writer task");
        };
        let Task::LatencyRequestFinished { key, outcome } = completion else {
            unreachable!("matched latency completion")
        };
        assert_eq!(key.attempt, 1);
        assert!(matches!(
            &outcome,
            crate::global_state::LatencyAttemptOutcome::Cancelled {
                kind: salsa::Cancelled::PendingWrite,
                ..
            }
        ));
        handle_task(&mut state, Task::LatencyRequestFinished { key: key.clone(), outcome })
            .expect("production handler queues the real write-cancelled request");
        assert!(state.request_lifecycles.get(&request_id).is_some_and(|lifecycle| {
            lifecycle.phase == RequestLifecyclePhase::RetryQueued && lifecycle.attempt == 2
        }));
        assert_eq!(state.pending_latency_retries.len(), 1);
        let original_origin = state.request_lifecycles[&request_id]
            .origin
            .clone()
            .expect("definition request keeps its origin");

        let caller_path = caller_uri.to_file_path().expect("caller has a file URI");
        let removed_directory = caller_path.parent().expect("caller has an editor directory");
        std::fs::remove_dir_all(removed_directory).expect("remove open document directory");
        handle_loader_msg(
            &mut state,
            vfs::loader::Message::RemovedRecursive {
                paths: vec![paths::AbsPathBuf::assert_utf8(removed_directory.to_path_buf())],
            },
        )
        .expect("apply coalesced recursive removal through production loader handler");

        let file_id = state
            .vfs
            .read()
            .file_id(&vfs::VfsPath::new(caller_path.clone()))
            .expect("removed open document retains its VFS identity");
        assert!(!request_origin_matches(&state, &original_origin));
        assert_ne!(state.mem_docs.identity(&caller_uri), Some(original_origin.identity));
        assert_eq!(state.mem_docs.get(&caller_uri).as_deref(), Some(
            "Процедура TargetProcedure()\nКонецПроцедуры\n\nПроцедура Caller()\n    TargetProcedure();\nКонецПроцедуры\n"
        ));
        assert_eq!(
            &*state.analysis_host.raw_database().file_text(file_id),
            state.mem_docs.get(&caller_uri).as_deref().unwrap(),
            "recursive removal preserves the authoritative editor buffer"
        );
        assert_eq!(state.pending_latency_retries.len(), 1);
        assert!(state.task_pool.pool.has_capacity());
        admit_pending_latency_retries(&mut state).expect("reject the now-stale original position");

        let response =
            g1_wait_for_result(&mut state, &task_receiver, &client_receiver, request_id.clone());
        assert_eq!(response.id, request_id);
        assert_eq!(
            response.error.as_ref().map(|error| error.code),
            Some(lsp_server::ErrorCode::ContentModified as i32)
        );
        assert_eq!(
            response.error.as_ref().map(|error| error.message.as_str()),
            Some("content modified: document origin changed")
        );
        assert!(!state.request_lifecycles.contains_key(&response.id));
        assert!(state.request_tokens.is_empty());
        assert!(state.pending_latency_retries.is_empty());
        tracing::info!(
            g1_marker = "recursive_removal_refused_queued_retry",
            request_id = ?response.id,
            buffer_preserved = true,
            attempt = 1,
            retries = 0,
            response = "ContentModified"
        );

        let (drained_tasks, completed_barriers) =
            g1_drain_task_pool_to_fixed_point(&mut state, &task_receiver);
        assert!(state.request_lifecycles.is_empty());
        assert!(state.request_tokens.is_empty());
        assert!(state.pending_latency_retries.is_empty());
        while let Ok(message) = client_receiver.try_recv() {
            if let Message::Response(other) = message {
                assert_ne!(other.id, request_id, "no duplicate response or second attempt");
            }
        }
        tracing::info!(
            g1_marker = "recursive_removal_background_tasks_drained",
            request_id = ?response.id,
            drained_tasks,
            completed_barriers,
            semantic_lifecycle_empty = true,
        );

        let fresh = g1_framed_request_response(
            &mut state,
            g1_definition_request(735, caller_uri.clone()),
            &task_receiver,
            &client_receiver,
        );
        assert!(fresh.error.is_none(), "fresh request still resolves the open buffer: {fresh:?}");
        let fresh_result = fresh.result.expect("fresh semantic response has a result");
        let fresh_location = if fresh_result.is_array() { &fresh_result[0] } else { &fresh_result };
        assert_eq!(fresh_location, &control);
        g1_wait_for_task_pool_quiescence(&mut state, &task_receiver);
    });
}

#[test]
fn g1_propagated_cancellation_from_live_writer_retries_only_after_new_revision() {
    g1_with_tracing(|| {
        let _panic_guard = crate::panic_watch::panic_test_guard();
        crate::panic_watch::install();
        crate::panic_watch::reset_for_test();
        let (_dir, mut state, client_receiver, changed, caller_uri, control) =
            live_definition_fixture();
        let db = state.analysis_host.raw_database().clone();
        let old_revision = salsa::plumbing::current_revision(&db);
        let cancellation_baseline = db
            .salsa_event_global()
            .expect("test database records cancellation events")
            .set_cancellation;
        let block_baseline = db
            .salsa_event_report()
            .expect("test database records Salsa events")
            .into_iter()
            .find(|row| row.name.contains("shared_blocking_query"))
            .map_or(0, |row| row.block_on);
        let (gate, owner_entered, release_owner) = install_gate(OwnerExit::PendingWrite);
        let _clear_gate = ClearQueryGateOnDrop;
        let owner_db = db.clone();
        let caller_file_id = state
            .vfs
            .read()
            .file_id(&vfs::VfsPath::new(caller_uri.to_file_path().unwrap()))
            .expect("caller is present in live VFS");
        let waiter_key = base_db::FileIdInput::new(&db, caller_file_id);
        let owner_key_control = base_db::FileIdInput::new(&owner_db, caller_file_id);
        assert_eq!(
            owner_key_control, waiter_key,
            "the cloned Salsa owner and LSP waiter intern the same file key"
        );
        let owner = std::thread::spawn(move || {
            let owner_key = base_db::FileIdInput::new(&owner_db, caller_file_id);
            shared_blocking_query(&owner_db, owner_key)
        });
        owner_entered
            .recv_timeout(HANG_GUARD)
            .expect("peer owns the tracked query before the LSP request");

        let task_receiver = state.task_pool.receiver.clone();
        RequestDispatcher {
            req: Some(g1_definition_request(731, caller_uri.clone())),
            global_state: &mut state,
        }
        .on_retryable_latency::<GotoDefinition>(shared_query_definition)
        .finish();
        wait_until_blocked(&db, block_baseline);
        drop(db);

        let writer_text = "Процедура Other()\nЗначение = 1;\nКонецПроцедуры\n";
        std::fs::write(&changed, writer_text).expect("write changed source file");
        let observer_db = state.analysis_host.raw_database().clone();
        let observed_writer = std::thread::spawn(move || {
            let deadline = Instant::now() + HANG_GUARD;
            let mut observed = false;
            loop {
                let cancellation = observer_db
                    .salsa_event_global()
                    .expect("test database records cancellation events")
                    .set_cancellation;
                if cancellation > cancellation_baseline {
                    observed = true;
                    break;
                }
                if Instant::now() >= deadline {
                    break;
                }
                std::thread::yield_now();
            }
            drop(observer_db);
            release_owner.send(()).expect("release query owner after Salsa cancellation");
            observed
        });
        handle_loader_msg(
            &mut state,
            vfs::loader::Message::Changed {
                files: vec![(
                    paths::AbsPathBuf::assert_utf8(changed.clone()),
                    Some(writer_text.as_bytes().to_vec()),
                )],
            },
        )
        .expect("production VFS handler publishes the changed input");
        assert!(observed_writer.join().expect("Salsa cancellation observer completes"));
        let owner_result = owner.join().expect_err("writer cancels the owner query");
        assert!(
            matches!(
                owner_result.downcast_ref::<salsa::Cancelled>(),
                Some(salsa::Cancelled::PendingWrite)
            ),
            "owner exits through Salsa PendingWrite"
        );

        let request_id = lsp_server::RequestId::from(731);
        let first_attempt = loop {
            let task = task_receiver
                .recv_timeout(HANG_GUARD)
                .expect("first LSP attempt did not complete");
            if matches!(
                &task,
                crate::global_state::Task::LatencyRequestFinished { key, .. } if key.id == request_id
            ) {
                break task;
            }
            handle_task(&mut state, task)
                .expect("apply unrelated task before first attempt result");
        };
        let crate::global_state::Task::LatencyRequestFinished { key, outcome } = first_attempt
        else {
            unreachable!("matched latency completion")
        };
        assert_eq!(key.id, request_id);
        assert_eq!(key.attempt, 1);
        assert!(matches!(
            &outcome,
            crate::global_state::LatencyAttemptOutcome::Cancelled {
                kind: salsa::Cancelled::PropagatedPanic,
                revision,
            } if *revision == old_revision
        ));
        handle_task(
            &mut state,
            crate::global_state::Task::LatencyRequestFinished { key: key.clone(), outcome },
        )
        .expect("production event loop queues retry for actual PropagatedPanic");
        assert!(state.request_lifecycles.get(&request_id).is_some_and(|lifecycle| {
            lifecycle.phase == RequestLifecyclePhase::RetryQueued && lifecycle.attempt == 2
        }));
        assert!(
        !client_receiver.try_iter().any(
            |message| matches!(message, Message::Response(response) if response.id == request_id)
        ),
        "first attempt emits no intermediate response"
    );
        admit_pending_latency_retries(&mut state)
            .expect("admit original request through production router");
        let retry_task = loop {
            let task = task_receiver
                .recv_timeout(HANG_GUARD)
                .expect("production retry did not complete");
            if matches!(
                &task,
                crate::global_state::Task::LatencyRequestFinished { key, .. } if key.id == request_id
            ) {
                break task;
            }
            handle_task(&mut state, task)
                .expect("apply unrelated task before production retry result");
        };
        let crate::global_state::Task::LatencyRequestFinished { key, outcome } = retry_task else {
            unreachable!("matched retry completion")
        };
        assert_eq!(key.id, request_id);
        assert_eq!(key.attempt, 2, "exactly one retry attempt is admitted");
        let crate::global_state::LatencyAttemptOutcome::Response(response) = outcome else {
            panic!("production definition retry must complete successfully")
        };
        assert!(response.error.is_none(), "writer-cancelled request retries: {response:?}");
        let result = response.result.as_ref().expect("retried definition result").clone();
        let result = if result.is_array() { result[0].clone() } else { result };
        assert_eq!(result, control, "fresh attempt preserves semantic result");
        handle_task(
            &mut state,
            crate::global_state::Task::LatencyRequestFinished {
                key: key.clone(),
                outcome: crate::global_state::LatencyAttemptOutcome::Response(response),
            },
        )
        .expect("production event loop sends the single successful response");
        let wire_response = recv_client_response(&client_receiver, request_id.clone());
        assert_eq!(wire_response.id, request_id);
        assert!(wire_response.error.is_none());
        assert!(gate.waiter_observed_propagated.load(Ordering::Acquire));
        assert_eq!(
            gate.calls.load(Ordering::Acquire),
            1,
            "the production retry does not reuse the test gate"
        );
        assert!(
            !crate::panic_watch::panicked_in(old_revision),
            "Salsa cancellation does not stamp the panic watch"
        );
        let new_revision = salsa::plumbing::current_revision(state.analysis_host.raw_database());
        assert!(new_revision > old_revision, "the actual writer publishes a newer revision");
        assert_eq!(
            &*state
                .analysis_host
                .raw_database()
                .file_text(state.vfs.read().file_id(&vfs::VfsPath::new(changed)).unwrap()),
            writer_text
        );
        assert!(state.vfs_done, "a live Changed package preserves readiness");
        assert!(!state.request_lifecycles.contains_key(&request_id));
        assert!(!state.request_tokens.contains_key(&request_id));
        assert!(state.pending_latency_retries.is_empty());
        assert!(task_receiver.try_recv().is_err(), "no request completion remains queued");
        assert!(
            client_receiver.try_iter().all(|message| !matches!(message, Message::Response(_))),
            "retry emitted exactly one response"
        );
        remove_gate(&gate);
        g1_wait_for_task_pool_quiescence(&mut state, &task_receiver);
        crate::panic_watch::reset_for_test();
    });
}

#[test]
fn g1_propagated_cancellation_from_real_query_panic_is_terminal_internal_error() {
    g1_with_tracing(|| {
        let _panic_guard = crate::panic_watch::panic_test_guard();
        crate::panic_watch::install();
        crate::panic_watch::reset_for_test();
        let (_dir, mut state, client_receiver, _changed, caller_uri, _control) =
            live_definition_fixture();
        let db = state.analysis_host.raw_database().clone();
        let attempt_revision = salsa::plumbing::current_revision(&db);
        let block_baseline = db
            .salsa_event_report()
            .expect("test database records Salsa events")
            .into_iter()
            .find(|row| row.name.contains("shared_blocking_query"))
            .map_or(0, |row| row.block_on);
        let (gate, owner_entered, release_owner) = install_gate(OwnerExit::Panic);
        let _clear_gate = ClearQueryGateOnDrop;
        let owner_db = db.clone();
        let caller_file_id = state
            .vfs
            .read()
            .file_id(&vfs::VfsPath::new(caller_uri.to_file_path().unwrap()))
            .expect("caller is present in live VFS");
        let waiter_key = base_db::FileIdInput::new(&db, caller_file_id);
        let owner_key_control = base_db::FileIdInput::new(&owner_db, caller_file_id);
        assert_eq!(
            owner_key_control, waiter_key,
            "the cloned Salsa owner and LSP waiter intern the same file key"
        );
        let owner = std::thread::spawn(move || {
            let owner_key = base_db::FileIdInput::new(&owner_db, caller_file_id);
            shared_blocking_query(&owner_db, owner_key)
        });
        owner_entered
            .recv_timeout(HANG_GUARD)
            .expect("peer owns the tracked query before the LSP request");

        let task_receiver = state.task_pool.receiver.clone();
        RequestDispatcher {
            req: Some(g1_definition_request(732, caller_uri)),
            global_state: &mut state,
        }
        .on_retryable_latency::<GotoDefinition>(shared_query_definition)
        .finish();
        wait_until_blocked(&db, block_baseline);
        drop(db);
        release_owner.send(()).expect("release panicking query owner after waiter blocks");
        let owner_result = owner.join().expect_err("query owner panics");
        assert!(!owner_result.is::<salsa::Cancelled>());

        let request_id = lsp_server::RequestId::from(732);
        let first_attempt = loop {
            let task = task_receiver
                .recv_timeout(HANG_GUARD)
                .expect("panicking LSP attempt did not complete");
            if matches!(
                &task,
                crate::global_state::Task::LatencyRequestFinished { key, .. } if key.id == request_id
            ) {
                break task;
            }
            handle_task(&mut state, task).expect("apply unrelated task before panic result");
        };
        let crate::global_state::Task::LatencyRequestFinished { key, outcome } = first_attempt
        else {
            unreachable!("matched latency completion")
        };
        assert_eq!(key.id, request_id);
        assert_eq!(key.attempt, 1);
        assert!(matches!(
            &outcome,
            crate::global_state::LatencyAttemptOutcome::Cancelled {
                kind: salsa::Cancelled::PropagatedPanic,
                revision,
            } if *revision == attempt_revision
        ));
        handle_task(&mut state, crate::global_state::Task::LatencyRequestFinished { key, outcome })
            .expect("production event loop terminally reports real panic");
        let response = recv_client_response(&client_receiver, request_id.clone());
        assert_eq!(response.id, request_id);
        assert_eq!(
            response.error.as_ref().map(|error| error.code),
            Some(lsp_server::ErrorCode::InternalError as i32),
            "a real panic must remain a terminal internal error"
        );
        assert!(gate.waiter_observed_propagated.load(Ordering::Acquire));
        assert_eq!(gate.calls.load(Ordering::Acquire), 1, "a real panic does not retry");
        assert!(crate::panic_watch::panicked_in(attempt_revision));
        assert_eq!(
            salsa::plumbing::current_revision(state.analysis_host.raw_database()),
            attempt_revision,
            "a panic without a writer does not publish a newer revision"
        );
        assert!(state.request_lifecycles.is_empty());
        assert!(state.request_tokens.is_empty());
        assert!(state.pending_latency_retries.is_empty());
        assert!(task_receiver.try_recv().is_err(), "no retry task remains queued");
        assert!(
            client_receiver.try_iter().all(|message| !matches!(message, Message::Response(_))),
            "panic produced exactly one terminal response"
        );
        remove_gate(&gate);
        g1_wait_for_task_pool_quiescence(&mut state, &task_receiver);
        crate::panic_watch::reset_for_test();
    });
}

#[test]
fn g1_propagated_without_new_revision_is_refused_controller_control() {
    g1_with_tracing(|| {
        let _panic_guard = crate::panic_watch::panic_test_guard();
        crate::panic_watch::reset_for_test();
        let (_dir, mut state, client_receiver, _changed, caller_uri, _control) =
            live_definition_fixture();
        let task_receiver = state.task_pool.receiver.clone();
        let request_id = lsp_server::RequestId::from(733);
        let key = crate::global_state::RequestAttemptKey {
            id: request_id.clone(),
            generation: 1,
            attempt: 1,
        };
        let origin = crate::global_state::RequestOriginGuard {
            identity: state.mem_docs.identity(&caller_uri).unwrap(),
            uri: caller_uri.clone(),
            workspace_root: state.workspace_root.clone(),
            source_roots: state.project.as_ref().unwrap().source_roots(),
        };
        state.request_lifecycles.insert(
            request_id.clone(),
            crate::global_state::RequestLifecycle {
                generation: key.generation,
                method: GotoDefinition::METHOD.to_owned(),
                request: Some(g1_definition_request(733, caller_uri)),
                attempt: key.attempt,
                phase: RequestLifecyclePhase::Running,
                retry_origin_revision: None,
                retry_eligible: true,
                origin: Some(origin),
                client_cancelled: false,
            },
        );
        let revision = salsa::plumbing::current_revision(state.analysis_host.raw_database());
        handle_task(
            &mut state,
            crate::global_state::Task::LatencyRequestFinished {
                key,
                outcome: crate::global_state::LatencyAttemptOutcome::Cancelled {
                    kind: salsa::Cancelled::PropagatedPanic,
                    revision,
                },
            },
        )
        .expect("controller refusal passes through the production event-loop decision");
        let response = recv_client_response(&client_receiver, request_id.clone());
        assert_eq!(response.id, request_id);
        assert_eq!(
            response.error.as_ref().map(|error| error.code),
            Some(lsp_server::ErrorCode::ContentModified as i32)
        );
        assert_eq!(
            response.error.as_ref().map(|error| error.message.as_str()),
            Some("content modified: no newer workspace revision")
        );
        assert!(state.request_lifecycles.is_empty());
        assert!(state.pending_latency_retries.is_empty());
        assert!(client_receiver.try_recv().is_err());
        g1_wait_for_task_pool_quiescence(&mut state, &task_receiver);
    });
}
