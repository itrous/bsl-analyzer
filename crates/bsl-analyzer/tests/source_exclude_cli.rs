use std::process::Command;

/// JSONL escapes a Windows separator as two backslashes.
fn forward_slashes(jsonl: String) -> String {
    jsonl.replace("\\\\", "/")
}

#[test]
fn analyze_command_excludes_hidden_sources_but_keeps_component_prefix_siblings() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for relative in ["Allowed.bsl", ".tmp/Hidden.bsl", ".tmp2/Visible.bsl"] {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "").unwrap();
    }
    std::fs::write(
        root.join("bsl-analyzer.toml"),
        "[source]\nroot = \".\"\nexclude = [\".tmp\"]\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_bsl-analyzer-app"))
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
        .unwrap();
    assert!(output.status.success(), "analyze failed: {}", String::from_utf8_lossy(&output.stderr));

    let stdout = forward_slashes(String::from_utf8(output.stdout).unwrap());
    let lines: Vec<_> = stdout.lines().collect();
    assert!(
        lines.first().is_some_and(|line| line.contains("\"total_files\":2")),
        "the command analyzed the wrong universe: {stdout}"
    );
    assert!(lines.iter().any(|line| line.contains("Allowed.bsl")), "{stdout}");
    assert!(lines.iter().any(|line| line.contains(".tmp2/Visible.bsl")), "{stdout}");
    assert!(lines.iter().all(|line| !line.contains(".tmp/Hidden.bsl")), "{stdout}");
}

#[test]
fn analyze_command_keeps_an_external_config_as_the_exclusion_base() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    let config_dir = project.join("ci");
    let source = project.join("src/cf");
    let other_cwd = dir.path().join("cwd");
    for path in [&config_dir, &source.join(".tmp"), &config_dir.join(".tmp"), &other_cwd] {
        std::fs::create_dir_all(path).unwrap();
    }
    std::fs::write(source.join("Configuration.xml"), "<Configuration/>").unwrap();
    std::fs::write(source.join("Allowed.bsl"), "").unwrap();
    std::fs::write(source.join(".tmp/StillVisible.bsl"), "").unwrap();
    std::fs::write(config_dir.join(".tmp/OutsideSource.bsl"), "").unwrap();
    let config = config_dir.join("bsl-analyzer.toml");
    std::fs::write(&config, "[source]\nroot = \"src/cf\"\nexclude = [\".tmp\"]\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_bsl-analyzer-app"))
        .current_dir(other_cwd)
        .args([
            "analyze",
            "--source-dir",
            project.to_str().unwrap(),
            "--workspace-dir",
            project.to_str().unwrap(),
            "--config",
            config.to_str().unwrap(),
            "--format",
            "jsonl",
            "--quiet",
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "analyze failed: {}", String::from_utf8_lossy(&output.stderr));

    let stdout = forward_slashes(String::from_utf8(output.stdout).unwrap());
    assert!(
        stdout.lines().next().is_some_and(|line| line.contains("\"total_files\":2")),
        "the command resolved exclusions from the source root, cwd, or ignored --config: {stdout}"
    );
    assert!(stdout.contains("Allowed.bsl"), "{stdout}");
    assert!(stdout.contains(".tmp/StillVisible.bsl"), "{stdout}");
    assert!(!stdout.contains("OutsideSource.bsl"), "{stdout}");
}

#[test]
fn analyze_command_does_not_restore_a_fallback_when_the_source_root_is_excluded() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("Configuration.xml"), "<Configuration/>").unwrap();
    std::fs::write(root.join("Hidden.bsl"), "Процедура Скрытая()\nКонецПроцедуры").unwrap();
    std::fs::write(root.join("bsl-analyzer.toml"), "[source]\nroot = \".\"\nexclude = [\".\"]\n")
        .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_bsl-analyzer-app"))
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
        .unwrap();
    assert!(output.status.success(), "analyze failed: {}", String::from_utf8_lossy(&output.stderr));

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.lines().next().is_some_and(|line| line.contains("\"total_files\":0")),
        "{stdout}"
    );
    assert!(!stdout.contains("Hidden.bsl"), "{stdout}");
}
