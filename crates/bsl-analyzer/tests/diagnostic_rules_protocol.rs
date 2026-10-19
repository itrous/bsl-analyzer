mod common;
#[path = "fixtures/diagnostic_rules/mod.rs"]
mod rules;

use common::Lsp;
use serde_json::{json, Value};
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

/// A key the analyzer does not know: its warning comes only from loading the reloaded
/// config, since the startup config never names it.
const RELOAD_KEY: &str = "ReloadMarker = true\n";
const RELOAD_WARNING: &str = "unknown diagnostic code `ReloadMarker`";

/// Each config load logs its warning once, and a backend that is starting (or replaced by a
/// reload) still loads the config a few times after it answers its first pull, so the count
/// has no stable sampling point. A load per pull cannot hide in that noise: it grows by at
/// least one for every pull, so this many pulls outgrow any startup sequence (at most 5 loads
/// per backend, 8 with an in-place reload before a replacement).
const PULLS_PER_LOAD_BOUND: usize = 16;

#[test]
fn cli_lsp_ranges_and_load_time_warnings_survive_real_config_reload() {
    let dir = tempfile::tempdir().unwrap();
    let source = rules::project(dir.path());
    let output = Command::new(env!("CARGO_BIN_EXE_bsl-analyzer-app"))
        .args(["analyze", "-s", ".", "--format", "jsonl", "-q"])
        .current_dir(dir.path())
        .env("BSL_LOG", "warn")
        .env_remove("BSL_LOG_FILE")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert_eq!(rules::warning_count(&stderr), 1);
    let events: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).expect("CLI stdout contains JSONL only"))
        .collect();
    let files: Vec<_> = events.iter().filter(|e| e["type"] == "file").collect();
    assert_eq!(files.len(), 1, "{events:?}");
    assert!(files[0]["error"].is_null(), "{events:?}");
    assert_eq!(events.last().unwrap()["failed_files"], 0);
    assert_eq!(
        rules::legacy_ranges(files[0]["diagnostics"].as_array().unwrap()),
        rules::expected(&source, false, true)
    );

    let log_file = dir.path().join("lsp-{pid}.log");
    let mut lsp = Lsp::start_with_capabilities_and_log(
        dir.path(),
        json!({"general":{"positionEncodings":["utf-16"]}}),
        Some(&log_file),
    );
    let file = dir.path().join("src/Module.bsl");
    lsp.open(&file, &source);
    let uri = lsp_types::Url::from_file_path(&file).unwrap();
    let mut request_id = 100;
    let mut pull = |lsp: &mut Lsp, deadline: Instant| loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(!remaining.is_zero(), "LSP diagnostics deadline expired");
        request_id += 1;
        lsp.send(json!({"jsonrpc":"2.0","id":request_id,"method":"textDocument/diagnostic","params":{"textDocument":{"uri":uri}}}));
        let response = lsp
            .wait_for_within(remaining, |message| {
                message["id"] == request_id
                    && message["method"].is_null()
                    && (message.get("result").is_some() || message.get("error").is_some())
            })
            .expect("LSP diagnostic response exceeded its deadline");
        if response["error"]["code"] == -32801 {
            assert!(Instant::now() < deadline, "LSP remained ContentModified: {response}");
            std::thread::sleep(Duration::from_millis(20));
            continue;
        }
        assert!(response["error"].is_null(), "{response}");
        return rules::lsp_ranges(
            response["result"]["items"].as_array().expect("full diagnostic report"),
        );
    };
    assert_eq!(
        pull(&mut lsp, Instant::now() + Duration::from_secs(60)),
        rules::expected(&source, true, true)
    );
    let before = rules::warning_count(&session_log(dir.path()));
    assert!(before > 0);
    assert!(!session_log(dir.path()).contains(RELOAD_WARNING));
    for _ in 0..PULLS_PER_LOAD_BOUND {
        assert_eq!(
            pull(&mut lsp, Instant::now() + Duration::from_secs(60)),
            rules::expected(&source, true, true)
        );
    }
    let grown = rules::warning_count(&session_log(dir.path())) - before;
    assert!(grown < PULLS_PER_LOAD_BOUND, "pulls reloaded the startup config: {grown} loads");

    let next = dir.path().join("bsl-analyzer.toml.next");
    std::fs::write(&next, format!("{}{RELOAD_KEY}", rules::config(7))).unwrap();
    std::fs::rename(next, dir.path().join("bsl-analyzer.toml")).unwrap();
    let config_uri = lsp_types::Url::from_file_path(dir.path().join("bsl-analyzer.toml")).unwrap();
    lsp.send(json!({"jsonrpc":"2.0","method":"workspace/didChangeWatchedFiles","params":{"changes":[{"uri":config_uri,"type":2}]}}));
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if pull(&mut lsp, deadline) == rules::expected(&source, true, false) {
            break;
        }
        assert!(Instant::now() < deadline, "changed Typo config never reached diagnostics");
        std::thread::sleep(Duration::from_millis(20));
    }
    let log = session_log(dir.path());
    assert!(log.contains(RELOAD_WARNING), "the reloaded config logged no load-time warning");
    assert!(!log.contains(rules::PRIVATE_VALUE));
    let before = log.matches(RELOAD_WARNING).count();
    for _ in 0..PULLS_PER_LOAD_BOUND {
        assert_eq!(
            pull(&mut lsp, Instant::now() + Duration::from_secs(60)),
            rules::expected(&source, true, false)
        );
    }
    let grown = session_log(dir.path()).matches(RELOAD_WARNING).count() - before;
    assert!(grown < PULLS_PER_LOAD_BOUND, "pulls reloaded the changed config: {grown} loads");
    assert!(lsp.shutdown(1000).success());
    lsp.assert_protocol_clean();
}

/// The warnings of every process that logged into `root`. A config change can replace the
/// backend daemon, which inherits `BSL_LOG_FILE`; an explicit file is truncated on start, so
/// the replacement would erase what the old backend had logged. `{pid}` gives each process
/// its own file.
fn session_log(root: &Path) -> String {
    let mut logs: Vec<_> = std::fs::read_dir(root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("lsp-") && name.ends_with(".log"))
        })
        .collect();
    logs.sort();
    logs.iter().map(|path| std::fs::read_to_string(path).unwrap()).collect::<Vec<_>>().join("\n")
}
