#![allow(dead_code)] // Shared by process and resident tests, which use different adapters.

use serde_json::Value;
use std::path::Path;

pub const MODULE: &str = include_str!("Module.bsl");
pub const WARNING: &str = "invalid value for diagnostic code";
pub const PRIVATE_VALUE: &str = "shape-value-must-stay-private";
pub type Finding = (String, u64, u64, u64, u64);

pub fn config(min_word_length: usize) -> String {
    format!(
        r#"[source]
root = "src"
[platform_help]
source = "none"
[diagnostics.parameters]
Typo = {{ minWordLength = {min_word_length} }}
RewriteMethodParameter = true
LineLength = "{PRIVATE_VALUE}"
"#
    )
}

pub fn project(root: &Path) -> String {
    let source = MODULE.replace("\r\n", "\n").replace('\n', "\r\n");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/Module.bsl"), &source).unwrap();
    std::fs::write(root.join("bsl-analyzer.toml"), config(3)).unwrap();
    source
}

pub fn replace_config(root: &Path, min_word_length: usize) {
    let next = root.join("bsl-analyzer.toml.next");
    std::fs::write(&next, config(min_word_length)).unwrap();
    std::fs::rename(next, root.join("bsl-analyzer.toml")).unwrap();
}

pub fn selected(code: &str) -> bool {
    matches!(code, "Typo" | "RewriteMethodParameter")
}

pub fn expected(source: &str, utf16: bool, include_typos: bool) -> Vec<Finding> {
    let mut findings = Vec::new();
    let mut add = |code: &str, offset: usize, word: &str| {
        let prefix = &source[..offset];
        let line = prefix.bytes().filter(|&b| b == b'\n').count() as u64;
        let line_prefix = prefix.rsplit('\n').next().unwrap();
        let column =
            if utf16 { line_prefix.encode_utf16().count() } else { line_prefix.chars().count() }
                as u64;
        let length = if utf16 { word.encode_utf16().count() } else { word.chars().count() } as u64;
        findings.push((code.to_owned(), line, column, line, column + length));
    };
    add("RewriteMethodParameter", source.find("Value = 21").unwrap(), "Value");
    if include_typos {
        for (offset, word) in source.match_indices("гатово") {
            add("Typo", offset, word);
        }
    }
    findings.sort();
    findings
}

pub fn legacy_ranges(findings: &[Value]) -> Vec<Finding> {
    let mut ranges: Vec<_> = findings
        .iter()
        .filter_map(|finding| {
            let code = finding["code"].as_str().unwrap();
            if !selected(code) {
                return None;
            }
            let range = finding.get("range").unwrap_or(finding);
            Some((
                code.to_owned(),
                range["start_line"].as_u64().unwrap(),
                range["start_column"].as_u64().unwrap(),
                range["end_line"].as_u64().unwrap(),
                range["end_column"].as_u64().unwrap(),
            ))
        })
        .collect();
    ranges.sort();
    ranges
}

pub fn lsp_ranges(findings: &[Value]) -> Vec<Finding> {
    let mut ranges: Vec<_> = findings
        .iter()
        .filter_map(|finding| {
            let code = finding["code"].as_str().unwrap();
            if !selected(code) {
                return None;
            }
            let range = &finding["range"];
            Some((
                code.to_owned(),
                range["start"]["line"].as_u64().unwrap(),
                range["start"]["character"].as_u64().unwrap(),
                range["end"]["line"].as_u64().unwrap(),
                range["end"]["character"].as_u64().unwrap(),
            ))
        })
        .collect();
    ranges.sort();
    ranges
}

pub fn warning_count(log: &str) -> usize {
    assert!(!log.contains(PRIVATE_VALUE), "invalid config value leaked: {log}");
    let warnings: Vec<_> = log.lines().filter(|line| line.contains(WARNING)).collect();
    for warning in &warnings {
        for field in [
            "LineLength",
            "[diagnostics.parameters]",
            "bsl-analyzer.toml",
            "bool",
            "table",
            "default",
        ] {
            assert!(warning.contains(field), "missing {field}: {warning}");
        }
    }
    warnings.len()
}
