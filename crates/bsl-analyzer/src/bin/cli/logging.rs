use std::{
    env, fs, io,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

mod bounded;
mod journal;
pub use journal::JournalGuard;

pub fn activate_vector_journal(source: Option<&std::path::Path>) {
    journal::activate(source);
}

/// Placeholder in `BSL_LOG_FILE` that is replaced with the current process id.
pub const PID_PLACEHOLDER: &str = "{pid}";

/// Expands every `{pid}` in an explicit `BSL_LOG_FILE` value to `pid`.
///
/// An explicit log file is appended to, so processes started with the same value
/// (several editor windows, an LSP server next to an MCP server) interleave their
/// lines in one file. The placeholder is an opt-in to one file per live process; a
/// value without it is returned unchanged. A pid is unique only among live processes:
/// a new process the OS hands a finished one's pid appends to that file, and the
/// earlier lines stay.
pub fn expand_log_file_path(path: PathBuf, pid: u32) -> PathBuf {
    match path.to_str() {
        Some(text) if text.contains(PID_PLACEHOLDER) => {
            PathBuf::from(text.replace(PID_PLACEHOLDER, &pid.to_string()))
        }
        _ => path,
    }
}

/// Reads an environment variable, treating a blanked-out value (`VAR=`) as unset.
///
/// A launcher that cannot remove a variable from a child's environment blanks it
/// instead, so an empty value means "no value", never a zero-length path or filter.
pub fn non_empty_env(name: &str) -> Option<String> {
    env::var(name).ok().and_then(non_empty)
}

fn non_empty(value: String) -> Option<String> {
    (!value.trim().is_empty()).then_some(value)
}

/// Set up the process's logging, failing the start when a log file the user named cannot
/// be set up. `default_log` marks the daemon's own default log instead: it falls back to
/// stderr. `bounded_file` caps the file for a long-lived daemon by rotating it. A failure
/// never truncates the file; the reason is appended to it when it can be opened.
pub fn start_logging(
    log_file: Option<PathBuf>,
    default_log: bool,
    bounded_file: bool,
    profile_filter: Option<String>,
    json_profile_filter: Option<String>,
) -> anyhow::Result<JournalGuard> {
    use std::io::Write;

    if !default_log {
        if let Some(path) = &log_file {
            // A bounded writer already rotates while it writes; the start-time rotation is
            // for the plain explicit log only.
            if !bounded_file {
                rotate_explicit_log(path);
            }
            // The path arrives already expanded, so the configured value is the only
            // place that still says whether the user opted into per-pid names.
            if let Some(pattern) = non_empty_env("BSL_LOG_FILE")
                .filter(|value| value.contains(PID_PLACEHOLDER))
                .map(PathBuf::from)
            {
                prune_stale_pid_logs(&pattern, path, mcp_server::ROTATED_LOG_RETENTION);
            }
        }
    }

    match setup_logging(
        log_file.clone(),
        bounded_file,
        profile_filter.clone(),
        json_profile_filter.clone(),
    ) {
        Ok(guard) => Ok(guard),
        Err(error) => {
            eprintln!("Failed to setup logging: {error}");
            if default_log {
                return setup_logging(None, false, profile_filter, json_profile_filter);
            }
            if let Some(path) = &log_file {
                let _ =
                    fs::OpenOptions::new().create(true).append(true).open(path).and_then(
                        |mut file| writeln!(file, "ERROR: Failed to setup logging: {error}"),
                    );
            }
            Err(error)
        }
    }
}

/// Moves an explicit log above [`mcp_server::LOG_ROTATE_BYTES`] to its `.prev` trail, replacing
/// the previous one, so the file opened next starts empty: an appended log otherwise grows
/// without bound (one measured file reached 5.9 GB).
///
/// The trail is never deleted here, unlike the daemon's cache log: the directory belongs to
/// whoever named the path, so a file there is theirs however old, and the bound is two files of
/// at most the threshold each. Every filesystem failure is ignored: a log that stays
/// unrotated keeps being appended to, and must not fail the start.
fn rotate_explicit_log(path: &std::path::Path) {
    if !fs::metadata(path).is_ok_and(|meta| meta.len() > mcp_server::LOG_ROTATE_BYTES) {
        return;
    }
    if let Some(trail) = mcp_server::rotated_log_path(path) {
        let _ = fs::rename(path, trail);
    }
}

/// Deletes sibling logs of a `{pid}` pattern left by processes that are long gone: every process
/// creates its own file and nothing else ever removes them.
///
/// A live process's open file cannot be deleted on Windows, which is the natural guard against
/// removing a log that is still being written; the current process's own file is skipped
/// explicitly for the platforms where deletion would succeed. Every error is ignored and the scan
/// is bounded, so this can neither fail nor stall the start.
fn prune_stale_pid_logs(pattern: &std::path::Path, own: &std::path::Path, retention: Duration) {
    const MAX_ENTRIES: usize = 10_000;

    let Some((prefix, suffix)) = pattern
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.split_once(PID_PLACEHOLDER))
    else {
        return;
    };
    // The pattern is the unexpanded setting and may be relative to a directory this process is
    // not in; the own path is already expanded, so the directory comes from it.
    let dir = match own.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => std::path::Path::new("."),
    };
    let own_name = own.file_name().and_then(|name| name.to_str()).unwrap_or_default();
    let own_trail = format!("{own_name}.prev");
    let Ok(entries) = fs::read_dir(dir) else { return };

    for entry in entries.take(MAX_ENTRIES).flatten() {
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else { continue };
        if name == own_name || name == own_trail {
            continue;
        }
        let stem = name.strip_suffix(".prev").unwrap_or(name);
        let Some(pid) = stem.strip_prefix(prefix).and_then(|rest| rest.strip_suffix(suffix)) else {
            continue;
        };
        if pid.is_empty() || !pid.bytes().all(|byte| byte.is_ascii_digit()) {
            continue;
        }
        let is_stale = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age > retention);
        if is_stale {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// An explicit log file that rotates while the process runs, not only at start: a daemon lives
/// for hours and an appended log otherwise grows without bound.
///
/// Logging must never be the reason the process dies, so a failure while rotating is swallowed:
/// the write that triggered it has already succeeded.
struct RotatingLogFile {
    path: PathBuf,
    limit: u64,
    state: Mutex<LogState>,
}

struct LogState {
    file: Option<fs::File>,
    written: u64,
}

impl RotatingLogFile {
    fn new(path: PathBuf) -> io::Result<Self> {
        Self::with_limit(path, mcp_server::LOG_ROTATE_BYTES)
    }

    fn with_limit(path: PathBuf, limit: u64) -> io::Result<Self> {
        let file = Self::open(&path)?;
        // Seeded from the existing length so a file already near the threshold rotates soon
        // after start instead of only at the next restart.
        let written = file.metadata().map_or(0, |meta| meta.len());
        Ok(Self { path, limit, state: Mutex::new(LogState { file: Some(file), written }) })
    }

    fn open(path: &std::path::Path) -> io::Result<fs::File> {
        fs::OpenOptions::new().create(true).append(true).open(path)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, LogState> {
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn rotate(&self, state: &mut LogState) {
        // The handle goes first: on Windows a rename fails on a file that is still open
        // without FILE_SHARE_DELETE.
        state.file = None;
        if let Some(trail) = mcp_server::rotated_log_path(&self.path) {
            let _ = fs::rename(&self.path, trail);
        }
        // A failed rename leaves the old file in place; counting from zero retries after
        // another threshold's worth of bytes rather than on every write.
        state.file = Self::open(&self.path).ok();
        state.written = 0;
    }
}

impl io::Write for &RotatingLogFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut state = self.lock();
        if state.file.is_none() {
            let file = RotatingLogFile::open(&self.path)?;
            state.written = file.metadata().map_or(0, |meta| meta.len());
            state.file = Some(file);
        }
        let Some(file) = state.file.as_mut() else { return Ok(0) };
        let count = io::Write::write(file, buf)?;
        state.written += count as u64;
        if state.written > self.limit {
            self.rotate(&mut state);
        }
        Ok(count)
    }

    fn flush(&mut self) -> io::Result<()> {
        match self.lock().file.as_mut() {
            Some(file) => io::Write::flush(file),
            None => Ok(()),
        }
    }
}

fn setup_logging(
    log_file: Option<PathBuf>,
    bounded_file: bool,
    profile_filter: Option<String>,
    json_profile_filter: Option<String>,
) -> anyhow::Result<JournalGuard> {
    use tracing_subscriber::fmt::writer::BoxMakeWriter;

    // A file log is appended to: a restart keeps the previous run's trail, and
    // concurrent short-lived daemon candidates (the broker's spawn race) do not
    // truncate the winner's live file.
    let writer: BoxMakeWriter = match log_file {
        Some(file) if bounded_file => {
            BoxMakeWriter::new(Mutex::new(bounded::BoundedLog::open(&file, true)?))
        }
        Some(file) => BoxMakeWriter::new(Arc::new(RotatingLogFile::new(file)?)),
        None => BoxMakeWriter::new(io::stderr),
    };

    let user_filter = non_empty_env("BSL_LOG").unwrap_or_else(|| "warn".to_owned());
    // The graph-build heartbeat (target `bsl_graph`) stays visible at the default
    // `warn` level: it is the only trail that localises a wedged cold build. The
    // directive precedes the user filter, so an explicit `bsl_graph=off` wins.
    let filter = format!("bsl_graph=info,{},salsa=warn", user_filter);

    use tracing_subscriber::Layer;
    bsl_search::lifecycle::set_process_id(uuid::Uuid::new_v4().to_string());
    let (layer, guard) = journal::layer();
    bsl_analyzer::tracing::Config { writer, filter, profile_filter, json_profile_filter }
        .init_with_layer(layer.with_filter(journal::filter(&user_filter)))?;

    Ok(guard)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn an_active_mcp_session_survives_repeated_log_rotation() {
        use mcp_server::{McpProfile, McpServer, SharedState};
        use rmcp::ServiceExt;
        use std::{io::Write, time::Duration};

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("daemon.log");
        let mut writer = bounded::BoundedLog::open(&path, true).unwrap();
        let (client_io, server_io) = tokio::io::duplex(1024 * 1024);
        let server = McpServer::new(McpProfile::Reference, SharedState::reference(None));
        let task = tokio::spawn(mcp_server::serve_stream(server, server_io));
        let budget = test_utils::wait::ceiling(Duration::from_secs(20));
        let client = tokio::time::timeout(budget, ().serve(client_io)).await.unwrap().unwrap();
        let payload = vec![b'x'; bounded::LOG_BYTES as usize];
        for _ in 0..6 {
            writer.write_all(&payload).unwrap();
            let tools =
                tokio::time::timeout(budget, client.list_all_tools()).await.unwrap().unwrap();
            assert!(!tools.is_empty());
            assert!(path.metadata().unwrap().len() <= bounded::LOG_BYTES);
            let previous = directory.path().join("daemon.log.prev");
            assert!(previous.metadata().map(|m| m.len()).unwrap_or(0) <= bounded::LOG_BYTES);
        }
        tokio::time::timeout(budget, client.cancel()).await.unwrap().unwrap();
        task.abort();
    }

    #[test]
    fn a_blank_value_is_read_as_no_value() {
        assert_eq!(non_empty(String::new()), None);
        assert_eq!(non_empty("   ".to_owned()), None);
        assert_eq!(non_empty("\t\n".to_owned()), None);
    }

    #[test]
    fn a_value_with_content_is_kept_verbatim() {
        assert_eq!(non_empty("warn".to_owned()), Some("warn".to_owned()));
        assert_eq!(non_empty(" warn ".to_owned()), Some(" warn ".to_owned()));
    }

    #[test]
    fn pid_placeholder_is_replaced_with_the_process_id() {
        assert_eq!(
            expand_log_file_path(PathBuf::from("logs/bsl-{pid}.log"), 4242),
            PathBuf::from("logs/bsl-4242.log")
        );
    }

    #[test]
    fn every_pid_placeholder_occurrence_is_replaced() {
        assert_eq!(
            expand_log_file_path(PathBuf::from("logs/{pid}/bsl-{pid}.log"), 7),
            PathBuf::from("logs/7/bsl-7.log")
        );
    }

    #[test]
    fn path_without_placeholder_is_unchanged() {
        for raw in ["logs/bsl.log", "logs/bsl-pid.log", "logs/{PID}.log", "logs/{pid.log"] {
            assert_eq!(expand_log_file_path(PathBuf::from(raw), 4242), PathBuf::from(raw));
        }
    }

    /// Set in a child this module's tests start: what the child does with its one process-wide
    /// logging setup, `start:<marker>` or `default` / `explicit` for an unwritable destination.
    const CHILD_ROLE: &str = "BSL_LOGGING_TEST_CHILD";
    const CHILD_LOG: &str = "BSL_LOGGING_TEST_LOG";

    /// Run `test` again in a child process as `role` over `log`: a process installs its global
    /// subscriber once, so every start a test compares is a process of its own.
    fn run_child(
        test: &str,
        role: &str,
        log: &std::path::Path,
        bsl_log: &str,
    ) -> std::process::Output {
        let output = std::process::Command::new(env::current_exe().unwrap())
            .args(["--exact", &format!("cli::logging::tests::{test}"), "--nocapture"])
            .env(CHILD_ROLE, role)
            .env(CHILD_LOG, log)
            .env("BSL_LOG", bsl_log)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "child {role} failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    /// What a child of these tests does; `false` in the test's own process.
    fn act_as_child() -> bool {
        let Ok(role) = env::var(CHILD_ROLE) else { return false };
        let log = PathBuf::from(env::var_os(CHILD_LOG).unwrap());
        match role.split_once(':') {
            Some(("start", marker)) => {
                let _guard = start_logging(Some(log), false, false, None, None).unwrap();
                tracing::warn!("{marker}");
            }
            Some(("fail", _)) => {
                assert!(start_logging(Some(log), false, false, None, None).is_err());
            }
            Some(("default", marker)) => {
                let _guard = start_logging(Some(log), true, false, None, None)
                    .expect("the daemon's default log falls back to stderr");
                tracing::warn!("{marker}");
            }
            Some(("explicit", _)) => {
                assert!(
                    start_logging(Some(log), false, false, None, None).is_err(),
                    "an explicit log that cannot be opened must fail the start"
                );
            }
            _ => panic!("unknown child role {role}"),
        }
        true
    }

    const SENTINEL: &str = "earlier run's last line\n";

    fn assert_appended(log: &std::path::Path, markers: &[&str]) -> String {
        let text = fs::read_to_string(log).unwrap();
        assert!(text.starts_with(SENTINEL), "the earlier bytes were truncated:\n{text}");
        let mut from = SENTINEL.len();
        for marker in markers {
            let at = text[from..]
                .find(marker)
                .unwrap_or_else(|| panic!("{marker} is not appended in order:\n{text}"));
            from += at + marker.len();
        }
        text
    }

    #[test]
    fn an_explicit_log_file_is_appended_across_starts() {
        if act_as_child() {
            return;
        }
        const TEST: &str = "an_explicit_log_file_is_appended_across_starts";
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("run.log");
        fs::write(&log, SENTINEL).unwrap();

        run_child(TEST, "start:first-start-line", &log, "warn");
        run_child(TEST, "start:second-start-line", &log, "warn");

        assert_appended(&log, &["first-start-line", "second-start-line"]);
    }

    #[test]
    fn a_pid_log_file_a_new_process_inherits_is_appended() {
        if act_as_child() {
            return;
        }
        const TEST: &str = "a_pid_log_file_a_new_process_inherits_is_appended";
        let dir = tempfile::tempdir().unwrap();
        // Two processes the OS gave the same pid, one after the other.
        let log = expand_log_file_path(dir.path().join("run-{pid}.log"), 4242);
        assert_eq!(log, dir.path().join("run-4242.log"));
        fs::write(&log, SENTINEL).unwrap();

        run_child(TEST, "start:first-owner-of-4242", &log, "warn");
        run_child(TEST, "start:second-owner-of-4242", &log, "warn");

        assert_appended(&log, &["first-owner-of-4242", "second-owner-of-4242"]);
    }

    /// The setup fails after the file is already open (the log filter is invalid): the start
    /// fails, and the file keeps its bytes with the reason appended.
    #[test]
    fn a_setup_error_after_the_file_is_opened_keeps_its_bytes() {
        if act_as_child() {
            return;
        }
        const TEST: &str = "a_setup_error_after_the_file_is_opened_keeps_its_bytes";
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("run.log");
        fs::write(&log, SENTINEL).unwrap();

        run_child(TEST, "fail:", &log, "bsl_graph=[");

        let text = assert_appended(&log, &["ERROR: Failed to setup logging"]);
        assert!(text.contains("invalid log filter"), "{text}");
    }

    /// A destination that cannot be opened at all: an explicit log fails the start, the
    /// daemon's default log falls back to stderr and the process runs on.
    #[test]
    fn an_unwritable_explicit_log_fails_and_a_default_one_falls_back_to_stderr() {
        if act_as_child() {
            return;
        }
        const TEST: &str =
            "an_unwritable_explicit_log_fails_and_a_default_one_falls_back_to_stderr";
        let dir = tempfile::tempdir().unwrap();
        let unwritable = dir.path().join("a-directory");
        fs::create_dir(&unwritable).unwrap();

        let explicit = run_child(TEST, "explicit:", &unwritable, "warn");
        let default = run_child(TEST, "default:served-from-stderr", &unwritable, "warn");

        assert!(String::from_utf8_lossy(&explicit.stderr).contains("Failed to setup logging"));
        let stderr = String::from_utf8_lossy(&default.stderr);
        assert!(stderr.contains("Failed to setup logging"), "{stderr}");
        assert!(stderr.contains("served-from-stderr"), "no stderr fallback:\n{stderr}");
        assert!(unwritable.is_dir(), "the destination was replaced");
    }

    /// Writes `len` real bytes (not a sparse `set_len` file): the rotation reads only the
    /// length, but real bytes keep "the old bytes moved" checkable by content.
    fn write_log(path: &std::path::Path, head: &str, len: u64) {
        let mut bytes = head.as_bytes().to_vec();
        bytes.resize(usize::try_from(len).unwrap(), b'.');
        fs::write(path, bytes).unwrap();
    }

    const OVER: u64 = mcp_server::LOG_ROTATE_BYTES + 1;

    #[test]
    fn a_log_over_the_threshold_is_rotated_to_its_trail() {
        if act_as_child() {
            return;
        }
        const TEST: &str = "a_log_over_the_threshold_is_rotated_to_its_trail";
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("run.log");
        let trail = dir.path().join("run.log.prev");
        write_log(&log, SENTINEL, OVER);

        run_child(TEST, "start:fresh-after-rotation", &log, "warn");

        let old = fs::read(&trail).unwrap();
        assert_eq!(u64::try_from(old.len()).unwrap(), OVER);
        assert!(old.starts_with(SENTINEL.as_bytes()));
        let live = fs::read_to_string(&log).unwrap();
        assert!(!live.contains(SENTINEL), "the old bytes stayed in the live log");
        assert!(live.contains("fresh-after-rotation"), "{live}");
    }

    /// The control of the test above: a log not over the threshold is a normal log.
    #[test]
    fn a_log_at_the_threshold_is_appended_and_leaves_no_trail() {
        if act_as_child() {
            return;
        }
        const TEST: &str = "a_log_at_the_threshold_is_appended_and_leaves_no_trail";
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("run.log");
        // Short of the threshold by more than the one line the child writes.
        write_log(&log, SENTINEL, mcp_server::LOG_ROTATE_BYTES - 4096);

        run_child(TEST, "start:appended-not-rotated", &log, "warn");

        assert!(!dir.path().join("run.log.prev").exists(), "a log at the threshold was rotated");
        let bytes = fs::read(&log).unwrap();
        assert!(bytes.starts_with(SENTINEL.as_bytes()));
        assert!(String::from_utf8_lossy(&bytes).contains("appended-not-rotated"));
    }

    #[test]
    fn a_rotation_replaces_an_existing_trail() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("run.log");
        let trail = dir.path().join("run.log.prev");
        fs::write(&trail, "an older trail").unwrap();
        write_log(&log, SENTINEL, OVER);

        rotate_explicit_log(&log);

        let moved = fs::read(&trail).unwrap();
        assert_eq!(u64::try_from(moved.len()).unwrap(), OVER);
        assert!(moved.starts_with(SENTINEL.as_bytes()), "the older trail was kept");
        assert!(!log.exists(), "the rotated log was not moved away");
    }

    /// The start itself leaves the daemon's cache log alone (the MCP command rotates it before
    /// logging starts); once the process writes, the running writer applies the same bound, so
    /// the first line pushes an over-threshold file to its trail.
    #[test]
    fn the_daemon_default_log_is_rotated_by_the_writer_not_the_logging_start() {
        if act_as_child() {
            return;
        }
        const TEST: &str = "the_daemon_default_log_is_rotated_by_the_writer_not_the_logging_start";
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("run.log");
        write_log(&log, SENTINEL, OVER);

        run_child(TEST, "default:default-log-line", &log, "warn");

        let trail = fs::read(dir.path().join("run.log.prev")).unwrap();
        assert!(u64::try_from(trail.len()).unwrap() > OVER, "the line was not written first");
        assert!(trail.starts_with(SENTINEL.as_bytes()));
        assert!(fs::metadata(&log).map_or(0, |meta| meta.len()) < OVER);
    }

    fn write_through(log: &RotatingLogFile, bytes: &[u8]) {
        use std::io::Write;
        let mut writer = log;
        writer.write_all(bytes).unwrap();
        writer.flush().unwrap();
    }

    #[test]
    fn writing_past_the_threshold_rotates_while_running() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.log");
        let log = RotatingLogFile::with_limit(path.clone(), 100).unwrap();

        write_through(&log, &[b'a'; 60]);
        write_through(&log, &[b'b'; 60]);

        let trail = fs::read(dir.path().join("run.log.prev")).unwrap();
        assert_eq!(trail.len(), 120);
        assert!(fs::metadata(&path).unwrap().len() < 100);
        write_through(&log, b"after");
        assert_eq!(fs::read(&path).unwrap(), b"after");
    }

    /// The control of the test above: the same writes under the threshold leave no trail.
    #[test]
    fn writing_below_the_threshold_leaves_no_trail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.log");
        let log = RotatingLogFile::with_limit(path.clone(), 100).unwrap();

        write_through(&log, &[b'a'; 40]);
        write_through(&log, &[b'b'; 40]);

        assert!(!dir.path().join("run.log.prev").exists());
        assert_eq!(fs::metadata(&path).unwrap().len(), 80);
    }

    #[test]
    fn a_file_already_near_the_threshold_rotates_after_a_small_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.log");
        write_log(&path, SENTINEL, 95);
        let log = RotatingLogFile::with_limit(path.clone(), 100).unwrap();

        write_through(&log, b"0123456789");

        let trail = fs::read(dir.path().join("run.log.prev")).unwrap();
        assert!(trail.starts_with(SENTINEL.as_bytes()));
        assert_eq!(trail.len(), 105);
        assert!(fs::metadata(&path).unwrap().len() < 100);
    }

    fn set_age(path: &std::path::Path, age: Duration) {
        let file = fs::OpenOptions::new().write(true).open(path).unwrap();
        file.set_modified(std::time::SystemTime::now() - age).unwrap();
    }

    #[test]
    fn pruning_removes_only_old_siblings_of_the_pid_pattern() {
        let dir = tempfile::tempdir().unwrap();
        let day = Duration::from_secs(24 * 60 * 60);
        let pattern = dir.path().join("run-{pid}.log");
        let own = dir.path().join("run-100.log");
        let old_match = dir.path().join("run-200.log");
        let old_trail = dir.path().join("run-201.log.prev");
        let fresh_match = dir.path().join("run-300.log");
        let unrelated = dir.path().join("run-abc.log");
        let other = dir.path().join("notes.txt");
        for file in [&own, &old_match, &old_trail, &fresh_match, &unrelated, &other] {
            fs::write(file, "x").unwrap();
        }
        for file in [&own, &old_match, &old_trail, &unrelated, &other] {
            set_age(file, day * 3);
        }

        prune_stale_pid_logs(&pattern, &own, day);

        assert!(!old_match.exists(), "an old sibling was kept");
        assert!(!old_trail.exists(), "an old sibling trail was kept");
        assert!(own.exists(), "the current file was deleted");
        assert!(fresh_match.exists(), "a fresh sibling was deleted");
        assert!(unrelated.exists(), "a non-digit name matched the pattern");
        assert!(other.exists(), "a file outside the pattern was deleted");
    }

    /// The configured pattern is often relative and unexpanded; the directory to scan comes from
    /// the process's own expanded log path, never from the process's working directory.
    #[test]
    fn pruning_scans_the_directory_of_the_own_log_not_of_the_pattern() {
        let dir = tempfile::tempdir().unwrap();
        let day = Duration::from_secs(24 * 60 * 60);
        let own = dir.path().join("bsl-analyzer-100.log");
        let old_match = dir.path().join("bsl-analyzer-200.log");
        for file in [&own, &old_match] {
            fs::write(file, "x").unwrap();
        }
        set_age(&old_match, day * 3);

        prune_stale_pid_logs(std::path::Path::new("bsl-analyzer-{pid}.log"), &own, day);

        assert!(!old_match.exists(), "the sibling next to the own log was not pruned");
        assert!(own.exists(), "the current file was deleted");
    }

    /// The trail path is a directory, so the rename cannot happen: the start still succeeds,
    /// and the old file keeps its bytes and keeps being appended to.
    #[test]
    fn a_rotation_that_cannot_happen_keeps_the_old_log_and_starts() {
        if act_as_child() {
            return;
        }
        const TEST: &str = "a_rotation_that_cannot_happen_keeps_the_old_log_and_starts";
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("run.log");
        let trail = dir.path().join("run.log.prev");
        fs::create_dir(&trail).unwrap();
        write_log(&log, SENTINEL, OVER);

        run_child(TEST, "start:appended-despite-failed-rotation", &log, "warn");

        assert!(trail.is_dir(), "the trail directory was replaced");
        let bytes = fs::read(&log).unwrap();
        assert!(bytes.starts_with(SENTINEL.as_bytes()), "the old bytes were lost");
        assert!(String::from_utf8_lossy(&bytes).contains("appended-despite-failed-rotation"));
    }
}

#[cfg(all(test, target_os = "linux"))]
mod broker_rotation_tests;
