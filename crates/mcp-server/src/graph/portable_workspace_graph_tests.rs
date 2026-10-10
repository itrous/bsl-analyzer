use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::test_support::{
    holding_the_first_build, meta_string, sample_workspace, seed_cache, seed_cache_with_layout,
    wait_ready, wait_until,
};
use crate::cache::WorkspaceCacheLayout;

const CLIENT_MODULE_PATH: &str = "CommonModules/Клиент/Ext/Module.bsl";
const CLIENT_SOURCE: &str =
    "&НаКлиенте\nПроцедура Главная() Экспорт\nСервер.Считать();\nКонецПроцедуры";
const SERVER_MODULE_PATH: &str = "CommonModules/Сервер/Ext/Module.bsl";
const SERVER_SOURCE: &str = "&НаСервере\nФункция Считать() Экспорт КонецФункции";
type StoredEmbeddings = Vec<(i64, Vec<f32>)>;

/// A tiny local OpenAI-compatible endpoint. The test only needs the request count: a
/// response is still returned so an accidental request fails at the assertion rather
/// than hanging the embedding client's retry loop.
struct CountingEmbeddingServer {
    addr: SocketAddr,
    calls: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl CountingEmbeddingServer {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test embedding server");
        listener.set_nonblocking(true).expect("make test embedding server non-blocking");
        let addr = listener.local_addr().expect("read test embedding server address");
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&calls);
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = Arc::clone(&stop);
        let thread = std::thread::spawn(move || {
            while !stopped.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        if stopped.load(Ordering::SeqCst) {
                            break;
                        }
                        let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
                        let mut request = Vec::new();
                        let mut chunk = [0_u8; 2048];
                        let mut header_end = None;
                        let mut content_len = 0usize;
                        while let Ok(read) = stream.read(&mut chunk) {
                            if read == 0 {
                                break;
                            }
                            request.extend_from_slice(&chunk[..read]);
                            if header_end.is_none() {
                                if let Some(end) =
                                    request.windows(4).position(|part| part == b"\r\n\r\n")
                                {
                                    header_end = Some(end + 4);
                                    let headers =
                                        String::from_utf8_lossy(&request[..end]).to_lowercase();
                                    content_len = headers
                                        .lines()
                                        .find_map(|line| line.strip_prefix("content-length:"))
                                        .and_then(|value| value.trim().parse().ok())
                                        .unwrap_or(0);
                                }
                            }
                            if header_end.is_some_and(|end| request.len() >= end + content_len) {
                                break;
                            }
                        }
                        observed.fetch_add(1, Ordering::SeqCst);
                        let inputs = header_end
                            .and_then(|end| {
                                serde_json::from_slice::<serde_json::Value>(&request[end..]).ok()
                            })
                            .and_then(|value| value.get("input")?.as_array().map(Vec::len))
                            .unwrap_or(1);
                        let data: Vec<_> = (0..inputs)
                            .map(|index| {
                                serde_json::json!({
                                    "index": index,
                                    "embedding": [1.0, 0.0, 0.0]
                                })
                            })
                            .collect();
                        let body = serde_json::json!({ "data": data }).to_string();
                        let response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        );
                        let _ = stream.write_all(response.as_bytes());
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    Err(_) => break,
                }
            }
        });
        Self { addr, calls, stop, thread: Some(thread) }
    }

    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl Drop for CountingEmbeddingServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.addr);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn copy_tree(source: &Path, destination: &Path) {
    fs::create_dir_all(destination).expect("create moved workspace");
    for entry in walkdir::WalkDir::new(source) {
        let entry = entry.expect("walk source workspace");
        let relative = entry.path().strip_prefix(source).expect("source walk prefix");
        if relative.as_os_str().is_empty() {
            continue;
        }
        let target = destination.join(relative);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&target).expect("copy workspace directory");
        } else if entry.file_type().is_file() {
            fs::create_dir_all(target.parent().expect("copied file parent"))
                .expect("create copied file parent");
            fs::copy(entry.path(), target).expect("copy workspace file");
        }
    }
}

fn set_file_modified(path: &Path, modified: SystemTime) {
    fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open file to set modification time")
        .set_modified(modified)
        .expect("set file modification time");
}

fn seed_ready_search(cache: &WorkspaceCacheLayout, workspace_root: &Path, vector: &[f32]) {
    cache.ensure().expect("create workspace cache");
    let project = super::input::ProjectSnapshot::load(workspace_root);
    let roots = project.search_roots.as_ref().expect("workspace roots for search fixture");
    let client_key = roots
        .key_of_path(&workspace_root.join(CLIENT_MODULE_PATH))
        .expect("client search file key");
    let server_key = roots
        .key_of_path(&workspace_root.join(SERVER_MODULE_PATH))
        .expect("server search file key");
    let store = crate::graph::GraphStore::serving_file_for_test(&cache.graph_db_path(), None)
        .expect("open graph for search fixture context");
    let generation =
        store.status().and_then(|status| status.generation).expect("the served generation");
    let provider =
        crate::graph_query::GraphDbContextProvider::new(store, generation, Some(roots), None);
    let chunks = bsl_search::Chunker::chunk(CLIENT_SOURCE);
    assert_eq!(chunks.len(), 1, "client fixture must have one method chunk");
    let client_contexts = chunks
        .iter()
        .map(|chunk| {
            let context = bsl_search::GraphContextProvider::graph_context(
                &provider,
                &client_key.path,
                &chunk.name,
                chunk.kind.label(),
            );
            assert!(context.is_some(), "client graph context in search fixture");
            context
        })
        .collect::<Vec<_>>();
    let server_vector = [0.0_f32, 1.0, 0.0];
    let server_chunks = bsl_search::Chunker::chunk(SERVER_SOURCE);
    assert_eq!(server_chunks.len(), 1, "server fixture must have one method chunk");
    let server_contexts = server_chunks
        .iter()
        .map(|chunk| {
            let context = bsl_search::GraphContextProvider::graph_context(
                &provider,
                &server_key.path,
                &chunk.name,
                chunk.kind.label(),
            );
            assert!(context.is_some(), "server graph context in search fixture");
            context
        })
        .collect::<Vec<_>>();
    drop(provider);
    let mut store = bsl_search::Store::open(&cache.search_db_path()).expect("open search store");
    let embeddings = [vector.to_vec()];
    let hash = bsl_search::content_blake3(CLIENT_SOURCE.as_bytes());
    store
        .reindex_file_with_context(
            &client_key.root_id,
            &client_key.path,
            &hash,
            &chunks,
            Some(&embeddings),
            Some(&client_contexts),
        )
        .expect("seed ready search vector");
    let server_hash = bsl_search::content_blake3(SERVER_SOURCE.as_bytes());
    let server_embeddings = [server_vector.to_vec()];
    store
        .reindex_file_with_context(
            &server_key.root_id,
            &server_key.path,
            &server_hash,
            &server_chunks,
            Some(&server_embeddings),
            Some(&server_contexts),
        )
        .expect("seed ready server search vector");
}

/// The replacement the publisher makes is a plain rename, which goes through once no handle is
/// open on the file. The negative control opens the file past the graph store, as the old
/// readers did: on Windows, where SQLite shares no delete access, that handle blocks the rename.
#[test]
fn replacing_graph_db_needs_every_handle_closed_first() {
    let dir = tempfile::tempdir().expect("graph replacement probe tempdir");
    let root = dir.path();
    sample_workspace(root);
    super::test_support::write(root, "Configuration.xml", "<Configuration/>");
    super::test_support::write(
        root,
        "bsl-analyzer.toml",
        "[source]\nroot = \".\"\nextensions = []\n",
    );
    let fingerprint = super::scan::workspace_fingerprint(root);
    seed_cache(root, fingerprint);

    let canonical = WorkspaceCacheLayout::for_workspace(root).graph_db_path();
    let stamped_copy = |name: &str| {
        let replacement = canonical.with_file_name(name);
        fs::copy(&canonical, &replacement).expect("copy replacement graph");
        rusqlite::Connection::open(&replacement)
            .expect("open replacement graph")
            .execute("UPDATE meta SET value = '8' WHERE key = 'revision'", [])
            .expect("stamp replacement graph");
        replacement
    };

    let bypass = crate::graph_query::GraphDb::open(&canonical).expect("open past the store");
    let blocked = fs::rename(stamped_copy("bsl-graph.db.blocked"), &canonical);
    #[cfg(windows)]
    assert!(blocked.is_err(), "an open SQLite handle keeps Windows from replacing the file");
    #[cfg(not(windows))]
    assert!(blocked.is_ok());
    assert_eq!(bypass.freshness_token().expect("read the bypassing handle").0, 7);
    drop(bypass);

    fs::rename(stamped_copy("bsl-graph.db.closed"), &canonical).unwrap_or_else(|error| {
        panic!(
            "replacing the graph with no handle open failed: kind={:?}, raw_os_error={:?}: {error}",
            error.kind(),
            error.raw_os_error()
        )
    });
    let new = crate::graph_query::GraphDb::open(&canonical).expect("open replacement graph");
    assert_eq!(new.freshness_token().expect("read replacement graph token").0, 8);
}

fn semantic_engine(cache: &WorkspaceCacheLayout, server: &str) -> bsl_search::SearchEngine {
    bsl_search::SearchEngine::new(
        &cache.search_db_path(),
        crate::state::test_support::mock_semantic_config(server),
    )
    .expect("open semantic search engine")
}

fn wait_for_search_initialization(state: &crate::state::SharedState) {
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        let engine_ready =
            state.search_engine().lock().map(|engine| engine.is_some()).unwrap_or(false);
        if !state.search_init_running() && engine_ready {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "workspace search initialization did not finish"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn search_vectors_and_hit(
    state: &crate::state::SharedState,
    vector: &[f32],
) -> (StoredEmbeddings, Vec<Option<String>>) {
    let guard = state.search_engine().lock().expect("lock workspace search engine");
    let engine = guard.as_ref().expect("workspace search engine is published");
    let (_, vectors) = engine
        .store()
        .load_all_embeddings_with_generation(3)
        .expect("read workspace search vectors");
    let documents =
        engine.store().load_indexed_documents(Some("code")).expect("read indexed graph contexts");
    assert_eq!(documents.len(), 2);
    assert!(documents.iter().all(|document| document.graph_context.is_some()));
    assert_eq!(engine.vector_count(), 2);
    let hits =
        engine.search_with_embedding(vector, 1, Some("code")).expect("search workspace vector");
    assert_eq!(hits.len(), 1, "search workspace vector must return the seeded hit");
    assert_eq!(hits[0].symbol_name, "Главная");
    let contexts = documents.into_iter().map(|document| document.graph_context).collect();
    (vectors, contexts)
}

#[test]
fn moved_cached_graph_reuses_revision_reads_new_sources_and_preserves_search_vectors() {
    let _env_lock = crate::state::test_support::env_lock();
    let old_dir = tempfile::tempdir().expect("old workspace tempdir");
    let old_root = old_dir.path().join("old-workspace");
    sample_workspace(&old_root);
    super::test_support::write(&old_root, "Configuration.xml", "<Configuration/>");
    super::test_support::write(
        &old_root,
        "bsl-analyzer.toml",
        "[source]\nroot = \".\"\nextensions = []\n",
    );
    let old_project = super::input::ProjectSnapshot::load(&old_root);
    let fingerprint = super::scan::workspace_fingerprint(&old_root);
    let old_cache = WorkspaceCacheLayout::for_workspace(&old_root);
    let vector = vec![1.0_f32, 0.0, 0.0];
    seed_cache(&old_root, fingerprint);
    seed_ready_search(&old_cache, &old_root, &vector);

    let new_dir = tempfile::tempdir().expect("new workspace tempdir");
    let new_root = new_dir.path().join("new-workspace");
    copy_tree(&old_root, &new_root);
    super::test_support::write(
        &old_root,
        CLIENT_MODULE_PATH,
        "&НаКлиенте\nПроцедура Главная() Экспорт\nСТАРЫЙ();\nКонецПроцедуры",
    );

    let cache = WorkspaceCacheLayout::for_workspace(&new_root);
    let server = CountingEmbeddingServer::start();
    let server_url = server.url();
    let _test_embedding = crate::state::test_support::EnvVarGuard::set("BSL_TEST_EMBEDDING", "1");
    let _embedding_url = crate::state::test_support::EnvVarGuard::set("EMBEDDING_URL", &server_url);
    let _embedding_model =
        crate::state::test_support::EnvVarGuard::set("EMBEDDING_MODEL", "test-model");
    let _embedding_dim = crate::state::test_support::EnvVarGuard::set("EMBEDDING_DIM", "3");

    let graph_path = cache.graph_db_path();
    let mtime_before = fs::metadata(&graph_path)
        .expect("moved graph database metadata")
        .modified()
        .expect("moved graph database modification time");
    let state = crate::state::SharedState::workspace_with_cache(new_root.clone(), cache.clone())
        .expect("initialize workspace state over moved cache");
    wait_for_search_initialization(&state);
    wait_ready(state.graph());
    state.graph().flush_hook_obligations();
    assert_eq!(
        state.graph().full_builds_started.load(Ordering::SeqCst),
        0,
        "a moved cache is reused"
    );
    let (vectors_before, contexts_before) = search_vectors_and_hit(&state, &vector);
    assert_eq!(server.calls(), 0, "the real publish hook must not call the model");

    {
        let snapshot = state.graph().snapshot().expect("moved graph snapshot");
        assert_eq!(snapshot.generation, 7, "the moved graph keeps its revision");
        let roots = snapshot.workspace_roots().expect("roots paired with moved graph");
        let node = snapshot
            .graph
            .node("method/common/Клиент/Главная", ide::GraphDetail::Bodies, Some(roots))
            .expect("read moved graph node")
            .expect("moved graph method");
        let body = node.node.source.as_deref().expect("method body from new tree");
        assert!(body.contains("Сервер.Считать"), "body must come from the new tree");
        assert!(!body.contains("СТАРЫЙ"), "the old source location must not be consulted");
        assert!(
            node.node
                .signature
                .as_deref()
                .is_some_and(|signature| signature.contains("Процедура Главная")),
            "signature must be read from the moved source"
        );

        let context = snapshot
            .graph
            .graph_context("method/common/Клиент/Главная", Some(roots))
            .expect("read moved graph context")
            .expect("method graph context");
        assert!(context.contains("Signature: Процедура Главная"));
        assert!(context.contains("Calls: Считать"));

        let neighbours = snapshot
            .graph
            .neighbors(
                &ide::NeighborsParams {
                    id: "method/common/Клиент/Главная",
                    dir: ide::Direction::Out,
                    depth: 1,
                    max_nodes: 20,
                    detail: ide::GraphDetail::Signatures,
                    provenance_filter: Vec::new(),
                    edge_kind_filter: Vec::new(),
                    call_sites: true,
                    max_call_sites: 20,
                },
                Some(roots),
            )
            .expect("read moved graph neighbours")
            .expect("moved graph neighbours result");
        let call = neighbours
            .edges
            .iter()
            .find(|edge| edge.kind == "call")
            .expect("moved graph call edge");
        assert!(
            call.call_sites.as_ref().is_some_and(|sites| !sites.is_empty()),
            "call sites must resolve through the new root"
        );
        assert!(call.call_sites_unavailable.is_none());
    }

    state.shutdown();
    drop(state);

    // Exercise the same root-transition path that the real publish hook owns, with the
    // transferred search database opened under the old physical roots first. The state boot
    // above proves the production hook completed; this direct two-phase call proves a move is
    // classified as a root retarget with no reindex or embedding work.
    let new_project = super::input::ProjectSnapshot::load(&new_root);
    let old_roots = old_project.search_roots.clone().expect("old workspace roots");
    let new_roots = new_project.search_roots.clone().expect("new workspace roots");
    let mut engine = semantic_engine(&cache, &server_url);
    engine.initialize_workspace_roots(old_roots).expect("initialize old workspace roots");
    let transition = engine
        .workspace_roots_transition_seed(new_roots)
        .expect("prepare moved search roots")
        .plan()
        .expect("plan moved search roots");
    let validated = transition
        .revalidate()
        .expect("validate moved search roots")
        .expect("moved search roots remain current");
    let outcome = engine
        .apply_validated_workspace_roots_transition(validated)
        .expect("apply moved search roots");
    assert!(matches!(
        outcome,
        bsl_search::WorkspaceRootsTransitionOutcome::Applied {
            removed: 0,
            rebuilt: 0,
            added: 0,
            pending_collection_embeddings: false,
            pending_overlay_embeddings: false,
        }
    ));
    let (_, vectors_after) = engine
        .store()
        .load_all_embeddings_with_generation(3)
        .expect("read stored vectors after move");
    let contexts_after = engine
        .store()
        .load_indexed_documents(Some("code"))
        .expect("read stored graph contexts after move")
        .into_iter()
        .map(|document| document.graph_context)
        .collect::<Vec<_>>();
    assert_eq!(vectors_after, vectors_before, "a physical move preserves stored vectors");
    assert_eq!(contexts_after, contexts_before, "a physical move preserves graph contexts");
    assert_eq!(engine.vector_count(), 2, "the live vector index survives the move");
    let hits = engine
        .search_with_embedding(&vector, 1, Some("code"))
        .expect("search moved workspace vector");
    assert_eq!(hits[0].symbol_name, "Главная");
    assert_eq!(server.calls(), 0, "cache reuse and root move must not call the model");
    assert_eq!(
        fs::metadata(&graph_path)
            .expect("moved graph database metadata after reuse")
            .modified()
            .expect("moved graph database modification time after reuse"),
        mtime_before,
        "cache reuse must not rewrite the graph"
    );
}

#[test]
fn moved_external_cache_reuses_graph_without_a_full_build() {
    let _env_lock = crate::state::test_support::env_lock();
    let old_dir = tempfile::tempdir().expect("old workspace tempdir");
    let old_root = old_dir.path().join("old-workspace");
    sample_workspace(&old_root);
    super::test_support::write(&old_root, "Configuration.xml", "<Configuration/>");
    super::test_support::write(
        &old_root,
        "bsl-analyzer.toml",
        "[source]\nroot = \".\"\nextensions = []\n",
    );
    let old_cache = WorkspaceCacheLayout::from_root(old_dir.path().join("old-external-cache"));
    old_cache.ensure().expect("prepare old external cache");
    let old_excluded = old_cache.exclusions(&old_root);
    let old_project = super::input::ProjectSnapshot::load_excluding(&old_root, &old_excluded);
    let old_universe = super::universe::ScannedUniverse::scan_excluding(
        &old_project.scan_roots,
        &old_project.excluded,
    );
    let fingerprint = super::scan::fingerprint_of_project(&old_universe.stats, &old_project)
        .expect("external-cache project has a portable fingerprint");
    seed_cache_with_layout(&old_root, &old_cache, fingerprint);

    let new_dir = tempfile::tempdir().expect("new workspace tempdir");
    let new_root = new_dir.path().join("new-workspace");
    let new_cache = WorkspaceCacheLayout::from_root(new_dir.path().join("new-external-cache"));
    new_cache.ensure().expect("prepare new external cache");
    copy_tree(&old_root, &new_root);
    copy_tree(old_cache.root(), new_cache.root());
    assert_ne!(old_cache.root(), new_cache.root(), "the external cache location moved");

    let new_excluded = new_cache.exclusions(&new_root);
    let new_project = super::input::ProjectSnapshot::load_excluding(&new_root, &new_excluded);
    assert_eq!(
        old_project.portable_topology, new_project.portable_topology,
        "external cache locations do not alter portable topology"
    );
    let graph_path = new_cache.graph_db_path();
    let database_before = fs::read(&graph_path).expect("read moved external graph database");
    let state = crate::state::SharedState::workspace_with_cache(new_root, new_cache)
        .expect("initialize workspace state over moved external cache");
    wait_ready(state.graph());
    assert_eq!(
        state.graph().full_builds_started.load(Ordering::SeqCst),
        0,
        "a moved external cache is reused"
    );
    assert_eq!(state.graph().snapshot().expect("moved external graph snapshot").generation, 7);
    assert_eq!(
        meta_string(&graph_path, "publication_id"),
        "test-1",
        "a moved database keeps the identity another owner published it with, and is served"
    );
    state.shutdown();
    drop(state);
    assert_eq!(
        fs::read(&graph_path).expect("read external graph database after reuse"),
        database_before,
        "cache reuse does not replace the graph database"
    );
}

#[test]
fn cached_graph_reuses_when_source_mtime_changes_but_bytes_do_not() {
    let dir = tempfile::tempdir().expect("workspace tempdir");
    let root = dir.path();
    sample_workspace(root);
    super::test_support::write(root, "Configuration.xml", "<Configuration/>");
    super::test_support::write(
        root,
        "bsl-analyzer.toml",
        "[source]\nroot = \".\"\nextensions = []\n",
    );

    let source = root.join(CLIENT_MODULE_PATH);
    let bytes_before = fs::read(&source).expect("read source before mtime-only change");
    let metadata_before = fs::metadata(&source).expect("source metadata before mtime-only change");
    let mtime_before = metadata_before.modified().expect("source mtime before mtime-only change");
    let fingerprint = super::scan::workspace_fingerprint(root);
    let cache = WorkspaceCacheLayout::for_workspace(root);
    seed_cache(root, fingerprint);

    let changed_mtime = UNIX_EPOCH + Duration::from_secs(1);
    set_file_modified(&source, changed_mtime);
    let metadata_after = fs::metadata(&source).expect("source metadata after mtime-only change");
    assert_ne!(
        metadata_after.modified().expect("source mtime after mtime-only change"),
        mtime_before,
        "the fixture must change the source mtime"
    );
    assert_eq!(
        fs::read(&source).expect("read source after mtime-only change"),
        bytes_before,
        "the mtime-only fixture must preserve source bytes"
    );

    let graph = crate::graph::GraphState::for_workspace(root.to_path_buf());
    graph.ensure_loading();
    wait_ready(&graph);
    assert_eq!(
        graph.snapshot().expect("cached graph snapshot").generation,
        7,
        "mtime-only change must preserve the cached revision"
    );
    assert_eq!(
        meta_string(&cache.graph_db_path(), "built_at"),
        "cached-build-sentinel",
        "mtime-only reuse must not rewrite the graph"
    );
    assert_eq!(
        graph.full_builds_started.load(Ordering::SeqCst),
        0,
        "mtime-only reuse must not start a full graph build"
    );
}

#[test]
fn moved_graph_keeps_nested_roots_for_same_relative_paths() {
    let old_dir = tempfile::tempdir().expect("old workspace tempdir");
    let old_root = old_dir.path().join("workspace");
    let configuration = old_root.join("src/cf");
    let extension = old_root.join("src/cfe/Расш");
    fs::create_dir_all(&configuration).expect("create configuration root");
    fs::create_dir_all(&extension).expect("create extension root");
    super::test_support::write(
        &old_root,
        "bsl-analyzer.toml",
        "[source]\nroot = \"src/cf\"\nextensions = [\"src/cfe/Расш\"]\n",
    );
    super::test_support::write(&old_root, "src/cf/Configuration.xml", "<Configuration/>");
    super::test_support::write(&old_root, "src/cfe/Расш/Configuration.xml", "<Configuration/>");
    super::test_support::write_common_module(
        &configuration,
        "Одинаковый",
        true,
        "&НаСервере\nФункция Проверить() Экспорт\nВозврат 1;\nКонецФункции",
    );
    super::test_support::write_common_module(
        &extension,
        "Одинаковый",
        true,
        "&НаСервере\nФункция Проверить() Экспорт\nВозврат 2;\nКонецФункции",
    );

    let fingerprint = super::scan::workspace_fingerprint(&old_root);
    seed_cache(&old_root, fingerprint);

    let new_dir = tempfile::tempdir().expect("new workspace tempdir");
    let new_root = new_dir.path().join("workspace");
    copy_tree(&old_root, &new_root);
    super::test_support::write(
        &old_root,
        "src/cf/CommonModules/Одинаковый/Ext/Module.bsl",
        "&НаСервере\nФункция Проверить() Экспорт\nВозврат 99;\nКонецФункции",
    );
    super::test_support::write(
        &old_root,
        "src/cfe/Расш/CommonModules/Одинаковый/Ext/Module.bsl",
        "&НаСервере\nФункция Проверить() Экспорт\nВозврат 98;\nКонецФункции",
    );

    let cache = WorkspaceCacheLayout::for_workspace(&new_root);
    let graph = crate::graph::GraphState::for_workspace(new_root.clone());
    graph.ensure_loading();
    wait_ready(&graph);
    assert_eq!(graph.snapshot().expect("moved nested graph snapshot").generation, 7);
    assert_eq!(
        meta_string(&cache.graph_db_path(), "built_at"),
        "cached-build-sentinel",
        "moving nested roots must reuse the portable graph",
    );
    assert_eq!(
        graph.full_builds_started.load(Ordering::SeqCst),
        0,
        "moving nested roots must not start a full graph build"
    );

    let snapshot = graph.snapshot().expect("moved nested graph snapshot");
    let roots = snapshot.workspace_roots().expect("roots paired with moved graph");
    let relative = "CommonModules/Одинаковый/Ext/Module.bsl";
    let configuration_key = roots
        .key_of_path(
            &new_root
                .join("src/cf")
                .join(relative)
                .canonicalize()
                .expect("canonical configuration module"),
        )
        .expect("configuration module root key");
    let extension_key = roots
        .key_of_path(
            &new_root
                .join("src/cfe/Расш")
                .join(relative)
                .canonicalize()
                .expect("canonical extension module"),
        )
        .expect("extension module root key");
    let expected_path = configuration_key.path.clone();
    assert_eq!(extension_key.path, expected_path);
    assert_ne!(configuration_key.root_id, extension_key.root_id);

    let connection = rusqlite::Connection::open(cache.graph_db_path())
        .expect("open moved nested graph database");
    let mut statement = connection
        .prepare("SELECT root_id, path FROM files WHERE path = ?1")
        .expect("prepare moved nested graph file query");
    let rows = statement
        .query_map([expected_path.as_str()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .expect("query moved nested graph file keys")
        .map(|row| row.expect("read moved nested graph file key"))
        .collect::<Vec<_>>();
    assert!(
        rows.contains(&(configuration_key.root_id.clone(), expected_path.clone())),
        "configuration file keeps its root id: {rows:?}"
    );
    assert!(
        rows.contains(&(extension_key.root_id.clone(), expected_path)),
        "extension file keeps its root id: {rows:?}"
    );

    let node = snapshot
        .graph
        .node("method/common/Одинаковый/Проверить", ide::GraphDetail::Bodies, Some(roots))
        .expect("read moved nested graph method")
        .expect("moved nested graph method exists");
    assert!(
        node.node.source.as_deref().is_some_and(|source| {
            (source.contains("Возврат 1;") || source.contains("Возврат 2;"))
                && !source.contains("Возврат 98;")
                && !source.contains("Возврат 99;")
        }),
        "the moved graph must read one of the new root texts, not either old root"
    );
}

#[test]
fn content_change_with_same_size_and_mtime_invalidates_cached_graph() {
    let dir = tempfile::tempdir().expect("workspace tempdir");
    let root = dir.path();
    sample_workspace(root);
    super::test_support::write(root, "Configuration.xml", "<Configuration/>");
    super::test_support::write(
        root,
        "bsl-analyzer.toml",
        "[source]\nroot = \".\"\nextensions = []\n",
    );

    let source = root.join(CLIENT_MODULE_PATH);
    let original = fs::metadata(&source).expect("source metadata before same-stat edit");
    let original_mtime = original.modified().expect("source mtime before same-stat edit");
    let original_bytes = fs::read(&source).expect("source bytes before same-stat edit");
    let replacement = CLIENT_SOURCE.replace("Считать", "Запросы");
    assert_eq!(
        replacement.len(),
        original_bytes.len(),
        "same-stat fixture must preserve byte length"
    );
    assert_ne!(replacement.as_bytes(), original_bytes.as_slice());

    let fingerprint = super::scan::workspace_fingerprint(root);
    let cache = WorkspaceCacheLayout::for_workspace(root);
    seed_cache(root, fingerprint);
    fs::write(&source, replacement.as_bytes()).expect("write same-stat replacement");
    set_file_modified(&source, original_mtime);
    let current = fs::metadata(&source).expect("source metadata after same-stat edit");
    assert_eq!(current.len(), original.len());
    assert_eq!(current.modified().expect("source mtime after same-stat edit"), original_mtime);
    let current_bytes = fs::read(&source).expect("source bytes after same-stat edit");
    assert_ne!(
        blake3::hash(&current_bytes),
        blake3::hash(&original_bytes),
        "same-stat content change must alter the content hash"
    );

    let graph = crate::graph::GraphState::for_workspace(root.to_path_buf());
    graph.ensure_loading();
    wait_ready(&graph);
    wait_until(&graph, "same-stat content rebuild", || {
        graph.snapshot().is_some_and(|snapshot| snapshot.generation == 8)
    });
    assert_ne!(
        meta_string(&cache.graph_db_path(), "built_at"),
        "cached-build-sentinel",
        "same-stat content change must replace the cached graph"
    );
    let full_builds = graph.full_builds_started.load(Ordering::SeqCst);
    assert!(
        full_builds <= 1,
        "same-stat content change must publish after at most one full graph build; started {full_builds}"
    );

    let snapshot = graph.snapshot().expect("rebuilt graph snapshot");
    let roots = snapshot.workspace_roots().expect("roots paired with rebuilt graph");
    let node = snapshot
        .graph
        .node("method/common/Клиент/Главная", ide::GraphDetail::Bodies, Some(roots))
        .expect("read rebuilt graph method")
        .expect("rebuilt graph method exists");
    assert!(
        node.node.source.as_deref().is_some_and(|source| source.contains("Сервер.Запросы")),
        "the rebuilt graph must contain the replacement bytes"
    );
}

#[test]
fn incompatible_graph_format_builds_once_without_reembedding_ready_search_index() {
    let _env_lock = crate::state::test_support::env_lock();
    let dir = tempfile::tempdir().expect("workspace tempdir");
    let root = dir.path();
    sample_workspace(root);
    super::test_support::write(root, "Configuration.xml", "<Configuration/>");
    super::test_support::write(
        root,
        "bsl-analyzer.toml",
        "[source]\nroot = \".\"\nextensions = []\n",
    );
    let fingerprint = super::scan::workspace_fingerprint(root);
    let cache = WorkspaceCacheLayout::for_workspace(root);
    let vector = vec![1.0_f32, 0.0, 0.0];
    seed_cache(root, fingerprint);
    seed_ready_search(&cache, root, &vector);
    rusqlite::Connection::open(cache.graph_db_path())
        .expect("open graph database for old format fixture")
        .execute("INSERT OR REPLACE INTO meta (key, value) VALUES ('schema_version', '20')", [])
        .expect("mark graph database as old format");

    let server = CountingEmbeddingServer::start();
    let server_url = server.url();
    let _test_embedding = crate::state::test_support::EnvVarGuard::set("BSL_TEST_EMBEDDING", "1");
    let _embedding_url = crate::state::test_support::EnvVarGuard::set("EMBEDDING_URL", &server_url);
    let _embedding_model =
        crate::state::test_support::EnvVarGuard::set("EMBEDDING_MODEL", "test-model");
    let _embedding_dim = crate::state::test_support::EnvVarGuard::set("EMBEDDING_DIM", "3");

    // Keep the initial graph claim held while the real workspace search initialization publishes
    // its semantic engine. This forces the old-format graph through the ordinary background
    // loader, so the counter below measures one actual rebuild rather than a fused startup pass.
    let (state, held) = holding_the_first_build(|| {
        crate::state::SharedState::workspace_with_cache(root.to_path_buf(), cache.clone())
            .expect("initialize workspace state over old graph format")
    });
    wait_for_search_initialization(&state);
    let (vectors_before, contexts_before) = search_vectors_and_hit(&state, &vector);
    drop(held);
    state.graph().ensure_loading();
    wait_ready(state.graph());
    wait_until(state.graph(), "the real publish hook", || {
        state.graph().publish_passes.load(Ordering::SeqCst) >= 1
    });

    assert_eq!(
        state.graph().full_builds_started.load(Ordering::SeqCst),
        1,
        "one full background rebuild for old format"
    );
    assert_ne!(
        meta_string(&cache.graph_db_path(), "built_at"),
        "cached-build-sentinel",
        "old format must be replaced by a newly built graph"
    );
    assert_eq!(
        meta_string(&cache.graph_db_path(), "schema_version"),
        crate::graph_db::SCHEMA_VERSION.to_string()
    );
    let (vectors_after, contexts_after) = search_vectors_and_hit(&state, &vector);
    assert_eq!(vectors_after, vectors_before, "graph format rebuild keeps vectors");
    assert_eq!(contexts_after, contexts_before, "graph format rebuild keeps graph contexts");
    assert_eq!(server.calls(), 0, "graph format migration must not call the model");
    state.shutdown();
}

/// Set every workspace file an hour back, so no hash taken of it falls in the racy window.
fn age_tree(root: &Path) {
    let hour_ago = SystemTime::now() - Duration::from_secs(3600);
    for entry in fs::read_dir(root).expect("read workspace directory") {
        let path = entry.expect("workspace entry").path();
        if path.is_dir() {
            age_tree(&path);
        } else {
            set_file_modified(&path, hour_ago);
        }
    }
}

fn scan_project(root: &Path) -> (super::input::ProjectSnapshot, bsl_search::WorkspaceRoots) {
    let cache = WorkspaceCacheLayout::for_workspace(root);
    let excluded = cache.exclusions(root);
    let project = super::input::ProjectSnapshot::load_excluding(root, &excluded);
    let roots = project.search_roots.clone().expect("validated workspace roots");
    (project, roots)
}

/// A restarted process reuses the hashes the cached graph recorded for unchanged files, and a
/// copied tree — other inodes, other change times — is read in full.
#[test]
fn a_restarted_process_reuses_the_hashes_its_graph_recorded() {
    let dir = tempfile::tempdir().expect("workspace tempdir");
    let root = dir.path().join("ws");
    sample_workspace(&root);
    super::test_support::write(&root, "Configuration.xml", "<Configuration/>");
    super::test_support::write(
        &root,
        "bsl-analyzer.toml",
        "[source]\nroot = \".\"\nextensions = []\n",
    );
    age_tree(&root);
    let fingerprint = super::scan::workspace_fingerprint(&root);
    seed_cache(&root, fingerprint);

    let (project, roots) = scan_project(&root);
    super::content_hash::forget_under(&root);
    let cold =
        super::universe::ScannedUniverse::scan_excluding(&project.scan_roots, &project.excluded);
    assert!(cold.hashed_files > 0, "a process that remembers nothing reads every file");
    super::content_hash::forget_under(&root);
    super::content_hash::seed(crate::graph_db::read_stored_observations(
        &WorkspaceCacheLayout::for_workspace(&root).graph_db_path(),
        &roots,
    ));
    let seeded =
        super::universe::ScannedUniverse::scan_excluding(&project.scan_roots, &project.excluded);
    assert_eq!(seeded.hashed_files, 0, "the recorded hashes stand in for the reads");
    assert_eq!(
        super::scan::fingerprint_of_project(&seeded.stats, &project),
        Some(fingerprint),
        "reused hashes describe the same content",
    );

    let copy = dir.path().join("copy");
    copy_tree(&root, &copy);
    age_tree(&copy);
    let (copy_project, copy_roots) = scan_project(&copy);
    super::content_hash::seed(crate::graph_db::read_stored_observations(
        &WorkspaceCacheLayout::for_workspace(&copy).graph_db_path(),
        &copy_roots,
    ));
    let copied = super::universe::ScannedUniverse::scan_excluding(
        &copy_project.scan_roots,
        &copy_project.excluded,
    );
    #[cfg(unix)]
    assert_eq!(copied.hashed_files, copied.stats.len(), "a copy is read in full");
    assert_eq!(
        super::scan::fingerprint_of_project(&copied.stats, &copy_project).map(|fp| fp.files),
        Some(fingerprint.files),
        "the copy has the same content identity",
    );
}
