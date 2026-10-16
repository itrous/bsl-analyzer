use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use crate::{
    tools::check_executor::{check, set_test_ui_limit_target_job, test_job_assign_error},
    CheckExecutorProfile,
};
use check_contract::{CheckResponse, InputKind, Status};
use tokio_util::sync::CancellationToken;

const FAKE_EXECUTOR: &str = r#"
import ctypes, json, os, subprocess, sys, time
mode, marker = sys.argv[1:3]
kernel = ctypes.WinDLL('kernel32', use_last_error=True)
kernel.IsProcessInJob.argtypes = [ctypes.c_void_p, ctypes.c_void_p, ctypes.POINTER(ctypes.c_int)]
kernel.IsProcessInJob.restype = ctypes.c_int
kernel.GetCurrentProcess.restype = ctypes.c_void_p
kernel.WaitForSingleObject.argtypes = [ctypes.c_void_p, ctypes.c_uint]
kernel.WaitForSingleObject.restype = ctypes.c_uint
kernel.CloseHandle.argtypes = [ctypes.c_void_p]
kernel.CloseHandle.restype = ctypes.c_int
in_job = ctypes.c_int()
assert kernel.IsProcessInJob(kernel.GetCurrentProcess(), None, ctypes.byref(in_job))

def response(status='valid'):
    value = {
        'schema_version': '1', 'request_id': request['request_id'],
        'connection': request['connection'], 'backend': 'fake/1',
        'status': status, 'valid': status == 'valid', 'issues': [],
        'contexts_required': [], 'contexts_checked': ['server'], 'truncated': False,
        'failure': None, 'compilation_status': 'valid', 'compiler': None,
        'cleanup_status': 'complete', 'module_type': None, 'context': None,
        'backend_info': None,
    }
    return json.dumps(value)

if mode == 'no-spawn-marker':
    open(marker, 'w').write('spawned')
    sys.exit(0)

request = json.loads(sys.stdin.buffer.read())
if mode == 'event-exit':
    with open(marker, 'w') as out:
        out.write(f'{os.getpid()} {in_job.value}')
    event_name = os.environ['BSL_CHECK_STOP_EVENT']
    kernel.OpenEventW.argtypes = [ctypes.c_uint, ctypes.c_int, ctypes.c_wchar_p]
    kernel.OpenEventW.restype = ctypes.c_void_p
    event = kernel.OpenEventW(0x00100000, False, event_name)
    assert event
    assert kernel.WaitForSingleObject(event, 30_000) == 0
    assert kernel.CloseHandle(event)
    time.sleep(0.05)
    print(response(), flush=True)
    sys.exit(0)

if mode == 'job-hang':
    child_code = (
        "import ctypes,os,sys,time; "
        "k=ctypes.WinDLL('kernel32'); k.GetCurrentProcess.restype=ctypes.c_void_p; "
        "k.IsProcessInJob.argtypes=[ctypes.c_void_p,ctypes.c_void_p,ctypes.POINTER(ctypes.c_int)]; "
        "k.IsProcessInJob.restype=ctypes.c_int; p=ctypes.c_int(); "
        "assert k.IsProcessInJob(k.GetCurrentProcess(),None,ctypes.byref(p)); "
        "open(sys.argv[1],'w').write(str(os.getpid())+' '+str(p.value)); time.sleep(30)"
    )
    child = subprocess.Popen([sys.executable, '-c', child_code, marker + '.child'])
    with open(marker, 'w') as out:
        out.write(f'{os.getpid()} {in_job.value} {child.pid}')
    event_name = os.environ['BSL_CHECK_STOP_EVENT']
    kernel.OpenEventW.argtypes = [ctypes.c_uint, ctypes.c_int, ctypes.c_wchar_p]
    kernel.OpenEventW.restype = ctypes.c_void_p
    event = kernel.OpenEventW(0x00100000, False, event_name)
    assert event
    assert kernel.WaitForSingleObject(event, 30_000) == 0
    assert kernel.CloseHandle(event)
    time.sleep(30)

if mode == 'exit1-valid':
    print(response(), flush=True)
    sys.exit(1)
if mode == 'malformed':
    print('{', flush=True)
    sys.exit(0)
if mode == 'overflow':
    sys.stdout.buffer.write(b'x' * 4096)
    sys.stdout.flush()
    time.sleep(30)
if mode == 'valid':
    print(response(), flush=True)
"#;

fn python() -> OsString {
    std::env::var_os("BSL_TEST_PYTHON").unwrap_or_else(|| "python".into())
}

fn profile(script: &Path, mode: &str, marker: &Path) -> CheckExecutorProfile {
    CheckExecutorProfile {
        program: python().into(),
        args: vec![script.as_os_str().to_owned(), mode.into(), marker.as_os_str().to_owned()],
        timeout: Duration::from_secs(3),
        max_code_bytes: 1024,
        max_response_bytes: 16 * 1024,
        max_stderr_bytes: 1024,
        stop_grace: Duration::from_millis(250),
        reap_timeout: Duration::from_secs(2),
        max_concurrent: 1,
        handles_snippet: true,
        semaphore: Arc::new(tokio::sync::Semaphore::new(1)),
    }
}

fn fake(temp: &tempfile::TempDir) -> PathBuf {
    let path = temp.path().join("fake_executor.py");
    std::fs::write(&path, FAKE_EXECUTOR).unwrap();
    path
}

async fn wait_for_file(path: &Path) -> bool {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .is_ok()
}

fn open_process(pid: u32) -> windows_sys::Win32::Foundation::HANDLE {
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
    };

    let process =
        unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE, 0, pid) };
    assert!(!process.is_null(), "could not open process {pid}");
    process
}

fn wait_for_process_exit(process: windows_sys::Win32::Foundation::HANDLE) -> u32 {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, WAIT_OBJECT_0},
        System::Threading::{GetExitCodeProcess, WaitForSingleObject},
    };

    assert_eq!(unsafe { WaitForSingleObject(process, 3_000) }, WAIT_OBJECT_0);
    let mut code = u32::MAX;
    assert_ne!(unsafe { GetExitCodeProcess(process, &mut code) }, 0);
    unsafe { CloseHandle(process) };
    code
}

#[tokio::test]
async fn event_is_signaled_and_cooperative_child_finishes_within_grace() {
    let temp = tempfile::tempdir().unwrap();
    let marker = temp.path().join("event.txt");
    let script = fake(&temp);
    let mut profile = profile(&script, "event-exit", &marker);
    profile.stop_grace = Duration::from_millis(300);
    let token = CancellationToken::new();
    let task = tokio::spawn({
        let token = token.clone();
        async move {
            check(&profile, "windows-test", "x = 1;", InputKind::Snippet, None, None, token).await
        }
    });
    assert!(wait_for_file(&marker).await, "fake executor should create its marker");
    let values = std::fs::read_to_string(&marker).unwrap();
    let mut values = values.split_whitespace();
    let parent: u32 = values.next().unwrap().parse().unwrap();
    assert_eq!(values.next(), Some("1"), "executor started inside its Job");
    let process = open_process(parent);
    let started = std::time::Instant::now();
    token.cancel();
    assert!(task.await.unwrap().is_err(), "cancellation takes precedence over JSON");
    assert!(started.elapsed() >= Duration::from_millis(250), "caller observes full stop grace");
    assert_eq!(wait_for_process_exit(process), 0, "Event stop exited without Job termination");
}

#[tokio::test]
async fn timeout_hard_stops_job_and_reaps_its_descendant() {
    let temp = tempfile::tempdir().unwrap();
    let marker = temp.path().join("processes.txt");
    let script = fake(&temp);
    let mut profile = profile(&script, "job-hang", &marker);
    profile.timeout = Duration::from_secs(5);
    profile.stop_grace = Duration::from_millis(250);
    let task = tokio::spawn(async move {
        check(
            &profile,
            "windows-test",
            "x = 1;",
            InputKind::Snippet,
            None,
            None,
            CancellationToken::new(),
        )
        .await
    });
    assert!(wait_for_file(&marker).await, "executor should create its marker");
    assert!(
        wait_for_file(&marker.with_file_name("processes.txt.child")).await,
        "executor should create its child marker"
    );
    let pids = std::fs::read_to_string(&marker).unwrap();
    let mut pids = pids.split_whitespace();
    let parent: u32 = pids.next().unwrap().parse().unwrap();
    assert_eq!(pids.next(), Some("1"), "executor was resumed inside its Job");
    let child: u32 = pids.next().unwrap().parse().unwrap();
    let parent_process = open_process(parent);
    let child_process = open_process(child);
    let result = task.await.unwrap().unwrap();
    let value = result.structured_content.unwrap();
    let response: CheckResponse = serde_json::from_value(value).unwrap();
    assert_eq!(response.failure.unwrap().code, "transport_timeout");
    assert_eq!(
        std::fs::read_to_string(marker.with_file_name("processes.txt.child"))
            .unwrap()
            .split_whitespace()
            .nth(1),
        Some("1"),
        "descendant inherited the Job"
    );
    assert_eq!(wait_for_process_exit(parent_process), 1);
    assert_eq!(wait_for_process_exit(child_process), 1);
}

#[tokio::test]
async fn cancellation_hard_stops_the_live_job_and_its_descendant() {
    let temp = tempfile::tempdir().unwrap();
    let marker = temp.path().join("processes.txt");
    let script = fake(&temp);
    let profile = profile(&script, "job-hang", &marker);
    let token = CancellationToken::new();
    let task = tokio::spawn({
        let token = token.clone();
        async move {
            check(&profile, "windows-test", "x = 1;", InputKind::Snippet, None, None, token).await
        }
    });
    assert!(wait_for_file(&marker).await, "executor should create its marker");
    assert!(
        wait_for_file(&marker.with_file_name("processes.txt.child")).await,
        "executor should create its child marker"
    );
    let pids = std::fs::read_to_string(&marker).unwrap();
    let mut pids = pids.split_whitespace();
    let parent: u32 = pids.next().unwrap().parse().unwrap();
    assert_eq!(pids.next(), Some("1"), "executor started inside its Job");
    let child: u32 = pids.next().unwrap().parse().unwrap();
    let parent_process = open_process(parent);
    let child_process = open_process(child);
    let started = std::time::Instant::now();
    token.cancel();
    assert!(task.await.unwrap().is_err());
    assert!(started.elapsed() >= Duration::from_millis(250));
    assert_eq!(wait_for_process_exit(parent_process), 1);
    assert_eq!(wait_for_process_exit(child_process), 1);
}

#[tokio::test]
async fn queued_cancellation_does_not_spawn_the_executor() {
    let temp = tempfile::tempdir().unwrap();
    let marker = temp.path().join("spawned");
    let script = fake(&temp);
    let mut profile = profile(&script, "no-spawn-marker", &marker);
    profile.semaphore = Arc::new(tokio::sync::Semaphore::new(0));
    let token = CancellationToken::new();
    let task = tokio::spawn({
        let token = token.clone();
        async move {
            check(&profile, "windows-test", "x = 1;", InputKind::Snippet, None, None, token).await
        }
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    token.cancel();
    assert!(task.await.unwrap().is_err());
    assert!(!marker.exists(), "queued cancellation must happen before spawn/resume");
}

#[tokio::test]
async fn valid_bad_wire_nonzero_exit_and_output_limit_keep_caller_priority() {
    let temp = tempfile::tempdir().unwrap();
    let script = fake(&temp);
    let valid = check(
        &profile(&script, "valid", &temp.path().join("valid")),
        "windows-test",
        "x = 1;",
        InputKind::Snippet,
        None,
        None,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(
        serde_json::from_value::<CheckResponse>(valid.structured_content.unwrap()).unwrap().status,
        Status::Valid
    );

    for (mode, limit, expected) in [
        ("malformed", 16 * 1024, "transport_malformed_response"),
        ("exit1-valid", 16 * 1024, "transport_exit_status"),
        ("overflow", 64, "transport_output_limit"),
    ] {
        let mut profile = profile(&script, mode, &temp.path().join(mode));
        profile.max_response_bytes = limit;
        let result = check(
            &profile,
            "windows-test",
            "x = 1;",
            InputKind::Snippet,
            None,
            None,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let response: CheckResponse =
            serde_json::from_value(result.structured_content.unwrap()).unwrap();
        assert_eq!(response.failure.unwrap().code, expected, "mode={mode}");
    }
}

#[tokio::test]
async fn nested_ui_job_rejects_caller_job_admission_before_executor_resume() {
    const CHILD_ROLE: &str = "BSL_TEST_UI_JOB_ADMISSION_CHILD";
    const ROOT_ENV: &str = "BSL_TEST_UI_JOB_ADMISSION_ROOT";
    const TEST_ID: &str =
        "check_executor_windows_tests::nested_ui_job_rejects_caller_job_admission_before_executor_resume";

    if let Some(root) = std::env::var_os(ROOT_ENV)
        .filter(|_| std::env::var_os(CHILD_ROLE).as_deref() == Some(std::ffi::OsStr::new("1")))
    {
        let root = PathBuf::from(root);
        let ready = root.join("child-ready");
        let release = root.join("child-release");
        let marker = root.join("fake-executor-started");
        let result_path = root.join("child-result");
        std::fs::write(&ready, "waiting").unwrap();
        assert!(wait_for_file(&release).await, "parent did not release child test gate");

        let script = root.join("fake_executor.py");
        let profile = profile(&script, "no-spawn-marker", &marker);
        // The test child already belongs to a UI-limited parent Job. Applying
        // the same UI limit to the fresh target Job makes Windows reject the
        // real suspended-child assignment before the executor can resume.
        set_test_ui_limit_target_job(true);
        let result = check(
            &profile,
            "windows-test",
            "x = 1;",
            InputKind::Snippet,
            None,
            None,
            CancellationToken::new(),
        )
        .await;
        set_test_ui_limit_target_job(false);
        let assignment_error = test_job_assign_error();
        let result = result.expect("caller returns a structured spawn failure");
        assert_ne!(
            assignment_error, 0,
            "the target UI limit must cause an actual AssignProcessToJobObject rejection"
        );
        let response: CheckResponse =
            serde_json::from_value(result.structured_content.expect("structured response"))
                .unwrap();
        assert_eq!(response.failure.unwrap().code, "spawn_failed");
        assert!(!marker.exists(), "fake executor must not run before admission");

        // Admission failure transfers the permit to the background reaper; it
        // becomes available only after the suspended process has been waited.
        let permit =
            tokio::time::timeout(Duration::from_secs(3), profile.semaphore.clone().acquire_owned())
                .await
                .expect("reaper returns the caller permit after process exit")
                .expect("test semaphore remains open");
        drop(permit);
        std::fs::write(
            &result_path,
            format!("spawn_failed;target_ui_assign_error={assignment_error};marker_absent;permit_reaped"),
        )
        .unwrap();
        return;
    }

    use std::{
        os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
        process::{Command, Stdio},
    };
    use windows_sys::Win32::{
        Foundation::{GetLastError, HANDLE},
        System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, IsProcessInJob,
            JobObjectBasicAccountingInformation, JobObjectBasicUIRestrictions,
            JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
            JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_BASIC_UI_RESTRICTIONS,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOB_OBJECT_UILIMIT_DESKTOP,
        },
    };

    async fn wait_for_child(
        child: &mut std::process::Child,
    ) -> std::io::Result<std::process::ExitStatus> {
        tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                if let Some(status) = child.try_wait()? {
                    return Ok(status);
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "test child exit timeout"))?
    }

    let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    assert!(!job.is_null(), "CreateJobObjectW failed: {}", unsafe { GetLastError() });
    let job = unsafe { OwnedHandle::from_raw_handle(job as _) };
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
        0,
        "setting kill-on-close failed: {}",
        unsafe { GetLastError() }
    );
    let ui = JOBOBJECT_BASIC_UI_RESTRICTIONS { UIRestrictionsClass: JOB_OBJECT_UILIMIT_DESKTOP };
    assert_ne!(
        unsafe {
            SetInformationJobObject(
                job.as_raw_handle() as HANDLE,
                JobObjectBasicUIRestrictions,
                &ui as *const _ as *const _,
                std::mem::size_of_val(&ui) as u32,
            )
        },
        0,
        "setting UI restriction failed: {}",
        unsafe { GetLastError() }
    );

    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let marker = root.join("fake-executor-started");
    let _ = fake(&temp);
    let ready = root.join("child-ready");
    let release = root.join("child-release");
    let result_path = root.join("child-result");
    let executable = std::env::current_exe().expect("current Windows test executable");
    let mut child = Command::new(executable)
        .arg("--exact")
        .arg(TEST_ID)
        .arg("--nocapture")
        .env(CHILD_ROLE, "1")
        .env(ROOT_ENV, root)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn a copy of this test executable");

    if !wait_for_file(&ready).await {
        let _ = child.kill();
        let _ = child.wait();
        panic!("UNVERIFIED: child test did not reach its release gate");
    }
    let process = child.as_raw_handle() as HANDLE;
    if unsafe { AssignProcessToJobObject(job.as_raw_handle() as HANDLE, process) } == 0 {
        let error = unsafe { GetLastError() };
        let _ = child.kill();
        let _ = child.wait();
        panic!(
            "UNVERIFIED: could not assign test child to UI-restricted Job; parent-job nesting unavailable (win32={error})"
        );
    }
    let mut assigned = 0;
    if unsafe { IsProcessInJob(process, job.as_raw_handle() as HANDLE, &mut assigned) } == 0
        || assigned == 0
    {
        unsafe {
            windows_sys::Win32::System::JobObjects::TerminateJobObject(
                job.as_raw_handle() as HANDLE,
                1,
            )
        };
        let _ = child.wait();
        panic!("UNVERIFIED: Windows did not confirm child membership in the UI Job");
    }
    if let Err(error) = std::fs::write(&release, "go") {
        unsafe {
            windows_sys::Win32::System::JobObjects::TerminateJobObject(
                job.as_raw_handle() as HANDLE,
                1,
            )
        };
        let _ = child.wait();
        panic!("could not release nested-admission child: {error}");
    }
    if !wait_for_file(&result_path).await {
        unsafe {
            windows_sys::Win32::System::JobObjects::TerminateJobObject(
                job.as_raw_handle() as HANDLE,
                1,
            )
        };
        let _ = child.wait();
        panic!("nested-admission child produced no result before timeout");
    }
    let status = match wait_for_child(&mut child).await {
        Ok(status) => status,
        Err(error) => {
            unsafe {
                windows_sys::Win32::System::JobObjects::TerminateJobObject(
                    job.as_raw_handle() as HANDLE,
                    1,
                )
            };
            let _ = child.wait();
            panic!("nested-admission test child did not exit: {error}");
        }
    };
    assert!(status.success(), "child test failed with {status}");
    let mut accounting: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { std::mem::zeroed() };
    let mut returned = 0;
    assert_ne!(
        unsafe {
            QueryInformationJobObject(
                job.as_raw_handle() as HANDLE,
                JobObjectBasicAccountingInformation,
                &mut accounting as *mut _ as *mut _,
                std::mem::size_of_val(&accounting) as u32,
                &mut returned,
            )
        },
        0,
        "reading UI Job active process count failed: {}",
        unsafe { GetLastError() }
    );
    assert_eq!(accounting.ActiveProcesses, 0, "owned UI Job must be drained");
    let result = std::fs::read_to_string(result_path).unwrap();
    let mut fields = result.split(';');
    assert_eq!(fields.next(), Some("spawn_failed"));
    let assignment_error = fields
        .next()
        .and_then(|field| field.strip_prefix("target_ui_assign_error="))
        .and_then(|value| value.parse::<u32>().ok())
        .expect("child records the actual AssignProcessToJobObject error");
    assert_ne!(assignment_error, 0, "admission error must be nonzero");
    assert_eq!(fields.next(), Some("marker_absent"));
    assert_eq!(fields.next(), Some("permit_reaped"));
    assert_eq!(fields.next(), None, "child result has no unrecognized fields");
    assert!(!marker.exists(), "fake executor was launched despite rejected admission");
}
