//! Protocol acceptance uses isolated XDG directories and the real CLI entrypoint.
// macOS resolves the journal root under `~/Library`, not from XDG variables.
#![cfg(all(unix, not(target_os = "macos")))]
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    process::{Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

/// Bound of a wait for an event that a regression would never deliver; a hang watchdog,
/// not a latency measure.
const HANG_WATCHDOG: Duration = test_utils::wait::ceiling(Duration::from_secs(60));

fn command(root: &std::path::Path, profile: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_bsl-analyzer-app"));
    command
        .args(["mcp", "serve", "--profile", profile, "--mode", "stdio"])
        .env("HOME", root.join("home"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("BSL_MCP_BROKER", "0")
        .env("BSL_LOG", "warn")
        .env_remove("BSL_LOG_FILE")
        .env_remove("BSL_ONEC_CONNECTIONS")
        .env_remove("BSL_ONEC_PASSWORD")
        .env_remove("BSL_PROFILE")
        .env_remove("BSL_PROFILE_JSON");
    command
}

#[test]
fn vector_journal_failure_keeps_stdout_protocol_only() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("home")).unwrap();
    // State path is deliberately a file: the sink must fail without stopping MCP.
    fs::write(root.path().join("state"), b"preserve").unwrap();
    let mut child = command(root.path(), "reference")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let (sender, receiver) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            sender.send(line.unwrap()).unwrap();
        }
    });
    let stderr = child.stderr.take().unwrap();
    let (diagnosed, observed) = mpsc::channel();
    let diagnostics = std::thread::spawn(move || {
        let mut text = String::new();
        for line in BufReader::new(stderr).lines() {
            let line = line.unwrap();
            if line.contains("vector journal unavailable") {
                let _ = diagnosed.send(());
            }
            text.push_str(&line);
            text.push('\n');
        }
        text
    });
    writeln!(stdin, "{}", serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
        "protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"journal-fixture","version":"1"}
    }})).unwrap();
    stdin.flush().unwrap();
    let first = receiver
        .recv_timeout(test_utils::wait::ceiling(Duration::from_secs(20)))
        .expect("initialize response");
    let response: serde_json::Value =
        serde_json::from_str(&first).expect("stdout contains protocol JSON only");
    assert_eq!(response["id"], 1);
    assert!(response.get("result").is_some(), "{response}");
    writeln!(
        stdin,
        "{}",
        serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized"})
    )
    .unwrap();
    writeln!(
        stdin,
        "{}",
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}})
    )
    .unwrap();
    stdin.flush().unwrap();
    let second = receiver
        .recv_timeout(test_utils::wait::ceiling(Duration::from_secs(20)))
        .expect("tools response");
    let response: serde_json::Value =
        serde_json::from_str(&second).expect("stdout contains protocol JSON only");
    assert_eq!(response["id"], 2);
    assert!(response["result"]["tools"].is_array());
    observed.recv_timeout(HANG_WATCHDOG).expect("journal failure diagnostic");
    // This fixture tests a live protocol exchange. MCP transport teardown is
    // independent; JournalGuard's bounded graceful drain is exercised in unit tests.
    child.kill().unwrap();
    child.wait().unwrap();
    drop(stdin);
    reader.join().unwrap();
    for line in receiver.try_iter() {
        serde_json::from_str::<serde_json::Value>(&line).expect("protocol-only trailing stdout");
    }
    let diagnostic = diagnostics.join().unwrap();
    assert!(diagnostic.contains("vector journal unavailable"), "{diagnostic}");
    assert_eq!(diagnostic.matches("vector journal unavailable").count(), 1);
    assert_eq!(fs::read(root.path().join("state")).unwrap(), b"preserve");
}

#[test]
fn invalid_serve_arguments_do_not_create_a_journal() {
    let root = tempfile::tempdir().unwrap();
    let output = command(root.path(), "reference").args(["--port", "10001"]).output().unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(!root.path().join("state").exists());
}

#[test]
fn workspace_startup_journal_survives_real_cli_restart() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("home")).unwrap();
    let source = root.path().join("source");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("Module.bsl"), "Процедура Пример()\nКонецПроцедуры\n").unwrap();
    let key = blake3::hash(source.canonicalize().unwrap().as_os_str().as_encoded_bytes());
    let journal = root.path().join("state/bsl-analyzer/vector-journal").join(key.to_hex().as_str());
    let mut processes: Vec<String> = Vec::new();
    for _ in 0..2 {
        let mut child = command(root.path(), "workspace")
            .arg("--source-dir")
            .arg(&source)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        let deadline = Instant::now() + test_utils::wait::ceiling(Duration::from_secs(20));
        let observed = loop {
            let mut records = Vec::new();
            for slot in 0..8 {
                if let Ok(bytes) = fs::read(journal.join(format!("{slot}.jsonl"))) {
                    // A live writer can expose an incomplete final line.
                    for line in bytes.split_inclusive(|byte| *byte == b'\n') {
                        if line.last() == Some(&b'\n') {
                            records
                                .push(serde_json::from_slice::<serde_json::Value>(line).unwrap());
                        }
                    }
                }
            }
            if records
                .iter()
                .any(|record| record["kind"] == "startup_snapshot" && record["pid"] == pid)
            {
                break records;
            }
            if Instant::now() >= deadline || child.try_wait().unwrap().is_some() {
                break records;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let _ = child.kill();
        child.wait().unwrap();
        let snapshot = observed
            .iter()
            .find(|record| record["kind"] == "startup_snapshot" && record["pid"] == pid)
            .expect(
                "real workspace startup must persist a lifecycle snapshot at broad BSL_LOG=warn",
            );
        assert_eq!(snapshot["event_version"], 1);
        let process = snapshot["process_id"].as_str().unwrap().to_owned();
        assert!(!processes.contains(&process));
        for previous in &processes {
            assert!(
                observed.iter().any(|record| record["process_id"] == previous.as_str()),
                "restart must retain previous process records"
            );
        }
        processes.push(process);
    }
}
