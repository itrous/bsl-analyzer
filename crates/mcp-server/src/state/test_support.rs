use std::env;
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};

impl super::SharedState {
    /// Install a graph whose lifecycle is controlled by the test, not the boot worker.
    pub(crate) fn with_graph_for_test(mut self, graph: crate::graph::GraphState) -> Self {
        self.graph = graph;
        self
    }
}

static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

/// Take the process-wide serialization lock used by tests that toggle a global — an
/// environment variable, a forced-failure seam.
///
/// The lock guards nothing but the right to run alone: there is no state under it for a
/// panic to leave half-written, so the poison flag carries no information. Honouring it
/// would only make ONE failing test re-report itself as a failure in every test that later
/// takes the lock, burying the one panic that actually happened under a pile of
/// `PoisonError`s naming innocent tests.
pub(crate) fn env_lock() -> MutexGuard<'static, ()> {
    ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap_or_else(PoisonError::into_inner)
}

pub(crate) struct EnvVarGuard {
    key: &'static str,
    previous: Option<std::ffi::OsString>,
}

impl EnvVarGuard {
    pub(crate) fn set(key: &'static str, value: &str) -> Self {
        let previous = env::var_os(key);
        env::set_var(key, value);
        Self { key, previous }
    }

    pub(crate) fn unset(key: &'static str) -> Self {
        let previous = env::var_os(key);
        env::remove_var(key);
        Self { key, previous }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        if let Some(value) = self.previous.take() {
            env::set_var(self.key, value);
        } else {
            env::remove_var(self.key);
        }
    }
}

pub(super) use fixtures::{mock_embedding_env, write_common_module, write_common_module_tree};
pub(crate) use fixtures::{mock_semantic_config, spawn_mock_embedding_server};

/// Explicit local fixtures for the handler acceptance tests: no environment changes,
/// cache discovery, resident graph or filesystem watcher is needed by these search routes.
pub(crate) fn payload_workspace_state(engine: bsl_search::SearchEngine) -> super::SharedState {
    let state = super::SharedState::shared();
    *state.search_engine.lock().unwrap() = Some(engine);
    *state.semantic_runtime.lock().unwrap() = super::SemanticRuntimeStatus::Indexing;
    state
}

pub(crate) fn payload_reference_state(
    engine: Option<bsl_search::SearchEngine>,
    runtime: super::SemanticRuntimeStatus,
) -> super::SharedState {
    let mut state = super::SharedState::shared();
    *state.reference_search.lifecycle.lock().unwrap() = if engine.is_some() {
        super::ReferenceSearchLifecycle::Ready
    } else {
        super::ReferenceSearchLifecycle::Loading
    };
    *state.reference_search.engine.lock().unwrap() = engine;
    *state.reference_search.semantic_runtime.lock().unwrap() = runtime;
    state.search_engine = state.reference_search.engine.clone();
    state.semantic_runtime = state.reference_search.semantic_runtime.clone();
    state.index_progress = state.reference_search.progress.clone();
    state.baseline = crate::baseline::DeferredBaselineRuntime::absent();
    state.reference_search.baseline = crate::baseline::DeferredBaselineRuntime::absent();
    state
}

/// Workspace docs share the reference resident but keep a separate workspace verdict.
pub(crate) fn payload_workspace_reference_state(
    reference: &super::SharedState,
) -> super::SharedState {
    let mut state = reference.clone();
    state.semantic_runtime = std::sync::Arc::new(Mutex::new(super::SemanticRuntimeStatus::Ready));
    state
}

/// Exercise the production MCP background owner with an explicit fixture configuration.
pub(crate) fn payload_start_embed(
    state: &super::SharedState,
    db_path: &std::path::Path,
    config: bsl_search::SearchConfig,
) {
    super::SharedState::spawn_embed_pass(
        state.search_engine.clone(),
        state.owners.clone(),
        state.semantic_runtime.clone(),
        state.index_progress.clone(),
        state.embed_flight.clone(),
        state.workspace_lease.clone(),
        db_path.to_path_buf(),
        config,
        super::bootstrap::DEFAULT_EMBEDDING_PUBLISH_RETRY_BUDGET,
    );
}

#[cfg(test)]
mod fixtures {
    use super::EnvVarGuard;
    use std::fs;
    /// First byte offset of `needle` in `haystack`, or `None`.
    pub(super) fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack.windows(needle.len()).position(|w| w == needle)
    }

    /// A minimal in-process HTTP embedding endpoint: answers `POST /v1/embeddings` with one
    /// fixed vector per input, so the real `Embedder` produces deterministic vectors without
    /// a live service. Returns the base URL; the detached server thread stops on process exit.
    pub(crate) fn spawn_mock_embedding_server(vector: Vec<f32>) -> String {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut buf = Vec::new();
                let mut tmp = [0u8; 2048];
                let mut header_end: Option<usize> = None;
                let mut content_len = 0usize;
                loop {
                    let n = match stream.read(&mut tmp) {
                        Ok(0) => break,
                        Ok(n) => n,
                        Err(_) => break,
                    };
                    buf.extend_from_slice(&tmp[..n]);
                    if header_end.is_none() {
                        if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
                            header_end = Some(pos + 4);
                            let headers = String::from_utf8_lossy(&buf[..pos]).to_lowercase();
                            for line in headers.lines() {
                                if let Some(v) = line.strip_prefix("content-length:") {
                                    content_len = v.trim().parse().unwrap_or(0);
                                }
                            }
                        }
                    }
                    if header_end.is_some_and(|he| buf.len() >= he + content_len) {
                        break;
                    }
                }
                let body = header_end.map(|he| &buf[he..]).unwrap_or(&[]);
                let n_inputs = serde_json::from_slice::<serde_json::Value>(body)
                    .ok()
                    .and_then(|v| v.get("input").and_then(|i| i.as_array().map(|a| a.len())))
                    .unwrap_or(1);
                let data: Vec<serde_json::Value> = (0..n_inputs)
                    .map(|i| serde_json::json!({ "index": i, "embedding": vector }))
                    .collect();
                let resp_body = serde_json::json!({ "data": data }).to_string();
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    resp_body.len(),
                    resp_body,
                );
                let _ = stream.write_all(resp.as_bytes());
                let _ = stream.flush();
            }
        });
        format!("http://{addr}")
    }

    /// A semantic `SearchConfig` pointed at `base_url`, dim 3.
    pub(crate) fn mock_semantic_config(base_url: &str) -> bsl_search::SearchConfig {
        bsl_search::SearchConfig {
            embedder: bsl_search::EmbedderConfig {
                base_url: base_url.to_owned(),
                model: "test-model".to_owned(),
                dim: Some(3),
                api_key: None,
                provider: None,
                ..Default::default()
            },
            execution: bsl_search::EmbeddingExecutionPolicy::default(),
        }
    }

    /// Point `Self::embedding_config()` (env-driven) at the mock server for the duration of a
    /// test. Returns the guards (kept alive by the caller) plus the shared env lock guard.
    pub(in crate::state) fn mock_embedding_env(base_url: &str) -> Vec<EnvVarGuard> {
        vec![
            EnvVarGuard::set("BSL_TEST_EMBEDDING", "1"),
            EnvVarGuard::set("EMBEDDING_URL", base_url),
            EnvVarGuard::set("EMBEDDING_MODEL", "test-model"),
            EnvVarGuard::set("EMBEDDING_DIM", "3"),
        ]
    }

    /// A minimal CommonModule descriptor + body under `root`, so the module is declared and
    /// its method resolves to a durable graph id (`method/common/<name>/<method>`).
    pub(in crate::state) fn write_common_module(root: &std::path::Path, name: &str, body: &str) {
        let xml = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:v8="http://v8.1c.ru/8.1/data/core">
	<CommonModule uuid="00000000-0000-0000-0000-0000000000{id:02}">
		<Properties>
			<Name>{name}</Name>
			<Global>false</Global>
			<ClientManagedApplication>false</ClientManagedApplication>
			<Server>true</Server>
			<ExternalConnection>false</ExternalConnection>
			<ClientOrdinaryApplication>false</ClientOrdinaryApplication>
			<ServerCall>false</ServerCall>
			<Privileged>false</Privileged>
			<ReturnValuesReuse>DontUse</ReturnValuesReuse>
		</Properties>
	</CommonModule>
</MetaDataObject>"#,
            id = name.len(),
        );
        let xml_path = root.join(format!("CommonModules/{name}.xml"));
        fs::create_dir_all(xml_path.parent().unwrap()).unwrap();
        fs::write(&xml_path, xml).unwrap();
        let module_path = root.join(format!("CommonModules/{name}/Ext/Module.bsl"));
        fs::create_dir_all(module_path.parent().unwrap()).unwrap();
        fs::write(&module_path, body).unwrap();
    }

    /// Write a common module (descriptor XML + `Ext/Module.bsl`) under `base`.
    pub(in crate::state) fn write_common_module_tree(
        base: &std::path::Path,
        name: &str,
        body: &str,
    ) {
        let xml = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <MetaDataObject xmlns=\"http://v8.1c.ru/8.3/MDClasses\">\n\
         \t<CommonModule uuid=\"00000000-0000-0000-0000-000000000001\">\n\
         \t\t<Properties><Name>{name}</Name><Server>true</Server></Properties>\n\
         \t</CommonModule>\n\
         </MetaDataObject>\n"
        );
        fs::create_dir_all(base.join("CommonModules").join(name).join("Ext")).unwrap();
        fs::write(base.join("CommonModules").join(format!("{name}.xml")), xml).unwrap();
        fs::write(base.join("CommonModules").join(name).join("Ext").join("Module.bsl"), body)
            .unwrap();
    }
}
