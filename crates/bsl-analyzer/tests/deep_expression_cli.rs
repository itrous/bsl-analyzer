//! A single statement with a long chain of calls used to abort the process with a stack
//! overflow, losing the whole run and naming no file. The CLI now does its work on a
//! thread with an explicit stack, so the run completes.
use std::{fs, process::Command};

use tempfile::TempDir;

/// Long enough to overflow an 8 MiB main-thread stack — the Linux and macOS default — in
/// a debug build, so the test fails without the explicit analysis stack on every CI host,
/// not only on Windows, where the main thread gets 1 MiB.
const LINKS: usize = 1000;

/// Builds a module whose single statement chains `links` calls.
fn module_with_chain(links: usize) -> String {
    let mut text = String::from("Процедура Собрать()\r\n\tОписание = Конструктор()\r\n");
    for index in 0..links {
        text.push_str(&format!("\t\t.Свойство(\"П{index}\")\r\n"));
    }
    text.push_str(";\r\nКонецПроцедуры\r\n");
    text
}

#[test]
fn a_long_call_chain_does_not_abort_the_run() {
    let temp = TempDir::new().expect("tempdir");
    let module_dir = temp.path().join("CommonModules").join("Probe").join("Ext");
    fs::create_dir_all(&module_dir).expect("create module dir");
    fs::write(module_dir.join("Module.bsl"), module_with_chain(LINKS)).expect("write module");

    let output = Command::new(env!("CARGO_BIN_EXE_bsl-analyzer-app"))
        .arg("analyze")
        .arg("--source-dir")
        .arg(temp.path())
        .arg("--format")
        .arg("console")
        .env_remove("BSL_LOG")
        .env_remove("BSL_LOG_FILE")
        .env_remove("BSL_PROFILE")
        .env_remove("BSL_PROFILE_JSON")
        .output()
        .expect("spawn bsl-analyzer-app");

    assert!(
        output.status.success(),
        "a {LINKS}-link chain aborted the run: status {:?}, stderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
}
