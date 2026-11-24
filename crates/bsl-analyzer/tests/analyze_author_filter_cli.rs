//! End-to-end `ignored_authors` filtering in `analyze`: findings on lines
//! last touched by an ignored git author must not reach the report, while
//! the user's own, uncommitted and untracked code always does.

mod common;

use std::{fs, path::Path, process::Command};

use tempfile::TempDir;

/// A module that reliably produces at least one diagnostic (unclosed procedure).
const BROKEN: &str = "Процедура Тест(\n";

const VENDOR_NAME: &str = "Фирма Тест";
const VENDOR_EMAIL: &str = "vendor@example.com";
const DEV_NAME: &str = "Разработчик";
const DEV_EMAIL: &str = "dev@example.com";

/// Drop the ambient git environment. When the suite runs inside a `git commit` pre-commit
/// hook, inherited GIT_* variables win over both the `-c` identity below — silently
/// reattributing the fixture commits — and the fixture directory itself: GIT_DIR and
/// GIT_INDEX_FILE would point every command at the developer's own repository.
fn hermetic(command: &mut Command) -> &mut Command {
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("GIT_") {
            command.env_remove(&key);
        }
    }
    command
}

fn run_git_as(dir: &Path, name: &str, email: &str, args: &[&str]) {
    let mut cmd = Command::new("git");
    let output = hermetic(&mut cmd)
        .arg("-C")
        .arg(dir)
        .args(["-c", &format!("user.name={name}"), "-c", &format!("user.email={email}")])
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

/// `Vendor.bsl` committed by the vendor author, `Own.bsl` by the developer.
fn setup_repo() -> TempDir {
    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();

    // Distinct content: identical files share one git blob, and corrupting it in
    // `corrupt_blame_source` would damage the developer's file as well.
    fs::write(root.join("Vendor.bsl"), format!("{BROKEN}// vendor\n")).expect("vendor file");
    run_git_as(root, VENDOR_NAME, VENDOR_EMAIL, &["init", "-q"]);
    run_git_as(root, VENDOR_NAME, VENDOR_EMAIL, &["add", "."]);
    run_git_as(root, VENDOR_NAME, VENDOR_EMAIL, &["commit", "-q", "-m", "vendor"]);

    fs::write(root.join("Own.bsl"), BROKEN).expect("own file");
    run_git_as(root, DEV_NAME, DEV_EMAIL, &["add", "."]);
    run_git_as(root, DEV_NAME, DEV_EMAIL, &["commit", "-q", "-m", "own module"]);

    temp
}

fn run_analyze(root: &Path, extra_args: &[&str]) -> (bool, String, String) {
    let (ok, sarif, _stdout, stderr) = run_analyze_verbose(root, extra_args, true);
    (ok, sarif, stderr)
}

/// `quiet` is a parameter because the operator-facing summary of the author
/// filter only exists without `-q`: a fixture that always passes `-q` cannot
/// observe whether that summary states the truth.
fn run_analyze_verbose(
    root: &Path,
    extra_args: &[&str],
    quiet: bool,
) -> (bool, String, String, String) {
    let output_dir = root.join("reports");
    fs::create_dir_all(&output_dir).expect("output dir");

    let mut command = Command::new(common::qualified_app());
    command.args(["analyze", "-s"]).arg(root).args(["-r", "sarif", "-o"]).arg(&output_dir);
    if quiet {
        command.arg("-q");
    }
    let output = command.args(extra_args).output().expect("run bsl-analyzer");

    let sarif = fs::read_to_string(output_dir.join("bsl-analyzer.sarif")).unwrap_or_default();
    (
        output.status.success(),
        sarif,
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// Overwrites the loose object backing `path` in HEAD with bytes that are not a
/// valid zlib object, leaving the commit and tree readable. Blame then fails for
/// that one file with a real read error — deleting the object instead would look
/// like "not in HEAD", which resolves to "keep everything" and never fails.
fn corrupt_blame_source(root: &Path, path: &str) {
    let mut command = Command::new("git");
    let output = hermetic(&mut command)
        .arg("-C")
        .arg(root)
        .args(["rev-parse", &format!("HEAD:{path}")])
        .output()
        .expect("run git rev-parse");
    assert!(output.status.success(), "git rev-parse failed for {path}");
    let object = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let (prefix, rest) = object.split_at(2);
    let loose = root.join(".git/objects").join(prefix).join(rest);
    assert!(loose.is_file(), "expected a loose object at {}", loose.display());
    // Loose objects are written read-only; replacing the file works both here and
    // under a CI user for whom the mode bits would not have applied anyway.
    fs::remove_file(&loose).expect("remove loose object");
    fs::write(&loose, b"not a zlib stream").expect("corrupt loose object");
}

fn reported_files(sarif: &str) -> (bool, bool) {
    (sarif.contains("Vendor.bsl"), sarif.contains("Own.bsl"))
}

#[test]
fn ignored_author_findings_are_dropped_and_own_findings_kept() {
    let temp = setup_repo();

    // Without the filter both broken modules are reported.
    let (ok, sarif, _) = run_analyze(temp.path(), &[]);
    assert!(ok);
    let (vendor, own) = reported_files(&sarif);
    assert!(vendor && own, "unfiltered run must report both files:\n{sarif}");

    let (ok, sarif, _) = run_analyze(temp.path(), &["--ignored-author", VENDOR_NAME]);
    assert!(ok);
    let (vendor, own) = reported_files(&sarif);
    assert!(!vendor, "vendor-authored findings must be dropped:\n{sarif}");
    assert!(own, "developer-authored findings must stay:\n{sarif}");
}

#[test]
fn matching_by_email_and_config_without_cli_flags() {
    let temp = setup_repo();
    fs::write(
        temp.path().join("bsl-analyzer.toml"),
        format!("[analysis]\nignored_authors = [\"{VENDOR_EMAIL}\"]\n"),
    )
    .expect("config");

    let (ok, sarif, _) = run_analyze(temp.path(), &[]);
    assert!(ok);
    let (vendor, own) = reported_files(&sarif);
    assert!(!vendor && own, "[analysis].ignored_authors must filter by email:\n{sarif}");
}

#[test]
fn cli_flag_replaces_the_config_list() {
    let temp = setup_repo();
    // The config ignores the developer; the CLI flag must replace (not merge
    // with) that list, so only vendor findings disappear.
    fs::write(
        temp.path().join("bsl-analyzer.toml"),
        format!("[analysis]\nignored_authors = [\"{DEV_NAME}\"]\n"),
    )
    .expect("config");

    let (ok, sarif, _) = run_analyze(temp.path(), &["--ignored-author", VENDOR_NAME]);
    assert!(ok);
    let (vendor, own) = reported_files(&sarif);
    assert!(!vendor, "CLI-listed vendor author must be filtered:\n{sarif}");
    assert!(own, "config-listed author must be overridden by the CLI list:\n{sarif}");
}

#[test]
fn untracked_and_uncommitted_code_is_never_filtered() {
    let temp = setup_repo();

    // Untracked broken module plus an uncommitted broken addition to the
    // vendor file: both are the user's own work regardless of blame history.
    fs::write(temp.path().join("New.bsl"), BROKEN).expect("untracked file");
    fs::write(temp.path().join("Vendor.bsl"), format!("{BROKEN}КонецПроцедуры\nПроцедура Ещё(\n"))
        .expect("uncommitted vendor edit");

    let (ok, sarif, _) = run_analyze(temp.path(), &["--ignored-author", VENDOR_NAME]);
    assert!(ok);
    assert!(sarif.contains("New.bsl"), "untracked file must not be filtered:\n{sarif}");
    assert!(
        sarif.contains("Vendor.bsl"),
        "findings on uncommitted lines must survive even in a vendor file:\n{sarif}"
    );
}

#[test]
fn shallow_clone_is_a_hard_error() {
    let temp = setup_repo();

    // A file:// clone honours --depth and produces a real shallow repository.
    let clones = TempDir::new().expect("clone dir");
    let clone_dir = clones.path().join("shallow");
    run_git_as(
        temp.path(),
        DEV_NAME,
        DEV_EMAIL,
        &[
            "clone",
            "-q",
            "--depth",
            "1",
            &format!("file://{}", temp.path().display()),
            clone_dir.to_str().expect("utf-8 path"),
        ],
    );

    let (ok, _, stderr) = run_analyze(&clone_dir, &["--ignored-author", VENDOR_NAME]);
    assert!(!ok, "author filtering on a shallow clone must fail the run");
    assert!(stderr.contains("unshallow"), "the error must point at the fix:\n{stderr}");
}

#[test]
fn author_filter_composes_with_git_diff_scope() {
    let temp = setup_repo();
    run_git_as(temp.path(), DEV_NAME, DEV_EMAIL, &["branch", "base"]);

    // Own.bsl gains a broken addition after the base branch point; Vendor.bsl
    // is untouched, so the scope alone already excludes it.
    fs::write(temp.path().join("Own.bsl"), format!("{BROKEN}КонецПроцедуры\nПроцедура Ещё(\n"))
        .expect("own edit");
    run_git_as(temp.path(), DEV_NAME, DEV_EMAIL, &["commit", "-aqm", "own change"]);

    let (ok, sarif, _) = run_analyze(
        temp.path(),
        &["--incremental", "--git-diff", "base", "--ignored-author", VENDOR_NAME],
    );
    assert!(ok);
    let (vendor, own) = reported_files(&sarif);
    assert!(!vendor, "out-of-scope vendor file must stay excluded:\n{sarif}");
    assert!(own, "in-scope developer change must be reported:\n{sarif}");
}

#[test]
fn baseline_classification_precedes_author_presentation_filter() {
    let temp = setup_repo();
    fs::write(
        temp.path().join("bsl-analyzer.toml"),
        "[diagnostics.baseline]\npath = \"baseline.json\"\n",
    )
    .unwrap();
    fs::write(
        temp.path().join("baseline.json"),
        r#"{
  "schema_version": 1,
  "scope": { "source_root": "", "extensions": [] },
  "diagnostics": []
}
"#,
    )
    .unwrap();

    let run = |extra: &[&str]| {
        let output = Command::new(common::qualified_app())
            .args(["analyze", "-s"])
            .arg(temp.path())
            .args(["--format", "jsonl"])
            .args(extra)
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>()
    };
    let unfiltered = run(&[]);
    let filtered = run(&["--ignored-author", VENDOR_NAME]);
    let unfiltered_done = unfiltered.iter().find(|event| event["type"] == "done").unwrap();
    let filtered_done = filtered.iter().find(|event| event["type"] == "done").unwrap();
    let findings = |events: &[serde_json::Value]| {
        events
            .iter()
            .filter(|event| event["type"] == "file")
            .map(|event| event["diagnostics"].as_array().unwrap().len())
            .sum::<usize>()
    };

    assert_eq!(unfiltered_done["baseline"]["new"], filtered_done["baseline"]["new"]);
    assert_eq!(filtered_done["baseline"]["state"], "partial");
    assert!(findings(&filtered) < findings(&unfiltered));
}

#[test]
fn blame_failure_aborts_the_run_instead_of_dropping_findings() {
    let temp = setup_repo();
    corrupt_blame_source(temp.path(), "Vendor.bsl");

    let (ok, sarif, _stdout, stderr) =
        run_analyze_verbose(temp.path(), &["--ignored-author", VENDOR_NAME], true);

    assert!(
        !ok,
        "a blame failure must fail the run: a report missing findings it could not \
         classify is worse than no report\nstderr: {stderr}\nsarif: {sarif}"
    );
    assert!(
        stderr.contains("author filter"),
        "the failure must name the author filter\nstderr: {stderr}"
    );
}

#[test]
fn author_filter_summary_states_the_counts_it_actually_dropped() {
    let temp = setup_repo();

    let (ok, sarif, stdout, stderr) =
        run_analyze_verbose(temp.path(), &["--ignored-author", VENDOR_NAME], false);
    assert!(ok, "stderr: {stderr}");
    let (vendor, own) = reported_files(&sarif);
    assert!(!vendor && own, "fixture must actually drop a finding:\n{sarif}");

    let summary = stdout
        .lines()
        .find(|line| line.starts_with("Author filter:"))
        .unwrap_or_else(|| panic!("no author filter summary in stdout:\n{stdout}"));
    assert!(
        !summary.contains("dropped 0 of 0"),
        "summary is read before the filter runs and reports nothing: {summary}"
    );
}

/// Runs `analyze` with the Code Quality reporter and pairs every finding with its
/// fingerprint, keyed by rule and line so the two runs can be matched up.
fn code_quality_findings(root: &Path, extra_args: &[&str]) -> Vec<((String, u64), String)> {
    let output_dir = root.join("reports");
    fs::create_dir_all(&output_dir).expect("output dir");
    let output = Command::new(common::qualified_app())
        .args(["analyze", "-s"])
        .arg(root)
        .args(["-r", "codequality", "-o"])
        .arg(&output_dir)
        .arg("-q")
        .args(extra_args)
        .output()
        .expect("run bsl-analyzer");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let report = fs::read_to_string(output_dir.join("gl-code-quality-report.json"))
        .expect("code quality report");
    serde_json::from_str::<Vec<serde_json::Value>>(&report)
        .expect("code quality report is a JSON array")
        .into_iter()
        .map(|issue| {
            let key = (
                issue["check_name"].as_str().expect("check_name").to_owned(),
                issue["location"]["lines"]["begin"].as_u64().expect("begin line"),
            );
            (key, issue["fingerprint"].as_str().expect("fingerprint").to_owned())
        })
        .collect()
}

/// The ordinal folded into a Code Quality fingerprint is counted over the FULL set,
/// before any filtering. A run that drops the vendor's finding must therefore leave
/// every survivor with the fingerprint it had in the unfiltered run — otherwise a
/// finding inherits the identity of one that was filtered out, and the merge-request
/// widget diffs it against the wrong entry.
#[test]
fn author_filtered_survivors_keep_their_code_quality_fingerprint() {
    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();

    // The vendor's self-assignment sits on its own source line, so it opens the
    // ordinal sequence; the developer's two are identical to each other and are
    // numbered 0 and 1 among themselves. Dropping the vendor's finding shifts every
    // later position by one, which is exactly what must not reach the ordinals.
    fs::write(root.join("Mixed.bsl"), "Процедура Тест()\n    А = 1;\n    А = А;\nКонецПроцедуры\n")
        .expect("vendor file");
    run_git_as(root, VENDOR_NAME, VENDOR_EMAIL, &["init", "-q"]);
    run_git_as(root, VENDOR_NAME, VENDOR_EMAIL, &["add", "."]);
    run_git_as(root, VENDOR_NAME, VENDOR_EMAIL, &["commit", "-q", "-m", "vendor"]);

    fs::write(
        root.join("Mixed.bsl"),
        "Процедура Тест()\n    А = 1;\n    А = А;\n    Б = 2;\n    Б = Б;\n    Б = Б;\nКонецПроцедуры\n",
    )
    .expect("developer edit");
    run_git_as(root, DEV_NAME, DEV_EMAIL, &["add", "."]);
    run_git_as(root, DEV_NAME, DEV_EMAIL, &["commit", "-q", "-m", "developer"]);

    let full = code_quality_findings(root, &[]);
    let filtered = code_quality_findings(root, &["--ignored-author", VENDOR_NAME]);

    // Without a drop the comparison below holds for any implementation.
    assert!(
        filtered.len() < full.len(),
        "the fixture must lose at least one finding to the author filter: {full:?} -> {filtered:?}"
    );
    assert!(filtered.len() >= 2, "two developer findings must survive: {filtered:?}");

    for (key, fingerprint) in &filtered {
        let before = full
            .iter()
            .find(|(full_key, _)| full_key == key)
            .unwrap_or_else(|| panic!("{key:?} survived the filter but is absent from {full:?}"));
        assert_eq!(
            *fingerprint, before.1,
            "{key:?} changed fingerprint once an earlier finding was filtered out"
        );
    }
}
