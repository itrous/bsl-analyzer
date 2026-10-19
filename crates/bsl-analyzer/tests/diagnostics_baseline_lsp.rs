mod common;

use ide::diagnostics_baseline::{
    diagnostic_fingerprint, DiagnosticsBaseline, DiagnosticsBaselineEntry, DiagnosticsBaselineRange,
};
use serde_json::{json, Value};

use std::time::Duration;

use common::*;

#[test]
fn parity() {
    let dir = project();
    let baseline: DiagnosticsBaseline =
        serde_json::from_slice(&std::fs::read(dir.path().join("baseline.json")).unwrap()).unwrap();
    assert!(!baseline.diagnostics.is_empty(), "CLI must create at least one known diagnostic");

    let mut lsp = Lsp::start(dir.path());
    let published = lsp.open(&dir.path().join("src/Main.bsl"), BROKEN);
    assert!(
        published["params"]["diagnostics"].as_array().unwrap().is_empty(),
        "LSP must suppress the same diagnostics the CLI recorded: {published}"
    );
}

#[test]
fn partial_document() {
    let dir = project();
    let baseline_path = dir.path().join("baseline.json");
    let mut baseline: DiagnosticsBaseline =
        serde_json::from_slice(&std::fs::read(&baseline_path).unwrap()).unwrap();
    baseline.diagnostics.push(DiagnosticsBaselineEntry {
        fingerprint: diagnostic_fingerprint("src/Other.bsl", "UnreachableCode", "Возврат;", 0),
        path: "src/Other.bsl".to_owned(),
        code: "UnreachableCode".to_owned(),
        snippet: "Возврат;".to_owned(),
        occurrence: 0,
        message: "resolved outside the open document".to_owned(),
        severity: "warning".to_owned(),
        range: DiagnosticsBaselineRange {
            start_line: 0,
            start_column: 0,
            end_line: 0,
            end_column: 8,
        },
    });
    std::fs::write(
        &baseline_path,
        ide::diagnostics_baseline::diagnostics_baseline_json(&baseline).unwrap(),
    )
    .unwrap();

    let mut lsp = Lsp::start(dir.path());
    let published = lsp.open(&dir.path().join("src/Main.bsl"), BROKEN);
    assert!(published["params"]["uri"].as_str().unwrap().ends_with("Main.bsl"));
    assert!(published["params"]["diagnostics"].as_array().unwrap().is_empty());

    // The resolved entry belongs to another file, so no publication may carry it —
    // neither this one nor any that follows. Asserting only on the (already empty)
    // publication above could not tell the two apart.
    let deadline = std::time::Instant::now() + Duration::from_millis(500);
    while std::time::Instant::now() < deadline {
        match lsp.messages.recv_timeout(Duration::from_millis(50)) {
            Ok(message) => assert!(
                !message.to_string().contains("resolved outside the open document"),
                "a resolved entry of another file must not be synthesized: {message}"
            ),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}

/// Takes in what the server has already sent, without waiting for more, and says whether any
/// of it was a `window/showMessage`. It also leaves nothing stale for the next positive wait.
fn drained_a_show_message(lsp: &Lsp) -> bool {
    let mut shown = false;
    while let Some(message) = lsp.wait_for_within(Duration::ZERO, |_| true) {
        shown |= message["method"] == "window/showMessage";
    }
    shown
}

#[test]
fn legacy_extension_scope_warning_uses_lsp_channel_and_tracks_config_reload() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("src/cf");
    let extension = dir.path().join("ext");
    std::fs::create_dir_all(&main).unwrap();
    std::fs::create_dir_all(&extension).unwrap();
    std::fs::write(main.join("Configuration.xml"), "<Configuration/>").unwrap();
    std::fs::write(
        extension.join("Configuration.xml"),
        "<Properties><ConfigurationExtensionPurpose>Customization</ConfigurationExtensionPurpose></Properties>",
    )
    .unwrap();
    std::fs::write(main.join("Main.bsl"), "Процедура Проверка()\nКонецПроцедуры\n").unwrap();
    let config = dir.path().join("bsl-analyzer.toml");
    let write_config = |scope: Option<&str>| {
        let scope = scope.map_or_else(String::new, |scope| format!(", scope = \"{scope}\""));
        std::fs::write(
            &config,
            format!(
                "[source]\nroot = \"src/cf\"\nextensions = [{{ name = \"Feature\", path = \"ext\"{scope} }}]\n"
            ),
        )
        .unwrap();
    };
    write_config(None);

    // initialize and all following traffic pass through the real Content-Length LSP
    // transport. The harness retains notifications delivered before initialize's reply.
    // In-process: under auto mode the load runs in a shared backend whose progress and
    // warning reach the client only through a proxy that may close its output first.
    let mut lsp = Lsp::start_stdio(dir.path());
    // The warning and the end of the load come in no fixed order, and a wait for one takes
    // in the other unseen: keep the warning while waiting for the load to finish. A config
    // change that lands while the workspace is still loading is folded into the load, so the
    // save below needs the load finished first.
    let first_warning = std::cell::RefCell::new(None);
    lsp.wait_for(|message| {
        if message["method"] == "window/showMessage" {
            first_warning.borrow_mut().get_or_insert_with(|| message.clone());
        }
        message["method"] == "$/progress"
            && message["params"]["token"].as_str().is_some_and(|token| token.ends_with("/Loading"))
            && message["params"]["value"]["kind"] == "end"
    });
    let warning = first_warning
        .into_inner()
        .unwrap_or_else(|| lsp.wait_for(|message| message["method"] == "window/showMessage"));
    assert_eq!(warning["params"]["type"], 2, "warning severity: {warning}");
    let warning_text = warning["params"]["message"].as_str().unwrap();
    assert!(warning_text.contains("Feature") && warning_text.contains("ext"), "{warning}");
    assert!(
        warning_text.contains("InfoBase") && warning_text.contains("DataSeparation"),
        "the client gets the explicit-scope action and precision limit: {warning}"
    );

    let main_uri = lsp_types::Url::from_file_path(main.join("Main.bsl")).unwrap();
    let diagnostics = lsp.open(&main.join("Main.bsl"), "Процедура Проверка()\nКонецПроцедуры\n");
    assert_eq!(diagnostics["method"], "textDocument/publishDiagnostics");
    let hover_params = json!({
        "textDocument": {"uri": main_uri}, "position": {"line": 0, "character": 10}
    });
    // `Lsp::request` drops what arrives before the reply, so the wait notes any
    // `showMessage` itself: one sent ahead of the reply is a repeated warning too.
    let shown_before_reply = std::cell::Cell::new(false);
    for id in [2, 3] {
        lsp.send(json!({
            "jsonrpc": "2.0", "id": id, "method": "textDocument/hover",
            "params": hover_params.clone()
        }));
        let response = lsp.wait_for(|message| {
            if message["method"] == "window/showMessage" {
                shown_before_reply.set(true);
            }
            message["id"] == id
                && message["method"].is_null()
                && (message.get("result").is_some() || message.get("error").is_some())
        });
        assert!(response.get("result").is_some(), "hover is answered: {response}");
    }
    assert!(
        !shown_before_reply.get() && !drained_a_show_message(&lsp),
        "ordinary document requests do not repeat the snapshot warning"
    );
    if cfg!(timing_contract) {
        assert!(
            lsp.wait_for_within(Duration::from_millis(250), |message| {
                message["method"] == "window/showMessage"
            })
            .is_none(),
            "ordinary document requests do not repeat the snapshot warning"
        );
    }

    // didSave is the existing project-config reload path. The explicit snapshot loses
    // the warning; deleting the field later produces one new warning for that snapshot.
    write_config(Some("InfoBase"));
    lsp.send(json!({
        "jsonrpc": "2.0", "method": "textDocument/didSave",
        "params": {"textDocument": {"uri": lsp_types::Url::from_file_path(&config).unwrap()}}
    }));
    // The save can cancel requests already running against the old snapshot: a write
    // cancellation the server could not replay internally reaches the client as
    // ContentModified, an explicit cancellation as RequestCanceled. Retry only those;
    // any other error remains a test failure.
    let deadline = std::time::Instant::now() + test_utils::wait::ceiling(Duration::from_secs(45));
    let mut response = Value::Null;
    let shown_before_reply = std::cell::Cell::new(false);
    for id in 4.. {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        assert!(!remaining.is_zero(), "explicit snapshot never became ready: {response}");
        // Only the attempt that succeeds counts: the save itself may still be settling in
        // the attempts it cancels.
        shown_before_reply.set(false);
        lsp.send(json!({
            "jsonrpc": "2.0", "id": id, "method": "textDocument/hover",
            "params": hover_params.clone()
        }));
        response = lsp
            .wait_for_within(remaining, |message| {
                if message["method"] == "window/showMessage" {
                    shown_before_reply.set(true);
                }
                message["id"] == id
                    && message["method"].is_null()
                    && (message.get("result").is_some() || message.get("error").is_some())
            })
            .expect("explicit snapshot hover returns within the readiness deadline");
        if response.get("result").is_some() {
            break;
        }
        assert!(
            matches!(response["error"]["code"].as_i64(), Some(-32800 | -32801)),
            "only a reload cancellation may be retried: {response}"
        );
        assert!(
            std::time::Instant::now() < deadline,
            "explicit snapshot never became ready: {response}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(response.get("result").is_some(), "explicit snapshot serves hover: {response}");
    assert!(
        !shown_before_reply.get() && !drained_a_show_message(&lsp),
        "the explicit snapshot removes the compatibility warning"
    );
    if cfg!(timing_contract) {
        assert!(
            lsp.wait_for_within(Duration::from_millis(250), |message| {
                message["method"] == "window/showMessage"
            })
            .is_none(),
            "the explicit snapshot removes the compatibility warning"
        );
    }

    write_config(None);
    lsp.send(json!({
        "jsonrpc": "2.0", "method": "textDocument/didSave",
        "params": {"textDocument": {"uri": lsp_types::Url::from_file_path(&config).unwrap()}}
    }));
    let warning = lsp.wait_for(|message| message["method"] == "window/showMessage");
    assert!(warning["params"]["message"].as_str().unwrap().contains("Feature"));
}

/// The control for the harness's own judgement: a wait on a server that is idle and
/// will never meet the condition fails on the FIRST silent window, and says why. A
/// wait that could only ever report "Timeout" after a minute could not tell this case
/// from a server still working, which is what every timeout in this family looked like.
///
/// The server never meets the condition, so however long a loaded machine keeps it busy, the
/// wait ends on the idle verdict; only how soon it ends is the timing profile's.
#[test]
fn a_wait_an_idle_server_will_never_satisfy_is_judged_lost_not_timed_out() {
    let dir = project();
    let lsp = Lsp::start(dir.path());
    let silence = Duration::from_secs(2);

    let started = std::time::Instant::now();
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        lsp.wait_for_judging(silence, common::BUSY_CEILING, |_| false);
    }));
    let waited = started.elapsed();

    let payload = outcome.expect_err("a condition no message meets must fail the wait");
    let message = payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_owned()))
        .unwrap_or_default();
    assert!(message.contains("used no CPU"), "the verdict names idleness: {message}");
    assert!(message.contains(file!()), "the verdict names the waiting line: {message}");
    assert!(
        message.contains(&format!("for {silence:?} and used no CPU")),
        "an idle server is judged on its own silence window, not the default: {message}",
    );
    if cfg!(timing_contract) {
        assert!(
            waited < SILENCE,
            "an idle server is judged on its own silence window, not the default: {waited:?}",
        );
    }
}

#[test]
#[cfg_attr(
    not(timing_contract),
    ignore = "необязательный тест: the verdict depends on a producer keeping a real-time cadence; запуск: CARGO_TARGET_DIR=target-timing RUSTFLAGS='--cfg timing_contract' cargo test --workspace"
)]
fn a_continuous_unrelated_message_stream_cannot_extend_a_wait_past_its_ceiling() {
    let dir = project();
    let mut lsp = Lsp::start(dir.path());
    let (sender, injected) = std::sync::mpsc::channel::<Value>();
    let original = std::mem::replace(&mut lsp.messages, injected);
    let stream_for = Duration::from_millis(300);
    let producer = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + stream_for;
        while std::time::Instant::now() < deadline {
            if sender.send(json!({"method": "$/testNoise"})).is_err() {
                return;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    });

    let started = std::time::Instant::now();
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        lsp.wait_for_judging(Duration::from_millis(20), Duration::from_millis(80), |_| false);
    }));
    let waited = started.elapsed();
    let payload = outcome.expect_err("unrelated traffic cannot satisfy the requested condition");
    let message = payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_owned()))
        .unwrap_or_default();
    assert!(
        message.contains("total deadline elapsed"),
        "the verdict names the total bound: {message}"
    );
    assert!(message.contains("$/testNoise"), "the verdict names the unrelated traffic: {message}");
    if cfg!(timing_contract) {
        assert!(
            waited < stream_for,
            "the wait ended before the bounded noise producer: {waited:?}"
        );
    }
    producer.join().expect("the bounded noise producer exits");
    lsp.messages = original;
}

#[test]
fn an_owned_live_lsp_child_is_killed_and_reaped_with_a_bounded_wait() {
    let dir = project();
    let mut lsp = Lsp::start(dir.path());
    let pid = lsp.child.id();
    assert!(lsp.child.try_wait().unwrap().is_none(), "owned LSP child {pid} is running");

    assert_eq!(
        lsp.wait_for_exit_within(Duration::from_millis(20), "process deadline control").unwrap(),
        None,
        "a running child remains alive when its bounded poll expires"
    );
    assert!(lsp.child.try_wait().unwrap().is_none(), "the deadline poll did not kill child {pid}");

    lsp.child.kill().expect("the owned LSP child can be killed directly");
    let status = lsp
        .wait_for_exit_within(Duration::from_secs(2), "process cleanup control")
        .expect("the post-kill process poll succeeds")
        .expect("the killed LSP child is reaped within the bound");
    assert!(!status.success(), "forced cleanup is not a graceful LSP shutdown");
    assert!(lsp.child.try_wait().unwrap().is_some(), "owned LSP child {pid} was reaped");
}

#[test]
fn a_server_request_reusing_the_client_id_is_not_taken_for_its_response() {
    let dir = project();
    let mut lsp = Lsp::start(dir.path());
    let (sender, injected) = std::sync::mpsc::channel::<Value>();
    let original = std::mem::replace(&mut lsp.messages, injected);
    // Client and server number their requests independently, so a server request can
    // carry the very id the client is waiting on.
    let server_request = json!({
        "jsonrpc": "2.0", "id": 2, "method": "window/workDoneProgress/create",
        "params": {"token": "bslAnalyzer/Analyzing"}
    });
    let response = json!({"jsonrpc": "2.0", "id": 2, "result": null});
    sender.send(server_request).unwrap();
    sender.send(response.clone()).unwrap();

    let answer = lsp.request(2, "textDocument/hover", json!({}));
    assert_eq!(answer, response, "the server request with the same id is skipped");
    lsp.messages = original;
}

#[test]
fn a_wait_whose_bound_has_run_out_still_takes_a_message_already_delivered() {
    let dir = project();
    let mut lsp = Lsp::start(dir.path());
    let (sender, injected) = std::sync::mpsc::channel::<Value>();
    let original = std::mem::replace(&mut lsp.messages, injected);
    let delivered = json!({"jsonrpc": "2.0", "method": "window/logMessage", "params": {}});
    sender.send(delivered.clone()).unwrap();

    assert_eq!(
        lsp.wait_for_within(Duration::ZERO, |_| true),
        Some(delivered),
        "a message that already arrived is not silence, however short the wait"
    );
    lsp.messages = original;
}

#[test]
fn a_matching_message_taken_in_past_the_busy_ceiling_is_still_returned() {
    let dir = project();
    let mut lsp = Lsp::start(dir.path());
    let (sender, injected) = std::sync::mpsc::channel::<Value>();
    let original = std::mem::replace(&mut lsp.messages, injected);
    let delivered = json!({"jsonrpc": "2.0", "method": "window/logMessage", "params": {}});
    sender.send(delivered.clone()).unwrap();

    // A zero ceiling has always run out by the time the message is taken in, as a late
    // wake-up of the test thread leaves it under load.
    assert_eq!(
        lsp.wait_for_judging(Duration::ZERO, Duration::ZERO, |_| true),
        delivered,
        "a message delivered in time is judged, not lost to the ceiling"
    );
    lsp.messages = original;
}
