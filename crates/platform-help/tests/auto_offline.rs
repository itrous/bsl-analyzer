//! `auto` has no network step: without a saved snapshot and an installation it
//! serves the built-in interface facts and keeps the reason, and explicit
//! sources never fall back to another one.

mod common;

use std::path::{Path, PathBuf};

use bsl_platform::{PlatformData, PlatformHelp, PlatformHelpRequest, PlatformHelpSourceKind};
use html_parser::fixture_hbk;
use platform_help::{load_with, LoadContext};

fn context(cache: &Path) -> LoadContext {
    LoadContext {
        cache_dir: cache.to_path_buf(),
        discovery_roots: Vec::new(),
        platform_path_env: None,
    }
}

fn add_text(help: PlatformHelp) -> Option<String> {
    let data = PlatformData::from_help(help);
    let add = data.get_method("Массив", "Добавить")?;
    data.get_method_docs(add.id).map(|docs| docs.description)
}

/// `auto` fell back to the compiled-in interface facts: `Массив.Добавить` is
/// known without any text, and the reason nothing richer served is kept.
fn degraded_to_facts(help: PlatformHelp) -> String {
    let origin = help.origin.clone().unwrap_or_else(|| panic!("{:?}", help.missing_reason));
    assert_eq!(origin.source, PlatformHelpSourceKind::Bundled);
    assert_eq!(origin.platform_version.as_deref(), Some(bsl_platform::BUNDLED_PLATFORM_VERSION));
    let reason = help.missing_reason.clone().expect("the reason for degrading is kept");
    assert_eq!(add_text(help).as_deref(), Some(""), "facts carry no text");
    reason
}

fn installation(root: &Path, text: &str) -> PathBuf {
    let dir = root.join("8.3.27.9");
    let mut pages = fixture_hbk::shcntx_pages();
    pages[1].1 = pages[1].1.replace("appends one element", text);
    fixture_hbk::write_pair(&dir, &pages, 101);
    dir
}

#[test]
fn auto_without_an_installation_or_a_snapshot_serves_the_facts_offline() {
    let cache = tempfile::tempdir().unwrap();
    let help = load_with(&PlatformHelpRequest::Auto, &context(cache.path()));
    let reason = degraded_to_facts(help);
    assert!(reason.contains("no saved auto snapshot"), "{reason}");
    assert!(!reason.contains("pinned"), "{reason}");
    assert!(!cache.path().join("pinned").exists(), "no download slot is created");
}

#[test]
fn auto_with_an_installation_serves_it_and_then_its_saved_snapshot() {
    let root = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    installation(root.path(), "Installed text.");
    let ctx =
        LoadContext { discovery_roots: vec![root.path().to_path_buf()], ..context(cache.path()) };
    let help = load_with(&PlatformHelpRequest::Auto, &ctx);
    assert!(add_text(help).unwrap().contains("Installed text."));

    // The installation is gone: the saved auto snapshot serves.
    let help = load_with(&PlatformHelpRequest::Auto, &context(cache.path()));
    assert!(add_text(help).unwrap().contains("Installed text."));
}

#[test]
fn explicit_sources_never_fall_back_to_the_facts() {
    let cache = tempfile::tempdir().unwrap();
    let ctx = context(cache.path());
    let absent = cache.path().join("absent");
    for request in [
        PlatformHelpRequest::None,
        PlatformHelpRequest::Installed { path: None },
        PlatformHelpRequest::Installed { path: Some(absent.clone()) },
        PlatformHelpRequest::ExternalPath(absent.join("corpus.json")),
    ] {
        let help = load_with(&request, &ctx);
        assert!(help.origin.is_none(), "{request:?} never falls back to another source");
    }
    // `bundled` is the facts by choice: no reason to record, nothing richer asked.
    let bundled = load_with(&PlatformHelpRequest::Bundled, &ctx);
    assert_eq!(bundled.origin.as_ref().map(|o| o.source), Some(PlatformHelpSourceKind::Bundled));
    assert!(bundled.missing_reason.is_none());
    assert_eq!(add_text(bundled).as_deref(), Some(""));
}
