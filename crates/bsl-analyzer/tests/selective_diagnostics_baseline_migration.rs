mod common;

use std::{fs, path::Path, process::Command};

use tempfile::TempDir;

const BROKEN: &str = "Процедура Тест(\n";

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

fn run(root: &Path, args: &[&str]) -> std::process::Output {
    Command::new(common::qualified_app())
        .current_dir(root)
        .args(args)
        .output()
        .expect("run bsl-analyzer")
}

fn setup() -> TempDir {
    let temp = tempfile::tempdir().unwrap();
    for root in ["src/cf", "src/cfe/Ext"] {
        fs::create_dir_all(temp.path().join(root)).unwrap();
        fs::write(temp.path().join(root).join("Configuration.xml"), "<Configuration/>").unwrap();
    }
    fs::write(temp.path().join("src/cf/Main.bsl"), BROKEN).unwrap();
    fs::write(temp.path().join("src/cfe/Ext/Ext.bsl"), BROKEN).unwrap();
    write_config(temp.path(), false);
    assert!(run(temp.path(), &["diagnostics", "baseline", "create", "-s", "."]).status.success());
    temp
}

fn write_config(root: &Path, partitioned: bool) {
    let baseline = if partitioned {
        "directory = \"baselines\"\ninclude = [\"main\"]"
    } else {
        "path = \"legacy.json\""
    };
    fs::write(
        root.join("bsl-analyzer.toml"),
        format!(
            r#"[source]
root = "src/cf"
extensions = [{{ name = "Ext", path = "src/cfe/Ext", scope = "InfoBase" }}]

[diagnostics.baseline]
{baseline}
"#
        ),
    )
    .unwrap();
}

fn write_partitioned_config(root: &Path, extensions: &str, include: Option<&[&str]>) {
    let include = include.map_or_else(String::new, |ids| {
        format!(
            "include = [{}]\n",
            ids.iter().map(|id| format!(r#""{id}""#)).collect::<Vec<_>>().join(", ")
        )
    });
    fs::write(
        root.join("bsl-analyzer.toml"),
        format!(
            r#"[source]
root = "src/cf"
extensions = [{extensions}]

[diagnostics.baseline]
directory = "baselines"
{include}"#
        ),
    )
    .unwrap();
}

fn migrate(root: &Path) -> std::process::Output {
    run(
        root,
        &[
            "diagnostics",
            "baseline",
            "create",
            "-s",
            ".",
            "--from-v1",
            "legacy.json",
            "--format",
            "json",
        ],
    )
}

#[test]
fn selective_v1_migration_streams_enabled_entries_and_preserves_source() {
    let temp = setup();
    let root = temp.path();
    let source = fs::read(root.join("legacy.json")).unwrap();
    let legacy: serde_json::Value = serde_json::from_slice(&source).unwrap();
    let migrated = legacy["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|entry| entry["path"].as_str().unwrap().starts_with("src/cf/"))
        .count();
    let skipped = legacy["diagnostics"].as_array().unwrap().len() - migrated;
    fs::write(root.join("src/cf/New.bsl"), BROKEN).unwrap();
    write_config(root, true);

    let output = migrate(root);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["added"], migrated);
    assert_eq!(result["skipped_unsuppressed"], skipped);
    assert_eq!(fs::read(root.join("legacy.json")).unwrap(), source);
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("baselines/manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["partitions"].as_array().unwrap().len(), 1);
    assert_eq!(manifest["partitions"][0]["partition_id"], "main");

    let checked = run(
        root,
        &["diagnostics", "baseline", "check", "-s", ".", "--partition", "main", "--format", "json"],
    );
    let checked: serde_json::Value = serde_json::from_slice(&checked.stdout).unwrap();
    assert!(checked["added"].as_u64().unwrap() > 0, "current new diagnostics were accepted");
}

/// `./legacy.json` is the spelling a shell completes to; rejecting it as an invalid
/// managed path made the documented command fail on a file that is right there.
#[test]
fn selective_v1_migration_accepts_a_dot_slash_source() {
    let temp = setup();
    let root = temp.path();
    write_config(root, true);
    let output = run(
        root,
        &[
            "diagnostics",
            "baseline",
            "create",
            "-s",
            ".",
            "--from-v1",
            "./legacy.json",
            "--format",
            "json",
        ],
    );
    assert!(
        output.status.success(),
        "./legacy.json must be accepted:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Migration only rewrites the v1 file into partition objects — no current diagnostic
/// takes part — so an analysis narrowed by `[analysis]` filters cannot make its result
/// wrong. Refusing there would lock the format change away from exactly the projects
/// that have a large baseline to migrate.
#[test]
fn selective_v1_migration_runs_under_analysis_filters() {
    let temp = setup();
    let root = temp.path();
    // The set is written to a directory; the legacy file stays as the migration source.
    write_config(root, true);
    let config = fs::read_to_string(root.join("bsl-analyzer.toml")).unwrap();
    fs::write(
        root.join("bsl-analyzer.toml"),
        format!("{config}\n[analysis]\nignored_authors = [\"Vendor\"]\n"),
    )
    .unwrap();
    // The author filter needs a repository to blame against.
    for args in [
        vec!["init", "-q"],
        vec!["-c", "user.name=T", "-c", "user.email=t@e", "add", "."],
        vec!["-c", "user.name=T", "-c", "user.email=t@e", "commit", "-q", "-m", "fixture"],
    ] {
        let mut command = Command::new("git");
        let status =
            hermetic(&mut command).arg("-C").arg(root).args(&args).status().expect("run git");
        assert!(status.success(), "git {args:?} failed");
    }

    let output = migrate(root);
    assert!(
        output.status.success(),
        "migration must not require full coverage:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["operation"], "created", "{result}");
}

#[test]
fn selective_cli_rejects_from_v1_with_partition() {
    let temp = setup();
    write_config(temp.path(), true);
    let output = run(
        temp.path(),
        &[
            "diagnostics",
            "baseline",
            "create",
            "-s",
            ".",
            "--from-v1",
            "legacy.json",
            "--partition",
            "main",
        ],
    );
    assert!(!output.status.success());
    assert!(!temp.path().join("baselines/manifest.json").exists());
}

#[test]
fn selective_v1_migration_tracks_uniqueness_only_for_enabled_entries() {
    let temp = setup();
    let root = temp.path();
    let mut legacy: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("legacy.json")).unwrap()).unwrap();
    let skipped = legacy["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["path"].as_str().unwrap().starts_with("src/cfe/Ext/"))
        .unwrap()
        .clone();
    legacy["diagnostics"].as_array_mut().unwrap().push(skipped);
    fs::write(root.join("legacy.json"), serde_json::to_vec(&legacy).unwrap()).unwrap();
    write_config(root, true);
    let output = migrate(root);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(result["skipped_unsuppressed"].as_u64().unwrap() >= 2);

    let temp = setup();
    let root = temp.path();
    let mut legacy: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("legacy.json")).unwrap()).unwrap();
    let enabled = legacy["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["path"].as_str().unwrap().starts_with("src/cf/"))
        .unwrap()
        .clone();
    legacy["diagnostics"].as_array_mut().unwrap().push(enabled);
    fs::write(root.join("legacy.json"), serde_json::to_vec(&legacy).unwrap()).unwrap();
    write_config(root, true);
    let output = migrate(root);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("duplicate diagnostic"), "{stderr}");
}

#[test]
fn selective_existing_full_manifest_needs_no_file_migration() {
    let temp = setup();
    let root = temp.path();
    write_partitioned_config(
        root,
        r#"{ name = "Ext", path = "src/cfe/Ext", scope = "InfoBase" }"#,
        None,
    );
    assert!(run(root, &["diagnostics", "baseline", "create", "-s", "."]).status.success());
    let before: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("baselines/manifest.json")).unwrap()).unwrap();
    let dormant = before["partitions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["partition_id"] == "extension:Ext")
        .unwrap()
        .clone();
    let dormant_path = root.join("baselines").join(dormant["file"].as_str().unwrap());
    fs::write(&dormant_path, b"corrupt but dormant").unwrap();
    write_partitioned_config(
        root,
        r#"{ name = "Ext", path = "src/cfe/Ext", scope = "InfoBase" }"#,
        Some(&["main"]),
    );

    assert!(run(root, &["diagnostics", "baseline", "check", "-s", "."]).status.success());
    assert!(run(root, &["diagnostics", "baseline", "update", "-s", "."]).status.success());
    let after: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("baselines/manifest.json")).unwrap()).unwrap();
    assert_eq!(
        after["partitions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["partition_id"] == "extension:Ext")
            .unwrap(),
        &dormant
    );
    assert_eq!(fs::read(dormant_path).unwrap(), b"corrupt but dormant");
}

#[test]
fn selective_full_update_reconciles_topology_and_prunes_dormant_metadata() {
    let temp = setup();
    let root = temp.path();
    write_partitioned_config(
        root,
        r#"{ name = "Ext", path = "src/cfe/Ext", scope = "InfoBase" }"#,
        None,
    );
    assert!(run(root, &["diagnostics", "baseline", "create", "-s", "."]).status.success());
    let before = fs::read(root.join("baselines/manifest.json")).unwrap();
    let old: serde_json::Value = serde_json::from_slice(&before).unwrap();
    let old_ext = old["partitions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["partition_id"] == "extension:Ext")
        .unwrap()["file"]
        .as_str()
        .unwrap()
        .to_owned();
    fs::create_dir_all(root.join("src/cfe/NewExt")).unwrap();
    fs::write(root.join("src/cfe/NewExt/Configuration.xml"), "<Configuration/>").unwrap();
    fs::write(root.join("src/cfe/NewExt/New.bsl"), BROKEN).unwrap();
    write_partitioned_config(
        root,
        r#"{ name = "Ext", path = "src/cfe/Ext", scope = "InfoBase" }, { name = "NewExt", path = "src/cfe/NewExt", scope = "InfoBase" }"#,
        Some(&["main", "extension:NewExt"]),
    );

    assert!(!run(root, &["diagnostics", "baseline", "check", "-s", "."]).status.success());
    assert!(!run(root, &["diagnostics", "baseline", "update", "-s", ".", "--partition", "main"])
        .status
        .success());
    assert_eq!(fs::read(root.join("baselines/manifest.json")).unwrap(), before);
    let updated = run(root, &["diagnostics", "baseline", "update", "-s", ".", "--format", "json"]);
    assert!(updated.status.success(), "{}", String::from_utf8_lossy(&updated.stderr));
    let result: serde_json::Value = serde_json::from_slice(&updated.stdout).unwrap();
    assert!(result["unsuppressed"].as_u64().unwrap() > 0);
    let after: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("baselines/manifest.json")).unwrap()).unwrap();
    let ids = after["partitions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["partition_id"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(ids, ["main", "extension:NewExt"]);
    assert!(root.join("baselines").join(old_ext).exists());
}

#[test]
fn selective_reenable_is_fail_closed_until_explicit_creation_or_repair() {
    let temp = setup();
    let root = temp.path();
    write_partitioned_config(
        root,
        r#"{ name = "Ext", path = "src/cfe/Ext", scope = "InfoBase" }"#,
        Some(&["main"]),
    );
    assert!(run(root, &["diagnostics", "baseline", "create", "-s", "."]).status.success());
    write_partitioned_config(
        root,
        r#"{ name = "Ext", path = "src/cfe/Ext", scope = "InfoBase" }"#,
        Some(&["main", "extension:Ext"]),
    );
    assert!(!run(root, &["diagnostics", "baseline", "check", "-s", "."]).status.success());
    assert!(run(
        root,
        &["diagnostics", "baseline", "create", "-s", ".", "--partition", "extension:Ext",],
    )
    .status
    .success());

    let temp = setup();
    let root = temp.path();
    write_partitioned_config(
        root,
        r#"{ name = "Ext", path = "src/cfe/Ext", scope = "InfoBase" }"#,
        None,
    );
    assert!(run(root, &["diagnostics", "baseline", "create", "-s", "."]).status.success());
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("baselines/manifest.json")).unwrap()).unwrap();
    let ext = manifest["partitions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["partition_id"] == "extension:Ext")
        .unwrap();
    let object = root.join("baselines").join(ext["file"].as_str().unwrap());
    fs::write(&object, b"corrupt").unwrap();
    write_partitioned_config(
        root,
        r#"{ name = "Ext", path = "src/cfe/Ext", scope = "InfoBase" }"#,
        Some(&["main", "extension:Ext"]),
    );
    assert!(!run(root, &["diagnostics", "baseline", "check", "-s", "."]).status.success());
    let repaired = run(
        root,
        &["diagnostics", "baseline", "create", "-s", ".", "--partition", "extension:Ext"],
    );
    assert!(repaired.status.success(), "{}", String::from_utf8_lossy(&repaired.stderr));
    assert_eq!(blake3::hash(&fs::read(object).unwrap()).to_hex().as_str(), ext["blake3"]);
}
