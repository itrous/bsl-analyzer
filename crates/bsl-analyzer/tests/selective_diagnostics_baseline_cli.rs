mod common;

use std::{fs, path::Path, process::Command};

use tempfile::TempDir;

const BROKEN: &str = "Процедура Тест(\n";

fn run(root: &Path, args: &[&str]) -> std::process::Output {
    Command::new(common::qualified_app())
        .current_dir(root)
        .args(args)
        .output()
        .expect("run bsl-analyzer")
}

fn json(root: &Path, args: &[&str]) -> serde_json::Value {
    let output = run(root, args);
    assert!(
        output.status.success() || !output.stdout.is_empty(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn setup(include: &[&str]) -> TempDir {
    let temp = tempfile::tempdir().unwrap();
    for root in ["src/cf", "src/cfe/Ext"] {
        fs::create_dir_all(temp.path().join(root)).unwrap();
        fs::write(temp.path().join(root).join("Configuration.xml"), "<Configuration/>").unwrap();
    }
    fs::write(temp.path().join("src/cf/Main.bsl"), BROKEN).unwrap();
    fs::write(temp.path().join("src/cfe/Ext/Ext.bsl"), BROKEN).unwrap();
    write_config(temp.path(), include, "");
    temp
}

fn write_config(root: &Path, include: &[&str], extra: &str) {
    let include = include.iter().map(|id| format!(r#""{id}""#)).collect::<Vec<_>>().join(", ");
    fs::write(
        root.join("bsl-analyzer.toml"),
        format!(
            r#"[source]
root = "src/cf"
extensions = [{{ name = "Ext", path = "src/cfe/Ext", scope = "InfoBase" }}]

[diagnostics.baseline]
directory = "baselines"
include = [{include}]

{extra}"#
        ),
    )
    .unwrap();
}

fn baseline(root: &Path, action: &str, extra: &[&str]) -> std::process::Output {
    let mut args = vec!["diagnostics", "baseline", action, "-s", ".", "--format", "json"];
    args.extend_from_slice(extra);
    run(root, &args)
}

fn manifest(root: &Path) -> serde_json::Value {
    serde_json::from_slice(&fs::read(root.join("baselines/manifest.json")).unwrap()).unwrap()
}

#[test]
fn documented_selective_usage() {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    for relative in [
        "docs/configuration/DIAGNOSTICS.md",
        "docs/configuration/PROJECT_CONFIGURATION.md",
        "docs/mcp/TOOLS_AND_EXTENSION.md",
        "docs/CI_REPORTERS.md",
    ] {
        let document = fs::read_to_string(repository.join(relative)).unwrap();
        assert!(document.contains("include"), "{relative} omits include");
        assert!(document.contains("unsuppressed"), "{relative} omits policy semantics");
    }
    let help = run(&repository, &["diagnostics", "--help"]);
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("selected owners"));
}

#[test]
fn check_config_cli_accepts_selective_include_and_reports_policy_partitions() {
    let temp = setup(&["main"]);
    assert!(baseline(temp.path(), "create", &[]).status.success());

    let output = run(temp.path(), &["check-config", "--config", "bsl-analyzer.toml"]);
    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Configuration is valid."), "{stdout}");
    assert!(stdout.contains("selection selective"), "{stdout}");
    assert!(stdout.contains("enabled partitions: main"), "{stdout}");
    assert!(stdout.contains("unsuppressed partitions: extension:Ext"), "{stdout}");
}

#[test]
fn check_config_cli_rejects_invalid_include_before_baseline_io() {
    let temp = setup(&["extension:Missing"]);

    let output = run(temp.path(), &["check-config", "--config", "bsl-analyzer.toml"]);
    assert!(!output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stdout.contains("Configuration is invalid."), "{stdout}");
    assert!(
        stderr.contains(
            "configuration is invalid: invalid diagnostics baseline config: include references unknown partition selector: extension:Missing"
        ),
        "{stderr}"
    );
    assert!(!stdout.contains("file is missing"), "{stdout}");
    assert!(!temp.path().join("baselines").exists());
}

/// Drop the ambient git environment. When the suite runs inside a `git commit` pre-commit
/// hook, GIT_DIR / GIT_INDEX_FILE point at the real repository and win over the fixture
/// directory: the temporary project would be staged into the developer's own index, and
/// its blobs would die with the temporary directory, leaving that index unable to build a
/// tree.
fn hermetic(command: &mut Command) -> &mut Command {
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("GIT_") {
            command.env_remove(&key);
        }
    }
    command
}

fn init_git(root: &Path) {
    for args in [
        &["init", "-q"][..],
        &["config", "user.email", "test@example.com"],
        &["config", "user.name", "Test"],
        &["add", "."],
        &["commit", "-qm", "fixture"],
    ] {
        let mut command = Command::new("git");
        assert!(hermetic(&mut command).current_dir(root).args(args).status().unwrap().success());
    }
}

#[test]
fn selective_cli_create_publishes_only_enabled_partitions_atomically() {
    let temp = setup(&["main"]);
    let created =
        json(temp.path(), &["diagnostics", "baseline", "create", "-s", ".", "--format", "json"]);
    let entries = manifest(temp.path())["partitions"].as_array().unwrap().clone();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["partition_id"], "main");
    assert_eq!(created["selection"], "selective");
    assert_eq!(created["partitions_enabled"], 1);
    assert_eq!(created["partitions_unsuppressed"], 1);
    assert!(temp.path().join("baselines").join(entries[0]["file"].as_str().unwrap()).is_file());
}

#[test]
fn selective_cli_check_ignores_intentional_unsuppressed_drift() {
    let temp = setup(&["main"]);
    assert!(baseline(temp.path(), "create", &[]).status.success());
    let before = fs::read(temp.path().join("baselines/manifest.json")).unwrap();
    fs::write(temp.path().join("src/cfe/Ext/New.bsl"), BROKEN).unwrap();
    let checked =
        json(temp.path(), &["diagnostics", "baseline", "check", "-s", ".", "--format", "json"]);
    assert_eq!(checked["success"], true);
    assert!(checked["unsuppressed"].as_u64().unwrap() > 0);
    assert_eq!(fs::read(temp.path().join("baselines/manifest.json")).unwrap(), before);
}

#[test]
fn selective_cli_selected_operations_require_global_full_coverage() {
    let temp = setup(&["main"]);
    assert!(baseline(temp.path(), "create", &[]).status.success());
    init_git(temp.path());
    write_config(temp.path(), &["main"], "[analysis]\nignored_authors = [\"nobody\"]\n");
    for action in ["check", "update"] {
        let output = baseline(temp.path(), action, &["--partition", "main"]);
        assert!(!output.status.success(), "{action} unexpectedly succeeded");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("full diagnostics coverage required"), "{action}: {stderr}");
    }
}

#[test]
fn selective_cli_all_selected_policy_matrix() {
    let temp = setup(&["main"]);
    assert!(baseline(temp.path(), "create", &[]).status.success());

    let main = json(
        temp.path(),
        &["diagnostics", "baseline", "check", "-s", ".", "--partition", "main", "--format", "json"],
    );
    assert_eq!(main["success"], true);
    assert_eq!(main["partitions"].as_array().unwrap().len(), 1);

    let ext = json(
        temp.path(),
        &[
            "diagnostics",
            "baseline",
            "check",
            "-s",
            ".",
            "--partition",
            "extension:Ext",
            "--format",
            "json",
        ],
    );
    assert_eq!(ext["success"], true);
    assert!(ext["unsuppressed"].as_u64().unwrap() > 0);
    assert_eq!(ext["partitions"].as_array().unwrap().len(), 1);
    assert_eq!(ext["partitions"][0]["policy"], "unsuppressed");

    let before = fs::read(temp.path().join("baselines/manifest.json")).unwrap();
    for action in ["create", "update"] {
        let output = baseline(temp.path(), action, &["--partition", "extension:Ext"]);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("partition_unsuppressed"));
    }
    assert_eq!(fs::read(temp.path().join("baselines/manifest.json")).unwrap(), before);
    assert!(baseline(temp.path(), "update", &["--partition", "main"]).status.success());
}

#[test]
fn selective_cli_create_selected_missing_entry_accepts_only_selected_owner() {
    let temp = setup(&["main"]);
    assert!(baseline(temp.path(), "create", &[]).status.success());
    let main_before = manifest(temp.path())["partitions"][0]["file"].as_str().unwrap().to_owned();

    write_config(temp.path(), &["main", "extension:Ext"], "");
    let accepted = json(
        temp.path(),
        &[
            "diagnostics",
            "baseline",
            "create",
            "-s",
            ".",
            "--partition",
            "extension:Ext",
            "--format",
            "json",
        ],
    );
    assert!(accepted["added"].as_u64().unwrap() > 0);
    assert!(accepted["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .all(|entry| entry["path"].as_str().unwrap().starts_with("src/cfe/Ext/")));
    let entries = manifest(temp.path())["partitions"].as_array().unwrap().clone();
    assert_eq!(entries.len(), 2);
    assert_eq!(
        entries.iter().find(|entry| entry["partition_id"] == "main").unwrap()["file"],
        main_before
    );
}

#[test]
fn selective_cli_missing_entry_validates_enabled_siblings_before_publish() {
    let temp = setup(&["main"]);
    assert!(baseline(temp.path(), "create", &[]).status.success());
    let before = fs::read(temp.path().join("baselines/manifest.json")).unwrap();
    let current = manifest(temp.path());
    let main = current["partitions"][0]["file"].as_str().unwrap();
    fs::write(temp.path().join("baselines").join(main), b"corrupt sibling").unwrap();
    write_config(temp.path(), &["main", "extension:Ext"], "");

    let output = baseline(temp.path(), "create", &["--partition", "extension:Ext"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("object is corrupt"));
    assert_eq!(fs::read(temp.path().join("baselines/manifest.json")).unwrap(), before);
}

#[test]
fn selective_cli_repair_preserves_no_acceptance_contract() {
    let temp = setup(&["main", "extension:Ext"]);
    assert!(baseline(temp.path(), "create", &[]).status.success());
    let before = manifest(temp.path());
    let ext = before["partitions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["partition_id"] == "extension:Ext")
        .unwrap()
        .clone();
    fs::write(temp.path().join("baselines").join(ext["file"].as_str().unwrap()), b"corrupt")
        .unwrap();
    fs::write(temp.path().join("src/cfe/Ext/New.bsl"), BROKEN).unwrap();

    let output = baseline(temp.path(), "create", &["--partition", "extension:Ext"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("diagnostics baseline object is corrupt"), "{stderr}");
    assert_eq!(manifest(temp.path())["generation"], before["generation"]);
}

/// Repairing a damaged enabled object must not take the dormant ones with it:
/// dropping `include` is documented as the way back to a full set, and that
/// promise only holds while their objects stay on disk.
#[test]
fn repairing_a_damaged_object_keeps_dormant_partition_objects() {
    let temp = setup(&["main"]);
    let root = temp.path();
    // A full set first: `include` is what makes a partition dormant, so the
    // fixture has to publish every partition before adopting a selection.
    fs::write(
        root.join("bsl-analyzer.toml"),
        r#"[source]
root = "src/cf"
extensions = [{ name = "Ext", path = "src/cfe/Ext", scope = "InfoBase" }]

[diagnostics.baseline]
directory = "baselines"
"#,
    )
    .unwrap();
    let created = baseline(root, "create", &[]);
    assert!(created.status.success(), "{}", String::from_utf8_lossy(&created.stderr));

    let dormant: Vec<_> = walk_objects(&root.join("baselines/objects/extensions"));
    assert!(!dormant.is_empty(), "fixture must publish an extension object");

    write_config(root, &["main"], "");
    for object in walk_objects(&root.join("baselines/objects/main")) {
        fs::remove_file(&object).unwrap();
    }

    let output = baseline(root, "update", &[]);
    assert!(
        output.status.success(),
        "repair run failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        result["operation"], "rebuilt",
        "a run that could not read the previous set must not report an update: {result}"
    );
    assert_eq!(result["removed"], 0);

    for object in &dormant {
        assert!(
            object.is_file(),
            "repair deleted a dormant partition object the user still owns: {}",
            object.display()
        );
    }
}

fn walk_objects(directory: &Path) -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = fs::read_dir(directory) else { return found };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.is_dir() {
            found.extend(walk_objects(&path));
        } else if path.extension().and_then(|value| value.to_str()) == Some("json") {
            found.push(path);
        }
    }
    found
}
