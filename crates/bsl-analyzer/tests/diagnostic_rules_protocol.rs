mod common;
#[path = "fixtures/diagnostic_rules/mod.rs"]
mod rules;

use common::Lsp;
use serde_json::{json, Value};
use std::process::Command;
use std::time::{Duration, Instant};

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

    let log_file = dir.path().join("lsp.log");
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
    let initial = rules::warning_count(&std::fs::read_to_string(&log_file).unwrap());
    assert!(initial > 0);
    for _ in 0..2 {
        assert_eq!(
            pull(&mut lsp, Instant::now() + Duration::from_secs(60)),
            rules::expected(&source, true, true)
        );
    }
    assert_eq!(rules::warning_count(&std::fs::read_to_string(&log_file).unwrap()), initial);

    rules::replace_config(dir.path(), 7);
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
    let reloaded = rules::warning_count(&std::fs::read_to_string(&log_file).unwrap());
    assert!(reloaded > initial);
    for _ in 0..2 {
        assert_eq!(
            pull(&mut lsp, Instant::now() + Duration::from_secs(60)),
            rules::expected(&source, true, false)
        );
    }
    assert_eq!(rules::warning_count(&std::fs::read_to_string(&log_file).unwrap()), reloaded);
    assert!(lsp.shutdown(1000).success());
    lsp.assert_protocol_clean();
}
