//! `workspace/semanticTokens/refresh` after a change made to a module on disk while the
//! editor has another module open: a `git pull` or a configuration re-dump. The editor's
//! semanticTokens requests issued during the resulting reindex are declined or cancelled,
//! and it asks again only if the server tells it to.

mod common;

use std::time::{Duration, Instant};

use common::*;

const REFRESH: &str = "workspace/semanticTokens/refresh";

fn is_refresh(message: &serde_json::Value) -> bool {
    message["method"] == REFRESH
}

#[test]
fn an_external_module_change_asks_the_client_to_refresh_semantic_tokens() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("bsl-analyzer.toml"), "[source]\nroot = \"src\"\n").unwrap();
    std::fs::write(root.join("src/Main.bsl"), "Процедура Основная()\r\nКонецПроцедуры\r\n")
        .unwrap();
    std::fs::write(root.join("src/Other.bsl"), "Процедура Другая()\r\nКонецПроцедуры\r\n").unwrap();

    let mut lsp = Lsp::start(&root);

    // The refresh that ends the initial load is not the one under test: wait it out, so
    // that any refresh seen below was caused by the disk change.
    lsp.wait_for(is_refresh);

    lsp.open(&root.join("src/Main.bsl"), "Процедура Основная()\r\nКонецПроцедуры\r\n");

    // The file watcher is armed asynchronously and nothing announces when, so the change
    // is written again until the server reacts or the deadline passes.
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut revision = 0;
    let refreshed = loop {
        revision += 1;
        std::fs::write(
            root.join("src/Other.bsl"),
            format!("Процедура Другая()\r\n    Перем Р{revision};\r\nКонецПроцедуры\r\n"),
        )
        .unwrap();
        if lsp.wait_for_within(Duration::from_secs(2), is_refresh).is_some() {
            break true;
        }
        if Instant::now() >= deadline {
            break false;
        }
    };

    assert!(
        refreshed,
        "a module changed on disk while another one is open must make the server ask the \
         client for {REFRESH}; none arrived in 30 s of {revision} rewrites"
    );
}
