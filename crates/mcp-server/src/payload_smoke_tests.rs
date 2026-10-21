//! Owned loopback acceptance for bounded embedding requests and MCP failure projections.

use super::*;
use crate::state::test_support::{
    payload_reference_state, payload_start_embed, payload_workspace_reference_state,
    payload_workspace_state,
};
use bsl_search::{
    semantic_text_for_indexed_document, Document, EmbedderConfig, EmbeddingExecutionPolicy,
    EmbeddingFailureCode, SearchConfig, SearchEngine,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// A hang watchdog for one smoke case: the timing profile keeps the nominal bound, while an
/// ordinary run gets a finite ten-minute guard and still returns as soon as the case is done.
const CASE_TIMEOUT: Duration = test_utils::wait::hang_guard(Duration::from_secs(30));
const UNSAFE_PROVIDER_BODY: &str = "RAW_PROVIDER_SENTINEL: пароль 🚫 UTF-8 тело запроса";

#[derive(Clone)]
struct Request {
    body: Vec<u8>,
    inputs: Vec<String>,
    rejected: bool,
}

struct Endpoint {
    address: SocketAddr,
    requests: Arc<Mutex<Vec<Request>>>,
    fail_on: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Endpoint {
    fn new(max_bytes: usize) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let fail_on = Arc::new(AtomicUsize::new(usize::MAX));
        let stop = Arc::new(AtomicBool::new(false));
        let worker = {
            let requests = requests.clone();
            let fail_on = fail_on.clone();
            let stop = stop.clone();
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    let mut stream = stream.unwrap();
                    stream
                        .set_read_timeout(Some(test_utils::wait::hang_guard(Duration::from_secs(
                            2,
                        ))))
                        .unwrap();
                    stream
                        .set_write_timeout(Some(test_utils::wait::hang_guard(Duration::from_secs(
                            2,
                        ))))
                        .unwrap();
                    let mut received = Vec::new();
                    let mut buffer = [0; 2048];
                    let (header_end, length) = loop {
                        let n = stream.read(&mut buffer).unwrap();
                        assert_ne!(n, 0, "request headers ended prematurely");
                        received.extend_from_slice(&buffer[..n]);
                        if let Some(position) = received.windows(4).position(|v| v == b"\r\n\r\n") {
                            let headers = String::from_utf8_lossy(&received[..position]);
                            let length = headers
                                .lines()
                                .find_map(|line| {
                                    let (key, value) = line.split_once(':')?;
                                    key.eq_ignore_ascii_case("content-length")
                                        .then(|| value.trim().parse::<usize>().unwrap())
                                })
                                .expect("embedding HTTP request has Content-Length");
                            break (position + 4, length);
                        }
                    };
                    while received.len() < header_end + length {
                        let n = stream.read(&mut buffer).unwrap();
                        assert_ne!(n, 0, "request body ended prematurely");
                        received.extend_from_slice(&buffer[..n]);
                    }
                    let body = received[header_end..header_end + length].to_vec();
                    let value: Value = serde_json::from_slice(&body).unwrap();
                    let inputs: Vec<String> = value["input"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|input| input.as_str().unwrap().to_owned())
                        .collect();
                    let rejected = {
                        let mut requests = requests.lock().unwrap();
                        let rejected = body.len() > max_bytes
                            || requests.len() + 1 == fail_on.load(Ordering::Acquire);
                        requests.push(Request { body, inputs: inputs.clone(), rejected });
                        rejected
                    };
                    let (status, response) =
                        if rejected {
                            (413, UNSAFE_PROVIDER_BODY.to_owned())
                        } else {
                            let data: Vec<_> = inputs.iter().enumerate().map(|(index, text)| {
                            json!({"index": index, "embedding": vector(text)})
                        }).collect();
                            (200, json!({"data": data}).to_string())
                        };
                    write!(stream,
                        "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                        response.len()).unwrap();
                }
            })
        };
        Self { address, requests, fail_on, stop, worker: Some(worker) }
    }

    fn config(&self, max_request_bytes: usize) -> SearchConfig {
        SearchConfig {
            embedder: EmbedderConfig {
                base_url: format!("http://{}", self.address),
                model: "payload-fixture".to_owned(),
                dim: Some(3),
                max_request_bytes,
                ..Default::default()
            },
            execution: EmbeddingExecutionPolicy {
                batch_size: 8,
                concurrency: 1,
                ..Default::default()
            },
        }
    }

    fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }

    fn reject_after(&self, offset: usize) {
        self.fail_on.store(self.requests().len() + offset, Ordering::Release);
    }
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = TcpStream::connect(self.address);
        if let Some(worker) = self.worker.take() {
            let result = crate::test_wait::join(worker, "payload endpoint exits");
            if !std::thread::panicking() {
                result.unwrap();
            }
        }
    }
}

fn vector(text: &str) -> Vec<f32> {
    vec![1.0, text.len() as f32, text.bytes().map(u32::from).sum::<u32>() as f32]
}

fn write_modules(root: &std::path::Path, version: &str) {
    for i in 0..12 {
        let path = root.join(format!("CommonModules/Модуль{i:02}/Ext/Module.bsl"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, format!(
            "Процедура Проверка{i:02}() Экспорт\n    // {version} {}\n    Сообщить(\"строка \"\"кавычки\"\"\");\nКонецПроцедуры\n",
            "я".repeat(40 + i * 7),
        )).unwrap();
    }
}

fn pending_vectors(state: &SharedState) -> BTreeMap<i64, Vec<f32>> {
    state
        .search_engine()
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .store()
        .load_pending_embedding_documents("code")
        .unwrap()
        .into_iter()
        .map(|(id, doc)| (id, vector(&semantic_text_for_indexed_document(&doc))))
        .collect()
}

fn stored_vectors(state: &SharedState) -> BTreeMap<i64, Vec<f32>> {
    state
        .search_engine()
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .store()
        .load_all_embeddings(3)
        .unwrap()
        .into_iter()
        .collect()
}

async fn wait_build(state: &SharedState) {
    while state.background_work_active() {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

async fn call(
    server: &McpServer,
    profile: McpProfile,
    params: Value,
) -> Result<CallToolResult, McpError> {
    match profile {
        McpProfile::Workspace => {
            server
                .workspace_search(
                    Parameters(serde_json::from_value(params).unwrap()),
                    CancellationToken::new(),
                )
                .await
        }
        McpProfile::Reference => {
            server
                .reference_search(
                    Parameters(serde_json::from_value(params).unwrap()),
                    CancellationToken::new(),
                )
                .await
        }
    }
}

fn payload(result: &CallToolResult) -> &Value {
    result.structured_content.as_ref().expect("actual handler structured content")
}

fn assert_safe(value: &impl serde::Serialize) {
    let rendered = serde_json::to_string(value).unwrap();
    assert!(!rendered.contains("RAW_PROVIDER_SENTINEL"));
    assert!(!rendered.contains("пароль"));
    assert!(!rendered.contains("http://"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "необязательный тест: real HTTP and fixed deadlines depend on host scheduling; needs a deterministic transport fixture; запуск: cargo test -p mcp-server --lib payload_smoke_tests::payload_mcp_smoke_build_failure_recovery_and_query_locality -- --exact --ignored"]
async fn payload_mcp_smoke_build_failure_recovery_and_query_locality() {
    tokio::time::timeout(CASE_TIMEOUT, async {
        let endpoint = Endpoint::new(600);
        let config = endpoint.config(600);
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("source");
        write_modules(&root, "первая");
        let db = temp.path().join("search.db");
        let mut engine = SearchEngine::new(&db, endpoint.config(600)).unwrap();
        engine.set_workspace_root(&root);
        engine.index_directory_deferred(&root).unwrap();
        engine.initialize_workspace_overlay_clean().unwrap();
        let state = payload_workspace_state(engine);
        let server = McpServer::new(McpProfile::Workspace, state.clone());
        let expected = pending_vectors(&state);
        assert_eq!(expected.len(), 12);
        payload_start_embed(&state, &db, endpoint.config(600));
        wait_build(&state).await;
        assert_eq!(*state.semantic_runtime().lock().unwrap(), state::SemanticRuntimeStatus::Ready);
        assert_eq!(stored_vectors(&state), expected);
        let initial = endpoint.requests();
        assert!(initial.len() > expected.len().div_ceil(config.execution.batch_size));
        assert!(initial.iter().all(|request| !request.rejected
            && request.body.len() <= 600
            && request.inputs.len() <= 8));
        assert_eq!(
            initial.iter().map(|request| request.inputs.len()).sum::<usize>(),
            expected.len()
        );
        let status =
            call(&server, McpProfile::Workspace, json!({"action":"status"})).await.unwrap();
        assert_eq!(payload(&status)["state"], "ready");
        assert!(payload(&status).get("semantic_failure").is_none());
        assert_eq!(endpoint.requests().len(), initial.len(), "status must not probe the provider");

        // Query errors are returned on both lexical hit and empty branches, never persisted.
        for (query, hits) in [("Проверка00", 1), ("НесуществующийИдентификатор", 0)]
        {
            endpoint.reject_after(1);
            let answer =
                call(&server, McpProfile::Workspace, json!({"action":"search_code","query":query}))
                    .await
                    .unwrap();
            assert_eq!(payload(&answer)["hits"].as_array().unwrap().len(), hits);
            assert_eq!(payload(&answer)["semantic_failure"]["code"], "embedding_request_too_large");
            assert_safe(&answer);
            assert_eq!(
                *state.semantic_runtime().lock().unwrap(),
                state::SemanticRuntimeStatus::Ready
            );
        }

        write_modules(&root, "вторая");
        {
            let mut guard = state.search_engine().lock().unwrap();
            let engine = guard.as_mut().unwrap();
            engine.index_directory_deferred(&root).unwrap();
            engine.initialize_workspace_overlay_clean().unwrap();
        }
        let expected = pending_vectors(&state);
        assert_eq!(expected.len(), 12);
        endpoint.reject_after(2);
        payload_start_embed(&state, &db, endpoint.config(600));
        wait_build(&state).await;
        let failure = state.semantic_runtime().lock().unwrap().embedding_failure().unwrap();
        assert_eq!(failure.code, EmbeddingFailureCode::EmbeddingRequestTooLarge);
        let committed = stored_vectors(&state);
        assert!(!committed.is_empty() && committed.len() < expected.len());
        assert!(committed.iter().all(|(id, actual)| expected.get(id) == Some(actual)));
        let remaining = pending_vectors(&state);
        assert_eq!(committed.len() + remaining.len(), expected.len());
        let before_status = endpoint.requests().len();
        let status =
            call(&server, McpProfile::Workspace, json!({"action":"status"})).await.unwrap();
        assert_eq!(payload(&status)["state"], "ready", "lexical index remains available");
        assert_eq!(payload(&status)["semantic_failure"]["code"], "embedding_request_too_large");
        assert_safe(&status);
        assert_eq!(endpoint.requests().len(), before_status);

        for (query, hits) in [("Проверка00", 1), ("НесуществующийИдентификатор", 0)]
        {
            let answer =
                call(&server, McpProfile::Workspace, json!({"action":"search_code","query":query}))
                    .await
                    .unwrap();
            assert_eq!(payload(&answer)["hits"].as_array().unwrap().len(), hits);
            assert_eq!(payload(&answer)["semantic_failure"]["code"], "embedding_request_too_large");
            assert_safe(&answer);
            assert_eq!(
                endpoint.requests().len(),
                before_status,
                "known build failure uses lexical fallback without a provider request"
            );
        }

        payload_start_embed(&state, &db, config);
        wait_build(&state).await;
        assert_eq!(*state.semantic_runtime().lock().unwrap(), state::SemanticRuntimeStatus::Ready);
        assert_eq!(stored_vectors(&state), expected);
        let recovered = endpoint.requests();
        assert_eq!(
            recovered[before_status..].iter().map(|request| request.inputs.len()).sum::<usize>(),
            remaining.len(),
            "the retry embeds only rows still pending"
        );
        let count = recovered.len();
        let status =
            call(&server, McpProfile::Workspace, json!({"action":"status"})).await.unwrap();
        assert!(payload(&status).get("semantic_failure").is_none());
        assert_eq!(endpoint.requests().len(), count);
        server.shutdown();
    })
    .await
    .expect("owned synthetic MCP smoke must finish within 30 seconds");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn payload_mcp_contract_reference_handlers_both_profiles() {
    tokio::time::timeout(CASE_TIMEOUT, async {
        let endpoint = Endpoint::new(512);
        let temp = tempfile::tempdir().unwrap();
        let mut engine =
            SearchEngine::new(&temp.path().join("reference.db"), endpoint.config(512)).unwrap();
        let docs = [Document {
            title: "СправочнаяПроверка".to_owned(),
            body: "fixturemark описание справки".to_owned(),
            kind: "type".to_owned(),
        }];
        endpoint.reject_after(1);
        let outcome = engine
            .replace_reference_collection_if_stale(
                "platform",
                "platform://docs",
                "payload-reference",
                &docs,
                None,
            )
            .unwrap();
        let failure = outcome.embedding_failure.unwrap();
        let state = payload_reference_state(
            Some(engine),
            state::SemanticRuntimeStatus::EmbeddingFailed(failure),
        );
        for profile in [McpProfile::Workspace, McpProfile::Reference] {
            let profile_state = match profile {
                McpProfile::Workspace => payload_workspace_reference_state(&state),
                McpProfile::Reference => state.clone(),
            };
            let server = McpServer::new(profile, profile_state);
            let before = endpoint.requests().len();
            for (query, hits) in [("fixturemark", 1), ("missingfixturemark", 0)] {
                let answer = call(&server, profile, json!({"action":"find_docs","query":query}))
                    .await
                    .unwrap();
                assert_eq!(payload(&answer)["hits"].as_array().unwrap().len(), hits);
                assert_eq!(
                    payload(&answer)["semantic_failure"]["code"],
                    "embedding_request_too_large"
                );
                assert_safe(&answer);
            }
            let error =
                call(&server, profile, json!({"action":"search_docs","query":"fixturemark"}))
                    .await
                    .unwrap_err();
            assert_eq!(
                error.data.as_ref().unwrap()["semantic_failure"]["code"],
                "embedding_request_too_large"
            );
            assert_safe(&error);
            let status = call(&server, profile, json!({"action":"status"})).await.unwrap();
            assert_eq!(payload(&status)["state"], "ready");
            match profile {
                McpProfile::Workspace => assert!(
                    payload(&status).get("semantic_failure").is_none(),
                    "workspace status must not borrow the reference owner's failure"
                ),
                McpProfile::Reference => assert_eq!(
                    payload(&status)["semantic_failure"]["code"],
                    "embedding_request_too_large"
                ),
            }
            assert_eq!(
                endpoint.requests().len(),
                before,
                "known failure projections need no request"
            );
            // Shared fixture handles remain in use by the next profile.
        }

        // Native recovery gives reference semantics a usable index; query-local 413 still
        // follows the RPC error route without overwriting the owner's successful verdict.
        {
            let mut slot = state.search_engine().lock().unwrap();
            let outcome = slot
                .as_mut()
                .unwrap()
                .replace_reference_collection_if_stale(
                    "platform",
                    "platform://docs",
                    "payload-reference",
                    &docs,
                    None,
                )
                .unwrap();
            assert!(outcome.embedding_failure.is_none());
        }
        *state.reference_semantic_runtime().lock().unwrap() = state::SemanticRuntimeStatus::Ready;
        for profile in [McpProfile::Workspace, McpProfile::Reference] {
            let profile_state = match profile {
                McpProfile::Workspace => payload_workspace_reference_state(&state),
                McpProfile::Reference => state.clone(),
            };
            let server = McpServer::new(profile, profile_state);
            endpoint.reject_after(1);
            let error =
                call(&server, profile, json!({"action":"search_docs","query":"fixturemark"}))
                    .await
                    .unwrap_err();
            assert_eq!(
                error.data.as_ref().unwrap()["semantic_failure"]["code"],
                "embedding_request_too_large"
            );
            assert_safe(&error);
            let before = endpoint.requests().len();
            let status = call(&server, profile, json!({"action":"status"})).await.unwrap();
            assert!(payload(&status).get("semantic_failure").is_none());
            assert_eq!(endpoint.requests().len(), before);
            assert_eq!(
                *state.reference_semantic_runtime().lock().unwrap(),
                state::SemanticRuntimeStatus::Ready
            );
        }
        McpServer::new(McpProfile::Reference, state).shutdown();

        for profile in [McpProfile::Workspace, McpProfile::Reference] {
            let waiting = payload_reference_state(
                None,
                state::SemanticRuntimeStatus::EmbeddingFailed(failure),
            );
            let server = McpServer::new(profile, waiting);
            let answer =
                call(&server, profile, json!({"action":"find_docs","query":"fixturemark"}))
                    .await
                    .unwrap();
            assert_eq!(payload(&answer)["status"], "not_ready");
            assert_eq!(payload(&answer)["semantic_failure"]["code"], "embedding_request_too_large");
            assert_safe(&answer);
            server.shutdown();
        }
    })
    .await
    .expect("owned synthetic MCP handler matrix must finish within 30 seconds");
}
