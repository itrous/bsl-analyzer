//! The jemalloc configuration compiled into the binary, read the way an operator reads
//! it: `stats_print` at exit of a short command. The environment variable only switches
//! the printing on, so what the binary reports for `opt.narenas` is its embedded value.
#![cfg(target_os = "linux")]

use std::process::Command;

/// `opt.narenas` as jemalloc prints it at exit of `--version`, with `conf` prepended to
/// the options the test passes through the environment.
fn reported_narenas(conf: &str) -> u32 {
    let output = Command::new(env!("CARGO_BIN_EXE_bsl-analyzer-app"))
        .arg("--version")
        .env("_RJEM_MALLOC_CONF", format!("{conf}stats_print:true"))
        .output()
        .expect("run bsl-analyzer-app --version");
    assert!(output.status.success(), "--version failed: {:?}", output.status);
    let report = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    report
        .lines()
        .find_map(|line| line.trim().strip_prefix("opt.narenas:"))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|count| count.parse().ok())
        .unwrap_or_else(|| panic!("no `opt.narenas` line in the jemalloc report:\n{report}"))
}

/// Four arenas instead of `4 × ncpu`, with no environment variable asking for them.
#[test]
fn the_binary_runs_four_jemalloc_arenas_by_itself() {
    assert_eq!(
        reported_narenas("narenas:8,"),
        8,
        "the control: the report follows the configuration, so a different embedded value would show"
    );
    assert_eq!(reported_narenas(""), 4, "the arena count compiled into the binary");
}
