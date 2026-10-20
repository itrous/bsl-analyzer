use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use crate::state::CheckExecutorProfile;
use check_contract::{
    CheckRequest, CheckResponse, CleanupStatus, CompilerContext, Failure, InputKind, ModuleContext,
    ModuleType, Status, WireLimits, SCHEMA_VERSION,
};
use rmcp::{model::CallToolResult, ErrorData as McpError};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::{Child, Command},
    sync::{watch, OwnedSemaphorePermit},
    task::JoinHandle,
    time::Instant,
};
use tokio_util::sync::CancellationToken;

static REQUEST_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[cfg(all(test, windows))]
thread_local! {
    static TEST_UI_LIMIT_TARGET_JOB: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static TEST_JOB_ASSIGN_ERROR: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

#[cfg(all(test, windows))]
pub(crate) fn set_test_ui_limit_target_job(enabled: bool) {
    TEST_UI_LIMIT_TARGET_JOB.with(|value| value.set(enabled));
    if enabled {
        TEST_JOB_ASSIGN_ERROR.with(|value| value.set(0));
    }
}

#[cfg(all(test, windows))]
pub(crate) fn test_job_assign_error() -> u32 {
    TEST_JOB_ASSIGN_ERROR.with(std::cell::Cell::get)
}

pub(crate) async fn check(
    profile: &CheckExecutorProfile,
    connection: &str,
    code: &str,
    input_kind: InputKind,
    module_type: Option<ModuleType>,
    context: Option<ModuleContext>,
    ct: CancellationToken,
) -> Result<CallToolResult, McpError> {
    if code.trim().is_empty() {
        return Err(McpError::invalid_params("Пустой код", None));
    }
    if code.len() as u64 > profile.max_code_bytes {
        return Err(McpError::invalid_params(
            "BSL source exceeds check_executor max_code_bytes",
            None,
        ));
    }
    if input_kind == InputKind::Snippet && has_module_syntax(code) {
        return Ok(caller_failure(
            &new_request(connection, code, profile, input_kind, module_type, context),
            "context_required",
            "This source contains module declarations or directives; retry with input_kind=module and module context.",
        ));
    }
    let mut request = new_request(connection, code, profile, input_kind, module_type, context);
    request.validate().map_err(|_| McpError::invalid_params("Invalid check request", None))?;
    let Some(deadline) = Instant::now().checked_add(profile.timeout) else {
        return Err(McpError::internal_error("check_executor timeout is out of range", None));
    };

    let permit = tokio::select! {
        biased;
        _ = ct.cancelled() => return Err(McpError::internal_error("Request cancelled", None)),
        permit = tokio::time::timeout_at(deadline, profile.semaphore.clone().acquire_owned()) => {
            match permit {
                Ok(Ok(permit)) => permit,
                Ok(Err(_)) => return Err(McpError::internal_error("check_executor queue closed", None)),
                Err(_) => return Ok(caller_failure(&request, "transport_timeout", "check_executor deadline expired in queue")),
            }
        }
    };
    if ct.is_cancelled() {
        return Err(McpError::internal_error("Request cancelled", None));
    }
    if Instant::now() >= deadline {
        return Ok(caller_failure(
            &request,
            "transport_timeout",
            "check_executor deadline expired in queue",
        ));
    }
    let Some(timeout_ms) = remaining_timeout_ms(deadline) else {
        return Ok(caller_failure(
            &request,
            "transport_timeout",
            "check_executor deadline expired in queue",
        ));
    };
    request.limits.timeout_ms = timeout_ms;
    execute(profile, request, permit, deadline, ct).await
}

fn remaining_timeout_ms(deadline: Instant) -> Option<u64> {
    let remaining_ns = deadline.checked_duration_since(Instant::now())?.as_nanos();
    if remaining_ns == 0 {
        return None;
    }
    let remaining_ms = remaining_ns.div_ceil(1_000_000).max(1);
    Some(u64::try_from(remaining_ms).unwrap_or(u64::MAX))
}

fn new_request(
    connection: &str,
    code: &str,
    profile: &CheckExecutorProfile,
    input_kind: InputKind,
    module_type: Option<ModuleType>,
    context: Option<ModuleContext>,
) -> CheckRequest {
    CheckRequest {
        schema_version: SCHEMA_VERSION.to_owned(),
        request_id: request_id(),
        connection: connection.to_owned(),
        code: code.to_owned(),
        limits: WireLimits {
            timeout_ms: u64::try_from(profile.timeout.as_millis()).unwrap_or(u64::MAX),
            max_code_bytes: profile.max_code_bytes,
            max_response_bytes: profile.max_response_bytes,
            max_stderr_bytes: profile.max_stderr_bytes,
            stop_grace_ms: u64::try_from(profile.stop_grace.as_millis()).unwrap_or(u64::MAX),
        },
        input_kind,
        module_type,
        context,
    }
}

async fn execute(
    profile: &CheckExecutorProfile,
    mut request: CheckRequest,
    permit: OwnedSemaphorePermit,
    deadline: Instant,
    ct: CancellationToken,
) -> Result<CallToolResult, McpError> {
    if ct.is_cancelled() {
        return Err(McpError::internal_error("Request cancelled", None));
    }
    let Some(timeout_ms) = remaining_timeout_ms(deadline) else {
        return Ok(caller_failure(
            &request,
            "transport_timeout",
            "check_executor deadline expired",
        ));
    };
    request.limits.timeout_ms = timeout_ms;
    let bytes = serde_json::to_vec(&request).expect("check request is serializable");
    if request.limits.frame_bytes().is_none_or(|maximum| bytes.len() as u64 > maximum) {
        return Err(McpError::invalid_params("Check request exceeds frame limit", None));
    }
    let mut permit = Some(permit);
    let (mut child, control) = match spawn_executor(profile, &mut permit) {
        Ok(child) => child,
        Err(error) => {
            tracing::warn!(kind = %error.kind(), "check_executor spawn failed");
            return Ok(caller_failure(
                &request,
                "spawn_failed",
                "check_executor could not be started",
            ));
        }
    };
    let permit = permit.take().expect("successful spawn keeps its connection permit");

    let stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let mut input = tokio::spawn(async move {
        let mut stdin = stdin;
        stdin.write_all(&bytes).await.map_err(|_| PipeError::Io)?;
        stdin.shutdown().await.map_err(|_| PipeError::Io)
    });
    let out_limit = usize::try_from(profile.max_response_bytes).unwrap_or(usize::MAX);
    let err_limit = usize::try_from(profile.max_stderr_bytes).unwrap_or(usize::MAX);
    let (overflow_tx, mut overflow_rx) = watch::channel(false);
    let mut output = tokio::spawn(drain_bounded(stdout, out_limit, overflow_tx.clone()));
    let mut errors = tokio::spawn(drain_bounded(stderr, err_limit, overflow_tx.clone()));
    let wait = async { child.wait().await.map_err(|_| PipeError::Io) };
    let outcome = {
        let completed = async {
            let (input, stdout, stderr, status) =
                tokio::join!(&mut input, &mut output, &mut errors, wait);
            Completed {
                input: join_result(input),
                stdout: join_result(stdout),
                stderr: join_result(stderr),
                status,
            }
        };
        tokio::pin!(completed);
        tokio::select! {
            biased;
            _ = ct.cancelled() => Outcome::Cancelled,
            _ = tokio::time::sleep_until(deadline) => Outcome::TimedOut,
            _ = overflow_rx.changed() => Outcome::OutputLimit,
            result = &mut completed => classify_completed(result),
        }
    };

    match outcome {
        Outcome::Cancelled => {
            input.abort();
            stop_child(
                child,
                control,
                profile.stop_grace,
                profile.reap_timeout,
                output,
                errors,
                permit,
            )
            .await;
            Err(McpError::internal_error("Request cancelled", None))
        }
        Outcome::TimedOut => {
            input.abort();
            stop_child(
                child,
                control,
                profile.stop_grace,
                profile.reap_timeout,
                output,
                errors,
                permit,
            )
            .await;
            Ok(caller_failure(&request, "transport_timeout", "check_executor deadline expired"))
        }
        Outcome::OutputLimit => {
            input.abort();
            stop_child(
                child,
                control,
                profile.stop_grace,
                profile.reap_timeout,
                output,
                errors,
                permit,
            )
            .await;
            Ok(caller_failure(
                &request,
                "transport_output_limit",
                "check_executor pipe limit exceeded",
            ))
        }
        Outcome::IoFailure => {
            input.abort();
            stop_child(
                child,
                control,
                profile.stop_grace,
                profile.reap_timeout,
                output,
                errors,
                permit,
            )
            .await;
            Ok(caller_failure(
                &request,
                "transport_malformed_response",
                "check_executor pipe failed",
            ))
        }
        Outcome::Finished { stdout, stderr, status } => {
            drop(permit);
            tracing::debug!(stderr_bytes = stderr.len(), "check_executor stderr captured");
            if !status.success() {
                return Ok(caller_failure(
                    &request,
                    "transport_exit_status",
                    "check_executor exited unsuccessfully",
                ));
            }
            let response: CheckResponse = match serde_json::from_slice(&stdout) {
                Ok(response) => response,
                Err(_) => {
                    return Ok(caller_failure(
                        &request,
                        "transport_malformed_response",
                        "check_executor returned malformed JSON",
                    ));
                }
            };
            if response.validate_for(&request).is_err() {
                let code = if response.request_id != request.request_id {
                    "transport_request_id_mismatch"
                } else {
                    "transport_malformed_response"
                };
                return Ok(caller_failure(
                    &request,
                    code,
                    "check_executor response violated the contract",
                ));
            }
            Ok(structured_response(&response))
        }
    }
}

enum Outcome {
    Cancelled,
    TimedOut,
    OutputLimit,
    IoFailure,
    Finished { stdout: Vec<u8>, stderr: Vec<u8>, status: std::process::ExitStatus },
}

struct Completed {
    input: Result<(), PipeError>,
    stdout: Result<DrainResult, PipeError>,
    stderr: Result<DrainResult, PipeError>,
    status: Result<std::process::ExitStatus, PipeError>,
}

fn join_result<T>(
    result: Result<Result<T, PipeError>, tokio::task::JoinError>,
) -> Result<T, PipeError> {
    result.unwrap_or(Err(PipeError::Io))
}

fn classify_completed(completed: Completed) -> Outcome {
    let status = match completed.status {
        Ok(status) => status,
        Err(_) => return Outcome::IoFailure,
    };
    if !status.success() {
        return Outcome::Finished { stdout: Vec::new(), stderr: Vec::new(), status };
    }
    match (completed.input, completed.stdout, completed.stderr) {
        (Ok(()), Ok(stdout), Ok(stderr)) if !stdout.overflow && !stderr.overflow => {
            Outcome::Finished { stdout: stdout.bytes, stderr: stderr.bytes, status }
        }
        _ => Outcome::IoFailure,
    }
}

#[cfg(unix)]
struct ProcessControl;

#[cfg(windows)]
struct ProcessControl {
    event: std::os::windows::io::OwnedHandle,
    job: std::os::windows::io::OwnedHandle,
}

#[cfg(unix)]
fn spawn_executor(
    profile: &CheckExecutorProfile,
    _permit: &mut Option<OwnedSemaphorePermit>,
) -> std::io::Result<(Child, ProcessControl)> {
    let child = Command::new(&profile.program)
        .args(&profile.args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(false)
        .spawn()?;
    Ok((child, ProcessControl))
}

#[cfg(windows)]
fn spawn_executor(
    profile: &CheckExecutorProfile,
    permit: &mut Option<OwnedSemaphorePermit>,
) -> std::io::Result<(Child, ProcessControl)> {
    use std::{
        ffi::OsStr,
        os::windows::{
            ffi::OsStrExt,
            io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle},
        },
    };
    use windows_sys::Win32::{
        Foundation::{CloseHandle, GetLastError, HANDLE},
        System::{
            Diagnostics::ToolHelp::{
                CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD,
                THREADENTRY32,
            },
            JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
                SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
                JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            },
            Threading::{
                CreateEventW, OpenThread, ResumeThread, CREATE_SUSPENDED, THREAD_SUSPEND_RESUME,
            },
        },
    };

    let event_name = format!("Local\\BSL_CHECK_STOP_{}", request_id());
    let event_name_wide: Vec<u16> = OsStr::new(&event_name).encode_wide().chain(Some(0)).collect();
    let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, event_name_wide.as_ptr()) };
    if event.is_null() {
        return Err(std::io::Error::from_raw_os_error(unsafe { GetLastError() } as i32));
    }
    let event = unsafe { OwnedHandle::from_raw_handle(event as RawHandle) };
    let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if job.is_null() {
        return Err(std::io::Error::from_raw_os_error(unsafe { GetLastError() } as i32));
    }
    let job = unsafe { OwnedHandle::from_raw_handle(job as RawHandle) };
    let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    let configured = unsafe {
        SetInformationJobObject(
            job.as_raw_handle() as HANDLE,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const _,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    if configured == 0 {
        let error = unsafe { GetLastError() } as i32;
        return Err(std::io::Error::from_raw_os_error(error));
    }

    #[cfg(all(test, windows))]
    if TEST_UI_LIMIT_TARGET_JOB.with(std::cell::Cell::get) {
        // A test-only, real Win32 admission rejection: the caller process is
        // already in a UI-limited parent job, so Windows rejects assigning its
        // suspended child to this UI-limited target job before ResumeThread.
        use windows_sys::Win32::System::JobObjects::{
            JobObjectBasicUIRestrictions, JOBOBJECT_BASIC_UI_RESTRICTIONS,
            JOB_OBJECT_UILIMIT_DESKTOP,
        };
        let ui =
            JOBOBJECT_BASIC_UI_RESTRICTIONS { UIRestrictionsClass: JOB_OBJECT_UILIMIT_DESKTOP };
        let configured = unsafe {
            SetInformationJobObject(
                job.as_raw_handle() as HANDLE,
                JobObjectBasicUIRestrictions,
                &ui as *const _ as *const _,
                std::mem::size_of_val(&ui) as u32,
            )
        };
        if configured == 0 {
            let error = unsafe { GetLastError() } as i32;
            return Err(std::io::Error::from_raw_os_error(error));
        }
    }

    let control = ProcessControl { event, job };
    let mut command = Command::new(&profile.program);
    command
        .args(&profile.args)
        .env("BSL_CHECK_STOP_EVENT", &event_name)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(false)
        .creation_flags(CREATE_SUSPENDED);
    let mut child = command.spawn()?;
    let process_handle = child.raw_handle().expect("running child handle") as HANDLE;
    if unsafe { AssignProcessToJobObject(control.job.as_raw_handle() as HANDLE, process_handle) }
        == 0
    {
        let error = unsafe { GetLastError() } as i32;
        #[cfg(all(test, windows))]
        if TEST_UI_LIMIT_TARGET_JOB.with(std::cell::Cell::get) {
            TEST_JOB_ASSIGN_ERROR.with(|value| value.set(error as u32));
        }
        let _ = child.start_kill();
        reap_in_background(child, control, permit.take().expect("admission failure owns permit"));
        return Err(std::io::Error::from_raw_os_error(error));
    }

    let pid = child.id().expect("new child has a process id");
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot as isize == -1 {
        let error = unsafe { GetLastError() } as i32;
        unsafe {
            TerminateJobObject(control.job.as_raw_handle() as HANDLE, 1);
        }
        reap_in_background(child, control, permit.take().expect("admission failure owns permit"));
        return Err(std::io::Error::from_raw_os_error(error));
    }
    let mut entry: THREADENTRY32 = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
    let mut found = unsafe { Thread32First(snapshot, &mut entry) };
    let mut main_thread = std::ptr::null_mut();
    while found != 0 {
        if entry.th32OwnerProcessID == pid {
            main_thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
            break;
        }
        found = unsafe { Thread32Next(snapshot, &mut entry) };
    }
    unsafe {
        CloseHandle(snapshot);
    }
    if main_thread.is_null() || unsafe { ResumeThread(main_thread) } == u32::MAX {
        let error = unsafe { GetLastError() } as i32;
        if !main_thread.is_null() {
            unsafe {
                CloseHandle(main_thread);
            }
        }
        unsafe {
            TerminateJobObject(control.job.as_raw_handle() as HANDLE, 1);
        }
        reap_in_background(child, control, permit.take().expect("admission failure owns permit"));
        return Err(std::io::Error::from_raw_os_error(error));
    }
    unsafe {
        CloseHandle(main_thread);
    }
    Ok((child, control))
}

#[derive(Debug)]
enum PipeError {
    Io,
}

struct DrainResult {
    bytes: Vec<u8>,
    overflow: bool,
}

async fn drain_bounded<R: AsyncRead + Unpin>(
    mut reader: R,
    limit: usize,
    overflow_tx: watch::Sender<bool>,
) -> Result<DrainResult, PipeError> {
    let mut bytes = Vec::new();
    let mut chunk = [0; 8192];
    let mut overflow = false;
    loop {
        let read = reader.read(&mut chunk).await.map_err(|_| PipeError::Io)?;
        if read == 0 {
            return Ok(DrainResult { bytes, overflow });
        }
        if !overflow && bytes.len().saturating_add(read) > limit {
            overflow = true;
            bytes.clear();
            overflow_tx.send_replace(true);
        }
        if !overflow {
            bytes.extend_from_slice(&chunk[..read]);
        }
    }
}

async fn stop_child(
    mut child: Child,
    control: ProcessControl,
    grace: Duration,
    reap: Duration,
    stdout: JoinHandle<Result<DrainResult, PipeError>>,
    stderr: JoinHandle<Result<DrainResult, PipeError>>,
    permit: OwnedSemaphorePermit,
) {
    control.soft_stop();
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        // SAFETY: signal only the directly owned executor PID, never its process group.
        unsafe {
            libc::kill(pid as i32, libc::SIGTERM);
        }
    }
    let grace_deadline = Instant::now() + grace;
    let mut wait = Box::pin(child.wait());
    let mut reaped = match tokio::time::timeout_at(grace_deadline, &mut wait).await {
        Ok(Ok(_)) => true,
        Ok(Err(error)) => {
            tracing::warn!(kind = %error.kind(), "check_executor wait failed during stop grace");
            false
        }
        Err(_) => false,
    };
    if Instant::now() < grace_deadline {
        tokio::time::sleep_until(grace_deadline).await;
    }
    drop(wait);
    if !reaped {
        #[cfg(unix)]
        if let Some(pid) = child.id() {
            // SAFETY: this is still the directly owned executor PID after its grace expired.
            unsafe {
                libc::kill(pid as i32, libc::SIGKILL);
            }
        }
        #[cfg(windows)]
        control.hard_stop();
    }

    stdout.abort();
    stderr.abort();
    let mut wait_reap = Box::pin(child.wait());
    if !reaped {
        match tokio::time::timeout(reap, &mut wait_reap).await {
            Ok(Ok(_)) => reaped = true,
            Ok(Err(error)) => {
                tracing::warn!(kind = %error.kind(), "check_executor wait failed during reap window")
            }
            Err(_) => {}
        }
    }
    drop(wait_reap);
    let _ = stdout.await;
    let _ = stderr.await;
    if !reaped {
        tracing::warn!(
            "check_executor child exceeded reap_timeout_ms; retaining its connection permit until reaped"
        );
        reap_in_background(child, control, permit);
    }
}

/// `wait` fails with ECHILD once something else has collected the child: there is nothing left
/// to reap, so retrying would hold the connection permit forever.
#[cfg(unix)]
fn child_is_gone(error: &std::io::Error) -> bool {
    error.raw_os_error() == Some(libc::ECHILD)
}

#[cfg(not(unix))]
fn child_is_gone(_error: &std::io::Error) -> bool {
    false
}

fn reap_in_background(mut child: Child, control: ProcessControl, permit: OwnedSemaphorePermit) {
    tokio::spawn(async move {
        let _permit = permit;
        let _control = control;
        loop {
            match child.wait().await {
                Ok(_) => break,
                Err(error) if child_is_gone(&error) => break,
                Err(error) => {
                    tracing::warn!(kind = %error.kind(), "check_executor background reap wait failed");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    });
}

#[cfg(unix)]
impl ProcessControl {
    fn soft_stop(&self) {}
}

#[cfg(windows)]
impl ProcessControl {
    fn soft_stop(&self) {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::HANDLE;
        unsafe {
            windows_sys::Win32::System::Threading::SetEvent(self.event.as_raw_handle() as HANDLE);
        }
    }

    fn hard_stop(&self) {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::HANDLE;
        unsafe {
            windows_sys::Win32::System::JobObjects::TerminateJobObject(
                self.job.as_raw_handle() as HANDLE,
                1,
            );
        }
    }
}

fn caller_failure(request: &CheckRequest, code: &str, message: &str) -> CallToolResult {
    let response = CheckResponse {
        schema_version: SCHEMA_VERSION.to_owned(),
        request_id: request.request_id.clone(),
        connection: request.connection.clone(),
        backend: "caller/1".to_owned(),
        status: match code {
            "context_required" => Status::ContextRequired,
            "unsupported" => Status::Unsupported,
            _ => Status::Error,
        },
        valid: None,
        issues: Vec::new(),
        contexts_required: Vec::new(),
        contexts_checked: Vec::new(),
        truncated: false,
        failure: Some(Failure { code: code.to_owned(), message: message.to_owned() }),
        compilation_status: None,
        compiler: None,
        cleanup_status: None,
        module_type: None,
        context: None,
        backend_info: None,
    };
    structured_response(&response)
}

pub(crate) fn http_position_unavailable(
    request_id: &str,
    connection: &str,
    error_text: &str,
) -> CallToolResult {
    let response = CheckResponse {
        schema_version: SCHEMA_VERSION.to_owned(),
        request_id: request_id.to_owned(),
        connection: connection.to_owned(),
        backend: "http/1".to_owned(),
        status: Status::Error,
        valid: None,
        issues: vec![check_contract::Issue {
            message: "The HTTP backend reported invalid BSL but supplied no source position."
                .to_owned(),
            line: None,
            column: None,
        }],
        contexts_required: Vec::new(),
        contexts_checked: vec![CompilerContext::Server],
        truncated: false,
        failure: Some(Failure {
            code: "position_unavailable".to_owned(),
            message: "HTTP backend did not provide a provable source position.".to_owned(),
        }),
        compilation_status: Some(check_contract::CompilationStatus::Invalid),
        compiler: None,
        cleanup_status: None,
        module_type: None,
        context: None,
        backend_info: None,
    };
    let mut result = structured_response(&response);
    result.content.push(rmcp::model::ContentBlock::text(error_text.to_owned()));
    result
}

pub(crate) fn http_valid(request_id: &str, connection: &str) -> CallToolResult {
    structured_response(&CheckResponse {
        schema_version: SCHEMA_VERSION.to_owned(),
        request_id: request_id.to_owned(),
        connection: connection.to_owned(),
        backend: "http/1".to_owned(),
        status: Status::Valid,
        valid: Some(true),
        issues: Vec::new(),
        contexts_required: Vec::new(),
        contexts_checked: vec![CompilerContext::Server],
        truncated: false,
        failure: None,
        compilation_status: Some(check_contract::CompilationStatus::Valid),
        compiler: None,
        cleanup_status: Some(CleanupStatus::Complete),
        module_type: None,
        context: None,
        backend_info: None,
    })
}

pub(crate) fn unsupported(connection: &str) -> CallToolResult {
    let request = empty_request(connection);
    caller_failure(&request, "unsupported", "No backend is configured for this check route.")
}

pub(crate) fn context_required(connection: &str, has_executor: bool) -> CallToolResult {
    let request = empty_request(connection);
    let message = if has_executor {
        "This source contains module declarations or directives; retry with input_kind=module and module context."
    } else {
        "This source contains module declarations or directives; module checks need a check_executor configured for this connection."
    };
    caller_failure(&request, "context_required", message)
}

fn empty_request(connection: &str) -> CheckRequest {
    CheckRequest {
        schema_version: SCHEMA_VERSION.to_owned(),
        request_id: request_id(),
        connection: connection.to_owned(),
        code: String::new(),
        limits: WireLimits {
            timeout_ms: 1,
            max_code_bytes: 1,
            max_response_bytes: 1,
            max_stderr_bytes: 1,
            stop_grace_ms: 1,
        },
        input_kind: InputKind::Snippet,
        module_type: None,
        context: None,
    }
}

pub(crate) fn new_request_id() -> String {
    request_id()
}

fn structured_response(response: &CheckResponse) -> CallToolResult {
    let value = serde_json::to_value(response).expect("CheckResponse is serializable");
    let text = format!("BSL check: {:?}.", response.status);
    let mut result = CallToolResult::success(vec![rmcp::model::ContentBlock::text(text)]);
    result.structured_content = Some(value);
    result
}

fn request_id() -> String {
    let sequence = REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("{}-{sequence}", std::process::id())
}

pub(crate) fn has_module_syntax(code: &str) -> bool {
    let root = parser::parse(code).syntax_node();
    root.descendants().any(|node| {
        matches!(
            node.kind(),
            syntax::SyntaxKind::PROCEDURE_DEF
                | syntax::SyntaxKind::FUNCTION_DEF
                | syntax::SyntaxKind::ANNOTATION
                | syntax::SyntaxKind::COMPILER_DIRECTIVE
        )
    })
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// Writes an executable through a child process. A file some process still holds open for
    /// writing cannot be executed (`ETXTBSY`), and a child that another test thread forks while
    /// this one writes holds the descriptor until it execs: a program written here and spawned
    /// at once would start or not depending on what the other threads were doing.
    fn write_executable(path: &std::path::Path, text: &str) {
        let staged = path.with_extension("staged");
        std::fs::write(&staged, text).unwrap();
        let installed = std::process::Command::new("install")
            .args(["-m", "700"])
            .arg(&staged)
            .arg(path)
            .status()
            .unwrap();
        assert!(installed.success(), "install {}", path.display());
    }

    #[test]
    fn reaper_stops_when_the_child_was_collected_elsewhere() {
        assert!(child_is_gone(&std::io::Error::from_raw_os_error(libc::ECHILD)));
        assert!(!child_is_gone(&std::io::Error::from_raw_os_error(libc::EINTR)));
    }

    #[test]
    fn ast_preflight_ignores_words_inside_strings_and_comments() {
        assert!(!has_module_syntax("Текст = \"Процедура &НаСервере\"; // Функция"));
        assert!(has_module_syntax("Процедура Проверка() КонецПроцедуры"));
        assert!(has_module_syntax("Функция Проверка() Возврат Истина; КонецФункции"));
        assert!(has_module_syntax("&НаКлиенте"));
    }

    #[test]
    fn caller_failures_are_complete_structured_contract_objects() {
        let profile = test_profile();
        let request = new_request("test", "x", &profile, InputKind::Snippet, None, None);
        let result = caller_failure(&request, "transport_timeout", "timeout");
        let value = result.structured_content.unwrap();
        let parsed: CheckResponse = serde_json::from_value(value).unwrap();
        assert_eq!(parsed.status, Status::Error);
        assert_eq!(parsed.valid, None);
        assert_eq!(parsed.failure.unwrap().code, "transport_timeout");
    }

    fn test_profile() -> CheckExecutorProfile {
        CheckExecutorProfile {
            program: "unused".into(),
            args: Vec::new(),
            timeout: Duration::from_secs(2),
            max_code_bytes: 1024,
            max_response_bytes: 16384,
            max_stderr_bytes: 1024,
            stop_grace: Duration::from_millis(500),
            reap_timeout: Duration::from_millis(100),
            max_concurrent: 1,
            handles_snippet: true,
            semaphore: std::sync::Arc::new(tokio::sync::Semaphore::new(1)),
        }
    }

    const FAKE_EXECUTOR: &str = r#"#!/usr/bin/env python3
import json, os, signal, subprocess, sys, time
mode = sys.argv[1]
if mode == 'exit1-immediate':
    print('{"status":"valid"}', flush=True)
    sys.exit(1)
request = json.loads(sys.stdin.buffer.read())
if mode in ('coordinated-within-r', 'coordinated-over-r'):
    event_path, manifest_path, recovery_path, identity_path, ready_path, observe_delay = sys.argv[2:8]
    child_code = "import pathlib,signal,sys,time; signal.signal(signal.SIGTERM,signal.SIG_IGN); pathlib.Path(sys.argv[2]).touch(); time.sleep(60)"
    child = subprocess.Popen([sys.executable, '-c', child_code, 'check-executor-owned-descendant', ready_path],
                             stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    while not os.path.exists(ready_path): time.sleep(0.005)
    with open('/proc/%d/stat' % child.pid) as stat_file:
        start_ticks = stat_file.read().rsplit(')', 1)[1].split()[19]
    with open(manifest_path, 'w') as manifest_file:
        manifest_file.write('%d %s' % (child.pid, start_ticks))
    def event(name):
        with open(event_path, 'a') as events:
            events.write('%s %d\n' % (name, time.monotonic_ns()))
    def on_term(*_):
        event('term')
        over_r = mode == 'coordinated-over-r'
        time.sleep(float(observe_delay))
        event('stop-observed')
        if over_r:
            with open(recovery_path, 'w') as recovery_file:
                recovery_file.write('cleanup budget exhausted; recovery required')
            child.kill()
            child.wait(timeout=1)
            while True: time.sleep(1)
        event('cleanup-started')
        child.send_signal(signal.SIGTERM)
        time.sleep(0.030)
        if child.poll() is None: child.kill()
        child.wait(timeout=1)
        event('cleanup-complete')
        os.unlink(manifest_path)
        raise SystemExit(0)
    signal.signal(signal.SIGTERM, on_term)
    with open(identity_path + '.tmp', 'w') as identity_file:
        identity_file.write('%d %s' % (child.pid, start_ticks))
    os.rename(identity_path + '.tmp', identity_path)
    while True: time.sleep(1)
if mode.startswith('route'):
    with open(sys.argv[2], 'a') as marker:
        marker.write(request['input_kind'] + '\n')
if mode == 'hang': time.sleep(30)
if mode == 'term-ignore':
    signal.signal(signal.SIGTERM, signal.SIG_IGN)
    time.sleep(30)
if mode == 'early-term-exit':
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
    time.sleep(30)
if mode == 'descendant-pipe':
    child = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(0.45)'])
    sys.exit(0)
if mode == 'overflow':
    sys.stdout.buffer.write(b'x' * 20000); sys.stdout.flush(); time.sleep(30)
    sys.exit(0)
if mode == 'stderr-overflow':
    sys.stderr.buffer.write(b'x' * 20000); sys.stderr.flush(); time.sleep(30)
    sys.exit(0)
if mode == 'malformed':
    print('{'); sys.exit(0)
if mode == 'two-documents':
    print('{} {}'); sys.exit(0)

status = mode
valid = {'valid': True, 'invalid': False}.get(status)
failure = None
issues = []
compilation = None
cleanup = None
contexts = []
if status == 'valid':
    compilation, cleanup, contexts = 'valid', 'complete', ['server']
elif status == 'invalid':
    compilation, contexts = 'invalid', ['server']
    issues = [{'message':'bad source','line':1,'column':1}]
elif status in ('context_required', 'unsupported'):
    failure = {'code':status, 'message':'failure'}
elif status == 'error' or status.startswith('reserved-'):
    status = 'error'
    failure = {'code': (mode[9:] if mode.startswith('reserved-') else mode), 'message':'failure'}
elif status == 'route-error':
    status, valid = 'error', None
    failure = {'code':'executor_failed', 'message':'selected worker failed'}
else:
    status = 'valid'
    valid, compilation, cleanup, contexts = True, 'valid', 'complete', ['server']
response = {
  'schema_version':'1', 'request_id':request['request_id'], 'connection':request['connection'],
  'backend':'fake/1', 'status':status, 'valid':valid, 'issues':issues,
  'contexts_required':[], 'contexts_checked':contexts, 'truncated':False, 'failure':failure,
  'compilation_status':compilation, 'compiler':None, 'cleanup_status':cleanup,
  'module_type':request.get('module_type') if mode.startswith('route') else None,
  'context':request.get('context') if mode.startswith('route') else None, 'backend_info':None
}
if mode == 'wrong-id': response['request_id'] = 'wrong'
if mode == 'unknown-field': response['extra'] = True
if mode == 'status-mismatch': response['valid'] = False
if mode == 'context-mismatch':
    response['context'] = {'kind':'metadata', 'owner':'X', 'origin':{'kind':'configuration'}}
if mode == 'record-timeout':
    with open(sys.argv[2], 'w') as limits_file:
        limits_file.write(str(request['limits']['timeout_ms']))
encoded = json.dumps(response)
if mode == 'two-documents': encoded += '{}'
print(encoded)
if mode == 'exit1': sys.exit(1)
"#;

    async fn fake_result(
        mode: &str,
        mutate: impl FnOnce(&mut CheckExecutorProfile),
    ) -> CallToolResult {
        let temp = tempfile::tempdir().unwrap();
        let program = temp.path().join("executor.py");
        write_executable(&program, FAKE_EXECUTOR);
        let mut profile = test_profile();
        profile.program = program;
        profile.args = vec![mode.into()];
        mutate(&mut profile);
        check(&profile, "fake", "x = 1;", InputKind::Snippet, None, None, CancellationToken::new())
            .await
            .unwrap()
    }

    fn result_code(result: &CallToolResult) -> Option<String> {
        let value = result.structured_content.as_ref()?;
        serde_json::from_value::<CheckResponse>(value.clone()).ok()?.failure.map(|f| f.code)
    }

    async fn run_routing_check(
        profile: Option<CheckExecutorProfile>,
        with_http: bool,
        code: &str,
        input_kind: InputKind,
        module_type: Option<ModuleType>,
        context: Option<ModuleContext>,
    ) -> Result<CallToolResult, McpError> {
        let client =
            with_http.then(|| onec_client::Client::new("unsupported+route://no-network", "", ""));
        let mut state = crate::SharedState::shared();
        state.add_onec_connection(
            "route-test".to_owned(),
            crate::state::OnecConnection::with_optional_http(client, false, profile),
        );
        crate::tools::execution::check_syntax(
            &state,
            code,
            Some("route-test"),
            6000,
            input_kind,
            module_type,
            context,
            CancellationToken::new(),
        )
        .await
    }

    fn routing_profile(
        program: &std::path::Path,
        marker: &std::path::Path,
        mode: &str,
        handles_snippet: bool,
    ) -> CheckExecutorProfile {
        let mut profile = test_profile();
        profile.program = program.to_owned();
        profile.args = vec![mode.into(), marker.as_os_str().to_owned()];
        profile.handles_snippet = handles_snippet;
        profile
    }

    fn read_route_marker(marker: &std::path::Path) -> Vec<String> {
        std::fs::read_to_string(marker).unwrap_or_default().lines().map(str::to_owned).collect()
    }

    #[tokio::test]
    async fn input_bounds_reject_before_ast_preflight_or_executor_spawn() {
        let temp = tempfile::tempdir().unwrap();
        let program = temp.path().join("executor.py");
        write_executable(&program, FAKE_EXECUTOR);
        let marker = temp.path().join("spawned");
        let profile = routing_profile(&program, &marker, "route-valid", true);

        let code = format!("Процедура Проверка() КонецПроцедуры\n{}", " ".repeat(1024));
        let error =
            run_routing_check(Some(profile.clone()), false, &code, InputKind::Snippet, None, None)
                .await
                .unwrap_err();
        assert!(error.message.contains("max_code_bytes"));

        let connection = "x".repeat(6 * profile.max_code_bytes as usize + 65536 + 1);
        let error = check(
            &profile,
            &connection,
            "x = 1;",
            InputKind::Snippet,
            None,
            None,
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert!(error.message.contains("frame limit"));
        assert!(!marker.exists());
        assert_eq!(profile.semaphore.available_permits(), 1);
    }

    #[tokio::test]
    async fn check_syntax_routes_snippets_and_modules_by_profile_without_fallback() {
        let temp = tempfile::tempdir().unwrap();
        let program = temp.path().join("route-executor.py");
        write_executable(&program, FAKE_EXECUTOR);
        let marker = temp.path().join("routes.log");

        // Snippet routing follows handles_snippet, while module input always selects the
        // executor when it is configured. The worker echoes the request's module metadata.
        for handles_snippet in [true, false] {
            let before_snippet = read_route_marker(&marker);
            let profile = routing_profile(&program, &marker, "route-valid", handles_snippet);
            let snippet = run_routing_check(
                Some(profile.clone()),
                true,
                "x = 1;",
                InputKind::Snippet,
                None,
                None,
            )
            .await;
            if handles_snippet {
                let response: CheckResponse =
                    serde_json::from_value(snippet.unwrap().structured_content.unwrap()).unwrap();
                assert_eq!(response.status, Status::Valid);
                assert_eq!(response.backend, "fake/1");
                assert_eq!(read_route_marker(&marker).len(), before_snippet.len() + 1);
                assert_eq!(read_route_marker(&marker).last().map(String::as_str), Some("snippet"));
            } else {
                let error = snippet.expect_err("HTTP route rejects unsupported URL scheme locally");
                assert!(format!("{error:?}").contains("Ошибка проверки синтаксиса в 1С"));
                assert_eq!(read_route_marker(&marker), before_snippet);
            }

            let context = ModuleContext::Synthetic { module_type: ModuleType::Common };
            let module = run_routing_check(
                Some(profile),
                true,
                "Перем X;",
                InputKind::Module,
                Some(ModuleType::Common),
                Some(context.clone()),
            )
            .await
            .unwrap();
            let response: CheckResponse =
                serde_json::from_value(module.structured_content.unwrap()).unwrap();
            assert_eq!(response.status, Status::Valid);
            assert_eq!(response.module_type, Some(ModuleType::Common));
            assert_eq!(response.context, Some(context));
            assert_eq!(read_route_marker(&marker).last().map(String::as_str), Some("module"));
        }

        // Executor-only connections return unsupported for snippets when disabled, but modules
        // still use the worker. A selected worker error is returned without trying HTTP.
        let no_snippets = routing_profile(&program, &marker, "route-valid", false);
        let unsupported = run_routing_check(
            Some(no_snippets.clone()),
            false,
            "x = 1;",
            InputKind::Snippet,
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(result_code(&unsupported).as_deref(), Some("unsupported"));
        assert_eq!(read_route_marker(&marker), ["snippet", "module", "module"]);

        let module = run_routing_check(
            Some(no_snippets),
            false,
            "Перем X;",
            InputKind::Module,
            Some(ModuleType::Common),
            Some(ModuleContext::Synthetic { module_type: ModuleType::Common }),
        )
        .await
        .unwrap();
        assert_eq!(
            serde_json::from_value::<CheckResponse>(module.structured_content.unwrap())
                .unwrap()
                .status,
            Status::Valid
        );
        let worker_error = run_routing_check(
            Some(routing_profile(&program, &marker, "route-error", true)),
            true,
            "x = 1;",
            InputKind::Snippet,
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(result_code(&worker_error).as_deref(), Some("executor_failed"));
        assert_eq!(read_route_marker(&marker).last().map(String::as_str), Some("snippet"));

        // Real declarations/directives are rejected before spawning; words in literals and
        // comments do not trigger AST preflight and keep the chosen worker route.
        let before_preflight = read_route_marker(&marker);
        for code in ["Процедура Проверка() КонецПроцедуры", "&НаКлиенте"]
        {
            let preflight = run_routing_check(
                Some(routing_profile(&program, &marker, "route-valid", true)),
                false,
                code,
                InputKind::Snippet,
                None,
                None,
            )
            .await
            .unwrap();
            assert_eq!(result_code(&preflight).as_deref(), Some("context_required"));
        }
        assert_eq!(read_route_marker(&marker), before_preflight);

        let literal_and_comment = run_routing_check(
            Some(routing_profile(&program, &marker, "route-valid", true)),
            false,
            "Text = \"Процедура Hidden() &НаСервере\"; // Функция Hidden()",
            InputKind::Snippet,
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            serde_json::from_value::<CheckResponse>(
                literal_and_comment.structured_content.unwrap()
            )
            .unwrap()
            .status,
            Status::Valid
        );
        assert_eq!(read_route_marker(&marker).last().map(String::as_str), Some("snippet"));
    }

    #[tokio::test]
    async fn fake_executor_accepts_each_contract_status_and_rejects_bad_wire() {
        for (mode, expected_status) in [
            ("valid", Status::Valid),
            ("invalid", Status::Invalid),
            ("context_required", Status::ContextRequired),
            ("unsupported", Status::Unsupported),
            ("error", Status::Error),
        ] {
            let result = fake_result(mode, |_| {}).await;
            let response: CheckResponse =
                serde_json::from_value(result.structured_content.unwrap()).unwrap();
            assert_eq!(response.status, expected_status, "mode={mode}");
        }
        for (mode, expected_code) in [
            ("malformed", "transport_malformed_response"),
            ("two-documents", "transport_malformed_response"),
            ("wrong-id", "transport_request_id_mismatch"),
            ("exit1", "transport_exit_status"),
            ("exit1-immediate", "transport_exit_status"),
            ("unknown-field", "transport_malformed_response"),
            ("status-mismatch", "transport_malformed_response"),
            ("context-mismatch", "transport_malformed_response"),
            ("reserved-executor_not_found", "transport_malformed_response"),
            ("reserved-spawn_failed", "transport_malformed_response"),
            ("reserved-transport_timeout", "transport_malformed_response"),
            ("reserved-transport_output_limit", "transport_malformed_response"),
            ("reserved-transport_malformed_response", "transport_malformed_response"),
            ("reserved-transport_request_id_mismatch", "transport_malformed_response"),
            ("reserved-transport_exit_status", "transport_malformed_response"),
        ] {
            let result = fake_result(mode, |_| {}).await;
            assert_eq!(result_code(&result).as_deref(), Some(expected_code), "mode={mode}");
        }
    }

    #[tokio::test]
    async fn fake_executor_enforces_pipe_and_deadline_limits_and_reaps_term_ignoring_child() {
        for mode in ["overflow", "stderr-overflow"] {
            let result = fake_result(mode, |profile| {
                profile.timeout = Duration::from_secs(3);
                profile.max_response_bytes = 16_384;
                profile.max_stderr_bytes = 1_024;
            })
            .await;
            assert_eq!(
                result_code(&result).as_deref(),
                Some("transport_output_limit"),
                "mode={mode}"
            );
        }
        for mode in ["hang", "term-ignore", "early-term-exit", "descendant-pipe"] {
            let started = Instant::now();
            let result = fake_result(mode, |profile| {
                profile.timeout = Duration::from_millis(100);
                profile.stop_grace = Duration::from_millis(150);
                profile.reap_timeout = Duration::from_millis(250);
            })
            .await;
            assert_eq!(result_code(&result).as_deref(), Some("transport_timeout"), "mode={mode}");
            if mode != "descendant-pipe" {
                assert!(
                    started.elapsed() >= Duration::from_millis(230),
                    "full grace must be observed"
                );
            } else {
                assert!(
                    started.elapsed() >= Duration::from_millis(90),
                    "held descendant pipe must remain under deadline"
                );
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            assert!(
                started.elapsed() < Duration::from_secs(3),
                "hard stop and reap remain bounded"
            );
        }
    }

    #[tokio::test]
    async fn caller_grace_reserves_recovery_window_for_delayed_native_stop_observation() {
        struct OwnedDescendant {
            pid: u32,
            start_ticks: String,
        }

        impl OwnedDescendant {
            fn from_identity(path: &std::path::Path) -> Self {
                let identity = std::fs::read_to_string(path).unwrap();
                let mut fields = identity.split_whitespace();
                Self {
                    pid: fields.next().unwrap().parse().unwrap(),
                    start_ticks: fields.next().unwrap().to_owned(),
                }
            }

            fn state(&self) -> Option<char> {
                let stat = std::fs::read_to_string(format!("/proc/{}/stat", self.pid)).ok()?;
                let (_, rest) = stat.rsplit_once(')')?;
                let cmdline = std::fs::read(format!("/proc/{}/cmdline", self.pid)).ok()?;
                if !cmdline
                    .windows(b"check-executor-owned-descendant".len())
                    .any(|window| window == b"check-executor-owned-descendant")
                {
                    return None;
                }
                let fields: Vec<_> = rest.split_whitespace().collect();
                if fields.get(19).copied() != Some(self.start_ticks.as_str()) {
                    return None;
                }
                fields.first()?.chars().next()
            }

            fn terminate_if_still_owned(&self) {
                if matches!(self.state(), Some(state) if state != 'Z') {
                    // SAFETY: state() verifies PID start time and the unique child command marker.
                    unsafe { libc::kill(self.pid as i32, libc::SIGKILL) };
                }
            }
        }

        impl Drop for OwnedDescendant {
            fn drop(&mut self) {
                self.terminate_if_still_owned();
            }
        }

        // Cancellation runs the same stop path as a deadline, but it is requested once the
        // executor has installed its stop handler instead of at a wall-clock instant a loaded
        // machine can reach first. Time bounds that hold only on a fast machine sit under
        // `timing_contract`; the rest are lower bounds.
        const GRACE_MS: u64 = 2_000;
        let grace = Duration::from_millis(GRACE_MS);
        let recovery_window = Duration::from_millis(GRACE_MS.div_ceil(4));
        let native_cleanup_budget = grace - recovery_window;
        for mode in ["coordinated-within-r", "coordinated-over-r"] {
            let observe_delay = if mode == "coordinated-over-r" {
                recovery_window + Duration::from_millis(100)
            } else {
                Duration::from_millis(40)
            };
            let temp = tempfile::tempdir().unwrap();
            let program = temp.path().join("coordinated-executor.py");
            let event_path = temp.path().join("events");
            let manifest_path = temp.path().join("stop-manifest");
            let recovery_path = temp.path().join("recovery-required");
            let identity_path = temp.path().join("child-identity");
            let ready_path = temp.path().join("child-ready");
            write_executable(&program, FAKE_EXECUTOR);
            let mut profile = test_profile();
            profile.program = program;
            profile.args = vec![
                mode.into(),
                event_path.clone().into_os_string(),
                manifest_path.clone().into_os_string(),
                recovery_path.clone().into_os_string(),
                identity_path.clone().into_os_string(),
                ready_path.into_os_string(),
                observe_delay.as_secs_f64().to_string().into(),
            ];
            profile.timeout = Duration::from_secs(60 * 60);
            profile.stop_grace = grace;
            profile.reap_timeout = test_utils::wait::hang_guard(Duration::from_millis(150));

            let cancel = CancellationToken::new();
            let checked =
                check(&profile, "fake", "x = 1;", InputKind::Snippet, None, None, cancel.clone());
            let mut checked = std::pin::pin!(checked);
            let identity_published = async {
                // The executor publishes its identity only after its stop handler is installed.
                while !identity_path.exists() {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            };
            let requested = tokio::time::timeout(
                test_utils::wait::hang_guard(Duration::from_secs(60)),
                async {
                    tokio::select! {
                        early = &mut checked => {
                            panic!("check ended before the executor was ready to stop: {early:?}")
                        }
                        () = identity_published => {}
                    }
                    let requested = Instant::now();
                    cancel.cancel();
                    requested
                },
            )
            .await
            .expect("fake executor published its identity");
            let checked = checked.await;
            let descendant = OwnedDescendant::from_identity(&identity_path);
            assert!(checked.is_err(), "a cancelled check reports cancellation, mode={mode}");

            let events = std::fs::read_to_string(&event_path).unwrap();
            let event_time = |name: &str| -> u128 {
                events
                    .lines()
                    .find_map(|line| {
                        let (event, timestamp) = line.split_once(' ')?;
                        (event == name).then(|| timestamp.parse().unwrap())
                    })
                    .unwrap_or_else(|| panic!("missing {name} event in {events:?}"))
            };
            let term = event_time("term");
            let observed = event_time("stop-observed");
            assert!(observed > term);
            if mode == "coordinated-within-r" {
                let cleanup = event_time("cleanup-complete");
                if cfg!(timing_contract) {
                    assert!(
                        Duration::from_nanos((observed - term) as u64) < recovery_window,
                        "stop observation must leave the full R recovery window"
                    );
                    assert!(
                        Duration::from_nanos((cleanup - observed) as u64) < native_cleanup_budget,
                        "native cleanup must finish inside B=G-R"
                    );
                    assert!(cleanup < term + grace.as_nanos());
                }
                assert!(cleanup > observed, "cleanup follows the observed stop");
                assert!(!manifest_path.exists(), "successful cleanup removes its manifest");
                assert!(!recovery_path.exists(), "in-budget cleanup does not require recovery");
                assert!(matches!(descendant.state(), None | Some('Z')), "owned child was reaped");
            } else {
                assert!(
                    Duration::from_nanos((observed - term) as u64) > recovery_window,
                    "late stop observation exceeds R"
                );
                assert!(recovery_path.exists(), "over-R cleanup retains recovery evidence");
                assert!(manifest_path.exists(), "over-R cleanup retains its manifest");
                assert!(
                    !events.lines().any(|line| line.starts_with("cleanup-started ")),
                    "over-R cleanup cannot count as success"
                );
                assert!(requested.elapsed() >= grace, "caller waits its full G before returning");
                assert!(
                    matches!(descendant.state(), None | Some('Z')),
                    "fake executor reclaimed its child"
                );
            }
            descendant.terminate_if_still_owned();
            assert!(
                matches!(descendant.state(), None | Some('Z')),
                "no owned descendant survives caller return"
            );
        }
    }

    #[tokio::test]
    async fn queue_deadline_and_cancellation_do_not_spawn_executor() {
        let temp = tempfile::tempdir().unwrap();
        let marker = temp.path().join("spawned");
        let program = temp.path().join("executor.py");
        write_executable(
            &program,
            &format!(
                "#!/usr/bin/env python3\nimport pathlib,sys\npathlib.Path({:?}).touch()\n",
                marker.to_string_lossy()
            ),
        );
        let mut profile = test_profile();
        profile.program = program;
        profile.timeout = Duration::from_millis(40);
        profile.semaphore = Arc::new(tokio::sync::Semaphore::new(0));
        let result = check(
            &profile,
            "fake",
            "x=1;",
            InputKind::Snippet,
            None,
            None,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(result_code(&result).as_deref(), Some("transport_timeout"));
        assert!(!marker.exists(), "expired queue must not spawn");

        let queued_limits = temp.path().join("queued-timeout-ms");
        let queued_program = temp.path().join("queued-executor.py");
        write_executable(&queued_program, FAKE_EXECUTOR);
        let mut queued_profile = test_profile();
        queued_profile.program = queued_program;
        // Far past the scheduling of the permit below, so the queue cannot run out first.
        queued_profile.timeout = Duration::from_secs(600);
        queued_profile.args = vec!["record-timeout".into(), queued_limits.clone().into_os_string()];
        queued_profile.semaphore = Arc::new(tokio::sync::Semaphore::new(0));
        let semaphore = queued_profile.semaphore.clone();
        let queued = tokio::spawn({
            let profile = queued_profile.clone();
            async move {
                check(
                    &profile,
                    "fake",
                    "x=1;",
                    InputKind::Snippet,
                    None,
                    None,
                    CancellationToken::new(),
                )
                .await
            }
        });
        tokio::time::sleep(Duration::from_millis(120)).await;
        semaphore.add_permits(1);
        let result = queued.await.unwrap().unwrap();
        assert_eq!(result_code(&result), None);
        let forwarded = std::fs::read_to_string(queued_limits).unwrap().parse::<u64>().unwrap();
        assert!(
            forwarded > 0 && forwarded < 600_000,
            "queued request receives only the remaining deadline: {forwarded}ms"
        );

        let token = CancellationToken::new();
        let queued = tokio::spawn({
            let profile = profile.clone();
            let token = token.clone();
            async move { check(&profile, "fake", "x=1;", InputKind::Snippet, None, None, token).await }
        });
        tokio::time::sleep(Duration::from_millis(10)).await;
        token.cancel();
        assert!(
            queued.await.unwrap().is_err(),
            "cancellation while queued is an internal MCP error"
        );
        assert!(!marker.exists(), "cancelled queue must not spawn");
    }

    #[tokio::test]
    async fn background_reaper_keeps_connection_permit_until_actual_reap() {
        let semaphore = Arc::new(tokio::sync::Semaphore::new(1));
        let permit = semaphore.clone().acquire_owned().await.unwrap();
        let child = Command::new("sleep")
            .arg("30")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(false)
            .spawn()
            .unwrap();
        let pid = child.id().unwrap();
        // SAFETY: this test signals only the child it just spawned.
        assert_eq!(unsafe { libc::kill(pid as i32, libc::SIGSTOP) }, 0);
        reap_in_background(child, ProcessControl, permit);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(semaphore.available_permits(), 0, "permit remains held while child is unreaped");
        // SAFETY: this test resumes cleanup of the same owned child.
        assert_eq!(unsafe { libc::kill(pid as i32, libc::SIGKILL) }, 0);
        tokio::time::timeout(Duration::from_secs(2), async {
            while semaphore.available_permits() == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("reaper returns the permit only after child.wait completes");
    }
}
