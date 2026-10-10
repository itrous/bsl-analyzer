mod common;

use ide::diagnostics_baseline::{
    diagnostic_fingerprint, DiagnosticsBaseline, DiagnosticsBaselineEntry, DiagnosticsBaselineRange,
};

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

/// The control for the harness's own judgement: a wait on a server that is idle and
/// will never meet the condition fails on the FIRST silent window, and says why. A
/// wait that could only ever report "Timeout" after a minute could not tell this case
/// from a server still working, which is what every timeout in this family looked like.
#[test]
fn a_wait_an_idle_server_will_never_satisfy_is_judged_lost_not_timed_out() {
    let dir = project();
    let lsp = Lsp::start(dir.path());
    let silence = Duration::from_secs(2);

    let started = std::time::Instant::now();
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        lsp.wait_for_judging(silence, |_| false);
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
        waited < SILENCE,
        "an idle server is judged on its own silence window, not the default: {waited:?}",
    );
}
