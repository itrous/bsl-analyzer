//! End-to-end checks that the source-set flags actually change the analysis,
//! driven through the real `analyze` binary and its `--format jsonl` output.
//!
//! Unit tests over `SourceSetArgs` only prove that argv turns into the right
//! project model. They cannot show that the model reaches the analyzer, which
//! is the whole point of the flags: an extension analyzed without its main
//! configuration reports valid calls into that configuration as unresolved.
//!
//! Every check here is an A/B differing by exactly one flag, and the run that
//! is expected to be clean is only trusted next to a run that is not — a
//! "diagnostic absent" result on its own is equally consistent with the file
//! never having been analyzed at all.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
#[cfg(unix)]
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::Value;

const MAIN_MODULE: &str = "БазовыйМодуль";
const DEP_MODULE: &str = "МодульЗависимости";
const EXT_MODULE: &str = "МодульРасширения";

/// A call nothing in any source set can resolve. Kept beside the call under
/// test so that "the diagnostic disappeared" can be told apart from "the
/// diagnostic stopped being produced at all in the flagged branch".
const MISSING_MODULE: &str = "ЗаведомоНетТакогоМодуля";
const MISSING_CALL: &str = "ЗаведомоНетТакогоМодуля.НетТакогоМетода();";

/// A configuration root deep enough that `Project`'s own two-level search for
/// `Configuration.xml` cannot find it, and not under `src/cf` or `Configuration`
/// either. Without this the "no main configuration" run would silently acquire
/// one by discovery, and the control it provides would be worthless.
const MAIN: &str = "a/b/main";
const EXT: &str = "a/b/ext";
const DEP: &str = "a/b/dep";
#[cfg(unix)]
const CACHE_ADOPTION_LOG: &str = "reused cached graph database (workspace unchanged)";
const STDERR_TAIL_LIMIT: usize = 64 * 1024;

/// Drains child stderr continuously while retaining only a bounded tail. This
/// keeps native startup diagnostics available without allowing a verbose child
/// to block on a full stderr pipe or grow the fixture's memory without bound.
struct StderrCapture {
    tail: Arc<Mutex<String>>,
    reader: Option<JoinHandle<()>>,
}

impl StderrCapture {
    fn attach(stderr: std::process::ChildStderr) -> Self {
        use std::io::Read as _;

        let tail = Arc::new(Mutex::new(String::new()));
        let reader_tail = Arc::clone(&tail);
        let reader = std::thread::spawn(move || {
            let mut stderr = stderr;
            let mut buffer = [0_u8; 4096];
            loop {
                match stderr.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => {
                        let text = String::from_utf8_lossy(&buffer[..read]);
                        let mut tail = reader_tail.lock().expect("stderr capture mutex");
                        tail.push_str(&text);
                        if tail.len() > STDERR_TAIL_LIMIT {
                            let mut trim = tail.len() - STDERR_TAIL_LIMIT;
                            while !tail.is_char_boundary(trim) {
                                trim += 1;
                            }
                            tail.drain(..trim);
                        }
                    }
                }
            }
        });
        Self { tail, reader: Some(reader) }
    }

    fn snapshot(&self) -> String {
        self.tail.lock().expect("stderr capture mutex").clone()
    }

    #[cfg(unix)]
    fn cache_adoption_evidence(&self) -> Vec<String> {
        self.snapshot()
            .lines()
            .filter(|line| line.contains(CACHE_ADOPTION_LOG))
            // Keep only the fixed native message in the receipt; tracing's
            // surrounding fields can include machine-specific paths.
            .map(|_| CACHE_ADOPTION_LOG.to_owned())
            .collect()
    }

    #[cfg(unix)]
    fn wait_for_cache_adoption(&self) -> Vec<String> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let evidence = self.cache_adoption_evidence();
            if !evidence.is_empty() || Instant::now() >= deadline {
                return evidence;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[cfg(unix)]
    fn wait_for_text(&self, needle: &str, timeout: Duration) -> Option<String> {
        let deadline = Instant::now() + timeout;
        loop {
            let snapshot = self.snapshot();
            if snapshot.contains(needle) || Instant::now() >= deadline {
                return snapshot.contains(needle).then_some(snapshot);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn join(&mut self) {
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

fn configuration_xml(name: &str, module: &str, extension: bool) -> String {
    let purpose = if extension {
        "<ConfigurationExtensionPurpose>Customization</ConfigurationExtensionPurpose>"
    } else {
        ""
    };
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:v8="http://v8.1c.ru/8.1/data/core">
	<Configuration uuid="11111111-0000-0000-0000-000000000001">
		<Properties><Name>{name}</Name><Synonym/><Comment/><NamePrefix/>{purpose}<DefaultRunMode>ManagedApplication</DefaultRunMode></Properties>
		<ChildObjects><CommonModule>{module}</CommonModule></ChildObjects>
	</Configuration>
</MetaDataObject>"#
    )
}

fn common_module_xml(module: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:v8="http://v8.1c.ru/8.1/data/core">
	<CommonModule uuid="22222222-0000-0000-0000-000000000002">
		<Properties><Name>{module}</Name><Synonym/><Comment/><Global>false</Global><ClientManagedApplication>false</ClientManagedApplication><Server>true</Server><ExternalConnection>false</ExternalConnection><ClientOrdinaryApplication>false</ClientOrdinaryApplication><ServerCall>false</ServerCall><Privileged>false</Privileged><ReturnValuesReuse>DontUse</ReturnValuesReuse></Properties>
	</CommonModule>
</MetaDataObject>"#
    )
}

fn write_configuration(root: &Path, rel: &str, name: &str, module: &str, body: &str, ext: bool) {
    let dir = root.join(rel);
    let module_dir = dir.join("CommonModules").join(module).join("Ext");
    std::fs::create_dir_all(&module_dir).unwrap();
    std::fs::write(dir.join("Configuration.xml"), configuration_xml(name, module, ext)).unwrap();
    std::fs::write(
        dir.join("CommonModules").join(format!("{module}.xml")),
        common_module_xml(module),
    )
    .unwrap();
    std::fs::write(module_dir.join("Module.bsl"), body).unwrap();
}

/// Main configuration plus an extension whose module calls the main
/// configuration's exported common-module method.
fn workspace_calling_main_configuration(root: &Path) {
    write_configuration(
        root,
        MAIN,
        "ОсновнаяКонфигурация",
        MAIN_MODULE,
        "Функция Экспортируемая() Экспорт\n\tВозврат 1;\nКонецФункции\n",
        false,
    );
    write_configuration(
        root,
        EXT,
        "Расширение",
        EXT_MODULE,
        &format!(
            "Процедура Вызвать() Экспорт\n\t{MAIN_MODULE}.Экспортируемая();\n\t{MISSING_CALL}\nКонецПроцедуры\n"
        ),
        true,
    );
}

struct Run {
    files: Vec<Value>,
    done: Value,
    stderr: String,
}

impl Run {
    /// Matched on the module's own path tail rather than on the name appearing
    /// anywhere in the absolute path: a temp directory that happens to carry the
    /// module's name in an ancestor would otherwise pick the wrong file.
    fn file_event(&self, module: &str) -> Option<&Value> {
        self.file_event_at(&format!("CommonModules/{module}/Ext/Module.bsl"))
    }

    fn file_event_at(&self, tail: &str) -> Option<&Value> {
        let tail = Path::new(tail);
        let tail = if tail.is_absolute() { tail.canonicalize().ok()? } else { tail.to_path_buf() };
        self.files
            .iter()
            .find(|e| e["path"].as_str().is_some_and(|p| Path::new(p).ends_with(&tail)))
    }

    /// The module's file event, having established that it was actually
    /// analyzed. A `file` event is emitted for files whose analysis panicked or
    /// whose text could not be read, with the failure recorded in `error` and
    /// `done.failed_files` and the process still exiting zero — so the event's
    /// mere presence proves nothing.
    fn analyzed(&self, module: &str) -> &Value {
        self.analyzed_at(&format!("CommonModules/{module}/Ext/Module.bsl"))
    }

    fn analyzed_at(&self, tail: &str) -> &Value {
        let event = self
            .file_event_at(tail)
            .unwrap_or_else(|| panic!("{tail} was not analyzed at all; jsonl: {:?}", self.files));
        assert_eq!(event["error"], Value::Null, "{tail} failed to analyze: {event}");
        assert_eq!(self.done["failed_files"], 0, "some file failed: {}", self.done);
        event
    }

    /// Messages of the given code reported for the module, in order.
    ///
    /// Compared as text rather than counted: the fixture keeps a deliberately
    /// unresolvable call beside the one under test, and a count alone cannot
    /// tell "the real call resolved" from "the two swapped places" or from the
    /// diagnostic being suppressed wholesale.
    fn messages_at(&self, tail: &str, code: &str) -> Vec<String> {
        self.analyzed_at(tail)["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|d| d["code"].as_str() == Some(code))
            .map(|d| d["message"].as_str().unwrap_or_default().to_owned())
            .collect()
    }

    /// Which module names the given code complains about, sorted.
    fn unresolved_modules(&self, module: &str) -> Vec<String> {
        self.unresolved_modules_at(&format!("CommonModules/{module}/Ext/Module.bsl"))
    }

    fn unresolved_modules_at(&self, tail: &str) -> Vec<String> {
        let mut names: Vec<String> = self
            .messages_at(tail, "UnresolvedMethodCall")
            .iter()
            .filter_map(|m| m.split('\'').nth(1).map(str::to_owned))
            .collect();
        names.sort();
        names
    }
}

fn analyze(source_dir: &Path, flags: &[&str]) -> Run {
    let output = Command::new(env!("CARGO_BIN_EXE_bsl-analyzer-app"))
        .arg("analyze")
        .arg("-s")
        .arg(source_dir)
        .args(flags)
        .args(["--format", "jsonl"])
        .env_remove("ONEC_CONFIGURATIONS_ROOT")
        .output()
        .expect("failed to run the analyzer");
    // Checked for every run, including the ones inspected only through stderr:
    // the notice is printed before the walk, so a process that dies afterwards
    // still satisfies a bare `contains` and turns its paired run into noise.
    assert!(
        output.status.success(),
        "analyze {flags:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).unwrap();
    let events: Vec<Value> = stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    Run {
        files: events.iter().filter(|e| e["type"] == "file").cloned().collect(),
        done: events
            .iter()
            .find(|e| e["type"] == "done")
            .cloned()
            .unwrap_or_else(|| panic!("no done event; stdout: {stdout}")),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

fn workspace() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct TreeEntry {
    kind: &'static str,
    size: u64,
    hash: Option<String>,
    permissions: u32,
    symlink_target: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct TreeSnapshot {
    entries: BTreeMap<String, TreeEntry>,
    fingerprint: String,
}

fn tree_snapshot(root: &Path) -> TreeSnapshot {
    fn walk(root: &Path, path: &Path, entries: &mut BTreeMap<String, TreeEntry>) {
        let metadata = std::fs::symlink_metadata(path).unwrap();
        let file_type = metadata.file_type();
        let kind = if file_type.is_symlink() {
            "symlink"
        } else if file_type.is_dir() {
            "directory"
        } else if file_type.is_file() {
            "file"
        } else {
            "other"
        };
        #[cfg(unix)]
        let permissions = {
            use std::os::unix::fs::PermissionsExt as _;
            metadata.permissions().mode()
        };
        #[cfg(windows)]
        let permissions = {
            use std::os::windows::fs::MetadataExt as _;
            metadata.file_attributes()
        };
        #[cfg(not(any(unix, windows)))]
        let permissions = if metadata.permissions().readonly() { 1 } else { 0 };
        let symlink_target = file_type
            .is_symlink()
            .then(|| std::fs::read_link(path).unwrap().to_string_lossy().into_owned());
        let hash = file_type.is_file().then(|| {
            let bytes = std::fs::read(path)
                .unwrap_or_else(|error| panic!("read snapshot file {}: {error}", path.display()));
            blake3::hash(&bytes).to_hex().to_string()
        });
        let relative = path.strip_prefix(root).unwrap();
        entries.insert(
            relative.to_string_lossy().into_owned(),
            TreeEntry { kind, size: metadata.len(), hash, permissions, symlink_target },
        );
        if file_type.is_dir() {
            for child in std::fs::read_dir(path).unwrap() {
                walk(root, &child.unwrap().path(), entries);
            }
        }
    }

    let mut entries = BTreeMap::new();
    walk(root, root, &mut entries);
    let encoded = serde_json::to_vec(&entries).unwrap();
    TreeSnapshot { entries, fingerprint: blake3::hash(&encoded).to_hex().to_string() }
}

fn snapshot_report(snapshot: &TreeSnapshot) -> Value {
    serde_json::to_value(snapshot).unwrap()
}

fn assert_tree_unchanged(expected: &TreeSnapshot, root: &Path, phase: &str) {
    let actual = tree_snapshot(root);
    assert_eq!(
        actual, *expected,
        "source tree changed {phase}: expected {}, got {}",
        expected.fingerprint, actual.fingerprint
    );
}

#[cfg(unix)]
fn assert_tree_unchanged_except_workspace_cache(
    expected: &TreeSnapshot,
    root: &Path,
    cache_leaf: &Path,
    phase: &str,
) -> TreeSnapshot {
    let actual = tree_snapshot(root);
    let cache_rel = cache_leaf.strip_prefix(root).unwrap().to_string_lossy().into_owned();
    for (path, before) in &expected.entries {
        let after = actual
            .entries
            .get(path)
            .unwrap_or_else(|| panic!("source path disappeared {phase}: {path:?}"));
        if path == ".build" {
            assert_eq!(after.kind, before.kind, "legacy namespace type is unchanged {phase}");
            assert_eq!(
                after.permissions, before.permissions,
                "legacy namespace permissions are unchanged {phase}"
            );
            assert_eq!(after.symlink_target, before.symlink_target);
        } else {
            assert_eq!(after, before, "pre-existing source path changed {phase}: {path:?}");
        }
    }
    for path in actual.entries.keys().filter(|path| !expected.entries.contains_key(*path)) {
        assert!(
            path == ".build/workspaces"
                || path == ".build/workspaces/v1"
                || path == &cache_rel
                || path.starts_with(&format!("{cache_rel}/")),
            "only the explicitly selected cache family may be new {phase}: {path:?}"
        );
    }
    actual
}

#[cfg(unix)]
fn assert_flat_canaries_unchanged(before: &TreeSnapshot, source_root: &Path) {
    let current = tree_snapshot(&source_root.join(".build"));
    for (path, entry) in &before.entries {
        if path.is_empty() {
            continue;
        }
        assert_eq!(
            current.entries.get(path),
            Some(entry),
            "legacy flat cache entry is preserved: {path}"
        );
    }
    for path in current.entries.keys().filter(|path| !before.entries.contains_key(*path)) {
        assert!(
            path == "workspaces" || path == "workspaces/v1" || path.starts_with("workspaces/v1/"),
            "no legacy flat cache path is added or overwritten: {path:?}"
        );
    }
}

#[cfg(unix)]
fn assert_private_test_dir(path: &Path) {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::create_dir_all(path).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
}

#[cfg(unix)]
struct SourcePermissionsRestore(Vec<(PathBuf, std::fs::Permissions)>);

#[cfg(unix)]
impl Drop for SourcePermissionsRestore {
    fn drop(&mut self) {
        for (path, permissions) in self.0.iter().rev() {
            let _ = std::fs::set_permissions(path, permissions.clone());
        }
    }
}

#[cfg(unix)]
fn make_source_tree_read_only(root: &Path) -> SourcePermissionsRestore {
    use std::os::unix::fs::PermissionsExt as _;

    fn collect(path: &Path, entries: &mut Vec<(PathBuf, std::fs::Permissions)>) {
        let metadata = std::fs::symlink_metadata(path).unwrap();
        if metadata.file_type().is_symlink() {
            return;
        }
        entries.push((path.to_path_buf(), metadata.permissions()));
        if metadata.is_dir() {
            for child in std::fs::read_dir(path).unwrap() {
                collect(&child.unwrap().path(), entries);
            }
        }
    }

    let mut restore = Vec::new();
    collect(root, &mut restore);
    let guard = SourcePermissionsRestore(restore);
    for (path, original) in &guard.0 {
        let mut read_only = original.clone();
        read_only.set_mode(original.mode() & !0o222);
        std::fs::set_permissions(path, read_only).unwrap();
    }
    guard
}

struct EmbeddingStub {
    url: String,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    #[cfg(unix)]
    seen_a: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    #[cfg(unix)]
    seen_b: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl EmbeddingStub {
    fn start() -> Self {
        use std::net::TcpListener;
        use std::sync::atomic::{AtomicBool, Ordering};

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let thread_stop = std::sync::Arc::clone(&stop);
        #[cfg(unix)]
        let seen_a = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        #[cfg(unix)]
        let seen_b = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        #[cfg(unix)]
        let thread_seen_a = std::sync::Arc::clone(&seen_a);
        #[cfg(unix)]
        let thread_seen_b = std::sync::Arc::clone(&seen_b);
        let thread = std::thread::spawn(move || {
            while !thread_stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        use std::io::{Read, Write};
                        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                        let mut request = Vec::new();
                        let mut buf = [0u8; 2048];
                        let mut header_end = None;
                        let mut content_len = 0usize;
                        while let Ok(n) = stream.read(&mut buf) {
                            if n == 0 {
                                break;
                            }
                            request.extend_from_slice(&buf[..n]);
                            if header_end.is_none() {
                                if let Some(pos) = request.windows(4).position(|w| w == b"\r\n\r\n")
                                {
                                    let end = pos + 4;
                                    let headers = String::from_utf8_lossy(&request[..pos])
                                        .to_ascii_lowercase();
                                    content_len = headers
                                        .lines()
                                        .find_map(|line| line.strip_prefix("content-length:"))
                                        .and_then(|value| value.trim().parse().ok())
                                        .unwrap_or(0);
                                    header_end = Some(end);
                                }
                            }
                            if header_end.is_some_and(|end| request.len() >= end + content_len) {
                                break;
                            }
                        }
                        let body = header_end.map(|end| &request[end..]).unwrap_or(&[]);
                        let inputs = serde_json::from_slice::<Value>(body)
                            .ok()
                            .and_then(|value| value.get("input").cloned())
                            .map(|input| match input {
                                Value::Array(values) => values,
                                value => vec![value],
                            })
                            .unwrap_or_default();
                        let data: Vec<_> = inputs
                            .iter()
                            .enumerate()
                            .map(|(index, input)| {
                                let text = input.as_str().unwrap_or_default();
                                let embedding = if text.contains("CACHE_VECTOR_A_235")
                                    || text.contains("orange quiet river")
                                {
                                    #[cfg(unix)]
                                    thread_seen_a.fetch_add(1, Ordering::Relaxed);
                                    [1.0, 0.0, 0.0]
                                } else if text.contains("CACHE_VECTOR_B_235")
                                    || text.contains("violet steady mountain")
                                {
                                    #[cfg(unix)]
                                    thread_seen_b.fetch_add(1, Ordering::Relaxed);
                                    [0.0, 1.0, 0.0]
                                } else {
                                    [0.0, 0.0, 1.0]
                                };
                                serde_json::json!({"index": index, "embedding": embedding})
                            })
                            .collect();
                        let response_body = serde_json::json!({"data": data}).to_string();
                        let response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            response_body.len(),
                            response_body
                        );
                        let _ = stream.write_all(response.as_bytes());
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            url,
            stop,
            #[cfg(unix)]
            seen_a,
            #[cfg(unix)]
            seen_b,
            thread: Some(thread),
        }
    }
}

impl Drop for EmbeddingStub {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}

fn source_composition(root: &Path) {
    std::fs::write(root.join(".gitignore"), ".build/\nignored-*/\n").unwrap();
    std::fs::write(root.join("bsl-analyzer.toml"), "[platform_help]\nsource = \"none\"\n").unwrap();
    std::fs::create_dir_all(root.join("ignored-fixture")).unwrap();
    std::fs::write(root.join("ignored-fixture/canary"), b"ignored source canary").unwrap();
    write_configuration(
        root,
        MAIN,
        "ОсновнаяКонфигурация",
        MAIN_MODULE,
        "Функция Экспортируемая() Экспорт\n\tВозврат 1;\nКонецФункции\n",
        false,
    );
    for (relative, config, module, graph, lexical, vector) in [
        (
            "a/b/ext-a",
            "EXT_A",
            "МодульРасширенияА",
            "CacheGraphA235",
            "CACHE_LEXICAL_A_235",
            "CACHE_VECTOR_A_235",
        ),
        (
            "a/b/ext-b",
            "EXT_B",
            "МодульРасширенияБ",
            "CacheGraphB235",
            "CACHE_LEXICAL_B_235",
            "CACHE_VECTOR_B_235",
        ),
    ] {
        write_configuration(
            root,
            relative,
            config,
            module,
            &format!(
                "Функция {graph}() Экспорт\n\tСтрока = \"{lexical} {vector}\";\n\t{MAIN_MODULE}.Экспортируемая();\n\tВозврат Строка;\nКонецФункции\n"
            ),
            true,
        );
    }
    std::fs::create_dir_all(root.join(".build")).unwrap();
    for (name, bytes) in [
        ("bsl-graph.db", b"legacy graph canary".as_slice()),
        ("bsl-graph.db-wal", b"legacy graph wal canary".as_slice()),
        ("bsl-search.db", b"legacy search canary".as_slice()),
        ("writer.lease", b"legacy lease canary".as_slice()),
    ] {
        std::fs::write(root.join(".build").join(name), bytes).unwrap();
    }
}

fn composition_flags(extension: &str) -> [String; 4] {
    let path = if extension == "EXT_A" { "a/b/ext-a" } else { "a/b/ext-b" };
    [
        "--configuration-root".into(),
        MAIN.into(),
        "--extension".into(),
        format!("{extension}={path}"),
    ]
}

fn cache_layout(
    root: &Path,
    extension: &str,
    base: Option<&Path>,
) -> mcp_server::WorkspaceCacheLayout {
    use project_model::{Project, ProjectConfig, SourceSetOverride, StructuredExtensionDecl};
    let mut config = ProjectConfig::load(root).unwrap().unwrap_or_default();
    let extension_path = if extension == "EXT_A" { "a/b/ext-a" } else { "a/b/ext-b" };
    SourceSetOverride {
        configuration_root: Some(MAIN.into()),
        extensions: Some(vec![project_model::ExtensionDecl::Structured(StructuredExtensionDecl {
            name: extension.to_owned(),
            path: extension_path.into(),
            depends_on: Vec::new(),
        })]),
        externals: None,
    }
    .apply_to(&mut config);
    let project = Project::with_config(root, config).unwrap();
    mcp_server::WorkspaceCacheLayout::for_project(&project, base, root, None).unwrap()
}

fn lease_receipt(path: &Path, pid: u32) -> Value {
    let record: Value =
        serde_json::from_slice(&std::fs::read(path).expect("writer lease exists")).unwrap();
    assert_eq!(
        record["pid"].as_u64(),
        Some(u64::from(pid)),
        "lease belongs to the live MCP child: {record}"
    );
    assert!(
        record["generation"].as_u64().unwrap_or_default() > 0,
        "lease generation is published: {record}"
    );
    assert!(record["token"].as_u64().unwrap_or_default() > 0, "lease token is published: {record}");
    record
}

#[cfg(unix)]
fn wait_for_lease_pid(path: &Path, timeout: Duration) -> u32 {
    let deadline = Instant::now() + timeout;
    loop {
        let pid = std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .and_then(|record| record["pid"].as_u64())
            .and_then(|pid| u32::try_from(pid).ok());
        if let Some(pid) = pid {
            return pid;
        }
        assert!(Instant::now() < deadline, "live daemon lease appears at {}", path.display());
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[cfg(unix)]
fn wait_for_cache_adoption_log(
    path: &Path,
    minimum_count: usize,
    timeout: Duration,
) -> Vec<String> {
    use std::io::{Read, Seek, SeekFrom};

    let deadline = Instant::now() + timeout;
    loop {
        let evidence = (|| {
            let mut file = std::fs::File::open(path).ok()?;
            let length = file.metadata().ok()?.len();
            let start = length.saturating_sub(STDERR_TAIL_LIMIT as u64);
            file.seek(SeekFrom::Start(start)).ok()?;
            let mut bytes = Vec::with_capacity((length - start) as usize);
            file.read_to_end(&mut bytes).ok()?;
            Some(
                String::from_utf8_lossy(&bytes)
                    .lines()
                    .filter(|line| line.contains(CACHE_ADOPTION_LOG))
                    .map(|_| CACHE_ADOPTION_LOG.to_owned())
                    .collect::<Vec<_>>(),
            )
        })()
        .unwrap_or_default();
        if evidence.len() > minimum_count || Instant::now() >= deadline {
            return evidence;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(unix)]
fn stop_owned_daemon_after_timeout(pid: u32) {
    // This is used only on a fixture cleanup failure. Target exactly the PID
    // claimed by this test's private cache lease; never search or signal by name.
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGTERM);
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while unsafe { libc::kill(pid as libc::pid_t, 0) } == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(25));
    }
    if unsafe { libc::kill(pid as libc::pid_t, 0) } == 0 {
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGKILL);
        }
        let kill_deadline = Instant::now() + Duration::from_secs(2);
        while unsafe { libc::kill(pid as libc::pid_t, 0) } == 0 && Instant::now() < kill_deadline {
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

#[cfg(unix)]
fn stop_scoped_daemons_after_fixture_timeout(cache_base: &Path, runtime: &Path) {
    let mut owned_pids = std::collections::BTreeSet::new();
    let family = cache_base.join("workspaces/v1");
    if let Ok(scopes) = std::fs::read_dir(family) {
        for scope in scopes.flatten() {
            let lease = scope.path().join("writer.lease");
            let Ok(record) = std::fs::read(&lease).and_then(|bytes| {
                serde_json::from_slice::<Value>(&bytes)
                    .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
            }) else {
                continue;
            };
            if let Some(pid) = record["pid"].as_u64().and_then(|pid| u32::try_from(pid).ok()) {
                owned_pids.insert(pid);
            }
        }
    }
    if let Ok(pid) = std::fs::read_to_string(runtime.join("real-cli-child-started")) {
        if let Ok(pid) = pid.trim().parse::<u32>() {
            owned_pids.insert(pid);
        }
    }
    for pid in owned_pids {
        stop_owned_daemon_after_timeout(pid);
    }
}

#[cfg(unix)]
fn wait_for_scope_guard_exit(
    child: &mut Child,
    stderr: &mut StderrCapture,
    lease_path: &Path,
    phase: &str,
) -> (ExitStatus, usize) {
    const SCOPE_STOP: &str = "workspace source composition changed; restart/reconnect MCP";

    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll scope-guarded native child") {
            break status;
        }
        if Instant::now() >= deadline {
            let stderr_tail = stderr.snapshot();
            stop_owned_daemon_after_timeout(child.id());
            let _ = child.wait();
            stderr.join();
            panic!(
                "{phase} process did not gracefully stop after workspace composition drift; stderr: {stderr_tail}"
            );
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    stderr.join();
    let diagnostic =
        stderr.wait_for_text(SCOPE_STOP, Duration::from_secs(2)).unwrap_or_else(|| {
            panic!("{phase} emits scope-stop diagnostic; stderr: {}", stderr.snapshot())
        });
    let occurrences = diagnostic.matches(SCOPE_STOP).count();
    assert_eq!(occurrences, 1, "{phase} emits exactly one scope-stop diagnostic");
    assert!(status.success(), "{phase} exits successfully after scope drift: {status}");
    let lease_deadline = Instant::now() + Duration::from_secs(2);
    while lease_path.exists() && Instant::now() < lease_deadline {
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(!lease_path.exists(), "{phase} releases its workspace writer lease");
    (status, occurrences)
}

fn search_marker_hits(session: &mut McpSession, marker: &str) -> Vec<Value> {
    session.wait_ready("search");
    let reply = session
        .call("search", serde_json::json!({"action": "search_code", "query": marker, "limit": 50}));
    assert!(reply["error"].is_null(), "search_code failed for {marker}: {reply}");
    reply["result"]["structuredContent"]["hits"]
        .as_array()
        .unwrap_or_else(|| panic!("search_code returned no hits array for {marker}: {reply}"))
        .clone()
}

fn search_marker(session: &mut McpSession, marker: &str) -> Vec<String> {
    search_marker_hits(session, marker)
        .iter()
        .filter_map(|hit| hit["path"].as_str().map(str::to_owned))
        .collect()
}

#[cfg(unix)]
fn assert_search_marker_root(session: &mut McpSession, marker: &str, expected_root: &str) {
    let hits = search_marker_hits(session, marker);
    assert!(
        hits.iter().any(|hit| hit["root_id"] == expected_root),
        "lexical search finds {marker} in its own {expected_root} source root: {hits:?}"
    );
}

#[cfg(unix)]
fn assert_search_excludes_root(session: &mut McpSession, marker: &str, excluded_root: &str) {
    let hits = search_marker_hits(session, marker);
    assert!(
        hits.iter().all(|hit| hit["root_id"].is_string()),
        "workspace lexical results identify their source roots: {hits:?}"
    );
    assert!(
        hits.iter().all(|hit| hit["root_id"] != excluded_root),
        "lexical search for {marker} returns no {excluded_root} rows: {hits:?}"
    );
}

#[cfg(unix)]
fn semantic_hits(session: &mut McpSession, query: &str) -> Vec<Value> {
    session.wait_ready("search");
    let reply = session
        .call("search", serde_json::json!({"action":"search_code","query":query,"limit":50}));
    assert!(reply["error"].is_null(), "semantic query succeeds: {reply}");
    reply["result"]["structuredContent"]["hits"]
        .as_array()
        .unwrap_or_else(|| panic!("semantic search returned no hits array: {reply}"))
        .clone()
}

#[cfg(unix)]
fn assert_semantic_scope(hits: &[Value], own_root: &str, foreign_root: &str) {
    assert!(!hits.is_empty(), "native semantic query returns hits");
    let semantic = hits.iter().filter(|hit| hit["modality"] == "S").collect::<Vec<_>>();
    assert!(
        semantic.iter().any(|hit| hit["root_id"] == own_root),
        "semantic-only results positively identify {own_root}: {hits:?}"
    );
    assert!(
        hits.iter().all(|hit| hit["root_id"] != foreign_root),
        "no lexical or semantic result carries foreign root {foreign_root}: {hits:?}"
    );
}

#[cfg(unix)]
fn wait_for_semantic_scope(
    session: &mut McpSession,
    query: &str,
    own_root: &str,
    foreign_root: &str,
) -> Vec<Value> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let hits = semantic_hits(session, query);
        if hits.iter().any(|hit| hit["modality"] == "S" && hit["root_id"] == own_root) {
            assert_semantic_scope(&hits, own_root, foreign_root);
            return hits;
        }
        assert!(
            Instant::now() < deadline,
            "{own_root} semantic results never became ready; lexical hits cannot satisfy this check: {hits:?}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(unix)]
fn compact_hits(hits: &[Value]) -> Vec<Value> {
    hits.iter()
        .map(|hit| {
            serde_json::json!({
                "modality": hit["modality"],
                "root_id": hit["root_id"],
                "path": hit["path"],
                "rank": hit["rank"],
            })
        })
        .collect()
}

#[cfg(unix)]
fn durable_cache_snapshot(layout: &mcp_server::WorkspaceCacheLayout) -> TreeSnapshot {
    let mut snapshot = tree_snapshot(layout.root());
    snapshot.entries.retain(|path, _| path != "writer.lease");
    let encoded = serde_json::to_vec(&snapshot.entries).unwrap();
    snapshot.fingerprint = blake3::hash(&encoded).to_hex().to_string();
    snapshot
}

#[cfg(unix)]
fn logical_cache_inventory(layout: &mcp_server::WorkspaceCacheLayout) -> Value {
    let graph = mcp_server::read_sqlite_method_call_digest(&layout.graph_db_path())
        .expect("read the published graph's logical method-call inventory");
    let store = bsl_search::Store::open_existing(&layout.search_db_path())
        .expect("open the existing workspace search store");
    let documents = store
        .load_indexed_documents(Some("code"))
        .expect("read the published workspace search inventory")
        .into_iter()
        .map(|document| {
            serde_json::json!({
                "root_id": document.root_id,
                "path": document.path,
                "symbol": document.symbol_name,
                "kind": document.kind,
                "line_start": document.line_start,
                "line_end": document.line_end,
                "content_hash": document.content_hash,
                "graph_context_hash": document.graph_context
                    .map(|context| blake3::hash(context.as_bytes()).to_hex().to_string()),
                "source_span": document.source_span,
            })
        })
        .collect::<Vec<_>>();
    serde_json::json!({
        "graph_method_call_rows": graph.rows(),
        "search_documents": documents,
    })
}

#[cfg(unix)]
fn assert_same_lease_owner(before: &Value, after: &Value, phase: &str) {
    let mut before = before.clone();
    let mut after = after.clone();
    before.as_object_mut().unwrap().remove("heartbeat_secs");
    after.as_object_mut().unwrap().remove("heartbeat_secs");
    assert_eq!(before, after, "{phase} preserves all non-heartbeat lease content");
}

#[cfg(unix)]
fn wait_for_file(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !path.is_file() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(path.is_file(), "expected native cache artifact {}", path.display());
}

#[cfg(unix)]
fn wait_for_embedding(stub: &EmbeddingStub, extension: &str) {
    let count = if extension == "EXT_A" { &stub.seen_a } else { &stub.seen_b };
    let deadline = Instant::now() + Duration::from_secs(30);
    while count.load(std::sync::atomic::Ordering::Relaxed) == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        count.load(std::sync::atomic::Ordering::Relaxed) > 0,
        "the {extension} source marker reaches the deterministic embedding endpoint"
    );
}

#[cfg(unix)]
fn file_hash(path: &Path) -> String {
    blake3::hash(&std::fs::read(path).unwrap()).to_hex().to_string()
}

#[cfg(unix)]
fn http_exchange(
    address: SocketAddr,
    body: &Value,
    session_id: Option<&str>,
) -> (u16, String, Option<Value>) {
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(2)).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let body = body.to_string();
    let session = session_id.map(|id| format!("Mcp-Session-Id: {id}\r\n")).unwrap_or_default();
    use std::io::Write as _;
    write!(
        stream,
        "POST /mcp HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\n{session}Content-Length: {}\r\nConnection: close\r\n\r\n{}",
        address,
        body.len(),
        body
    )
    .unwrap();
    let mut response = Vec::new();
    std::io::Read::read_to_end(&mut stream, &mut response).unwrap();
    let response = String::from_utf8(response).unwrap();
    let (headers, wire_body) = response.split_once("\r\n\r\n").expect("HTTP response headers");
    let status = headers
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse().ok())
        .expect("HTTP status code");
    let session_id = headers.lines().find_map(|line| {
        line.split_once(':')
            .filter(|(name, _)| name.eq_ignore_ascii_case("mcp-session-id"))
            .map(|(_, value)| value.trim().to_owned())
    });
    let body = if headers.to_ascii_lowercase().contains("transfer-encoding: chunked") {
        let mut decoded = String::new();
        let mut rest = wire_body;
        loop {
            let (size, after_size) = rest.split_once("\r\n").expect("HTTP chunk size line");
            let size = usize::from_str_radix(size.split(';').next().unwrap().trim(), 16)
                .expect("valid HTTP chunk size");
            if size == 0 {
                break;
            }
            let (chunk, after_chunk) = after_size.split_at(size);
            decoded.push_str(chunk);
            rest = after_chunk.strip_prefix("\r\n").expect("HTTP chunk terminator");
        }
        decoded
    } else {
        wire_body.to_owned()
    };
    let json = body.lines().find_map(|line| {
        let candidate = line.strip_prefix("data: ").unwrap_or(line).trim();
        candidate.starts_with('{').then(|| serde_json::from_str(candidate).ok()).flatten()
    });
    (status, session_id.unwrap_or_default(), json)
}

#[cfg(unix)]
fn http_rpc(
    address: SocketAddr,
    request: Value,
    session_id: Option<&str>,
) -> (String, Option<Value>) {
    let (status, next_session, response) = http_exchange(address, &request, session_id);
    assert!(
        (200..300).contains(&status),
        "HTTP MCP request {} returned {status}: {response:?}",
        request["method"]
    );
    (
        if next_session.is_empty() {
            session_id.unwrap_or_default().to_owned()
        } else {
            next_session
        },
        response,
    )
}

#[cfg(unix)]
fn http_call(address: SocketAddr, session_id: &str, id: u64, tool: &str, args: Value) -> Value {
    let request = serde_json::json!({
        "jsonrpc": "2.0", "id": id, "method": "tools/call",
        "params": {"name": tool, "arguments": args}
    });
    let (_, response) = http_rpc(address, request, Some(session_id));
    let response = response.expect("HTTP MCP tool response");
    assert_eq!(response["id"], id, "HTTP MCP response matches its request: {response}");
    assert!(response["error"].is_null(), "HTTP MCP call succeeded: {response}");
    response["result"]["structuredContent"].clone()
}

#[cfg(unix)]
fn start_http_scope_session(
    root: &Path,
    flags: &[&str],
    cache: &Path,
    state: &Path,
) -> (Child, StderrCapture, SocketAddr, String, Value, Vec<String>) {
    let reservation = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = reservation.local_addr().unwrap();
    drop(reservation);
    let mut command = Command::new(env!("CARGO_BIN_EXE_bsl-analyzer-app"));
    command
        .args([
            "mcp",
            "serve",
            "--profile",
            "workspace",
            "--mode",
            "http",
            "--host",
            "127.0.0.1",
            "--port",
        ])
        .arg(address.port().to_string())
        .args(["-s"])
        .arg(root)
        .args(flags)
        .arg("--cache-dir")
        .arg(cache)
        .env("BSL_MCP_BROKER", "0")
        .env("XDG_CACHE_HOME", state.join("cache"))
        .env("XDG_STATE_HOME", state)
        .env("XDG_RUNTIME_DIR", state.join("runtime"))
        .env_remove("BSL_CACHE_DIR")
        .env_remove("EMBEDDING_URL")
        .env_remove("EMBEDDING_MODEL")
        .env_remove("EMBEDDING_DIM")
        .env_remove("EMBEDDING_API_KEY")
        .env_remove("EMBEDDING_PROVIDER")
        .env_remove("EMBEDDING_QUERY_PREFIX")
        .env_remove("EMBEDDING_DOCUMENT_PREFIX")
        .env_remove("BSL_LOG_FILE")
        .env("BSL_LOG", "info")
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("native HTTP scope fixture starts");
    let mut stderr = StderrCapture::attach(child.stderr.take().unwrap());
    let health_deadline = Instant::now() + Duration::from_secs(30);
    while !http_health(address) && Instant::now() < health_deadline {
        if let Some(status) = child.try_wait().unwrap() {
            stderr.join();
            panic!("native HTTP scope fixture exited before ready: {status}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    if !http_health(address) {
        stop_owned_daemon_after_timeout(child.id());
        let _ = child.wait();
        stderr.join();
        panic!("native HTTP scope fixture did not become ready within 30 seconds");
    }

    let initialize = serde_json::json!({
        "jsonrpc":"2.0", "id":1, "method":"initialize", "params":{
            "protocolVersion":"2025-06-18", "capabilities":{},
            "clientInfo":{"name":"workspace-cache-scope-drift","version":"1"}
        }
    });
    let (status, session_id, initialized) = http_exchange(address, &initialize, None);
    assert_eq!(status, 200, "HTTP scope fixture initializes: {initialized:?}");
    assert!(!session_id.is_empty());
    let (status, _, _) = http_exchange(
        address,
        &serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        Some(&session_id),
    );
    assert!((200..300).contains(&status));

    let graph_deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let status =
            http_call(address, &session_id, 2, "graph", serde_json::json!({"action":"status"}));
        if status["state"] == "ready" {
            break;
        }
        assert_ne!(status["state"], "failed", "HTTP scope graph failed: {status}");
        assert!(Instant::now() < graph_deadline, "HTTP scope graph becomes ready");
        std::thread::sleep(Duration::from_millis(100));
    }
    let graph =
        http_call(address, &session_id, 3, "graph", serde_json::json!({"action":"overview"}))
            ["result"]
            .clone();
    let search_deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let status =
            http_call(address, &session_id, 4, "search", serde_json::json!({"action":"status"}));
        if status["state"] == "ready" {
            break;
        }
        assert_ne!(status["state"], "failed", "HTTP scope search failed: {status}");
        assert!(Instant::now() < search_deadline, "HTTP scope search becomes ready");
        std::thread::sleep(Duration::from_millis(100));
    }
    let search = http_call(
        address,
        &session_id,
        5,
        "search",
        serde_json::json!({"action":"search_code","query":"CACHE_LEXICAL_A_235","limit":20}),
    );
    let markers = search["hits"]
        .as_array()
        .unwrap_or_else(|| panic!("HTTP scope search returns hits: {search}"))
        .iter()
        .filter_map(|hit| hit["path"].as_str().map(str::to_owned))
        .collect();
    (child, stderr, address, session_id, graph, markers)
}

#[cfg(unix)]
fn http_health(address: SocketAddr) -> bool {
    let Ok(mut stream) = TcpStream::connect_timeout(&address, Duration::from_millis(200)) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(300)));
    use std::io::Write as _;
    if write!(stream, "GET /health HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n")
        .is_err()
    {
        return false;
    }
    let mut bytes = [0u8; 256];
    let Ok(count) = std::io::Read::read(&mut stream, &mut bytes) else { return false };
    String::from_utf8_lossy(&bytes[..count]).starts_with("HTTP/1.1 200")
}

#[cfg(unix)]
fn start_scope(
    root: &Path,
    extension: &str,
    cache: &Path,
    state: &Path,
    embedding_url: &str,
) -> McpSession {
    let flags = composition_flags(extension);
    let flags: Vec<_> = flags.iter().map(String::as_str).collect();
    McpSession::start_with_cache(root, &flags, Some(cache), Some(state), Some(embedding_url))
}

fn graph_overview(session: &mut McpSession) -> Value {
    session.wait_ready("graph");
    let reply = session.call("graph", serde_json::json!({"action": "overview"}));
    assert!(reply["error"].is_null(), "graph overview failed: {reply}");
    reply["result"]["structuredContent"]["result"].clone()
}

#[cfg(unix)]
fn wait_for_graph_nodes(session: &mut McpSession, expected: u64) -> Value {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let graph = graph_overview(session);
        if graph["nodes"].as_u64() == Some(expected) {
            return graph;
        }
        assert!(
            Instant::now() < deadline,
            "graph reaches {expected} nodes before the bounded deadline: {graph}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn wait_for_marker(session: &mut McpSession, marker: &str) -> Vec<String> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let hits = search_marker(session, marker);
        if !hits.is_empty() || Instant::now() >= deadline {
            return hits;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn assert_leaf_artifacts(layout: &mcp_server::WorkspaceCacheLayout) {
    assert!(
        layout.graph_db_path().is_file(),
        "graph database exists: {}",
        layout.graph_db_path().display()
    );
    assert!(
        layout.search_db_path().is_file(),
        "search database exists: {}",
        layout.search_db_path().display()
    );
    assert!(
        layout.lease_path().is_file(),
        "writer lease exists: {}",
        layout.lease_path().display()
    );
}

#[cfg(unix)]
fn assert_vector_artifacts(layout: &mcp_server::WorkspaceCacheLayout) {
    wait_for_file(&PathBuf::from(format!("{}.usearch", layout.search_db_path().display())));
    wait_for_file(&PathBuf::from(format!("{}.usearch.json", layout.search_db_path().display())));
    let artifacts = tree_snapshot(layout.root());
    assert!(artifacts.entries.contains_key("bsl-search.db.usearch"));
    assert!(artifacts.entries.contains_key("bsl-search.db.usearch.json"));
}

#[cfg(unix)]
#[test]
fn workspace_cache_scope_two_compositions_isolate_native_stores_and_warm_restarts() {
    let root = workspace();
    source_composition(root.path());
    let initial = tree_snapshot(root.path());
    let legacy = tree_snapshot(&root.path().join(".build"));
    let cache = tempfile::tempdir().unwrap();
    let state_a = tempfile::tempdir().unwrap();
    let state_b = tempfile::tempdir().unwrap();
    let state_a_restart = tempfile::tempdir().unwrap();
    let state_b_restart = tempfile::tempdir().unwrap();
    let state_a_final = tempfile::tempdir().unwrap();
    let embedder = EmbeddingStub::start();
    let layout_a = cache_layout(root.path(), "EXT_A", Some(cache.path()));
    let layout_b = cache_layout(root.path(), "EXT_B", Some(cache.path()));
    assert_ne!(
        layout_a.root(),
        layout_b.root(),
        "different source compositions have different leaves"
    );
    assert!(layout_a.root().starts_with(cache.path().join("workspaces/v1")));
    assert!(layout_b.root().starts_with(cache.path().join("workspaces/v1")));
    assert!(!layout_a.root().starts_with(root.path()) && !layout_b.root().starts_with(root.path()));

    let mut a = start_scope(root.path(), "EXT_A", cache.path(), state_a.path(), &embedder.url);
    let graph_a = graph_overview(&mut a);
    assert_eq!(graph_a["nodes"], 2, "A graph contains main plus its extension: {graph_a}");
    let marker_a = wait_for_marker(&mut a, "CACHE_LEXICAL_A_235");
    assert!(!marker_a.is_empty(), "A lexical marker indexed in A: {marker_a:?}");
    assert_search_marker_root(&mut a, "CACHE_LEXICAL_A_235", "a/b/ext-a");
    wait_for_embedding(&embedder, "EXT_A");
    assert_leaf_artifacts(&layout_a);
    let lease_a = lease_receipt(&layout_a.lease_path(), a.pid());
    assert_tree_unchanged(&initial, root.path(), "while composition A holds open stores");
    assert_eq!(
        tree_snapshot(&root.path().join(".build")),
        legacy,
        "legacy flat cache remains byte-for-byte unchanged"
    );

    let mut b = start_scope(root.path(), "EXT_B", cache.path(), state_b.path(), &embedder.url);
    let graph_b = graph_overview(&mut b);
    assert_eq!(graph_b["nodes"], 2, "B graph contains main plus its extension: {graph_b}");
    let marker_b = wait_for_marker(&mut b, "CACHE_LEXICAL_B_235");
    assert!(!marker_b.is_empty(), "B lexical marker indexed in B: {marker_b:?}");
    assert_search_marker_root(&mut b, "CACHE_LEXICAL_B_235", "a/b/ext-b");
    wait_for_embedding(&embedder, "EXT_B");
    assert_search_excludes_root(&mut a, "CACHE_LEXICAL_B_235", "a/b/ext-b");
    assert_search_excludes_root(&mut b, "CACHE_LEXICAL_A_235", "a/b/ext-a");
    let vector_hits_a =
        wait_for_semantic_scope(&mut a, "orange quiet river", "a/b/ext-a", "a/b/ext-b");
    let vector_hits_b =
        wait_for_semantic_scope(&mut b, "violet steady mountain", "a/b/ext-b", "a/b/ext-a");
    assert_vector_artifacts(&layout_a);
    assert_vector_artifacts(&layout_b);
    assert_leaf_artifacts(&layout_b);
    let lease_b = lease_receipt(&layout_b.lease_path(), b.pid());
    assert_ne!(
        lease_a["token"], lease_b["token"],
        "simultaneous composition leases are independent"
    );
    let vector_hash_a =
        file_hash(&PathBuf::from(format!("{}.usearch", layout_a.search_db_path().display())));
    let vector_hash_b =
        file_hash(&PathBuf::from(format!("{}.usearch", layout_b.search_db_path().display())));
    assert_ne!(vector_hash_a, vector_hash_b, "composition-specific vector artifacts differ");
    assert!(a.is_running() && b.is_running(), "both stdio processes remain alive concurrently");
    assert_tree_unchanged(
        &initial,
        root.path(),
        "while both composition stores and leases are open",
    );

    let module_a = root.path().join("a/b/ext-a/CommonModules/МодульРасширенияА/Ext/Module.bsl");
    let mut body_a = std::fs::read_to_string(&module_a).unwrap();
    body_a.push_str("\nФункция CacheFreshA235() Экспорт\n\tВозврат \"CACHE_LEXICAL_A_REFRESHED_235\";\nКонецФункции\n");
    std::fs::write(&module_a, body_a).unwrap();
    let edited = tree_snapshot(root.path());
    let fresh_marker = wait_for_marker(&mut a, "CACHE_LEXICAL_A_REFRESHED_235");
    assert!(
        !fresh_marker.is_empty(),
        "ordinary body edit becomes searchable without restart: {fresh_marker:?}"
    );
    let graph_after_edit = wait_for_graph_nodes(&mut a, 3);
    assert!(
        a.is_running() && a.pid() == lease_a["pid"].as_u64().unwrap() as u32,
        "body edit keeps the original process alive"
    );
    assert_eq!(
        layout_a.root(),
        cache_layout(root.path(), "EXT_A", Some(cache.path())).root(),
        "body edit leaves the namespace unchanged"
    );
    assert_tree_unchanged(
        &edited,
        root.path(),
        "after the intentional body edit and during both sessions",
    );
    assert_eq!(
        tree_snapshot(&root.path().join(".build")),
        legacy,
        "legacy flat cache remains unchanged after source edit"
    );

    let exit_a = a.shutdown().unwrap();
    assert!(exit_a.success(), "A original process exits cleanly");
    assert!(!layout_a.lease_path().exists(), "A original lease releases before its warm restart");
    assert!(b.is_running(), "B stays live across the A warm restart");
    let lease_b_before_a_restart = lease_receipt(&layout_b.lease_path(), b.pid());
    assert_same_lease_owner(&lease_b, &lease_b_before_a_restart, "before A warm restart");
    let cache_b_before_a_restart = durable_cache_snapshot(&layout_b);

    let mut a_restart =
        start_scope(root.path(), "EXT_A", cache.path(), state_a_restart.path(), &embedder.url);
    assert_eq!(
        graph_overview(&mut a_restart)["nodes"],
        graph_after_edit["nodes"],
        "A warm restart keeps its updated graph"
    );
    assert!(!wait_for_marker(&mut a_restart, "CACHE_LEXICAL_A_REFRESHED_235").is_empty());
    assert_search_marker_root(&mut a_restart, "CACHE_LEXICAL_A_REFRESHED_235", "a/b/ext-a");
    assert_search_excludes_root(&mut a_restart, "CACHE_LEXICAL_B_235", "a/b/ext-b");
    let lease_a_restart = lease_receipt(&layout_a.lease_path(), a_restart.pid());
    assert_ne!(lease_a["token"], lease_a_restart["token"]);
    let vector_hits_a_restart =
        wait_for_semantic_scope(&mut a_restart, "orange quiet river", "a/b/ext-a", "a/b/ext-b");
    let a_restart_reuse = a_restart.cache_adoption_evidence();
    assert!(
        !a_restart_reuse.is_empty(),
        "A warm restart must publish through native cache adoption; stderr: {}",
        a_restart.stderr_snapshot()
    );
    let lease_b_after_a_restart = lease_receipt(&layout_b.lease_path(), b.pid());
    assert_same_lease_owner(
        &lease_b_before_a_restart,
        &lease_b_after_a_restart,
        "while A warm restart runs beside B",
    );
    assert_eq!(
        durable_cache_snapshot(&layout_b),
        cache_b_before_a_restart,
        "A restart does not change B's graph/search/vector store contents"
    );
    assert!(b.is_running(), "B original process remains live after A restart");
    assert_leaf_artifacts(&layout_a);
    let exit_b = b.shutdown().unwrap();
    assert!(exit_b.success(), "B original process exits cleanly");
    assert!(!layout_b.lease_path().exists(), "B original lease releases before its warm restart");
    assert!(a_restart.is_running(), "A restart stays live across the B warm restart");
    let lease_a_before_b_restart = lease_receipt(&layout_a.lease_path(), a_restart.pid());
    assert_same_lease_owner(&lease_a_restart, &lease_a_before_b_restart, "before B warm restart");
    let cache_a_before_b_restart = durable_cache_snapshot(&layout_a);

    let mut b_restart =
        start_scope(root.path(), "EXT_B", cache.path(), state_b_restart.path(), &embedder.url);
    assert_eq!(
        graph_overview(&mut b_restart)["nodes"],
        graph_b["nodes"],
        "B warm restart retains its graph"
    );
    assert!(!wait_for_marker(&mut b_restart, "CACHE_LEXICAL_B_235").is_empty());
    assert_search_marker_root(&mut b_restart, "CACHE_LEXICAL_B_235", "a/b/ext-b");
    assert_search_excludes_root(&mut b_restart, "CACHE_LEXICAL_A_REFRESHED_235", "a/b/ext-a");
    let lease_b_restart = lease_receipt(&layout_b.lease_path(), b_restart.pid());
    assert_ne!(lease_b["token"], lease_b_restart["token"]);
    let vector_hits_b_restart =
        wait_for_semantic_scope(&mut b_restart, "violet steady mountain", "a/b/ext-b", "a/b/ext-a");
    let b_restart_reuse = b_restart.cache_adoption_evidence();
    assert!(
        !b_restart_reuse.is_empty(),
        "B warm restart must publish through native cache adoption; stderr: {}",
        b_restart.stderr_snapshot()
    );
    let lease_a_after_b_restart = lease_receipt(&layout_a.lease_path(), a_restart.pid());
    assert_same_lease_owner(
        &lease_a_before_b_restart,
        &lease_a_after_b_restart,
        "while B warm restart runs beside A",
    );
    assert_eq!(
        durable_cache_snapshot(&layout_a),
        cache_a_before_b_restart,
        "B restart does not change A's graph/search/vector store contents"
    );
    assert!(a_restart.is_running(), "A restart remains live after B restart");
    assert_leaf_artifacts(&layout_b);

    let exit_a_restart = a_restart.shutdown().unwrap();
    assert!(exit_a_restart.success(), "A warm process exits cleanly");
    assert!(!layout_a.lease_path().exists(), "A warm lease releases before A final restart");
    assert!(b_restart.is_running(), "B restart remains live across A final restart");
    let lease_b_before_a_final = lease_receipt(&layout_b.lease_path(), b_restart.pid());
    assert_same_lease_owner(&lease_b_restart, &lease_b_before_a_final, "before A final restart");
    let cache_b_before_a_final = durable_cache_snapshot(&layout_b);

    let mut a_final =
        start_scope(root.path(), "EXT_A", cache.path(), state_a_final.path(), &embedder.url);
    assert_eq!(
        graph_overview(&mut a_final)["nodes"],
        graph_after_edit["nodes"],
        "A after B reuses its own updated graph"
    );
    assert!(!wait_for_marker(&mut a_final, "CACHE_LEXICAL_A_REFRESHED_235").is_empty());
    assert_search_marker_root(&mut a_final, "CACHE_LEXICAL_A_REFRESHED_235", "a/b/ext-a");
    assert_search_excludes_root(&mut a_final, "CACHE_LEXICAL_B_235", "a/b/ext-b");
    let vector_hits_a_final =
        wait_for_semantic_scope(&mut a_final, "orange quiet river", "a/b/ext-a", "a/b/ext-b");
    let a_final_reuse = a_final.cache_adoption_evidence();
    assert!(
        !a_final_reuse.is_empty(),
        "A after B must publish through native cache adoption; stderr: {}",
        a_final.stderr_snapshot()
    );
    let lease_a_final = lease_receipt(&layout_a.lease_path(), a_final.pid());
    assert_ne!(
        lease_a["token"], lease_a_final["token"],
        "A reacquires its released lease with a new claim token"
    );
    let lease_b_after_a_final = lease_receipt(&layout_b.lease_path(), b_restart.pid());
    assert_same_lease_owner(
        &lease_b_before_a_final,
        &lease_b_after_a_final,
        "while A final restart runs beside B",
    );
    assert_eq!(
        durable_cache_snapshot(&layout_b),
        cache_b_before_a_final,
        "A final restart leaves B's graph/search/vector stores unchanged"
    );
    let exit_b_restart = b_restart.shutdown().unwrap();
    assert!(exit_b_restart.success(), "B warm process exits cleanly");
    assert!(!layout_b.lease_path().exists(), "B warm lease releases after owner shutdown");
    let exit_a_final = a_final.shutdown().unwrap();
    assert!(exit_a_final.success());
    let after = tree_snapshot(root.path());
    assert_eq!(after, edited, "the only source change is the intentional BSL body edit");
    assert_eq!(
        tree_snapshot(&root.path().join(".build")),
        legacy,
        "legacy flat cache canaries remain untouched"
    );

    let report = serde_json::json!({
        "case": "workspace_cache_scope_two_compositions",
        "os": std::env::consts::OS,
        "mode": "stdio",
        "source_before": snapshot_report(&initial),
        "source_during_both_live": snapshot_report(&initial),
        "source_after_intentional_edit": snapshot_report(&edited),
        "source_after_shutdown": snapshot_report(&after),
        "cache_base": cache.path(),
        "cache_origin": "explicit-environment-base",
        "composition_a": {"leaf": layout_a.root(), "scope": layout_a.scope_stamp(), "graph": graph_a,
            "search_marker": marker_a, "semantic_vector_hits": compact_hits(&vector_hits_a),
            "semantic_hits_after_warm_restart": compact_hits(&vector_hits_a_restart),
            "semantic_hits_after_final_restart": compact_hits(&vector_hits_a_final),
            "cache_adoption_log_after_warm_restart": a_restart_reuse,
            "cache_adoption_log_after_final_restart": a_final_reuse,
            "vector_index_hash": vector_hash_a,
            "vector_requests": embedder.seen_a.load(std::sync::atomic::Ordering::Relaxed),
            "lease_initial": lease_a, "lease_after_a_restart": lease_a_restart,
            "lease_stable_during_b_restart": lease_a_after_b_restart,
            "lease_after_a_b_a": lease_a_final,
            "cache_stable_during_b_restart": cache_a_before_b_restart,
            "cache_snapshot": snapshot_report(&tree_snapshot(layout_a.root()))},
        "composition_b": {"leaf": layout_b.root(), "scope": layout_b.scope_stamp(), "graph": graph_b,
            "search_marker": marker_b, "semantic_vector_hits": compact_hits(&vector_hits_b),
            "semantic_hits_after_warm_restart": compact_hits(&vector_hits_b_restart),
            "cache_adoption_log_after_warm_restart": b_restart_reuse,
            "vector_index_hash": vector_hash_b,
            "vector_requests": embedder.seen_b.load(std::sync::atomic::Ordering::Relaxed),
            "lease_initial": lease_b, "lease_stable_during_a_restart": lease_b_after_a_restart,
            "lease_after_b_restart": lease_b_restart,
            "lease_stable_during_a_final": lease_b_after_a_final,
            "cache_stable_during_a_restart": cache_b_before_a_restart,
            "cache_stable_during_a_final": cache_b_before_a_final,
            "cache_snapshot": snapshot_report(&tree_snapshot(layout_b.root()))},
        "legacy_flat_snapshot": snapshot_report(&legacy),
        "children_exit_codes": [exit_a.code(), exit_b.code(), exit_a_restart.code(), exit_b_restart.code(), exit_a_final.code()],
    });
    println!("BA235_NATIVE_RECEIPT={report}");
}

#[cfg(target_os = "linux")]
#[test]
fn workspace_cache_scope_linux_default_stdio_and_read_only_cli_stay_outside_sources() {
    let root = workspace();
    source_composition(root.path());
    let before = tree_snapshot(root.path());
    let legacy = tree_snapshot(&root.path().join(".build"));
    let paths = tempfile::tempdir().unwrap();
    let cache_home = paths.path().join("xdg-cache");
    let state = paths.path().join("xdg-state");
    let restart_state = paths.path().join("restart-state");
    assert_private_test_dir(&cache_home);
    assert_private_test_dir(&state);
    assert_private_test_dir(&restart_state);
    let expected_base = cache_home.join("bsl-analyzer");
    let layout = cache_layout(root.path(), "EXT_A", Some(&expected_base));
    let flags_owned = composition_flags("EXT_A");
    let flags: Vec<_> = flags_owned.iter().map(String::as_str).collect();

    let mut session = McpSession::start_transport(
        root.path(),
        &flags,
        "stdio",
        None,
        Some(&cache_home),
        Some(&state),
        None,
        Some("0"),
        None,
    );
    let graph = graph_overview(&mut session);
    assert_eq!(graph["nodes"], 2, "Linux OS-default cache serves the graph: {graph}");
    let marker = wait_for_marker(&mut session, "CACHE_LEXICAL_A_235");
    assert!(!marker.is_empty(), "default-path search returns the extension marker");
    assert_leaf_artifacts(&layout);
    let lease = lease_receipt(&layout.lease_path(), session.pid());
    assert_tree_unchanged(&before, root.path(), "during Linux default-path stdio service");
    assert_eq!(tree_snapshot(&root.path().join(".build")), legacy);
    let first_exit = session.shutdown().unwrap();
    assert!(first_exit.success(), "default-path stdio shuts down: {first_exit}");

    let mut warm = McpSession::start_transport(
        root.path(),
        &flags,
        "stdio",
        None,
        Some(&cache_home),
        Some(&restart_state),
        None,
        Some("0"),
        None,
    );
    assert_eq!(graph_overview(&mut warm)["nodes"], graph["nodes"]);
    assert!(!wait_for_marker(&mut warm, "CACHE_LEXICAL_A_235").is_empty());
    let warm_cache_adoption = warm.cache_adoption_evidence();
    assert!(
        !warm_cache_adoption.is_empty(),
        "Linux default-path warm restart must use native cache adoption; stderr: {}",
        warm.stderr_snapshot()
    );
    let warm_exit = warm.shutdown().unwrap();
    assert!(warm_exit.success(), "warm default-path stdio shuts down: {warm_exit}");

    let read_only_source = if unsafe { libc::geteuid() } == 0 {
        serde_json::json!({"status": "skipped-root", "reason": "chmod permissions do not constrain root"})
    } else {
        let read_only_cache_home = paths.path().join("readonly-source-cache");
        let read_only_state = paths.path().join("readonly-source-state");
        assert_private_test_dir(&read_only_cache_home);
        assert_private_test_dir(&read_only_state);
        let read_only_layout =
            cache_layout(root.path(), "EXT_A", Some(&read_only_cache_home.join("bsl-analyzer")));
        let source_permissions = make_source_tree_read_only(root.path());
        let source_read_only_before = tree_snapshot(root.path());
        assert!(
            source_read_only_before
                .entries
                .values()
                .filter(|entry| entry.kind != "symlink")
                .all(|entry| entry.permissions & 0o222 == 0),
            "native source tree has no write permission bits"
        );
        let mut read_only_session = McpSession::start_transport(
            root.path(),
            &flags,
            "stdio",
            None,
            Some(&read_only_cache_home),
            Some(&read_only_state),
            None,
            Some("0"),
            None,
        );
        let read_only_graph = graph_overview(&mut read_only_session);
        assert_eq!(read_only_graph["nodes"], 2, "MCP graph builds from read-only sources");
        let read_only_marker = wait_for_marker(&mut read_only_session, "CACHE_LEXICAL_A_235");
        assert!(!read_only_marker.is_empty(), "MCP search reads the read-only source marker");
        assert!(
            read_only_layout.root().starts_with(&read_only_cache_home),
            "read-only source runtime uses the external writable cache"
        );
        let source_read_only_during = tree_snapshot(root.path());
        assert_eq!(source_read_only_during, source_read_only_before);
        assert_leaf_artifacts(&read_only_layout);
        assert!(read_only_session.shutdown().unwrap().success());
        let source_read_only_after = tree_snapshot(root.path());
        assert_eq!(source_read_only_after, source_read_only_before);
        drop(source_permissions);
        assert_eq!(
            tree_snapshot(root.path()),
            before,
            "source permissions restore for temp cleanup"
        );
        serde_json::json!({
            "status": "passed",
            "source_before": snapshot_report(&source_read_only_before),
            "source_during": snapshot_report(&source_read_only_during),
            "source_after_shutdown": snapshot_report(&source_read_only_after),
            "external_cache_base": read_only_layout.base(),
            "cache_leaf": read_only_layout.root(),
            "graph": read_only_graph,
            "search_marker": read_only_marker,
        })
    };

    let read_only_state = paths.path().join("read-only-state");
    let read_only_cache = paths.path().join("read-only-cache");
    assert_private_test_dir(&read_only_state);
    assert_private_test_dir(&read_only_cache);
    let read_only_state_before = tree_snapshot(&read_only_state);
    let read_only_cache_before = tree_snapshot(&read_only_cache);
    assert_eq!(read_only_state_before.entries.len(), 1, "read-only state starts empty");
    assert_eq!(read_only_cache_before.entries.len(), 1, "read-only cache starts empty");
    let read_only = Command::new(env!("CARGO_BIN_EXE_bsl-analyzer-app"))
        .args(["analyze", "-s"])
        .arg(root.path())
        .args(&flags)
        .args(["--format", "jsonl"])
        .current_dir(root.path())
        .env("XDG_CACHE_HOME", &read_only_cache)
        .env("XDG_STATE_HOME", &read_only_state)
        .env_remove("BSL_CACHE_DIR")
        .env_remove("EMBEDDING_URL")
        .env_remove("EMBEDDING_MODEL")
        .env_remove("EMBEDDING_DIM")
        .env_remove("EMBEDDING_API_KEY")
        .env_remove("EMBEDDING_PROVIDER")
        .output()
        .expect("native read-only analyze command starts");
    assert!(
        read_only.status.success(),
        "analyze is a read-only CLI path: {}",
        String::from_utf8_lossy(&read_only.stderr)
    );
    let after = tree_snapshot(root.path());
    assert_eq!(after, before, "read-only analyze creates no source artifacts");
    assert_eq!(tree_snapshot(&root.path().join(".build")), legacy);
    assert!(
        tree_snapshot(&read_only_cache) == read_only_cache_before
            && tree_snapshot(&read_only_state) == read_only_state_before,
        "read-only analyze creates no cache/state descendants"
    );

    let report = serde_json::json!({
        "case": "workspace_cache_scope_linux_default_stdio_and_read_only_cli",
        "os": std::env::consts::OS,
        "mode": "stdio",
        "source_before": snapshot_report(&before),
        "source_during_stdio": snapshot_report(&before),
        "source_after_read_only_cli": snapshot_report(&after),
        "cache_origin": "linux-xdg-default",
        "cache_base": layout.base(),
        "cache_leaf": layout.root(),
        "scope": layout.scope_stamp(),
        "cache_snapshot": snapshot_report(&tree_snapshot(layout.root())),
        "graph": graph,
        "read_only_source_runtime": read_only_source,
        "search_marker": marker,
        "warm_cache_adoption_log": warm_cache_adoption,
        "lease": lease,
        "cold_native_exit": first_exit.code(),
        "warm_native_exit": warm_exit.code(),
        "legacy_flat_snapshot": snapshot_report(&legacy),
        "read_only_cli_exit": read_only.status.code(),
        "warm_restart_exit": warm_exit.code(),
    });
    println!("BA235_NATIVE_RECEIPT={report}");
}

#[cfg(unix)]
#[test]
fn workspace_cache_scope_explicit_workspace_build_namespace_preserves_flat_canaries() {
    let root = workspace();
    source_composition(root.path());
    let before = tree_snapshot(root.path());
    let legacy = tree_snapshot(&root.path().join(".build"));
    let paths = tempfile::tempdir().unwrap();
    let state = paths.path().join("state");
    assert_private_test_dir(&state);
    let cache_base = root.path().join(".build");
    let layout = cache_layout(root.path(), "EXT_A", Some(&cache_base));
    assert!(!layout.root().exists(), "explicit .build composition starts cold");
    let mut flags = composition_flags("EXT_A").to_vec();
    flags.extend(["--cache-dir".to_owned(), cache_base.to_string_lossy().into_owned()]);
    let flags: Vec<_> = flags.iter().map(String::as_str).collect();
    let embedder = EmbeddingStub::start();
    let mut session =
        McpSession::start_with_cache(root.path(), &flags, None, Some(&state), Some(&embedder.url));
    let graph = graph_overview(&mut session);
    assert_eq!(graph["nodes"], 2, "explicit .build cache builds the scoped graph: {graph}");
    let marker = wait_for_marker(&mut session, "CACHE_LEXICAL_A_235");
    assert!(!marker.is_empty(), "explicit .build cache serves the scoped search marker");
    wait_for_embedding(&embedder, "EXT_A");
    assert_leaf_artifacts(&layout);
    let lease = lease_receipt(&layout.lease_path(), session.pid());
    let during = assert_tree_unchanged_except_workspace_cache(
        &before,
        root.path(),
        layout.root(),
        "while explicit .build cache owns its namespace",
    );
    assert_flat_canaries_unchanged(&legacy, root.path());
    let status = session.shutdown().unwrap();
    assert!(status.success(), "explicit .build owner exits cleanly");
    assert!(!layout.lease_path().exists(), "explicit .build writer lease releases");
    let after = assert_tree_unchanged_except_workspace_cache(
        &before,
        root.path(),
        layout.root(),
        "after explicit .build cache shutdown",
    );
    assert_flat_canaries_unchanged(&legacy, root.path());
    let report = serde_json::json!({
        "case": "workspace_cache_scope_explicit_workspace_build_namespace",
        "os": std::env::consts::OS,
        "mode": "stdio",
        "cache_origin": "explicit-cli-base",
        "source_before": snapshot_report(&before),
        "source_during": snapshot_report(&during),
        "source_after": snapshot_report(&after),
        "cache_base": layout.base(),
        "cache_leaf": layout.root(),
        "scope": layout.scope_stamp(),
        "graph": graph,
        "search_marker": marker,
        "vector_index_hash": file_hash(&PathBuf::from(format!("{}.usearch", layout.search_db_path().display()))),
        "vector_requests": embedder.seen_a.load(std::sync::atomic::Ordering::Relaxed),
        "lease": lease,
        "native_exit": status.code(),
        "legacy_flat_canaries": snapshot_report(&legacy),
        "cache_snapshot": snapshot_report(&tree_snapshot(layout.root())),
    });
    println!("BA235_NATIVE_RECEIPT={report}");
}

#[cfg(unix)]
#[test]
fn workspace_cache_scope_http_cold_and_warm_runs_keep_sources_clean() {
    struct HttpScopeRun {
        graph: Value,
        search_markers: Vec<String>,
        pid: u32,
        source_during: TreeSnapshot,
        state_snapshot: TreeSnapshot,
        cache_after_shutdown: TreeSnapshot,
        lease: Value,
        exit_code: Option<i32>,
        lease_released: bool,
        cache_adoption: Vec<String>,
    }

    fn exercise(
        root: &Path,
        flags: &[&str],
        cache_home: &Path,
        state: &Path,
        expect_cache_adoption: bool,
    ) -> HttpScopeRun {
        let reservation = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = reservation.local_addr().unwrap();
        drop(reservation);
        let mut command = Command::new(env!("CARGO_BIN_EXE_bsl-analyzer-app"));
        command
            .args([
                "mcp",
                "serve",
                "--profile",
                "workspace",
                "--mode",
                "http",
                "--host",
                "127.0.0.1",
                "--port",
            ])
            .arg(address.port().to_string())
            .args(["-s"])
            .arg(root)
            .args(flags)
            .env("BSL_MCP_BROKER", "0")
            .env("XDG_CACHE_HOME", cache_home)
            .env("XDG_STATE_HOME", state)
            .env("XDG_RUNTIME_DIR", state.join("runtime"))
            .env_remove("BSL_CACHE_DIR")
            .env_remove("EMBEDDING_URL")
            .env_remove("EMBEDDING_MODEL")
            .env_remove("EMBEDDING_DIM")
            .env_remove("EMBEDDING_API_KEY")
            .env_remove("EMBEDDING_PROVIDER")
            .env_remove("EMBEDDING_QUERY_PREFIX")
            .env_remove("EMBEDDING_DOCUMENT_PREFIX")
            .env_remove("BSL_LOG_FILE")
            .env("BSL_LOG", "info")
            .current_dir(root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let mut child = command.spawn().expect("native HTTP MCP process starts");
        let mut stderr = StderrCapture::attach(child.stderr.take().unwrap());
        let deadline = Instant::now() + Duration::from_secs(30);
        while !http_health(address) && Instant::now() < deadline {
            if let Some(status) = child.try_wait().unwrap() {
                stderr.join();
                panic!(
                    "native HTTP process exited before health: {status}; stderr: {}",
                    stderr.snapshot()
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            http_health(address),
            "native HTTP /health becomes ready at {address}; stderr: {}",
            stderr.snapshot()
        );

        let initialize = serde_json::json!({
            "jsonrpc":"2.0", "id":1, "method":"initialize", "params":{
                "protocolVersion":"2025-06-18", "capabilities":{},
                "clientInfo":{"name":"workspace-cache-fixture","version":"1"}
            }
        });
        let (status, session_id, initialized) = http_exchange(address, &initialize, None);
        assert_eq!(status, 200, "HTTP initialize status: {initialized:?}");
        assert!(!session_id.is_empty(), "HTTP server assigns a session id");
        assert_eq!(initialized.as_ref().unwrap()["id"], 1);
        let (status, _, _) = http_exchange(
            address,
            &serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
            Some(&session_id),
        );
        assert!((200..300).contains(&status), "HTTP initialized notification succeeds");

        let ready_deadline = Instant::now() + Duration::from_secs(30);
        let mut graph_status = Value::Null;
        while Instant::now() < ready_deadline {
            graph_status =
                http_call(address, &session_id, 2, "graph", serde_json::json!({"action":"status"}));
            if graph_status["state"] == "ready" || graph_status["state"] == "failed" {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert_eq!(graph_status["state"], "ready", "HTTP graph is ready: {graph_status}");
        let graph =
            http_call(address, &session_id, 3, "graph", serde_json::json!({"action":"overview"}))
                ["result"]
                .clone();
        assert_eq!(graph["nodes"], 2, "HTTP graph sees the extension source: {graph}");

        let search_deadline = Instant::now() + Duration::from_secs(30);
        let mut search_status = Value::Null;
        while Instant::now() < search_deadline {
            search_status = http_call(
                address,
                &session_id,
                4,
                "search",
                serde_json::json!({"action":"status"}),
            );
            if search_status["state"] == "ready" || search_status["state"] == "failed" {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert_eq!(search_status["state"], "ready", "HTTP search is ready: {search_status}");
        let search = http_call(
            address,
            &session_id,
            5,
            "search",
            serde_json::json!({"action":"search_code","query":"CACHE_LEXICAL_A_235","limit":20}),
        );
        let markers = search["hits"]
            .as_array()
            .unwrap_or_else(|| panic!("HTTP search returns hits: {search}"))
            .iter()
            .filter_map(|hit| hit["path"].as_str().map(str::to_owned))
            .collect::<Vec<_>>();
        assert!(!markers.is_empty(), "HTTP search sees the source marker");

        let child_pid = child.id();
        let during = tree_snapshot(root);
        let state_snapshot = tree_snapshot(state);
        let cache_base = cache_home.join("bsl-analyzer");
        let layout = cache_layout(root, "EXT_A", Some(&cache_base));
        let lease = lease_receipt(&layout.lease_path(), child_pid);
        assert_leaf_artifacts(&layout);
        assert_tree_unchanged(&during, root, "while native HTTP process and stores are open");
        assert!(
            state_snapshot.entries.keys().any(|path| path.ends_with(".json")),
            "HTTP process record is isolated under the configured state directory"
        );
        assert!(
            state_snapshot.entries.keys().filter(|path| path.ends_with(".json")).any(|path| {
                std::fs::read(state.join(path))
                    .ok()
                    .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                    .is_some_and(|record| {
                        record["pid"].as_u64() == Some(u64::from(child_pid))
                            && record["state"] == "running"
                    })
            }),
            "HTTP process record names the live native child: pid={child_pid}"
        );
        let cache_adoption = if expect_cache_adoption {
            let evidence = stderr.wait_for_cache_adoption();
            assert!(
                !evidence.is_empty(),
                "HTTP warm restart must use native cache adoption; stderr: {}",
                stderr.snapshot()
            );
            evidence
        } else {
            stderr.cache_adoption_evidence()
        };
        let signal_result = unsafe { libc::kill(child_pid as libc::pid_t, libc::SIGTERM) };
        assert_eq!(signal_result, 0, "SIGTERM reaches the owned HTTP process {child_pid}");
        let deadline = Instant::now() + Duration::from_secs(10);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                stderr.join();
                panic!("HTTP child did not gracefully stop before the bounded deadline");
            }
            std::thread::sleep(Duration::from_millis(25));
        };
        stderr.join();
        assert!(status.success(), "HTTP child exits cleanly after SIGTERM: {status}");
        let lease_deadline = Instant::now() + Duration::from_secs(2);
        while layout.lease_path().exists() && Instant::now() < lease_deadline {
            std::thread::sleep(Duration::from_millis(25));
        }
        assert!(!layout.lease_path().exists(), "HTTP child releases its workspace lease");
        let cache_after_shutdown = tree_snapshot(layout.root());
        assert!(layout.graph_db_path().is_file(), "HTTP graph database remains after shutdown");
        assert!(layout.search_db_path().is_file(), "HTTP search database remains after shutdown");
        assert!(
            !cache_after_shutdown.entries.contains_key("writer.lease"),
            "HTTP cache snapshot after graceful exit has no writer lease"
        );
        HttpScopeRun {
            graph,
            search_markers: markers,
            pid: child_pid,
            source_during: during,
            state_snapshot,
            cache_after_shutdown,
            lease,
            exit_code: status.code(),
            lease_released: !layout.lease_path().exists(),
            cache_adoption,
        }
    }

    let root = workspace();
    source_composition(root.path());
    let before = tree_snapshot(root.path());
    let legacy = tree_snapshot(&root.path().join(".build"));
    let paths = tempfile::tempdir().unwrap();
    let cache_home = paths.path().join("xdg-cache");
    let state_cold = paths.path().join("state-cold");
    let state_warm = paths.path().join("state-warm");
    assert_private_test_dir(&cache_home);
    assert_private_test_dir(&state_cold);
    assert_private_test_dir(&state_warm);
    assert_private_test_dir(&state_cold.join("runtime"));
    assert_private_test_dir(&state_warm.join("runtime"));
    let cache_base = cache_home.join("bsl-analyzer");
    let layout = cache_layout(root.path(), "EXT_A", Some(&cache_base));
    let flags_owned = composition_flags("EXT_A");
    let flags: Vec<_> = flags_owned.iter().map(String::as_str).collect();
    let cold = exercise(root.path(), &flags, &cache_home, &state_cold, false);
    let warm = exercise(root.path(), &flags, &cache_home, &state_warm, true);
    assert_eq!(warm.graph, cold.graph, "HTTP warm restart reuses graph contents");
    assert_eq!(warm.search_markers, cold.search_markers, "HTTP warm restart retains search marker");
    assert_ne!(cold.pid, warm.pid, "cold and warm HTTP sessions use distinct processes");
    let after = tree_snapshot(root.path());
    assert_eq!(after, before, "HTTP cold and warm runs leave every source path unchanged");
    assert_eq!(tree_snapshot(&root.path().join(".build")), legacy);
    let scope = format!("default:{}", layout.scope_stamp().split_once(':').unwrap().1);
    let report = serde_json::json!({
        "case": "workspace_cache_scope_http_cold_and_warm_runs",
        "os": std::env::consts::OS,
        "mode": "http",
        "cache_origin": "linux-xdg-default",
        "source_before": snapshot_report(&before),
        "source_during_cold": snapshot_report(&cold.source_during),
        "source_during_warm": snapshot_report(&warm.source_during),
        "source_after": snapshot_report(&after),
        "cache_home": cache_home,
        "cache_base": layout.base(),
        "cache_leaf": layout.root(),
        "scope": scope,
        "cache_snapshot": snapshot_report(&tree_snapshot(layout.root())),
        "cold": {"pid": cold.pid, "graph": cold.graph, "search_markers": cold.search_markers,
            "state_snapshot": snapshot_report(&cold.state_snapshot),
            "cache_after_shutdown": snapshot_report(&cold.cache_after_shutdown),
            "lease": cold.lease, "native_exit": cold.exit_code, "lease_released": cold.lease_released,
        },
        "warm": {"pid": warm.pid, "graph": warm.graph, "search_markers": warm.search_markers,
            "state_snapshot": snapshot_report(&warm.state_snapshot),
            "cache_after_shutdown": snapshot_report(&warm.cache_after_shutdown),
            "lease": warm.lease, "native_exit": warm.exit_code, "lease_released": warm.lease_released,
            "cache_adoption_log": warm.cache_adoption},
        "legacy_flat_snapshot": snapshot_report(&legacy),
    });
    println!("BA235_NATIVE_RECEIPT={report}");
}

#[cfg(unix)]
#[test]
fn workspace_cache_scope_optional_journal_overlap_keeps_source_clean() {
    const JOURNAL_UNAVAILABLE: &str =
        "vector journal unavailable; diagnostic history may have gaps";

    let root = workspace();
    source_composition(root.path());
    let paths = tempfile::tempdir().unwrap();
    let state = paths.path().join("state");
    let runtime = paths.path().join("runtime");
    assert_private_test_dir(&state);
    assert_private_test_dir(&runtime);
    assert!(!state.starts_with(root.path()), "state root is external to the main workspace");

    let workspace_hash =
        blake3::hash(root.path().canonicalize().unwrap().as_os_str().as_encoded_bytes())
            .to_hex()
            .to_string();
    let journal_root = state.join("bsl-analyzer").join("vector-journal").join(&workspace_hash);
    assert_private_test_dir(&state.join("bsl-analyzer"));
    assert_private_test_dir(&state.join("bsl-analyzer/vector-journal"));
    assert_private_test_dir(&journal_root);
    write_configuration(
        &journal_root,
        "",
        "ВнешнееРасширение",
        "МодульРасширенияА",
        &format!(
            "Функция CacheGraphA235() Экспорт\n\tСтрока = \"CACHE_LEXICAL_A_235 CACHE_VECTOR_A_235\";\n\t{MAIN_MODULE}.Экспортируемая();\n\tВозврат Строка;\nКонецФункции\n"
        ),
        true,
    );
    let cache = tempfile::tempdir().unwrap();
    assert_private_test_dir(cache.path());
    let before = tree_snapshot(root.path());
    let legacy = tree_snapshot(&root.path().join(".build"));
    let journal_before = tree_snapshot(&journal_root);
    let mut config = project_model::ProjectConfig::load(root.path()).unwrap().unwrap_or_default();
    project_model::SourceSetOverride {
        configuration_root: Some(MAIN.into()),
        extensions: Some(vec![project_model::ExtensionDecl::Structured(
            project_model::StructuredExtensionDecl {
                name: "EXT_A".into(),
                path: journal_root.to_string_lossy().into_owned(),
                depends_on: Vec::new(),
            },
        )]),
        externals: None,
    }
    .apply_to(&mut config);
    let project = project_model::Project::with_config(root.path(), config).unwrap();
    let layout = mcp_server::WorkspaceCacheLayout::for_project(
        &project,
        Some(cache.path()),
        root.path(),
        None,
    )
    .unwrap();
    let extension_arg = format!("EXT_A={}", journal_root.display());
    let flags = ["--configuration-root", MAIN, "--extension", &extension_arg];

    let mut session = McpSession::start_transport(
        root.path(),
        &flags,
        "stdio",
        Some(cache.path()),
        None,
        Some(&state),
        None,
        Some("0"),
        None,
    );
    let graph = graph_overview(&mut session);
    assert_eq!(graph["nodes"], 2, "optional journal failure leaves MCP graph available");
    let marker = wait_for_marker(&mut session, "CACHE_LEXICAL_A_235");
    assert!(!marker.is_empty(), "search remains available for the external extension");
    assert_leaf_artifacts(&layout);
    let lease = lease_receipt(&layout.lease_path(), session.pid());
    let source_during = tree_snapshot(root.path());
    let journal_during = tree_snapshot(&journal_root);
    assert_tree_unchanged(&before, root.path(), "while optional journal overlap is live");
    assert_eq!(
        journal_during, journal_before,
        "overlapping journal leaf remains source-clean while live"
    );
    assert!(session.shutdown().unwrap().success(), "overlap fixture shuts down cleanly");
    let journal_diagnostic = session
        .stderr
        .wait_for_text(JOURNAL_UNAVAILABLE, Duration::from_secs(5))
        .expect("native runtime reports that the optional overlapping journal is unavailable");
    assert_tree_unchanged(&before, root.path(), "after optional journal overlap refusal");
    let journal_after = tree_snapshot(&journal_root);
    assert_eq!(
        journal_after, journal_before,
        "overlapping journal leaf remains source-clean after shutdown"
    );
    assert!(layout.root().starts_with(cache.path()));
    assert!(!layout.root().starts_with(root.path()), "graph cache remains external to sources");
    assert_eq!(tree_snapshot(&root.path().join(".build")), legacy);
    let source_after = tree_snapshot(root.path());

    let report = serde_json::json!({
        "case": "workspace_cache_scope_optional_journal_overlap",
        "os": std::env::consts::OS,
        "mode": "stdio",
        "source_before": snapshot_report(&before),
        "source_during": snapshot_report(&source_during),
        "source_after": snapshot_report(&source_after),
        "source_state_root": state,
        "declared_external_extension_root": journal_root,
        "workspace_hash": workspace_hash,
        "journal_root_before": snapshot_report(&journal_before),
        "journal_root_during": snapshot_report(&journal_during),
        "journal_root_after": snapshot_report(&journal_after),
        "journal_unavailable_diagnostic": JOURNAL_UNAVAILABLE,
        "diagnostic_observed": journal_diagnostic.contains(JOURNAL_UNAVAILABLE),
        "graph": graph,
        "search_marker": marker,
        "lease": lease,
        "cache_leaf": layout.root(),
        "cache_leaf_external": !layout.root().starts_with(root.path()),
        "legacy_flat_snapshot": snapshot_report(&legacy),
    });
    println!("BA235_NATIVE_RECEIPT={report}");
}

#[cfg(unix)]
#[test]
fn workspace_cache_scope_live_topology_drift_stops_stdio_and_http() {
    const SCOPE_STOP: &str = "workspace source composition changed; restart/reconnect MCP";

    let assert_only_config_changed = |before: &TreeSnapshot, after: &TreeSnapshot, phase: &str| {
        assert_eq!(
            before.entries.keys().collect::<Vec<_>>(),
            after.entries.keys().collect::<Vec<_>>(),
            "only the declared config changes {phase}"
        );
        let changed = before
            .entries
            .iter()
            .filter_map(|(path, entry)| {
                (after.entries.get(path) != Some(entry)).then_some(path.as_str())
            })
            .collect::<Vec<_>>();
        assert_eq!(changed, ["bsl-analyzer.toml"], "exact topology drift path {phase}");
    };

    let mut receipts = Vec::new();
    for mode in ["stdio", "http"] {
        let root = workspace();
        source_composition(root.path());
        let source_before = tree_snapshot(root.path());
        let legacy = tree_snapshot(&root.path().join(".build"));
        let config_path = root.path().join("bsl-analyzer.toml");
        let original_config = std::fs::read(&config_path).unwrap();
        let changed_config = format!(
            "{}\n[source]\nexclude = [\"a/b/ext-a\"]\n",
            String::from_utf8_lossy(&original_config)
        );
        let paths = tempfile::tempdir().unwrap();
        let cache = paths.path().join("cache");
        let old_state = paths.path().join("old-state");
        let restart_state = paths.path().join("restart-state");
        assert_private_test_dir(&cache);
        assert_private_test_dir(&old_state);
        assert_private_test_dir(&restart_state);
        assert_private_test_dir(&old_state.join("runtime"));
        assert_private_test_dir(&restart_state.join("runtime"));
        let flags_owned = composition_flags("EXT_A");
        let flags: Vec<_> = flags_owned.iter().map(String::as_str).collect();
        let old_layout = cache_layout(root.path(), "EXT_A", Some(&cache));
        let old_scope = old_layout.scope_stamp();

        if mode == "stdio" {
            let mut old = McpSession::start_transport(
                root.path(),
                &flags,
                "stdio",
                Some(&cache),
                None,
                Some(&old_state),
                None,
                Some("0"),
                None,
            );
            let old_graph = graph_overview(&mut old);
            let old_marker = wait_for_marker(&mut old, "CACHE_LEXICAL_A_235");
            assert_eq!(old_graph["nodes"], 2);
            assert!(!old_marker.is_empty(), "stdio source is queryable before composition drift");
            assert_leaf_artifacts(&old_layout);
            let old_lease = lease_receipt(&old_layout.lease_path(), old.pid());
            let old_cache_before = durable_cache_snapshot(&old_layout);
            let old_logical_before = logical_cache_inventory(&old_layout);
            assert_eq!(
                old_logical_before["graph_method_call_rows"].as_array().unwrap().len(),
                1,
                "old graph inventory contains the fixture's sole edge between its two method nodes"
            );
            assert!(
                old_logical_before["search_documents"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|document| { document["root_id"] == "a/b/ext-a" }),
                "old search inventory contains EXT_A documents"
            );
            assert!(old.stdin.is_some(), "stdio stdin stays open across project drift");
            assert!(old.is_running(), "stdio process is alive immediately before scope drift");

            std::fs::write(&config_path, changed_config.as_bytes()).unwrap();
            let source_changed = tree_snapshot(root.path());
            assert_only_config_changed(&source_before, &source_changed, "while stdio is live");
            let new_layout = cache_layout(root.path(), "EXT_A", Some(&cache));
            assert_ne!(
                old_scope,
                new_layout.scope_stamp(),
                "excluded extension changes resolver scope"
            );
            let (old_exit, old_stop_count) = wait_for_scope_guard_exit(
                &mut old.child,
                &mut old.stderr,
                &old_layout.lease_path(),
                "stdio",
            );
            assert!(old.shutdown().unwrap().success(), "collects already-exited stdio status");
            let old_logical_after = logical_cache_inventory(&old_layout);
            assert_eq!(
                old_logical_after, old_logical_before,
                "scope stop does not publish changed-composition graph or search rows into the old leaf"
            );
            assert!(
                !new_layout.root().exists(),
                "old stdio owner publishes nothing to the new scope"
            );

            let mut restarted = McpSession::start_transport(
                root.path(),
                &flags,
                "stdio",
                Some(&cache),
                None,
                Some(&restart_state),
                None,
                Some("0"),
                None,
            );
            let restarted_graph = graph_overview(&mut restarted);
            restarted.wait_ready("search");
            let restarted_marker = search_marker(&mut restarted, "CACHE_LEXICAL_A_235");
            assert!(restarted_marker.is_empty(), "new stdio scope excludes EXT_A source rows");
            assert_eq!(restarted_graph["nodes"], 1, "new stdio graph inventory excludes EXT_A");
            assert_leaf_artifacts(&new_layout);
            let restarted_lease = lease_receipt(&new_layout.lease_path(), restarted.pid());
            let new_cache_before_restore = durable_cache_snapshot(&new_layout);

            std::fs::write(&config_path, &original_config).unwrap();
            let source_restored = tree_snapshot(root.path());
            assert_eq!(
                source_restored, source_before,
                "restores the exact original source snapshot"
            );
            assert_eq!(cache_layout(root.path(), "EXT_A", Some(&cache)).scope_stamp(), old_scope);
            let (restarted_exit, restarted_stop_count) = wait_for_scope_guard_exit(
                &mut restarted.child,
                &mut restarted.stderr,
                &new_layout.lease_path(),
                "restarted stdio",
            );
            assert!(restarted.shutdown().unwrap().success());
            assert_eq!(tree_snapshot(&root.path().join(".build")), legacy);
            receipts.push(serde_json::json!({
                "mode": mode,
                "source_before": snapshot_report(&source_before),
                "source_changed": snapshot_report(&source_changed),
                "source_restored": snapshot_report(&source_restored),
                "intentional_changed_path": "bsl-analyzer.toml",
                "scope_stop_diagnostic": SCOPE_STOP,
                "old": {"pid": old.pid(), "exit": old_exit.code(), "scope": old_scope,
                    "scope_stop_count": old_stop_count,
                    "cache_leaf": old_layout.root(), "lease": old_lease,
                    "logical_before": old_logical_before, "logical_after": old_logical_after,
                    "cache_before": snapshot_report(&old_cache_before),
                    "cache_after_drift": snapshot_report(&durable_cache_snapshot(&old_layout)),
                    "lease_released": !old_layout.lease_path().exists()},
                "restart": {"pid": restarted.pid(), "exit": restarted_exit.code(),
                    "scope_stop_count": restarted_stop_count,
                    "scope": new_layout.scope_stamp(), "cache_leaf": new_layout.root(),
                    "graph": restarted_graph, "excluded_marker_hits": restarted_marker,
                    "lease": restarted_lease,
                    "cache_before_restore": snapshot_report(&new_cache_before_restore),
                    "cache_after_restore": snapshot_report(&durable_cache_snapshot(&new_layout)),
                    "lease_released": !new_layout.lease_path().exists()},
            }));
        } else {
            let (mut old, mut old_stderr, address, _session_id, old_graph, old_marker) =
                start_http_scope_session(root.path(), &flags, &cache, &old_state);
            let old_pid = old.id();
            assert_eq!(old_graph["nodes"], 2);
            assert!(!old_marker.is_empty(), "HTTP source is queryable before composition drift");
            assert!(http_health(address), "HTTP listener remains open before scope drift");
            assert_leaf_artifacts(&old_layout);
            let old_lease = lease_receipt(&old_layout.lease_path(), old_pid);
            let old_cache_before = durable_cache_snapshot(&old_layout);
            let old_logical_before = logical_cache_inventory(&old_layout);
            assert_eq!(
                old_logical_before["graph_method_call_rows"].as_array().unwrap().len(),
                1,
                "old graph inventory contains the fixture's sole edge between its two method nodes"
            );
            assert!(
                old_logical_before["search_documents"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|document| { document["root_id"] == "a/b/ext-a" }),
                "old search inventory contains EXT_A documents"
            );
            assert!(
                old.try_wait().unwrap().is_none(),
                "HTTP process is alive immediately before scope drift"
            );

            std::fs::write(&config_path, changed_config.as_bytes()).unwrap();
            let source_changed = tree_snapshot(root.path());
            assert_only_config_changed(&source_before, &source_changed, "while HTTP is live");
            let new_layout = cache_layout(root.path(), "EXT_A", Some(&cache));
            assert_ne!(
                old_scope,
                new_layout.scope_stamp(),
                "excluded extension changes resolver scope"
            );
            let (old_exit, old_stop_count) = wait_for_scope_guard_exit(
                &mut old,
                &mut old_stderr,
                &old_layout.lease_path(),
                "HTTP",
            );
            assert!(!http_health(address), "HTTP listener closes after scope drift");
            let old_logical_after = logical_cache_inventory(&old_layout);
            assert_eq!(
                old_logical_after, old_logical_before,
                "scope stop does not publish changed-composition graph or search rows into the old leaf"
            );
            assert!(
                !new_layout.root().exists(),
                "old HTTP owner publishes nothing to the new scope"
            );

            let (
                mut restarted,
                mut restarted_stderr,
                new_address,
                _new_session_id,
                restarted_graph,
                restarted_marker,
            ) = start_http_scope_session(root.path(), &flags, &cache, &restart_state);
            let restarted_pid = restarted.id();
            assert!(restarted_marker.is_empty(), "new HTTP scope excludes EXT_A source rows");
            assert_eq!(restarted_graph["nodes"], 1, "new HTTP graph inventory excludes EXT_A");
            assert_leaf_artifacts(&new_layout);
            let restarted_lease = lease_receipt(&new_layout.lease_path(), restarted_pid);
            let new_cache_before_restore = durable_cache_snapshot(&new_layout);

            std::fs::write(&config_path, &original_config).unwrap();
            let source_restored = tree_snapshot(root.path());
            assert_eq!(
                source_restored, source_before,
                "restores the exact original source snapshot"
            );
            assert_eq!(cache_layout(root.path(), "EXT_A", Some(&cache)).scope_stamp(), old_scope);
            let (restarted_exit, restarted_stop_count) = wait_for_scope_guard_exit(
                &mut restarted,
                &mut restarted_stderr,
                &new_layout.lease_path(),
                "restarted HTTP",
            );
            assert!(!http_health(new_address));
            assert_eq!(tree_snapshot(&root.path().join(".build")), legacy);
            receipts.push(serde_json::json!({
                "mode": mode,
                "source_before": snapshot_report(&source_before),
                "source_changed": snapshot_report(&source_changed),
                "source_restored": snapshot_report(&source_restored),
                "intentional_changed_path": "bsl-analyzer.toml",
                "scope_stop_diagnostic": SCOPE_STOP,
                "old": {"pid": old_pid, "exit": old_exit.code(), "scope": old_scope,
                    "scope_stop_count": old_stop_count,
                    "cache_leaf": old_layout.root(), "lease": old_lease,
                    "logical_before": old_logical_before, "logical_after": old_logical_after,
                    "cache_before": snapshot_report(&old_cache_before),
                    "cache_after_drift": snapshot_report(&durable_cache_snapshot(&old_layout)),
                    "lease_released": !old_layout.lease_path().exists()},
                "restart": {"pid": restarted_pid, "exit": restarted_exit.code(),
                    "scope_stop_count": restarted_stop_count,
                    "scope": new_layout.scope_stamp(), "cache_leaf": new_layout.root(),
                    "graph": restarted_graph, "excluded_marker_hits": restarted_marker,
                    "lease": restarted_lease,
                    "cache_before_restore": snapshot_report(&new_cache_before_restore),
                    "cache_after_restore": snapshot_report(&durable_cache_snapshot(&new_layout)),
                    "lease_released": !new_layout.lease_path().exists()},
            }));
        }
    }

    let report = serde_json::json!({
        "case": "workspace_cache_scope_live_topology_drift_stops_stdio_and_http",
        "os": std::env::consts::OS,
        "modes": receipts,
    });
    println!("BA235_NATIVE_RECEIPT={report}");
}

#[cfg(unix)]
#[test]
fn workspace_cache_scope_daemon_broker_and_broker_required_share_only_the_leaf() {
    let root = workspace();
    source_composition(root.path());
    let before = tree_snapshot(root.path());
    let legacy = tree_snapshot(&root.path().join(".build"));
    let paths = tempfile::tempdir().unwrap();
    let cache_home = paths.path().join("xdg-cache");
    let cache_base = cache_home.join("bsl-analyzer");
    let state = paths.path().join("state");
    let runtime = state.join("runtime");
    let daemon_log = paths.path().join("broker-daemon.log");
    let daemon_log_collision = paths.path().join("daemon-log-is-a-directory");
    std::fs::create_dir(&daemon_log_collision).unwrap();
    assert_private_test_dir(&cache_home);
    assert_private_test_dir(&state);
    assert_private_test_dir(&runtime);
    let layout = cache_layout(root.path(), "EXT_A", Some(&cache_base));
    let flags_owned = composition_flags("EXT_A");
    let flags: Vec<_> = flags_owned.iter().map(String::as_str).collect();

    let mut daemon_command = Command::new(env!("CARGO_BIN_EXE_bsl-analyzer-app"));
    daemon_command
        .args(["mcp", "serve", "--profile", "workspace", "--mode", "daemon", "-s"])
        .arg(root.path())
        .args(&flags)
        .env("BSL_MCP_BROKER", "0")
        .env("BSL_MCP_IDLE_TTL_SECS", "2")
        .env("BSL_MCP_ORPHAN_GRACE_SECS", "3")
        .env("BSL_LOG", "info")
        .env("XDG_CACHE_HOME", &cache_home)
        .env("XDG_STATE_HOME", &state)
        .env("XDG_RUNTIME_DIR", &runtime)
        .env_remove("BSL_CACHE_DIR")
        .env_remove("EMBEDDING_URL")
        .env_remove("EMBEDDING_MODEL")
        .env_remove("EMBEDDING_DIM")
        .env_remove("EMBEDDING_API_KEY")
        .env_remove("EMBEDDING_PROVIDER")
        .env_remove("EMBEDDING_QUERY_PREFIX")
        .env_remove("EMBEDDING_DOCUMENT_PREFIX")
        .env("BSL_MCP_DAEMON_LOG", &daemon_log_collision)
        .current_dir(root.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut daemon = daemon_command.spawn().expect("direct native daemon starts");
    let mut daemon_stderr = StderrCapture::attach(daemon.stderr.take().unwrap());
    let daemon_pid = daemon.id();
    let daemon_lease = wait_for_lease_pid(&layout.lease_path(), Duration::from_secs(30));
    assert_eq!(daemon_lease, daemon_pid, "the direct daemon owns the live lease");

    let mut required = McpSession::start_transport(
        root.path(),
        &flags,
        "broker-required",
        None,
        Some(&cache_home),
        Some(&state),
        None,
        None,
        Some(daemon_pid),
    );
    let graph_required = graph_overview(&mut required);
    assert_eq!(graph_required["nodes"], 2, "required proxy reaches the supervised daemon");
    let marker_required = wait_for_marker(&mut required, "CACHE_LEXICAL_A_235");
    assert!(!marker_required.is_empty(), "broker-required search reaches daemon data");
    let log_fallback = daemon_stderr
        .wait_for_text("Failed to setup logging:", Duration::from_secs(5))
        .expect("daemon logging IO failure truthfully falls back to stderr");
    assert_leaf_artifacts(&layout);
    let lease_required = lease_receipt(&layout.lease_path(), daemon_pid);
    assert_tree_unchanged(&before, root.path(), "with direct daemon and required proxy alive");
    assert!(required.shutdown().unwrap().success(), "required proxy exits cleanly");
    let daemon_deadline = Instant::now() + Duration::from_secs(15);
    let daemon_status = loop {
        if let Some(status) = daemon.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= daemon_deadline {
            let _ = daemon.kill();
            let _ = daemon.wait();
            panic!("fixture-owned daemon exceeded bounded idle shutdown");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(daemon_status.success(), "supervised daemon shuts down after its final client");
    assert!(!layout.lease_path().exists(), "daemon releases its lease after owner shutdown");
    daemon_stderr.join();

    daemon_command.env("BSL_MCP_DAEMON_LOG", &daemon_log);
    let mut warm_daemon = daemon_command.spawn().expect("direct native warm daemon starts");
    let mut warm_daemon_stderr = StderrCapture::attach(warm_daemon.stderr.take().unwrap());
    let warm_daemon_pid = warm_daemon.id();
    let warm_lease_pid = wait_for_lease_pid(&layout.lease_path(), Duration::from_secs(30));
    assert_eq!(warm_lease_pid, warm_daemon_pid, "warm daemon owns the live lease");
    let mut warm_required = McpSession::start_transport(
        root.path(),
        &flags,
        "broker-required",
        None,
        Some(&cache_home),
        Some(&state),
        None,
        None,
        Some(warm_daemon_pid),
    );
    let graph_warm_direct = graph_overview(&mut warm_required);
    assert_eq!(graph_warm_direct, graph_required, "direct warm daemon serves the same graph");
    let marker_warm_direct = wait_for_marker(&mut warm_required, "CACHE_LEXICAL_A_235");
    assert!(!marker_warm_direct.is_empty(), "direct warm daemon serves its search marker");
    let direct_warm_adoption = wait_for_cache_adoption_log(&daemon_log, 0, Duration::from_secs(10));
    assert!(
        !direct_warm_adoption.is_empty(),
        "direct warm daemon must publish the existing graph from disk; daemon log: {}",
        std::fs::read_to_string(&daemon_log).unwrap_or_else(|error| error.to_string())
    );
    let lease_direct_warm = lease_receipt(&layout.lease_path(), warm_daemon_pid);
    assert_tree_unchanged(&before, root.path(), "with direct warm daemon and required proxy alive");
    assert!(warm_required.shutdown().unwrap().success(), "warm required proxy exits cleanly");
    let warm_daemon_deadline = Instant::now() + Duration::from_secs(15);
    let warm_daemon_status = loop {
        if let Some(status) = warm_daemon.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= warm_daemon_deadline {
            let _ = warm_daemon.kill();
            let _ = warm_daemon.wait();
            panic!("fixture-owned warm daemon exceeded bounded idle shutdown");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(
        warm_daemon_status.success(),
        "warm daemon exits cleanly after required proxy shutdown"
    );
    warm_daemon_stderr.join();
    assert!(!layout.lease_path().exists(), "warm daemon releases its lease");

    let mut broker = McpSession::start_transport_with_cwd_and_log(
        root.path(),
        &flags,
        "broker",
        None,
        Some(&cache_home),
        Some(&state),
        None,
        None,
        None,
        root.path(),
        Some(&daemon_log),
    );
    let graph_broker = graph_overview(&mut broker);
    assert_eq!(graph_broker, graph_required, "broker proxy starts from the warm daemon cache");
    let marker_broker = wait_for_marker(&mut broker, "CACHE_LEXICAL_A_235");
    assert!(!marker_broker.is_empty(), "broker proxy serves the scoped search marker");
    let broker_cache_adoption = wait_for_cache_adoption_log(
        &daemon_log,
        direct_warm_adoption.len(),
        Duration::from_secs(10),
    );
    assert!(
        !broker_cache_adoption.is_empty(),
        "broker-launched warm daemon must publish the existing graph from disk; daemon log: {}",
        std::fs::read_to_string(&daemon_log).unwrap_or_else(|error| error.to_string())
    );
    let broker_lease_pid = wait_for_lease_pid(&layout.lease_path(), Duration::from_secs(10));
    assert_ne!(broker_lease_pid, broker.pid(), "broker proxy and daemon are separate processes");
    let lease_broker = lease_receipt(&layout.lease_path(), broker_lease_pid);
    assert_leaf_artifacts(&layout);
    assert_tree_unchanged(&before, root.path(), "with broker proxy and detached daemon alive");
    assert!(broker.shutdown().unwrap().success(), "broker proxy exits cleanly");

    let cleanup_deadline = Instant::now() + Duration::from_secs(15);
    while layout.lease_path().exists() && Instant::now() < cleanup_deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    if layout.lease_path().exists() {
        stop_owned_daemon_after_timeout(broker_lease_pid);
        panic!(
            "fixture-owned broker daemon exceeded the graceful idle-shutdown deadline; sent TERM to pid {broker_lease_pid} for bounded cleanup"
        );
    }
    assert!(!layout.lease_path().exists(), "fixture-owned broker daemon releases lease boundedly");
    assert_tree_unchanged(&before, root.path(), "after direct/broker daemon shutdown");
    assert_eq!(tree_snapshot(&root.path().join(".build")), legacy);
    let after = tree_snapshot(root.path());
    let scope = format!("default:{}", layout.scope_stamp().split_once(':').unwrap().1);
    let report = serde_json::json!({
        "case": "workspace_cache_scope_daemon_broker_and_broker_required",
        "os": std::env::consts::OS,
        "modes": ["daemon", "broker-required", "broker"],
        "cache_origin": "linux-xdg-default",
        "source_before": snapshot_report(&before),
        "source_during_required": snapshot_report(&before),
        "source_during_broker": snapshot_report(&before),
        "source_after": snapshot_report(&after),
        "cache_base": layout.base(),
        "cache_leaf": layout.root(),
        "scope": scope,
        "daemon_pid": daemon_pid,
        "daemon_exit": daemon_status.code(),
        "daemon_log_io_failure_fallback": log_fallback.contains("Failed to setup logging:"),
        "required": {"proxy_pid": required.pid(), "graph": graph_required,
            "search_marker": marker_required, "lease": lease_required},
        "direct_warm": {"pid": warm_daemon_pid, "exit": warm_daemon_status.code(),
            "graph": graph_warm_direct, "search_marker": marker_warm_direct,
            "lease": lease_direct_warm, "cache_adoption_log": direct_warm_adoption},
        "broker": {"proxy_pid": broker.pid(), "daemon_pid": broker_lease_pid,
            "graph": graph_broker, "search_marker": marker_broker, "lease": lease_broker,
            "cache_adoption_log": broker_cache_adoption},
        "daemon_log_path": daemon_log,
        "cache_home": cache_home,
        "cache_snapshot": snapshot_report(&tree_snapshot(layout.root())),
        "legacy_flat_snapshot": snapshot_report(&legacy),
    });
    println!("BA235_NATIVE_RECEIPT={report}");
}

#[cfg(unix)]
#[test]
fn workspace_cache_scope_s12_proxy_parent_child_composition_mismatch() {
    let embedder = EmbeddingStub::start();
    let paths = tempfile::tempdir().unwrap();
    let runtime = paths.path().join("runtime");
    let state = paths.path().join("state");
    let cache = paths.path().join("cache");
    let stdout_path = paths.path().join("driver.stdout");
    let stderr_path = paths.path().join("driver.stderr");
    assert_private_test_dir(&runtime);
    assert_private_test_dir(&state);
    assert_private_test_dir(&cache);
    let stdout = std::fs::File::create(&stdout_path).unwrap();
    let stderr = std::fs::File::create(&stderr_path).unwrap();
    let mut driver = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "s12_proxy_scope_mismatch_driver", "--nocapture"])
        .env("BA235_S12_PROXY_DRIVER", "1")
        .env("BA235_S12_PROXY_CACHE", &cache)
        .env("BA235_S12_PROXY_RUNTIME", &runtime)
        .env("BA235_S12_PROXY_STATE", &state)
        .env("XDG_RUNTIME_DIR", &runtime)
        .env("XDG_STATE_HOME", &state)
        .env("EMBEDDING_URL", &embedder.url)
        .env("EMBEDDING_MODEL", "workspace-cache-fixture")
        .env("EMBEDDING_DIM", "3")
        .env_remove("EMBEDDING_API_KEY")
        .env_remove("EMBEDDING_PROVIDER")
        .env_remove("EMBEDDING_QUERY_PREFIX")
        .env_remove("EMBEDDING_DOCUMENT_PREFIX")
        .env_remove(mcp_server::broker::EMBEDDING_QUERY_PREFIX_ENV)
        .env_remove(mcp_server::broker::EMBEDDING_DOCUMENT_PREFIX_ENV)
        .env_remove(mcp_server::broker::EMBEDDING_MAX_INPUT_TOKENS_ENV)
        .env_remove(mcp_server::broker::EMBEDDING_TOKENIZER_FILE_ENV)
        .env_remove(mcp_server::broker::EMBEDDING_TOKENIZER_SHA256_ENV)
        .env_remove("BSL_CACHE_DIR")
        .env_remove(mcp_server::WORKSPACE_CACHE_SCOPE_ENV)
        .env_remove("BSL_MCP_DAEMON_LOG")
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .spawn()
        .expect("isolated native proxy fixture starts");
    let deadline = Instant::now() + Duration::from_secs(45);
    let status = loop {
        if let Some(status) = driver.try_wait().expect("poll isolated proxy fixture") {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = driver.kill();
            let _ = driver.wait();
            stop_scoped_daemons_after_fixture_timeout(&cache, &runtime);
            panic!("isolated proxy fixture exceeded its 45 second watchdog");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let stdout = std::fs::read_to_string(&stdout_path).unwrap_or_default();
    let stderr = std::fs::read_to_string(&stderr_path).unwrap_or_default();
    if !status.success() {
        stop_scoped_daemons_after_fixture_timeout(&cache, &runtime);
    }
    assert!(status.success(), "isolated proxy driver passes: stdout={} stderr={}", stdout, stderr);
    let receipt = stdout
        .lines()
        .find(|line| line.starts_with("BA235_NATIVE_RECEIPT="))
        .expect("proxy driver emitted a durable receipt");
    println!("{receipt}");
}

#[cfg(unix)]
#[test]
fn s12_proxy_scope_mismatch_driver() {
    if std::env::var_os("BA235_S12_PROXY_DRIVER").is_none() {
        return;
    }

    use std::collections::BTreeSet;
    use std::os::unix::fs::PermissionsExt as _;

    let root = workspace();
    source_composition(root.path());
    let before = tree_snapshot(root.path());
    let config_path = root.path().join("bsl-analyzer.toml");
    let original_config = std::fs::read(&config_path).unwrap();
    let cache = PathBuf::from(std::env::var_os("BA235_S12_PROXY_CACHE").unwrap());
    let runtime = PathBuf::from(std::env::var_os("BA235_S12_PROXY_RUNTIME").unwrap());
    let state = PathBuf::from(std::env::var_os("BA235_S12_PROXY_STATE").unwrap());
    let layout = cache_layout(root.path(), "EXT_A", Some(&cache));
    let parent_stamp = layout.scope_stamp();
    let parent_topology = mcp_server::broker::workspace_topology_fingerprint(root.path());
    let embedding = mcp_server::broker::embedding_config_fingerprint_with_prefixes(
        &mcp_server::EmbeddingPrefixes::default(),
    );
    let project_config = project_model::ProjectConfig::load(root.path()).ok().flatten();
    let help = platform_help::requested_source(project_config.as_ref(), root.path())
        .map(|request| format!("{request:?}"))
        .unwrap_or_else(|reason| format!("invalid: {reason}"));
    let mut config_hasher = blake3::Hasher::new();
    config_hasher.update(&embedding.to_le_bytes());
    config_hasher.update(help.as_bytes());
    let config_digest = config_hasher.finalize();
    let parent_config = u64::from_le_bytes(config_digest.as_bytes()[..8].try_into().unwrap());
    let key = mcp_server::broker::BackendKey::new(
        root.path(),
        layout.root(),
        mcp_server::McpProfile::Workspace,
        parent_config,
        parent_topology,
        BTreeSet::new(),
    );
    let count_path = runtime.join("wrapper-count");
    let gate_path = runtime.join("release-child");
    let child_started_path = runtime.join("real-cli-child-started");
    let wrapper_path = runtime.join("launch-barrier.sh");
    let daemon_log = runtime.join("custom-daemon.log");
    let quote = |path: &Path| format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"));
    let child_started = quote(&child_started_path);
    let wrapper = format!(
        "#!/bin/sh\ncount_file={}\ngate_file={}\nchild_started={}\nn=0\nif [ -f \"$count_file\" ]; then n=$(cat \"$count_file\"); fi\nn=$((n + 1))\nprintf '%s\\n' \"$n\" > \"$count_file\"\nif [ \"$n\" -eq 1 ]; then while [ ! -e \"$gate_file\" ]; do sleep 0.02; done; printf '%s\\n' \"$$\" > \"$child_started\"; exec \"$@\"; fi\nexit 78\n",
        quote(&count_path),
        quote(&gate_path),
        child_started
    );
    std::fs::write(&wrapper_path, wrapper).unwrap();
    std::fs::set_permissions(&wrapper_path, std::fs::Permissions::from_mode(0o700)).unwrap();

    let mut daemon = Command::new("sh");
    daemon
        .arg(&wrapper_path)
        .arg(env!("CARGO_BIN_EXE_bsl-analyzer-app"))
        .args(["mcp", "serve", "--profile", "workspace", "--mode", "daemon", "-s"])
        .arg(root.path())
        .args(composition_flags("EXT_A"))
        .arg("--cache-dir")
        .arg(&cache)
        .env(mcp_server::WORKSPACE_CACHE_SCOPE_ENV, &parent_stamp)
        .env(mcp_server::broker::TOPOLOGY_FP_ENV, parent_topology.to_string())
        .env("BSL_MCP_BROKER", "0")
        .env("BSL_MCP_IDLE_TTL_SECS", "2")
        .env("BSL_MCP_ORPHAN_GRACE_SECS", "3")
        .env("BSL_MCP_DAEMON_LOG", &daemon_log)
        .env("XDG_RUNTIME_DIR", &runtime)
        .env("XDG_STATE_HOME", &state)
        .env("EMBEDDING_URL", std::env::var("EMBEDDING_URL").unwrap())
        .env("EMBEDDING_MODEL", "workspace-cache-fixture")
        .env("EMBEDDING_DIM", "3")
        .env("BSL_LOG", "info")
        .env_remove("BSL_CACHE_DIR")
        .env_remove("EMBEDDING_API_KEY")
        .env_remove("EMBEDDING_PROVIDER")
        .current_dir(root.path());

    let config_for_gate = config_path.clone();
    let original_for_gate = original_config.clone();
    let gate_for_thread = gate_path.clone();
    let count_for_thread = count_path.clone();
    let barrier = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let count = std::fs::read_to_string(&count_for_thread)
                .ok()
                .and_then(|text| text.trim().parse::<u32>().ok())
                .unwrap_or_default();
            if count > 0 || Instant::now() >= deadline {
                let drifted = format!(
                    "{}\n[source]\nexclude = [\"a/b/ext-a\"]\n",
                    String::from_utf8_lossy(&original_for_gate)
                );
                std::fs::write(&config_for_gate, drifted).unwrap();
                std::fs::write(&gate_for_thread, b"release").unwrap();
                return count > 0;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    });

    let runtime_builder =
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let outcome = runtime_builder
        .block_on(mcp_server::broker::proxy::connect_or_launch(key.clone(), daemon))
        .expect("native broker launch returns a bounded outcome");
    let unavailable = match outcome {
        mcp_server::broker::proxy::ProxyOutcome::Unavailable(error) => error.to_string(),
        mcp_server::broker::proxy::ProxyOutcome::Served => {
            panic!(
                "proxy must reject a child whose composition no longer matches the frozen parent"
            )
        }
    };
    assert!(unavailable.contains("within 30s"), "proxy failure is finite: {unavailable}");
    assert!(barrier.join().unwrap(), "wrapper observed proxy spawn before releasing child");
    assert!(child_started_path.exists(), "real CLI daemon launched after workspace mutation");
    let child_count = std::fs::read_to_string(&count_path).unwrap().trim().parse::<u32>().unwrap();
    let broker_log = runtime.join("bsl-mcp").join(format!("{}.log", key.digest()));
    let child_failure = {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let log = std::fs::read_to_string(&broker_log).unwrap_or_default();
            if log.contains("workspace cache scope changed between launcher and daemon")
                || Instant::now() >= deadline
            {
                break log;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    let drifted_source = tree_snapshot(root.path());
    std::fs::write(&config_path, original_config).unwrap();
    let after = tree_snapshot(root.path());

    assert!(
        child_failure.contains("workspace cache scope changed between launcher and daemon"),
        "real daemon rejected the frozen parent stamp after TOML drift: {child_failure}"
    );
    assert!(
        child_count > 0 && child_count <= 11,
        "spawns stay within the 30s proxy retry budget: {child_count}"
    );
    assert!(!layout.root().exists(), "scope mismatch creates no cache leaf");
    assert!(!layout.lease_path().exists(), "scope mismatch acquires no writer lease");
    assert!(!daemon_log.exists(), "scope mismatch creates no custom daemon log");
    assert_eq!(after, before, "restoring the intentional TOML drift restores the source tree");

    let report = serde_json::json!({
        "case": "workspace_cache_scope_s12_proxy_parent_child_composition_mismatch",
        "source_before": snapshot_report(&before),
        "source_during_child_mismatch": snapshot_report(&drifted_source),
        "source_after_restore": snapshot_report(&after),
        "parent_scope_stamp": parent_stamp,
        "parent_cache_leaf": layout.root(),
        "proxy_error": unavailable,
        "proxy_child_spawn_count": child_count,
        "child_scope_refusal_observed": "workspace cache scope changed between launcher and daemon",
        "child_custom_log_absent": !daemon_log.exists(),
        "child_cache_leaf_absent": !layout.root().exists(),
        "child_lease_absent": !layout.lease_path().exists(),
        "legacy_flat_snapshot": snapshot_report(&tree_snapshot(&root.path().join(".build"))),
    });
    println!("BA235_NATIVE_RECEIPT={report}");
}

#[cfg(windows)]
#[test]
fn workspace_cache_scope_windows_source_delete_default_and_explicit() {
    fn run_case(cache_base: Option<&Path>, default_base: &Path) {
        let root = workspace();
        source_composition(root.path());
        let before = tree_snapshot(root.path());
        let legacy = tree_snapshot(&root.path().join(".build"));
        let embedder = EmbeddingStub::start();
        let child_cwd = tempfile::tempdir().expect("disposable child working directory");
        assert!(root.path().is_absolute(), "native source directory is absolute");
        assert!(
            !child_cwd.path().starts_with(root.path()),
            "native child working directory stays outside the source tree"
        );
        let layout = cache_layout(root.path(), "EXT_A", Some(cache_base.unwrap_or(default_base)));
        let flags_owned = composition_flags("EXT_A");
        let mut flags: Vec<String> = flags_owned.to_vec();
        if let Some(base) = cache_base {
            flags.extend(["--cache-dir".into(), base.to_string_lossy().into_owned()]);
        }
        let flags: Vec<_> = flags.iter().map(String::as_str).collect();
        let mut session = McpSession::start_transport_with_cwd(
            root.path(),
            &flags,
            "stdio",
            None,
            None,
            None,
            Some(&embedder.url),
            Some("0"),
            None,
            child_cwd.path(),
        );
        let graph = graph_overview(&mut session);
        assert_eq!(graph["nodes"], 2, "native graph ready before source deletion: {graph}");
        let marker = wait_for_marker(&mut session, "CACHE_LEXICAL_A_235");
        assert!(!marker.is_empty(), "native search ready before deletion: {marker:?}");
        assert_leaf_artifacts(&layout);
        assert!(session.is_running(), "real MCP process remains alive before deletion");
        assert!(layout.root().starts_with(layout.base().join("workspaces/v1")));
        assert!(
            !layout.root().starts_with(root.path()),
            "cache lives outside disposable NTFS source"
        );
        let expected_base = cache_base.unwrap_or(default_base).canonicalize().unwrap();
        assert_eq!(layout.base(), expected_base.as_path());
        let scope = layout.scope_stamp();
        let scope = if cache_base.is_some() {
            scope
        } else {
            format!("default:{}", scope.split_once(':').unwrap().1)
        };
        let lease = lease_receipt(&layout.lease_path(), session.pid());
        assert_tree_unchanged(&before, root.path(), "while native Windows DBs and lease are open");
        assert_eq!(
            tree_snapshot(&root.path().join(".build")),
            legacy,
            "flat cache canaries survive native run"
        );

        std::fs::remove_dir_all(root.path())
            .expect("Windows must delete the live disposable source tree");
        assert!(!root.path().exists(), "source root is absent before graceful shutdown");
        let status = session.shutdown().expect("MCP shuts down successfully after source deletion");
        session.stderr.join();
        assert!(
            status.success(),
            "graceful native process exit after deletion: {status}; stderr: {}",
            session.stderr.snapshot()
        );
        assert!(!layout.lease_path().exists(), "lease released after owner/writer shutdown");
        assert!(
            layout.graph_db_path().is_file() && layout.search_db_path().is_file(),
            "external cache survives source deletion"
        );

        let report = serde_json::json!({
            "case": if cache_base.is_some() { "explicit" } else { "windows_known_folder_default" },
            "os": std::env::consts::OS,
            "mode": "stdio",
            "source_before": snapshot_report(&before),
            "source_while_open": snapshot_report(&before),
            "source_after_deletion": {"root_exists": root.path().exists()},
            "cache_origin": if cache_base.is_some() { "explicit" } else { "windows-known-folder-default" },
            "cache_base": layout.base(),
            "cache_leaf": layout.root(),
            "scope": scope,
            "child_pid": session.pid(),
            "native_exit": status.code(),
            "graph": graph,
            "search_marker": marker,
            "lease_before_delete": lease,
            "lease_after_shutdown": "released",
            "child_working_directory": child_cwd.path(),
            "child_working_directory_outside_source": !child_cwd.path().starts_with(root.path()),
            "legacy_flat_snapshot": snapshot_report(&legacy),
        });
        println!("BA235_NATIVE_S18_RECEIPT={report}");
    }

    let default_base =
        dirs::cache_dir().expect("native Windows cache known folder").join("bsl-analyzer");
    let explicit_base = tempfile::tempdir().expect("disposable NTFS external cache");
    run_case(None, &default_base);
    assert!(default_base.exists(), "default native known-folder cache base was created");
    run_case(Some(explicit_base.path()), &default_base);
}

fn path(root: &Path, rel: &str) -> PathBuf {
    root.join(rel)
}

fn extension_metadata_fixture() -> PathBuf {
    PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../bsl-metadata/fixtures/extension_metadata"
    ))
}

fn assert_extension_metadata_fields(run: &Run, tail: &str, extension_visible: bool) {
    let unresolved = run.messages_at(tail, "UnresolvedField");
    let query = run.messages_at(tail, "UnknownFieldInQuery");
    for field in ["Номенклатура", "Количество"] {
        assert!(
            !unresolved.iter().any(|message| message.contains(field)),
            "{field} is inherited for {tail}; UnresolvedField: {unresolved:?}"
        );
        assert!(
            !query.iter().any(|message| message.contains(field)),
            "{field} is inherited for {tail}; UnknownFieldInQuery: {query:?}"
        );
    }
    for (code, messages) in [("UnresolvedField", &unresolved), ("UnknownFieldInQuery", &query)] {
        assert_eq!(
            messages.iter().any(|message| message.contains("РасшПоле")),
            !extension_visible,
            "extension visibility for {tail}; {code}: {messages:?}"
        );
    }
    assert!(
        unresolved.iter().any(|message| message.contains("НетТакогоПослеДобавить")),
        "the Добавить() control must fire for {tail}: {unresolved:?}"
    );
    assert!(
        query.iter().any(|message| message.contains("НетТакогоВЗапросе")),
        "the query control must fire for {tail}: {query:?}"
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn extension_metadata_cli_covers_document_fields_common_module_and_external() {
    let root = extension_metadata_fixture();
    let run = analyze(
        &root,
        &[
            "--configuration-root",
            "base",
            "--extension",
            "Расширение=extension",
            "--external",
            "АРМ=external",
        ],
    );

    assert_extension_metadata_fields(&run, "base/Documents/Заказ/Ext/ObjectModule.bsl", false);
    assert_extension_metadata_fields(&run, "extension/Documents/Заказ/Ext/ObjectModule.bsl", true);
    assert_extension_metadata_fields(&run, "external/АРМ/Ext/ObjectModule.bsl", true);

    assert!(
        run.messages_at("extension/CommonModules/Сервер/Ext/Module.bsl", "CommonModuleNameCached")
            .is_empty(),
        "the borrowed Сервер inherits DontUse"
    );
    assert_eq!(
        run.messages_at(
            "base/CommonModules/СерверЗапросов/Ext/Module.bsl",
            "CommonModuleNameCached"
        )
        .len(),
        1,
        "a cached module without the naming marker remains a positive control"
    );
    assert!(
        run.messages_at(
            "base/CommonModules/СерверПовтИсп/Ext/Module.bsl",
            "CommonModuleNameCached"
        )
        .is_empty(),
        "a cached module with the marker remains clean"
    );
    assert_eq!(
        run.messages_at(
            "extension/CommonModules/СерверЗапросов/Ext/Module.bsl",
            "CommonModuleNameCached"
        )
        .len(),
        1,
        "the borrowed module inherits DuringRequest"
    );
    assert_eq!(
        run.messages_at(
            "extension/CommonModules/СерверСеанса/Ext/Module.bsl",
            "CommonModuleNameCached"
        )
        .len(),
        1,
        "the borrowed module inherits DuringSession"
    );
    assert_eq!(
        run.messages_at(
            "extension/CommonModules/СерверНеизвестный/Ext/Module.bsl",
            "CommonModuleNameCached"
        )
        .len(),
        1,
        "explicit Unknown remains cached because only DontUse disables caching"
    );
    assert!(
        run.messages_at(
            "extension/CommonModules/СерверОтключаемый/Ext/Module.bsl",
            "CommonModuleNameCached"
        )
        .is_empty(),
        "explicit DontUse in the extension disables caching"
    );
    assert_eq!(
        run.messages_at(
            "extension/CommonModules/СерверВключаемый/Ext/Module.bsl",
            "CommonModuleNameCached"
        )
        .len(),
        1,
        "explicit DuringRequest in the extension enables caching"
    );
    let extension_unresolved =
        run.messages_at("extension/Documents/Заказ/Ext/ObjectModule.bsl", "UnresolvedField");
    assert!(
        !extension_unresolved.iter().any(|message| message.contains("Добавлено")),
        "the extension-only section field resolves: {extension_unresolved:?}"
    );
    assert!(
        extension_unresolved.iter().any(|message| message.contains("НетТакогоВРасшТаблице")),
        "the extension-only section row stays typed: {extension_unresolved:?}"
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn extension_metadata_cli_no_extensions_keeps_base_and_rejects_extension_fields() {
    let root = extension_metadata_fixture();
    let run = analyze(
        &root,
        &["--configuration-root", "base", "--no-extensions", "--external", "АРМ=external"],
    );

    assert_extension_metadata_fields(&run, "base/Documents/Заказ/Ext/ObjectModule.bsl", false);
    assert_extension_metadata_fields(&run, "external/АРМ/Ext/ObjectModule.bsl", false);
    assert!(
        run.file_event_at("extension/Documents/Заказ/Ext/ObjectModule.bsl").is_none(),
        "the extension module must not be analyzed under --no-extensions"
    );
    for tail in [
        "base/CommonModules/Сервер/Ext/Module.bsl",
        "base/CommonModules/СерверПовтИсп/Ext/Module.bsl",
    ] {
        assert!(run.messages_at(tail, "CommonModuleNameCached").is_empty());
    }
    assert_eq!(
        run.messages_at(
            "base/CommonModules/СерверЗапросов/Ext/Module.bsl",
            "CommonModuleNameCached"
        )
        .len(),
        1
    );
    assert!(
        run.files.iter().all(|event| {
            event["path"].as_str().is_none_or(|path| !path.contains("/extension/"))
        }),
        "no extension file may enter the source set: {:?}",
        run.files
    );

    let base_only = analyze(&root, &["--configuration-root", "base"]);
    assert_extension_metadata_fields(
        &base_only,
        "base/Documents/Заказ/Ext/ObjectModule.bsl",
        false,
    );
    assert!(base_only
        .messages_at("base/CommonModules/Сервер/Ext/Module.bsl", "CommonModuleNameCached")
        .is_empty());
    assert!(
        base_only.file_event_at("extension/Documents/Заказ/Ext/ObjectModule.bsl").is_none(),
        "an ordinary base-only run must not analyze extension modules"
    );
    assert!(
        base_only.files.iter().all(|event| {
            event["path"].as_str().is_none_or(|path| !path.contains("/extension/"))
        }),
        "an ordinary base-only run must exclude every extension file: {:?}",
        base_only.files
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn extension_metadata_cli_external_depends_on_empty_excludes_only_extension_metadata() {
    let root = extension_metadata_fixture();
    let run = analyze(
        &root,
        &[
            "--configuration-root",
            "base",
            "--extension",
            "Расширение=extension",
            "--external",
            "АРМ=external",
            "--external-depends-on",
            "АРМ=",
        ],
    );

    assert_extension_metadata_fields(&run, "external/АРМ/Ext/ObjectModule.bsl", false);
    assert_extension_metadata_fields(&run, "extension/Documents/Заказ/Ext/ObjectModule.bsl", true);
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn extension_metadata_cli_external_dependency_excludes_other_selected_extension() {
    let root = extension_metadata_fixture();
    let run = analyze(
        &root,
        &[
            "--configuration-root",
            "base",
            "--extension",
            "Расширение=extension",
            "--extension",
            "Зависимое=dependent",
            "--extension-depends-on",
            "Зависимое=Расширение",
            "--external",
            "АРМ=external",
            "--external-depends-on",
            "АРМ=Расширение",
        ],
    );

    assert_extension_metadata_fields(&run, "external/АРМ/Ext/ObjectModule.bsl", true);
    for code in ["UnresolvedField", "UnknownFieldInQuery"] {
        let messages = run.messages_at("external/АРМ/Ext/ObjectModule.bsl", code);
        assert!(
            messages.iter().any(|message| message.contains("ЗависимоеПоле")),
            "metadata from the excluded dependent extension must stay invisible; {code}: {messages:?}"
        );
    }
}

#[test]
fn shared_configuration_dependency_resolves_from_project_dotenv() {
    let dir = workspace();
    let project = dir.path().join("project");
    let shared = dir.path().join("configurations");
    std::fs::create_dir_all(&project).unwrap();

    write_configuration(
        &shared,
        "UT11/11.5.22.129",
        "ОсновнаяКонфигурация",
        MAIN_MODULE,
        "Функция Экспортируемая() Экспорт\n\tВозврат 1;\nКонецФункции\n",
        false,
    );
    write_configuration(
        &project,
        "Расширения/EXT",
        "Расширение",
        EXT_MODULE,
        &format!(
            "Процедура Вызвать() Экспорт\n\t{MAIN_MODULE}.Экспортируемая();\n\t{MISSING_CALL}\nКонецПроцедуры\n"
        ),
        true,
    );
    std::fs::write(
        project.join("bsl-analyzer.toml"),
        r#"[source]
extensions = ["Расширения/EXT"]

[source.configuration]
id = "UT11"
version = "11.5.22.129"
"#,
    )
    .unwrap();
    std::fs::write(
        project.join(".env"),
        format!("ONEC_CONFIGURATIONS_ROOT={}\n", shared.display()),
    )
    .unwrap();

    let run = analyze(&project, &[]);
    assert_eq!(
        run.unresolved_modules(EXT_MODULE),
        vec![MISSING_MODULE.to_string()],
        "the shared configuration must provide the base module context"
    );
}

#[test]
fn binding_the_main_configuration_resolves_calls_into_it() {
    let dir = workspace();
    workspace_calling_main_configuration(dir.path());

    // Positive control. Without a main configuration the call cannot resolve,
    // and this assertion is what makes the paired one below mean anything.
    let standalone = analyze(dir.path(), &["--extension", &format!("EXT={EXT}")]);
    assert_eq!(
        standalone.unresolved_modules(EXT_MODULE),
        vec![MAIN_MODULE.to_string(), MISSING_MODULE.to_string()],
        "alone, neither the main configuration's module nor the missing one resolves"
    );

    let bound =
        analyze(dir.path(), &["--configuration-root", MAIN, "--extension", &format!("EXT={EXT}")]);
    assert_eq!(
        bound.unresolved_modules(EXT_MODULE),
        vec![MISSING_MODULE.to_string()],
        "binding must resolve the main configuration's module and leave only the missing one"
    );
}

#[test]
fn a_declared_dependency_resolves_calls_between_extensions() {
    let dir = workspace();
    workspace_calling_main_configuration(dir.path());
    write_configuration(
        dir.path(),
        DEP,
        "Зависимость",
        DEP_MODULE,
        "Функция ИзЗависимости() Экспорт\n\tВозврат 2;\nКонецФункции\n",
        true,
    );
    std::fs::write(
        path(dir.path(), EXT).join("CommonModules").join(EXT_MODULE).join("Ext/Module.bsl"),
        format!(
            "Процедура Вызвать() Экспорт\n\t{DEP_MODULE}.ИзЗависимости();\n\t{MISSING_CALL}\nКонецПроцедуры\n"
        ),
    )
    .unwrap();

    let declared: Vec<String> = vec![
        "--configuration-root".into(),
        MAIN.into(),
        "--extension".into(),
        format!("DEP={DEP}"),
        "--extension".into(),
        format!("EXT={EXT}"),
    ];
    let refs: Vec<&str> = declared.iter().map(String::as_str).collect();

    // Independent extensions do not see each other, so this is the control.
    let unrelated = analyze(dir.path(), &refs);
    assert_eq!(
        unrelated.unresolved_modules(EXT_MODULE),
        vec![MISSING_MODULE.to_string(), DEP_MODULE.to_string()],
        "without a declared edge the other extension's module must stay invisible"
    );

    let mut with_edge = refs.clone();
    with_edge.extend(["--extension-depends-on", "EXT=DEP"]);
    let dependent = analyze(dir.path(), &with_edge);
    assert_eq!(
        dependent.unresolved_modules(EXT_MODULE),
        vec![MISSING_MODULE.to_string()],
        "the edge must resolve the dependency's module and leave only the missing one"
    );
}

#[test]
fn no_extensions_drops_the_configured_list() {
    let dir = workspace();
    workspace_calling_main_configuration(dir.path());
    std::fs::write(
        dir.path().join("bsl-analyzer.toml"),
        format!("[source]\nroot = \"{MAIN}\"\nextensions = [\"{EXT}\"]\n"),
    )
    .unwrap();

    // Paired control: the flag is only shown to remove the extension if the same
    // workspace analyzes it without the flag. A wiring bug that always dropped
    // configured extensions would satisfy the one-sided check.
    let configured = analyze(dir.path(), &[]);
    configured.analyzed(EXT_MODULE);

    let opted_out = analyze(dir.path(), &["--no-extensions"]);
    assert!(
        opted_out.file_event(EXT_MODULE).is_none(),
        "--no-extensions must drop the list the config declared"
    );
    // The flag drops extensions, not the analysis. Without this, a wiring bug
    // that cleared every source root would satisfy the assertion above by
    // analyzing nothing at all.
    opted_out.analyzed(MAIN_MODULE);
}

/// How many times the notice appears — the invariant is exactly one message per
/// run, and a substring check would pass just as happily on a duplicate.
fn notices(run: &Run) -> usize {
    // The run's own health is asserted here as well: the notice is printed
    // before the walk, so a per-file failure afterwards still leaves the phrase
    // in stderr while the process exits zero — and an analysis regression for
    // exactly the extension under test would slip through.
    assert_eq!(run.done["failed_files"], 0, "some file failed: {}", run.done);
    // Counted per line, because the message embeds the source path: a workspace
    // path containing the phrase would otherwise inflate the count.
    run.stderr
        .lines()
        .filter(|line| {
            line.contains("is a configuration extension analyzed without its main configuration")
        })
        .count()
}

#[test]
fn an_extension_taken_as_the_main_root_is_reported() {
    let dir = workspace();
    workspace_calling_main_configuration(dir.path());
    write_configuration(
        dir.path(),
        DEP,
        "Зависимость",
        DEP_MODULE,
        "Функция ИзЗависимости() Экспорт\n\tВозврат 2;\nКонецФункции\n",
        true,
    );

    // The case that needs no flag at all, and the one an integrator actually
    // hits: point `-s` straight at an extension. The notice is tied to the
    // resolved root, not to any flag, so narrowing it to the override path
    // would leave exactly this run silent in front of its false findings.
    assert_eq!(
        notices(&analyze(&path(dir.path(), EXT), &[])),
        1,
        "an extension given directly as the source dir must be called out once"
    );
    assert_eq!(
        notices(&analyze(&path(dir.path(), MAIN), &[])),
        0,
        "a main configuration given directly must stay silent"
    );

    // Only `--configuration-root` moves between the runs of each pair. Varying
    // the extension flags at the same time would let an implementation keyed on
    // `--no-extensions`, or on the list being empty, pass without ever asking
    // what the resolved root actually is.
    for extensions in [vec!["--no-extensions"], vec!["--extension", "DEP=a/b/dep"]] {
        let with_root = |root: &str| {
            let mut flags = vec!["--configuration-root", root];
            flags.extend(extensions.iter().copied());
            notices(&analyze(dir.path(), &flags))
        };

        assert_eq!(
            with_root(EXT),
            1,
            "an extension used as the main root must be called out once (extensions: {extensions:?})"
        );
        assert_eq!(
            with_root(MAIN),
            0,
            "a real main configuration must stay silent (extensions: {extensions:?})"
        );
    }
}

/// Drives `mcp serve` over stdio: handshake, wait for the resident database to
/// be ready, then one `diagnostics file` call. Returns the modules that stayed
/// unresolved, plus the `status` body that reported readiness.
///
/// The MCP path re-derives the project from a bare workspace path in a dozen
/// places, none of which can see argv. Nothing in the `analyze` checks above
/// would notice one of them left on the old source, so this asks the question
/// again on the channel an embedding host actually uses.
///
/// Readiness is polled rather than assumed: a freshly started server answers a
/// data action with a "still building" envelope that carries no diagnostics at
/// all, which would read exactly like "everything resolved".
/// One MCP server over stdio, driven by JSON-RPC a line at a time.
struct McpSession {
    child: Child,
    stdin: Option<std::process::ChildStdin>,
    stdout: Receiver<Option<String>>,
    stderr: StderrCapture,
    next_id: i64,
    _temporary_paths: Option<tempfile::TempDir>,
}

impl McpSession {
    fn start(workspace: &Path, flags: &[&str]) -> Self {
        let paths = tempfile::tempdir().unwrap();
        let cache = paths.path().join("cache");
        let state = paths.path().join("state");
        let mut session =
            Self::start_with_cache(workspace, flags, Some(&cache), Some(&state), None);
        session._temporary_paths = Some(paths);
        session
    }

    fn start_with_cache(
        workspace: &Path,
        flags: &[&str],
        cache_base: Option<&Path>,
        isolated_state: Option<&Path>,
        embedding_url: Option<&str>,
    ) -> Self {
        Self::start_transport(
            workspace,
            flags,
            "stdio",
            cache_base,
            None,
            isolated_state,
            embedding_url,
            Some("0"),
            None,
        )
    }

    // Keep each launch dimension explicit for native fixtures and avoid hidden defaults.
    #[allow(clippy::too_many_arguments)]
    fn start_transport(
        workspace: &Path,
        flags: &[&str],
        mode: &str,
        cache_base: Option<&Path>,
        default_cache_home: Option<&Path>,
        isolated_state: Option<&Path>,
        embedding_url: Option<&str>,
        broker_override: Option<&str>,
        backend_pid: Option<u32>,
    ) -> Self {
        Self::start_transport_with_cwd_and_log(
            workspace,
            flags,
            mode,
            cache_base,
            default_cache_home,
            isolated_state,
            embedding_url,
            broker_override,
            backend_pid,
            workspace,
            None,
        )
    }

    // These launch inputs stay explicit so each native fixture's cache, state, transport,
    // embedder, and owned-process relationship is visible at its call site.
    #[allow(clippy::too_many_arguments)]
    #[cfg(windows)]
    fn start_transport_with_cwd(
        workspace: &Path,
        flags: &[&str],
        mode: &str,
        cache_base: Option<&Path>,
        default_cache_home: Option<&Path>,
        isolated_state: Option<&Path>,
        embedding_url: Option<&str>,
        broker_override: Option<&str>,
        backend_pid: Option<u32>,
        child_cwd: &Path,
    ) -> Self {
        Self::start_transport_with_cwd_and_log(
            workspace,
            flags,
            mode,
            cache_base,
            default_cache_home,
            isolated_state,
            embedding_url,
            broker_override,
            backend_pid,
            child_cwd,
            None,
        )
    }

    // The extra inputs are required by the native broker fixtures and remain explicit above.
    #[allow(clippy::too_many_arguments)]
    fn start_transport_with_cwd_and_log(
        workspace: &Path,
        flags: &[&str],
        mode: &str,
        cache_base: Option<&Path>,
        default_cache_home: Option<&Path>,
        isolated_state: Option<&Path>,
        embedding_url: Option<&str>,
        broker_override: Option<&str>,
        backend_pid: Option<u32>,
        child_cwd: &Path,
        daemon_log_path: Option<&Path>,
    ) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_bsl-analyzer-app"));
        command
            .args(["mcp", "serve", "--profile", "workspace", "--mode", mode, "-s"])
            .arg(workspace)
            .args(flags)
            .env_remove("BSL_CACHE_DIR")
            .env_remove("XDG_CACHE_HOME")
            .env_remove("XDG_STATE_HOME")
            .env_remove("XDG_RUNTIME_DIR")
            .env_remove("EMBEDDING_URL")
            .env_remove("EMBEDDING_MODEL")
            .env_remove("EMBEDDING_DIM")
            .env_remove("EMBEDDING_API_KEY")
            .env_remove("EMBEDDING_PROVIDER")
            .env_remove("EMBEDDING_QUERY_PREFIX")
            .env_remove("EMBEDDING_DOCUMENT_PREFIX")
            .env_remove("BSL_LOG_FILE")
            .env_remove("BSL_MCP_DAEMON_LOG")
            .env("BSL_LOG", "info")
            .current_dir(child_cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(value) = broker_override {
            command.env("BSL_MCP_BROKER", value);
        } else {
            command.env_remove("BSL_MCP_BROKER");
        }
        if let Some(pid) = backend_pid {
            command.arg("--backend-pid").arg(pid.to_string());
        }
        if let Some(path) = daemon_log_path {
            command.env("BSL_MCP_DAEMON_LOG", path);
        }
        if mode == "broker" || mode == "daemon" {
            // A fixture-owned broker backend must not inherit the production
            // five-minute idle TTL after its last test client disconnects.
            command.env("BSL_MCP_IDLE_TTL_SECS", "2").env("BSL_MCP_ORPHAN_GRACE_SECS", "3");
        }
        if let Some(cache_base) = cache_base {
            command.env("BSL_CACHE_DIR", cache_base);
        }
        #[cfg(unix)]
        if let Some(cache_home) = default_cache_home {
            command.env("XDG_CACHE_HOME", cache_home);
        }
        #[cfg(not(unix))]
        let _ = default_cache_home;
        if let Some(state) = isolated_state {
            // dirs uses XDG state/runtime only on Unix; Windows S18 intentionally
            // keeps the native known folders untouched.
            #[cfg(unix)]
            {
                let runtime = state.join("runtime");
                assert_private_test_dir(&runtime);
                command.env("XDG_STATE_HOME", state).env("XDG_RUNTIME_DIR", runtime);
            }
            #[cfg(not(unix))]
            {
                #[cfg(windows)]
                command.env("LOCALAPPDATA", state);
                #[cfg(not(windows))]
                let _ = state;
            }
        }
        if let Some(url) = embedding_url {
            command
                .env("EMBEDDING_URL", url)
                .env("EMBEDDING_MODEL", "workspace-cache-fixture")
                .env("EMBEDDING_DIM", "3")
                .env("EMBEDDING_CONCURRENCY", "2");
        }
        let mut child = command.spawn().expect("failed to start the MCP server");
        let stderr = StderrCapture::attach(child.stderr.take().unwrap());
        let stdin = child.stdin.take().unwrap();
        let mut reader = BufReader::new(child.stdout.take().unwrap());
        let (stdout_tx, stdout) = mpsc::channel();
        std::thread::spawn(move || loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => {
                    let _ = stdout_tx.send(None);
                    break;
                }
                Ok(_) => {
                    if stdout_tx.send(Some(line)).is_err() {
                        break;
                    }
                }
                Err(err) => {
                    let _ = stdout_tx.send(Some(format!("__read_error__:{err}")));
                    break;
                }
            }
        });
        let mut session =
            Self { child, stdin: Some(stdin), stdout, next_id: 1, stderr, _temporary_paths: None };
        session.request(
            "initialize",
            serde_json::json!({"protocolVersion": "2024-11-05", "capabilities": {},
                               "clientInfo": {"name": "t", "version": "1"}}),
        );
        session.send(serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        session
    }

    fn send(&mut self, value: Value) {
        use std::io::Write as _;
        writeln!(self.stdin.as_mut().expect("the session is open"), "{value}").unwrap();
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(
            serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}),
        );
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let line = self.stdout.recv_timeout(remaining).unwrap_or_else(|err| {
                panic!(
                    "MCP {method} request exceeded 30 seconds: {err}; stderr tail: {}",
                    self.stderr.snapshot()
                )
            });
            let line = line.unwrap_or_else(|| {
                panic!(
                    "the server closed stdout during {method}; stderr tail: {}",
                    self.stderr.snapshot()
                )
            });
            assert!(!line.starts_with("__read_error__:"), "failed to read MCP stdout: {line}");
            if let Ok(message) = serde_json::from_str::<Value>(&line) {
                if message["id"] == id {
                    return message;
                }
            }
        }
    }

    /// A tool call's reply, whole.
    fn call(&mut self, tool: &str, arguments: Value) -> Value {
        self.request("tools/call", serde_json::json!({"name": tool, "arguments": arguments}))
    }

    /// Poll `tool`'s `status` action until its state settles; the settled status.
    fn wait_ready(&mut self, tool: &str) -> Value {
        let mut status = Value::Null;
        for _ in 0..300 {
            status = self.call(tool, serde_json::json!({"action": "status"}))["result"]
                ["structuredContent"]
                .clone();
            if status["state"] == "ready" || status["state"] == "failed" {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert_eq!(status["state"], "ready", "{tool} never became ready: {status}");
        status
    }

    /// One `diagnostics` call that must succeed; its reply, whole.
    fn diagnostics(&mut self, arguments: Value) -> Value {
        let reply = self.call("diagnostics", arguments);
        let body = reply.to_string();
        assert!(!body.contains("\"isError\":true"), "the diagnostics call failed: {body}");
        reply
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    #[cfg(unix)]
    fn cache_adoption_evidence(&self) -> Vec<String> {
        self.stderr.wait_for_cache_adoption()
    }

    #[cfg(unix)]
    fn stderr_snapshot(&self) -> String {
        self.stderr.snapshot()
    }

    fn is_running(&mut self) -> bool {
        self.child.try_wait().expect("poll MCP process").is_none()
    }

    fn shutdown(&mut self) -> std::io::Result<ExitStatus> {
        drop(self.stdin.take());
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = self.child.try_wait()? {
                return Ok(status);
            }
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                let _ = self.child.wait()?;
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "MCP shutdown exceeded 5 seconds",
                ));
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

impl Drop for McpSession {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.shutdown();
        }
    }
}

fn mcp_probe(workspace: &Path, module_file: &Path, flags: &[&str]) -> (Vec<String>, Value) {
    let mut session = McpSession::start(workspace, flags);
    let status = session.wait_ready("diagnostics");
    let body = session
        .diagnostics(
            serde_json::json!({"action": "file", "path": module_file.display().to_string()}),
        )
        .to_string();

    let mut names: Vec<String> = body
        .match_indices("разрешить получателя вызова '")
        .map(|(at, needle)| {
            let rest = &body[at + needle.len()..];
            rest[..rest.find('\'').unwrap_or(0)].to_owned()
        })
        .collect();
    names.sort();
    names.dedup();
    (names, status)
}

/// The graph's own view of the source set: node and edge counts once its build
/// settles.
///
/// Graph passes re-derive the project from a bare workspace path, separately
/// from the resident diagnostics host. A regression that left that path on the
/// on-disk config would keep every diagnostics check green while the graph was
/// built over a different set of roots.
fn mcp_graph_overview(workspace: &Path, flags: &[&str]) -> Value {
    let mut session = McpSession::start(workspace, flags);
    // `failed` is a real outcome here, not a flake: the builder reports it when
    // it panicked, and an overview read past it would compare empty to empty.
    session.wait_ready("graph");
    session.call("graph", serde_json::json!({"action": "overview"}))["result"]["structuredContent"]
        ["result"]
        .clone()
}

#[test]
fn the_source_set_reaches_the_graph() {
    let dir = workspace();
    workspace_calling_main_configuration(dir.path());

    // Both runs bind the main configuration, so the only thing moving is whether
    // the extension is part of the set — which is exactly what the graph passes
    // re-derive for themselves.
    let base_only =
        mcp_graph_overview(dir.path(), &["--configuration-root", MAIN, "--no-extensions"]);
    assert_eq!(base_only["nodes"], 1, "only the main configuration's method: {base_only}");
    assert_eq!(base_only["edges"], 0, "nothing calls it: {base_only}");

    let with_extension = mcp_graph_overview(
        dir.path(),
        &["--configuration-root", MAIN, "--extension", "EXT=a/b/ext"],
    );
    assert_eq!(with_extension["nodes"], 2, "both methods: {with_extension}");
    assert_eq!(
        with_extension["edge_provenance"]["resolved"], 1,
        "the extension's call into the main configuration must resolve into an edge: \
         {with_extension}"
    );
}

/// Two configuration directories deep enough that discovery finds neither, and
/// no flags binding them: the workspace itself becomes the only declared root,
/// while each module still attributes to the nested directory that holds its
/// metadata. The build's pre-pool warm-up covers declared roots, so a second
/// attributed root reaches the whole-config loader lazily — from inside the
/// worker pool, where its fan-out may deadlock the build.
///
/// One such configuration is not enough to show this: the single module doubles
/// as the batch representative the warm-up already touches.
#[test]
fn nested_configurations_under_a_bare_workspace_still_build_a_graph() {
    let dir = workspace();
    workspace_calling_main_configuration(dir.path());

    let overview = mcp_graph_overview(dir.path(), &[]);
    assert_eq!(overview["nodes"], 2, "both nested configurations' methods: {overview}");
}

#[test]
fn the_mcp_status_reports_a_standalone_extension() {
    let dir = workspace();
    workspace_calling_main_configuration(dir.path());
    let module =
        path(dir.path(), EXT).join("CommonModules").join(EXT_MODULE).join("Ext").join("Module.bsl");

    // Pointed straight at the extension, with no main configuration behind it —
    // the state in which this backend's findings are wrong and nothing else in
    // the protocol says why.
    let (_, standalone) = mcp_probe(&path(dir.path(), EXT), &module, &[]);
    assert!(
        standalone["standalone_extension"]
            .as_str()
            .is_some_and(|s| s.contains("configuration extension analyzed without")),
        "status must carry the notice: {standalone}"
    );

    let (_, bound) = mcp_probe(
        dir.path(),
        &module,
        &["--configuration-root", MAIN, "--extension", "EXT=a/b/ext"],
    );
    assert!(
        bound.get("standalone_extension").is_none(),
        "a bound main configuration must leave the field out: {bound}"
    );
}

#[test]
fn the_source_set_reaches_the_mcp_server() {
    let dir = workspace();
    workspace_calling_main_configuration(dir.path());
    let module =
        path(dir.path(), EXT).join("CommonModules").join(EXT_MODULE).join("Ext").join("Module.bsl");

    assert_eq!(
        mcp_probe(dir.path(), &module, &[]).0,
        vec![MAIN_MODULE.to_string(), MISSING_MODULE.to_string()],
        "without a source set the main configuration's module cannot resolve"
    );
    assert_eq!(
        mcp_probe(
            dir.path(),
            &module,
            &["--configuration-root", MAIN, "--extension", "EXT=a/b/ext"]
        )
        .0,
        vec![MISSING_MODULE.to_string()],
        "the source set must resolve the main configuration's module here too"
    );
}

/// An external data processor export: `<Name>.xml` beside `<Name>/`, with one
/// managed form whose module is `body`. The same tree the designer writes,
/// minus everything the analysis does not read.
const EPF: &str = "a/b/epf";
const EPF_NAME: &str = "АРМ";
const EPF_FORM_MODULE: &str = "АРМ/Forms/Форма/Ext/Form/Module.bsl";

fn write_external(root: &Path, rel: &str, name: &str, body: &str) {
    write_external_with_attribute(root, rel, name, body, None);
}

const VALID_EXTERNAL_BODY: &str =
    "Процедура Проверить() Экспорт\n    Значение = 1;\nКонецПроцедуры\n";
const BROKEN_EXTERNAL_BODY: &str =
    "Процедура Проверить() Экспорт\n    Значение = ;\nКонецПроцедуры\n";

/// Both kinds, including an ordinary form with an available textual module.
/// `managed` is a container, not an export root that discovery may recurse into.
fn nested_external_modules(root: &Path) -> Vec<PathBuf> {
    workspace_calling_main_configuration(root);
    let mut modules = Vec::new();
    let mut declarations = Vec::new();
    for (folder, name, kind) in
        [("epf", "Обработка", "ExternalDataProcessor"), ("erf", "Отчёт", "ExternalReport")]
    {
        let relative = format!("src/{folder}/managed/{name}");
        let dir = root.join(&relative);
        write_external(root, &relative, name, BROKEN_EXTERNAL_BODY);
        std::fs::write(dir.join(format!("{name}.xml")), processor_xml(kind, name, None)).unwrap();
        let object = dir.join(name).join("Ext/ObjectModule.bsl");
        std::fs::create_dir_all(object.parent().unwrap()).unwrap();
        std::fs::write(&object, BROKEN_EXTERNAL_BODY).unwrap();
        let ordinary = dir.join(name).join("Forms/Обычная/Ext/Form/Module.bsl");
        std::fs::create_dir_all(ordinary.parent().unwrap()).unwrap();
        std::fs::write(&ordinary, BROKEN_EXTERNAL_BODY).unwrap();
        let form_xml = std::fs::read_to_string(dir.join(name).join("Forms/Форма.xml")).unwrap();
        std::fs::write(
            dir.join(name).join("Forms/Обычная.xml"),
            form_xml
                .replace("<Name>Форма</Name>", "<Name>Обычная</Name>")
                .replace("<FormType>Managed</FormType>", "<FormType>Ordinary</FormType>"),
        )
        .unwrap();
        let xml = processor_xml(kind, name, None)
            .replace("<Form>Форма</Form>", "<Form>Форма</Form><Form>Обычная</Form>");
        std::fs::write(dir.join(format!("{name}.xml")), xml).unwrap();
        modules.extend([object, dir.join(name).join("Forms/Форма/Ext/Form/Module.bsl"), ordinary]);
        declarations.push(format!("{{ name = \"{name}\", path = \"{relative}\" }}"));
    }
    std::fs::write(
        root.join("bsl-analyzer.toml"),
        format!(
            "[source]\nroot = \"{MAIN}\"\nextensions = []\nexternals = [{}]\n",
            declarations.join(",")
        ),
    )
    .unwrap();
    modules.into_iter().map(|module| module.canonicalize().unwrap()).collect()
}

fn external_diagnostics(session: &mut McpSession, path: &Path) -> Value {
    session.diagnostics(serde_json::json!({"action": "file", "path": path,
        "codes": ["ParseError"]}))["result"]["structuredContent"]
        .clone()
}

fn wait_external_diagnostics(
    session: &mut McpSession,
    path: &Path,
    accept: impl Fn(&Value) -> bool,
) -> Value {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let body = external_diagnostics(session, path);
        if body["stale"] == false && accept(&body) {
            return body;
        }
        assert!(std::time::Instant::now() < deadline, "{} never settled: {body}", path.display());
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

fn wait_file_diagnostics(
    session: &mut McpSession,
    path: &Path,
    accept: impl Fn(&Value) -> bool,
) -> Value {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let body = session.diagnostics(serde_json::json!({
            "action": "file",
            "path": path.display().to_string(),
            "codes": ["UnknownFieldInQuery"]
        }))["result"]["structuredContent"]
            .clone();
        if body["stale"] == false && accept(&body) {
            return body;
        }
        assert!(std::time::Instant::now() < deadline, "{} never settled: {body}", path.display());
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

fn wait_query_semantics(session: &mut McpSession, query: &str, root_id: &str) -> Value {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let reply = session.call(
            "query",
            serde_json::json!({"action": "validate", "query": query, "root_id": root_id}),
        );
        assert!(reply["error"].is_null(), "query validate failed: {reply}");
        let answer = reply["result"]["structuredContent"].clone();
        if answer["results"][0]["backend"] == "workspace_semantics" {
            return answer;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "query semantics never became ready: {answer}"
        );
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

fn document_configuration_xml(name: &str, extension: bool) -> String {
    let purpose = if extension {
        "<ConfigurationExtensionPurpose>Customization</ConfigurationExtensionPurpose>"
    } else {
        ""
    };
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses"><Configuration uuid="11111111-0000-0000-0000-000000000001"><Properties><Name>{name}</Name>{purpose}</Properties><ChildObjects><Document>Документ1</Document></ChildObjects></Configuration></MetaDataObject>"#
    )
}

fn effective_document_xml(extension: bool, base_attribute: &str) -> String {
    let identity = if extension {
        "<ObjectBelonging>Adopted</ObjectBelonging><ExtendedConfigurationObject>11111111-1111-1111-1111-111111111111</ExtendedConfigurationObject>"
    } else {
        ""
    };
    let uuid = if extension {
        "22222222-2222-2222-2222-222222222222"
    } else {
        "11111111-1111-1111-1111-111111111111"
    };
    let attribute = if extension { "ДобавленноеПоле" } else { base_attribute };
    let attribute_uuid = if extension {
        "66666666-6666-6666-6666-666666666666"
    } else {
        "44444444-4444-4444-4444-444444444444"
    };
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:v8="http://v8.1c.ru/8.1/data/core" version="2.20">
<Document uuid="{uuid}"><Properties><Name>Документ1</Name>{identity}</Properties><ChildObjects>
<TabularSection uuid="33333333-3333-3333-3333-333333333333"><Properties><Name>Товары</Name></Properties><ChildObjects>
<Attribute uuid="{attribute_uuid}"><Properties><Name>{attribute}</Name><Type><v8:Type>xs:string</v8:Type></Type></Properties></Attribute>
</ChildObjects></TabularSection></ChildObjects></Document></MetaDataObject>"#
    )
}

/// Editing a base attribute while the resident is open must refresh both its file
/// diagnostics and query validation. The adopted extension keeps its own added field
/// through the drift and after the base is restored.
#[test]
fn adopted_document_base_attribute_drift_reaches_resident_diagnostics_and_query_validate() {
    let dir = workspace();
    let base = path(dir.path(), MAIN);
    let extension = path(dir.path(), EXT);
    for (root, config_name, is_extension) in
        [(&base, "ОсновнаяКонфигурация", false), (&extension, "Расширение", true)]
    {
        std::fs::create_dir_all(root.join("Documents")).unwrap();
        std::fs::write(
            root.join("Configuration.xml"),
            document_configuration_xml(config_name, is_extension),
        )
        .unwrap();
        std::fs::write(
            root.join("Documents/Документ1.xml"),
            effective_document_xml(is_extension, "БазовоеПоле"),
        )
        .unwrap();
    }
    let module = extension.join("Documents/Документ1/Ext/ManagerModule.bsl");
    std::fs::create_dir_all(module.parent().unwrap()).unwrap();
    let query = "ВЫБРАТЬ Т.БазовоеПоле, Т.ДобавленноеПоле ИЗ Документ.Документ1.Товары КАК Т";
    let source = format!(
        "Процедура Проверить() Экспорт\n    Запрос = Новый Запрос;\n    Запрос.Текст = \"{query}\";\n    Запрос.Выполнить();\nКонецПроцедуры\n"
    );
    std::fs::write(&module, source).unwrap();
    std::fs::write(
        dir.path().join("bsl-analyzer.toml"),
        format!(
            "[source]\nroot = \"{MAIN}\"\nextensions = [{{ name = \"EXT\", path = \"{EXT}\" }}]\n"
        ),
    )
    .unwrap();

    let flags = ["--configuration-root", MAIN, "--extension", "EXT=a/b/ext"];
    let mut session = McpSession::start(dir.path(), &flags);
    session.wait_ready("diagnostics");

    let initial = wait_file_diagnostics(&mut session, &module, |answer| {
        answer["result"]["kind"] == "full"
            && answer["result"]["findings"].as_array().is_some_and(Vec::is_empty)
    });
    session.wait_ready("search");
    let search_reply = session.call(
        "search",
        serde_json::json!({"action": "search_code", "query": "БазовоеПоле", "limit": 50}),
    );
    assert!(search_reply["error"].is_null(), "search_code failed: {search_reply}");
    let hits = search_reply["result"]["structuredContent"]["hits"]
        .as_array()
        .expect("search_code returns structured hits");
    let module_root_ids: Vec<_> = hits
        .iter()
        .filter(|hit| {
            hit["path"].as_str().is_some_and(|path| {
                Path::new(path).ends_with("Documents/Документ1/Ext/ManagerModule.bsl")
            })
        })
        .map(|hit| hit["root_id"].as_str().expect("code hit names its source root"))
        .collect();
    assert!(!module_root_ids.is_empty(), "search finds the module's indexed query text: {hits:?}");
    assert!(
        module_root_ids.iter().all(|root_id| *root_id == module_root_ids[0]),
        "all module hits name the same source root: {module_root_ids:?}"
    );
    let extension_root_id = module_root_ids[0].to_owned();
    let initial_query = wait_query_semantics(&mut session, query, &extension_root_id);
    assert!(
        initial_query["results"][0]["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .all(|finding| finding["code"] != "UnknownFieldInQuery"),
        "base and adopted fields should both resolve: {initial_query}"
    );
    assert_eq!(initial_query["context"]["asserted_root_id"], extension_root_id);
    let missing_query = "ВЫБРАТЬ Т.НетТакогоПоля ИЗ Документ.Документ1.Товары КАК Т";
    let negative = wait_query_semantics(&mut session, missing_query, &extension_root_id);
    assert!(
        negative["results"][0]["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|finding| finding["code"] == "UnknownFieldInQuery"),
        "negative control proves workspace metadata semantics are active: {negative}"
    );

    let base_xml = base.join("Documents/Документ1.xml");
    let base_before = std::fs::read_to_string(&base_xml).unwrap();
    let drifted = base_before.replace("БазовоеПоле", "ПереименованноеПоле");
    std::fs::write(&base_xml, drifted).unwrap();
    let drift_diagnostics = wait_file_diagnostics(&mut session, &module, |answer| {
        answer["revision"].as_u64() > initial["revision"].as_u64()
            && answer["result"]["findings"].as_array().is_some_and(|findings| {
                findings.iter().any(|finding| finding["code"] == "UnknownFieldInQuery")
            })
    });
    let drift_query = wait_query_semantics(&mut session, query, &extension_root_id);
    assert!(
        drift_query["results"][0]["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|finding| finding["code"] == "UnknownFieldInQuery"),
        "query validation must agree with fresh diagnostics after base drift: {drift_query}"
    );
    assert!(drift_diagnostics["revision"].as_u64() > initial["revision"].as_u64());

    std::fs::write(&base_xml, base_before).unwrap();
    let restored = wait_file_diagnostics(&mut session, &module, |answer| {
        answer["revision"].as_u64() > drift_diagnostics["revision"].as_u64()
            && answer["result"]["findings"].as_array().is_some_and(Vec::is_empty)
    });
    let restored_query = wait_query_semantics(&mut session, query, &extension_root_id);
    assert!(
        restored_query["results"][0]["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .all(|finding| finding["code"] != "UnknownFieldInQuery"),
        "restoring the base field must restore the effective query schema: {restored_query}"
    );
    assert!(restored["revision"].as_u64() > drift_diagnostics["revision"].as_u64());
}

#[test]
fn nested_external_text_modules_have_cli_and_mcp_parse_errors_and_recover() {
    let dir = workspace();
    let modules = nested_external_modules(dir.path());
    let config_path = dir.path().join("bsl-analyzer.toml");
    let explicit_config = std::fs::read_to_string(&config_path).unwrap();
    std::fs::write(&config_path, format!("[source]\nroot = \"{MAIN}\"\nextensions = []\n"))
        .unwrap();
    let undiscovered = analyze(dir.path(), &[]);
    for module in &modules {
        assert!(undiscovered.file_event_at(module.to_str().unwrap()).is_none());
    }
    assert!(undiscovered.stderr.contains("managed"), "{}", undiscovered.stderr);
    std::fs::write(config_path, explicit_config).unwrap();
    let disabled = analyze(dir.path(), &["--no-externals"]);
    for module in &modules {
        assert!(disabled.file_event_at(module.to_str().unwrap()).is_none());
    }

    let broken = analyze(dir.path(), &[]);
    let mut session = McpSession::start(dir.path(), &[]);
    session.wait_ready("diagnostics");
    let mut initial = Vec::new();
    for module in &modules {
        assert!(!broken.messages_at(module.to_str().unwrap(), "ParseError").is_empty());
        let answer = external_diagnostics(&mut session, module);
        assert_eq!(answer["stale"], false, "{answer}");
        assert_eq!(answer["result"]["kind"], "full", "{answer}");
        assert_eq!(answer["result"]["truncated"], false, "{answer}");
        let findings =
            answer["result"]["findings"].as_array().expect("findings, not not_in_workspace");
        assert!(!findings.is_empty(), "{answer}");
        let cli_findings: Vec<_> = broken.analyzed_at(module.to_str().unwrap())["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|finding| finding["code"] == "ParseError")
            .collect();
        assert_eq!(findings.len(), cli_findings.len());
        for (finding, cli) in findings.iter().zip(cli_findings) {
            assert_eq!(finding["code"], "ParseError", "{finding}");
            assert!(finding["location"]["root_id"].is_string(), "{finding}");
            for coordinate in ["start_line", "start_column", "end_line", "end_column"] {
                assert_eq!(finding["range"][coordinate], cli[coordinate]);
            }
        }
        let rooted = session.diagnostics(serde_json::json!({
            "action": "file", "root_id": findings[0]["location"]["root_id"],
            "path": findings[0]["location"]["path"], "codes": ["ParseError"]
        }));
        assert_eq!(
            rooted["result"]["structuredContent"]["result"]["result_id"],
            answer["result"]["result_id"],
            "the published root/path must name this module"
        );
        initial.push(answer);
    }
    for module in &modules {
        std::fs::write(module, VALID_EXTERNAL_BODY).unwrap();
    }
    let corrected = analyze(dir.path(), &[]);
    for (module, before) in modules.iter().zip(initial) {
        assert!(corrected.messages_at(module.to_str().unwrap(), "ParseError").is_empty());
        wait_external_diagnostics(&mut session, module, |answer| {
            answer["revision"].as_u64() > before["revision"].as_u64()
                && answer["result"]["kind"] == "full"
                && answer["result"]["result_id"] != before["result"]["result_id"]
                && answer["result"]["findings"].as_array().is_some_and(Vec::is_empty)
        });
    }
}

#[test]
fn nested_external_module_addition_and_removal_reach_the_resident() {
    let dir = workspace();
    let modules = nested_external_modules(dir.path());
    let module = modules.last().unwrap();
    std::fs::remove_file(module).unwrap();
    let mut session = McpSession::start(dir.path(), &[]);
    session.wait_ready("diagnostics");
    let missing = external_diagnostics(&mut session, module);
    assert_eq!(missing["result"]["error"], "not_in_workspace", "{missing}");
    std::fs::write(module, BROKEN_EXTERNAL_BODY).unwrap();
    let added = wait_external_diagnostics(&mut session, module, |answer| {
        answer["result"]["kind"] == "full"
            && answer["result"]["findings"].as_array().is_some_and(|v| !v.is_empty())
    });
    std::fs::remove_file(module).unwrap();
    wait_external_diagnostics(&mut session, module, |answer| {
        answer["revision"].as_u64() > added["revision"].as_u64()
            && answer["result"]["error"] == "not_in_workspace"
    });
}

#[test]
fn a_binary_external_is_refused_instead_of_reported_clean() {
    let dir = workspace();
    std::fs::write(dir.path().join("Binary.epf"), [0u8, 1, 2, 3]).unwrap();
    let error = analyze_refuses(dir.path(), &["--external", "Binary=Binary.epf"]);
    assert!(error.contains("Binary"), "the refused input must be named: {error}");
}

#[test]
fn native_syntax_regressions_have_matching_cli_and_mcp_ranges() {
    let dir = workspace();
    let modules = nested_external_modules(dir.path());
    for module in &modules {
        std::fs::write(module, VALID_EXTERNAL_BODY).unwrap();
    }
    let module = &modules[0];
    let mut session = McpSession::start(dir.path(), &[]);
    session.wait_ready("diagnostics");
    let mut before = external_diagnostics(&mut session, module);
    // These fragment pairs were checked on platform 8.3.27.1989 in demo.
    for (bad, good) in [
        ("Если Истина Тогда\nКонецЕслли;", "Если Истина Тогда\nКонецЕсли;"),
        (
            "ПолныйПуть = Новый Файл(\"a\").ПолноеИмя;",
            "Файл = Новый Файл(\"a\");\nПолныйПуть = Файл.ПолноеИмя;",
        ),
        (
            "Если Новый Файл(\"a\").Существует() Тогда\nКонецЕсли;",
            "Файл = Новый Файл(\"a\");\nЕсли Файл.Существует() Тогда\nКонецЕсли;",
        ),
    ] {
        for (body, broken) in [(bad, true), (good, false)] {
            std::fs::write(
                module,
                format!("Процедура Проверить() Экспорт\n{body}\nКонецПроцедуры\n"),
            )
            .unwrap();
            let answer = wait_external_diagnostics(&mut session, module, |answer| {
                answer["revision"].as_u64() > before["revision"].as_u64()
                    && answer["result"]["kind"] == "full"
                    && answer["result"]["findings"]
                        .as_array()
                        .is_some_and(|findings| findings.is_empty() != broken)
            });
            let cli = analyze(dir.path(), &[]);
            let cli_findings: Vec<_> = cli.analyzed_at(module.to_str().unwrap())["diagnostics"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|finding| finding["code"] == "ParseError")
                .collect();
            let findings = answer["result"]["findings"].as_array().unwrap();
            assert_eq!(findings.len(), cli_findings.len(), "{body}");
            assert_eq!(answer["result"]["truncated"], false);
            for (finding, cli) in findings.iter().zip(cli_findings) {
                for coordinate in ["start_line", "start_column", "end_line", "end_column"] {
                    assert_eq!(finding["range"][coordinate], cli[coordinate], "{body}");
                }
            }
            before = answer;
        }
    }
}

/// The processor's XML, internal or external: `element` is the object element and
/// `attribute`, when given, one string attribute of the object.
fn processor_xml(element: &str, name: &str, attribute: Option<&str>) -> String {
    let attribute = attribute.map_or(String::new(), |attribute| {
        format!(
            r#"<Attribute uuid="d010948a-27f1-4b21-80a2-361efec05def"><Properties><Name>{attribute}</Name><Type><v8:Type>xs:string</v8:Type></Type></Properties></Attribute>"#
        )
    });
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:v8="http://v8.1c.ru/8.1/data/core" version="2.20">
	<{element} uuid="3696c164-ad14-4a0d-b659-10e3bf6d6ad2">
		<Properties><Name>{name}</Name><Synonym/><Comment/><DefaultForm>{element}.{name}.Form.Форма</DefaultForm></Properties>
		<ChildObjects>{attribute}<Form>Форма</Form></ChildObjects>
	</{element}>
</MetaDataObject>"#
    )
}

fn write_external_with_attribute(
    root: &Path,
    rel: &str,
    name: &str,
    body: &str,
    attribute: Option<&str>,
) {
    let dir = root.join(rel);
    let form_dir = dir.join(name).join("Forms").join("Форма").join("Ext").join("Form");
    std::fs::create_dir_all(&form_dir).unwrap();
    std::fs::write(
        dir.join(format!("{name}.xml")),
        processor_xml("ExternalDataProcessor", name, attribute),
    )
    .unwrap();
    std::fs::write(
        dir.join(name).join("Forms").join("Форма.xml"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:v8="http://v8.1c.ru/8.1/data/core" version="2.20">
	<Form uuid="8919791a-5b27-410f-9404-010ce96c6db6">
		<Properties><Name>Форма</Name><Synonym/><Comment/><FormType>Managed</FormType></Properties>
	</Form>
</MetaDataObject>"#,
    )
    .unwrap();
    std::fs::write(
        form_dir.parent().unwrap().join("Form.xml"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<Form xmlns="http://v8.1c.ru/8.3/xcf/logform" xmlns:v8="http://v8.1c.ru/8.1/data/core" version="2.20">
	<AutoCommandBar name="ФормаКоманднаяПанель" id="-1"/>
	<Attributes/>
</Form>"#,
    )
    .unwrap();
    std::fs::write(form_dir.join("Module.bsl"), body).unwrap();
}

/// Runs `analyze` expecting it to refuse, and returns its stderr.
fn analyze_refuses(source_dir: &Path, flags: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_bsl-analyzer-app"))
        .arg("analyze")
        .arg("-s")
        .arg(source_dir)
        .args(flags)
        .args(["--format", "jsonl"])
        .env_remove("ONEC_CONFIGURATIONS_ROOT")
        .output()
        .expect("failed to run the analyzer");
    assert!(!output.status.success(), "analyze {flags:?} must refuse to start");
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn external_notices(run: &Run) -> usize {
    assert_eq!(run.done["failed_files"], 0, "some file failed: {}", run.done);
    run.stderr
        .lines()
        .filter(|line| line.contains("are analyzed without an owning configuration"))
        .count()
}

#[test]
fn binding_the_main_configuration_resolves_calls_from_an_external_object() {
    let dir = workspace();
    workspace_calling_main_configuration(dir.path());
    write_external(
        dir.path(),
        EPF,
        EPF_NAME,
        &format!(
            "&НаСервере\nПроцедура ПриСозданииНаСервере(Отказ, СтандартнаяОбработка)\n\t{MAIN_MODULE}.Экспортируемая();\n\t{MISSING_CALL}\nКонецПроцедуры\n"
        ),
    );
    let external = format!("{EPF_NAME}={EPF}");

    // Positive control: alone, the owning configuration's module is unresolved
    // and the run says why. Without this the bound run's clean result could
    // equally mean the form module was never analyzed.
    let standalone = analyze(dir.path(), &["--external", &external]);
    assert_eq!(
        standalone.unresolved_modules_at(EPF_FORM_MODULE),
        vec![MAIN_MODULE.to_string(), MISSING_MODULE.to_string()],
    );
    assert_eq!(external_notices(&standalone), 1, "the missing owner is called out once");

    let bound = analyze(dir.path(), &["--configuration-root", MAIN, "--external", &external]);
    assert_eq!(
        bound.unresolved_modules_at(EPF_FORM_MODULE),
        vec![MISSING_MODULE.to_string()],
        "the owning configuration's module resolves and only the missing one remains"
    );
    assert_eq!(external_notices(&bound), 0, "bound, there is nothing to announce");
}

#[test]
fn an_external_object_sees_every_extension_while_the_base_sees_none() {
    let dir = workspace();
    workspace_calling_main_configuration(dir.path());
    // The extension exports a method; the base calls it too, as the control:
    // an extension's API is invisible from the base, so the same call that
    // resolves from the external object must stay unresolved there.
    std::fs::write(
        path(dir.path(), EXT).join("CommonModules").join(EXT_MODULE).join("Ext/Module.bsl"),
        "Функция ИзРасширения() Экспорт\n\tВозврат 3;\nКонецФункции\n",
    )
    .unwrap();
    std::fs::write(
        path(dir.path(), MAIN).join("CommonModules").join(MAIN_MODULE).join("Ext/Module.bsl"),
        format!(
            "Функция Экспортируемая() Экспорт\n\tВозврат 1;\nКонецФункции\n\
             Процедура Контроль()\n\t{EXT_MODULE}.ИзРасширения();\nКонецПроцедуры\n"
        ),
    )
    .unwrap();
    write_external(
        dir.path(),
        EPF,
        EPF_NAME,
        &format!(
            "&НаСервере\nПроцедура ПриСозданииНаСервере(Отказ, СтандартнаяОбработка)\n\t{EXT_MODULE}.ИзРасширения();\n\t{MISSING_CALL}\nКонецПроцедуры\n"
        ),
    );
    let external = format!("{EPF_NAME}={EPF}");
    // No dependency is declared anywhere: the external object sees the
    // extension by construction, not by an edge.
    let run = analyze(
        dir.path(),
        &["--configuration-root", MAIN, "--extension", "EXT=a/b/ext", "--external", &external],
    );

    assert_eq!(
        run.unresolved_modules(MAIN_MODULE),
        vec![EXT_MODULE.to_string()],
        "control: the base does not see the extension"
    );
    assert_eq!(
        run.unresolved_modules_at(EPF_FORM_MODULE),
        vec![MISSING_MODULE.to_string()],
        "the external object sees the extension without any declared edge"
    );
}

#[test]
fn an_external_depends_on_narrows_what_it_sees_to_the_named_extensions() {
    let dir = workspace();
    workspace_calling_main_configuration(dir.path());
    std::fs::write(
        path(dir.path(), EXT).join("CommonModules").join(EXT_MODULE).join("Ext/Module.bsl"),
        "Функция ИзРасширения() Экспорт\n\tВозврат 3;\nКонецФункции\n",
    )
    .unwrap();
    write_configuration(
        dir.path(),
        DEP,
        "Зависимость",
        DEP_MODULE,
        "Функция ИзЗависимости() Экспорт\n\tВозврат 4;\nКонецФункции\n",
        true,
    );
    write_external(
        dir.path(),
        EPF,
        EPF_NAME,
        &format!(
            "&НаСервере\nПроцедура ПриСозданииНаСервере(Отказ, СтандартнаяОбработка)\n\t{MAIN_MODULE}.Экспортируемая();\n\t{EXT_MODULE}.ИзРасширения();\n\t{DEP_MODULE}.ИзЗависимости();\n\t{MISSING_CALL}\nКонецПроцедуры\n"
        ),
    );
    let external = format!("{EPF_NAME}={EPF}");
    let flags = |extra: &[&str]| -> Vec<String> {
        [
            "--configuration-root",
            MAIN,
            "--extension",
            "EXT=a/b/ext",
            "--extension",
            "DEP=a/b/dep",
            "--external",
            &external,
        ]
        .into_iter()
        .chain(extra.iter().copied())
        .map(str::to_owned)
        .collect()
    };
    fn as_str(flags: &[String]) -> Vec<&str> {
        flags.iter().map(String::as_str).collect()
    }

    let every = analyze(dir.path(), &as_str(&flags(&[])));
    assert_eq!(
        every.unresolved_modules_at(EPF_FORM_MODULE),
        vec![MISSING_MODULE.to_string()],
        "control: without dependsOn both extensions are visible"
    );

    let narrowed = analyze(dir.path(), &as_str(&flags(&["--external-depends-on", "АРМ=EXT"])));
    assert_eq!(
        narrowed.unresolved_modules_at(EPF_FORM_MODULE),
        vec![MISSING_MODULE.to_string(), DEP_MODULE.to_string()],
        "narrowed to EXT, the other extension's module is gone and the base stays"
    );

    // The base alone, declared in the file: `dependsOn = []` is not "no key".
    std::fs::write(
        dir.path().join("bsl-analyzer.toml"),
        format!(
            "[source]\nroot = \"{MAIN}\"\nextensions = [\n  {{ name = \"EXT\", path = \"{EXT}\" }},\n  {{ name = \"DEP\", path = \"{DEP}\" }},\n]\nexternals = [{{ name = \"{EPF_NAME}\", path = \"{EPF}\", dependsOn = [] }}]\n"
        ),
    )
    .unwrap();
    let base_only = analyze(dir.path(), &[]);
    assert_eq!(
        base_only.unresolved_modules_at(EPF_FORM_MODULE),
        vec![MISSING_MODULE.to_string(), DEP_MODULE.to_string(), EXT_MODULE.to_string()],
        "an empty list leaves the base visible and nothing else"
    );
}

#[test]
fn an_external_under_src_epf_is_discovered_unless_opted_out() {
    let dir = workspace();
    workspace_calling_main_configuration(dir.path());
    write_external(
        dir.path(),
        "src/epf/АРМ",
        EPF_NAME,
        &format!(
            "&НаСервере\nПроцедура ПриСозданииНаСервере(Отказ, СтандартнаяОбработка)\n\t{MAIN_MODULE}.Экспортируемая();\n\t{MISSING_CALL}\nКонецПроцедуры\n"
        ),
    );
    let discovered = analyze(dir.path(), &["--configuration-root", MAIN]);
    assert_eq!(
        discovered.unresolved_modules_at(EPF_FORM_MODULE),
        vec![MISSING_MODULE.to_string()],
        "found without a declaration, and bound to the owner"
    );
    assert_eq!(external_notices(&discovered), 0);

    let opted_out = analyze(dir.path(), &["--configuration-root", MAIN, "--no-externals"]);
    assert!(
        opted_out.file_event_at(EPF_FORM_MODULE).is_none(),
        "control: opted out, the export is not a root and its module is not analyzed"
    );
}

#[test]
fn a_root_declared_under_the_wrong_key_is_refused_by_name() {
    let dir = workspace();
    workspace_calling_main_configuration(dir.path());
    write_external(dir.path(), EPF, EPF_NAME, "Процедура П()\nКонецПроцедуры\n");

    let as_extension = analyze_refuses(
        dir.path(),
        &["--configuration-root", MAIN, "--extension", &format!("{EPF_NAME}={EPF}")],
    );
    // The CLI renders a project error in its Debug form, so the variant name is
    // what reaches the operator; matching on it keeps the check honest either way.
    assert!(
        as_extension.contains("StructuredNotAnExtension"),
        "an export named as an extension: {as_extension}"
    );

    let as_external = analyze_refuses(
        dir.path(),
        &["--configuration-root", MAIN, "--external", &format!("EXT={EXT}")],
    );
    assert!(
        as_external.contains("ExternalIsAConfiguration"),
        "an extension named as an external: {as_external}"
    );

    let inside = analyze_refuses(
        dir.path(),
        &["--configuration-root", MAIN, "--external", &format!("CM={MAIN}/CommonModules")],
    );
    // `CommonModules/` holds exactly one object XML in this fixture, so it is
    // refused by what that XML describes, naming the element it found.
    assert!(
        inside.contains("ExternalNotAnExternalObject") && inside.contains("CommonModule"),
        "a directory that is not one export: {inside}"
    );
}

#[test]
fn the_mcp_status_reports_an_external_object_without_its_owner() {
    let dir = workspace();
    workspace_calling_main_configuration(dir.path());
    write_external(
        dir.path(),
        EPF,
        EPF_NAME,
        &format!(
            "&НаСервере\nПроцедура ПриСозданииНаСервере(Отказ, СтандартнаяОбработка)\n\t{MAIN_MODULE}.Экспортируемая();\nКонецПроцедуры\n"
        ),
    );
    let module = path(dir.path(), EPF).join(EPF_FORM_MODULE);
    let external = format!("{EPF_NAME}={EPF}");

    let (unresolved, standalone) = mcp_probe(dir.path(), &module, &["--external", &external]);
    assert_eq!(unresolved, vec![MAIN_MODULE.to_string()], "control: alone, the owner is missing");
    assert!(
        standalone["standalone_extension"]
            .as_str()
            .is_some_and(|s| s.contains("analyzed without an owning configuration")),
        "status must carry the notice: {standalone}"
    );

    let (unresolved, bound) =
        mcp_probe(dir.path(), &module, &["--configuration-root", MAIN, "--external", &external]);
    assert!(unresolved.is_empty(), "bound, the owner's module resolves: {unresolved:?}");
    assert!(
        bound.get("standalone_extension").is_none(),
        "a bound owning configuration must leave the field out: {bound}"
    );
}

/// The `workspace` sweep aggregates the same findings the `file` action flags, so
/// it must carry the same advisory when the project has no owner for its external
/// objects: a consumer reading only that envelope must not take the unresolved
/// calls for real.
#[test]
fn the_diagnostics_workspace_action_reports_a_missing_owner_like_the_file_action() {
    let dir = workspace();
    workspace_calling_main_configuration(dir.path());
    write_external(
        dir.path(),
        EPF,
        EPF_NAME,
        &format!(
            "&НаСервере\nПроцедура ПриСозданииНаСервере(Отказ, СтандартнаяОбработка)\n\t{MAIN_MODULE}.Экспортируемая();\nКонецПроцедуры\n"
        ),
    );
    let module = path(dir.path(), EPF).join(EPF_FORM_MODULE);
    let external = format!("{EPF_NAME}={EPF}");
    const REASON: &str = "owning_configuration_missing";

    let names_reason = |flags: &[&str], arguments: Value| -> bool {
        let mut session = McpSession::start(dir.path(), flags);
        session.wait_ready("diagnostics");
        session.diagnostics(arguments).to_string().contains(REASON)
    };
    let file = serde_json::json!({"action": "file", "path": module.display().to_string()});
    let sweep = serde_json::json!({"action": "workspace"});

    let alone: &[&str] = &["--external", &external];
    assert!(names_reason(alone, file), "control: the file action names the missing owner");
    assert!(names_reason(alone, sweep.clone()), "the workspace sweep names it too");

    let bound: &[&str] = &["--configuration-root", MAIN, "--external", &external];
    assert!(!names_reason(bound, sweep), "bound, the sweep carries no such reason");
}

// Prefixed by the root directory: the base carries a namesake at
// `DataProcessors/АРМ/Ext/ObjectModule.bsl`, and a bare tail would find it first.
const EPF_OBJECT_MODULE: &str = "epf/АРМ/Ext/ObjectModule.bsl";

/// A method that exists on no object: the negative control for `UnresolvedMethodCall`.
const BOGUS_CALL: &str = "\tЭтотОбъект.ЗаведомоНетТакогоМетода();\n";

fn object_module_body(attribute: &str) -> String {
    format!(
        "Процедура ОбработкаПроведения() Экспорт\n\tЗначение = ЭтотОбъект.{attribute};\n\tОпечатка = ЭтотОбъект.{attribute}ЛишняяБуква;\nКонецПроцедуры\n"
    )
}

/// Which attributes `UnresolvedField` complains about, sorted.
fn unresolved_fields(run: &Run, tail: &str) -> Vec<String> {
    let mut names: Vec<String> = run
        .messages_at(tail, "UnresolvedField")
        .iter()
        .filter_map(|m| m.split('\'').nth(1).map(str::to_owned))
        .collect();
    names.sort();
    names
}

#[test]
fn an_external_object_module_knows_its_own_attributes_and_not_a_namesakes() {
    let dir = workspace();
    workspace_calling_main_configuration(dir.path());
    // The base carries an INTERNAL processor of the same name with a different
    // attribute — the ERP shape, where the export is a copy of a built-in one.
    let internal = path(dir.path(), MAIN).join("DataProcessors");
    std::fs::create_dir_all(internal.join("АРМ/Ext")).unwrap();
    std::fs::write(
        internal.join("АРМ.xml"),
        processor_xml("DataProcessor", "АРМ", Some("Внутренний")),
    )
    .unwrap();
    std::fs::write(
        internal.join("АРМ/Ext/ObjectModule.bsl"),
        format!(
            "{}{BOGUS_CALL}КонецПроцедуры\n",
            object_module_body("Внутренний").trim_end_matches("КонецПроцедуры\n")
        ),
    )
    .unwrap();

    write_external_with_attribute(
        dir.path(),
        EPF,
        "АРМ",
        "Процедура П()\nКонецПроцедуры\n",
        Some("Внешний"),
    );
    let epf_object = path(dir.path(), EPF).join("АРМ/Ext");
    std::fs::create_dir_all(&epf_object).unwrap();
    std::fs::write(
        epf_object.join("ObjectModule.bsl"),
        format!(
            "{}\tЧужой = ЭтотОбъект.Внутренний;\n{BOGUS_CALL}КонецПроцедуры\n",
            object_module_body("Внешний").trim_end_matches("КонецПроцедуры\n")
        ),
    )
    .unwrap();

    let run = analyze(dir.path(), &["--configuration-root", MAIN, "--external", "АРМ=a/b/epf"]);

    // The internal module is the equivalence control: same shape, same verdicts.
    assert_eq!(
        unresolved_fields(&run, "DataProcessors/АРМ/Ext/ObjectModule.bsl"),
        vec!["ВнутреннийЛишняяБуква".to_string()],
        "control: the internal processor resolves its attribute and flags the typo"
    );
    assert_eq!(
        unresolved_fields(&run, EPF_OBJECT_MODULE),
        vec!["ВнешнийЛишняяБуква".to_string(), "Внутренний".to_string()],
        "the external object owns Внешний, flags its typo, and does not borrow the \
         namesake's Внутренний"
    );

    // A call that exists nowhere must be flagged on both: the external kind has
    // no manager collection to name the receiver by, and that must not turn
    // into silence.
    let bogus_calls = |tail: &str| run.messages_at(tail, "UnresolvedMethodCall");
    assert!(
        bogus_calls("DataProcessors/АРМ/Ext/ObjectModule.bsl")
            .iter()
            .any(|m| m.contains("ЗаведомоНетТакогоМетода")),
        "control: the internal processor flags the bogus call"
    );
    let external_calls = bogus_calls(EPF_OBJECT_MODULE);
    assert!(
        external_calls
            .iter()
            .any(|m| m.contains("ЗаведомоНетТакогоМетода") && m.contains("ВнешняяОбработка.АРМ")),
        "the external object flags the bogus call and names itself: {external_calls:?}"
    );
}
