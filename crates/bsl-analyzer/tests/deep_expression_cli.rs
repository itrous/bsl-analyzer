//! A single statement with a long chain of calls used to abort the process with a stack
//! overflow, losing the whole run and naming no file. The CLI now does its work on threads
//! with an explicit stack, so the run completes.
use std::{fs, path::Path, process::Command};

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

fn write_module(root: &Path, name: &str, text: &str) {
    let module_dir = root.join("CommonModules").join(name).join("Ext");
    fs::create_dir_all(&module_dir).expect("create module dir");
    fs::write(module_dir.join("Module.bsl"), text).expect("write module");
}

fn assert_run_completes(root: &Path) {
    let output = Command::new(env!("CARGO_BIN_EXE_bsl-analyzer-app"))
        .arg("analyze")
        .arg("--source-dir")
        .arg(root)
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

/// A lone file is analysed on the thread that drives the run: rayon has nothing to split,
/// so the parallel loop folds it in place.
#[test]
fn a_long_call_chain_does_not_abort_the_run() {
    let temp = TempDir::new().expect("tempdir");
    write_module(temp.path(), "Probe", &module_with_chain(LINKS));

    assert_run_completes(temp.path());
}

/// With more than one file the parallel loop splits and every file is analysed on a
/// worker of the global rayon pool, so the workers need the headroom too.
#[test]
fn a_long_call_chain_among_other_modules_does_not_abort_the_run() {
    let temp = TempDir::new().expect("tempdir");
    write_module(temp.path(), "Probe", &module_with_chain(LINKS));
    for index in 0..7 {
        write_module(
            temp.path(),
            &format!("Other{index}"),
            &format!("Процедура П{index}()\r\nКонецПроцедуры\r\n"),
        );
    }

    assert_run_completes(temp.path());
}
