use std::path::Path;
use std::process::Command;

/// Text that is not a module: every file holding it yields findings, so a file
/// left out of the analysis is told apart by its absence from the output.
const NOT_A_MODULE: &str =
    "ВЫБРАТЬ\n\tТаблица.Поле КАК Поле\nИЗ\n\tСправочник.Таблица КАК Таблица\n";

const FILES: [&str; 6] = [
    "Allowed.bsl",
    "temp/Hidden.bsl",
    ".tool/Hidden.bsl",
    "Reports/R/Templates/T/Ext/Template.Queries.bsl",
    "Reports/R/Ext/ObjectModule.bsl",
    "temporary/Visible.bsl",
];

fn project(config: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for relative in FILES {
        let path = dir.path().join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, NOT_A_MODULE).unwrap();
    }
    std::fs::write(dir.path().join("bsl-analyzer.toml"), config).unwrap();
    dir
}

fn analyze(root: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_bsl-analyzer-app"))
        .args([
            "analyze",
            "--source-dir",
            root.to_str().unwrap(),
            "--workspace-dir",
            root.to_str().unwrap(),
            "--format",
            "jsonl",
            "--quiet",
        ])
        .output()
        .unwrap()
}

/// The files of `FILES` that have at least one finding in the output.
/// Paths are compared with `/` separators: the JSON escapes a Windows separator.
fn files_with_findings(stdout: &str) -> Vec<&'static str> {
    let lines: Vec<String> = stdout
        .lines()
        .filter(|line| line.contains("\"type\":\"file\"") && line.contains("\"code\""))
        .map(|line| line.replace("\\\\", "/"))
        .collect();
    FILES
        .into_iter()
        .filter(|file| lines.iter().any(|line| line.contains(&format!("/{file}\""))))
        .collect()
}

#[test]
fn analyze_command_leaves_out_files_matched_by_exclude_globs() {
    let dir = project(
        "[source]\nroot = \".\"\nexclude_globs = [\"temp\", \".tool/**\", \"**/*.Queries.bsl\"]\n",
    );

    let output = analyze(dir.path());
    assert!(output.status.success(), "analyze failed: {}", String::from_utf8_lossy(&output.stderr));

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.lines().next().is_some_and(|line| line.contains("\"total_files\":3")),
        "the command analyzed the wrong universe: {stdout}"
    );
    assert_eq!(
        files_with_findings(&stdout),
        ["Allowed.bsl", "Reports/R/Ext/ObjectModule.bsl", "temporary/Visible.bsl"],
        "{stdout}"
    );
}

#[test]
fn analyze_command_without_exclude_globs_analyzes_every_file() {
    let dir = project("[source]\nroot = \".\"\n");

    let output = analyze(dir.path());
    assert!(output.status.success(), "analyze failed: {}", String::from_utf8_lossy(&output.stderr));

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.lines().next().is_some_and(|line| line.contains("\"total_files\":6")),
        "{stdout}"
    );
    assert_eq!(files_with_findings(&stdout), FILES, "{stdout}");
}

#[test]
fn analyze_command_refuses_an_unusable_exclude_glob() {
    let dir = project("[source]\nroot = \".\"\nexclude_globs = [\"Reports/[\"]\n");

    let output = analyze(dir.path());
    assert!(!output.status.success(), "an unusable pattern was silently dropped");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("source.exclude_globs") && stderr.contains("Reports/["),
        "the error does not name the key and the pattern: {stderr}"
    );
}
