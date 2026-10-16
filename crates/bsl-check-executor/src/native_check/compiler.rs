use std::ffi::OsString;
use std::future::Future;
use std::io;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::task::JoinHandle;
use tokio::time::sleep_until;
use tokio_util::sync::CancellationToken;

use super::NativeProfile;

static STOP_GRACE_MS: AtomicU64 = AtomicU64::new(2_000);

tokio::task_local! {
    static STOP_CLOCK: StopClock;
}

#[derive(Clone)]
pub(crate) struct StopClock {
    started: Arc<Mutex<Option<Instant>>>,
    budget: std::time::Duration,
}

impl StopClock {
    fn new() -> Self {
        Self {
            started: Arc::new(Mutex::new(None)),
            budget: cleanup_budget(STOP_GRACE_MS.load(Ordering::Relaxed)),
        }
    }

    pub(crate) fn mark(&self) -> Instant {
        let mut started = self.started.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        *started.get_or_insert_with(Instant::now)
    }

    pub(crate) fn cleanup_deadline(&self) -> Instant {
        self.started
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .unwrap_or_else(Instant::now)
            + self.budget
    }

    fn stop_deadline(&self, external_stop: bool) -> Instant {
        (if external_stop { self.mark() } else { Instant::now() }) + self.budget
    }
}

pub(crate) fn current_stop_clock() -> StopClock {
    STOP_CLOCK.try_with(Clone::clone).unwrap_or_else(|_| StopClock::new())
}

struct StopWatch {
    task: tokio::task::JoinHandle<()>,
    cancel: CancellationToken,
    clock: StopClock,
    complete: bool,
}

impl Drop for StopWatch {
    fn drop(&mut self) {
        if !self.complete {
            self.clock.mark();
        }
        self.cancel.cancel();
        self.task.abort();
    }
}

pub(crate) async fn with_stop_clock<F: Future>(
    cancel: CancellationToken,
    deadline: Instant,
    future: F,
) -> F::Output {
    let clock = StopClock::new();
    let watcher_cancel = CancellationToken::new();
    let watcher_stop = watcher_cancel.clone();
    let watcher_clock = clock.clone();
    let task = tokio::spawn(async move {
        tokio::select! {
            _ = cancel.cancelled() => { watcher_clock.mark(); }
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => { watcher_clock.mark(); }
            _ = watcher_stop.cancelled() => {}
        }
    });
    let mut watcher = StopWatch { task, cancel: watcher_cancel, clock, complete: false };
    let output = STOP_CLOCK.scope(watcher.clock.clone(), future).await;
    watcher.complete = true;
    output
}

pub(super) fn configure_stop_grace(milliseconds: u64) {
    STOP_GRACE_MS.store(milliseconds, Ordering::Relaxed);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Termination {
    Completed(Option<i32>),
    Cancelled,
    Deadline,
    OutputLimit,
}

pub(crate) struct ProcessOutput {
    pub termination: Termination,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub truncated: bool,
    #[cfg(target_os = "linux")]
    pub x11_conflict: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProcessFailure {
    Spawn,
    Wait,
    OutputRead,
    Cleanup,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NativeDiagnostic {
    pub owner: String,
    pub line: u32,
    pub column: u32,
    pub message: String,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ParsedDiagnostics {
    pub issues: Vec<NativeDiagnostic>,
    pub truncated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DiagnosticParseError {
    InvalidUtf8,
    MalformedPosition,
}

pub(crate) fn parse_diagnostics(log: &[u8]) -> Result<ParsedDiagnostics, DiagnosticParseError> {
    let text = std::str::from_utf8(log).map_err(|_| DiagnosticParseError::InvalidUtf8)?;
    let mut issues = Vec::new();
    let mut truncated = false;
    for line in text.lines() {
        let Some(open) = line.find('{') else { continue };
        let Some(close_rel) = line[open + 1..].find("}: ") else { continue };
        let close = open + 1 + close_rel;
        let location = &line[open + 1..close];
        let Some(paren) = location.rfind('(') else { continue };
        let Some((line_number, column)) =
            location[paren + 1..].strip_suffix(')').and_then(|pair| {
                let (line, column) = pair.split_once(',')?;
                Some((line.parse::<u32>().ok()?, column.parse::<u32>().ok()?))
            })
        else {
            return Err(DiagnosticParseError::MalformedPosition);
        };
        let owner = &location[..paren];
        if owner.is_empty()
            || owner.len() > 512
            || owner
                .chars()
                .any(|character| character.is_control() || character == '{' || character == '}')
            || line_number == 0
            || column == 0
        {
            return Err(DiagnosticParseError::MalformedPosition);
        }
        if issues.len() == 64 {
            truncated = true;
            continue;
        }
        issues.push(NativeDiagnostic {
            owner: owner.to_owned(),
            line: line_number,
            column,
            message: safe_message(&line[close + 3..]),
        });
    }
    Ok(ParsedDiagnostics { issues, truncated })
}

fn safe_message(message: &str) -> String {
    let mut safe = String::with_capacity(message.len().min(256));
    let mut quote = None;
    let mut escaped = false;
    for character in message.chars() {
        if let Some(delimiter) = quote {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == delimiter {
                quote = None;
                safe.push_str("<literal>");
            }
            continue;
        }
        if character == '"' || character == '\'' {
            quote = Some(character);
        } else if character.is_control() {
            if character == '\t' {
                safe.push(' ');
            }
        } else {
            safe.push(character);
        }
        if safe.len() >= 256 {
            break;
        }
    }
    if safe.len() > 256 {
        safe.truncate(previous_char_boundary(&safe, 256));
    }
    if quote.is_some() {
        safe.push_str("<literal>");
        if safe.len() > 256 {
            safe.truncate(previous_char_boundary(&safe, 256));
        }
    }
    safe.trim().to_owned()
}

fn previous_char_boundary(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

pub(crate) async fn run_designer(
    profile: &NativeProfile,
    args: &[OsString],
    cwd: &Path,
    log_path: &Path,
    deadline: Instant,
    cancel: &CancellationToken,
    max_output_bytes: usize,
) -> Result<ProcessOutput, ProcessFailure> {
    let mut designer_args = args.to_vec();
    designer_args.push(OsString::from("/Out"));
    designer_args.push(log_path.as_os_str().to_owned());
    #[cfg(target_os = "linux")]
    if let Some(wrapper) = &profile.xvfb_run_path {
        return run_designer_xvfb(
            wrapper,
            profile,
            &designer_args,
            cwd,
            log_path,
            deadline,
            cancel,
            max_output_bytes,
            Path::new("/tmp"),
        )
        .await;
    }
    if profile.xvfb_run_path.is_some() {
        return Err(ProcessFailure::Spawn);
    }
    create_native_log(log_path)?;
    run_bounded_with_log(
        &profile.designer_path,
        &designer_args,
        cwd,
        deadline,
        cancel,
        max_output_bytes,
        Some((log_path, max_output_bytes)),
        None,
    )
    .await
}

fn create_native_log(path: &Path) -> Result<(), ProcessFailure> {
    use std::io::Write;
    let mut log = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|_| ProcessFailure::Spawn)?;
    log.flush().map_err(|_| ProcessFailure::Spawn)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|_| ProcessFailure::Spawn)?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
#[cfg(target_os = "linux")]
async fn run_designer_xvfb(
    wrapper: &Path,
    profile: &NativeProfile,
    designer_args: &[OsString],
    cwd: &Path,
    log_path: &Path,
    deadline: Instant,
    cancel: &CancellationToken,
    max_output_bytes: usize,
    x11_root: &Path,
) -> Result<ProcessOutput, ProcessFailure> {
    for attempt in 0..10_u16 {
        if cancel.is_cancelled() {
            return Ok(ProcessOutput {
                termination: Termination::Cancelled,
                stdout: Vec::new(),
                stderr: Vec::new(),
                truncated: false,
                x11_conflict: false,
            });
        }
        if Instant::now() >= deadline {
            return Ok(ProcessOutput {
                termination: Termination::Deadline,
                stdout: Vec::new(),
                stderr: Vec::new(),
                truncated: false,
                x11_conflict: false,
            });
        }
        let display = super::x11::next_display(std::process::id(), attempt);
        if super::x11::occupied(x11_root, display).map_err(|_| ProcessFailure::Cleanup)? {
            continue;
        }
        create_native_log(log_path)?;
        let error_path = cwd.join(format!(".native-check-xvfb-{display}.err"));
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&error_path)
            .map_err(|_| ProcessFailure::Spawn)?;
        let args = xvfb_wrapper_args(display, &error_path, &profile.designer_path, designer_args);
        let mut output = run_bounded_with_log(
            wrapper,
            &args,
            cwd,
            deadline,
            cancel,
            max_output_bytes,
            Some((log_path, max_output_bytes)),
            Some((x11_root, display)),
        )
        .await?;
        let startup_conflict = matches!(output.termination, Termination::Completed(Some(code)) if code != 0)
            && super::x11::is_startup_conflict(&error_path);
        let _ = std::fs::remove_file(&error_path);
        if startup_conflict && attempt < 9 {
            let _ = std::fs::remove_file(log_path);
            continue;
        }
        if output.x11_conflict && !startup_conflict {
            return Err(ProcessFailure::Cleanup);
        }
        output.x11_conflict = false;
        return Ok(output);
    }
    Err(ProcessFailure::Spawn)
}

#[cfg(target_os = "linux")]
fn xvfb_wrapper_args(
    display: u16,
    error_file: &Path,
    designer: &Path,
    designer_args: &[OsString],
) -> Vec<OsString> {
    let mut args = vec![
        OsString::from("-n"),
        OsString::from(display.to_string()),
        OsString::from("-e"),
        error_file.as_os_str().to_owned(),
        designer.as_os_str().to_owned(),
    ];
    args.extend_from_slice(designer_args);
    args
}

pub(crate) async fn run_bounded(
    executable: &Path,
    args: &[OsString],
    cwd: &Path,
    deadline: Instant,
    cancel: &CancellationToken,
    max_output_bytes: usize,
) -> Result<ProcessOutput, ProcessFailure> {
    run_bounded_with_log(executable, args, cwd, deadline, cancel, max_output_bytes, None, None)
        .await
}

#[cfg(windows)]
fn append_designer_args(command: &mut Command, args: &[OsString]) -> Result<(), ProcessFailure> {
    if args.first().is_some_and(|arg| arg == "CREATEINFOBASE") {
        let [action, connection, startup_messages, startup_dialogs, out, output_path] = args else {
            return Err(ProcessFailure::Spawn);
        };
        let connection = connection.to_str().ok_or(ProcessFailure::Spawn)?;
        let path = connection
            .strip_prefix("File=\"")
            .and_then(|value| value.strip_suffix("\";"))
            .ok_or(ProcessFailure::Spawn)?;
        if !Path::new(path).is_absolute()
            || path.chars().any(|character| character.is_control() || character == '"')
            || action != "CREATEINFOBASE"
            || startup_messages != "/DisableStartupMessages"
            || startup_dialogs != "/DisableStartupDialogs"
            || out != "/Out"
        {
            return Err(ProcessFailure::Spawn);
        }

        // Designer parses this embedded File= quote itself; CRT escaping changes the connection string.
        command.arg(action).raw_arg(connection).args([
            startup_messages,
            startup_dialogs,
            out,
            output_path,
        ]);
    } else {
        command.args(args);
    }
    Ok(())
}

#[allow(
    clippy::too_many_arguments,
    reason = "The bounded child runner takes distinct process, deadline, and log inputs"
)]
async fn run_bounded_with_log(
    executable: &Path,
    args: &[OsString],
    cwd: &Path,
    deadline: Instant,
    cancel: &CancellationToken,
    max_output_bytes: usize,
    monitored_log: Option<(&Path, usize)>,
    x11_launch: Option<(&Path, u16)>,
) -> Result<ProcessOutput, ProcessFailure> {
    let mut command = Command::new(executable);
    command
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // The supervisor owns child termination; Child::drop must not preempt TERM cleanup.
        .kill_on_drop(false);
    #[cfg(windows)]
    append_designer_args(&mut command, args)?;
    #[cfg(not(windows))]
    command.args(args);
    #[cfg(unix)]
    command.env("TMPDIR", cwd);
    #[cfg(target_os = "linux")]
    if let Some((root, _)) = x11_launch {
        command.env("BSL_CHECK_X11_ROOT", root);
    }
    #[cfg(windows)]
    command.env("TEMP", cwd).env("TMP", cwd);
    #[cfg(unix)]
    command.process_group(0);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::System::Threading::CREATE_SUSPENDED;
        command.as_std_mut().creation_flags(CREATE_SUSPENDED);
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;
        let parent_pid = std::process::id() as libc::pid_t;
        // A crashed MCP process must not leave its private compiler child running.
        unsafe {
            command.as_std_mut().pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0
                    || libc::getppid() != parent_pid
                {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }

    #[cfg(windows)]
    let pending_manifest = prepare_active_process_marker(cwd).map_err(|_| ProcessFailure::Spawn)?;
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_) => {
            #[cfg(windows)]
            if std::fs::remove_file(pending_manifest).is_err() {
                return Err(ProcessFailure::Cleanup);
            }
            return Err(ProcessFailure::Spawn);
        }
    };
    let process_id = child.id();
    #[cfg(unix)]
    let mut process_group = ProcessGroupGuard::new(process_id);
    #[cfg(windows)]
    let manifest = match process_id {
        Some(pid) => match write_active_process(cwd, pid) {
            Ok(path) => Some(path),
            Err(_) => {
                return Err(terminate_unadmitted_child(&mut child, cwd, process_id).await);
            }
        },
        None => None,
    };
    #[cfg(windows)]
    let mut process_group = match ProcessGroupGuard::new(&child) {
        Ok(group) => group,
        Err(_) => {
            return Err(terminate_unadmitted_child(&mut child, cwd, process_id).await);
        }
    };
    #[cfg(unix)]
    let manifest = match process_id {
        Some(pid) => Some(write_active_process(cwd, pid).map_err(|_| ProcessFailure::Spawn)?),
        None => None,
    };
    #[cfg(target_os = "linux")]
    let mut x11_tracker = x11_launch.map(|(root, display)| {
        super::x11::Tracker::new(root, cwd, display, process_id.unwrap_or_default())
    });
    #[cfg(not(target_os = "linux"))]
    let mut x11_tracker = None::<()>;
    #[cfg(not(target_os = "linux"))]
    let _ = x11_launch;
    #[cfg(windows)]
    process_group.resume(process_id.ok_or(ProcessFailure::Spawn)?)?;
    let stdout = child.stdout.take().ok_or(ProcessFailure::OutputRead)?;
    let stderr = child.stderr.take().ok_or(ProcessFailure::OutputRead)?;
    let output_overflow = CancellationToken::new();
    let stdout_task =
        tokio::spawn(read_bounded(stdout, max_output_bytes.div_ceil(2), output_overflow.clone()));
    let stderr_task =
        tokio::spawn(read_bounded(stderr, max_output_bytes / 2, output_overflow.clone()));

    let mut x11_tick = tokio::time::interval(std::time::Duration::from_millis(10));
    x11_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut x11_observation_failed = false;
    let termination = loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break Termination::Cancelled,
            _ = sleep_until(tokio::time::Instant::from_std(deadline)) => break Termination::Deadline,
            _ = monitor_log_size(monitored_log) => break Termination::OutputLimit,
            _ = output_overflow.cancelled() => break Termination::OutputLimit,
            _ = x11_tick.tick(), if x11_tracker.is_some() => {
                if observe_x11_tracker(&mut x11_tracker) {
                    x11_observation_failed = true;
                    break Termination::OutputLimit;
                }
            }
            status = child.wait() => {
                break Termination::Completed(status.map_err(|_| ProcessFailure::Wait)?.code());
            }
        }
    };

    let stop_deadline =
        process_group.clock.stop_deadline(!matches!(termination, Termination::Completed(_)));
    if !process_group.stop(&mut child, stop_deadline).await {
        stdout_task.abort();
        stderr_task.abort();
        return Err(ProcessFailure::Cleanup);
    }
    if x11_observation_failed {
        return Err(ProcessFailure::Cleanup);
    }
    #[cfg(target_os = "linux")]
    let x11_conflict =
        finish_x11_tracker(x11_tracker.take()).map_err(|_| ProcessFailure::Cleanup)?;
    #[cfg(not(target_os = "linux"))]
    finish_x11_tracker(x11_tracker.take()).map_err(|_| ProcessFailure::Cleanup)?;
    if let Some(path) = manifest {
        std::fs::remove_file(path).map_err(|_| ProcessFailure::Cleanup)?;
    }
    let mut stdout_task = stdout_task;
    let mut stderr_task = stderr_task;
    let stdout_abort = stdout_task.abort_handle();
    let stderr_abort = stderr_task.abort_handle();
    let readers = async {
        let stdout = join_reader(&mut stdout_task).await?;
        let stderr = join_reader(&mut stderr_task).await?;
        Ok::<_, ProcessFailure>((stdout, stderr))
    };
    tokio::pin!(readers);
    let reader_result = tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            stdout_abort.abort();
            stderr_abort.abort();
            None
        }
        _ = sleep_until(tokio::time::Instant::from_std(deadline)) => {
            stdout_abort.abort();
            stderr_abort.abort();
            None
        }
        result = &mut readers => Some(result?),
    };
    let (mut termination, (stdout, stdout_truncated), (stderr, stderr_truncated)) =
        match reader_result {
            Some(((stdout, stdout_truncated), (stderr, stderr_truncated))) => {
                (termination, (stdout, stdout_truncated), (stderr, stderr_truncated))
            }
            None if cancel.is_cancelled() => {
                (Termination::Cancelled, (Vec::new(), false), (Vec::new(), false))
            }
            None => (Termination::Deadline, (Vec::new(), false), (Vec::new(), false)),
        };
    if matches!(termination, Termination::Completed(_)) && (stdout_truncated || stderr_truncated) {
        termination = Termination::OutputLimit;
    }
    Ok(ProcessOutput {
        termination,
        stdout,
        stderr,
        truncated: stdout_truncated || stderr_truncated,
        #[cfg(target_os = "linux")]
        x11_conflict,
    })
}

#[cfg(windows)]
async fn terminate_unadmitted_child(
    child: &mut tokio::process::Child,
    cwd: &Path,
    pid: Option<u32>,
) -> ProcessFailure {
    let deadline = current_stop_clock().stop_deadline(true);
    if let Some(pid) = pid {
        let _ = write_active_process(cwd, pid);
    }
    let _ = child.start_kill();
    let reaped = tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), child.wait())
        .await
        .is_ok_and(|status| status.is_ok());
    if reaped {
        return ProcessFailure::Spawn;
    }
    ProcessFailure::Cleanup
}

#[cfg(target_os = "linux")]
fn observe_x11_tracker(tracker: &mut Option<super::x11::Tracker>) -> bool {
    tracker.as_mut().is_some_and(|tracker| tracker.observe().is_err())
}

#[cfg(not(target_os = "linux"))]
fn observe_x11_tracker(_tracker: &mut Option<()>) -> bool {
    false
}

#[cfg(target_os = "linux")]
fn finish_x11_tracker(tracker: Option<super::x11::Tracker>) -> io::Result<bool> {
    tracker.map_or(Ok(false), super::x11::Tracker::finish)
}

#[cfg(not(target_os = "linux"))]
fn finish_x11_tracker(_tracker: Option<()>) -> io::Result<bool> {
    Ok(false)
}

#[cfg(unix)]
fn write_active_process(cwd: &Path, pid: u32) -> io::Result<std::path::PathBuf> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let start = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let start_time =
        proc_start_time(&start).ok_or_else(|| io::Error::other("process identity unavailable"))?;
    let path = cwd.join(".native-check-active-pgid");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)?;
    writeln!(file, "{pid}:{start_time}")?;
    file.sync_all()?;
    Ok(path)
}

#[cfg(not(unix))]
fn write_active_process(cwd: &Path, pid: u32) -> io::Result<std::path::PathBuf> {
    #[cfg(windows)]
    {
        use std::io::Write;
        use std::os::windows::fs::MetadataExt;
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT,
        };
        let start = windows_process_start(pid)?;
        let path = cwd.join(".native-check-active-pid");
        let metadata = std::fs::symlink_metadata(&path)?;
        if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "unsafe active marker"));
        }
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(&path)?;
        if file.metadata()?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "unsafe active marker"));
        }
        writeln!(file, "{pid}:{start}")?;
        file.sync_all()?;
        return Ok(path);
    }
    #[cfg(not(windows))]
    {
        let _ = (cwd, pid);
        Ok(std::path::PathBuf::new())
    }
}

#[cfg(windows)]
fn prepare_active_process_marker(cwd: &Path) -> io::Result<std::path::PathBuf> {
    use std::io::Write;
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;

    let path = cwd.join(".native-check-active-pid");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(&path)?;
    file.write_all(b"pending\n")?;
    file.sync_all()?;
    Ok(path)
}

#[cfg(unix)]
fn proc_start_time(stat: &str) -> Option<u64> {
    let close = stat.rfind(')')?;
    let fields: Vec<&str> = stat[close + 1..].split_whitespace().collect();
    fields.get(19)?.parse().ok()
}

async fn monitor_log_size(monitored_log: Option<(&Path, usize)>) {
    let Some((path, limit)) = monitored_log else {
        std::future::pending::<()>().await;
        return;
    };
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        match std::fs::symlink_metadata(path) {
            Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
                if metadata.len() > limit as u64 {
                    return;
                }
            }
            Ok(_) => return,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(_) => return,
        }
    }
}

async fn read_bounded<R: AsyncRead + Unpin>(
    mut reader: R,
    limit: usize,
    overflow: CancellationToken,
) -> io::Result<(Vec<u8>, bool)> {
    let mut output = Vec::with_capacity(limit.min(16 * 1024));
    let mut chunk = [0_u8; 8192];
    let mut truncated = false;
    loop {
        let count = reader.read(&mut chunk).await?;
        if count == 0 {
            return Ok((output, truncated));
        }
        let available = limit.saturating_sub(output.len());
        let keep = count.min(available);
        output.extend_from_slice(&chunk[..keep]);
        truncated |= keep != count;
        if truncated {
            overflow.cancel();
        }
    }
}

async fn join_reader(
    task: &mut JoinHandle<io::Result<(Vec<u8>, bool)>>,
) -> Result<(Vec<u8>, bool), ProcessFailure> {
    task.await.map_err(|_| ProcessFailure::OutputRead)?.map_err(|_| ProcessFailure::OutputRead)
}

pub(crate) fn read_bounded_log(
    path: &Path,
    limit: usize,
) -> Result<(Vec<u8>, bool), ProcessFailure> {
    use std::fs::OpenOptions;
    use std::io::Read;
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;

    let metadata = std::fs::symlink_metadata(path).map_err(|_| ProcessFailure::OutputRead)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(ProcessFailure::OutputRead);
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW);
    let mut file = options.open(path).map_err(|_| ProcessFailure::OutputRead)?;
    let mut output = Vec::with_capacity(limit.min(16 * 1024));
    file.by_ref()
        .take(limit.saturating_add(1) as u64)
        .read_to_end(&mut output)
        .map_err(|_| ProcessFailure::OutputRead)?;
    let truncated = output.len() > limit;
    output.truncate(limit);
    Ok((output, truncated))
}

fn cleanup_budget(grace_ms: u64) -> std::time::Duration {
    let reap_ms = grace_ms.div_ceil(4);
    std::time::Duration::from_millis(grace_ms.saturating_sub(reap_ms))
}

struct ProcessGroupGuard {
    #[cfg(unix)]
    pid: Option<u32>,
    armed: bool,
    clock: StopClock,
    #[cfg(windows)]
    job: windows_sys::Win32::Foundation::HANDLE,
}

// SAFETY: this guard exclusively owns the Windows Job Object handle; Job Object handles have no
// thread affinity, and all ownership transfers remain through this guard or its cleanup worker.
#[cfg(windows)]
unsafe impl Send for ProcessGroupGuard {}

impl ProcessGroupGuard {
    #[cfg(unix)]
    fn new(pid: Option<u32>) -> Self {
        Self { pid, armed: true, clock: current_stop_clock() }
    }

    #[cfg(windows)]
    fn new(child: &tokio::process::Child) -> Result<Self, ProcessFailure> {
        use windows_sys::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
            SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        };
        let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if job.is_null() {
            return Err(ProcessFailure::Spawn);
        }
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let configured = unsafe {
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        } != 0;
        let assigned = configured
            && child
                .raw_handle()
                .is_some_and(|handle| unsafe { AssignProcessToJobObject(job, handle.cast()) != 0 });
        if !assigned {
            unsafe {
                windows_sys::Win32::Foundation::CloseHandle(job);
            }
            return Err(ProcessFailure::Spawn);
        }
        Ok(Self { armed: true, job, clock: current_stop_clock() })
    }

    #[cfg(windows)]
    fn resume(&mut self, pid: u32) -> Result<(), ProcessFailure> {
        use windows_sys::Win32::System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
        };
        use windows_sys::Win32::System::Threading::{
            OpenThread, ResumeThread, THREAD_SUSPEND_RESUME,
        };
        unsafe {
            let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
            if snapshot.is_null() || snapshot as isize == -1 {
                return Err(ProcessFailure::Spawn);
            }
            let mut entry = THREADENTRY32::default();
            entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
            let mut thread_id = None;
            let mut found = Thread32First(snapshot, &mut entry) != 0;
            while found {
                if entry.th32OwnerProcessID == pid {
                    if thread_id.replace(entry.th32ThreadID).is_some() {
                        windows_sys::Win32::Foundation::CloseHandle(snapshot);
                        return Err(ProcessFailure::Spawn);
                    }
                }
                found = Thread32Next(snapshot, &mut entry) != 0;
            }
            windows_sys::Win32::Foundation::CloseHandle(snapshot);
            let thread_id = thread_id.ok_or(ProcessFailure::Spawn)?;
            let thread = OpenThread(THREAD_SUSPEND_RESUME, 0, thread_id);
            if thread.is_null() {
                return Err(ProcessFailure::Spawn);
            }
            let resumed = ResumeThread(thread) != u32::MAX;
            windows_sys::Win32::Foundation::CloseHandle(thread);
            if resumed {
                Ok(())
            } else {
                Err(ProcessFailure::Spawn)
            }
        }
    }

    #[cfg(unix)]
    async fn stop(&mut self, child: &mut tokio::process::Child, deadline: Instant) -> bool {
        let Some(pid) = self.pid else {
            return child.try_wait().is_ok_and(|status| status.is_some());
        };
        let Ok(group) = libc::pid_t::try_from(pid) else { return false };
        let started = tokio::time::Instant::now();
        let end = tokio::time::Instant::from_std(deadline);
        let budget = end.saturating_duration_since(started);
        let soft_until = started + budget / 2;
        unsafe {
            libc::kill(-group, libc::SIGTERM);
        }
        let mut status_seen = child.try_wait().ok().flatten().is_some();
        while group_exists(group) && tokio::time::Instant::now() < soft_until {
            if !status_seen {
                status_seen = child.try_wait().ok().flatten().is_some();
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        if group_exists(group) {
            unsafe {
                libc::kill(-group, libc::SIGKILL);
            }
        }
        while tokio::time::Instant::now() < end {
            if !status_seen {
                status_seen = child.try_wait().ok().flatten().is_some();
            }
            if status_seen && !group_exists(group) {
                self.armed = false;
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        false
    }

    #[cfg(not(unix))]
    async fn stop(&mut self, child: &mut tokio::process::Child, deadline: Instant) -> bool {
        #[cfg(windows)]
        {
            let started = tokio::time::Instant::now();
            let end = tokio::time::Instant::from_std(deadline);
            let budget = end.saturating_duration_since(started);
            let soft_until = started + budget / 2;
            while windows_job_active(self.job) == Some(true)
                && tokio::time::Instant::now() < soft_until
            {
                if child.try_wait().is_err() {
                    return false;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            if windows_job_active(self.job).is_none() {
                return false;
            }
            if windows_job_active(self.job) == Some(true) {
                unsafe {
                    windows_sys::Win32::System::JobObjects::TerminateJobObject(self.job, 1);
                }
            }
            while tokio::time::Instant::now() < end {
                let reaped = child.try_wait().ok().flatten().is_some();
                if reaped && windows_job_active(self.job) == Some(false) {
                    self.close();
                    return true;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            false
        }
        #[cfg(not(windows))]
        {
            let _ = deadline;
            child.kill().await.is_ok() && child.wait().await.is_ok()
        }
    }

    fn terminate(&mut self) {
        if self.armed {
            #[cfg(unix)]
            if let Some(pid) = self.pid {
                schedule_process_group_cleanup(pid, self.clock.stop_deadline(true));
            }
            #[cfg(windows)]
            {
                if !self.job.is_null() {
                    schedule_windows_job_cleanup(self.job, self.clock.stop_deadline(true));
                    self.job = std::ptr::null_mut();
                }
            }
            self.armed = false;
        }
    }

    #[cfg(windows)]
    fn close(&mut self) {
        if !self.job.is_null() {
            unsafe {
                windows_sys::Win32::Foundation::CloseHandle(self.job);
            }
            self.job = std::ptr::null_mut();
        }
        self.armed = false;
    }
}

#[cfg(unix)]
fn group_exists(group: libc::pid_t) -> bool {
    #[cfg(target_os = "linux")]
    {
        if let Some(exists) = linux_group_has_live_member(group) {
            return exists;
        }
    }
    let result = unsafe { libc::kill(-group, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

#[cfg(target_os = "linux")]
fn linux_group_has_live_member(group: libc::pid_t) -> Option<bool> {
    let entries = std::fs::read_dir("/proc").ok()?;
    for entry in entries {
        let entry = entry.ok()?;
        if !entry.file_name().to_str().is_some_and(|name| name.bytes().all(|b| b.is_ascii_digit()))
        {
            continue;
        }
        let stat = match std::fs::read_to_string(entry.path().join("stat")) {
            Ok(stat) => stat,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(_) => return None,
        };
        let (state, process_group) = linux_process_identity(&stat)?;
        if process_group == group && state != b'Z' && state != b'X' {
            return Some(true);
        }
    }
    Some(false)
}

#[cfg(target_os = "linux")]
fn linux_process_identity(stat: &str) -> Option<(u8, libc::pid_t)> {
    let close = stat.rfind(')')?;
    let fields: Vec<&str> = stat[close + 1..].split_whitespace().collect();
    Some((fields.first()?.as_bytes().first().copied()?, fields.get(2)?.parse().ok()?))
}

#[cfg(windows)]
fn windows_process_start(pid: u32) -> io::Result<u64> {
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if process.is_null() {
            return Err(io::Error::last_os_error());
        }
        let mut created = std::mem::zeroed();
        let mut exited = std::mem::zeroed();
        let mut kernel = std::mem::zeroed();
        let mut user = std::mem::zeroed();
        let ok = GetProcessTimes(process, &mut created, &mut exited, &mut kernel, &mut user) != 0;
        windows_sys::Win32::Foundation::CloseHandle(process);
        if !ok {
            return Err(io::Error::last_os_error());
        }
        Ok(((created.dwHighDateTime as u64) << 32) | created.dwLowDateTime as u64)
    }
}

#[cfg(windows)]
fn windows_job_active(job: windows_sys::Win32::Foundation::HANDLE) -> Option<bool> {
    use windows_sys::Win32::System::JobObjects::{
        JobObjectBasicAccountingInformation, QueryInformationJobObject,
        JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
    };
    unsafe {
        let mut info = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
        let queried = QueryInformationJobObject(
            job,
            JobObjectBasicAccountingInformation,
            (&mut info as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
            std::mem::size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
            std::ptr::null_mut(),
        ) != 0;
        queried.then_some(info.ActiveProcesses != 0)
    }
}

#[cfg(windows)]
fn schedule_windows_job_cleanup(job: windows_sys::Win32::Foundation::HANDLE, deadline: Instant) {
    use windows_sys::Win32::{Foundation::CloseHandle, System::JobObjects::TerminateJobObject};

    let raw_job = job as usize;
    let cleanup = move || {
        let job = raw_job as windows_sys::Win32::Foundation::HANDLE;
        let started = Instant::now();
        let soft_until = started + deadline.saturating_duration_since(started) / 2;
        while Instant::now() < soft_until && windows_job_active(job) == Some(true) {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        if Instant::now() < deadline && windows_job_active(job) != Some(false) {
            unsafe { TerminateJobObject(job, 1) };
        }
        while Instant::now() < deadline && windows_job_active(job) != Some(false) {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        // Closing the kill-on-close Job is the final containment fallback.
        unsafe { CloseHandle(job) };
    };
    if std::thread::Builder::new().name("native-check-stop".to_owned()).spawn(cleanup).is_err() {
        unsafe {
            TerminateJobObject(job, 1);
            CloseHandle(job);
        }
    }
}

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        self.terminate();
    }
}

#[cfg(unix)]
fn schedule_process_group_cleanup(pid: u32, deadline: Instant) {
    let Ok(pgid) = libc::pid_t::try_from(pid) else { return };
    unsafe {
        libc::kill(-pgid, libc::SIGTERM);
    }
    let cleanup = move || {
        let started = std::time::Instant::now();
        let budget = deadline.saturating_duration_since(started);
        let soft_until = started + budget / 2;
        while std::time::Instant::now() < soft_until && group_exists(pgid) {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        if group_exists(pgid) {
            unsafe {
                libc::kill(-pgid, libc::SIGKILL);
            }
        }
        while std::time::Instant::now() < deadline && group_exists(pgid) {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    };
    if std::thread::Builder::new().name("native-check-stop".to_owned()).spawn(cleanup).is_err() {
        unsafe {
            libc::kill(-pgid, libc::SIGKILL);
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;

    #[test]
    fn cleanup_budget_reserves_the_final_grace_quarter_for_reaping() {
        assert_eq!(cleanup_budget(2000), Duration::from_millis(1500));
        assert_eq!(cleanup_budget(1000), Duration::from_millis(750));
        assert_eq!(cleanup_budget(500), Duration::from_millis(375));
    }

    #[test]
    fn repeated_stop_observations_keep_the_original_cleanup_deadline() {
        let clock = StopClock::new();
        let first = clock.mark();
        std::thread::sleep(Duration::from_millis(2));
        assert_eq!(clock.mark(), first);
        assert_eq!(clock.cleanup_deadline(), first + clock.budget);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_proc_identity_distinguishes_zombies_from_live_group_members() {
        assert_eq!(
            linux_process_identity("123 (a tricky ) name) Z 1 42 42 0 0 0"),
            Some((b'Z', 42))
        );
        assert_eq!(linux_process_identity("123 (worker) S 1 42 42 0 0 0"), Some((b'S', 42)));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn xvfb_uses_a_bounded_known_display_number_without_auto_allocation() {
        assert_eq!(super::super::x11::next_display(42, 0), 142);
        assert_eq!(super::super::x11::next_display(999, 1), 200);
        let args = xvfb_wrapper_args(
            142,
            Path::new("/private/xvfb.err"),
            Path::new("/platform/1cv8"),
            &["DESIGNER".into()],
        );
        let args =
            args.iter().map(|value| value.to_string_lossy().into_owned()).collect::<Vec<_>>();
        assert_eq!(args, ["-n", "142", "-e", "/private/xvfb.err", "/platform/1cv8", "DESIGNER"]);
        assert!(!args.iter().any(|argument| argument == "-a"));
    }

    fn script(directory: &Path, body: &str) -> std::path::PathBuf {
        let path = directory.join("designer-stub");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    #[cfg(target_os = "linux")]
    fn python_script(directory: &Path, name: &str, body: &str) -> std::path::PathBuf {
        let path = directory.join(name);
        std::fs::write(&path, format!("#!/usr/bin/python3\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    #[cfg(target_os = "linux")]
    fn short_test_root() -> tempfile::TempDir {
        tempfile::Builder::new().prefix("x").tempdir().unwrap()
    }

    #[tokio::test]
    async fn child_output_is_drained_but_kept_within_the_limit() {
        let directory = tempfile::tempdir().unwrap();
        let executable = script(directory.path(), "printf error >&2; printf 1234567890");
        let output = run_bounded(
            &executable,
            &[],
            directory.path(),
            Instant::now() + Duration::from_secs(5),
            &CancellationToken::new(),
            8,
        )
        .await
        .unwrap();

        assert_eq!(output.termination, Termination::OutputLimit);
        assert_eq!(output.stdout, b"1234");
        assert_eq!(output.stderr, b"erro");
        assert!(output.truncated);
    }

    #[tokio::test]
    async fn chatty_child_is_stopped_when_bounded_output_overflows() {
        let directory = tempfile::tempdir().unwrap();
        let executable =
            script(directory.path(), "trap '' TERM; while :; do printf 1234567890; done");
        let started = tokio::time::Instant::now();
        let output = run_bounded(
            &executable,
            &[],
            directory.path(),
            Instant::now() + Duration::from_secs(5),
            &CancellationToken::new(),
            1024,
        )
        .await
        .unwrap();

        assert_eq!(output.termination, Termination::OutputLimit);
        assert!(output.truncated);
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[tokio::test]
    async fn native_log_growth_over_the_limit_stops_the_owned_process() {
        let directory = tempfile::tempdir().unwrap();
        let executable = script(
            directory.path(),
            "while :; do printf 1234567890 > native.log; sleep 0.01; done",
        );
        let log = directory.path().join("native.log");
        let output = run_bounded_with_log(
            &executable,
            &[],
            directory.path(),
            Instant::now() + Duration::from_secs(5),
            &CancellationToken::new(),
            1024,
            Some((&log, 4)),
            None,
        )
        .await
        .unwrap();
        assert_eq!(output.termination, Termination::OutputLimit);
        let (bytes, truncated) = read_bounded_log(&log, 4).unwrap();
        assert_eq!(bytes, b"1234");
        assert!(truncated);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn xvfb_fake_uses_private_roots_and_cleans_its_owned_listener_and_lock() {
        let temp = short_test_root();
        let root = temp.path().join("x11");
        let cwd = temp.path().join("job");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(root.join(".X11-unix")).unwrap();
        std::fs::create_dir(&cwd).unwrap();
        let marker = cwd.join("tmpdir-value");
        let designer =
            script(&cwd, &format!("sleep 0.1; printf '%s' \"$TMPDIR\" > '{}'", marker.display()));
        let wrapper = python_script(
            &cwd,
            "xvfb-run-fake",
            "import os,socket,subprocess,sys\na=sys.argv[1:]\nd=int(a[a.index('-n')+1]); e=a[a.index('-e')+1]; cmd=a[a.index('-e')+2:]\nr=os.environ['BSL_CHECK_X11_ROOT']; lock=f'{r}/.X{d}-lock'; sock=f'{r}/.X11-unix/X{d}'\nopen(lock,'w').write(str(os.getpid())+'\\n')\ns=socket.socket(socket.AF_UNIX); s.bind(sock); s.listen()\ntry:\n p=subprocess.run(cmd,check=False); sys.exit(p.returncode)\nfinally:\n s.close(); os.unlink(sock); os.unlink(lock)",
        );
        let profile = NativeProfile {
            designer_path: designer,
            python_path: "/usr/bin/python3".into(),
            xvfb_run_path: Some(wrapper.clone()),
            expected_build: "test-build".into(),
            work_root: temp.path().to_owned(),
            source_kind: super::super::profile::SourceKind::File,
            source_connection_env: "BSL_SOURCE".into(),
            runtime_url_env: None,
            user_env: None,
            password_env: None,
            max_input_bytes: 1024,
            deadline_ms: 5_000,
            max_log_bytes: 4096,
        };
        let log = cwd.join("native.log");
        let args = [OsString::from("DESIGNER"), OsString::from("/Out"), log.as_os_str().to_owned()];
        let output = run_designer_xvfb(
            &wrapper,
            &profile,
            &args,
            &cwd,
            &log,
            Instant::now() + Duration::from_secs(5),
            &CancellationToken::new(),
            4096,
            &root,
        )
        .await
        .unwrap();
        assert_eq!(output.termination, Termination::Completed(Some(0)));
        assert_eq!(std::fs::read_to_string(marker).unwrap(), cwd.to_string_lossy());
        let mut files = std::fs::read_dir(root.join(".X11-unix")).unwrap();
        assert!(files.next().is_none());
        assert!(std::fs::read_dir(&root).unwrap().all(|entry| {
            entry.unwrap().file_name().to_string_lossy().starts_with(".X11-unix")
        }));
        assert!(!cwd.join(".native-check-x11-*.owner").exists());
        assert!(std::fs::read_dir(&cwd).unwrap().all(|entry| {
            !entry.unwrap().file_name().to_string_lossy().starts_with(".native-check-xvfb-")
        }));
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn xvfb_retries_only_the_documented_startup_conflict() {
        let temp = short_test_root();
        let root = temp.path().join("x11");
        let cwd = temp.path().join("job");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(root.join(".X11-unix")).unwrap();
        std::fs::create_dir(&cwd).unwrap();
        let counter = cwd.join("attempts");
        let designer = script(&cwd, "sleep 0.1; exit 0");
        let wrapper = python_script(
            &cwd,
            "xvfb-run-fake",
            &format!(
                "import os,socket,subprocess,sys\na=sys.argv[1:]\nd=int(a[a.index('-n')+1]); e=a[a.index('-e')+1]; cmd=a[a.index('-e')+2:]\np='{}'; n=int(open(p).read())+1 if os.path.exists(p) else 1; open(p,'w').write(str(n))\nif n == 1:\n open(e,'w').write(f'Fatal server error: Server is already active for display {{d}}\\n'); sys.exit(1)\nr=os.environ['BSL_CHECK_X11_ROOT']; lock=f'{{r}}/.X{{d}}-lock'; sock=f'{{r}}/.X11-unix/X{{d}}'; open(lock,'w').write(str(os.getpid())+'\\n')\ns=socket.socket(socket.AF_UNIX); s.bind(sock); s.listen()\ntry:\n sys.exit(subprocess.run(cmd,check=False).returncode)\nfinally:\n s.close(); os.unlink(sock); os.unlink(lock)",
                counter.display()
            ),
        );
        let profile = NativeProfile {
            designer_path: designer,
            python_path: "/usr/bin/python3".into(),
            xvfb_run_path: Some(wrapper.clone()),
            expected_build: "test-build".into(),
            work_root: temp.path().to_owned(),
            source_kind: super::super::profile::SourceKind::File,
            source_connection_env: "BSL_SOURCE".into(),
            runtime_url_env: None,
            user_env: None,
            password_env: None,
            max_input_bytes: 1024,
            deadline_ms: 5_000,
            max_log_bytes: 4096,
        };
        let log = cwd.join("native.log");
        let args = [OsString::from("DESIGNER"), OsString::from("/Out"), log.as_os_str().to_owned()];
        let output = run_designer_xvfb(
            &wrapper,
            &profile,
            &args,
            &cwd,
            &log,
            Instant::now() + Duration::from_secs(5),
            &CancellationToken::new(),
            4096,
            &root,
        )
        .await
        .unwrap();
        assert_eq!(output.termination, Termination::Completed(Some(0)));
        assert_eq!(std::fs::read_to_string(counter).unwrap(), "2");
        assert!(std::fs::read_dir(root.join(".X11-unix")).unwrap().next().is_none());
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn xvfb_cancel_kills_ignored_term_group_then_recovers_only_its_x11_paths() {
        let temp = short_test_root();
        let root = temp.path().join("x11");
        let cwd = temp.path().join("job");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(root.join(".X11-unix")).unwrap();
        std::fs::create_dir(&cwd).unwrap();
        let designer = script(&cwd, "trap '' TERM; while :; do sleep 1; done");
        let wrapper = python_script(
            &cwd,
            "xvfb-run-fake",
            "import os,socket,subprocess,sys\na=sys.argv[1:]; d=int(a[a.index('-n')+1]); cmd=a[a.index('-e')+2:]\nr=os.environ['BSL_CHECK_X11_ROOT']; lock=f'{r}/.X{d}-lock'; sock=f'{r}/.X11-unix/X{d}'; open(lock,'w').write(str(os.getpid())+'\\n')\ns=socket.socket(socket.AF_UNIX); s.bind(sock); s.listen(); p=subprocess.Popen(cmd); open('designer.pid','w').write(str(p.pid)); sys.exit(p.wait())",
        );
        let profile = NativeProfile {
            designer_path: designer,
            python_path: "/usr/bin/python3".into(),
            xvfb_run_path: Some(wrapper.clone()),
            expected_build: "test-build".into(),
            work_root: temp.path().to_owned(),
            source_kind: super::super::profile::SourceKind::File,
            source_connection_env: "BSL_SOURCE".into(),
            runtime_url_env: None,
            user_env: None,
            password_env: None,
            max_input_bytes: 1024,
            deadline_ms: 5_000,
            max_log_bytes: 4096,
        };
        let log = cwd.join("native.log");
        let args = [OsString::from("DESIGNER"), OsString::from("/Out"), log.as_os_str().to_owned()];
        let cancel = CancellationToken::new();
        let child_cancel = cancel.clone();
        let root_for_task = root.clone();
        let cwd_for_task = cwd.clone();
        let task = tokio::spawn(async move {
            run_designer_xvfb(
                &wrapper,
                &profile,
                &args,
                &cwd_for_task,
                &log,
                Instant::now() + Duration::from_secs(5),
                &child_cancel,
                4096,
                &root_for_task,
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let designer_started = cwd.join("designer.pid").is_file();
                let ownership_recorded = std::fs::read_dir(&cwd).is_ok_and(|entries| {
                    entries.filter_map(Result::ok).any(|entry| {
                        entry.file_name().to_string_lossy().starts_with(".native-check-x11-")
                    })
                });
                if designer_started && ownership_recorded {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("fake Designer must start before cancellation");
        let designer_pid: u32 =
            std::fs::read_to_string(cwd.join("designer.pid")).unwrap().parse().unwrap();
        cancel.cancel();
        let output = task.await.unwrap().unwrap();
        assert_eq!(output.termination, Termination::Cancelled);
        let designer_stat = std::fs::read_to_string(format!("/proc/{designer_pid}/stat"));
        assert!(
            match designer_stat {
                Ok(stat) => linux_process_identity(&stat)
                    .is_some_and(|(state, _)| state == b'Z' || state == b'X'),
                Err(error) => error.kind() == io::ErrorKind::NotFound,
            },
            "fake Designer must be gone or a non-running zombie after cleanup"
        );
        assert!(std::fs::read_dir(root.join(".X11-unix")).unwrap().next().is_none());
        assert!(std::fs::read_dir(&root).unwrap().all(|entry| {
            entry.unwrap().file_name().to_string_lossy().starts_with(".X11-unix")
        }));
        assert!(std::fs::read_dir(&cwd).unwrap().all(|entry| {
            !entry.unwrap().file_name().to_string_lossy().starts_with(".native-check-x11-")
        }));
    }

    #[tokio::test]
    async fn cancellation_kills_the_owned_process_group() {
        let directory = tempfile::tempdir().unwrap();
        let executable = script(directory.path(), "while :; do sleep 1; done");
        let cancel = CancellationToken::new();
        let child_cancel = cancel.clone();
        let executable_clone = executable.clone();
        let cwd = directory.path().to_owned();
        let task = tokio::spawn(async move {
            run_bounded(
                &executable_clone,
                &[],
                &cwd,
                Instant::now() + Duration::from_secs(10),
                &child_cancel,
                1024,
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(30)).await;
        cancel.cancel();

        let output = task.await.unwrap().unwrap();
        assert_eq!(output.termination, Termination::Cancelled);
    }

    #[tokio::test]
    async fn ignored_term_is_killed_after_the_soft_window() {
        let directory = tempfile::tempdir().unwrap();
        let executable = script(directory.path(), "trap '' TERM; while :; do sleep 1; done");
        let cancel = CancellationToken::new();
        let child_cancel = cancel.clone();
        let executable_clone = executable.clone();
        let cwd = directory.path().to_owned();
        let task = tokio::spawn(async move {
            run_bounded(
                &executable_clone,
                &[],
                &cwd,
                Instant::now() + Duration::from_secs(10),
                &child_cancel,
                1024,
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        let stopped_at = tokio::time::Instant::now();
        cancel.cancel();
        let output = task.await.unwrap().unwrap();
        let elapsed = stopped_at.elapsed();
        assert_eq!(output.termination, Termination::Cancelled);
        assert!(elapsed >= Duration::from_millis(600), "TERM was skipped: {elapsed:?}");
        assert!(elapsed < Duration::from_millis(1400), "cleanup exceeded B: {elapsed:?}");
        assert!(!directory.path().join(".native-check-active-pgid").exists());
    }

    #[tokio::test]
    async fn active_manifest_write_failure_kills_spawned_process_group() {
        let directory = tempfile::tempdir().unwrap();
        let sentinel = directory.path().join("descendant-survived");
        let ready = directory.path().join("descendant-ready");
        let executable = script(
            directory.path(),
            &format!(
                "(/bin/sleep 0.2; /usr/bin/touch '{}') &\nprintf ready > '{}'\nwait",
                sentinel.display(),
                ready.display()
            ),
        );
        std::fs::write(directory.path().join(".native-check-active-pgid"), b"occupied\n").unwrap();
        let result = run_bounded(
            &executable,
            &[],
            directory.path(),
            Instant::now() + Duration::from_secs(5),
            &CancellationToken::new(),
            1024,
        )
        .await;
        assert!(matches!(result, Err(ProcessFailure::Spawn)));
        tokio::time::sleep(Duration::from_millis(350)).await;
        assert!(!sentinel.exists(), "early return left a child in the private process group");
    }

    #[tokio::test]
    async fn dropping_process_supervision_future_allows_term_cleanup_before_hard_stop() {
        let directory = tempfile::tempdir().unwrap();
        let sentinel = directory.path().join("term-cleanup-ran");
        let ready = directory.path().join("descendant-ready");
        let executable = script(
            directory.path(),
            &format!(
                "trap \"/usr/bin/touch '{}'; exit 0\" TERM\nprintf ready > '{}'\nwhile :; do sleep 10 & wait \"$!\"; done",
                sentinel.display(),
                ready.display()
            ),
        );
        let executable_for_task = executable.clone();
        let cwd = directory.path().to_owned();
        let task = tokio::spawn(async move {
            run_bounded(
                &executable_for_task,
                &[],
                &cwd,
                Instant::now() + Duration::from_secs(5),
                &CancellationToken::new(),
                1024,
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(3), async {
            while !ready.is_file() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        task.abort();
        let _ = task.await;
        tokio::time::timeout(Duration::from_secs(1), async {
            while !sentinel.exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("TERM-cooperative child was killed before it could clean up");
    }

    #[test]
    fn native_issue_parser_keeps_positions_and_drops_source_literals() {
        let parsed = parse_diagnostics(
            "{ОбщаяФорма.Форма.Форма(6,5)}: Keyword EndFunction expected\n".as_bytes(),
        )
        .unwrap();
        assert_eq!(parsed.issues.len(), 1);
        assert_eq!(parsed.issues[0].owner, "ОбщаяФорма.Форма.Форма");
        assert_eq!((parsed.issues[0].line, parsed.issues[0].column), (6, 5));
        assert_eq!(parsed.issues[0].message, "Keyword EndFunction expected");

        let parsed =
            parse_diagnostics(b"{CommonModule.Test.Module(1,1)}: Bad value \"PRIVATE_MARKER\"")
                .unwrap();
        assert_eq!(parsed.issues[0].message, "Bad value <literal>");
        assert!(!parsed.issues[0].message.contains("PRIVATE_MARKER"));
    }

    #[test]
    fn malformed_or_non_utf8_diagnostic_output_is_not_interpreted_as_success() {
        assert_eq!(
            parse_diagnostics(b"{Module(0,1)}: invalid"),
            Err(DiagnosticParseError::MalformedPosition)
        );
        assert_eq!(parse_diagnostics(&[0xff]), Err(DiagnosticParseError::InvalidUtf8));
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::Duration;

    async fn echoed_arguments(args: &[OsString]) -> String {
        let system_root = PathBuf::from(std::env::var_os("SystemRoot").unwrap());
        let cmd = system_root.join("System32").join("cmd.exe");
        let mut command = Command::new(cmd);
        command.args(["/d", "/u", "/c", "echo"]);
        append_designer_args(&mut command, args).unwrap();
        command.kill_on_drop(true);
        let output = tokio::time::timeout(Duration::from_secs(5), command.output())
            .await
            .expect("cmd echo should finish promptly")
            .expect("cmd.exe should start");
        assert!(output.status.success());
        let units =
            output.stdout.chunks_exact(2).map(|bytes| u16::from_le_bytes([bytes[0], bytes[1]]));
        let mut text = String::from_utf16_lossy(&units.collect::<Vec<_>>());
        text = text.trim_start_matches('\u{feff}').trim_end_matches(['\r', '\n']).to_owned();
        text
    }

    #[tokio::test]
    async fn create_infobase_raw_file_argument_preserves_only_the_trusted_token() {
        use super::super::commands::create_file_infobase_args;

        let output = Path::new(r"C:\bsl-check-executor-test\job logs\create.log");
        for target in [
            Path::new(r"C:\bsl-check-executor-test\ib"),
            Path::new(r"C:\bsl-check-executor-test\ib with space Ω;"),
        ] {
            let mut args = create_file_infobase_args(target).unwrap();
            args.extend([OsString::from("/Out"), output.as_os_str().to_owned()]);
            let expected = format!(
                "CREATEINFOBASE File=\"{}\"; /DisableStartupMessages /DisableStartupDialogs /Out \"{}\"",
                target.display(), output.display()
            );
            assert_eq!(echoed_arguments(&args).await, expected);
        }

        let mut valid =
            create_file_infobase_args(Path::new(r"C:\bsl-check-executor-test\ib")).unwrap();
        valid.extend([OsString::from("/Out"), output.as_os_str().to_owned()]);
        let mut relative = valid.clone();
        relative[1] = OsString::from(r#"File="relative";"#);
        let mut embedded_quote = valid.clone();
        embedded_quote[1] = OsString::from(r#"File="C:\bsl-check-executor-test\bad"quote";"#);
        let mut control = valid.clone();
        control[1] = OsString::from("File=\"C:\\bsl-check-executor-test\\bad\npath\";");
        let mut wrong_flag = valid.clone();
        wrong_flag[2] = OsString::from("/OtherStartupMode");
        let mut malformed_shape = valid.clone();
        malformed_shape.pop();
        for invalid in [relative, embedded_quote, control, wrong_flag, malformed_shape] {
            let mut command = Command::new("cmd.exe");
            assert_eq!(append_designer_args(&mut command, &invalid), Err(ProcessFailure::Spawn));
        }

        let designer_args = [
            OsString::from("DESIGNER"),
            OsString::from("/F"),
            OsString::from(r"C:\bsl-check-executor-test\source with space\ib"),
            OsString::from("/DumpDBCfg"),
            output.as_os_str().to_owned(),
        ];
        assert_eq!(
            echoed_arguments(&designer_args).await,
            format!(
                "DESIGNER /F \"C:\\bsl-check-executor-test\\source with space\\ib\" /DumpDBCfg \"{}\"",
                output.display()
            )
        );

        let python_args = [
            OsString::from("python.exe"),
            OsString::from(r"C:\bsl-check-executor-test\helper with space.py"),
        ];
        assert_eq!(
            echoed_arguments(&python_args).await,
            "python.exe \"C:\\bsl-check-executor-test\\helper with space.py\""
        );
    }
}
