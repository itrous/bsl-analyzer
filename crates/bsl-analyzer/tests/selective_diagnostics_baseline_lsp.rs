mod common;

use common::*;
use std::io::Write;
use std::process::Command;
use std::time::Duration;

use serde_json::Value;

const UNSUPPRESSED: &str =
    "Процедура Тест()\n    // bsl-analyzer:off NoSuchRule\n    А = А;\nКонецПроцедуры\n";

fn selective_project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for source in ["src/cf", "src/cfe/Ext"] {
        std::fs::create_dir_all(dir.path().join(source)).unwrap();
        std::fs::write(dir.path().join(source).join("Configuration.xml"), "<Configuration/>")
            .unwrap();
    }
    std::fs::write(dir.path().join("src/cf/Main.bsl"), BROKEN).unwrap();
    std::fs::write(dir.path().join("src/cfe/Ext/Ext.bsl"), UNSUPPRESSED).unwrap();
    std::fs::write(
        dir.path().join("bsl-analyzer.toml"),
        r#"[source]
root = "src/cf"
extensions = [{ name = "Ext", path = "src/cfe/Ext", scope = "InfoBase" }]
[diagnostics.baseline]
directory = "baselines"
include = ["main"]
"#,
    )
    .unwrap();
    let created = Command::new(env!("CARGO_BIN_EXE_bsl-analyzer-app"))
        .current_dir(dir.path())
        .args(["diagnostics", "baseline", "create", "-s", "."])
        .output()
        .unwrap();
    assert!(created.status.success(), "{}", String::from_utf8_lossy(&created.stderr));
    dir
}

fn full_manifest_selective_project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for source in ["src/cf", "src/cfe/Ext"] {
        std::fs::create_dir_all(dir.path().join(source)).unwrap();
        std::fs::write(dir.path().join(source).join("Configuration.xml"), "<Configuration/>")
            .unwrap();
    }
    std::fs::write(dir.path().join("src/cf/Main.bsl"), BROKEN).unwrap();
    std::fs::write(dir.path().join("src/cfe/Ext/Ext.bsl"), UNSUPPRESSED).unwrap();
    let config = |include: &str| {
        std::fs::write(
            dir.path().join("bsl-analyzer.toml"),
            format!(
                r#"[source]
root = "src/cf"
extensions = [{{ name = "Ext", path = "src/cfe/Ext", scope = "InfoBase" }}]
[diagnostics.baseline]
directory = "baselines"
{include}"#
            ),
        )
        .unwrap();
    };
    config("");
    let created = Command::new(env!("CARGO_BIN_EXE_bsl-analyzer-app"))
        .current_dir(dir.path())
        .args(["diagnostics", "baseline", "create", "-s", "."])
        .output()
        .unwrap();
    assert!(created.status.success(), "{}", String::from_utf8_lossy(&created.stderr));
    config("include = [\"main\"]\n");
    dir
}

#[test]
fn selective_lsp_publishes_new_unsuppressed_and_protected() {
    let dir = selective_project();
    let root = dir.path();
    let mut lsp = Lsp::start(root);
    let main = lsp.open(&root.join("src/cf/Main.bsl"), BROKEN);
    assert!(main["params"]["diagnostics"].as_array().unwrap().is_empty());
    let ext = lsp.open(&root.join("src/cfe/Ext/Ext.bsl"), UNSUPPRESSED);
    let codes = ext["params"]["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|diagnostic| diagnostic["code"].as_str())
        .collect::<std::collections::BTreeSet<_>>();
    assert!(codes.contains("SelfAssign"));
    assert!(codes.contains("UnknownSuppressionCode"));
}

#[test]
fn selective_lsp_enabled_error_is_fail_visible_and_recovers() {
    let dir = selective_project();
    // The server republishes under the path it resolved through the filesystem, so a root
    // reached by a symlink — every temporary directory on macOS — has to be named the same way
    // here, or the awaited notification never matches.
    let root = &dir.path().canonicalize().unwrap();
    let mut lsp = Lsp::start(root);
    assert!(lsp.open(&root.join("src/cf/Main.bsl"), BROKEN)["params"]["diagnostics"]
        .as_array()
        .unwrap()
        .is_empty());
    assert!(!lsp.open(&root.join("src/cfe/Ext/Ext.bsl"), UNSUPPRESSED)["params"]["diagnostics"]
        .as_array()
        .unwrap()
        .is_empty());

    let manifest: Value =
        serde_json::from_slice(&std::fs::read(root.join("baselines/manifest.json")).unwrap())
            .unwrap();
    let relative = manifest["partitions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["partition_id"] == "main")
        .unwrap()["file"]
        .as_str()
        .unwrap()
        .to_owned();
    let object = root.join("baselines").join(&relative);
    let valid = std::fs::read(&object).unwrap();
    let directory =
        project_model::ManagedBaselineDirectory::open(root, "baselines", false).unwrap();
    // Publish one complete state: truncate/write can expose an empty file with a
    // different error epoch between two identical corrupt snapshots.
    let replace_object = |contents: &[u8]| {
        directory.create_file_new("replacement.tmp").unwrap().write_all(contents).unwrap();
        directory.replace_file("replacement.tmp", &relative).unwrap();
    };
    // Broken ONCE: the write may land before the watcher is armed and raise no event, and
    // the server has to notice it anyway, the next time the client asks it for anything.
    replace_object(b"{broken");
    let deadline =
        std::time::Instant::now() + test_utils::wait::hang_guard(Duration::from_secs(60));
    let main_uri = lsp_types::Url::from_file_path(root.join("src/cf/Main.bsl")).unwrap();
    let ext_uri = lsp_types::Url::from_file_path(root.join("src/cfe/Ext/Ext.bsl")).unwrap();
    let (mut notified, mut main_seen, mut ext_seen) = (false, false, false);
    while !notified || !main_seen || !ext_seen {
        let Some(message) = lsp.wait_for_within(Duration::from_secs(1), |_| true) else {
            assert!(std::time::Instant::now() < deadline, "the server never saw the broken object",);
            lsp.poke();
            continue;
        };
        if message["method"] == "window/showMessage"
            && message["params"]["message"]
                .as_str()
                .is_some_and(|message| message.contains("diagnostics baseline"))
        {
            notified = true;
        } else if message["method"] == "textDocument/publishDiagnostics" {
            let uri = message["params"]["uri"].as_str().unwrap();
            if uri == main_uri.as_str() || uri == ext_uri.as_str() {
                assert!(!message["params"]["diagnostics"].as_array().unwrap().is_empty());
                main_seen |= uri == main_uri.as_str();
                ext_seen |= uri == ext_uri.as_str();
            }
        }
    }

    // A repeated error raises no message at all, so only a window can say it was not
    // reported again, and a window proves nothing on a machine that has not got there yet.
    if cfg!(timing_contract) {
        replace_object(b"{broken");
        let duplicate = std::time::Instant::now() + Duration::from_millis(500);
        while std::time::Instant::now() < duplicate {
            match lsp.messages.recv_timeout(Duration::from_millis(50)) {
                Ok(message) => assert!(
                    message["method"] != "window/showMessage"
                        || !message["params"]["message"]
                            .as_str()
                            .is_some_and(|message| message.contains("diagnostics baseline")),
                    "unchanged baseline error was reported again: {message}"
                ),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    }

    replace_object(&valid);
    let mut main_seen = false;
    let mut ext_seen = false;
    let repaired =
        std::time::Instant::now() + test_utils::wait::hang_guard(Duration::from_secs(60));
    while !main_seen || !ext_seen {
        // Poked for the reason the first provocation is: the repair may have landed
        // before the watcher was re-armed and raised no event, and the server notices
        // that only when the client asks it for something.
        let Some(message) = lsp.wait_for_within(Duration::from_secs(1), |message| {
            message["method"] == "textDocument/publishDiagnostics"
        }) else {
            assert!(std::time::Instant::now() < repaired, "the server never republished");
            lsp.poke();
            continue;
        };
        let uri = message["params"]["uri"].as_str().unwrap();
        // A publication of the broken baseline may still be on its way; the repair is the
        // first one that clears the main document.
        if uri == main_uri.as_str() {
            main_seen |= message["params"]["diagnostics"].as_array().unwrap().is_empty();
        } else if uri == ext_uri.as_str() {
            assert!(!message["params"]["diagnostics"].as_array().unwrap().is_empty());
            ext_seen = true;
        }
    }

    replace_object(b"{broken-again");
    let again = std::time::Instant::now() + test_utils::wait::hang_guard(Duration::from_secs(60));
    let notified = loop {
        if let Some(message) = lsp.wait_for_within(Duration::from_secs(1), |message| {
            message["method"] == "window/showMessage"
        }) {
            break message;
        }
        assert!(std::time::Instant::now() < again, "the server never saw the second break");
        lsp.poke();
    };
    assert!(notified["params"]["message"]
        .as_str()
        .is_some_and(|message| message.contains("diagnostics baseline")));
}

fn select_both(root: &std::path::Path) {
    std::fs::write(
        root.join("bsl-analyzer.toml"),
        r#"[source]
root = "src/cf"
extensions = [{ name = "Ext", path = "src/cfe/Ext", scope = "InfoBase" }]
[diagnostics.baseline]
directory = "baselines"
include = ["main", "extension:Ext"]
"#,
    )
    .unwrap();
}

#[test]
fn selective_lsp_config_reload_applies_selection_and_republishes() {
    let dir = full_manifest_selective_project();
    // The server republishes under the path it resolved through the filesystem, so a root
    // reached by a symlink — every temporary directory on macOS — has to be named the same way
    // here, or the awaited notification never matches.
    let root = &dir.path().canonicalize().unwrap();
    let mut lsp = Lsp::start(root);
    assert!(lsp.open(&root.join("src/cf/Main.bsl"), BROKEN)["params"]["diagnostics"]
        .as_array()
        .unwrap()
        .is_empty());
    assert!(!lsp.open(&root.join("src/cfe/Ext/Ext.bsl"), UNSUPPRESSED)["params"]["diagnostics"]
        .as_array()
        .unwrap()
        .is_empty());
    let selected = Provocation::start(|| select_both(root));
    let mut remaining = std::collections::BTreeSet::from([
        lsp_types::Url::from_file_path(root.join("src/cf/Main.bsl")).unwrap().to_string(),
        lsp_types::Url::from_file_path(root.join("src/cfe/Ext/Ext.bsl")).unwrap().to_string(),
    ]);
    while !remaining.is_empty() {
        let Some(message) = lsp.wait_for_within(Duration::from_secs(2), |message| {
            message["method"] == "textDocument/publishDiagnostics"
                && message["params"]["uri"].as_str().is_some_and(|uri| remaining.contains(uri))
        }) else {
            selected.again(&lsp, "diagnostics after the selection change");
            continue;
        };
        let uri = message["params"]["uri"].as_str().unwrap();
        let diagnostics = message["params"]["diagnostics"].as_array().unwrap();
        let final_state = if uri.ends_with("Main.bsl") {
            diagnostics.is_empty()
        } else {
            !diagnostics.is_empty()
                && diagnostics
                    .iter()
                    .all(|diagnostic| diagnostic["code"].as_str() == Some("UnknownSuppressionCode"))
        };
        if final_state {
            remaining.remove(uri);
        }
    }
}

#[test]
#[cfg_attr(
    not(timing_contract),
    ignore = "необязательный тест: единственное утверждение — окно без активности LSP после записи в неотслеживаемый объект; событийной замены нет: наблюдатель сервера взводится без извещения, а реакция на последующую запись в отслеживаемый объект не доказывает, что сервер уже разобрал эту — порядок реакций контракт не обещает; запуск: cargo test -p bsl-analyzer --test selective_diagnostics_baseline_lsp -- --ignored --exact selective_lsp_does_not_watch_unsuppressed_objects"
)]
fn selective_lsp_does_not_watch_unsuppressed_objects() {
    let dir = full_manifest_selective_project();
    let root = dir.path();
    let mut lsp = Lsp::start(root);
    lsp.open(&root.join("src/cf/Main.bsl"), BROKEN);
    lsp.open(&root.join("src/cfe/Ext/Ext.bsl"), UNSUPPRESSED);
    let manifest: Value =
        serde_json::from_slice(&std::fs::read(root.join("baselines/manifest.json")).unwrap())
            .unwrap();
    let dormant = manifest["partitions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["partition_id"] == "extension:Ext")
        .unwrap()["file"]
        .as_str()
        .unwrap();
    std::fs::write(root.join("baselines").join(dormant), b"{broken").unwrap();
    // No activity is a window: nothing marks the moment the server has passed over the write.
    let deadline = std::time::Instant::now() + Duration::from_secs(1);
    while std::time::Instant::now() < deadline {
        match lsp.messages.recv_timeout(Duration::from_millis(50)) {
            Ok(message) => assert!(
                message["method"] != "window/showMessage"
                    && message["method"] != "textDocument/publishDiagnostics",
                "dormant object caused LSP activity: {message}"
            ),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}
