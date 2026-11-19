use std::{
    io::{Read, Write},
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    path::Path,
    process::{Command, Stdio},
    time::Duration,
};

use rmcp::{model::PingRequest, ServiceExt};
use windows_sys::Win32::{
    Foundation::{HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT},
    System::{
        JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JobObjectBasicAccountingInformation,
            JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
            JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        },
        Threading::{
            GetExitCodeProcess, OpenProcess, TerminateProcess, WaitForSingleObject,
            PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
        },
    },
};

use super::{connect_existing, spawn_detached, BackendKey, BrokerCommand};
use crate::{broker, McpProfile, McpServer, SharedState};

mod launcher_lifetime {
    include!(concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-launcher/src/parent_death.rs"));
}

const WORKER: &str = "broker::proxy::windows_lifetime_tests::worker";
const ROLE: &str = "BSL_TEST_WINDOWS_LIFETIME_ROLE";
const ROOT: &str = "BSL_TEST_WINDOWS_LIFETIME_ROOT";

struct OwnedProcess(OwnedHandle);

impl OwnedProcess {
    fn open(pid: u32) -> Self {
        let handle = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE | PROCESS_TERMINATE,
                0,
                pid,
            )
        };
        assert!(
            !handle.is_null(),
            "cannot observe own test process {pid}: {}",
            std::io::Error::last_os_error()
        );
        Self(unsafe { OwnedHandle::from_raw_handle(handle) })
    }

    fn handle(&self) -> HANDLE {
        self.0.as_raw_handle() as HANDLE
    }

    fn is_running(&self) -> bool {
        unsafe { WaitForSingleObject(self.handle(), 0) == WAIT_TIMEOUT }
    }

    fn wait(&self) -> u32 {
        // Async callers keep a reactor thread free to finish pending pipe closes during this wait.
        let timeout = test_utils::wait::hang_guard(Duration::from_secs(30));
        assert_eq!(
            unsafe { WaitForSingleObject(self.handle(), timeout.as_millis() as u32) },
            WAIT_OBJECT_0,
            "own test process must exit"
        );
        let mut code = u32::MAX;
        assert_ne!(unsafe { GetExitCodeProcess(self.handle(), &mut code) }, 0);
        code
    }
}

impl Drop for OwnedProcess {
    fn drop(&mut self) {
        if self.is_running() {
            unsafe {
                TerminateProcess(self.handle(), 250);
                WaitForSingleObject(self.handle(), 30_000);
            }
        }
    }
}

fn key(root: &Path) -> BackendKey {
    BackendKey::new(
        root,
        crate::WorkspaceCacheLayout::for_workspace(root).root(),
        McpProfile::Workspace,
        0,
        0,
        std::collections::BTreeSet::new(),
    )
}

fn worker_command(role: &str, root: &Path) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", WORKER, "--ignored", "--nocapture"])
        .env(ROLE, role)
        .env(ROOT, root)
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    launcher_lifetime::configure_parent_death(&mut command);
    for key in worker_env_removals() {
        command.env_remove(key);
    }
    command
}

fn broker_command(role: &str, root: &Path) -> BrokerCommand {
    let mut command = BrokerCommand::new(std::env::current_exe().unwrap());
    command.args(["--exact", WORKER, "--ignored", "--nocapture"]).env(ROLE, role).env(ROOT, root);
    for key in worker_env_removals() {
        command.env_remove(key);
    }
    command
}

fn worker_env_removals() -> impl Iterator<Item = std::ffi::OsString> {
    std::env::vars_os().map(|(key, _)| key).filter(|key| {
        let name = key.to_string_lossy().to_ascii_uppercase();
        name.starts_with("PG")
            || name == "DATABASE_URL"
            || name.starts_with("BSL_SEARCH_")
            || name.starts_with("EMBEDDING_")
            || name.starts_with("BSL_MCP_EFFECTIVE_EMBEDDING_")
            || name == "BSL_TEST_EMBEDDING"
    })
}

fn marker(root: &Path, name: &str) -> String {
    let path = root.join(name);
    assert!(
        test_utils::wait::poll_until(Duration::from_secs(30), Duration::from_millis(20), || path
            .exists()),
        "missing worker marker {}",
        path.display()
    );
    std::fs::read_to_string(path).unwrap()
}

fn write_marker(root: &Path, name: &str, value: String) {
    let path = root.join(name);
    let pending = path.with_extension("pending");
    std::fs::write(&pending, value).unwrap();
    std::fs::rename(pending, path).unwrap();
}

async fn handshake(root: &Path, pid: u32) {
    let key = key(root);
    let deadline =
        tokio::time::Instant::now() + test_utils::wait::hang_guard(Duration::from_secs(30));
    let stream = loop {
        match connect_existing(&key, pid).await {
            Ok(stream) => break stream,
            Err(error) if tokio::time::Instant::now() >= deadline => {
                panic!("broker {pid} did not become ready: {error}")
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
        }
    };
    let client = ().serve(stream).await.expect("real MCP handshake");
    assert!(client.peer_info().is_some());
    client
        .peer()
        .send_request(rmcp::model::ClientRequest::PingRequest(PingRequest::default()))
        .await
        .expect("same broker answers ping");
    client.cancel().await.expect("client session closes");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn broker_survives_launcher_job_and_serves_second_client_until_idle_ttl() {
    let root = tempfile::tempdir().unwrap();
    let mut proxy = worker_command("proxy", root.path()).spawn().unwrap();
    let proxy_process = OwnedProcess::open(proxy.id());
    let launcher_job = launcher_lifetime::adopt_child(&proxy).unwrap();
    proxy.stdin.as_mut().unwrap().write_all(b"1").unwrap();
    let broker_pid = marker(root.path(), "broker.pid").parse().unwrap();
    let broker_process = OwnedProcess::open(broker_pid);
    let ordinary_pid = marker(root.path(), "ordinary.pid").parse().unwrap();
    let ordinary_process = OwnedProcess::open(ordinary_pid);

    handshake(root.path(), broker_pid).await;
    drop(launcher_job);
    proxy_process.wait();
    ordinary_process.wait();
    proxy.wait().unwrap();
    assert!(broker_process.is_running(), "closing the launcher job must preserve the broker");

    handshake(root.path(), broker_pid).await;
    assert!(broker_process.is_running());
    assert_eq!(broker_process.wait(), 0, "broker exits normally on its idle TTL");
    assert_eq!(marker(root.path(), "broker-started").parse::<u32>().unwrap(), broker_pid);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn directly_spawned_broker_serves_and_idles_out_without_a_launcher_job() {
    let root = tempfile::tempdir().unwrap();
    let mut command = broker_command("broker", root.path());
    let mut child = spawn_detached(&key(root.path()), &mut command).unwrap();
    let process = OwnedProcess::open(child.id());
    handshake(root.path(), child.id()).await;
    assert_eq!(process.wait(), 0);
    assert!(child.wait().unwrap().success());
}

#[test]
fn detached_broker_preserves_unicode_environment_and_working_directory() {
    let root = tempfile::tempdir().unwrap();
    let cwd = root.path().join("Рабочий каталог");
    std::fs::create_dir(&cwd).unwrap();
    let mut command = broker_command("launch-contract", root.path());
    command
        .current_dir(&cwd)
        .env("BSL_TEST_BROKER_VALUE", "другое")
        .env("bsl_test_broker_value", "строка = 𐐷")
        .env("BSL_TEST_BROKER_REMOVED", "must be removed")
        .env_remove("bsl_test_broker_removed");
    let mut child = spawn_detached(&key(root.path()), &mut command).unwrap();
    assert!(child.wait().unwrap().success());
    assert_eq!(marker(root.path(), "launch-contract"), "ok");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cold_proxy_stdout_reaches_eof_while_another_session_keeps_broker_alive() {
    let root = tempfile::tempdir().unwrap();
    let mut proxy =
        worker_command("stdout-proxy", root.path()).stdout(Stdio::piped()).spawn().unwrap();
    let proxy_process = OwnedProcess::open(proxy.id());
    let mut stdout = proxy.stdout.take().unwrap();
    let (sender, receiver) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        sender.send(stdout.read_to_end(&mut bytes)).unwrap();
    });
    let broker_pid = marker(root.path(), "broker.pid").parse().unwrap();
    let broker_process = OwnedProcess::open(broker_pid);
    handshake(root.path(), broker_pid).await;
    let stream = connect_existing(&key(root.path()), broker_pid).await.unwrap();
    let session = ().serve(stream).await.unwrap();
    assert!(session.peer_info().is_some());

    proxy.stdin.take().unwrap().write_all(b"1").unwrap();
    assert_eq!(proxy_process.wait(), 0);
    assert!(proxy.wait().unwrap().success());
    let eof_before_broker_exit =
        receiver.recv_timeout(test_utils::wait::hang_guard(Duration::from_secs(2)));
    let broker_still_running = broker_process.is_running();

    session.cancel().await.unwrap();
    assert_eq!(broker_process.wait(), 0);
    if eof_before_broker_exit.is_err() {
        receiver
            .recv_timeout(test_utils::wait::hang_guard(Duration::from_secs(30)))
            .expect("stdout drains after the owned broker exits")
            .unwrap();
    }
    reader.join().unwrap();
    assert!(broker_still_running, "the second session keeps the broker alive");
    eof_before_broker_exit
        .expect(
            "cold proxy stdout must reach EOF while the broker is still serving another session",
        )
        .unwrap();
}

#[test]
fn foreign_job_that_forbids_breakaway_is_not_modified_or_silently_inherited() {
    for (role, expected_processes) in [("blocked-proxy", 2), ("nested-proxy", 3)] {
        let root = tempfile::tempdir().unwrap();
        let mut proxy = worker_command(role, root.path()).spawn().unwrap();
        let process = OwnedProcess::open(proxy.id());
        let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        assert!(!job.is_null());
        let job = unsafe { OwnedHandle::from_raw_handle(job) };
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        assert_ne!(
            unsafe {
                SetInformationJobObject(
                    job.as_raw_handle() as HANDLE,
                    JobObjectExtendedLimitInformation,
                    &limits as *const _ as *const _,
                    std::mem::size_of_val(&limits) as u32,
                )
            },
            0
        );
        assert_ne!(
            unsafe {
                AssignProcessToJobObject(
                    job.as_raw_handle() as HANDLE,
                    proxy.as_raw_handle() as HANDLE,
                )
            },
            0
        );
        let launcher_job =
            (role == "nested-proxy").then(|| launcher_lifetime::adopt_child(&proxy).unwrap());
        proxy.stdin.as_mut().unwrap().write_all(b"1").unwrap();
        assert!(marker(root.path(), "spawn-error").contains("breakaway"));
        process.wait();
        assert!(proxy.wait().unwrap().success());
        let mut after: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        assert_ne!(
            unsafe {
                QueryInformationJobObject(
                    job.as_raw_handle() as HANDLE,
                    JobObjectExtendedLimitInformation,
                    &mut after as *mut _ as *mut _,
                    std::mem::size_of_val(&after) as u32,
                    std::ptr::null_mut(),
                )
            },
            0
        );
        assert_eq!(after.BasicLimitInformation.LimitFlags, limits.BasicLimitInformation.LimitFlags);
        let mut accounting: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { std::mem::zeroed() };
        assert_ne!(
            unsafe {
                QueryInformationJobObject(
                    job.as_raw_handle() as HANDLE,
                    JobObjectBasicAccountingInformation,
                    &mut accounting as *mut _ as *mut _,
                    std::mem::size_of_val(&accounting) as u32,
                    std::ptr::null_mut(),
                )
            },
            0
        );
        assert_eq!(
            accounting.TotalProcesses, expected_processes,
            "exactly one broker spawn is attempted"
        );
        assert_eq!(
            accounting.ActiveProcesses, 1,
            "only the ordinary child remains in the foreign job"
        );
        if role == "blocked-proxy" {
            assert!(!root.path().join("broker-started").exists());
        }
        drop(launcher_job);
    }
}

#[test]
#[ignore = "self-exec fixture invoked only by the Windows process lifecycle tests"]
#[allow(
    clippy::zombie_processes,
    reason = "The parent test owns worker process handles and terminates ordinary children through its Job Object"
)]
fn worker() {
    let role = std::env::var(ROLE).expect("worker role");
    let root = std::path::PathBuf::from(std::env::var_os(ROOT).expect("worker root"));
    match role.as_str() {
        "ordinary" => loop {
            std::thread::park();
        },
        "stdout-proxy" => {
            let mut command = broker_command("broker", &root);
            let broker = spawn_detached(&key(&root), &mut command).unwrap();
            write_marker(&root, "broker.pid", broker.id().to_string());
            let mut gate = [0];
            std::io::stdin().read_exact(&mut gate).unwrap();
        }
        "proxy" | "blocked-proxy" | "nested-proxy" => {
            let mut gate = [0];
            std::io::stdin().read_exact(&mut gate).unwrap();
            let ordinary = worker_command("ordinary", &root).spawn().unwrap();
            write_marker(&root, "ordinary.pid", ordinary.id().to_string());
            let mut command = broker_command("broker", &root);
            match spawn_detached(&key(&root), &mut command) {
                Ok(broker) => {
                    assert_eq!(role, "proxy", "foreign restrictive job must refuse detachment");
                    write_marker(&root, "broker.pid", broker.id().to_string());
                    std::io::stdin().read_exact(&mut gate).unwrap();
                }
                Err(error) => {
                    write_marker(&root, "spawn-error", format!("{error:#}"));
                    assert!(
                        matches!(role.as_str(), "blocked-proxy" | "nested-proxy"),
                        "launcher-owned job must allow broker detachment"
                    );
                }
            }
        }
        "launch-contract" => {
            assert_eq!(
                std::env::current_dir().unwrap().canonicalize().unwrap(),
                root.join("Рабочий каталог").canonicalize().unwrap(),
            );
            assert_eq!(std::env::var("BSL_TEST_BROKER_VALUE").unwrap(), "строка = 𐐷");
            assert!(std::env::var_os("BSL_TEST_BROKER_REMOVED").is_none());
            assert!(std::env::var_os("SystemRoot").is_some());
            write_marker(&root, "launch-contract", "ok".to_owned());
        }
        "broker" => {
            use tracing_subscriber::prelude::*;

            let log = std::fs::File::create(root.join("broker-lifecycle.log")).unwrap();
            let subscriber = tracing_subscriber::fmt()
                .with_max_level(tracing::Level::DEBUG)
                .with_writer(std::sync::Mutex::new(log))
                .with_ansi(false)
                .with_target(false)
                .finish()
                .with(tracing_subscriber::filter::filter_fn(|metadata| {
                    metadata.target() == "broker_lifetime_probe"
                }));
            tracing::subscriber::set_global_default(subscriber)
                .expect("the dedicated worker owns its diagnostic subscriber");
            write_marker(&root, "broker-started", std::process::id().to_string());
            let runtime = tokio::runtime::Runtime::new().unwrap();
            let result = runtime.block_on(broker::daemon::run(
                || Ok(McpServer::new(McpProfile::Reference, SharedState::reference(None))),
                key(&root),
                Duration::from_secs(30),
                Duration::from_secs(5),
            ));
            write_marker(&root, "block_on-returned", std::process::id().to_string());
            tracing::debug!(target: "broker_lifetime_probe", phase = "block_on-returned");
            drop(runtime);
            write_marker(&root, "runtime-dropped", std::process::id().to_string());
            tracing::debug!(target: "broker_lifetime_probe", phase = "runtime-dropped");
            result.unwrap();
        }
        _ => panic!("unknown test worker role"),
    }
}
