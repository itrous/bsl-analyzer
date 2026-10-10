#![cfg(unix)]

use std::os::unix::fs::MetadataExt;
use std::time::{Duration, UNIX_EPOCH};

use mcp_server::{serve_stream, McpProfile, McpServer, SharedState, WorkspaceCacheLayout};
use rmcp::model::CallToolRequestParams;
use rmcp::service::RunningService;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{json, Map, Value};

async fn client(server: McpServer) -> RunningService<RoleClient, ()> {
    let (client_io, server_io) = tokio::io::duplex(4 * 1024 * 1024);
    tokio::spawn(serve_stream(server, server_io));
    ().serve(client_io).await.expect("MCP session initializes")
}

fn arguments(action: &str) -> Map<String, Value> {
    Map::from_iter([("action".to_owned(), json!(action))])
}

async fn graph_status(client: &RunningService<RoleClient, ()>) -> Value {
    client
        .call_tool(CallToolRequestParams::new("graph").with_arguments(arguments("status")))
        .await
        .expect("graph status transport")
        .structured_content
        .expect("graph status is structured")
}

async fn wait_current_graph(client: &RunningService<RoleClient, ()>, previous: Option<u64>) -> u64 {
    let mut last = Value::Null;
    for _ in 0..400 {
        last = graph_status(client).await;
        if last["state"] == "ready" && last["stale"] == false {
            let revision = last["revision"].as_u64().expect("ready status carries revision");
            if previous != Some(revision) {
                return revision;
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("graph did not publish the expected current generation: {last}");
}

async fn wait_superseded(client: &RunningService<RoleClient, ()>) -> Value {
    let mut last = Value::Null;
    for _ in 0..200 {
        last = graph_status(client).await;
        if last["superseded"] == true {
            return last;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("old daemon did not observe supersession: {last}");
}

#[derive(Debug, PartialEq, Eq)]
struct FileStamp {
    inode: u64,
    len: u64,
    modified_ns: u128,
    hash: String,
}

fn stamp(path: &std::path::Path) -> FileStamp {
    let metadata = std::fs::metadata(path).unwrap();
    let modified_ns = metadata.modified().unwrap().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    FileStamp {
        inode: metadata.ino(),
        len: metadata.len(),
        modified_ns,
        hash: blake3::hash(&std::fs::read(path).unwrap()).to_hex().to_string(),
    }
}

fn search_storage_stamp(path: &std::path::Path) -> (FileStamp, Option<FileStamp>) {
    let mut wal = path.as_os_str().to_os_string();
    wal.push("-wal");
    let wal = std::path::PathBuf::from(wal);
    (stamp(path), wal.exists().then(|| stamp(&wal)))
}

fn write_workspace(root: &std::path::Path, value: u32) {
    let module = root.join("CommonModules/Сервер/Ext/Module.bsl");
    std::fs::create_dir_all(module.parent().unwrap()).unwrap();
    std::fs::write(root.join("Configuration.xml"), "<Configuration/>").unwrap();
    std::fs::write(
        root.join("CommonModules/Сервер.xml"),
        r#"<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses"><CommonModule uuid="00000000-0000-0000-0000-000000000001"><Properties><Name>Сервер</Name><Global>false</Global><ClientManagedApplication>false</ClientManagedApplication><Server>true</Server><ExternalConnection>false</ExternalConnection><ClientOrdinaryApplication>false</ClientOrdinaryApplication><ServerCall>false</ServerCall><Privileged>false</Privileged><ReturnValuesReuse>DontUse</ReturnValuesReuse></Properties></CommonModule></MetaDataObject>"#,
    )
    .unwrap();
    std::fs::write(
        module,
        format!("&НаСервере\nФункция Считать() Экспорт\nВозврат {value};\nКонецФункции"),
    )
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn superseded_daemon_lifecycle() {
    let workspace = tempfile::tempdir().unwrap();
    let cache_dir = tempfile::tempdir().unwrap();
    let root = workspace.path().to_path_buf();
    write_workspace(&root, 1);
    let project = mcp_server::project::at(&root).unwrap();
    let cache =
        WorkspaceCacheLayout::for_project(&project, Some(cache_dir.path()), &root, None).unwrap();

    let old_server = McpServer::new(
        McpProfile::Workspace,
        SharedState::workspace_with_cache(root.clone(), cache.clone()).unwrap(),
    );
    let old_client = client(old_server.clone()).await;
    let old_revision = wait_current_graph(&old_client, None).await;
    old_client
        .call_tool(CallToolRequestParams::new("graph").with_arguments(arguments("overview")))
        .await
        .expect("old daemon primes its descriptor pool");
    let old_file = std::fs::File::open(cache.graph_db_path()).unwrap();
    let old_inode = old_file.metadata().unwrap().ino();
    // A connection opened past the daemon's graph store: what an outside reader of the file
    // keeps across the replacement.
    let old_db = rusqlite::Connection::open_with_flags(
        cache.graph_db_path(),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();

    write_workspace(&root, 2);
    let new_server = McpServer::new(
        McpProfile::Workspace,
        SharedState::workspace_with_cache(root.clone(), cache.clone()).unwrap(),
    );
    let new_client = client(new_server.clone()).await;

    let terminal = wait_superseded(&old_client).await;
    assert!(matches!(terminal["state"].as_str(), Some("ready" | "failed")), "{terminal}");

    let new_revision = wait_current_graph(&new_client, Some(old_revision)).await;
    assert_ne!(new_revision, old_revision);
    // A body-only edit is patched into the published file itself, not renamed over it.
    assert_eq!(stamp(&cache.graph_db_path()).inode, old_inode, "the patch kept the file");
    assert_eq!(old_file.metadata().unwrap().ino(), old_inode, "the old inode stays open");
    let revision: String = old_db
        .query_row("SELECT value FROM meta WHERE key = 'revision'", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        revision.parse::<u64>().unwrap(),
        new_revision,
        "an outside connection on the file sees the publication the new owner committed"
    );
    let nodes: i64 = old_db.query_row("SELECT COUNT(*) FROM nodes", [], |row| row.get(0)).unwrap();
    assert!(nodes > 0, "the pre-replacement SQLite handle remains readable");

    for _ in 0..200 {
        if cache.search_db_path().exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(cache.search_db_path().exists(), "workspace search store was initialized");
    new_client.cancel().await.ok();
    new_server.shutdown();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let graph_after_owner = stamp(&cache.graph_db_path());
    let search_after_owner = search_storage_stamp(&cache.search_db_path());

    for _ in 0..3 {
        let status = graph_status(&old_client).await;
        assert_eq!(status["superseded"], true);
        // Never an empty answer from a daemon that lost the workspace: the client is told to
        // open a new session.
        match old_client
            .call_tool(CallToolRequestParams::new("graph").with_arguments(arguments("overview")))
            .await
        {
            Err(rmcp::ServiceError::McpError(error)) => assert_eq!(
                error.data.as_ref().and_then(|data| data["reason"].as_str()),
                Some("owner_changed"),
                "{error:?}"
            ),
            other => panic!("a superseded daemon answered the graph: {other:?}"),
        }
        let _ = old_client
            .call_tool(CallToolRequestParams::new("search").with_arguments(arguments("status")))
            .await;
        let mut search = arguments("search_code");
        search.insert("query".to_owned(), json!("Считать"));
        let _ =
            old_client.call_tool(CallToolRequestParams::new("search").with_arguments(search)).await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(stamp(&cache.graph_db_path()), graph_after_owner);
    assert_eq!(search_storage_stamp(&cache.search_db_path()), search_after_owner);

    let before_third = stamp(&cache.graph_db_path());
    let third_server = McpServer::new(
        McpProfile::Workspace,
        SharedState::workspace_with_cache(root, cache.clone()).unwrap(),
    );
    let third_client = client(third_server.clone()).await;
    assert_eq!(wait_current_graph(&third_client, None).await, new_revision);
    assert_eq!(stamp(&cache.graph_db_path()), before_third, "third generation reused the cache");

    third_client.cancel().await.ok();
    third_server.shutdown();
    old_client.cancel().await.ok();
    old_server.shutdown();
}
