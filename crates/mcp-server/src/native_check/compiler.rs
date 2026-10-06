use std::ffi::OsString;
use std::io;
use std::path::Path;
use std::process::Stdio;
use std::time::Instant;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::task::JoinHandle;
use tokio::time::sleep_until;
use tokio_util::sync::CancellationToken;

use super::NativeProfile;

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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProcessFailure {
    Spawn,
    Wait,
    OutputRead,
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
    use std::fs::OpenOptions;
    use std::io::Write;

    let mut log = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(log_path)
        .map_err(|_| ProcessFailure::Spawn)?;
    log.flush().map_err(|_| ProcessFailure::Spawn)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(log_path, std::fs::Permissions::from_mode(0o600))
            .map_err(|_| ProcessFailure::Spawn)?;
    }

    let mut designer_args = args.to_vec();
    designer_args.push(OsString::from("/Out"));
    designer_args.push(log_path.as_os_str().to_owned());
    if let Some(wrapper) = &profile.xvfb_run_path {
        let mut wrapper_args =
            vec![OsString::from("-a"), profile.designer_path.as_os_str().to_owned()];
        wrapper_args.extend(designer_args);
        run_bounded_with_log(
            wrapper,
            &wrapper_args,
            cwd,
            deadline,
            cancel,
            max_output_bytes,
            Some((log_path, max_output_bytes)),
            None,
        )
        .await
    } else {
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

pub(crate) async fn run_bounded_isolated(
    executable: &Path,
    args: &[OsString],
    cwd: &Path,
    deadline: Instant,
    cancel: &CancellationToken,
    max_output_bytes: usize,
    env: &[(OsString, OsString)],
) -> Result<ProcessOutput, ProcessFailure> {
    run_bounded_with_log(executable, args, cwd, deadline, cancel, max_output_bytes, None, Some(env))
        .await
}

#[allow(
    clippy::too_many_arguments,
    reason = "The bounded child runner takes distinct process, deadline, log, and environment inputs"
)]
async fn run_bounded_with_log(
    executable: &Path,
    args: &[OsString],
    cwd: &Path,
    deadline: Instant,
    cancel: &CancellationToken,
    max_output_bytes: usize,
    monitored_log: Option<(&Path, usize)>,
    isolated_env: Option<&[(OsString, OsString)]>,
) -> Result<ProcessOutput, ProcessFailure> {
    let mut command = Command::new(executable);
    if let Some(env) = isolated_env {
        command.env_clear().envs(env.iter().cloned());
    }
    command
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
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

    let mut child = command.spawn().map_err(|_| ProcessFailure::Spawn)?;
    let process_id = child.id();
    let mut process_group = ProcessGroupGuard::new(process_id);
    let manifest = match process_id {
        Some(pid) => Some(write_active_process(cwd, pid).map_err(|_| ProcessFailure::Spawn)?),
        None => None,
    };
    let stdout = child.stdout.take().ok_or(ProcessFailure::OutputRead)?;
    let stderr = child.stderr.take().ok_or(ProcessFailure::OutputRead)?;
    let stdout_task = tokio::spawn(read_bounded(stdout, max_output_bytes.div_ceil(2)));
    let stderr_task = tokio::spawn(read_bounded(stderr, max_output_bytes / 2));

    let termination = tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            terminate(&mut child).await;
            Termination::Cancelled
        }
        _ = sleep_until(tokio::time::Instant::from_std(deadline)) => {
            terminate(&mut child).await;
            Termination::Deadline
        }
        _ = monitor_log_size(monitored_log) => {
            terminate(&mut child).await;
            Termination::OutputLimit
        }
        status = child.wait() => {
            Termination::Completed(status.map_err(|_| ProcessFailure::Wait)?.code())
        }
    };

    process_group.terminate();
    if let Some(path) = manifest {
        let _ = std::fs::remove_file(path);
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
    let (termination, (stdout, stdout_truncated), (stderr, stderr_truncated)) = match reader_result
    {
        Some(((stdout, stdout_truncated), (stderr, stderr_truncated))) => {
            (termination, (stdout, stdout_truncated), (stderr, stderr_truncated))
        }
        None if cancel.is_cancelled() => {
            (Termination::Cancelled, (Vec::new(), false), (Vec::new(), false))
        }
        None => (Termination::Deadline, (Vec::new(), false), (Vec::new(), false)),
    };
    Ok(ProcessOutput {
        termination,
        stdout,
        stderr,
        truncated: stdout_truncated || stderr_truncated,
    })
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
fn write_active_process(_cwd: &Path, _pid: u32) -> io::Result<std::path::PathBuf> {
    Ok(std::path::PathBuf::new())
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

async fn terminate(child: &mut tokio::process::Child) {
    let _ = child.kill().await;
}

struct ProcessGroupGuard {
    pid: Option<u32>,
    armed: bool,
}

impl ProcessGroupGuard {
    fn new(pid: Option<u32>) -> Self {
        Self { pid, armed: true }
    }

    fn terminate(&mut self) {
        if self.armed {
            if let Some(pid) = self.pid {
                terminate_process_group(pid);
            }
            self.armed = false;
        }
    }
}

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        self.terminate();
    }
}

#[cfg(unix)]
fn terminate_process_group(pid: u32) {
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
}

#[cfg(not(unix))]
fn terminate_process_group(_pid: u32) {}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;

    fn script(directory: &Path, body: &str) -> std::path::PathBuf {
        let path = directory.join("designer-stub");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    #[tokio::test]
    async fn child_output_is_drained_but_kept_within_the_limit() {
        let directory = tempfile::tempdir().unwrap();
        let executable = script(directory.path(), "printf 1234567890; printf error >&2");
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

        assert_eq!(output.termination, Termination::Completed(Some(0)));
        assert_eq!(output.stdout, b"1234");
        assert_eq!(output.stderr, b"erro");
        assert!(output.truncated);
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
    async fn dropping_process_supervision_future_kills_spawned_process_group() {
        let directory = tempfile::tempdir().unwrap();
        let sentinel = directory.path().join("descendant-survived");
        let ready = directory.path().join("descendant-ready");
        let executable = script(
            directory.path(),
            &format!(
                "(/bin/sleep 0.25; /usr/bin/touch '{}') &\nprintf ready > '{}'\nwait",
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
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(
            !sentinel.exists(),
            "aborting supervision left a child in the private process group"
        );
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
