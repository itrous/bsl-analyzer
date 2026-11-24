//! End-to-end analysis-scope filtering in `analyze`: `--git-diff`,
//! `--changed-files` and `[analysis].diff_base` must restrict the SARIF
//! report to files changed relative to the vendor state.

mod common;

use std::{fs, path::Path, process::Command};

use tempfile::TempDir;

/// A module that reliably produces at least one diagnostic (unclosed procedure).
const BROKEN: &str = "Процедура Тест(\n";

/// Drop the ambient git environment. When the suite runs inside a `git commit` pre-commit hook,
/// GIT_DIR / GIT_INDEX_FILE point at the real repository and win over `git -C <tempdir>`: the
/// fixture would build its history in the developer's repository, or die on its locked index.
/// The analysis binary is scrubbed for the same reason — resolving `--git-diff` shells out to
/// git from the source directory it was given.
fn hermetic(command: &mut Command) -> &mut Command {
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("GIT_") {
            command.env_remove(&key);
        }
    }
    command
}

fn run_git(dir: &Path, args: &[&str]) {
    let mut command = Command::new("git");
    let output = hermetic(&mut command)
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=test", "-c", "user.email=test@example.com"])
        .args(args)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {:?} failed:\n{}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Vendor commit with two broken modules, then a head state where only
/// `Changed.bsl` differs from vendor.
fn setup_repo() -> TempDir {
    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();

    fs::write(root.join("Vendor.bsl"), BROKEN).expect("vendor file");
    fs::write(root.join("Changed.bsl"), BROKEN).expect("changed file");
    run_git(root, &["init", "-q"]);
    run_git(root, &["add", "."]);
    run_git(root, &["commit", "-q", "-m", "vendor"]);
    run_git(root, &["branch", "vendor"]);

    let changed = format!("{BROKEN}\nПроцедура Ещё(\n");
    fs::write(root.join("Changed.bsl"), changed).expect("modify changed file");

    temp
}

fn run_analyze(root: &Path, extra_args: &[&str]) -> (bool, String) {
    let output_dir = root.join("reports");
    fs::create_dir_all(&output_dir).expect("output dir");

    let mut command = Command::new(common::qualified_app());
    let output = hermetic(&mut command)
        .args(["analyze", "-s"])
        .arg(root)
        .args(["-r", "sarif", "-o"])
        .arg(&output_dir)
        .arg("-q")
        .args(extra_args)
        .output()
        .expect("run bsl-analyzer");

    let sarif = fs::read_to_string(output_dir.join("bsl-analyzer.sarif")).unwrap_or_default();
    (output.status.success(), sarif)
}

fn reported_files(sarif: &str) -> (bool, bool) {
    (sarif.contains("Vendor.bsl"), sarif.contains("Changed.bsl"))
}

#[test]
fn git_diff_scope_reports_only_files_changed_from_vendor() {
    let temp = setup_repo();

    // Without a scope both broken modules are reported.
    let (ok, sarif) = run_analyze(temp.path(), &[]);
    assert!(ok);
    let (vendor, changed) = reported_files(&sarif);
    assert!(vendor && changed, "unscoped run must report both files:\n{sarif}");

    let (ok, sarif) = run_analyze(temp.path(), &["--incremental", "--git-diff", "vendor"]);
    assert!(ok);
    let (vendor, changed) = reported_files(&sarif);
    assert!(!vendor, "vendor-identical file must be out of scope:\n{sarif}");
    assert!(changed, "file changed against vendor must stay in scope:\n{sarif}");
}

#[test]
fn changed_files_scope_reports_only_the_listed_files() {
    let temp = setup_repo();

    // Named as the caller would: a temporary directory is reached through a symlink on
    // macOS, and the flag must accept that spelling. Resolving it here would only hide
    // whether the run does.
    let changed_path = temp.path().join("Changed.bsl");
    let (ok, sarif) = run_analyze(
        temp.path(),
        &["--incremental", "--changed-files", changed_path.to_str().expect("utf-8 path")],
    );
    assert!(ok);
    let (vendor, changed) = reported_files(&sarif);
    assert!(!vendor && changed, "only the listed file must be reported:\n{sarif}");
}

#[test]
fn config_diff_base_applies_without_cli_flags() {
    let temp = setup_repo();
    fs::write(temp.path().join("bsl-analyzer.toml"), "[analysis]\ndiff_base = \"vendor\"\n")
        .expect("config");

    let (ok, sarif) = run_analyze(temp.path(), &[]);
    assert!(ok);
    let (vendor, changed) = reported_files(&sarif);
    assert!(!vendor && changed, "[analysis].diff_base must scope the run:\n{sarif}");
}

#[test]
fn missing_git_ref_fails_the_run() {
    let temp = setup_repo();

    let (ok, _) = run_analyze(temp.path(), &["--incremental", "--git-diff", "no-such-ref"]);
    assert!(!ok, "a configured but unresolvable scope must be a hard error");
}

/// A relative `-s .` must still match the absolute keys of a native git scope.
#[test]
fn relative_source_dir_matches_the_native_git_scope() {
    let temp = setup_repo();
    fs::create_dir_all(temp.path().join("reports")).expect("output dir");

    let output = Command::new(common::qualified_app())
        .current_dir(temp.path())
        .args(["analyze", "-s", ".", "-r", "sarif", "-o", "reports", "-q"])
        .args(["--incremental", "--git-diff", "vendor"])
        .output()
        .expect("run bsl-analyzer");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));

    let sarif =
        fs::read_to_string(temp.path().join("reports/bsl-analyzer.sarif")).expect("sarif report");
    let (vendor, changed) = reported_files(&sarif);
    assert!(!vendor, "vendor-identical file must be out of scope:\n{sarif}");
    assert!(changed, "relative -s must not silently empty the scope:\n{sarif}");
}

/// The line-gate travels from the CLI into the diagnostics config: a file
/// admitted by hunks that cover none of its diagnostics must produce nothing.
#[test]
fn diff_filter_hunks_outside_diagnostics_drop_all_results() {
    let temp = setup_repo();
    let report = r#"{
        "base_ref": "vendor",
        "head_ref": "HEAD",
        "files": {
            "Changed.bsl": { "hunks": [[999, 999]] },
            "Vendor.bsl": { "hunks": [] }
        }
    }"#;
    let report_path = temp.path().join("diff-report.json");
    fs::write(&report_path, report).expect("diff report");

    let (ok, sarif) =
        run_analyze(temp.path(), &["--diff-filter", report_path.to_str().expect("utf-8 path")]);
    assert!(ok);
    let (vendor, changed) = reported_files(&sarif);
    assert!(
        !vendor && !changed,
        "hunks covering no diagnostic line must drop every result:\n{sarif}"
    );
}

/// `--diff-filter` wins over `--git-diff` when both are given.
#[test]
fn diff_filter_takes_precedence_over_git_diff() {
    let temp = setup_repo();
    // The external report admits only the vendor-identical file — the exact
    // opposite of what `--git-diff vendor` would compute.
    let report =
        r#"{ "base_ref": "x", "head_ref": "y", "files": { "Vendor.bsl": { "hunks": null } } }"#;
    let report_path = temp.path().join("diff-report.json");
    fs::write(&report_path, report).expect("diff report");

    let (ok, sarif) = run_analyze(
        temp.path(),
        &[
            "--diff-filter",
            report_path.to_str().expect("utf-8 path"),
            "--incremental",
            "--git-diff",
            "vendor",
        ],
    );
    assert!(ok);
    let (vendor, changed) = reported_files(&sarif);
    assert!(vendor && !changed, "the external diff report must win over --git-diff:\n{sarif}");
}
