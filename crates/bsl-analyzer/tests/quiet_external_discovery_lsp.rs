mod common;

use common::Lsp;
use serde_json::json;

#[test]
fn initial_lsp_load_reports_invalid_discovery_once_across_internal_requests() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("src");
    std::fs::create_dir_all(source.join("epf/not-an-export")).unwrap();
    let module = source.join("Main.bsl");
    let text = "Процедура Тест()\nКонецПроцедуры\n";
    std::fs::write(&module, text).unwrap();
    std::fs::write(root.path().join("bsl-analyzer.toml"), "[source]\nroot = \"src\"\n").unwrap();

    let mut lsp = Lsp::start(root.path());
    lsp.open(&module, text);
    let uri = lsp_types::Url::from_file_path(&module).unwrap();
    for id in 2..5 {
        let response = lsp.request(
            id,
            "textDocument/hover",
            json!({
                "textDocument": {"uri": uri.clone()},
                "position": {"line": 0, "character": 10}
            }),
        );
        assert!(response["error"].is_null(), "hover failed: {response}");
    }

    let status = lsp.shutdown(5);
    assert!(status.success(), "LSP subprocess exited with {status}");

    // shutdown joins the stderr reader after the child closes its pipe, so this count covers
    // the complete process lifetime and cannot miss a delayed internal warning.
    let stderr = lsp.stderr_text();
    assert_eq!(
        stderr.matches("auto-discovered external directories were skipped").count(),
        1,
        "the initial LSP project load must report discovery once; internal requests must stay quiet:\n{stderr}"
    );
    let summary = stderr
        .lines()
        .find(|line| line.contains("auto-discovered external directories were skipped"))
        .expect("initial discovery summary should be present");
    assert!(
        summary.contains("src/epf/not-an-export"),
        "summary should use a relative path: {summary}"
    );
    assert!(
        !summary.contains(root.path().to_string_lossy().as_ref()),
        "absolute root leaked in the discovery summary: {summary}"
    );
}
