use super::*;
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::Duration;

/// A joined loopback fixture shared by the native payload integration checks.
pub(crate) struct PayloadServer {
    pub url: String,
    address: std::net::SocketAddr,
    requests: Arc<Mutex<Vec<Vec<u8>>>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl PayloadServer {
    pub fn new(mut reply: impl FnMut(usize, &[u8]) -> (u16, String) + Send + 'static) -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let url = format!("http://{address}");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let captured = requests.clone();
        let stopping = stop.clone();
        // The loop ends at the stop the fixture's `Drop` raises, woken out of `accept` by the
        // connection `Drop` makes after it. An exchange waits for its peer as long as the peer
        // takes; only the timing profile bounds it, so a slow client is never cut off.
        let thread = std::thread::spawn(move || loop {
            let (mut stream, _) = listener.accept().expect("loopback accept");
            if stopping.load(Ordering::Acquire) {
                break;
            }
            {
                let exchange = test_utils::wait::hang_guard(Duration::from_secs(2));
                stream.set_read_timeout(Some(exchange)).unwrap();
                stream.set_write_timeout(Some(exchange)).unwrap();
                let mut bytes = Vec::new();
                let mut buffer = [0; 4096];
                let body = loop {
                    let n = stream.read(&mut buffer).unwrap();
                    assert_ne!(n, 0, "incomplete request");
                    bytes.extend_from_slice(&buffer[..n]);
                    let Some(split) = bytes.windows(4).position(|w| w == b"\r\n\r\n") else {
                        continue;
                    };
                    let headers = String::from_utf8_lossy(&bytes[..split]).to_lowercase();
                    assert!(headers.contains("content-type: application/json"));
                    let length: usize = headers
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .unwrap()
                        .trim()
                        .parse()
                        .unwrap();
                    if bytes.len() >= split + 4 + length {
                        break bytes[split + 4..split + 4 + length].to_vec();
                    }
                };
                let index = {
                    let mut all = captured.lock().unwrap();
                    let index = all.len();
                    all.push(body.clone());
                    index
                };
                let (status, response) = reply(index, &body);
                // A response-limit/timeout test deliberately closes the peer early.
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                    response.len()
                );
            }
        });
        Self { url, address, requests, stop, thread: Some(thread) }
    }

    pub fn requests(&self) -> Vec<Vec<u8>> {
        self.requests.lock().unwrap().clone()
    }

    pub fn config(&self, max_request_bytes: usize) -> EmbedderConfig {
        EmbedderConfig {
            base_url: self.url.clone(),
            model: "fixture".into(),
            dim: Some(3),
            max_request_bytes,
            ..Default::default()
        }
    }
}

impl Drop for PayloadServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = std::net::TcpStream::connect(self.address);
        let result = self.thread.take().unwrap().join();
        if !std::thread::panicking() {
            result.unwrap();
        }
    }
}

pub(crate) fn vector(text: &str) -> Vec<f32> {
    vec![text.len() as f32, text.bytes().map(u32::from).sum::<u32>() as f32, 1.0]
}

pub(crate) fn success(_: usize, body: &[u8]) -> (u16, String) {
    let request: Value = serde_json::from_slice(body).unwrap();
    let data: Vec<_> = request["input"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
        .rev()
        .map(|(index, text)| json!({"index":index,"embedding":vector(text.as_str().unwrap())}))
        .collect();
    (200, json!({"data":data}).to_string())
}

/// An undeclared width leaves `dimensions` out of the request entirely: an endpoint
/// that refuses the parameter (`litellm.UnsupportedParamsError` among them) answers
/// only then, and the width is the model's own.
#[test]
fn payload_transport_omits_an_undeclared_dimension() {
    let embedder = Embedder::new(EmbedderConfig { dim: None, ..Default::default() });
    let body = embedder.serialize_request(&["ASCII"]).unwrap();
    let value: Value = serde_json::from_slice(&body).unwrap();
    assert!(value.get("dimensions").is_none(), "an unset width must not be sent: {value}");
}

/// The width the client resolves for itself is still checked: an undeclared one
/// resolves to the historical 1024, so a response of another width is a settled
/// invalid answer — never vectors indexed under a contract nobody declared.
#[test]
fn payload_transport_refuses_a_response_of_an_undeclared_other_width() {
    let server = PayloadServer::new(|_, _| {
        (200, json!({"data":[{"index":0,"embedding":[1,2,3]}]}).to_string())
    });
    let mut config = server.config(4096);
    config.dim = None;
    let error = Embedder::new(config).embed_batch(&["a"]).unwrap_err();
    assert_eq!(error.to_string(), "embedding_invalid_response");
    assert_eq!(server.requests().len(), 1, "a settled wrong width is not retried");
}

#[test]
fn payload_transport_serialized_ranges_and_exact_boundary() {
    let server = PayloadServer::new(success);
    let mut config = server.config(4096);
    config.model = "model\"\\модель".into();
    config.provider = Some("маршрут\n\\\"".into());
    let texts = ["ASCII", "Кириллица\n\"\\", "third", "хвост"];
    let encoder = Embedder::new(config.clone());
    let exact = encoder.serialize_request(&texts[..2]).unwrap();
    config.max_request_bytes = exact.len();
    let embedder = Embedder::new(config.clone());
    let ranges = embedder.batch_ranges(&texts, 32).unwrap();
    assert_eq!(ranges, vec![0..2, 2..4]);
    assert_eq!(embedder.batch_ranges(&texts, 1).unwrap(), vec![0..1, 1..2, 2..3, 3..4]);
    let mut actual = Vec::new();
    for range in ranges {
        actual.extend(embedder.embed_batch(&texts[range]).unwrap());
    }
    assert_eq!(actual, texts.iter().map(|t| vector(t)).collect::<Vec<_>>());
    let captured = server.requests();
    assert_eq!(captured[0], exact);
    assert!(captured.iter().all(|b| b.len() <= config.max_request_bytes));
    let value: Value = serde_json::from_slice(&captured[0]).unwrap();
    assert_eq!(value["provider"]["allow_fallbacks"], false);
    assert_eq!(value["dimensions"], 3);

    config.max_request_bytes -= 1;
    let smaller = Embedder::new(config);
    assert_eq!(smaller.batch_ranges(&texts[..2], 32).unwrap(), vec![0..1, 1..2]);
    let error = smaller.embed_batch(&texts[..2]).unwrap_err().embedding_failure().unwrap();
    assert_eq!(error.code, EmbeddingFailureCode::EmbeddingRequestTooLarge);
    assert_eq!(error.request_bytes, Some(exact.len()));
    assert_eq!(server.requests().len(), 2, "a local refusal sends nothing");
}

#[test]
fn payload_roles_apply_literal_prefixes_once_and_keep_legacy_bytes() {
    let server = PayloadServer::new(success);
    let mut config = server.config(4096);
    config.query_prefix = "search_query: ".into();
    config.document_prefix = "search_document: \"\\\n".into();
    let embedder = Embedder::new(config.clone());
    let original_query = "search_query: Найти";

    embedder.embed(original_query).unwrap();
    embedder.embed_batch(&["module\\\n"]).unwrap();
    embedder.embed_batch_interactive(&["module\\\n"]).unwrap();

    let inputs = server
        .requests()
        .iter()
        .map(|body| serde_json::from_slice::<Value>(body).unwrap()["input"][0].clone())
        .collect::<Vec<_>>();
    assert_eq!(inputs[0], format!("{}{}", config.query_prefix, original_query));
    assert_eq!(inputs[1], format!("{}module\\\n", config.document_prefix));
    assert_eq!(inputs[2], inputs[1]);

    let mut legacy = server.config(4096);
    let raw = "raw\\\n\"";
    let expected = Embedder::new(legacy.clone()).serialize_request(&[raw]).unwrap();
    Embedder::new(legacy.clone()).embed(raw).unwrap();
    assert_eq!(server.requests()[3], expected);
    assert_eq!(Embedder::new(legacy.clone()).storage_identity(), "fixture");
    assert_eq!(Embedder::new(legacy.clone()).model(), "fixture");
    assert_eq!(Embedder::new(legacy.clone()).dimension(), 3);

    let legacy_identity = Embedder::new(legacy.clone()).storage_identity().to_owned();
    legacy.dim = Some(9);
    let different_dimension = Embedder::new(legacy.clone());
    assert_eq!(different_dimension.storage_identity(), legacy_identity);
    assert_eq!(different_dimension.dimension(), 9);

    legacy.dim = Some(3);
    legacy.model = "other-wire-alias".into();
    let other_alias = Embedder::new(legacy.clone());
    assert_eq!(other_alias.model(), "other-wire-alias");
    assert_ne!(other_alias.storage_identity(), legacy_identity);

    legacy.query_prefix = "query:".into();
    let query_embedder = Embedder::new(legacy.clone());
    let query_profile = query_embedder.storage_identity().to_owned();
    legacy.query_prefix.clear();
    legacy.document_prefix = "document:".into();
    let document_embedder = Embedder::new(legacy);
    let document_profile = document_embedder.storage_identity().to_owned();
    assert_ne!(query_profile, document_profile);
    assert!(query_profile.starts_with("profile-v1:"));
}

#[test]
fn payload_document_prefix_is_counted_in_exact_serialized_payload_limit() {
    let server = PayloadServer::new(success);
    let mut config = server.config(4096);
    config.document_prefix = "документ:\"\\".into();
    let embedder = Embedder::new(config.clone());
    let texts = ["один", "два"];
    let exact = embedder.serialize_request(&["документ:\"\\один", "документ:\"\\два"]).unwrap();
    config.max_request_bytes = exact.len();
    let embedder = Embedder::new(config.clone());
    assert_eq!(embedder.batch_ranges(&texts, 10).unwrap(), vec![0..2]);
    embedder.embed_batch(&texts).unwrap();
    assert_eq!(server.requests()[0], exact);

    config.max_request_bytes -= 1;
    let embedder = Embedder::new(config);
    assert_eq!(embedder.batch_ranges(&texts, 10).unwrap(), vec![0..1, 1..2]);
    assert_eq!(
        embedder.embed_batch(&texts).unwrap_err().embedding_failure().unwrap().code,
        EmbeddingFailureCode::EmbeddingRequestTooLarge
    );
    assert_eq!(server.requests().len(), 1, "oversized serialized payload is refused locally");
}

#[test]
fn payload_transport_singleton_empty_and_optional_envelope() {
    let server = PayloadServer::new(success);
    for (dim, provider) in [(None, None), (Some(3), Some("route".to_owned()))] {
        let mut config = server.config(4096);
        config.dim = dim;
        config.provider = provider;
        let text = "Очень длинный вход с \\\"";
        let exact = Embedder::new(config.clone()).serialize_request(&[text]).unwrap().len();
        config.max_request_bytes = exact - 1;
        let embedder = Embedder::new(config);
        for error in
            [embedder.batch_ranges(&[text], 0).unwrap_err(), embedder.embed(text).unwrap_err()]
        {
            let failure = error.embedding_failure().unwrap();
            assert_eq!(failure.code, EmbeddingFailureCode::EmbeddingInputTooLarge);
            assert_eq!(failure.request_bytes, Some(exact));
            assert_eq!(failure.max_request_bytes, Some(exact - 1));
        }
        assert!(embedder.batch_ranges(&[], 0).unwrap().is_empty());
        assert!(embedder.embed_batch(&[]).unwrap().is_empty());
    }
    assert!(server.requests().is_empty());
}

#[test]
fn payload_transport_alignment_and_safe_errors() {
    let responses = [
        "я".repeat(250),
        json!({"data":[{"index":0,"embedding":[1,2,3]},{"index":0,"embedding":[4,5,6]}]})
            .to_string(),
        json!({"data":[{"index":0,"embedding":[0,0,0]},{"index":1,"embedding":[4,5,6]}]})
            .to_string(),
        json!({"data":[{"index":0,"embedding":[1,2,3]},{"index":2,"embedding":[4,5,6]}]})
            .to_string(),
        json!({"data":[{"index":0,"embedding":[1,2,3]}]}).to_string(),
        json!({"data":[{"index":0,"embedding":[1]},{"index":1,"embedding":[4,5,6]}]}).to_string(),
    ];
    for response in responses {
        let server = PayloadServer::new(move |_, _| (200, response.clone()));
        let error =
            Embedder::new(server.config(4096)).embed_batch_interactive(&["a", "b"]).unwrap_err();
        assert_eq!(error.to_string(), "embedding_invalid_response");
        assert_eq!(server.requests().len(), 1);
    }
    for (status, code) in [(413, "embedding_request_too_large"), (401, "embedding_provider_error")]
    {
        let server =
            PayloadServer::new(move |_, _| (status, "secret source provider response".into()));
        let embedder = Embedder::new(server.config(4096));
        let error = if status == 413 {
            embedder.embed_batch(&["x"])
        } else {
            embedder.embed("x").map(|v| vec![v])
        }
        .unwrap_err();
        assert_eq!(error.to_string(), code);
        assert!(error.embedding_failure().unwrap().request_bytes.is_none());
        assert_eq!(server.requests().len(), 1);
    }
    assert_eq!(
        SearchError::Embedder("secret legacy exception".into()).to_string(),
        "embedding_failed"
    );
}

#[test]
fn payload_transport_retry_is_one_body_and_later_failure_does_not_replay() {
    let server = PayloadServer::new(|i, body| match i {
        0 => (503, "retry".into()),
        1 => success(i, body),
        _ => (413, "too large".into()),
    });
    let embedder = Embedder::new(server.config(4096));
    let texts = ["first", "second", "third"];
    let ranges = embedder.batch_ranges(&texts, 2).unwrap();
    assert!(embedder.embed_batch(&texts[ranges[0].clone()]).is_ok());
    assert_eq!(
        embedder.embed_batch(&texts[ranges[1].clone()]).unwrap_err().to_string(),
        "embedding_request_too_large"
    );
    let bodies = server.requests();
    assert_eq!(bodies.len(), 3);
    assert_eq!(bodies[0], bodies[1]);
    assert_ne!(bodies[1], bodies[2]);
}

#[test]
fn payload_transport_response_limit_and_timeout_are_independent() {
    let server = PayloadServer::new(|_, _| (200, "x".repeat(10 * 1024 * 1024 + 1)));
    let embedder = Embedder::new(server.config(4096));
    assert_eq!(embedder.embed("small").unwrap_err().to_string(), "embedding_response_too_large");
    assert_eq!(server.requests().len(), 1);
    // The batch path retries a passing state, not an answer that is always too large.
    assert_eq!(
        embedder.embed_batch(&["small"]).unwrap_err().to_string(),
        "embedding_response_too_large"
    );
    assert_eq!(server.requests().len(), 2);
    // The answer is held until the client has given up, so the client's own deadline is the
    // only thing that can end the exchange, however the two are scheduled. The sender is bound
    // after the server so that a failing assertion drops it first: the held answer is let go
    // and the server's `Drop` can join its thread.
    let (sender, held) = std::sync::mpsc::channel::<()>();
    let server = PayloadServer::new(move |i, body| {
        let _ = held.recv();
        success(i, body)
    });
    let answer = sender;
    let mut embedder = Embedder::new(server.config(4096));
    embedder.interactive_agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_millis(20)))
        .build()
        .new_agent();
    assert_eq!(embedder.embed("small").unwrap_err().to_string(), "embedding_timeout");
    assert_eq!(server.requests().len(), 1);
    answer.send(()).unwrap();
}
