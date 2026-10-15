//! `BSL_LOG_FILE` through the real CLI entrypoint: the opt-in `{pid}` placeholder
//! and the unchanged truncate-on-start behaviour without it.
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
fn log_file_without_placeholder_is_shared_and_truncated_on_start() {
    let (temp, module) = fixture();
    let log = temp.path().join("run.log");

    let first = run_with_log_file(&log, &module);
    let second = run_with_log_file(&log, &module);

    let text = fs::read_to_string(&log).expect("read shared log");
    assert!(text.contains(&start_line(second)), "the last run must own the file:\n{text}");
    assert!(!text.contains(&start_line(first)), "the file must be truncated on start:\n{text}");
    let entries = fs::read_dir(temp.path()).expect("read dir").count();
    assert_eq!(entries, 2, "only Module.bsl and run.log are expected");
}
