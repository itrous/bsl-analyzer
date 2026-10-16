use super::*;
use serde_json::Value;
use std::io::{Read, Write};

#[test]
fn profiled_constructor_blocks_unproven_vectors_but_keeps_lexical_search() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("search.db");
    let mut store = crate::Store::open(&db_path).unwrap();
    let chunks = code_chunk::Chunker::chunk("Процедура Needle()\nКонецПроцедуры");
    store
        .reindex_file(
            crate::CONFIGURATION_ROOT_ID,
            "M.bsl",
            b"legacy",
            &chunks,
            Some(&[vec![1.0_f32, 0.0]]),
        )
        .unwrap();
    drop(store);

    let embedder = EmbedderConfig {
        dim: Some(2),
        query_prefix: "query: ".to_owned(),
        ..EmbedderConfig::default()
    };
    let engine = SearchEngine::new(
        &db_path,
        SearchConfig { embedder, execution: EmbeddingExecutionPolicy::default() },
    )
    .unwrap();

    assert_eq!(engine.index.len(), 0, "legacy vectors must not enter a profiled HNSW");
    assert!(!engine.store.text_search("Needle", 1, Some("code")).unwrap().is_empty());
    assert!(!engine.store.embedding_profile_matches("profile-v1:foreign", 2).unwrap());
}

#[test]
fn profiled_embedding_pass_refuses_unproven_vectors_without_claiming_coverage() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("search.db");
    let mut store = crate::Store::open(&db_path).unwrap();
    let chunks = code_chunk::Chunker::chunk("Процедура Needle()\nКонецПроцедуры");
    store
        .reindex_file(
            crate::CONFIGURATION_ROOT_ID,
            "M.bsl",
            b"legacy",
            &chunks,
            Some(&[vec![1.0_f32, 0.0]]),
        )
        .unwrap();
    let before = store.load_all_embeddings(2).unwrap();

    let config = EmbedderConfig {
        dim: Some(2),
        query_prefix: "query: ".to_owned(),
        ..EmbedderConfig::default()
    };
    let embedder = Embedder::new(config);
    let mut apply =
        |operation: &mut dyn FnMut() -> Result<(), SearchError>| FenceOutcome::Applied(operation());
    let mut retry = || false;
    let result = SearchEngine::run_embedding_pass_owned(
        &store,
        &embedder,
        2,
        32,
        1,
        None,
        None,
        &mut apply,
        Some(&mut retry),
    );

    assert!(
        matches!(result, Err(SearchError::Index(message)) if message == "embedding profile mismatch")
    );
    assert_eq!(store.load_all_embeddings(2).unwrap(), before);
    assert!(!store.text_search("Needle", 1, Some("code")).unwrap().is_empty());
    assert!(!store.embedding_profile_matches("profile-v1:new-inputs", 2).unwrap());
}

fn capture(f: impl FnOnce()) -> Vec<Value> {
    capture_level(tracing::level_filters::LevelFilter::DEBUG, f)
}

fn capture_level(level: tracing::level_filters::LevelFilter, f: impl FnOnce()) -> Vec<Value> {
    use tracing_subscriber::prelude::*;
    struct Capture(Arc<Mutex<Vec<Value>>>);
    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Capture {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _: tracing_subscriber::layer::Context<'_, S>,
        ) {
            struct Visitor<'a>(&'a mut Vec<Value>);
            impl tracing::field::Visit for Visitor<'_> {
                fn record_debug(&mut self, _: &tracing::field::Field, _: &dyn std::fmt::Debug) {}
                fn record_str(&mut self, field: &tracing::field::Field, text: &str) {
                    if field.name() == "record" {
                        let record: Value = serde_json::from_str(text).unwrap();
                        if record["kind"] == "startup_snapshot" {
                            CONSTRUCTOR_APPLY_ACTIVE.with(|active| {
                                assert!(!active.get(), "snapshot ran inside constructor fence")
                            });
                        }
                        self.0.push(record);
                    }
                }
            }
            if event.metadata().target() == lifecycle::TARGET {
                event.record(&mut Visitor(&mut self.0.lock().unwrap()));
            }
        }
    }
    let records = Arc::new(Mutex::new(Vec::new()));
    crate::lifecycle::test_with_subscriber(
        tracing_subscriber::registry().with(Capture(records.clone()).with_filter(level)),
        f,
    );
    Arc::try_unwrap(records).unwrap().into_inner().unwrap()
}

/// Read one complete JSON HTTP request using its declared Content-Length. Returning
/// the body before all its bytes have been consumed can make a server-side close reset
/// the connection on Windows, hiding the intended HTTP status from the client.
fn read_request_body(stream: &mut std::net::TcpStream) -> Value {
    stream.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
    let mut bytes = Vec::new();
    let mut buffer = [0; 2048];
    loop {
        let read = stream.read(&mut buffer).unwrap();
        assert_ne!(read, 0, "client closed before sending the complete HTTP request");
        bytes.extend_from_slice(&buffer[..read]);

        let Some(split) = bytes.windows(4).position(|w| w == b"\r\n\r\n") else { continue };
        let headers = String::from_utf8_lossy(&bytes[..split]).to_lowercase();
        let length = headers
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .expect("HTTP request has Content-Length")
            .trim()
            .parse::<usize>()
            .expect("Content-Length is an integer");
        let body_start = split + 4;
        if bytes.len() >= body_start + length {
            return serde_json::from_slice(&bytes[body_start..body_start + length])
                .expect("request body is JSON");
        }
    }
}

/// Finite local fake: returns exactly the requests the test expects, with no model/provider.
fn server(
    requests: usize,
    mut observe: impl FnMut() + Send + 'static,
) -> (Embedder, std::thread::JoinHandle<Vec<String>>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let thread = std::thread::spawn(move || {
        let mut submitted = Vec::new();
        for stream in listener.incoming().take(requests) {
            let mut stream = stream.unwrap();
            let body = read_request_body(&mut stream);
            observe();
            let inputs = body["input"].as_array().unwrap();
            submitted.extend(inputs.iter().map(|text| text.as_str().unwrap().to_owned()));
            let data: Vec<_> = (0..inputs.len())
                .map(|index| serde_json::json!({"index":index,"embedding":[1.0,0.0,0.0]}))
                .collect();
            let body = serde_json::json!({"data":data}).to_string();
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body).unwrap();
        }
        submitted
    });
    (
        Embedder::new(EmbedderConfig {
            base_url: format!("http://{address}"),
            model: "fixture".into(),
            dim: Some(3),
            api_key: None,
            provider: None,
            ..Default::default()
        }),
        thread,
    )
}

fn seed(path: &Path, complete: usize, pending: usize) -> Store {
    let mut store = Store::open(path).unwrap();
    for i in 0..complete + pending {
        let name = format!("Method{i}");
        let chunk = crate::Chunk {
            kind: crate::ChunkKind::Procedure,
            name: name.clone(),
            is_export: false,
            annotations: Vec::new(),
            line_start: 1,
            line_end: 2,
            text: format!("Процедура {name}()\nКонецПроцедуры"),
        };
        let vectors = vec![vec![0.0, 1.0, 0.0]];
        store
            .reindex_file(
                "",
                &format!("M{i}.bsl"),
                b"hash",
                &[chunk],
                (i < complete).then_some(vectors.as_slice()),
            )
            .unwrap();
    }
    store
}

#[test]
fn vector_lifecycle_artifact_publish_refuses_changed_generation_without_installing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("search.db");
    let store = seed(&path, 1, 0);
    let (generation, data) = store.load_all_embeddings_with_generation(3).unwrap();
    let index = VectorIndex::build(3, &data).unwrap();
    let key = crate::vector_persist::PersistKey { db_path: &path, model_id: "fixture", dim: 3 };
    let mut prepared = crate::vector_persist::prepare(&index, &key, generation).unwrap();
    store.set_chunk_embeddings(&[(data[0].0, vec![1.0, 0.0, 0.0])]).unwrap();
    let records =
        capture(|| assert!(SearchEngine::install_prepared_built(&store, &mut prepared).is_err()));
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["kind"], "artifact_publish");
    assert_eq!(records[0]["reason"], "artifact_stale");
    assert_eq!(records[0]["outcome"], "refused");
    assert_eq!(records[0]["counts"]["sqlite_vectors_removed"], 0);
    assert!(!dir.path().join("search.db.usearch").exists());
    assert_eq!(store.load_all_embeddings(3).unwrap(), vec![(data[0].0, vec![1.0, 0.0, 0.0])]);
    rusqlite::Connection::open(&path).unwrap().execute("DROP TABLE meta", []).unwrap();
    let records =
        capture(|| assert!(SearchEngine::install_prepared_built(&store, &mut prepared).is_err()));
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["reason"], "read_error");
    assert_eq!(records[0]["outcome"], "failed");
    assert_eq!(records[0]["examples"][0], "generation_read");
    assert_eq!(records[0]["counts"]["sqlite_vectors_removed"], 0);
}

fn terminal(records: &[Value]) -> &Value {
    records
        .iter()
        .rev()
        .find(|r| {
            r["kind"] == "embedding_pass" && r["outcome"] != "started" && r["outcome"] != "progress"
        })
        .unwrap()
}

#[test]
fn vector_lifecycle_partial_resume_only_submits_pending_then_warm_skip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("search.db");
    let store = seed(&path, 1, 2);
    let preserved = store.load_all_embeddings(3).unwrap()[0].clone();
    drop(store);
    let (embedder, requests) = server(2, || {});
    let records = capture(|| {
        let store = Store::open_existing(&path).unwrap();
        let (index, outcome) = SearchEngine::run_embedding_pass(
            &store,
            &embedder,
            3,
            1,
            1,
            None,
            None,
            &mut |op| FenceOutcome::Applied(op()),
            None,
        )
        .unwrap();
        assert!(matches!(outcome, FenceOutcome::Applied(())));
        assert_eq!(index.len(), 3);
        assert!(store.load_all_embeddings(3).unwrap().contains(&preserved));
    });
    let inputs = requests.join().unwrap();
    assert_eq!(inputs.len(), 2);
    assert!(inputs.iter().all(|text| !text.contains("Method0")));
    assert_eq!(terminal(&records)["outcome"], "completed");
    assert_eq!(terminal(&records)["committed_totals"]["embeddings_written"], 2);
    assert_eq!(terminal(&records)["counts"]["embedding_chunks_requested"], 2);
    assert_eq!(terminal(&records)["counts"]["embedding_chunks_submitted"], 2);
    assert_eq!(terminal(&records)["counts"]["embedding_http_requests"], 2);
    assert_eq!(terminal(&records)["pending"], 0);
    let generation = Store::open_existing(&path).unwrap().embedding_generation().unwrap();
    let records = capture(|| {
        let store = Store::open_existing(&path).unwrap();
        let (index, _) = SearchEngine::run_embedding_pass(
            &store,
            &embedder,
            3,
            1,
            1,
            None,
            None,
            &mut |op| FenceOutcome::Applied(op()),
            None,
        )
        .unwrap();
        assert_eq!(index.len(), 3);
        assert_eq!(store.embedding_generation().unwrap(), generation);
        assert!(store.load_all_embeddings(3).unwrap().contains(&preserved));
    });
    assert_eq!(terminal(&records)["outcome"], "skipped");
    assert_eq!(terminal(&records)["committed_totals"]["embeddings_written"], 0);
}

#[test]
fn interactive_query_embedding_is_not_counted_as_document_ingest() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("search.db");
    let (embedder, requests) = server(1, || {});
    let batch = Batch::new(&path, Reason::Embedding);
    let records = capture(|| {
        batch.context().in_scope(|| {
            embedder.embed("query only").unwrap();
        });
        batch.finish(Outcome::Completed);
    });
    assert_eq!(requests.join().unwrap().len(), 1);
    let terminal = terminal(&records);
    assert_eq!(terminal["counts"]["embedding_chunks_requested"], 0);
    assert_eq!(terminal["counts"]["embedding_chunks_submitted"], 0);
    assert_eq!(terminal["counts"]["embedding_http_requests"], 0);
}

#[test]
fn vector_lifecycle_refusal_and_failure_retain_earlier_committed_batch() {
    for failed in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let store = seed(&dir.path().join("search.db"), 0, 2);
        let (embedder, requests) = server(2, || {});
        let mut commits = 0;
        let records = capture(|| {
            let result = SearchEngine::run_embedding_pass(
                &store,
                &embedder,
                3,
                1,
                1,
                None,
                None,
                &mut |op| {
                    commits += 1;
                    if commits == 2 {
                        if failed {
                            FenceOutcome::Applied(Err(SearchError::Index("fixture failure".into())))
                        } else {
                            FenceOutcome::TransientRefusal
                        }
                    } else {
                        FenceOutcome::Applied(op())
                    }
                },
                None,
            );
            if failed {
                assert!(result.is_err());
            } else {
                assert!(matches!(result.unwrap().1, FenceOutcome::TransientRefusal));
            }
        });
        assert_eq!(requests.join().unwrap().len(), 2);
        assert_eq!(store.load_all_embeddings(3).unwrap().len(), 1);
        assert_eq!(store.load_pending_embedding_documents("code").unwrap().len(), 1);
        let terminal = terminal(&records);
        assert_eq!(terminal["outcome"], if failed { "failed" } else { "refused" });
        assert_eq!(terminal["committed_totals"]["embeddings_written"], 1);
        assert_eq!(terminal["pending"], 1);
    }
}

#[test]
fn late_embedding_response_cannot_write_after_rowid_reuse_for_new_payload() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("search.db");
    let mut store = Store::open(&path).unwrap();
    let old = crate::Chunk {
        kind: crate::ChunkKind::Procedure,
        name: "Method".to_owned(),
        is_export: false,
        annotations: Vec::new(),
        line_start: 1,
        line_end: 2,
        text: "Процедура Method()\nСообщить(\"old\");\nКонецПроцедуры".to_owned(),
    };
    store
        .reindex_file_with_identity(
            crate::CONFIGURATION_ROOT_ID,
            "M.bsl",
            b"old",
            &[old],
            Some(&[vec![0.0, 1.0, 0.0]]),
            None,
            "fixture",
            3,
        )
        .unwrap();
    let row_id = store.load_all_embeddings(3).unwrap()[0].0;
    store.clear_chunk_embedding(row_id).unwrap();

    let replacement_path = path.clone();
    let (embedder, requests) = server(1, move || {
        // Mutate the file after the worker has submitted its payload, but before the
        // provider response can reach the guarded SQLite write.
        let replacement = crate::Chunk {
            kind: crate::ChunkKind::Procedure,
            name: "Method".to_owned(),
            is_export: false,
            annotations: Vec::new(),
            line_start: 1,
            line_end: 2,
            text: "Процедура Method()\nСообщить(\"new\");\nКонецПроцедуры".to_owned(),
        };
        let mut concurrent = Store::open_existing(&replacement_path).unwrap();
        concurrent
            .reindex_file_with_identity(
                crate::CONFIGURATION_ROOT_ID,
                "M.bsl",
                b"new",
                &[replacement],
                None,
                None,
                "fixture",
                3,
            )
            .unwrap();
    });
    let result = SearchEngine::run_embedding_pass(
        &store,
        &embedder,
        3,
        1,
        1,
        None,
        None,
        &mut |operation| FenceOutcome::Applied(operation()),
        None,
    );
    let error = match result {
        Ok(_) => panic!("stale embedding response must be rejected"),
        Err(error) => error,
    };
    assert_eq!(error.to_string(), "index error: embedding target changed during request");
    assert_eq!(requests.join().unwrap().len(), 1);

    let pending = store.load_pending_embedding_documents("code").unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].0, row_id, "fixture must exercise SQLite rowid reuse");
    assert!(store.load_all_embeddings(3).unwrap().is_empty());
}

/// Batch totals the progress record shows while each request is in flight. A finished pass
/// clears its counters, so the plan is observable only while the pass still runs.
#[allow(clippy::type_complexity, reason = "a fixture reply paired with its sample log")]
fn sampling_batches(
    progress: &Arc<IndexProgress>,
    mut reply: impl FnMut(usize, &[u8]) -> (u16, String) + Send + 'static,
) -> (impl FnMut(usize, &[u8]) -> (u16, String) + Send + 'static, Arc<Mutex<Vec<Option<usize>>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (progress, record) = (Arc::clone(progress), Arc::clone(&seen));
    let wrapped = move |index, body: &[u8]| {
        let total = (0..10_000)
            .find_map(|_| progress.snapshot())
            .and_then(|snapshot| snapshot.counters)
            .and_then(|counters| counters.total_batches);
        record.lock().unwrap().push(total);
        reply(index, body)
    };
    (wrapped, seen)
}

#[test]
fn payload_pending_publication_retains_commits_and_recovers_in_both_paths() {
    use crate::embedder::payload_tests::{success, vector, PayloadServer};
    for fenced in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("search.db");
        let store = seed(&path, 1, 3);
        let preserved = store.load_all_embeddings(3).unwrap()[0].clone();
        let pending = store.load_pending_embedding_documents("code").unwrap();
        let texts: Vec<_> = pending
            .iter()
            .map(|(_, doc)| crate::document::semantic_text_for_indexed_document(doc))
            .collect();
        let limit = texts
            .iter()
            .map(|text| {
                serde_json::json!({"model":"fixture","input":[text],"dimensions":3})
                    .to_string()
                    .len()
            })
            .max()
            .unwrap();
        let progress = IndexProgress::new();
        let (reply, batches) = sampling_batches(&progress, |i, body| {
            if i == 1 {
                (413, "rejected".into())
            } else {
                success(i, body)
            }
        });
        let server = PayloadServer::new(reply);
        let embedder = Embedder::new(server.config(limit));
        let mut retry = || false;
        let error = SearchEngine::run_embedding_pass(
            &store,
            &embedder,
            3,
            32,
            2,
            Some(&progress),
            None,
            &mut |op| FenceOutcome::Applied(op()),
            if fenced { Some(&mut retry) } else { None },
        )
        .err()
        .expect("known later failure");
        assert_eq!(error.to_string(), "embedding_request_too_large");
        assert!(!progress.is_active());
        let planned = batches.lock().unwrap().clone();
        assert!(
            !planned.is_empty() && planned.iter().all(|total| *total == Some(3)),
            "{planned:?}"
        );
        let committed = store.load_all_embeddings(3).unwrap();
        assert!(committed.contains(&preserved));
        assert_eq!(committed.len(), if fenced { 2 } else { 3 });
        for (id, embedding) in committed.iter().filter(|row| row.0 != preserved.0) {
            let i = pending.iter().position(|(key, _)| key == id).unwrap();
            assert_eq!(*embedding, vector(&texts[i]));
        }
        assert!(!path.with_extension("db.usearch.json").exists());
        let remaining = store.load_pending_embedding_documents("code").unwrap().len();
        assert_eq!(remaining, 4 - committed.len());
        assert!(server.requests().iter().all(|body| body.len() <= limit));
        let before = server.requests().len();
        let (index, outcome) = SearchEngine::run_embedding_pass(
            &store,
            &embedder,
            3,
            32,
            2,
            Some(&progress),
            None,
            &mut |op| FenceOutcome::Applied(op()),
            if fenced { Some(&mut retry) } else { None },
        )
        .unwrap();
        assert!(matches!(outcome, FenceOutcome::Applied(())));
        assert_eq!(index.len(), 4);
        assert!(store.load_pending_embedding_documents("code").unwrap().is_empty());
        assert_eq!(server.requests().len() - before, remaining);
        assert!(path.with_extension("db.usearch.json").exists());
    }
}

#[test]
fn payload_pending_publication_owner_and_cancel_win_after_failed_request() {
    use crate::embedder::payload_tests::PayloadServer;
    use std::sync::atomic::AtomicBool;
    for fenced in [false, true] {
        for outcome in 0..4 {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("search.db");
            let store = seed(&path, 0, 1);
            let completed_request = Arc::new(AtomicBool::new(false));
            let completed = completed_request.clone();
            let server = PayloadServer::new(move |_, _| {
                completed.store(true, Ordering::Release);
                (413, "refused".into())
            });
            let embedder = Embedder::new(server.config(4096));
            let keep_going = || outcome != 3 || !completed_request.load(Ordering::Acquire);
            let mut retry = || false;
            let (_, actual) = SearchEngine::run_embedding_pass(
                &store,
                &embedder,
                3,
                32,
                1,
                None,
                Some(&keep_going),
                &mut |op| {
                    if !completed_request.load(Ordering::Acquire) {
                        return FenceOutcome::Applied(op());
                    }
                    match outcome {
                        0 => FenceOutcome::Released,
                        1 => FenceOutcome::Superseded,
                        2 => FenceOutcome::TransientRefusal,
                        _ => FenceOutcome::Applied(op()),
                    }
                },
                if fenced { Some(&mut retry) } else { None },
            )
            .unwrap();
            assert!(match outcome {
                0 | 3 => matches!(actual, FenceOutcome::Released),
                1 => matches!(actual, FenceOutcome::Superseded),
                _ => matches!(actual, FenceOutcome::TransientRefusal),
            });
            assert_eq!(server.requests().len(), 1);
            assert_eq!(store.load_pending_embedding_documents("code").unwrap().len(), 1);
            assert!(!path.with_extension("db.usearch.json").exists());
        }
    }
}

fn payload_documents() -> (String, Vec<crate::IndexedDocument>, usize) {
    let content = (0..4)
        .map(|i| {
            format!(
                "Процедура Метод{i}() Экспорт\n    Сообщить(\"данные{i}\");\nКонецПроцедуры\n\n"
            )
        })
        .collect::<String>();
    let key = crate::FileKey::configuration("Module.bsl");
    let docs: Vec<_> = crate::Chunker::chunk(&content)
        .iter()
        .map(|chunk| crate::document::indexed_document_for_chunk(&key, chunk, &content, None).0)
        .collect();
    assert_eq!(docs.len(), 4);
    let limit = docs.iter().map(|doc| serde_json::json!({"model":"fixture","input":[crate::document::semantic_text_for_indexed_document(doc)],"dimensions":3}).to_string().len()).max().unwrap();
    (content, docs, limit)
}

#[test]
fn index_directory_reuses_vectors_for_unchanged_semantic_payloads() {
    use crate::embedder::payload_tests::{success, vector, PayloadServer};

    let dir = tempfile::tempdir().unwrap();
    let (content, documents, limit) = payload_documents();
    let source = dir.path().join("Module.bsl");
    std::fs::write(&source, &content).unwrap();
    let server = PayloadServer::new(success);
    let embedder = server.config(limit);
    let config = SearchConfig { embedder, execution: crate::EmbeddingExecutionPolicy::default() };
    let db_path = dir.path().join("search.db");
    let mut engine = SearchEngine::new(&db_path, config).unwrap();

    assert_eq!(engine.index_directory(dir.path(), None).unwrap(), 1);
    assert_eq!(server.requests().len(), documents.len());
    assert!(engine.store.embedding_profile_matches("fixture", 3).unwrap());
    let original_vectors = engine.store.load_all_embeddings(3).unwrap();
    let original_target_ids = original_vectors.iter().map(|(id, _)| *id).collect::<Vec<_>>();
    let original_by_symbol = original_vectors
        .into_iter()
        .map(|(id, embedding)| {
            let chunk = engine.store.chunk_by_id(id).unwrap().unwrap();
            (chunk.symbol_name, embedding)
        })
        .collect::<HashMap<_, _>>();

    // Keep a later row alive while replacing Module.bsl so SQLite assigns its replacement
    // chunks new ids. The sidecar must then be rebuilt from those current ids.
    std::fs::write(
        dir.path().join("Later.bsl"),
        "Процедура Позже()\nСообщить(\"later\");\nКонецПроцедуры",
    )
    .unwrap();
    assert_eq!(engine.index_directory(dir.path(), None).unwrap(), 1);
    assert_eq!(server.requests().len(), documents.len() + 1);

    // Keep request size fixed while changing only Метод2's effective payload.
    let changed_content = content.replace("данные2", "данные9");
    assert_eq!(content.len(), changed_content.len());
    let changed_documents = documents
        .iter()
        .cloned()
        .map(|mut document| {
            if document.symbol_name == "Метод2" {
                document.text = document.text.replace("данные2", "данные9");
            }
            document
        })
        .collect::<Vec<_>>();
    for (before, after) in documents.iter().zip(&changed_documents) {
        assert_eq!(
            semantic_key_for_indexed_document(before) == semantic_key_for_indexed_document(after),
            before.symbol_name != "Метод2",
            "only Метод2 should have a different embedding key"
        );
    }
    std::fs::write(&source, changed_content).unwrap();
    assert_eq!(engine.index_directory(dir.path(), None).unwrap(), 1);

    assert_eq!(
        server.requests().len(),
        documents.len() + 2,
        "unchanged semantic payloads should not be submitted again"
    );
    let current = engine.store.load_all_embeddings(3).unwrap();
    let current_by_symbol = current
        .iter()
        .map(|(id, embedding)| {
            let chunk = engine.store.chunk_by_id(*id).unwrap().unwrap();
            (chunk.symbol_name, embedding)
        })
        .collect::<HashMap<_, _>>();
    assert_eq!(current_by_symbol.len(), documents.len() + 1);
    for document in documents.iter().filter(|document| document.symbol_name != "Метод2") {
        assert_eq!(
            current_by_symbol.get(&document.symbol_name).copied(),
            original_by_symbol.get(&document.symbol_name),
            "unchanged chunk {} should keep its exact vector",
            document.symbol_name
        );
    }
    for (id, embedding) in current {
        let chunk = engine.store.chunk_by_id(id).unwrap().unwrap();
        let text = crate::document::semantic_text_from_parts(
            &chunk.file_path,
            &chunk.kind,
            &chunk.symbol_name,
            "",
            &chunk.text,
        );
        assert_eq!(embedding, vector(&text));
    }

    let current = engine.store.load_all_embeddings(3).unwrap();
    assert_eq!(original_target_ids.len(), documents.len());
    let current_target_ids = engine
        .store
        .chunk_ids_for_file("code", crate::CONFIGURATION_ROOT_ID, "Module.bsl")
        .unwrap();
    assert!(
        current_target_ids.iter().all(|id| !original_target_ids.contains(id)),
        "fixture must move replacement chunks to current SQLite ids"
    );

    drop(engine);
    std::fs::remove_file(db_path.with_extension("db.usearch.json")).unwrap();
    let config = SearchConfig {
        embedder: server.config(limit),
        execution: crate::EmbeddingExecutionPolicy::default(),
    };
    let rebuilt = SearchEngine::new(&db_path, config).unwrap();
    assert_eq!(server.requests().len(), documents.len() + 2, "rebuild must not embed");
    assert_eq!(rebuilt.index.len(), current.len());
    for (id, vector) in current {
        let hit = rebuilt.index.search(&vector, 1).unwrap().remove(0);
        assert_eq!(hit.chunk_id, id, "rebuilt sidecar must use current SQLite ids");
        assert!(hit.score > 0.99, "rebuilt sidecar score must match the stored vector");
    }
}

#[test]
fn payload_file_collection_mapping_cached_gaps_and_batch_totals() {
    use crate::embedder::payload_tests::{success, vector, PayloadServer};
    for collection_sync in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let (content, mut docs, limit) = payload_documents();
        std::fs::write(dir.path().join("Module.bsl"), content).unwrap();
        let progress = IndexProgress::new();
        let (reply, batches) = sampling_batches(&progress, success);
        let server = PayloadServer::new(reply);
        let config = SearchConfig {
            embedder: server.config(limit),
            execution: crate::EmbeddingExecutionPolicy::default(),
        };
        let mut engine = SearchEngine::new(&dir.path().join("search.db"), config).unwrap();
        if collection_sync {
            let cached = [1, 3]
                .into_iter()
                .map(|i| {
                    (
                        semantic_key_for_indexed_document(&docs[i]),
                        vector(&semantic_text_for_indexed_document(&docs[i])),
                    )
                })
                .collect::<HashMap<_, _>>();
            docs.reverse();
            assert_eq!(
                engine
                    .sync_indexed_documents_in_collection_with_embeddings(
                        "code",
                        &docs,
                        Some(&cached),
                        Some(&progress)
                    )
                    .unwrap(),
                1
            );
        } else {
            assert_eq!(engine.index_directory(dir.path(), Some(&progress)).unwrap(), 1);
        }
        let requests = server.requests();
        assert_eq!(requests.len(), if collection_sync { 2 } else { 4 });
        assert!(requests.iter().all(|body| body.len() <= limit));
        let planned = batches.lock().unwrap().clone();
        assert_eq!(planned.len(), requests.len());
        assert!(planned.iter().all(|total| *total == Some(requests.len())), "{planned:?}");
        assert_eq!(progress.snapshot().unwrap().state, IndexPassState::Ready);
        for (id, actual) in engine.store.load_all_embeddings(3).unwrap() {
            let chunk = engine.store.chunk_by_id(id).unwrap().unwrap();
            let doc = docs.iter().find(|d| d.symbol_name == chunk.symbol_name).unwrap();
            assert_eq!(actual, vector(&semantic_text_for_indexed_document(doc)));
        }
        assert_eq!(engine.store.load_all_embeddings(3).unwrap().len(), 4);
    }
}

#[test]
fn payload_overlay_prime_installs_the_lexical_overlay_before_reporting_embedding_failure() {
    use crate::embedder::payload_tests::PayloadServer;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("Module.bsl"), "Процедура Тест()\nКонецПроцедуры").unwrap();
    let server = PayloadServer::new(|_, _| (401, "refused".into()));
    let config = SearchConfig {
        embedder: server.config(4096),
        execution: crate::EmbeddingExecutionPolicy::default(),
    };
    let mut engine = SearchEngine::new(&dir.path().join("search.db"), config).unwrap();
    engine.set_workspace_root(dir.path());
    let error =
        engine.prime_workspace_overlay_fenced(|op| FenceOutcome::Applied(op())).unwrap_err();
    assert_eq!(
        error.embedding_failure().unwrap().code,
        crate::EmbeddingFailureCode::EmbeddingProviderError
    );
    assert_eq!(server.requests().len(), 1, "a final refusal is not retried");
    let signals = engine.workspace_overlay_retry_signals().unwrap();
    assert!(signals.initialized, "the lexical overlay is installed despite the failure");
    assert_eq!(signals.pending_dirty_paths, 1, "the failed key stays owed a refresh: {signals:?}");

    // A barrier that let go wins over the embedding failure: nothing was installed.
    let mut released = SearchEngine::new(
        &dir.path().join("released.db"),
        SearchConfig {
            embedder: server.config(4096),
            execution: crate::EmbeddingExecutionPolicy::default(),
        },
    )
    .unwrap();
    released.set_workspace_root(dir.path());
    assert!(matches!(
        released.prime_workspace_overlay_fenced(|_| FenceOutcome::Released),
        Ok(FenceOutcome::Released)
    ));
}

#[test]
fn payload_file_collection_failed_file_keeps_its_previous_atomic_version() {
    use crate::embedder::payload_tests::{success, PayloadServer};
    for collection_sync in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("search.db");
        let (content, docs, limit) = payload_documents();
        std::fs::write(dir.path().join("Module.bsl"), content).unwrap();
        let server =
            PayloadServer::new(
                |i, body| if i == 1 { (413, "refused".into()) } else { success(i, body) },
            );
        let mut engine = SearchEngine::new(
            &path,
            SearchConfig {
                embedder: server.config(limit),
                execution: crate::EmbeddingExecutionPolicy { concurrency: 1, ..Default::default() },
            },
        )
        .unwrap();
        let old = &docs[..1];
        engine
            .store
            .reindex_indexed_documents_in_collection(
                CONFIGURATION_ROOT_ID,
                "Module.bsl",
                b"old",
                "code",
                old,
                Some(&[vec![1.0, 2.0, 3.0]]),
            )
            .unwrap();
        let previous = engine.store.load_all_embeddings(3).unwrap();
        let error = if collection_sync {
            engine.sync_indexed_documents_in_collection("code", &docs, None)
        } else {
            engine.index_directory(dir.path(), None)
        }
        .unwrap_err();
        assert_eq!(error.to_string(), "embedding_request_too_large");
        assert_eq!(server.requests().len(), 2);
        assert_eq!(engine.store.load_all_embeddings(3).unwrap(), previous);
        assert_eq!(engine.store.load_indexed_documents(Some("code")).unwrap().len(), 1);
    }
}

#[test]
fn payload_reference_publication_preserves_lexical_failure_then_retries_in_order() {
    use crate::embedder::payload_tests::{success, vector, PayloadServer};
    let dir = tempfile::tempdir().unwrap();
    let documents: Vec<_> = (0..5)
        .map(|i| Document {
            title: format!("Справка{i}"),
            body: format!("uniquemarker{i} {}", "я".repeat(i + 1)),
            kind: "type".into(),
        })
        .collect();
    let limit = documents
        .iter()
        .map(|d| {
            serde_json::json!({"model":"fixture","input":[d.body],"dimensions":3}).to_string().len()
        })
        .max()
        .unwrap();
    let progress = IndexProgress::new();
    let (reply, batches) = sampling_batches(&progress, |i, body| {
        if i == 1 {
            (413, "refused".into())
        } else {
            success(i, body)
        }
    });
    let server = PayloadServer::new(reply);
    let mut engine = SearchEngine::new(
        &dir.path().join("reference.db"),
        SearchConfig {
            embedder: server.config(limit),
            execution: crate::EmbeddingExecutionPolicy { concurrency: 2, ..Default::default() },
        },
    )
    .unwrap();
    let failed = engine
        .replace_reference_collection_if_stale(
            "platform",
            "platform://docs",
            "fp",
            &documents,
            Some(&progress),
        )
        .unwrap();
    assert!(failed.written);
    assert_eq!(
        failed.embedding_failure.unwrap().code,
        crate::EmbeddingFailureCode::EmbeddingRequestTooLarge
    );
    assert!(failed.committed_fingerprint.ends_with(":fts"));
    assert!(!progress.is_active());
    assert_eq!(engine.vector_count(), 0);
    assert_eq!(engine.text_search("uniquemarker0", 10, Some("platform")).unwrap().len(), 1);

    let before = server.requests().len();
    batches.lock().unwrap().clear();
    let recovered = engine
        .replace_reference_collection_if_stale(
            "platform",
            "platform://docs",
            "fp",
            &documents,
            Some(&progress),
        )
        .unwrap();
    assert!(recovered.written);
    assert!(recovered.embedding_failure.is_none());
    assert!(recovered.committed_fingerprint.ends_with(":fixture:3"));
    assert_eq!(server.requests().len() - before, documents.len());
    let batches = batches.lock().unwrap().clone();
    assert!(batches.iter().all(|total| *total == Some(documents.len())), "{batches:?}");
    for (id, actual) in engine.store.load_all_embeddings(3).unwrap() {
        let chunk = engine.store.chunk_by_id(id).unwrap().unwrap();
        assert_eq!(actual, vector(&chunk.text));
    }
    assert_eq!(engine.vector_count(), documents.len());
    let before = server.requests().len();
    let unchanged = engine
        .replace_reference_collection_if_stale(
            "platform",
            "platform://docs",
            "fp",
            &documents,
            None,
        )
        .unwrap();
    assert!(!unchanged.written && unchanged.embedding_failure.is_none());
    assert_eq!(server.requests().len(), before);
    assert!(server.requests().iter().all(|body| body.len() <= limit));
}

#[test]
fn payload_configuration_standalone_rejects_zero_for_empty_or_cached_work() {
    use crate::embedder::payload_tests::{success, vector, PayloadServer};
    let server = PayloadServer::new(success);
    for cached in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        if cached {
            std::fs::write(dir.path().join("Cached.bsl"), "Процедура Кэш()\nКонецПроцедуры")
                .unwrap();
        }
        let db = dir.path().join("overlay.db");
        let store = Store::open(&db).unwrap();
        let roots = WorkspaceRoots::build(dir.path(), dir.path(), &[]).0;
        let plan = WorkspaceOverlayCache::plan_full_refresh_from_manifest(
            &HashMap::new(),
            &roots,
            &store,
            &HashMap::new(),
            None,
            &HashSet::new(),
        )
        .unwrap();
        let warm: HashMap<_, _> = plan
            .missing_embeddings()
            .iter()
            .map(|(key, text)| (key.clone(), vector(text)))
            .collect();
        assert_eq!(!warm.is_empty(), cached);
        for limit in [0, 4096] {
            let result = SearchEngine::prime_workspace_overlay_standalone(
                &db,
                server.config(limit),
                &roots,
                warm.clone(),
                None,
                &|| true,
                |op| FenceOutcome::Applied(op()),
                &HashSet::new(),
            );
            if limit == 0 {
                let Err(error) = result else { panic!("invalid configuration must fail") };
                assert_eq!(error.to_string(), "embedding_invalid_config");
            } else {
                assert!(matches!(result.unwrap(), FenceOutcome::Applied(_)));
            }
        }
    }
    assert!(server.requests().is_empty());
}

#[test]
fn payload_overlay_offlock_preserves_paid_cache_and_checks_between_requests() {
    use crate::embedder::payload_tests::{success, vector, PayloadServer};
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("overlay.db")).unwrap();
    let missing: HashMap<_, _> =
        (0..4).map(|i| (format!("key{i}"), format!("Текст запроса {i}"))).collect();
    let limit = serde_json::json!({"model":"fixture","input":["Текст запроса 0"],"dimensions":3})
        .to_string()
        .len();
    let server =
        PayloadServer::new(
            |i, body| if i == 1 { (413, "refused".into()) } else { success(i, body) },
        );
    let embedder = Embedder::new(server.config(limit));
    let error = SearchEngine::embed_missing_overlay_chunks(
        &store,
        &embedder,
        &missing,
        32,
        &|| true,
        &mut |op| FenceOutcome::Applied(op()),
        &mut || false,
    )
    .unwrap_err();
    assert_eq!(error.to_string(), "embedding_request_too_large");
    let retained = store.load_overlay_embedding_cache("fixture", 3).unwrap();
    assert_eq!(retained.len(), 1);
    for (key, actual) in &retained {
        assert_eq!(*actual, vector(&missing[key]));
    }
    let remaining = missing
        .iter()
        .filter(|(key, _)| !retained.contains_key(*key))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let recovered = SearchEngine::embed_missing_overlay_chunks(
        &store,
        &embedder,
        &remaining,
        32,
        &|| true,
        &mut |op| FenceOutcome::Applied(op()),
        &mut || false,
    )
    .unwrap();
    assert!(matches!(recovered, FenceOutcome::Applied(_)));
    assert_eq!(server.requests().len(), 5);
    let cached = store.load_overlay_embedding_cache("fixture", 3).unwrap();
    assert_eq!(cached.len(), missing.len());
    for (key, actual) in cached {
        assert_eq!(actual, vector(&missing[&key]));
    }
    assert!(server.requests().iter().all(|body| body.len() <= limit));

    let cancelled_store = Store::open(&dir.path().join("cancelled.db")).unwrap();
    let before = server.requests().len();
    let completed = std::cell::Cell::new(0);
    let result = SearchEngine::embed_missing_overlay_chunks(
        &cancelled_store,
        &embedder,
        &missing,
        32,
        &|| completed.get() < 2,
        &mut |op| {
            let result = op();
            completed.set(completed.get() + 1);
            FenceOutcome::Applied(result)
        },
        &mut || false,
    )
    .unwrap();
    assert!(matches!(result, FenceOutcome::Released));
    assert_eq!(server.requests().len() - before, 1);
    assert_eq!(cancelled_store.load_overlay_embedding_cache("fixture", 3).unwrap().len(), 1);
}

#[test]
fn vector_lifecycle_startup_snapshot_precedes_fence_with_root_mode_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("search.db");
    drop(seed(&path, 1, 1));
    let records = capture(|| {
        lifecycle::with_startup_roots("local", vec![dir.path().to_owned()], || {
            let opened = SearchEngine::fts_only_fenced(&path, |op| {
                CONSTRUCTOR_APPLY_ACTIVE.with(|active| {
                    assert!(!active.replace(true));
                    let result = op(&mut || ControlFlow::Continue(())).continue_value().unwrap();
                    active.set(false);
                    FenceOutcome::Applied(result)
                })
            })
            .unwrap();
            assert!(matches!(opened, FenceOutcome::Applied(_)));
        })
    });
    let snapshot = records.iter().find(|r| r["kind"] == "startup_snapshot").unwrap();
    assert_eq!(snapshot["snapshot"]["vectors"], 1);
    assert_eq!(snapshot["snapshot"]["chunks"], 2);
    assert_eq!(snapshot["snapshot"]["mode"], "local");
    assert_eq!(snapshot["snapshot"]["roots_count"], 1);
    assert!(snapshot["snapshot"]["roots_digest"].as_str().is_some());
}

#[test]
fn vector_lifecycle_each_open_snapshots_once_before_schema_reset() {
    for fenced in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("search.db");
        drop(seed(&path, 1, 1));
        rusqlite::Connection::open(&path)
            .unwrap()
            .execute("UPDATE meta SET value = '999' WHERE key = 'schema_version'", [])
            .unwrap();
        let records = capture(|| {
            if fenced {
                let opened = SearchEngine::fts_only_fenced(&path, |op| {
                    CONSTRUCTOR_APPLY_ACTIVE.with(|active| {
                        assert!(!active.replace(true));
                        let result =
                            op(&mut || ControlFlow::Continue(())).continue_value().unwrap();
                        active.set(false);
                        FenceOutcome::Applied(result)
                    })
                })
                .unwrap();
                assert!(matches!(opened, FenceOutcome::Applied(_)));
            } else {
                Store::open(&path).unwrap();
            }
        });
        let snapshots: Vec<_> =
            records.iter().filter(|r| r["kind"] == "startup_snapshot").collect();
        assert_eq!(snapshots.len(), 1);
        let snapshot = snapshots[0];
        assert_eq!(snapshot["snapshot"]["vectors"], 1);
        assert_eq!(snapshot["snapshot"]["chunks"], 2);
        assert_eq!(snapshot["snapshot"]["schema_version"], 999);
        let reset = records
            .iter()
            .find(|r| r["reason"] == "schema_reset" && r["outcome"] == "committed")
            .unwrap();
        assert!(snapshot["event_seq"].as_u64().unwrap() < reset["event_seq"].as_u64().unwrap());
        assert_eq!(reset["counts"]["sqlite_vectors_removed"], 1);
        assert!(Store::open_existing(&path).unwrap().load_all_embeddings(3).unwrap().is_empty());
    }
}

#[test]
fn vector_lifecycle_context_and_mode_decisions_preserve_exact_loss() {
    struct Context;
    impl crate::ports::GraphContextProvider for Context {
        fn graph_context(&self, _: &str, _: &str, _: &str) -> Option<String> {
            Some("fixture context".into())
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("search.db");
    drop(seed(&path, 1, 1));
    let mut engine = SearchEngine::fts_only(&path).unwrap();
    for name in ["M0.bsl", "M1.bsl"] {
        engine.store().mark_context_dirty("code", "", name).unwrap();
    }
    let records = capture(|| {
        engine.refresh_dirty_contexts(&Context, i64::MAX).unwrap();
        engine.set_serves_external_baseline(true).unwrap();
        engine.set_serves_external_baseline(false).unwrap();
    });
    let context = records
        .iter()
        .find(|r| {
            r["kind"] == "mutation_summary"
                && r["reason"] == "context_changed"
                && r["outcome"] == "completed"
        })
        .unwrap();
    assert_eq!(context["committed_totals"]["sqlite_vectors_removed"], 1);
    assert_eq!(context["reasons"]["context_changed"], 2);
    assert_eq!(context["files"], 2);
    let modes: Vec<_> = records
        .iter()
        .filter(|r| r["kind"] == "mode_transition" && r["outcome"] == "completed")
        .collect();
    assert_eq!(modes.len(), 2);
    assert_eq!(modes[0]["snapshot"]["mode"], "external_baseline");
    assert_eq!(modes[1]["snapshot"]["mode"], "local");
    assert!(modes.iter().all(|r| r["counts"]["sqlite_vectors_removed"] == 0));
    assert_eq!(engine.store().load_pending_embedding_documents("code").unwrap().len(), 2);
}

#[test]
fn vector_lifecycle_bulk_removal_bounds_info_and_retains_commits_before_refusal() {
    for reconcile in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("search.db");
        drop(seed(&path, 257, 0));
        let mut engine = SearchEngine::fts_only(&path).unwrap();
        engine.set_workspace_roots(crate::WorkspaceRoots::build(dir.path(), dir.path(), &[]).0);
        let records = capture_level(tracing::level_filters::LevelFilter::INFO, || {
            if reconcile {
                let mut admitted = 0;
                let result = engine
                    .reconcile_workspace_files_fenced(&HashSet::new(), |op| {
                        admitted += 1;
                        if admitted > 129 {
                            FenceOutcome::TransientRefusal
                        } else {
                            FenceOutcome::Applied(op())
                        }
                    })
                    .unwrap();
                assert!(matches!(result, FenceOutcome::TransientRefusal));
            } else {
                let keys = (0..257).map(|i| FileKey::configuration(format!("M{i}.bsl"))).collect();
                assert_eq!(engine.remove_workspace_keys(keys).unwrap(), 257);
            }
        });
        assert!(records.iter().all(|r| r["kind"] == "mutation_summary"));
        let elapsed_seconds = (records.last().unwrap()["timestamp_ms"].as_u64().unwrap()
            - records[0]["timestamp_ms"].as_u64().unwrap())
            / 1000;
        // Each flush also emits the next group's intent. Allow the one-second
        // flushes on a slow machine without turning this into a timing benchmark.
        assert!(records.len() as u64 <= 6 + 2 * elapsed_seconds);
        assert!(records.iter().all(|r| r["examples"].as_array().unwrap().len() <= 10));
        let terminal = records.last().unwrap();
        assert_eq!(terminal["outcome"], if reconcile { "refused" } else { "completed" });
        let removed = if reconcile { 129 } else { 257 };
        assert_eq!(terminal["committed_totals"]["sqlite_vectors_removed"], removed);
        assert_eq!(engine.store().load_all_embeddings(3).unwrap().len(), 257 - removed);
    }
}

#[test]
fn vector_lifecycle_full_index_that_skipped_a_rejected_file_is_failed() {
    // A permanent embedder refusal leaves the other files' work committed but fails the pass
    // with the typed cause, exactly as a failed batch in the embedding pass does; both must end
    // the same way in the journal.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let refusals = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let request = read_request_body(&mut stream);
        assert!(request["input"].as_array().is_some_and(|input| !input.is_empty()));
        let body = r#"{"error":"rejected"}"#;
        write!(stream, "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
    });
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("src");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("M.bsl"), "Процедура A()\nКонецПроцедуры\n").unwrap();
    let mut engine = SearchEngine::new(
        &dir.path().join("search.db"),
        SearchConfig {
            embedder: EmbedderConfig {
                base_url: format!("http://{address}"),
                model: "fixture".into(),
                dim: Some(3),
                api_key: None,
                provider: None,
                query_prefix: String::new(),
                document_prefix: String::new(),
                max_request_bytes: EmbedderConfig::DEFAULT_MAX_REQUEST_BYTES,
                token_policy: None,
            },
            execution: crate::EmbeddingExecutionPolicy::default(),
        },
    )
    .unwrap();
    let records = capture(|| {
        let error = engine.index_directory(&root, None).unwrap_err();
        assert_eq!(
            error.embedding_failure().unwrap().code,
            crate::EmbeddingFailureCode::EmbeddingProviderError
        );
    });
    refusals.join().unwrap();
    let summary = records
        .iter()
        .rev()
        .find(|r| r["kind"] == "mutation_summary" && r["outcome"] != "started")
        .unwrap();
    assert_eq!(summary["outcome"], "failed", "{summary}");
}

#[test]
fn indexing_pass_lifecycle_sync_counts_batches_per_file() {
    let dir = tempfile::tempdir().unwrap();
    let progress = IndexProgress::new();
    let observer = Arc::clone(&progress);
    let (embedder, requests) = server(3, move || {
        let sample = observer.snapshot().unwrap();
        let counters = sample.counters.expect("each file batch retains the coherent sample");
        assert_eq!(counters.total_chunks, Some(3));
        assert_eq!(counters.total_batches, Some(3));
        assert_eq!(counters.done_chunks, counters.done_batches);
    });
    let mut engine = SearchEngine::fts_only(&dir.path().join("search.db")).unwrap();
    engine.embedder = Some(embedder);
    engine.dim = 3;
    engine.batch_size = 8;
    let documents: Vec<_> = (0..3)
        .map(|i| crate::IndexedDocument {
            collection: "code".into(),
            root_id: "".into(),
            path: format!("M{i}.bsl"),
            symbol_name: format!("Method{i}"),
            kind: "procedure".into(),
            line_start: 1,
            line_end: 2,
            text: format!("Процедура Method{i}() КонецПроцедуры"),
            content_hash: format!("hash{i}"),
            graph_context: None,
            source_span: None,
        })
        .collect();
    engine.sync_indexed_documents_in_collection("code", &documents, Some(&progress)).unwrap();
    assert_eq!(requests.join().unwrap().len(), 3);
    assert_eq!(progress.snapshot().unwrap().state, IndexPassState::Ready);
}

#[test]
fn indexing_pass_lifecycle_directory_and_documents_own_complete_attempts() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("M.bsl"), "Процедура One()\nКонецПроцедуры").unwrap();
    let progress = IndexProgress::new();
    let observer = Arc::clone(&progress);
    let (embedder, requests) = server(2, move || {
        let sample = observer.snapshot().unwrap();
        assert!(sample.active);
        assert_eq!(sample.state, IndexPassState::Running);
        assert_eq!(sample.phase, Some(IndexPhase::Embedding));
        assert_eq!(sample.counters.unwrap().total_chunks, Some(1));
    });
    let mut engine = SearchEngine::fts_only(&dir.path().join("search.db")).unwrap();
    engine.embedder = Some(embedder);
    engine.dim = 3;
    engine.concurrency = 1;
    engine.index_directory(dir.path(), Some(&progress)).unwrap();
    let first = progress.snapshot().unwrap();
    assert_eq!(first.state, IndexPassState::Ready);
    assert!(!first.active);
    let docs =
        [Document { title: "One".into(), body: "Reference text".into(), kind: "type".into() }];
    engine.index_documents("platform", "docs", b"v1", &docs, Some(&progress)).unwrap();
    let second = progress.snapshot().unwrap();
    assert_eq!(second.state, IndexPassState::Ready);
    assert!(!second.active);
    assert_ne!(first.pass_id, second.pass_id);
    assert_eq!(requests.join().unwrap().len(), 2);
}
