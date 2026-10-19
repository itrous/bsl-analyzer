mod common;
#[path = "fixtures/diagnostic_rules/mod.rs"]
mod rules;

use common::Lsp;
use serde_json::{json, Value};
use std::process::Command;
use std::time::{Duration, Instant};

/// Bound of a wait for an event that a regression would never deliver; a hang watchdog, not a
/// latency measure.
const HANG_WATCHDOG: Duration = test_utils::wait::ceiling(Duration::from_secs(60));

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

    // Outside the project root: the server watches that root recursively, so a log
    // inside it makes every log line a file change that reloads the project and logs again.
    let log_dir = tempfile::tempdir().unwrap();
    let log_file = rules::log_template(log_dir.path());
    // In-process: under auto mode the project may load in a shared backend whose log is
    // not in this directory, depending on whether the backend answers in time.
    let mut command = Command::new(env!("CARGO_BIN_EXE_bsl-analyzer-app"));
    command.args(["lsp", "--mode", "stdio"]).env("BSL_LOG_FILE", &log_file).env("BSL_LOG", "warn");
    let mut lsp = Lsp::start_command(
        command,
        dir.path(),
        json!({"general":{"positionEncodings":["utf-16"]}}),
        common::SILENCE,
    );
    // A config change that lands while the workspace is still loading is folded into
    // the load instead of reloading the project, so it logs no new warning; the reload
    // under test needs the load finished first.
    lsp.wait_for(|message| {
        message["method"] == "$/progress"
            && message["params"]["token"].as_str().is_some_and(|token| token.ends_with("/Loading"))
            && message["params"]["value"]["kind"] == "end"
    });
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
        pull(&mut lsp, Instant::now() + HANG_WATCHDOG),
        rules::expected(&source, true, true)
    );
    let before = rules::warning_count(&rules::session_log(log_dir.path()));
    assert!(before > 0);
    assert!(!rules::session_log(log_dir.path()).contains(rules::RELOAD_WARNING));
    for _ in 0..rules::PULLS_PER_LOAD_BOUND {
        assert_eq!(
            pull(&mut lsp, Instant::now() + HANG_WATCHDOG),
            rules::expected(&source, true, true)
        );
    }
    let grown = rules::warning_count(&rules::session_log(log_dir.path())) - before;
    assert!(
        grown < rules::PULLS_PER_LOAD_BOUND,
        "pulls reloaded the startup config: {grown} loads"
    );

    rules::replace_config_marked(dir.path(), 7);
    let config_uri = lsp_types::Url::from_file_path(dir.path().join("bsl-analyzer.toml")).unwrap();
    lsp.send(json!({"jsonrpc":"2.0","method":"workspace/didChangeWatchedFiles","params":{"changes":[{"uri":config_uri,"type":2}]}}));
    let deadline = Instant::now() + HANG_WATCHDOG;
    // The reloaded diagnostics and the reload's load-time warning are published
    // independently, so wait for both instead of assuming an order.
    let log = loop {
        let diagnostics_reloaded =
            pull(&mut lsp, deadline) == rules::expected(&source, true, false);
        let log = rules::session_log(log_dir.path());
        let warned = log.contains(rules::RELOAD_WARNING);
        if diagnostics_reloaded && warned {
            break log;
        }
        assert!(
            Instant::now() < deadline,
            "changed Typo config never reached diagnostics and the log \
             (diagnostics: {diagnostics_reloaded}, load-time warning: {warned})"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(!log.contains(rules::PRIVATE_VALUE));
    let before = log.matches(rules::RELOAD_WARNING).count();
    for _ in 0..rules::PULLS_PER_LOAD_BOUND {
        assert_eq!(
            pull(&mut lsp, Instant::now() + HANG_WATCHDOG),
            rules::expected(&source, true, false)
        );
    }
    let grown = rules::session_log(log_dir.path()).matches(rules::RELOAD_WARNING).count() - before;
    assert!(
        grown < rules::PULLS_PER_LOAD_BOUND,
        "pulls reloaded the changed config: {grown} loads"
    );
    assert!(lsp.shutdown(1000).success());
    lsp.assert_protocol_clean();
}
