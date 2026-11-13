use std::{env, fs, io, path::PathBuf, sync::Arc};

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
/// stderr. A failure never truncates the file; the reason is appended to it when it can
/// be opened.
pub fn start_logging(
    log_file: Option<PathBuf>,
    default_log: bool,
    profile_filter: Option<String>,
    json_profile_filter: Option<String>,
) -> anyhow::Result<JournalGuard> {
    use std::io::Write;

    match setup_logging(log_file.clone(), profile_filter.clone(), json_profile_filter.clone()) {
        Ok(guard) => Ok(guard),
        Err(error) => {
            eprintln!("Failed to setup logging: {error}");
            if default_log {
                return setup_logging(None, profile_filter, json_profile_filter);
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

fn setup_logging(
    log_file: Option<PathBuf>,
    profile_filter: Option<String>,
    json_profile_filter: Option<String>,
) -> anyhow::Result<JournalGuard> {
    use tracing_subscriber::fmt::writer::BoxMakeWriter;

    // A file log is appended to: a restart keeps the previous run's trail, and
    // concurrent short-lived daemon candidates (the broker's spawn race) do not
    // truncate the winner's live file.
    let writer: BoxMakeWriter = match log_file {
        Some(file) => BoxMakeWriter::new(Arc::new(
            fs::OpenOptions::new().create(true).append(true).open(&file)?,
        )),
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
                let _guard = start_logging(Some(log), false, None, None).unwrap();
                tracing::warn!("{marker}");
            }
            Some(("fail", _)) => {
                assert!(start_logging(Some(log), false, None, None).is_err());
            }
            Some(("default", marker)) => {
                let _guard = start_logging(Some(log), true, None, None)
                    .expect("the daemon's default log falls back to stderr");
                tracing::warn!("{marker}");
            }
            Some(("explicit", _)) => {
                assert!(
                    start_logging(Some(log), false, None, None).is_err(),
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
}
