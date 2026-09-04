//! The cancellation gates of the search path.
//!
//! Every stand here puts a search where it REALLY waits — a held engine lock, a latched
//! baseline actor, a silent embedder, a latched resident — and cancels it there. A fast
//! search cancelled before it waits is green under any implementation and proves nothing.
//! Each cancel case carries its own positive control: the same call, left alone, does get
//! answered once the wait ends, so the early return is the cancellation's doing.

use super::docs::{find_docs, search_docs};
use super::hybrid::hybrid_code_cancellable;
use super::lexical::lexical_code_hits;
use super::semantic::semantic_code_hits;
use super::test_support::latched_service;
use super::try_acquire_engine;
use super::types::{CodeHits, SearchFailure};
use crate::state::{SemanticRuntimeStatus, WorkspaceSearchMode};
use bsl_search::{EmbedderConfig, IndexProgress, SearchConfig, SearchEngine};
use rmcp::model::CallToolResult;
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

type SharedEngine = Arc<Mutex<Option<SearchEngine>>>;
type SearchCall = Box<
    dyn Fn(&SharedEngine, &CancellationToken) -> Result<CallToolResult, SearchFailure>
        + Send
        + Sync,
>;

/// How long a cancelled call may take to come back. Well above one poll (25 ms) and well
/// below every wait it must not sit out (a 2 s held lock, a 12 s embed, a latched actor).
const CANCEL_BOUND: Duration = Duration::from_millis(500);

fn fts_engine(dir: &TempDir) -> SharedEngine {
    let engine = SearchEngine::fts_only(&dir.path().join("search.db")).unwrap();
    Arc::new(Mutex::new(Some(engine)))
}

/// An engine with an embedder pointed at `base_url`, so a semantic query really embeds.
fn semantic_engine(dir: &TempDir, base_url: String) -> SharedEngine {
    let config = SearchConfig {
        embedder: EmbedderConfig {
            base_url,
            model: "test-model".to_owned(),
            dim: Some(8),
            api_key: None,
            provider: None,
        },
        ..SearchConfig::default()
    };
    let engine = SearchEngine::new(&dir.path().join("search.db"), config).unwrap();
    Arc::new(Mutex::new(Some(engine)))
}

fn ready_runtime() -> Arc<Mutex<SemanticRuntimeStatus>> {
    Arc::new(Mutex::new(SemanticRuntimeStatus::Ready))
}

/// Run `call` on its own thread and hand back the outcome with how long it took.
fn on_thread<T: Send + 'static>(
    call: impl FnOnce() -> T + Send + 'static,
) -> mpsc::Receiver<(T, Duration)> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let started = Instant::now();
        let out = call();
        let _ = tx.send((out, started.elapsed()));
    });
    rx
}

/// The three search actions as one shape, so a gate can run each of them through the same
/// stand. `find_docs` and `search_docs` are the same function in both profiles; the profile
/// matrix proper lives at the handler level.
fn actions() -> Vec<(&'static str, SearchCall)> {
    vec![
        (
            "search_code",
            Box::new(|engine, cancel| {
                hybrid_code_cancellable(
                    engine,
                    cancel,
                    &ready_runtime(),
                    WorkspaceSearchMode::SqliteLocal,
                    None,
                    None,
                    None,
                    &IndexProgress::new(),
                    "Процедура",
                    10,
                    usize::MAX,
                )
            }),
        ),
        (
            "find_docs",
            Box::new(|engine, cancel| {
                find_docs(engine, cancel, None, None, "Массив", 10, usize::MAX)
            }),
        ),
        (
            "search_docs",
            Box::new(|engine, cancel| {
                search_docs(engine, cancel, None, None, "Массив", 10, usize::MAX)
            }),
        ),
    ]
}

/// I1 — a search queued on a held engine lock stops waiting when its request is cancelled.
///
/// The positive control in the same stand: left alone, the same call queues until the
/// lock frees and is then ANSWERED (on this `fts_only` engine `search_docs`'s answer is
/// its "no semantic" refusal — an answer all the same, and one that proves the queueing).
#[test]
fn a_search_waiting_for_the_engine_lock_returns_at_its_cancellation() {
    const HOLD: Duration = Duration::from_millis(1500);
    for (name, call) in actions() {
        let dir = tempfile::tempdir().unwrap();
        let engine = fts_engine(&dir);
        let call = Arc::new(call);

        // Cancel case.
        let held = engine.lock().unwrap();
        let cancel = CancellationToken::new();
        let outcome = on_thread({
            let engine = Arc::clone(&engine);
            let cancel = cancel.clone();
            let call = Arc::clone(&call);
            move || call(&engine, &cancel)
        });
        std::thread::sleep(Duration::from_millis(50));
        cancel.cancel();
        let (out, waited) = outcome
            .recv_timeout(HOLD * 2)
            .unwrap_or_else(|_| panic!("{name}: the cancelled call never came back"));
        assert!(
            matches!(out, Err(SearchFailure::Cancelled)),
            "{name}: expected Cancelled, got {out:?}"
        );
        assert!(waited < CANCEL_BOUND, "{name}: waited {waited:?} past its cancellation");

        // Positive control: an uncancelled call queues until the lock frees, then answers.
        let outcome = on_thread({
            let engine = Arc::clone(&engine);
            let call = Arc::clone(&call);
            move || call(&engine, &CancellationToken::new())
        });
        std::thread::sleep(HOLD);
        drop(held);
        let (out, waited) = outcome.recv_timeout(Duration::from_secs(30)).unwrap();
        assert!(!matches!(out, Err(SearchFailure::Cancelled)), "{name}: control must be answered");
        assert!(waited >= HOLD / 2, "{name}: the control did not queue on the lock: {waited:?}");
    }
}

/// I3 — a search holding the engine guard while it waits on the actor releases the guard
/// at its cancellation: the next search takes the lock while the actor is still latched.
#[test]
fn a_cancelled_search_waiting_on_the_actor_releases_the_engine_lock() {
    let dir = tempfile::tempdir().unwrap();
    let engine = fts_engine(&dir);
    let (service, latch) = latched_service(&["resolve_snapshot"]);

    let cancel = CancellationToken::new();
    let first = on_thread({
        let engine = Arc::clone(&engine);
        let cancel = cancel.clone();
        let service = Arc::clone(&service);
        move || {
            lexical_code_hits(
                &engine,
                &cancel,
                WorkspaceSearchMode::SqliteLocal,
                None,
                Some(service),
                "Процедура",
                10,
            )
        }
    });
    latch.wait_started(1);
    assert!(engine.try_lock().is_err(), "sanity: the first search holds the engine while it waits");

    cancel.cancel();
    let (out, waited) =
        first.recv_timeout(Duration::from_secs(5)).expect("the cancelled search returns");
    assert!(matches!(out, Err(SearchFailure::Cancelled)), "got {:?}", out.map(|_| ()));
    assert!(waited < CANCEL_BOUND, "waited {waited:?} past the cancellation");

    // The actor is still latched, and the lock is free for the next caller.
    assert_eq!(latch.executed(), Vec::<&str>::new(), "the latched request has not answered");
    let acquired = Instant::now();
    let guard = try_acquire_engine(&engine, &CancellationToken::new());
    assert!(guard.is_ok(), "the next search must take the engine lock");
    assert!(acquired.elapsed() < CANCEL_BOUND, "the lock was still held: {:?}", acquired.elapsed());
    drop(guard);

    latch.release_one();
    service.shutdown();
}

/// I10 — the identity gate is one more wait on the actor under the guard, and the one whose
/// failures otherwise fall through and continue; a withdrawn wait there releases the engine.
#[test]
fn a_cancelled_identity_gate_releases_the_engine_lock() {
    let dir = tempfile::tempdir().unwrap();
    let engine = semantic_engine(&dir, "http://127.0.0.1:1".to_owned());
    let (service, latch) = latched_service(&["embedding_identity"]);

    let cancel = CancellationToken::new();
    let first = on_thread({
        let engine = Arc::clone(&engine);
        let cancel = cancel.clone();
        let service = Arc::clone(&service);
        move || {
            semantic_code_hits(
                &engine,
                &cancel,
                &ready_runtime(),
                WorkspaceSearchMode::SqliteLocal,
                None,
                Some(service),
                "Процедура",
                10,
            )
        }
    });
    latch.wait_started(1);
    assert!(engine.try_lock().is_err(), "sanity: the identity gate runs under the guard");

    cancel.cancel();
    let (out, waited) =
        first.recv_timeout(Duration::from_secs(5)).expect("the cancelled search returns");
    assert!(matches!(out, Err(SearchFailure::Cancelled)), "got {:?}", out.map(|_| ()));
    assert!(waited < CANCEL_BOUND, "waited {waited:?} past the cancellation");

    let acquired = Instant::now();
    let guard = try_acquire_engine(&engine, &CancellationToken::new());
    assert!(guard.is_ok(), "the next search must take the engine lock");
    assert!(acquired.elapsed() < CANCEL_BOUND, "the lock was still held: {:?}", acquired.elapsed());
    drop(guard);

    latch.release_one();
    service.shutdown();
}

/// I7 — a docs search cancelled while it waits on the actor produces no body and runs no
/// fallback: the whole-corpus view load and the local search that follow a transient
/// failure must not follow a cancellation.
#[test]
fn a_cancelled_docs_search_does_not_fall_back_to_the_corpus_view() {
    let dir = tempfile::tempdir().unwrap();
    let engine = fts_engine(&dir);
    let (service, latch) = latched_service(&["lexical"]);

    let cancel = CancellationToken::new();
    let outcome = on_thread({
        let engine = Arc::clone(&engine);
        let cancel = cancel.clone();
        let service = Arc::clone(&service);
        move || find_docs(&engine, &cancel, None, Some(service), "Массив", 10, usize::MAX)
    });
    latch.wait_started(2);
    assert_eq!(latch.executed(), vec!["resolve_snapshot"], "the lexical query is the one latched");

    cancel.cancel();
    let (out, waited) =
        outcome.recv_timeout(Duration::from_secs(5)).expect("the cancelled search returns");
    assert!(matches!(out, Err(SearchFailure::Cancelled)), "got {:?}", out.map(|_| ()));
    assert!(waited < CANCEL_BOUND, "waited {waited:?} past the cancellation");

    latch.release_one();
    service.shutdown();
    assert_eq!(
        latch.executed(),
        vec!["resolve_snapshot", "lexical", "shutdown"],
        "the corpus-view fallback (load_reference_documents) must never be requested"
    );
}

/// A listener that accepts and never answers: the shape of an embedder that is up but slow.
struct SilentEmbedder {
    base_url: String,
    accepted: Arc<AtomicUsize>,
    streams: Arc<Mutex<Vec<std::net::TcpStream>>>,
}

impl SilentEmbedder {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let accepted = Arc::new(AtomicUsize::new(0));
        let streams = Arc::new(Mutex::new(Vec::new()));
        std::thread::spawn({
            let accepted = Arc::clone(&accepted);
            let streams = Arc::clone(&streams);
            move || {
                for stream in listener.incoming().flatten() {
                    accepted.fetch_add(1, Ordering::SeqCst);
                    streams.lock().unwrap().push(stream);
                }
            }
        });
        Self { base_url, accepted, streams }
    }

    fn accepted(&self) -> usize {
        self.accepted.load(Ordering::SeqCst)
    }

    /// Close every held connection, so a helper thread still inside its HTTP call errors
    /// out now instead of sitting out the embedder timeout.
    fn hang_up(&self) {
        self.streams.lock().unwrap().clear();
    }
}

/// I4 — the query embed holds no lock and does not hold up a cancellation, on every path
/// that embeds: the code search, and the docs search with and without an external
/// baseline (the local tail used to embed a second time, under the lock).
#[test]
fn a_search_inside_the_query_embed_holds_no_lock_and_returns_at_its_cancellation() {
    let rows: Vec<(&str, bool)> =
        vec![("search_code", false), ("search_docs/external", true), ("search_docs/local", false)];
    for (name, external) in rows {
        let dir = tempfile::tempdir().unwrap();
        let embedder = SilentEmbedder::start();
        let engine = semantic_engine(&dir, embedder.base_url.clone());
        let service = external.then(|| latched_service(&[]));

        let cancel = CancellationToken::new();
        let outcome = on_thread({
            let engine = Arc::clone(&engine);
            let cancel = cancel.clone();
            let service = service.as_ref().map(|(service, _)| Arc::clone(service));
            move || -> Result<(), SearchFailure> {
                if name == "search_code" {
                    semantic_code_hits(
                        &engine,
                        &cancel,
                        &ready_runtime(),
                        WorkspaceSearchMode::SqliteLocal,
                        None,
                        service,
                        "Процедура",
                        10,
                    )
                    .map(|_| ())
                } else {
                    search_docs(&engine, &cancel, None, service, "Массив", 10, usize::MAX)
                        .map(|_| ())
                }
            }
        });

        // Let the call reach the embed, then look at what it holds while it waits there.
        let deadline = Instant::now() + Duration::from_secs(5);
        while embedder.accepted() == 0 {
            assert!(Instant::now() < deadline, "{name}: the embed never reached the embedder");
            std::thread::sleep(Duration::from_millis(5));
        }
        std::thread::sleep(Duration::from_millis(50));
        assert!(engine.try_lock().is_ok(), "{name}: the engine lock is held across the embed");
        assert_eq!(embedder.accepted(), 1, "{name}: exactly one embed per query");

        cancel.cancel();
        let (out, waited) = outcome
            .recv_timeout(Duration::from_secs(5))
            .unwrap_or_else(|_| panic!("{name}: the cancelled call never came back"));
        assert!(matches!(out, Err(SearchFailure::Cancelled)), "{name}: expected Cancelled");
        assert!(waited < CANCEL_BOUND, "{name}: waited {waited:?} past its cancellation");

        embedder.hang_up();
        if let Some((service, _)) = service {
            service.shutdown();
        }
    }
}

/// The lexical modality answers a cancellation the same way whether it reaches the actor
/// or not: the value, not a pending envelope. (Type-level: `CodeHits` has no cancelled
/// variant, so a cancellation cannot be rendered as a body from here.)
#[test]
fn a_cancelled_lexical_modality_is_a_failure_not_a_pending_body() {
    let dir = tempfile::tempdir().unwrap();
    let engine = fts_engine(&dir);
    let cancel = CancellationToken::new();
    cancel.cancel();
    let out = lexical_code_hits(
        &engine,
        &cancel,
        WorkspaceSearchMode::SqliteLocal,
        None,
        None,
        "Процедура",
        10,
    );
    assert!(
        !matches!(out, Ok(CodeHits::Pending(_))),
        "a cancelled call must not be a retry envelope"
    );
    assert!(matches!(out, Err(SearchFailure::Cancelled)));
}
