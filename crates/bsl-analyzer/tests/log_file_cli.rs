//! `BSL_LOG_FILE` through the real CLI entrypoint: the opt-in `{pid}` placeholder
//! and the append-on-start behaviour without it; a log file that cannot be opened fails the start.
use std::{fs, path::Path, process::Command};

use tempfile::TempDir;

/// Runs a short command that initialises logging and returns the child's pid.
fn run_with_log_file(log_file: &Path, module: &Path) -> u32 {
    let mut child = Command::new(env!("CARGO_BIN_EXE_bsl-analyzer-app"))
        .arg("format")
        .arg(module)
        .env("BSL_LOG_FILE", log_file)
        .env("BSL_LOG", "info")
        .env_remove("BSL_PROFILE")
        .env_remove("BSL_PROFILE_JSON")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn bsl-analyzer-app");
    let pid = child.id();
    assert!(child.wait().expect("wait").success(), "format run failed");
    pid
}

fn start_line(pid: u32) -> String {
    format!("BSL Analyzer starting (pid: {pid})")
}

fn fixture() -> (TempDir, std::path::PathBuf) {
    let temp = TempDir::new().expect("tempdir");
    let module = temp.path().join("Module.bsl");
    fs::write(&module, "Процедура Т()\nКонецПроцедуры\n").expect("write module");
    (temp, module)
}

#[test]
fn pid_placeholder_gives_each_process_its_own_log_file() {
    let (temp, module) = fixture();
    let template = temp.path().join("run-{pid}.log");

    let first = run_with_log_file(&template, &module);
    let second = run_with_log_file(&template, &module);
    assert_ne!(first, second);

    for pid in [first, second] {
        let log = temp.path().join(format!("run-{pid}.log"));
        let text = fs::read_to_string(&log)
            .unwrap_or_else(|error| panic!("{} is missing: {error}", log.display()));
        assert!(
            text.contains(&start_line(pid)),
            "{} lacks its own start line:\n{text}",
            log.display()
        );
    }
    assert!(!template.exists(), "the placeholder must not be used literally as a file name");
}

#[test]
fn log_file_without_placeholder_is_shared_and_appended_on_start() {
    let (temp, module) = fixture();
    let log = temp.path().join("run.log");

    let first = run_with_log_file(&log, &module);
    let second = run_with_log_file(&log, &module);

    let text = fs::read_to_string(&log).expect("read shared log");
    let first_at = text.find(&start_line(first));
    let second_at = text.find(&start_line(second));
    assert!(first_at.is_some(), "the earlier run's log must survive a restart:\n{text}");
    assert!(first_at < second_at, "the later run must be appended after it:\n{text}");
    let entries = fs::read_dir(temp.path()).expect("read dir").count();
    assert_eq!(entries, 2, "only Module.bsl and run.log are expected");
}

#[test]
fn an_explicit_log_file_that_cannot_be_opened_fails_the_start() {
    let (temp, module) = fixture();
    let unwritable = temp.path().join("a-directory");
    fs::create_dir(&unwritable).expect("create directory");

    let output = Command::new(env!("CARGO_BIN_EXE_bsl-analyzer-app"))
        .arg("format")
        .arg(&module)
        .env("BSL_LOG_FILE", &unwritable)
        .env_remove("BSL_PROFILE")
        .env_remove("BSL_PROFILE_JSON")
        .output()
        .expect("run bsl-analyzer-app");

    assert!(!output.status.success(), "the start went on without the log file the user named");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Failed to setup logging"), "{stderr}");
    assert!(unwritable.is_dir(), "the destination was replaced");
}

#[test]
fn a_blanked_out_log_file_variable_is_treated_as_unset() {
    let (temp, module) = fixture();

    let output = Command::new(env!("CARGO_BIN_EXE_bsl-analyzer-app"))
        .arg("format")
        .arg(&module)
        .env("BSL_LOG_FILE", "")
        .env_remove("BSL_PROFILE")
        .env_remove("BSL_PROFILE_JSON")
        .output()
        .expect("run bsl-analyzer-app");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "a blanked-out BSL_LOG_FILE failed the start:\n{stderr}");
    assert!(!stderr.contains("Failed to setup logging"), "{stderr}");
    let entries = fs::read_dir(temp.path()).expect("read dir").count();
    assert_eq!(entries, 1, "no log file was named, so none may be created");
}

#[test]
fn a_blanked_out_log_level_variable_keeps_the_default_level() {
    let (temp, _module) = fixture();
    let log = temp.path().join("run.log");

    let output = Command::new(env!("CARGO_BIN_EXE_bsl-analyzer-app"))
        .arg("analyze")
        .arg("-s")
        .arg(temp.path())
        .arg("--format")
        .arg("jsonl")
        .env("BSL_LOG_FILE", &log)
        .env("BSL_LOG", "")
        .env_remove("BSL_PROFILE")
        .env_remove("BSL_PROFILE_JSON")
        .output()
        .expect("run bsl-analyzer-app");

    assert!(output.status.success(), "analyze failed: {}", String::from_utf8_lossy(&output.stderr));
    let text = fs::read_to_string(&log).expect("read log");
    assert!(
        text.contains("METADATA LOAD"),
        "a blanked-out BSL_LOG silenced the default `warn` level:\n{text}"
    );
}
