//! `external.url` against a local HTTP fixture: success, outages and bad
//! packages fall back only to the same URL's last valid package, and sources
//! that must not touch the network never reach the server.

mod common;

use std::path::PathBuf;
use std::sync::atomic::Ordering;

use bsl_platform::{PlatformData, PlatformHelp, PlatformHelpRequest, PlatformHelpSourceKind};
use common::{corpus_with_add_text, Server};
use platform_help::{load_with, package, LoadContext};
use serde_json::Value;

fn context(cache: &std::path::Path) -> LoadContext {
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

fn url_request(url: String) -> PlatformHelpRequest {
    PlatformHelpRequest::ExternalUrl(url)
}

#[test]
fn published_package_loads_then_survives_outages_and_bad_updates() {
    let server = Server::start();
    let cache = tempfile::tempdir().unwrap();
    server.publish("a", "Package A text.");
    let request = url_request(server.url("/a/manifest.json"));

    let help = load_with(&request, &context(cache.path()));
    let origin = help.origin.clone().unwrap_or_else(|| panic!("{:?}", help.missing_reason));
    assert_eq!(origin.source, PlatformHelpSourceKind::External);
    assert_eq!(origin.platform_version.as_deref(), Some("8.3.27.1"));
    assert_eq!(add_text(help).as_deref(), Some("Package A text."));

    // Outage: the last valid package of this URL serves.
    server.take_down();
    let offline = load_with(&request, &context(cache.path()));
    assert_eq!(add_text(offline).as_deref(), Some("Package A text."));

    // A manifest whose digest does not match the corpus is not taken.
    server.publish("a", "Package A text.");
    server.put("/a/platform_data.json", &corpus_with_add_text("Tampered text."));
    let tampered = load_with(&request, &context(cache.path()));
    assert_eq!(add_text(tampered).as_deref(), Some("Package A text."));

    // A manifest of an unknown schema is not taken either.
    let mut manifest: Value =
        serde_json::from_slice(&server.files.lock().unwrap()["/a/manifest.json"]).unwrap();
    manifest["schema_version"] = Value::from(99);
    server.put("/a/manifest.json", &serde_json::to_vec(&manifest).unwrap());
    let unknown = load_with(&request, &context(cache.path()));
    assert_eq!(add_text(unknown).as_deref(), Some("Package A text."));

    // The same corpus republished with new metadata is taken as published.
    server.publish("a", "Package A text.");
    let mut manifest: Value =
        serde_json::from_slice(&server.files.lock().unwrap()["/a/manifest.json"]).unwrap();
    manifest["platform_version"] = Value::from("8.3.27.2");
    server.put("/a/manifest.json", &serde_json::to_vec(&manifest).unwrap());
    let relabelled = load_with(&request, &context(cache.path()));
    assert_eq!(relabelled.origin.unwrap().platform_version.as_deref(), Some("8.3.27.2"));

    // A corpus file name that leaves the manifest's directory is refused.
    manifest["corpus_file"] = Value::from("x\\..\\..\\b\\platform_data.json");
    server.put("/a/manifest.json", &serde_json::to_vec(&manifest).unwrap());
    let escaping = load_with(&request, &context(cache.path()));
    assert_eq!(
        escaping.origin.unwrap().platform_version.as_deref(),
        Some("8.3.27.2"),
        "cache serves"
    );

    // Correct digest is not enough: an undecodable corpus cannot replace valid help.
    let malformed = br#"{"methods":3}"#;
    manifest["schema_version"] = Value::from(1);
    manifest["corpus_file"] = Value::from("platform_data.json");
    manifest["sha256"] = Value::from(package::sha256_hex(malformed));
    server.put("/a/manifest.json", &serde_json::to_vec(&manifest).unwrap());
    server.put("/a/platform_data.json", malformed);
    let invalid = load_with(&request, &context(cache.path()));
    assert_eq!(add_text(invalid).as_deref(), Some("Package A text."));
    let empty_cache = tempfile::tempdir().unwrap();
    let invalid = load_with(&request, &context(empty_cache.path()));
    assert!(invalid.snapshot.is_empty() && invalid.origin.is_none());
    assert!(invalid.missing_reason.unwrap().contains("invalid help corpus"));
    let slot = package::Slot::new(platform_help::external::cache_dir_for(
        &server.url("/a/manifest.json"),
        &context(empty_cache.path()),
    ));
    assert!(slot.read().is_err(), "an invalid package was not published");

    // A valid new release replaces the cached one.
    server.publish("a", "Package A, second release.");
    let updated = load_with(&request, &context(cache.path()));
    assert_eq!(add_text(updated).as_deref(), Some("Package A, second release."));
}

#[test]
fn a_slow_download_does_not_roll_back_a_newer_publication() {
    let dir = tempfile::tempdir().unwrap();
    let slot = package::Slot::new(dir.path().join("slot"));
    // The slow fetch observed the slot before the newer release was published;
    // timestamps play no part, so equal or skewed clocks change nothing.
    let observed = slot.generation();
    slot.publish(b"{\"release\":2}", "r", None, "t", &[]).unwrap();
    let old = slot
        .publish_unless_superseded(b"{\"release\":1}", "r", None, "t", &[], Some(observed))
        .unwrap();
    assert_eq!(old.bytes, b"{\"release\":1}", "the caller still gets what it fetched");
    assert_eq!(slot.read().unwrap().bytes, b"{\"release\":2}", "the newer release stays current");
    // A writer that observed the current generation does publish.
    let current = slot.generation();
    slot.publish_unless_superseded(b"{\"release\":3}", "r", None, "t", &[], Some(current)).unwrap();
    assert_eq!(slot.read().unwrap().bytes, b"{\"release\":3}");
}

#[test]
fn a_failing_url_never_serves_another_urls_package() {
    let server = Server::start();
    let cache = tempfile::tempdir().unwrap();
    server.publish("a", "Package A text.");
    server.publish("b", "Package B text.");
    let a = url_request(server.url("/a/manifest.json"));
    let b = url_request(server.url("/b/manifest.json"));
    assert_eq!(add_text(load_with(&a, &context(cache.path()))).as_deref(), Some("Package A text."));

    server.take_down();
    let help = load_with(&b, &context(cache.path()));
    assert!(help.origin.is_none(), "no package of B was ever downloaded");
    assert!(help.missing_reason.unwrap().contains("no previously downloaded package"));

    // Nor does `auto` use the external cache: it degrades to the built-in facts.
    let auto = load_with(&PlatformHelpRequest::Auto, &context(cache.path()));
    assert_eq!(auto.origin.map(|o| o.source), Some(PlatformHelpSourceKind::Bundled));
    assert!(auto.missing_reason.unwrap().contains("no saved auto snapshot"));
}

#[test]
fn offline_sources_never_reach_the_server() {
    let server = Server::start();
    let cache = tempfile::tempdir().unwrap();
    server.publish("a", "Package A text.");
    for request in [
        PlatformHelpRequest::None,
        PlatformHelpRequest::Bundled,
        PlatformHelpRequest::Auto,
        PlatformHelpRequest::Installed { path: None },
    ] {
        let _ = load_with(&request, &context(cache.path()));
    }
    assert_eq!(server.requests.load(Ordering::SeqCst), 0);
}

#[test]
fn concurrent_first_downloads_publish_one_valid_package() {
    let server = Server::start();
    let cache = tempfile::tempdir().unwrap();
    server.publish("a", "Package A text.");
    let url = server.url("/a/manifest.json");
    let cache_dir: PathBuf = cache.path().to_path_buf();
    let workers: Vec<_> = (0..4)
        .map(|_| {
            let url = url.clone();
            let cache_dir = cache_dir.clone();
            std::thread::spawn(move || add_text(load_with(&url_request(url), &context(&cache_dir))))
        })
        .collect();
    for worker in workers {
        assert_eq!(worker.join().unwrap().as_deref(), Some("Package A text."));
    }
    let slot =
        package::Slot::new(platform_help::external::cache_dir_for(&url, &context(&cache_dir)));
    assert!(slot.read().is_ok(), "the cached package is whole");
}
