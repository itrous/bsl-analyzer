//! A local HTTP fixture standing in for package and corpus hosts: it serves
//! what a test puts at a path and counts every request it receives.

// Each test binary uses its own part of the fixture.
#![allow(dead_code)]

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use platform_help::package;
use serde_json::Value;

const FIXTURE: &str = include_str!("../../../bsl-platform/tests/fixtures/help/corpus.json");

#[derive(Clone, Default)]
pub struct Server {
    pub files: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    pub requests: Arc<AtomicUsize>,
    port: u16,
}

impl Server {
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let server = Self { port: listener.local_addr().unwrap().port(), ..Self::default() };
        let shared = server.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                shared.answer(stream);
            }
        });
        server
    }

    fn answer(&self, mut stream: TcpStream) {
        self.requests.fetch_add(1, Ordering::SeqCst);
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut request_line = String::new();
        reader.read_line(&mut request_line).unwrap();
        loop {
            let mut header = String::new();
            if reader.read_line(&mut header).unwrap() == 0 || header == "\r\n" {
                break;
            }
        }
        let path = request_line.split_whitespace().nth(1).unwrap_or("/").to_owned();
        let body = self.files.lock().unwrap().get(&path).cloned();
        let (status, body) = match body {
            Some(body) => ("200 OK", body),
            None => ("404 Not Found", b"missing".to_vec()),
        };
        let head = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(&body);
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    /// Publishes a package under `dir/` whose `Массив.Добавить` says `text`.
    pub fn publish(&self, dir: &str, text: &str) {
        let corpus = corpus_with_add_text(text);
        let manifest = serde_json::json!({
            "schema_version": 1,
            "corpus_id": format!("fixture-{dir}"),
            "platform_version": "8.3.27.1",
            "extractor_version": "test",
            "corpus_file": "platform_data.json",
            "sha256": package::sha256_hex(&corpus),
        });
        let mut files = self.files.lock().unwrap();
        files.insert(format!("/{dir}/manifest.json"), serde_json::to_vec(&manifest).unwrap());
        files.insert(format!("/{dir}/platform_data.json"), corpus);
    }

    pub fn put(&self, path: &str, body: &[u8]) {
        self.files.lock().unwrap().insert(path.to_owned(), body.to_vec());
    }

    pub fn take_down(&self) {
        self.files.lock().unwrap().clear();
    }
}

pub fn corpus_with_add_text(text: &str) -> Vec<u8> {
    let mut corpus: Value = serde_json::from_str(FIXTURE).unwrap();
    for method in corpus["methods"].as_array_mut().unwrap() {
        if method["type_name"] == "Array" && method["english_name"] == "Add" {
            method["documentation"]["description"] = Value::String(text.to_owned());
        }
    }
    serde_json::to_vec(&corpus).unwrap()
}

impl Server {
    pub fn request_count(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }
}
