use super::FusedChunkWriter;
use bsl_search::SearchEngine;
use ide::FusedChunkSink;
use serde_json::Value;
use std::path::Path;
use std::sync::{Arc, Mutex};

const ISSUE441_SCALE_CHUNKS: usize = 542_950;
const ISSUE441_SCALE_DIMENSION: usize = 3;
const ISSUE441_SCALE_EDIT_WINDOW: std::time::Duration = std::time::Duration::from_secs(180);
const ISSUE441_SCALE_PARTIAL_ROWS: usize = 3;

#[derive(Clone, Debug, Default)]
struct ScaleEmbeddingSnapshot {
    requests: usize,
    submissions: usize,
    max_request_body_bytes: usize,
    max_inputs_per_request: usize,
    payload_hashes: std::collections::HashSet<[u8; 32]>,
}

/// The ignored scale fixture uses a real OpenAI-compatible HTTP endpoint and the production
/// embedder/USEARCH path, while keeping vector contents and request receipts deterministic.
struct ScaleEmbeddingServer {
    url: String,
    state: Arc<Mutex<ScaleEmbeddingSnapshot>>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ScaleEmbeddingServer {
    fn start() -> Self {
        use std::net::TcpListener;
        use std::sync::atomic::Ordering;
        use std::time::Duration;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let state = Arc::new(Mutex::new(ScaleEmbeddingSnapshot::default()));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (observed, stopping) = (state.clone(), stop.clone());
        let thread = std::thread::spawn(move || {
            while !stopping.load(Ordering::Acquire) {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(error) => panic!("scale embedding accept: {error}"),
                };
                // Windows hands the listener's non-blocking mode to the accepted stream.
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(test_utils::wait::ceiling(Duration::from_secs(30))))
                    .unwrap();
                stream
                    .set_write_timeout(Some(test_utils::wait::ceiling(Duration::from_secs(30))))
                    .unwrap();
                let body = read_scale_http_body(&mut stream);
                let request: Value = serde_json::from_slice(&body).expect("embedding JSON body");
                let inputs = request["input"].as_array().expect("OpenAI input array");
                let response_data = inputs
                    .iter()
                    .enumerate()
                    .map(|(index, input)| {
                        let input = input.as_str().expect("embedding input string");
                        serde_json::json!({"index": index, "embedding": scale_vector(input)})
                    })
                    .collect::<Vec<_>>();
                {
                    let mut totals = observed.lock().unwrap();
                    totals.requests += 1;
                    totals.submissions += inputs.len();
                    totals.max_request_body_bytes = totals.max_request_body_bytes.max(body.len());
                    totals.max_inputs_per_request = totals.max_inputs_per_request.max(inputs.len());
                    totals.payload_hashes.extend(
                        inputs.iter().map(|input| {
                            *blake3::hash(input.as_str().unwrap().as_bytes()).as_bytes()
                        }),
                    );
                }
                let response = serde_json::json!({"data": response_data}).to_string();
                use std::io::Write;
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response.len(),
                    response
                )
                .unwrap();
            }
        });
        Self { url, state, stop, thread: Some(thread) }
    }

    fn snapshot(&self) -> ScaleEmbeddingSnapshot {
        self.state.lock().unwrap().clone()
    }
}

impl Drop for ScaleEmbeddingServer {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Release);
        let result = self.thread.take().unwrap().join();
        if !std::thread::panicking() {
            result.unwrap();
        }
    }
}

fn read_scale_http_body(stream: &mut std::net::TcpStream) -> Vec<u8> {
    use std::io::Read;
    let mut bytes = Vec::new();
    let mut chunk = [0; 4096];
    loop {
        let read = stream.read(&mut chunk).unwrap();
        assert_ne!(read, 0, "embedding client closed before complete request");
        bytes.extend_from_slice(&chunk[..read]);
        let Some(separator) = bytes.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let headers = String::from_utf8_lossy(&bytes[..separator]).to_ascii_lowercase();
        let length = headers
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .expect("OpenAI request has Content-Length")
            .trim()
            .parse::<usize>()
            .expect("Content-Length integer");
        let start = separator + 4;
        if bytes.len() >= start + length {
            return bytes[start..start + length].to_vec();
        }
    }
}

fn scale_vector(input: &str) -> [f32; ISSUE441_SCALE_DIMENSION] {
    let hash = blake3::hash(input.as_bytes());
    [
        0.25 + f32::from(hash.as_bytes()[0]) / 512.0,
        0.25 + f32::from(hash.as_bytes()[1]) / 512.0,
        0.25 + f32::from(hash.as_bytes()[2]) / 512.0,
    ]
}

fn write_scale_file(root: &Path, relative: &str, content: &str) -> std::path::PathBuf {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, content).unwrap();
    path
}

fn issue441_scope_config() -> String {
    let extensions = (0..5)
        .map(|index| {
            format!(
                "{{ name = \"ScaleExtension{index}\", path = \"src/cfe/ScaleExtension{index}\", scope = \"InfoBase\" }}"
            )
        })
        .collect::<Vec<_>>()
        .join(",\n  ");
    format!("[source]\nroot = \"src/cf\"\nextensions = [\n  {extensions}\n]\n")
}

fn issue441_clear_token_profile_env() -> Vec<crate::state::test_support::EnvVarGuard> {
    use crate::state::test_support::EnvVarGuard;
    vec![
        EnvVarGuard::unset("EMBEDDING_MAX_INPUT_TOKENS"),
        EnvVarGuard::unset("EMBEDDING_TOKENIZER_FILE"),
        EnvVarGuard::unset("EMBEDDING_TOKENIZER_SHA256"),
        EnvVarGuard::unset(crate::broker::EMBEDDING_MAX_INPUT_TOKENS_ENV),
        EnvVarGuard::unset(crate::broker::EMBEDDING_TOKENIZER_FILE_ENV),
        EnvVarGuard::unset(crate::broker::EMBEDDING_TOKENIZER_SHA256_ENV),
    ]
}

fn issue441_metadata_module(name: &str, uuid: &str) -> String {
    format!(
        "<MetaDataObject xmlns=\"http://v8.1c.ru/8.3/MDClasses\" xmlns:v8=\"http://v8.1c.ru/8.1/data/core\"><CommonModule uuid=\"{uuid}\"><Properties><Name>{name}</Name><Global>false</Global><ClientManagedApplication>false</ClientManagedApplication><Server>true</Server><ExternalConnection>false</ExternalConnection><ClientOrdinaryApplication>false</ClientOrdinaryApplication><ServerCall>false</ServerCall><Privileged>false</Privileged><ReturnValuesReuse>DontUse</ReturnValuesReuse></Properties></CommonModule></MetaDataObject>"
    )
}

fn issue441_write_generated_module(root: &Path, relative: &str, count: usize, ordinal: usize) {
    let mut source = String::with_capacity(count * 78);
    for index in 0..count {
        use std::fmt::Write;
        writeln!(
            source,
            "Процедура Issue441Method{ordinal:02}_{index:05}() Экспорт\n    Значение = {index};\nКонецПроцедуры\n"
        )
        .unwrap();
    }
    let module = write_scale_file(root, relative, &source);
    let actual = bsl_search::Chunker::chunk(&source).len();
    assert_eq!(actual, count, "generated methods produce one real code chunk each: {module:?}");
}

fn issue441_generate_workspace(root: &Path, chunk_total: usize) -> std::path::PathBuf {
    write_scale_file(
        root,
        "src/cf/Configuration.xml",
        "<Configuration><Properties><Version>1</Version><Comment>Issue441 initial</Comment></Properties></Configuration>",
    );
    let mut roots = vec![("src/cf/CommonModules/ScaleBase/Ext/Module.bsl".to_owned(), 0)];
    for index in 0..5 {
        let extension = format!("src/cfe/ScaleExtension{index}");
        write_scale_file(
            root,
            &format!("{extension}/Configuration.xml"),
            "<Properties><ConfigurationExtensionPurpose>Customization</ConfigurationExtensionPurpose></Properties>",
        );
        roots.push((format!("{extension}/CommonModules/ScaleExtension{index}/Ext/Module.bsl"), 0));
    }
    for index in 0..16 {
        let external = format!("src/epf/ScaleProcessor{index}");
        if index < 4 {
            write_scale_file(
                root,
                &format!("{external}/README.txt"),
                "invalid auto-discovery control",
            );
            continue;
        }
        let name = format!("ScaleProcessor{index}");
        let uuid = format!("00000000-0000-0000-0000-{index:012}");
        write_scale_file(
            root,
            &format!("{external}/{name}.xml"),
            &format!(
                "<MetaDataObject xmlns=\"http://v8.1c.ru/8.3/MDClasses\"><ExternalDataProcessor uuid=\"{uuid}\"><Properties><Name>{name}</Name></Properties></ExternalDataProcessor></MetaDataObject>"
            ),
        );
        roots.push((format!("{external}/{name}/Ext/ObjectModule.bsl"), 0));
    }
    assert_eq!(roots.len(), 18, "base + five CFE + twelve valid EPF source roots");
    let regular_total = chunk_total - 1;
    let per_root = regular_total / roots.len();
    let extra = regular_total % roots.len();
    for (index, (relative, slot)) in roots.iter_mut().enumerate() {
        *slot = per_root + if index < extra { 1 } else { 0 };
        let module_dir = Path::new(relative).parent().unwrap().parent().unwrap();
        let name = module_dir.file_name().unwrap().to_string_lossy();
        if relative.starts_with("src/cf/") || relative.starts_with("src/cfe/") {
            let metadata = issue441_metadata_module(
                &name,
                &format!("00000000-0000-0000-0000-{:012}", index + 1),
            );
            let metadata_path = module_dir.parent().unwrap().join(format!("{name}.xml"));
            write_scale_file(
                root,
                metadata_path.to_str().expect("scale metadata path is UTF-8"),
                &metadata,
            );
        }
        issue441_write_generated_module(root, relative, *slot, index);
    }
    let delta = write_scale_file(
        root,
        "src/cf/CommonModules/Issue441Delta/Ext/Module.bsl",
        "Процедура Issue441Delta() Экспорт\n    Значение = 1;\nКонецПроцедуры\n",
    );
    write_scale_file(
        root,
        "src/cf/CommonModules/Issue441Delta.xml",
        &issue441_metadata_module("Issue441Delta", "00000000-0000-0000-0000-000000009903"),
    );
    assert_eq!(bsl_search::Chunker::chunk(&std::fs::read_to_string(&delta).unwrap()).len(), 1);
    let catalog = r#"<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses"><Catalog uuid="00000000-0000-0000-0000-000000009901"><Properties><Name>Issue441Control</Name><CodeLength>9</CodeLength></Properties><ChildObjects><Attribute uuid="00000000-0000-0000-0000-000000009902"><Properties><Name>Issue441AttributeA</Name><Type><Type>xs:string</Type></Type></Properties></Attribute></ChildObjects></Catalog></MetaDataObject>"#;
    write_scale_file(root, "src/cf/Catalogs/Issue441Control.xml", catalog);
    write_scale_file(root, "bsl-analyzer.toml", &issue441_scope_config());
    assert_eq!(
        roots.iter().map(|(_, chunks)| chunks).sum::<usize>() + 1,
        chunk_total,
        "generated source has exactly the required number of chunks"
    );
    delta
}

fn issue441_scratch_parent() -> std::path::PathBuf {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().parent().unwrap();
    let scratch = repository.join(".artifacts/issue441/scratch");
    std::fs::create_dir_all(&scratch).expect("create mounted-disk acceptance scratch");
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        assert_eq!(
            std::fs::metadata(repository).unwrap().dev(),
            std::fs::metadata(&scratch).unwrap().dev(),
            "scale fixture scratch must be on the repository's mounted disk, not tmpfs"
        );
    }
    scratch
}

fn issue441_hash_tree(root: &Path) -> String {
    fn visit(root: &Path, dir: &Path, hasher: &mut blake3::Hasher) {
        let mut entries = std::fs::read_dir(dir).unwrap().map(Result::unwrap).collect::<Vec<_>>();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            if path.is_dir() {
                visit(root, &path, hasher);
                continue;
            }
            let relative = path.strip_prefix(root).unwrap().to_string_lossy();
            let bytes = std::fs::read(&path).unwrap();
            hasher.update(&(relative.len() as u64).to_le_bytes());
            hasher.update(relative.as_bytes());
            hasher.update(&(bytes.len() as u64).to_le_bytes());
            hasher.update(&bytes);
        }
    }
    let mut hasher = blake3::Hasher::new();
    visit(root, root, &mut hasher);
    hasher.finalize().to_hex().to_string()
}

fn issue441_graph_projection_hash(path: &Path) -> String {
    use rusqlite::types::ValueRef;

    fn hash_table(hasher: &mut blake3::Hasher, connection: &rusqlite::Connection, sql: &str) {
        let mut statement = connection.prepare(sql).unwrap();
        let columns = statement.column_count();
        let mut rows = statement.query([]).unwrap();
        while let Some(row) = rows.next().unwrap() {
            for column in 0..columns {
                match row.get_ref(column).unwrap() {
                    ValueRef::Null => {
                        hasher.update(&[0]);
                    }
                    ValueRef::Integer(value) => {
                        hasher.update(&[1]);
                        hasher.update(&value.to_le_bytes());
                    }
                    ValueRef::Real(value) => {
                        hasher.update(&[2]);
                        hasher.update(&value.to_le_bytes());
                    }
                    ValueRef::Text(value) | ValueRef::Blob(value) => {
                        hasher.update(&[3]);
                        hasher.update(&(value.len() as u64).to_le_bytes());
                        hasher.update(value);
                    }
                }
            }
            hasher.update(&[0xff]);
        }
    }
    let connection =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let mut hasher = blake3::Hasher::new();
    hash_table(
        &mut hasher,
        &connection,
        "SELECT id,kind,name,qualified,module,file_root_id,file_path,name_offset,sig_end,src_start,src_end,dispatch,is_export,addressable FROM nodes ORDER BY id",
    );
    hash_table(
        &mut hasher,
        &connection,
        "SELECT from_id,to_id,kind,provenance,call_start,call_end,call_absent,crosses FROM edges ORDER BY from_id,to_id,kind,provenance,call_start,call_end,call_absent,crosses",
    );
    hasher.finalize().to_hex().to_string()
}

fn issue441_graph_revision(path: &Path) -> u64 {
    crate::graph_query::GraphDb::open(path).unwrap().freshness_token().unwrap().0
}

fn issue441_configuration_byte_fingerprint(path: &Path) -> Option<[u8; 32]> {
    crate::graph_query::GraphDb::open(path)
        .unwrap()
        .stored_fingerprints()
        .get(&bsl_search::FileKey::configuration("Configuration.xml"))
        .copied()
}

fn issue441_repository_sha() -> String {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(Path::new(env!("CARGO_MANIFEST_DIR")))
        .output()
        .expect("read fixture source SHA");
    assert!(output.status.success(), "git rev-parse HEAD failed");
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn issue441_binary_sha() -> String {
    blake3::hash(&std::fs::read(std::env::current_exe().unwrap()).unwrap()).to_hex().to_string()
}

fn issue441_call_params(action: &str) -> rmcp::model::CallToolRequestParams {
    rmcp::model::CallToolRequestParams::new("graph")
        .with_arguments(serde_json::Map::from_iter([("action".into(), serde_json::json!(action))]))
}

async fn issue441_graph_status(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
) -> Value {
    client
        .call_tool(issue441_call_params("status"))
        .await
        .expect("graph status request")
        .structured_content
        .expect("structured graph status")
}

async fn issue441_graph_overview(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
) -> Value {
    client
        .call_tool(issue441_call_params("overview"))
        .await
        .expect("graph overview request")
        .structured_content
        .expect("structured graph overview")
}

async fn issue441_search_status_report(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
) -> (Value, String) {
    let response = client
        .call_tool(rmcp::model::CallToolRequestParams::new("search").with_arguments(
            serde_json::Map::from_iter([("action".into(), serde_json::json!("status"))]),
        ))
        .await
        .expect("search status request");
    let status = response.structured_content.expect("structured search status");
    let watch_and_backlog = response
        .content
        .iter()
        .filter_map(|block| block.as_text().map(|text| text.text.as_str()))
        .flat_map(str::lines)
        .filter(|line| line.contains("Watch:") || line.contains("Backlog:"))
        .collect::<Vec<_>>()
        .join(" | ");
    (status, watch_and_backlog)
}

fn issue441_target<'a>(status: &'a Value, kind: &str) -> &'a Value {
    status["indexing"]["targets"]
        .as_array()
        .expect("search status targets")
        .iter()
        .find(|target| target["kind"] == kind)
        .unwrap_or_else(|| panic!("missing {kind} target in status: {status}"))
}

async fn issue441_wait_graph_ready(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    previous: Option<u64>,
) -> Value {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3600);
    let mut last_report = tokio::time::Instant::now();
    loop {
        let status = issue441_graph_status(client).await;
        if status["state"] == "ready"
            && status["stale"] == false
            && previous.is_none_or(|generation| status["revision"].as_u64() != Some(generation))
        {
            return status;
        }
        if last_report.elapsed() >= std::time::Duration::from_secs(30) {
            eprintln!("ISSUE441 progress graph_ready status={status}");
            last_report = tokio::time::Instant::now();
        }
        assert!(tokio::time::Instant::now() < deadline, "graph workload deadline: {status}");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

async fn issue441_wait_graph_settled(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    minimum_revision: u64,
) -> Value {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3600);
    let mut candidate: Option<(u64, tokio::time::Instant)> = None;
    let mut last_report = tokio::time::Instant::now();
    loop {
        let status = issue441_graph_status(client).await;
        let Some(revision) = status["revision"].as_u64() else {
            candidate = None;
            if last_report.elapsed() >= std::time::Duration::from_secs(30) {
                eprintln!("ISSUE441 progress graph_settled status={status}");
                last_report = tokio::time::Instant::now();
            }
            assert!(tokio::time::Instant::now() < deadline, "graph workload deadline: {status}");
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            continue;
        };
        if status["state"] == "ready" && status["stale"] == false && revision >= minimum_revision {
            match candidate {
                Some((previous, since)) if previous == revision => {
                    if since.elapsed() >= std::time::Duration::from_secs(2) {
                        return status;
                    }
                }
                _ => candidate = Some((revision, tokio::time::Instant::now())),
            }
        } else {
            candidate = None;
        }
        if last_report.elapsed() >= std::time::Duration::from_secs(30) {
            eprintln!("ISSUE441 progress graph_settled status={status}");
            last_report = tokio::time::Instant::now();
        }
        assert!(tokio::time::Instant::now() < deadline, "graph workload deadline: {status}");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

async fn issue441_wait_search_ready(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
) -> Value {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3600);
    let mut last_report = tokio::time::Instant::now();
    loop {
        let (status, watch_and_backlog) = issue441_search_status_report(client).await;
        if issue441_target(&status, "lexical")["state"] == "ready"
            && issue441_target(&status, "semantic")["state"] == "ready"
        {
            eprintln!(
                "ISSUE441 progress search_ready reached structured={status} text_watch_backlog={watch_and_backlog:?}"
            );
            return status;
        }
        if last_report.elapsed() >= std::time::Duration::from_secs(30) {
            eprintln!(
                "ISSUE441 progress search_ready structured={status} text_watch_backlog={watch_and_backlog:?}"
            );
            last_report = tokio::time::Instant::now();
        }
        assert!(tokio::time::Instant::now() < deadline, "search workload deadline: {status}");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

async fn issue441_start_mcp(
    root: &Path,
    cache: crate::cache::WorkspaceCacheLayout,
) -> (
    crate::state::SharedState,
    crate::McpServer,
    rmcp::service::RunningService<rmcp::RoleClient, ()>,
) {
    use rmcp::ServiceExt;
    let state = crate::state::SharedState::workspace_with_cache(root.to_path_buf(), cache).unwrap();
    let server = crate::McpServer::new(crate::McpProfile::Workspace, state.clone());
    let (client_io, server_io) = tokio::io::duplex(4 * 1024 * 1024);
    tokio::spawn(crate::serve_stream(server.clone(), server_io));
    let client = ().serve(client_io).await.expect("MCP session initializes");
    (state, server, client)
}

async fn issue441_stop_mcp(
    server: crate::McpServer,
    client: rmcp::service::RunningService<rmcp::RoleClient, ()>,
) {
    client.cancel().await.ok();
    server.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "process-wide embedding environment fixture; run explicitly and serially"]
#[allow(
    clippy::await_holding_lock,
    reason = "the process environment lock must span this async fixture so embedding settings stay isolated"
)]
async fn issue441_small_single_edit_smoke() {
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    const CHUNKS: usize = 37;
    let _env_lock = crate::state::test_support::env_lock();
    let _token_profile_env = issue441_clear_token_profile_env();
    let embedding = ScaleEmbeddingServer::start();
    let _test_embedding = crate::state::test_support::EnvVarGuard::set("BSL_TEST_EMBEDDING", "1");
    let _embedding_url =
        crate::state::test_support::EnvVarGuard::set("EMBEDDING_URL", &embedding.url);
    let _embedding_model =
        crate::state::test_support::EnvVarGuard::set("EMBEDDING_MODEL", "issue441-deterministic");
    let _embedding_dim = crate::state::test_support::EnvVarGuard::set(
        "EMBEDDING_DIM",
        &ISSUE441_SCALE_DIMENSION.to_string(),
    );

    let scratch = issue441_scratch_parent();
    let nonce =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let run = scratch.join(format!("smoke-{}-{nonce}", std::process::id()));
    let root = run.join("workspace");
    std::fs::create_dir_all(&root).unwrap();
    issue441_generate_workspace(&root, CHUNKS);
    let cache = crate::cache::WorkspaceCacheLayout::from_root(run.join("cache"))
        .with_workspace(root.clone());
    cache.ensure().unwrap();

    let (state, server, client) = issue441_start_mcp(&root, cache.clone()).await;
    assert_eq!(state.workspace_search_mode(), crate::state::WorkspaceSearchMode::SqliteLocal);
    let initial_graph = issue441_wait_graph_ready(&client, None).await;
    issue441_wait_search_ready(&client).await;
    assert_eq!(issue441_search_vector_count(&state), CHUNKS);
    let initial_submissions = embedding.snapshot().submissions;
    assert_eq!(initial_submissions, CHUNKS);
    let initial_full = state.graph().full_builds_started.load(Ordering::SeqCst);
    assert_eq!(initial_full, 1);

    state.graph().ensure_loading();
    let warm_graph =
        issue441_wait_graph_settled(&client, initial_graph["revision"].as_u64().unwrap()).await;
    issue441_wait_search_ready(&client).await;
    assert_eq!(warm_graph["revision"], initial_graph["revision"]);
    assert_eq!(embedding.snapshot().submissions, initial_submissions);
    assert_eq!(state.graph().full_builds_started.load(Ordering::SeqCst), initial_full);

    issue441_write_delta(&root, 2);
    // OverlayRetry's existing retry schedule starts at its 30-second tick.
    eprintln!("ISSUE441 small smoke: waiting up to 60s for single-edit graph/search publication");
    let single_edit = tokio::time::timeout(Duration::from_secs(60), async {
        let graph =
            issue441_wait_graph_ready(&client, Some(initial_graph["revision"].as_u64().unwrap()))
                .await;
        let search = issue441_wait_search_ready(&client).await;
        (graph, search)
    })
    .await;
    let (single_graph, single_search) = match single_edit {
        Ok(result) => result,
        Err(_) => {
            let graph = issue441_graph_status(&client).await;
            let (search, watch_and_backlog) = issue441_search_status_report(&client).await;
            let embedding_snapshot = embedding.snapshot();
            let (overlay_signals, overlay_stats, qualification) = {
                let engine = state
                    .search_engine()
                    .lock()
                    .expect("search engine lock for timeout diagnostics");
                let engine = engine.as_ref().expect("workspace search engine is ready");
                (
                    engine.workspace_overlay_retry_signals(),
                    engine.workspace_overlay_stats_read_only(),
                    engine.semantic_index_qualification(),
                )
            };
            let overlay_warmup = state
                .overlay_warmup()
                .lock()
                .expect("overlay warmup lock for timeout diagnostics")
                .clone();
            let semantic_runtime = state
                .semantic_runtime()
                .lock()
                .expect("semantic runtime lock for timeout diagnostics")
                .clone();
            panic!(
                "single-edit publication exceeded 60s; graph={graph}; search={search}; Watch/Backlog={watch_and_backlog:?}; mock_requests={} mock_submissions={}; overlay_signals={overlay_signals:?}; overlay_stats={overlay_stats:?}; semantic_qualification={qualification:?}; overlay_warmup={overlay_warmup:?}; semantic_runtime={semantic_runtime:?}",
                embedding_snapshot.requests,
                embedding_snapshot.submissions,
            );
        }
    };
    eprintln!("ISSUE441 small smoke: single graph={single_graph}; search={single_search}");
    assert_eq!(
        embedding.snapshot().submissions - initial_submissions,
        1,
        "the single BSL edit submits exactly one replacement embedding"
    );
    assert_eq!(state.graph().full_builds_started.load(Ordering::SeqCst), initial_full);

    // A Configuration.xml Version/Comment-only edit is semantically ignored by the graph.
    let config_path = root.join("src/cf/Configuration.xml");
    let config_xml = std::fs::read_to_string(&config_path).unwrap();
    let before_noop_status = issue441_graph_status(&client).await;
    let before_noop_revision = before_noop_status["revision"].as_u64().unwrap();
    let before_noop_sqlite_revision = issue441_graph_revision(&cache.graph_db_path());
    assert_eq!(before_noop_sqlite_revision, before_noop_revision);
    assert_eq!(
        issue441_configuration_byte_fingerprint(&cache.graph_db_path()),
        Some(*blake3::hash(config_xml.as_bytes()).as_bytes()),
        "pre-no-op graph byte fingerprint is not settled"
    );
    let before_noop_projection = issue441_graph_projection_hash(&cache.graph_db_path());
    let before_noop_full = state.graph().full_builds_started.load(Ordering::SeqCst);
    let before_noop_embedding = embedding.snapshot();
    let before_noop_submissions = before_noop_embedding.submissions;
    let epf_context_counts = || {
        let connection = rusqlite::Connection::open_with_flags(
            cache.search_db_path(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        connection.query_row(
            "SELECT count(*),sum(c.graph_context IS NOT NULL) FROM chunks c JOIN files f ON f.id=c.file_id WHERE f.root_id LIKE 'src/epf/%'",
            [],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        ).unwrap()
    };
    let before_noop_epf_contexts = epf_context_counts();
    let graph = state.graph();
    let hub = graph.change_hub.as_ref().expect("workspace change hub").clone();
    let before_noop_fact = hub.seq();
    let semantic_noop = config_xml
        .replace("<Version>1</Version>", "<Version>2</Version>")
        .replace("Issue441 initial", "Version-only semantic no-op");
    assert_ne!(semantic_noop, config_xml, "the XML no-op fixture must change file bytes");
    let expected_noop_fingerprint = *blake3::hash(semantic_noop.as_bytes()).as_bytes();
    std::fs::write(&config_path, semantic_noop).unwrap();
    let noop_fact = crate::graph::test_support::wait_for_hub_seq_above(&hub, before_noop_fact);
    let noop_settled = tokio::time::timeout(
        Duration::from_secs(60),
        issue441_wait_graph_ready(&client, Some(before_noop_revision)),
    )
    .await;
    let noop_graph = match noop_settled {
        Ok(status) => status,
        Err(_) => {
            let graph_status = issue441_graph_status(&client).await;
            let sqlite_revision = issue441_graph_revision(&cache.graph_db_path());
            panic!(
                "Version/Comment XML no-op did not settle within 60 seconds; hub_fact={noop_fact}; before_graph={before_noop_status}; after_graph={graph_status}; before_sqlite_revision={before_noop_sqlite_revision}; after_sqlite_revision={sqlite_revision}"
            );
        }
    };
    let noop_revision = noop_graph["revision"].as_u64().unwrap();
    let noop_sqlite_revision = issue441_graph_revision(&cache.graph_db_path());
    assert!(noop_revision >= before_noop_revision, "MCP graph revision moved backwards");
    assert_eq!(noop_sqlite_revision, noop_revision, "MCP and SQLite revisions disagree");
    assert_eq!(
        issue441_graph_projection_hash(&cache.graph_db_path()),
        before_noop_projection,
        "Version/Comment XML no-op changed the graph projection"
    );
    assert_eq!(
        state.graph().full_builds_started.load(Ordering::SeqCst),
        before_noop_full,
        "Version/Comment XML no-op started a full graph build"
    );
    let after_noop_embedding = embedding.snapshot();
    eprintln!(
        "ISSUE441 XML diagnostic: inputs {}→{}, new unique payloads={}, EPF chunks/nonnull contexts {:?}→{:?}",
        before_noop_submissions,
        after_noop_embedding.submissions,
        after_noop_embedding.payload_hashes.difference(&before_noop_embedding.payload_hashes).count(),
        before_noop_epf_contexts,
        epf_context_counts(),
    );
    assert_eq!(
        after_noop_embedding.submissions, before_noop_submissions,
        "Version/Comment XML no-op submitted provider inputs"
    );
    assert_eq!(
        issue441_configuration_byte_fingerprint(&cache.graph_db_path()),
        Some(expected_noop_fingerprint),
        "published graph has not settled the Configuration.xml byte fingerprint"
    );

    let control_path = root.join("src/cf/Catalogs/Issue441Control.xml");
    let control_xml = std::fs::read_to_string(&control_path).unwrap();
    let changed_control_xml = control_xml.replace("Issue441AttributeA", "Issue441AttributeB");
    assert_ne!(changed_control_xml, control_xml, "the XML control must change bytes");
    let before_control_projection = issue441_graph_projection_hash(&cache.graph_db_path());
    let before_control_submissions = embedding.snapshot().submissions;
    std::fs::write(&control_path, changed_control_xml).unwrap();
    let control = tokio::time::timeout(Duration::from_secs(60), async {
        let graph = issue441_wait_graph_ready(&client, Some(noop_revision)).await;
        issue441_wait_search_ready(&client).await;
        graph
    })
    .await
    .expect("catalog attribute control did not settle in graph and search within 60 seconds");
    assert!(
        control["revision"].as_u64().unwrap() > noop_revision,
        "the catalog attribute edit must publish a new graph revision"
    );
    assert_eq!(
        issue441_graph_revision(&cache.graph_db_path()),
        control["revision"].as_u64().unwrap(),
        "the MCP and SQLite graph revisions disagree after the true XML control"
    );
    assert_ne!(
        issue441_graph_projection_hash(&cache.graph_db_path()),
        before_control_projection,
        "the catalog attribute control did not change the graph projection"
    );
    assert_eq!(
        embedding.snapshot().submissions,
        before_control_submissions,
        "the XML-only graph control must not submit embedding inputs"
    );

    let before_stop = embedding.snapshot();
    let stop_started = std::time::Instant::now();
    issue441_stop_mcp(server, client).await;
    drop(state);
    let after_stop = embedding.snapshot();
    let stop_elapsed = stop_started.elapsed();
    let stop_submissions = after_stop.submissions - before_stop.submissions;
    let stop_unique_payloads =
        after_stop.payload_hashes.difference(&before_stop.payload_hashes).count();
    eprintln!(
        "ISSUE441 tiny stop: elapsed={stop_elapsed:?}, submissions={stop_submissions}, new_unique_payloads={stop_unique_payloads}"
    );
    assert_eq!(stop_submissions, 0, "stopping after the true XML control sent embeddings");
    assert_eq!(stop_unique_payloads, 0, "stopping after the true XML control added payloads");

    let before_reopen = after_stop;
    let reopen_started = std::time::Instant::now();
    let (reopened_state, reopened_server, reopened_client) =
        issue441_start_mcp(&root, cache.clone()).await;
    let reopened = tokio::time::timeout(Duration::from_secs(60), async {
        issue441_wait_graph_ready(&reopened_client, None).await;
        issue441_wait_search_ready(&reopened_client).await;
    })
    .await;
    if let Err(error) = reopened {
        let graph = issue441_graph_status(&reopened_client).await;
        let (search, backlog) = issue441_search_status_report(&reopened_client).await;
        let details = reopened_state.search_engine().try_snapshot().and_then(|guard| {
            guard.as_ref().map(|engine| {
                format!(
                    "signals={:?}; stats={:?}; qualification={:?}",
                    engine.workspace_overlay_retry_signals(),
                    engine.workspace_overlay_stats_read_only(),
                    engine.semantic_index_qualification(),
                )
            })
        });
        let warmup = reopened_state
            .overlay_warmup()
            .try_lock()
            .map(|s| format!("{s:?}"))
            .unwrap_or_else(|error| error.to_string());
        let runtime = reopened_state
            .semantic_runtime()
            .try_lock()
            .map(|s| format!("{s:?}"))
            .unwrap_or_else(|error| error.to_string());
        panic!(
            "same-cache reopen exceeded60s: {error}; graph={graph}; search={search}; backlog={backlog:?}; engine={details:?}; warmup={warmup:?}; runtime={runtime:?}"
        );
    }
    let after_reopen = embedding.snapshot();
    let reopen_elapsed = reopen_started.elapsed();
    let reopen_submissions = after_reopen.submissions - before_reopen.submissions;
    let reopen_unique_payloads =
        after_reopen.payload_hashes.difference(&before_reopen.payload_hashes).count();
    eprintln!(
        "ISSUE441 tiny reopen: elapsed={reopen_elapsed:?}, submissions={reopen_submissions}, new_unique_payloads={reopen_unique_payloads}"
    );
    assert_eq!(issue441_search_vector_count(&reopened_state), CHUNKS);
    assert_eq!(
        reopen_submissions, 0,
        "reopening the same workspace/cache reuses the persisted edited vector"
    );
    assert_eq!(reopen_unique_payloads, 0, "reopen added a new embedding payload");
    let before_partial_status = issue441_graph_status(&reopened_client).await;
    let before_partial_revision = before_partial_status["revision"].as_u64().unwrap();
    let before_partial_projection = issue441_graph_projection_hash(&cache.graph_db_path());
    let (before_partial_rows, before_partial_vector_hash) =
        issue441_search_vector_hash(&cache.search_db_path());
    assert_eq!(before_partial_rows, CHUNKS);
    let partial_full_builds_before =
        reopened_state.graph().full_builds_started.load(std::sync::atomic::Ordering::SeqCst);
    assert_eq!(partial_full_builds_before, 0);

    issue441_stop_mcp(reopened_server, reopened_client).await;
    drop(reopened_state);

    let search_db = cache.search_db_path();
    let missing_ids = issue441_null_some_vectors(&search_db, ISSUE441_SCALE_PARTIAL_ROWS);
    let (mutated_rows, mutated_vector_hash) = issue441_search_vector_hash(&search_db);
    assert_eq!(mutated_rows, CHUNKS);
    assert_ne!(mutated_vector_hash, before_partial_vector_hash);
    let before_partial_submissions = embedding.snapshot().submissions;
    let before_partial_requests = embedding.snapshot().requests;
    let partial_started = std::time::Instant::now();
    let (partial_state, partial_server, partial_client) =
        issue441_start_mcp(&root, cache.clone()).await;
    let partial_ready = tokio::time::timeout(Duration::from_secs(60), async {
        let graph = issue441_wait_graph_ready(&partial_client, None).await;
        let (search_status, qualification) = issue441_wait_raw_semantic_work(
            &partial_client,
            &partial_state,
            CHUNKS,
            true,
            Duration::from_secs(60),
        )
        .await;
        (graph, search_status, qualification)
    })
    .await;
    let (partial_graph, partial_search_status, partial_qualification) = match partial_ready {
        Ok(result) => result,
        Err(error) => {
            let graph = issue441_graph_status(&partial_client).await;
            let (search, backlog) = issue441_search_status_report(&partial_client).await;
            let engine = partial_state.search_engine().try_snapshot().and_then(|guard| {
                guard.as_ref().map(|engine| {
                    format!(
                        "signals={:?}; stats={:?}; qualification={:?}",
                        engine.workspace_overlay_retry_signals(),
                        engine.workspace_overlay_stats_read_only(),
                        engine.semantic_index_qualification(),
                    )
                })
            });
            let progress = partial_state.index_progress().snapshot();
            let runtime = partial_state
                .semantic_runtime()
                .try_lock()
                .map(|status| format!("{status:?}"))
                .unwrap_or_else(|error| error.to_string());
            let embedding_snapshot = embedding.snapshot();
            panic!(
                "partial same-cache restart exceeded 60s: {error}; missing_ids={missing_ids:?}; graph={graph}; search={search}; backlog={backlog:?}; engine={engine:?}; progress={progress:?}; runtime={runtime}; requests_delta={}; submissions_delta={}",
                embedding_snapshot.requests - before_partial_requests,
                embedding_snapshot.submissions - before_partial_submissions,
            );
        }
    };
    eprintln!(
        "ISSUE441 tiny partial qualification: status={}, qualification={partial_qualification:?}",
        issue441_target(&partial_search_status, "semantic")
    );
    let partial_snapshot = embedding.snapshot();
    let partial_submissions = partial_snapshot.submissions - before_partial_submissions;
    let partial_requests = partial_snapshot.requests - before_partial_requests;
    eprintln!(
        "ISSUE441 tiny partial restart: elapsed={:?}, requests={partial_requests}, submissions={partial_submissions}, missing_ids={missing_ids:?}",
        partial_started.elapsed()
    );
    assert_eq!(partial_submissions, ISSUE441_SCALE_PARTIAL_ROWS);
    assert!(partial_requests > 0, "partial restart sent no HTTP embedding requests");
    assert_eq!(partial_graph["revision"], before_partial_revision);
    assert_eq!(
        partial_state.graph().full_builds_started.load(std::sync::atomic::Ordering::SeqCst),
        partial_full_builds_before
    );
    assert_eq!(issue441_search_vector_count(&partial_state), CHUNKS);
    let (restored_rows, restored_vector_hash) = issue441_search_vector_hash(&search_db);
    assert_eq!(restored_rows, CHUNKS);
    assert_eq!(restored_vector_hash, before_partial_vector_hash);
    assert_eq!(issue441_graph_projection_hash(&cache.graph_db_path()), before_partial_projection);
    let partial_hits = issue441_search_hits_match_sqlite(&partial_state, &search_db);
    issue441_stop_mcp(partial_server, partial_client).await;
    drop(partial_state);

    let sidecar = issue441_append_suffix(&search_db, ".usearch.json");
    assert!(sidecar.exists(), "partial publication writes a current USEARCH sidecar");
    std::fs::remove_file(&sidecar).unwrap();
    let before_sidecar_restart = embedding.snapshot();
    let sidecar_started = std::time::Instant::now();
    let (sidecar_state, sidecar_server, sidecar_client) =
        issue441_start_mcp(&root, cache.clone()).await;
    let sidecar_ready = tokio::time::timeout(Duration::from_secs(60), async {
        let graph = issue441_wait_graph_ready(&sidecar_client, None).await;
        let (search_status, qualification) = issue441_wait_raw_semantic_work(
            &sidecar_client,
            &sidecar_state,
            CHUNKS,
            false,
            Duration::from_secs(60),
        )
        .await;
        (graph, search_status, qualification)
    })
    .await;
    let (sidecar_graph, sidecar_search_status, sidecar_qualification) = match sidecar_ready {
        Ok(result) => result,
        Err(error) => {
            let graph = issue441_graph_status(&sidecar_client).await;
            let (search, backlog) = issue441_search_status_report(&sidecar_client).await;
            let engine = sidecar_state.search_engine().try_snapshot().and_then(|guard| {
                guard.as_ref().map(|engine| {
                    format!(
                        "signals={:?}; stats={:?}; qualification={:?}",
                        engine.workspace_overlay_retry_signals(),
                        engine.workspace_overlay_stats_read_only(),
                        engine.semantic_index_qualification(),
                    )
                })
            });
            let progress = sidecar_state.index_progress().snapshot();
            let runtime = sidecar_state
                .semantic_runtime()
                .try_lock()
                .map(|status| format!("{status:?}"))
                .unwrap_or_else(|error| error.to_string());
            let embedding_snapshot = embedding.snapshot();
            panic!(
                "sidecar-only same-cache reopen exceeded 60s: {error}; graph={graph}; search={search}; backlog={backlog:?}; engine={engine:?}; progress={progress:?}; runtime={runtime}; requests_delta={}; submissions_delta={}",
                embedding_snapshot.requests - before_sidecar_restart.requests,
                embedding_snapshot.submissions - before_sidecar_restart.submissions,
            );
        }
    };
    eprintln!(
        "ISSUE441 tiny sidecar qualification: status={}, qualification={sidecar_qualification:?}",
        issue441_target(&sidecar_search_status, "semantic")
    );
    let sidecar_snapshot = embedding.snapshot();
    assert_eq!(
        sidecar_snapshot.submissions - before_sidecar_restart.submissions,
        0,
        "rebuilding only USEARCH from complete SQLite vectors sends no embeddings"
    );
    assert_eq!(
        sidecar_snapshot.requests - before_sidecar_restart.requests,
        0,
        "sidecar-only reopen issues no HTTP requests"
    );
    assert_eq!(sidecar_graph["revision"], before_partial_revision);
    assert_eq!(issue441_search_vector_count(&sidecar_state), CHUNKS);
    assert_eq!(
        issue441_search_hits_match_sqlite(&sidecar_state, &search_db),
        partial_hits,
        "sidecar rebuild preserves the current SQLite hit ids and scores"
    );
    assert_eq!(
        sidecar_state.graph().full_builds_started.load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    assert_eq!(issue441_graph_projection_hash(&cache.graph_db_path()), before_partial_projection);
    eprintln!(
        "ISSUE441 tiny sidecar-only reopen: elapsed={:?}, requests=0, submissions=0",
        sidecar_started.elapsed()
    );
    issue441_stop_mcp(sidecar_server, sidecar_client).await;
    drop(sidecar_state);
}

fn issue441_write_delta(root: &Path, revision: usize) {
    write_scale_file(
        root,
        "src/cf/CommonModules/Issue441Delta/Ext/Module.bsl",
        &format!("Процедура Issue441Delta() Экспорт\n    Значение = {revision};\nКонецПроцедуры\n"),
    );
}

fn issue441_search_vector_hash(path: &Path) -> (usize, String) {
    use rusqlite::types::ValueRef;

    let connection =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let mut statement = connection.prepare("SELECT id,embedding FROM chunks ORDER BY id").unwrap();
    let mut rows = statement.query([]).unwrap();
    let mut count = 0;
    let mut hasher = blake3::Hasher::new();
    while let Some(row) = rows.next().unwrap() {
        hasher.update(&row.get::<_, i64>(0).unwrap().to_le_bytes());
        match row.get_ref(1).unwrap() {
            ValueRef::Blob(bytes) => {
                hasher.update(&(bytes.len() as u64).to_le_bytes());
                hasher.update(bytes);
            }
            ValueRef::Null => {
                hasher.update(&[0]);
            }
            other => panic!("unexpected embedding storage: {other:?}"),
        };
        count += 1;
    }
    (count, hasher.finalize().to_hex().to_string())
}

fn issue441_semantic_vector_hash(path: &Path) -> (usize, String) {
    use rusqlite::types::ValueRef;

    let connection =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let mut statement = connection
        .prepare(
            "SELECT f.root_id,f.path,c.kind,c.symbol_name,c.graph_context,c.text,c.embedding \
             FROM chunks c JOIN files f ON f.id=c.file_id \
             WHERE f.collection='code'",
        )
        .unwrap();
    let mut rows = statement.query([]).unwrap();
    let mut keyed_vectors = Vec::new();
    while let Some(row) = rows.next().unwrap() {
        let root_id: String = row.get(0).unwrap();
        let path: String = row.get(1).unwrap();
        let kind: String = row.get(2).unwrap();
        let symbol_name: String = row.get(3).unwrap();
        let graph_context: Option<String> = row.get(4).unwrap();
        let text: String = row.get(5).unwrap();
        let embedding = match row.get_ref(6).unwrap() {
            ValueRef::Blob(bytes) => bytes.to_vec(),
            other => panic!("expected complete semantic embedding, got {other:?}"),
        };
        let document = bsl_search::IndexedDocument {
            collection: "code".to_owned(),
            root_id: root_id.clone(),
            path: path.clone(),
            symbol_name,
            kind,
            line_start: 0,
            line_end: 0,
            content_hash: String::new(),
            text,
            graph_context,
            source_span: None,
        };
        let semantic_key = bsl_search::semantic_key_for_indexed_document(&document);
        keyed_vectors.push((root_id, path, semantic_key, embedding));
    }
    keyed_vectors
        .sort_by(|left, right| (&left.0, &left.1, &left.2).cmp(&(&right.0, &right.1, &right.2)));
    let mut hasher = blake3::Hasher::new();
    let mut previous_key: Option<(&str, &str, &str)> = None;
    for (root_id, path, semantic_key, embedding) in &keyed_vectors {
        let key = (root_id.as_str(), path.as_str(), semantic_key.as_str());
        assert_ne!(previous_key, Some(key), "semantic vector key must identify one current row");
        previous_key = Some(key);
        for part in [root_id.as_bytes(), path.as_bytes(), semantic_key.as_bytes()] {
            hasher.update(&(part.len() as u64).to_le_bytes());
            hasher.update(part);
        }
        hasher.update(&(embedding.len() as u64).to_le_bytes());
        hasher.update(embedding);
    }
    (keyed_vectors.len(), hasher.finalize().to_hex().to_string())
}

fn issue441_null_some_vectors(path: &Path, count: usize) -> Vec<i64> {
    let mut connection = rusqlite::Connection::open(path).unwrap();
    let ids = {
        let mut statement = connection
            .prepare("SELECT id FROM chunks WHERE embedding IS NOT NULL ORDER BY id LIMIT ?1")
            .unwrap();
        statement
            .query_map([i64::try_from(count).expect("small partial vector count")], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    };
    assert_eq!(ids.len(), count, "the scale index contains enough vectors to resume");
    let transaction = connection.transaction().unwrap();
    for id in &ids {
        let changed =
            transaction.execute("UPDATE chunks SET embedding=NULL WHERE id=?1", [id]).unwrap();
        assert_eq!(changed, 1, "only the selected current vector row is cleared");
    }
    transaction.commit().unwrap();
    ids
}

fn issue441_append_suffix(path: &Path, suffix: &str) -> std::path::PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    value.into()
}

fn issue441_file_device(path: &Path) -> u64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(path).unwrap().dev()
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        0
    }
}

fn issue441_search_vector_count(state: &crate::state::SharedState) -> usize {
    state
        .search_engine()
        .lock()
        .expect("search engine lock")
        .as_ref()
        .expect("workspace search engine is ready")
        .vector_count()
}

/// Wait for raw-profile native work to finish without requiring unknown vectors to become
/// semantically qualified. A partial restart must finish a native pass; a sidecar-only rebuild
/// may need none. The API status is recorded as-is and can remain Waiting/Unknown because raw
/// model provenance is intentionally unverified.
async fn issue441_wait_raw_semantic_work(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    state: &crate::state::SharedState,
    expected_vectors: usize,
    require_completed_pass: bool,
    timeout: std::time::Duration,
) -> (Value, bsl_search::SemanticIndexQualification) {
    use crate::state::SemanticRuntimeStatus;
    use bsl_search::IndexPassState;

    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let (status, backlog) = issue441_search_status_report(client).await;
        let progress = state.index_progress().snapshot();
        let runtime = state.semantic_runtime().try_lock().ok().map(|value| value.clone());
        let engine_state = state.search_engine().try_snapshot().and_then(|guard| {
            guard.as_ref().map(|engine| {
                (
                    engine.semantic_index_qualification(),
                    engine.try_workspace_overlay_retry_signals(),
                    engine.vector_count(),
                )
            })
        });
        if let (Some(runtime), Some((qualification, Some(signals), index_vectors))) =
            (runtime.as_ref(), engine_state.as_ref())
        {
            let pass_quiet = progress.as_ref().is_some_and(|sample| {
                !sample.active
                    && sample.state != IndexPassState::Running
                    && if require_completed_pass {
                        sample.state == IndexPassState::Ready && sample.pass_id.is_some()
                    } else {
                        sample.pass_id.is_none() || sample.state == IndexPassState::Ready
                    }
            });
            let no_overlay_debt = signals.initialized
                && !signals.needs_full_rescan
                && signals.pending_dirty_paths == 0
                && signals.unembedded_entries == 0
                && signals.unread_keys == 0;
            let semantic_not_running = issue441_target(&status, "semantic")["state"] != "running";
            if matches!(runtime, SemanticRuntimeStatus::Ready)
                && pass_quiet
                && no_overlay_debt
                && !qualification.context_pending
                && !qualification.identity_verified
                && semantic_not_running
                && *index_vectors == expected_vectors
            {
                eprintln!(
                    "ISSUE441 raw semantic work settled: require_completed_pass={require_completed_pass}, semantic_status={}, qualification={qualification:?}, progress={progress:?}, runtime={runtime:?}, overlay={signals:?}, index_vectors={index_vectors}/{expected_vectors}, backlog={backlog}",
                    issue441_target(&status, "semantic")
                );
                return (status, *qualification);
            }
        }
        let diagnostic = format!(
            "status={status}; backlog={backlog}; progress={progress:?}; runtime={runtime:?}; engine={engine_state:?}"
        );
        assert!(
            tokio::time::Instant::now() < deadline,
            "raw semantic work did not settle within {timeout:?}: {diagnostic}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

fn issue441_search_hits_match_sqlite(
    state: &crate::state::SharedState,
    search_db: &Path,
) -> Vec<(i64, f32)> {
    let query = scale_vector("issue441 sidecar current-index query");
    let hits = state
        .search_engine()
        .lock()
        .expect("search engine lock")
        .as_ref()
        .expect("workspace search engine is ready")
        .search_with_embedding(&query, 10, Some("code"))
        .expect("query the reopened USEARCH index");
    assert_eq!(hits.len(), 10, "the persisted index answers a ten-hit query");

    let connection = rusqlite::Connection::open_with_flags(
        search_db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let mut statement = connection
        .prepare(
            "SELECT c.id,c.embedding FROM chunks c JOIN files f ON f.id=c.file_id \
             WHERE f.root_id=?1 AND f.path=?2 AND c.kind=?3 AND c.symbol_name=?4 \
             AND f.collection='code'",
        )
        .unwrap();
    let mut ids = std::collections::HashSet::new();
    let mut current = Vec::with_capacity(hits.len());
    for hit in hits {
        let (id, bytes): (i64, Vec<u8>) = statement
            .query_row(
                rusqlite::params![hit.root_id, hit.file_path, hit.kind, hit.symbol_name],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("USEARCH hit resolves to a current SQLite chunk row");
        assert!(ids.insert(id), "USEARCH returned duplicate current SQLite chunk id {id}");
        assert_eq!(bytes.len(), ISSUE441_SCALE_DIMENSION * 4);
        let vector =
            bytes.as_chunks::<4>().0.iter().copied().map(f32::from_le_bytes).collect::<Vec<_>>();
        let dot = query.iter().zip(&vector).map(|(a, b)| a * b).sum::<f32>();
        let query_norm = query.iter().map(|value| value * value).sum::<f32>().sqrt();
        let vector_norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
        let sqlite_score = dot / (query_norm * vector_norm);
        assert!(
            (hit.score - sqlite_score).abs() < 0.0001,
            "USEARCH score {} disagrees with current SQLite row {id} score {sqlite_score}",
            hit.score
        );
        current.push((id, hit.score));
    }
    current
}

/// End-to-end acceptance workload for the issue441 graph and vector lifecycle. It deliberately
/// stays ignored: the generated corpus contains 542,950 real chunks and takes substantial disk,
/// memory, CPU, and time. The regular graph tests remain small and deterministic.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "large mounted-disk MCP/SQLite/USEARCH acceptance workload"]
#[allow(
    clippy::await_holding_lock,
    reason = "the process environment lock must span this async workload so embedding settings stay isolated"
)]
async fn issue441_scale_workload() {
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    let _env_lock = crate::state::test_support::env_lock();
    let _token_profile_env = issue441_clear_token_profile_env();
    let embedding = ScaleEmbeddingServer::start();
    let _test_embedding = crate::state::test_support::EnvVarGuard::set("BSL_TEST_EMBEDDING", "1");
    let _embedding_url =
        crate::state::test_support::EnvVarGuard::set("EMBEDDING_URL", &embedding.url);
    let _embedding_model =
        crate::state::test_support::EnvVarGuard::set("EMBEDDING_MODEL", "issue441-deterministic");
    let _embedding_dim = crate::state::test_support::EnvVarGuard::set(
        "EMBEDDING_DIM",
        &ISSUE441_SCALE_DIMENSION.to_string(),
    );

    let scratch = issue441_scratch_parent();
    let nonce =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let run = scratch.join(format!("scale-{}-{nonce}", std::process::id()));
    let root = run.join("workspace");
    std::fs::create_dir_all(&root).unwrap();
    let generation_started = Instant::now();
    let generated_at = generation_started;
    let delta = issue441_generate_workspace(&root, ISSUE441_SCALE_CHUNKS);
    let main_cache = crate::cache::WorkspaceCacheLayout::from_root(run.join("cache-main"))
        .with_workspace(root.clone());
    main_cache.ensure().unwrap();

    let generated = issue441_hash_tree(&root);
    let generation_elapsed = generation_started.elapsed();
    let repository_sha = issue441_repository_sha();
    let binary_sha = issue441_binary_sha();
    let initial_started = Instant::now();
    let before_initial_embedding = embedding.snapshot();
    let (state, server, client) = issue441_start_mcp(&root, main_cache.clone()).await;

    // Cold start is driven through real MCP status requests. Both graph and search background
    // owners must publish before the warm passes begin.
    let initial_graph = issue441_wait_graph_ready(&client, None).await;
    issue441_wait_search_ready(&client).await;
    assert_eq!(
        issue441_search_vector_count(&state),
        ISSUE441_SCALE_CHUNKS,
        "the real USEARCH index contains one embedding per generated graph chunk"
    );
    let cold_submissions = embedding.snapshot().submissions;
    let cold_requests = embedding.snapshot().requests - before_initial_embedding.requests;
    assert_eq!(cold_submissions, ISSUE441_SCALE_CHUNKS, "cold pass submits every payload once");
    let initial_full = state.graph().full_builds_started.load(Ordering::SeqCst);
    assert_eq!(initial_full, 1, "one initial projection publishes the generated workspace");
    let initial_elapsed = initial_started.elapsed();

    // An unchanged request/load check does not project or submit embeddings again.
    let warm_started = Instant::now();
    state.graph().ensure_loading();
    let warm_graph =
        issue441_wait_graph_settled(&client, initial_graph["revision"].as_u64().unwrap()).await;
    issue441_wait_search_ready(&client).await;
    assert_eq!(warm_graph["revision"], initial_graph["revision"]);
    assert_eq!(state.graph().full_builds_started.load(Ordering::SeqCst), initial_full);
    assert_eq!(embedding.snapshot().submissions, cold_submissions);
    let warm_elapsed = warm_started.elapsed();

    // A one-method edit exercises the live BSL path and makes requested payload count distinct
    // from generation size: only this missing/replaced input may reach the provider.
    let single_edit_started = Instant::now();
    let before_single_edit = embedding.snapshot();
    issue441_write_delta(&root, 2);
    eprintln!("ISSUE441 single_edit: before graph_ready revision={}", initial_graph["revision"]);
    let single_graph =
        issue441_wait_graph_ready(&client, Some(initial_graph["revision"].as_u64().unwrap())).await;
    eprintln!("ISSUE441 single_edit: after graph_ready status={single_graph}");
    eprintln!("ISSUE441 single_edit: before search_ready");
    issue441_wait_search_ready(&client).await;
    eprintln!("ISSUE441 single_edit: after search_ready");
    let single_submissions = embedding.snapshot().submissions;
    let single_requests = embedding.snapshot().requests - before_single_edit.requests;
    assert_eq!(
        single_submissions - cold_submissions,
        1,
        "one BSL payload edit submits exactly one replacement embedding"
    );
    assert_eq!(state.graph().full_builds_started.load(Ordering::SeqCst), initial_full);
    let single_edit_elapsed = single_edit_started.elapsed();

    // The one-shot SQL budget seam makes the first eligible patch move to a private candidate.
    let graph = state.graph();
    let hub = graph.change_hub.as_ref().expect("workspace change hub").clone();
    let private_entered = std::sync::mpsc::sync_channel(1);
    let (private_entered_tx, private_entered_rx) = private_entered;
    let private_released = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    let release_private = private_released.clone();
    let first_private_hook = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let first_private_hook_once = first_private_hook.clone();
    graph.set_patch_precommit_hook_for_test(Arc::new(move |_| {
        if !first_private_hook_once.swap(true, Ordering::SeqCst) {
            private_entered_tx.send(Instant::now()).expect("private patch reached SQL barrier");
            let (ready, wake) = &*release_private;
            let ready = ready.lock().unwrap();
            let (ready, timeout) =
                wake.wait_timeout_while(ready, Duration::from_secs(30), |ready| !*ready).unwrap();
            assert!(*ready && !timeout.timed_out(), "private patch barrier timed out");
        }
    }));
    graph.inject_next_patch_budget_for_test();
    eprintln!("ISSUE441 private_sql: before base overview");
    let private_base = issue441_graph_overview(&client).await;
    eprintln!("ISSUE441 private_sql: after base overview status={private_base}");
    let private_base_revision = private_base["revision"].as_u64().expect("overview revision");
    let private_graph_path = main_cache.graph_db_path();
    let private_edit_fact = hub.seq();
    issue441_write_delta(&root, 3);
    crate::graph::test_support::wait_for_hub_seq_above(&hub, private_edit_fact);
    let private_entered_at = tokio::task::spawn_blocking(move || {
        private_entered_rx.recv_timeout(Duration::from_secs(3600))
    })
    .await
    .expect("private patch barrier waiter")
    .expect("budgeted patch reached its private SQL transaction");
    let private_read_started = Instant::now();
    eprintln!("ISSUE441 private_sql: before held-reader overview");
    let private_during = issue441_graph_overview(&client).await;
    eprintln!("ISSUE441 private_sql: after held-reader overview status={private_during}");
    let private_during_read_elapsed = private_read_started.elapsed();
    assert_eq!(private_during["revision"], private_base_revision);
    assert_eq!(private_during["result"], private_base["result"]);
    assert!(private_during.get("error").is_none(), "private SQL work refused a graph reader");
    tokio::time::sleep_until(tokio::time::Instant::from_std(
        private_entered_at + Duration::from_secs(6),
    ))
    .await;
    let private_after_read_started = Instant::now();
    eprintln!("ISSUE441 private_sql: before second held-reader overview");
    let private_after_hold = issue441_graph_overview(&client).await;
    eprintln!(
        "ISSUE441 private_sql: after second held-reader overview status={private_after_hold}"
    );
    let private_after_hold_read_elapsed = private_after_read_started.elapsed();
    assert_eq!(private_after_hold["revision"], private_base_revision);
    assert_eq!(private_after_hold["result"], private_base["result"]);
    assert!(private_after_hold.get("error").is_none());

    // This supported body event is delivered while the private SQL transaction is parked.
    // Yield the next catch-up step exactly once so only the graph debt alarm can resume it.
    let private_resume_before = graph.catchup_stats_for_test().1;
    let private_event_before = hub.seq();
    issue441_write_delta(&root, 4);
    let private_barrier_edit_fact =
        crate::graph::test_support::wait_for_hub_seq_above(&hub, private_event_before);
    let private_barrier_source = std::fs::read(&delta).unwrap();
    graph.yield_next_catchup_step_for_test();
    let private_alarm_before_release = graph.alarms.load(Ordering::SeqCst);
    {
        let (ready, wake) = &*private_released;
        *ready.lock().unwrap() = true;
        wake.notify_all();
    }
    let private_barrier_elapsed = private_entered_at.elapsed();
    assert!(private_barrier_elapsed >= Duration::from_secs(6));
    graph.set_patch_precommit_hook_for_test(Arc::new(|_| {}));

    let private_continuation_started = Instant::now();
    let (private_resumes_after, private_frontier, private_revision) =
        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                let (_, resumes, frontier) = graph.catchup_stats_for_test();
                let revision = issue441_graph_revision(&private_graph_path);
                if resumes > private_resume_before
                    && frontier >= private_barrier_edit_fact
                    && revision > private_base_revision
                {
                    break (resumes, frontier, revision);
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect(
            "the graph debt alarm did not resume the pending private candidate within 60 seconds",
        );
    let private_continuation_elapsed = private_continuation_started.elapsed();
    let private_alarm_after_resume = graph.alarms.load(Ordering::SeqCst);
    assert!(private_resumes_after > private_resume_before);
    assert!(
        private_alarm_after_resume > private_alarm_before_release,
        "the graph owner did not wake its debt alarm after yielding the pending candidate"
    );
    assert!(private_frontier >= private_barrier_edit_fact);
    assert!(private_revision > private_base_revision);
    assert_eq!(
        std::fs::read(&delta).unwrap(),
        private_barrier_source,
        "the source stayed unchanged after the barrier edit until automatic resume completed",
    );
    eprintln!("ISSUE441 private_resume: before published overview");
    let private_resumed_graph = issue441_graph_overview(&client).await;
    eprintln!("ISSUE441 private_resume: after published overview status={private_resumed_graph}");
    assert_eq!(private_resumed_graph["revision"].as_u64(), Some(private_revision));
    assert!(private_resumed_graph.get("error").is_none());

    // Keep a real 180-second stream of BSL body edits after the deterministic private resume.
    let before_stream = embedding.snapshot();
    let stream_started = Instant::now();

    let stream_origin = Instant::now();
    for revision in 5..185 {
        issue441_write_delta(&root, revision);
        let next = stream_origin + Duration::from_secs((revision - 4) as u64);
        tokio::time::sleep_until(tokio::time::Instant::from_std(next)).await;
    }
    let stream_elapsed = stream_started.elapsed();
    assert!(
        stream_elapsed >= ISSUE441_SCALE_EDIT_WINDOW,
        "edit stream ran only {stream_elapsed:?}"
    );
    let stream_convergence_started = Instant::now();
    let (stream_graph, _) = tokio::time::timeout(Duration::from_secs(60), async {
        let streamed = issue441_graph_status(&client).await;
        let graph =
            issue441_wait_graph_settled(&client, streamed["revision"].as_u64().unwrap_or(0)).await;
        issue441_wait_search_ready(&client).await;
        (graph, ())
    })
    .await
    .expect("graph and search did not converge within 60 seconds after quiet");
    let stream_quiet_convergence = stream_convergence_started.elapsed();
    let stream_submissions = embedding.snapshot().submissions;
    let stream_requests = embedding.snapshot().requests - before_stream.requests;
    let stream_submission_delta = stream_submissions - before_stream.submissions;
    assert!(
        (1..=180).contains(&stream_submission_delta),
        "the 180-edit stream must submit only changed method payloads: {stream_submission_delta}"
    );
    let (passes_after, _, stream_frontier) = state.graph().catchup_stats_for_test();
    assert!(stream_frontier > 0, "no event frontier was recorded for catch-up");
    assert_eq!(state.graph().full_builds_started.load(Ordering::SeqCst), initial_full);

    // A forced full projection is parked after lowering. An edit lands while it is in flight;
    // publication then has to catch that fact up without starting another full projection.
    let before_force = stream_graph["revision"].as_u64().unwrap();
    let before_force_overview = issue441_graph_overview(&client).await;
    let before_force_embedding = embedding.snapshot();
    let graph = state.graph();
    let hub = graph.change_hub.as_ref().expect("workspace change hub").clone();
    let entered = std::sync::mpsc::sync_channel(1);
    let (entered_tx, entered_rx) = entered;
    let released = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    let release_gate = released.clone();
    let first_candidate = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let first_candidate_hook = first_candidate.clone();
    graph.set_build_candidate_hook_for_test(Arc::new(move |_| {
        if !first_candidate_hook.swap(true, Ordering::SeqCst) {
            entered_tx.send(Instant::now()).expect("scale full build reached candidate hook");
            let (ready, wake) = &*release_gate;
            let ready = ready.lock().unwrap();
            let (ready, timeout) =
                wake.wait_timeout_while(ready, Duration::from_secs(600), |ready| !*ready).unwrap();
            assert!(*ready && !timeout.timed_out(), "scale candidate barrier timed out");
        }
    }));
    let forced_fact = hub.seq();
    let forced_started = Instant::now();
    graph.record_forced_quietly(forced_fact);
    graph.drive();
    let forced_entered_at = entered_rx
        .recv_timeout(Duration::from_secs(3600))
        .expect("forced full projection reaches its post-lowering barrier");
    let edit_fact = hub.seq();
    let source_before_forced_edit = std::fs::read(&delta).unwrap();
    issue441_write_delta(&root, 185);
    let source_after_forced_edit = std::fs::read(&delta).unwrap();
    assert_ne!(
        source_after_forced_edit, source_before_forced_edit,
        "the forced-full straddle must be a real source edit",
    );
    crate::graph::test_support::wait_for_hub_seq_above(&hub, edit_fact);
    let forced_during = issue441_graph_overview(&client).await;
    assert_eq!(forced_during["revision"], before_force);
    assert_eq!(forced_during["result"], before_force_overview["result"]);
    assert!(forced_during.get("error").is_none());
    tokio::time::sleep_until(tokio::time::Instant::from_std(
        forced_entered_at + Duration::from_secs(61),
    ))
    .await;
    {
        let (ready, wake) = &*released;
        *ready.lock().unwrap() = true;
        wake.notify_all();
    }
    let forced_barrier_elapsed = forced_entered_at.elapsed();
    assert!(forced_barrier_elapsed >= Duration::from_secs(61));
    let forced_full_elapsed = forced_started.elapsed();
    let forced_catchup_started = Instant::now();
    let (straddled_graph, _) = tokio::time::timeout(Duration::from_secs(60), async {
        let graph = issue441_wait_graph_settled(&client, before_force + 1).await;
        issue441_wait_search_ready(&client).await;
        (graph, ())
    })
    .await
    .expect("straddled full candidate did not catch up within 60 seconds after release");
    let forced_catchup_elapsed = forced_catchup_started.elapsed();
    graph.set_build_candidate_hook_for_test(Arc::new(|_| {}));
    let full_after_force = graph.full_builds_started.load(Ordering::SeqCst);
    assert_eq!(
        full_after_force,
        initial_full + 1,
        "forced projection was repeated after its straddled edit"
    );
    let (passes_after_force, resumes_after_force, forced_frontier) = graph.catchup_stats_for_test();
    assert!(passes_after_force > passes_after, "straddled edit was not caught up");
    assert!(
        forced_frontier >= edit_fact,
        "candidate frontier missed the edit delivered during lowering"
    );
    let forced_submissions = embedding.snapshot().submissions;
    let forced_requests = embedding.snapshot().requests - before_force_embedding.requests;
    assert_eq!(
        forced_submissions - stream_submissions,
        1,
        "the edit straddling the full projection submits only its changed BSL payload"
    );

    // A Configuration.xml Version/Comment-only edit is semantically ignored by the graph.
    let config_path = root.join("src/cf/Configuration.xml");
    let config_xml = std::fs::read_to_string(&config_path).unwrap();
    let before_noop_hash = issue441_graph_projection_hash(&main_cache.graph_db_path());
    let before_noop_status = issue441_graph_status(&client).await;
    let before_noop_revision = before_noop_status["revision"].as_u64().unwrap();
    let before_noop_sqlite_revision = issue441_graph_revision(&main_cache.graph_db_path());
    assert_eq!(before_noop_sqlite_revision, before_noop_revision);
    assert_eq!(
        issue441_configuration_byte_fingerprint(&main_cache.graph_db_path()),
        Some(*blake3::hash(config_xml.as_bytes()).as_bytes()),
        "pre-no-op graph byte fingerprint is not settled"
    );
    let before_noop_full = graph.full_builds_started.load(Ordering::SeqCst);
    let before_noop_submissions = embedding.snapshot().submissions;
    let before_noop_fact = hub.seq();
    let semantic_noop = config_xml
        .replace("<Version>1</Version>", "<Version>2</Version>")
        .replace("Issue441 initial", "Version-only semantic no-op");
    assert_ne!(semantic_noop, config_xml, "the XML no-op fixture must change file bytes");
    let expected_noop_fingerprint = *blake3::hash(semantic_noop.as_bytes()).as_bytes();
    std::fs::write(&config_path, semantic_noop).unwrap();
    let noop_fact = crate::graph::test_support::wait_for_hub_seq_above(&hub, before_noop_fact);
    let noop_settled = tokio::time::timeout(
        Duration::from_secs(60),
        issue441_wait_graph_ready(&client, Some(before_noop_revision)),
    )
    .await;
    let noop_graph = match noop_settled {
        Ok(status) => status,
        Err(_) => {
            let graph_status = issue441_graph_status(&client).await;
            let sqlite_revision = issue441_graph_revision(&main_cache.graph_db_path());
            panic!(
                "Version/Comment XML no-op did not settle within 60 seconds; hub_fact={noop_fact}; before_graph={before_noop_status}; after_graph={graph_status}; before_sqlite_revision={before_noop_sqlite_revision}; after_sqlite_revision={sqlite_revision}"
            );
        }
    };
    let noop_revision = noop_graph["revision"].as_u64().unwrap();
    let noop_sqlite_revision = issue441_graph_revision(&main_cache.graph_db_path());
    assert!(noop_revision >= before_noop_revision, "MCP graph revision moved backwards");
    assert_eq!(noop_sqlite_revision, noop_revision, "MCP and SQLite revisions disagree");
    assert_eq!(issue441_graph_projection_hash(&main_cache.graph_db_path()), before_noop_hash);
    assert_eq!(graph.full_builds_started.load(Ordering::SeqCst), before_noop_full);
    assert_eq!(embedding.snapshot().submissions, before_noop_submissions);
    assert_eq!(
        issue441_configuration_byte_fingerprint(&main_cache.graph_db_path()),
        Some(expected_noop_fingerprint),
        "published graph has not settled the Configuration.xml byte fingerprint"
    );
    let noop_full_builds_unchanged =
        graph.full_builds_started.load(Ordering::SeqCst) == before_noop_full;
    let noop_provider_submission_delta = embedding.snapshot().submissions - before_noop_submissions;

    // This local metadata change is the true control: the graph projection must change and
    // publish a new revision. It is not used to claim that every XML topology change is patchable.
    let control_path = root.join("src/cf/Catalogs/Issue441Control.xml");
    let changed_xml = std::fs::read_to_string(&control_path)
        .unwrap()
        .replace("Issue441AttributeA", "Issue441AttributeB");
    let before_graph_only_submissions = embedding.snapshot().submissions;
    std::fs::write(&control_path, changed_xml).unwrap();
    let true_control_graph = issue441_wait_graph_ready(&client, Some(noop_revision)).await;
    issue441_wait_search_ready(&client).await;
    let graph_only_submissions = embedding.snapshot().submissions;
    assert_eq!(
        graph_only_submissions, before_graph_only_submissions,
        "an XML-only graph revision with unchanged BSL context must submit no embeddings"
    );
    let main_projection = issue441_graph_projection_hash(&main_cache.graph_db_path());
    assert_ne!(
        main_projection, before_noop_hash,
        "true metadata control did not change SQLite graph rows"
    );

    // Reopening the unchanged local cache must reuse the vector paid for by the point edit.
    // This also establishes the current base snapshot before the partial-vector control below.
    let full_after_xml_control = graph.full_builds_started.load(Ordering::SeqCst);
    let before_warm_reopen_projection = issue441_graph_projection_hash(&main_cache.graph_db_path());
    let before_warm_reopen = embedding.snapshot();
    let warm_stop_started = Instant::now();
    issue441_stop_mcp(server, client).await;
    drop(state);
    let after_warm_stop = embedding.snapshot();
    let warm_stop_elapsed = warm_stop_started.elapsed();
    let warm_stop_submissions = after_warm_stop.submissions - before_warm_reopen.submissions;
    let warm_stop_unique_payloads =
        after_warm_stop.payload_hashes.difference(&before_warm_reopen.payload_hashes).count();
    eprintln!(
        "ISSUE441 scale warm stop: elapsed={warm_stop_elapsed:?}, submissions={warm_stop_submissions}, new_unique_payloads={warm_stop_unique_payloads}"
    );
    assert_eq!(warm_stop_submissions, 0, "stopping after the XML control sent embeddings");
    assert_eq!(warm_stop_unique_payloads, 0, "stopping after the XML control added payloads");
    let warm_reopen_started = Instant::now();
    let (warm_reopen_state, warm_reopen_server, warm_reopen_client) =
        issue441_start_mcp(&root, main_cache.clone()).await;
    tokio::time::timeout(Duration::from_secs(60), async {
        issue441_wait_graph_ready(&warm_reopen_client, None).await;
        issue441_wait_search_ready(&warm_reopen_client).await;
    })
    .await
    .expect("unchanged same-cache graph/search reopen did not become ready within 60 seconds");
    let warm_reopen_snapshot = embedding.snapshot();
    let warm_reopen_phase_submissions =
        warm_reopen_snapshot.submissions - after_warm_stop.submissions;
    let warm_reopen_phase_requests = warm_reopen_snapshot.requests - after_warm_stop.requests;
    let warm_reopen_unique_payloads =
        warm_reopen_snapshot.payload_hashes.difference(&after_warm_stop.payload_hashes).count();
    let warm_combined_submissions =
        warm_reopen_snapshot.submissions - before_warm_reopen.submissions;
    let warm_combined_unique_payloads =
        warm_reopen_snapshot.payload_hashes.difference(&before_warm_reopen.payload_hashes).count();
    // Keep the original receipt fields as the combined stop+reopen window.
    let warm_reopen_submissions = warm_combined_submissions;
    let warm_reopen_requests = warm_reopen_snapshot.requests - before_warm_reopen.requests;
    eprintln!(
        "ISSUE441 scale warm reopen: elapsed={:?}, stop_submissions={warm_stop_submissions}, reopen_submissions={warm_reopen_phase_submissions}, combined_submissions={warm_combined_submissions}, stop_unique_payloads={warm_stop_unique_payloads}, reopen_unique_payloads={warm_reopen_unique_payloads}, combined_unique_payloads={warm_combined_unique_payloads}",
        warm_reopen_started.elapsed()
    );
    assert_eq!(warm_reopen_phase_submissions, 0, "same-cache warm reopen sent embedding inputs");
    assert_eq!(warm_reopen_unique_payloads, 0, "same-cache warm reopen added payloads");
    assert_eq!(warm_combined_submissions, 0, "warm stop and reopen sent embedding inputs");
    assert_eq!(warm_combined_unique_payloads, 0, "warm stop and reopen added payloads");
    assert_eq!(warm_reopen_state.graph().full_builds_started.load(Ordering::SeqCst), 0);
    assert_eq!(issue441_search_vector_count(&warm_reopen_state), ISSUE441_SCALE_CHUNKS);
    assert_eq!(
        issue441_graph_projection_hash(&main_cache.graph_db_path()),
        before_warm_reopen_projection,
        "unchanged same-cache reopen changed the graph projection"
    );
    let warm_reopen_elapsed = warm_reopen_started.elapsed();

    // A partial restart keeps every compatible vector and embeds only three intentionally
    // cleared rows. The SQL trigger advances the vector generation, so startup must not accept
    // the old sidecar as current. Its baseline hash is captured from the just-reopened cache,
    // after the persisted point-edit vector has been carried into the main SQLite rows.
    let partial_started = Instant::now();
    let before_partial_graph = issue441_graph_projection_hash(&main_cache.graph_db_path());
    let (before_partial_rows, before_partial_vector_hash) =
        issue441_search_vector_hash(&main_cache.search_db_path());
    assert_eq!(before_partial_rows, ISSUE441_SCALE_CHUNKS);
    issue441_stop_mcp(warm_reopen_server, warm_reopen_client).await;
    drop(warm_reopen_state);
    let search_db = main_cache.search_db_path();
    let sidecar = issue441_append_suffix(&search_db, ".usearch.json");
    assert!(sidecar.exists(), "the first search pass persists its USEARCH sidecar");
    let missing_vector_ids = issue441_null_some_vectors(&search_db, ISSUE441_SCALE_PARTIAL_ROWS);
    let (mutated_rows, partial_vector_hash) = issue441_search_vector_hash(&search_db);
    assert_eq!(mutated_rows, before_partial_rows);
    assert_ne!(partial_vector_hash, before_partial_vector_hash);
    let before_partial_restart = embedding.snapshot().submissions;
    let before_partial_restart_requests = embedding.snapshot().requests;
    let (partial_state, partial_server, partial_client) =
        issue441_start_mcp(&root, main_cache.clone()).await;
    let partial_graph = issue441_wait_graph_ready(&partial_client, None).await;
    let (partial_search_status, partial_qualification) = issue441_wait_raw_semantic_work(
        &partial_client,
        &partial_state,
        ISSUE441_SCALE_CHUNKS,
        true,
        Duration::from_secs(3600),
    )
    .await;
    eprintln!(
        "ISSUE441 scale partial qualification: status={}, qualification={partial_qualification:?}",
        issue441_target(&partial_search_status, "semantic")
    );
    let partial_restart_submissions = embedding.snapshot().submissions - before_partial_restart;
    let partial_restart_requests = embedding.snapshot().requests - before_partial_restart_requests;
    assert_eq!(partial_restart_submissions, ISSUE441_SCALE_PARTIAL_ROWS);
    assert_eq!(partial_graph["revision"], true_control_graph["revision"]);
    assert_eq!(partial_state.graph().full_builds_started.load(Ordering::SeqCst), 0);
    assert_eq!(issue441_search_vector_count(&partial_state), ISSUE441_SCALE_CHUNKS);
    let (restored_rows, restored_vector_hash) = issue441_search_vector_hash(&search_db);
    assert_eq!(restored_rows, ISSUE441_SCALE_CHUNKS);
    assert_eq!(
        restored_vector_hash, before_partial_vector_hash,
        "partial restart must retain the exact SQLite vector bytes and ids"
    );
    assert_eq!(issue441_graph_projection_hash(&main_cache.graph_db_path()), before_partial_graph);
    let partial_elapsed = partial_started.elapsed();

    // A separate reopen after deleting only the USEARCH sidecar rebuilds that derived index
    // from complete SQLite vectors and sends no embedding POST.
    issue441_stop_mcp(partial_server, partial_client).await;
    drop(partial_state);
    std::fs::remove_file(&sidecar).unwrap();
    let before_sidecar_reopen = embedding.snapshot().submissions;
    let before_sidecar_reopen_requests = embedding.snapshot().requests;
    let sidecar_started = Instant::now();
    let (reopened_state, reopened_server, reopened_client) =
        issue441_start_mcp(&root, main_cache.clone()).await;
    let sidecar_graph = issue441_wait_graph_ready(&reopened_client, None).await;
    let (sidecar_search_status, sidecar_qualification) = issue441_wait_raw_semantic_work(
        &reopened_client,
        &reopened_state,
        ISSUE441_SCALE_CHUNKS,
        false,
        Duration::from_secs(3600),
    )
    .await;
    eprintln!(
        "ISSUE441 scale sidecar qualification: status={}, qualification={sidecar_qualification:?}",
        issue441_target(&sidecar_search_status, "semantic")
    );
    let sidecar_submissions = embedding.snapshot().submissions;
    let sidecar_requests = embedding.snapshot().requests - before_sidecar_reopen_requests;
    assert_eq!(sidecar_submissions, before_sidecar_reopen, "sidecar-only rebuild sent embeddings");
    assert_eq!(sidecar_graph["revision"], true_control_graph["revision"]);
    assert_eq!(issue441_search_vector_count(&reopened_state), ISSUE441_SCALE_CHUNKS);
    assert_eq!(issue441_graph_projection_hash(&main_cache.graph_db_path()), main_projection);
    let sidecar_query_hits =
        issue441_search_hits_match_sqlite(&reopened_state, &main_cache.search_db_path());
    assert_eq!(embedding.snapshot().submissions, before_sidecar_reopen);
    let sidecar_elapsed = sidecar_started.elapsed();

    // A second, independent cold cache is the reference. Its complete graph projection and the
    // ordered SQLite vector bytes must match the incrementally maintained primary cache.
    issue441_stop_mcp(reopened_server, reopened_client).await;
    drop(reopened_state);
    let cold_cache =
        crate::cache::WorkspaceCacheLayout::from_root(run.join("cache-cold-reference"))
            .with_workspace(root.clone());
    cold_cache.ensure().unwrap();
    let before_cold_reference = embedding.snapshot();
    let cold_started = Instant::now();
    let (cold_state, cold_server, cold_client) =
        issue441_start_mcp(&root, cold_cache.clone()).await;
    issue441_wait_graph_ready(&cold_client, None).await;
    issue441_wait_search_ready(&cold_client).await;
    let cold_projection = issue441_graph_projection_hash(&cold_cache.graph_db_path());
    assert_eq!(
        cold_projection, main_projection,
        "cold graph rows differ from the incrementally updated publication"
    );
    let (main_vectors, main_vector_hash) =
        issue441_semantic_vector_hash(&main_cache.search_db_path());
    let (cold_vectors, cold_vector_hash) =
        issue441_semantic_vector_hash(&cold_cache.search_db_path());
    assert_eq!(main_vectors, ISSUE441_SCALE_CHUNKS);
    assert_eq!(cold_vectors, main_vectors);
    assert_eq!(
        cold_vector_hash, main_vector_hash,
        "cold and maintained semantic-key-to-vector-byte sets differ"
    );
    assert_eq!(issue441_search_vector_count(&cold_state), ISSUE441_SCALE_CHUNKS);
    let cold_reference_snapshot = embedding.snapshot();
    let cold_reference_submissions =
        cold_reference_snapshot.submissions - before_cold_reference.submissions;
    let cold_reference_requests = cold_reference_snapshot.requests - before_cold_reference.requests;
    assert_eq!(cold_reference_submissions, ISSUE441_SCALE_CHUNKS);
    issue441_stop_mcp(cold_server, cold_client).await;

    let mut receipt = serde_json::json!({
        "test": "graph::build::vector_lifecycle_tests::issue441_scale_workload",
        "status": "passed",
        "source_sha": repository_sha,
        "binary_sha256_blake3": binary_sha,
        "workspace": root,
        "workspace_device": issue441_file_device(&root),
        "corpus_hash": generated,
        "chunks": ISSUE441_SCALE_CHUNKS,
        "source_roots": { "base": 1, "cfe": 5, "valid_epf": 12, "invalid_auto_export": 4 },
        "embedding": {
            "endpoint": "127.0.0.1 loopback OpenAI-compatible test server",
            "model": "issue441-deterministic",
            "dimension": ISSUE441_SCALE_DIMENSION,
            "cold_initial_submissions": cold_submissions,
            "cold_initial_requests": cold_requests,
            "single_edit_submissions": single_submissions - cold_submissions,
            "single_edit_requests": single_requests,
            "body_stream_submissions": stream_submission_delta,
            "body_stream_requests": stream_requests,
            "forced_straddle_submissions": forced_submissions - stream_submissions,
            "forced_straddle_requests": forced_requests,
            "warm_reopen_submissions": warm_reopen_submissions,
            "warm_reopen_requests": warm_reopen_requests,
            "warm_stop_submissions": warm_stop_submissions,
            "warm_reopen_phase_submissions": warm_reopen_phase_submissions,
            "warm_reopen_phase_requests": warm_reopen_phase_requests,
            "warm_stop_unique_payloads": warm_stop_unique_payloads,
            "warm_reopen_unique_payloads": warm_reopen_unique_payloads,
            "partial_restart_submissions": partial_restart_submissions,
            "partial_restart_requests": partial_restart_requests,
            "partial_missing_vector_ids": missing_vector_ids,
            "sidecar_only_submissions": sidecar_submissions - before_sidecar_reopen,
            "sidecar_only_requests": sidecar_requests,
            "cold_reference_submissions": cold_reference_submissions,
            "cold_reference_requests": cold_reference_requests,
            "total_requests": embedding.snapshot().requests,
            "max_request_body_bytes": embedding.snapshot().max_request_body_bytes,
            "max_inputs_per_request": embedding.snapshot().max_inputs_per_request,
            "unique_payload_hashes": embedding.snapshot().payload_hashes.len(),
        },
        "full_builds": {
            "initial": initial_full,
            "after_body_stream": initial_full,
            "after_forced_straddle": full_after_force,
            "after_xml_control": full_after_xml_control,
        },
        "catchup": { "passes_after_stream": passes_after, "passes_after_force": passes_after_force, "resumes": resumes_after_force, "frontier": forced_frontier },
        "publication_barriers": {
            "private_sql": {
                "held_ms": private_barrier_elapsed.as_millis(),
                "base_revision": private_base_revision,
                "reader_revision_during": private_during["revision"],
                "reader_revision_after_hold": private_after_hold["revision"],
                "reader_result_matches_published_snapshot": private_after_hold["result"] == private_base["result"],
                "delivered_body_event_fact_while_held": private_barrier_edit_fact,
                "automatic_resume_count_before": private_resume_before,
                "automatic_resume_count_after": private_resumes_after,
                "automatic_resume_frontier": private_frontier,
                "automatic_resume_revision": private_revision,
                "debt_alarm_wake_count_before": private_alarm_before_release,
                "debt_alarm_wake_count_after": private_alarm_after_resume,
                "automatic_resume_elapsed_ms": private_continuation_elapsed.as_millis(),
                "continued_without_mcp_request": true,
            },
            "forced_full_post_lowering": {
                "held_ms": forced_barrier_elapsed.as_millis(),
                "base_revision": before_force,
                "reader_revision_during": forced_during["revision"],
                "reader_result_matches_published_snapshot": forced_during["result"] == before_force_overview["result"],
            },
        },
        "revisions": {
            "initial": initial_graph["revision"],
            "single_edit": single_graph["revision"],
            "body_stream": stream_graph["revision"],
            "forced_straddle": straddled_graph["revision"],
            "xml_noop_before": before_noop_revision,
            "xml_noop_published": noop_revision,
            "xml_noop_sqlite": noop_sqlite_revision,
            "xml_control": true_control_graph["revision"],
        },
        "xml_noop": {
            "delivered_hub_fact": noop_fact,
            "published_byte_fingerprint_matches_disk": true,
            "projection_unchanged": true,
            "full_builds_unchanged": noop_full_builds_unchanged,
            "provider_submission_delta": noop_provider_submission_delta,
        },
        "parity": {
            "main_graph_projection": main_projection,
            "cold_graph_projection": cold_projection,
            "vector_hash_key": "(root_id,path,shared_semantic_key) -> exact SQLite embedding bytes; rowid excluded",
            "main_vector_rows": main_vectors,
            "main_vector_sha256_blake3": main_vector_hash,
            "cold_vector_sha256_blake3": cold_vector_hash,
            "partial_restart_restored_vector_sha256_blake3": restored_vector_hash,
            "sidecar_query_hit_ids_and_scores": sidecar_query_hits,
        },
        "elapsed_ms": {
            "workspace_generation_cache_and_hash": generation_elapsed.as_millis(),
            "initial_cold": initial_elapsed.as_millis(),
            "warm_unchanged": warm_elapsed.as_millis(),
            "single_edit": single_edit_elapsed.as_millis(),
            "body_edit_window": stream_elapsed.as_millis(),
            "body_stream_quiet_convergence": stream_quiet_convergence.as_millis(),
            "warm_reopen": warm_reopen_elapsed.as_millis(),
            "private_candidate_barrier": private_barrier_elapsed.as_millis(),
            "private_barrier_mcp_read_during": private_during_read_elapsed.as_millis(),
            "private_barrier_mcp_read_after_hold": private_after_hold_read_elapsed.as_millis(),
            "private_candidate_automatic_resume": private_continuation_elapsed.as_millis(),
            "forced_full_with_barrier": forced_full_elapsed.as_millis(),
            "forced_full_barrier": forced_barrier_elapsed.as_millis(),
            "forced_candidate_catchup_after_release": forced_catchup_elapsed.as_millis(),
            "partial_restart": partial_elapsed.as_millis(),
            "sidecar_rebuild": sidecar_elapsed.as_millis(),
            "cold_reference": cold_started.elapsed().as_millis(),
            "total": generated_at.elapsed().as_millis(),
        },
    });
    receipt["raw_profile_semantic_completion"] = serde_json::json!({
        "partial_status": issue441_target(&partial_search_status, "semantic").clone(),
        "partial_qualification": {
            "identity_verified": partial_qualification.identity_verified,
            "coverage_complete": partial_qualification.coverage_complete,
            "pending_work": partial_qualification.pending_work,
            "context_pending": partial_qualification.context_pending,
        },
        "sidecar_status": issue441_target(&sidecar_search_status, "semantic").clone(),
        "sidecar_qualification": {
            "identity_verified": sidecar_qualification.identity_verified,
            "coverage_complete": sidecar_qualification.coverage_complete,
            "pending_work": sidecar_qualification.pending_work,
            "context_pending": sidecar_qualification.context_pending,
        },
    });
    let receipt_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join(".artifacts/issue441/receipts");
    std::fs::create_dir_all(&receipt_dir).unwrap();
    let receipt_path = receipt_dir.join("issue441-scale-workload.json");
    std::fs::write(&receipt_path, serde_json::to_vec_pretty(&receipt).unwrap()).unwrap();
    assert!(delta.exists(), "the generated/edited body remains available for inspection");
}

fn capture(f: impl FnOnce()) -> Vec<Value> {
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
                        self.0.push(serde_json::from_str(text).unwrap());
                    }
                }
            }
            if event.metadata().target() == bsl_search::lifecycle::TARGET {
                event.record(&mut Visitor(&mut self.0.lock().unwrap()));
            }
        }
    }
    let records = Arc::new(Mutex::new(Vec::new()));
    test_utils::with_subscriber(tracing_subscriber::registry().with(Capture(records.clone())), f);
    // tracing-core briefly upgrades every registered dispatcher while another test thread
    // registers a callsite, so the subscriber may still be shared here; take the records.
    let taken = std::mem::take(&mut *records.lock().unwrap());
    taken
}

#[test]
fn vector_lifecycle_capture_survives_first_callsite_on_unsubscribed_thread() {
    const CHILD: &str = "BSL_MCP_LIFECYCLE_CAPTURE_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "graph::build::vector_lifecycle_tests::vector_lifecycle_capture_survives_first_callsite_on_unsubscribed_thread"])
            .env(CHILD, "1")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let output = crate::graph::test_support::child_output(
            child,
            test_utils::wait::hang_guard(std::time::Duration::from_secs(60)),
            "lifecycle capture child",
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        // A drifted name would select no test and still exit successfully.
        assert!(output.status.success() && stdout.contains("1 passed"), "{stdout}");
        return;
    }
    let probe = || {
        bsl_search::lifecycle::Record::new(
            Path::new("fixture.db"),
            "capture_probe",
            bsl_search::lifecycle::Reason::Startup,
        )
        .emit(true);
    };
    let records = capture(|| {
        crate::test_wait::join(std::thread::spawn(probe), "capture probe exits").unwrap();
        probe();
    });
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["kind"], "capture_probe");
}

fn emit(engine: &mut SearchEngine, source: &Path, rows: &[ide::ChunkRow]) {
    let mut writer = FusedChunkWriter::new(
        engine,
        source.to_owned(),
        crate::workspace_lease::WorkspaceLease::unmanaged(),
    );
    writer.emit_chunks(rows).unwrap();
    writer.finish(bsl_search::lifecycle::Outcome::Completed);
}

fn row(path: &Path, symbol: &str) -> ide::ChunkRow {
    ide::ChunkRow {
        path: path.to_string_lossy().replace('\\', "/"),
        symbol: symbol.into(),
        kind: bsl_search::ChunkKind::Procedure,
        is_export: false,
        annotations: Vec::new(),
        line_start: 1,
        line_end: 2,
        text: "canary source text must not enter lifecycle records".into(),
        graph_context: None,
    }
}

fn seed_with_roots(source: &Path, install_roots: bool) -> (SearchEngine, Vec<ide::ChunkRow>) {
    let file = source.join("Module.bsl");
    std::fs::write(&file, "Процедура Делать()\nКонецПроцедуры").unwrap();
    let file = file.canonicalize().unwrap();
    let rows = ["A", "B"].into_iter().map(|name| row(&file, name)).collect::<Vec<_>>();
    let mut engine = SearchEngine::fts_only(&source.join("search.db")).unwrap();
    if install_roots {
        let (roots, rejected) = bsl_search::WorkspaceRoots::build(source, source, &[]);
        assert!(rejected.is_empty());
        engine.set_workspace_roots(roots);
        assert!(engine.workspace_roots().is_some(), "the root-table branch must be exercised");
    }
    emit(&mut engine, source, &rows);
    let pending = engine.store().load_pending_embedding_documents("code").unwrap();
    assert_eq!(pending.len(), 2);
    engine.store().set_chunk_embedding(pending[0].0, &[0.1, 0.2]).unwrap();
    (engine, rows)
}

fn seed(source: &Path) -> (SearchEngine, Vec<ide::ChunkRow>) {
    seed_with_roots(source, false)
}

fn rootless_source(dir: &tempfile::TempDir) -> std::path::PathBuf {
    #[cfg(unix)]
    {
        let canonical = dir.path().join("canonical-cf");
        let declared = dir.path().join("declared-cf");
        std::fs::create_dir(&canonical).unwrap();
        std::os::unix::fs::symlink(&canonical, &declared).unwrap();
        assert_ne!(declared.canonicalize().unwrap(), declared);
        declared
    }
    #[cfg(not(unix))]
    {
        dir.path().to_path_buf()
    }
}

fn assert_read_error_preserves_rows_and_vectors(
    source: &Path,
    mut engine: SearchEngine,
    rows: &[ide::ChunkRow],
) {
    let before_rows = engine.store().load_indexed_documents(Some("code")).unwrap();
    let before_files = engine.store().all_files_in_collection("code").unwrap();
    let before_vectors = engine.store().load_all_embeddings(2).unwrap();
    let generation = engine.store().embedding_generation().unwrap();
    std::fs::remove_file(source.join("Module.bsl")).unwrap();

    let records = capture(|| emit(&mut engine, source, rows));
    let decision = records
        .iter()
        .find(|record| record["kind"] == "decision")
        .unwrap_or_else(|| panic!("missing decision in lifecycle records: {records:#?}"));
    assert_eq!(decision["reason"], "read_error", "lifecycle records: {records:#?}");
    assert_eq!(decision["examples"], serde_json::json!([":Module.bsl"]));
    assert!(!records.iter().any(|record| record["kind"] == "mutation"));
    assert_eq!(engine.store().load_indexed_documents(Some("code")).unwrap(), before_rows);
    assert_eq!(engine.store().all_files_in_collection("code").unwrap(), before_files);
    assert_eq!(engine.store().load_all_embeddings(2).unwrap(), before_vectors);
    assert_eq!(engine.store().embedding_generation().unwrap(), generation);
    assert_eq!(engine.store().chunk_count().unwrap(), 2);
}

fn assert_replacement(records: &[Value], reason: &str) {
    let decision = records.iter().find(|r| r["kind"] == "decision").unwrap();
    assert_eq!(decision["reason"], reason);
    let mutations = records
        .iter()
        .filter(|r| r["kind"] == "mutation" && r["outcome"] == "committed")
        .collect::<Vec<_>>();
    assert_eq!(mutations.len(), 1);
    let mutation = mutations[0];
    assert_eq!(mutation["reason"], reason);
    assert_eq!(mutation["counts"]["sqlite_vectors_removed"], 1);
    assert_eq!(mutation["count_quality"], "exact");
    assert_eq!(mutation["process_id"], decision["process_id"]);
    assert_eq!(mutation["parent_operation_id"], decision["parent_operation_id"]);
    assert!(mutation["parent_operation_id"].as_u64().is_some());
    let terminal = records
        .iter()
        .find(|r| r["kind"] == "mutation_summary" && r["outcome"] == "completed")
        .unwrap();
    assert_eq!(terminal["committed_totals"]["sqlite_vectors_removed"], 1);
    assert!(!serde_json::to_string(records).unwrap().contains("canary source"));
}

#[test]
fn vector_lifecycle_unchanged_warm_reopen_preserves_mixed_vectors() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path();
    let (engine, rows) = seed(source);
    let before = engine.store().load_all_embeddings(2).unwrap();
    let generation = engine.store().embedding_generation().unwrap();
    drop(engine);
    let records = capture(|| {
        let mut engine = SearchEngine::fts_only(&source.join("search.db")).unwrap();
        emit(&mut engine, source, &rows);
        assert_eq!(engine.store().load_all_embeddings(2).unwrap(), before);
        assert_eq!(engine.store().embedding_generation().unwrap(), generation);
        assert_eq!(engine.store().load_pending_embedding_documents("code").unwrap().len(), 1);
    });
    let snapshot = records.iter().find(|r| r["kind"] == "startup_snapshot").unwrap();
    assert_eq!(snapshot["snapshot"]["state"], "observed");
    assert_eq!(snapshot["snapshot"]["files"], 1);
    assert_eq!(snapshot["snapshot"]["chunks"], 2);
    assert_eq!(snapshot["snapshot"]["vectors"], 1);
    let decision = records.iter().find(|r| r["kind"] == "decision").unwrap();
    assert_eq!(decision["reason"], "unchanged");
    assert!(records.iter().all(|r| r["counts"]["sqlite_vectors_removed"] == 0));
}

#[test]
fn vector_lifecycle_changed_file_attributes_actual_non_null_loss() {
    let dir = tempfile::tempdir().unwrap();
    let (mut engine, rows) = seed(dir.path());
    std::fs::write(dir.path().join("Module.bsl"), "Процедура Другая()\nКонецПроцедуры").unwrap();
    let records = capture(|| emit(&mut engine, dir.path(), &rows));
    assert_replacement(&records, "hash_changed");
    assert_eq!(engine.store().load_pending_embedding_documents("code").unwrap().len(), 2);
}

#[test]
fn vector_lifecycle_hash_lookup_error_reindexes_without_claiming_new_file() {
    let dir = tempfile::tempdir().unwrap();
    let (mut engine, rows) = seed(dir.path());
    let connection = rusqlite::Connection::open(engine.store().db_path()).unwrap();
    connection.execute("UPDATE files SET hash = 17 WHERE path = 'Module.bsl'", []).unwrap();
    assert!(engine.store().file_hash("", "Module.bsl").is_err());
    let records = capture(|| emit(&mut engine, dir.path(), &rows));
    assert_replacement(&records, "hash_lookup_error");
    assert_eq!(engine.store().load_pending_embedding_documents("code").unwrap().len(), 2);
    assert!(engine.store().file_hash("", "Module.bsl").unwrap().is_some());
}

#[test]
fn vector_lifecycle_cleared_hash_and_missing_record_keep_distinct_causes() {
    let dir = tempfile::tempdir().unwrap();
    let (mut engine, rows) = seed(dir.path());
    engine.store().clear_file_hashes("code").unwrap();
    let records = capture(|| emit(&mut engine, dir.path(), &rows));
    assert_replacement(&records, "hash_cleared");
    // No historical initiating actor is inferred from an empty stored hash.
    assert!(!records.iter().any(|r| r["reason"] == "hash_changed"));

    engine.store().remove_file("", "Module.bsl", "code").unwrap();
    let records = capture(|| emit(&mut engine, dir.path(), &rows));
    let decision = records.iter().find(|r| r["kind"] == "decision").unwrap();
    assert_eq!(decision["reason"], "missing_record");
    let mutation =
        records.iter().find(|r| r["kind"] == "mutation" && r["outcome"] == "committed").unwrap();
    assert_eq!(mutation["reason"], "missing_record");
    assert_eq!(mutation["counts"]["sqlite_vectors_removed"], 0);
    assert_eq!(decision["parent_operation_id"], mutation["parent_operation_id"]);
    assert_eq!(engine.store().load_pending_embedding_documents("code").unwrap().len(), 2);
}

#[test]
fn vector_lifecycle_read_error_preserves_rows_and_vectors() {
    let dir = tempfile::tempdir().unwrap();
    let source = rootless_source(&dir);
    let (engine, rows) = seed(&source);
    assert_read_error_preserves_rows_and_vectors(&source, engine, &rows);
}

#[test]
fn vector_lifecycle_read_error_with_root_table_preserves_rows_and_vectors() {
    let dir = tempfile::tempdir().unwrap();
    let (engine, rows) = seed_with_roots(dir.path(), true);
    assert_read_error_preserves_rows_and_vectors(dir.path(), engine, &rows);
}

#[cfg(unix)]
#[test]
fn vector_lifecycle_read_error_through_symlinked_root_with_root_table_preserves_rows_and_vectors() {
    let dir = tempfile::tempdir().unwrap();
    let source = rootless_source(&dir);
    let (engine, rows) = seed_with_roots(&source, true);
    assert_read_error_preserves_rows_and_vectors(&source, engine, &rows);
}

#[test]
fn vector_lifecycle_missing_prefix_neighbor_stays_outside_with_or_without_root_table() {
    for install_roots in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("cf");
        let outsider_dir = dir.path().join("cf_ext");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&outsider_dir).unwrap();
        let (mut engine, rows) = seed_with_roots(&source, install_roots);
        let before_rows = engine.store().load_indexed_documents(Some("code")).unwrap();
        let outsider = row(&outsider_dir.join("Module.bsl"), "Outsider");

        let records = capture(|| emit(&mut engine, &source, &[rows[0].clone(), outsider]));
        let decisions =
            records.iter().filter(|record| record["kind"] == "decision").collect::<Vec<_>>();
        assert_eq!(decisions.len(), 1, "lifecycle records: {records:#?}");
        assert_eq!(decisions[0]["reason"], "unchanged");
        assert_eq!(decisions[0]["examples"], serde_json::json!([":Module.bsl"]));
        assert_eq!(engine.store().load_indexed_documents(Some("code")).unwrap(), before_rows);
        assert_eq!(engine.store().chunk_count().unwrap(), 2);
    }
}

#[test]
fn vector_lifecycle_fused_writer_retains_totals_across_emit_batches_and_takeover() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path();
    let (mut engine, rows) = seed(source);
    let cache = crate::cache::WorkspaceCacheLayout::for_workspace(source);
    cache.ensure().unwrap();
    let lease = crate::workspace_lease::WorkspaceLease::claim_cache(&cache);
    assert!(lease.owns_caches_now());
    let newer = Arc::new(Mutex::new(None));
    let newer_hook = newer.clone();
    super::FUSED_FILE_COMMITTED_HOOK.with(|hook| {
        hook.replace(Some(Box::new(move || {
            let claim = crate::workspace_lease::WorkspaceLease::claim_cache(&cache);
            assert!(claim.owns_caches_now());
            *newer_hook.lock().unwrap() = Some(claim);
        })))
    });
    let records = capture(|| {
        let mut writer = FusedChunkWriter::new(&mut engine, source.to_owned(), lease.clone());
        std::fs::write(source.join("Module.bsl"), "first change").unwrap();
        writer.emit_chunks(&rows).unwrap();
        std::fs::write(source.join("Module.bsl"), "second change").unwrap();
        assert!(writer.emit_chunks(&rows).is_err());
        let outcome = writer.failure.as_ref().unwrap().lifecycle_outcome();
        writer.finish(outcome);
    });
    assert!(lease.is_superseded());
    newer.lock().unwrap().take().unwrap().release();
    let summaries: Vec<_> = records.iter().filter(|r| r["kind"] == "mutation_summary").collect();
    // A one-second timer decides whether a flush happens, so the count of progress summaries
    // varies; each one opens a fresh interval with its own started intent.
    let started = summaries.iter().filter(|r| r["outcome"] == "started").count();
    let progress = summaries.iter().filter(|r| r["outcome"] == "progress").count();
    assert_eq!(started, 1 + progress, "lifecycle summaries: {summaries:#?}");
    let terminal = summaries.last().unwrap();
    assert_eq!(terminal["outcome"], "interrupted");
    assert_eq!(terminal["committed_totals"]["sqlite_vectors_removed"], 1);
    let decisions: Vec<_> = records.iter().filter(|r| r["kind"] == "decision").collect();
    assert_eq!(decisions.len(), 2);
    assert_eq!(decisions[0]["parent_operation_id"], decisions[1]["parent_operation_id"]);
    assert_eq!(engine.store().chunk_count().unwrap(), 2);
}
