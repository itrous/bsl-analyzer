//! `auto`'s last network step: the pinned corpus, downloaded from a local HTTP
//! fixture standing in for the release asset. Only `auto` without a usable
//! snapshot or installation reaches it, only the pinned digest is accepted, and
//! when it cannot serve, `auto` degrades to the built-in interface facts.

mod common;

use std::path::{Path, PathBuf};

use bsl_platform::{PlatformData, PlatformHelp, PlatformHelpRequest, PlatformHelpSourceKind};
use common::{corpus_with_add_text, Server};
use html_parser::fixture_hbk;
use platform_help::pinned::PinnedCorpus;
use platform_help::{load_with, package, LoadContext};

const ASSET: &str = "/corpus-1/platform_data.json";

fn context(cache: &Path, pinned: Option<PinnedCorpus>) -> LoadContext {
    LoadContext {
        cache_dir: cache.to_path_buf(),
        discovery_roots: Vec::new(),
        platform_path_env: None,
        pinned,
    }
}

/// A server publishing a corpus whose `Массив.Добавить` says `text`, and the
/// pin naming it.
fn published(text: &str) -> (Server, PinnedCorpus) {
    let server = Server::start();
    let corpus = corpus_with_add_text(text);
    server.put(ASSET, &corpus);
    let pinned =
        PinnedCorpus { url: server.url(ASSET), sha256: Some(package::sha256_hex(&corpus)) };
    (server, pinned)
}

fn add_text(help: PlatformHelp) -> Option<String> {
    let data = PlatformData::from_help(help);
    let add = data.get_method("Массив", "Добавить")?;
    data.get_method_docs(add.id).map(|docs| docs.description)
}

/// `auto` fell back to the compiled-in interface facts: `Массив.Добавить` is
/// known without any text, and the reason the pinned corpus did not serve is
/// kept. Nothing of a refused download may leak into the answer.
fn degraded_to_facts(help: PlatformHelp) -> String {
    let origin = help.origin.clone().unwrap_or_else(|| panic!("{:?}", help.missing_reason));
    assert_eq!(origin.source, PlatformHelpSourceKind::Bundled);
    assert_eq!(origin.platform_version.as_deref(), Some(bsl_platform::BUNDLED_PLATFORM_VERSION));
    let reason = help.missing_reason.clone().expect("the reason for degrading is kept");
    assert_eq!(add_text(help).as_deref(), Some(""), "facts carry no text");
    reason
}

#[test]
fn auto_downloads_the_pinned_corpus_once_then_serves_it_offline() {
    let (server, pinned) = published("Pinned corpus text.");
    let cache = tempfile::tempdir().unwrap();
    let ctx = context(cache.path(), Some(pinned.clone()));

    let help = load_with(&PlatformHelpRequest::Auto, &ctx);
    let origin = help.origin.clone().unwrap_or_else(|| panic!("{:?}", help.missing_reason));
    assert_eq!(origin.source, PlatformHelpSourceKind::Auto);
    assert_eq!(origin.location.as_deref(), Some(pinned.url.as_str()));
    assert_eq!(origin.digest.as_deref(), pinned.sha256.as_deref());
    assert_eq!(add_text(help).as_deref(), Some("Pinned corpus text."));
    assert_eq!(server.request_count(), 1);

    let notice = package::Slot::new(pinned.cache_dir(&ctx)).current(&["NOTICE.md"]).unwrap();
    assert!(
        String::from_utf8_lossy(notice.extra("NOTICE.md").unwrap()).contains("ООО «1С-Софт»"),
        "the cached corpus keeps its notice"
    );

    server.take_down();
    let again = load_with(&PlatformHelpRequest::Auto, &ctx);
    assert_eq!(add_text(again).as_deref(), Some("Pinned corpus text."));
    assert_eq!(server.request_count(), 1, "the cached download needs no request");
}

#[test]
fn a_download_with_another_digest_or_no_corpus_is_refused_and_not_cached() {
    let (server, pinned) = published("Genuine text.");
    let cache = tempfile::tempdir().unwrap();
    let ctx = context(cache.path(), Some(pinned.clone()));

    // Same length, different bytes: only the digest tells them apart.
    let forged = corpus_with_add_text("Forged  text.");
    assert_eq!(forged.len(), corpus_with_add_text("Genuine text.").len());
    server.put(ASSET, &forged);
    let help = load_with(&PlatformHelpRequest::Auto, &ctx);
    assert!(degraded_to_facts(help).contains("SHA-256"));
    assert!(!pinned.cache_dir(&ctx).join("current").exists(), "nothing is cached");

    // A pinned file that is not a corpus is refused as well.
    let junk = b"{\"types\": 1}".to_vec();
    server.put(ASSET, &junk);
    let junk_pin =
        PinnedCorpus { url: pinned.url.clone(), sha256: Some(package::sha256_hex(&junk)) };
    let junk_ctx = context(cache.path(), Some(junk_pin.clone()));
    let help = load_with(&PlatformHelpRequest::Auto, &junk_ctx);
    assert!(degraded_to_facts(help).contains("invalid help corpus"));
    assert!(!junk_pin.cache_dir(&junk_ctx).join("current").exists());

    server.take_down();
    let help = load_with(&PlatformHelpRequest::Auto, &ctx);
    assert!(degraded_to_facts(help).contains("pinned corpus"), "an outage without a cache");
}

#[test]
fn an_unreachable_host_degrades_to_the_facts_without_a_cached_package() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/platform_data.json", listener.local_addr().unwrap());
    drop(listener);
    let pinned = PinnedCorpus {
        url: url.clone(),
        sha256: Some(package::sha256_hex(&corpus_with_add_text("Expected text."))),
    };
    let cache = tempfile::tempdir().unwrap();
    let ctx = context(cache.path(), Some(pinned.clone()));
    let help = load_with(&PlatformHelpRequest::Auto, &ctx);
    let reason = degraded_to_facts(help);
    assert!(reason.contains("pinned corpus") && reason.contains(&url), "{reason}");
    assert!(package::Slot::new(pinned.cache_dir(&ctx)).read().is_err());
}

#[test]
fn a_cached_package_with_the_wrong_pin_is_not_served_and_can_be_replaced() {
    let (server, pinned) = published("Expected text.");
    let cache = tempfile::tempdir().unwrap();
    let ctx = context(cache.path(), Some(pinned.clone()));
    let slot = package::Slot::new(pinned.cache_dir(&ctx));
    slot.publish(&corpus_with_add_text("Wrong cached text."), "wrong", None, "test", &[]).unwrap();
    assert_ne!(slot.read().unwrap().sha256, pinned.sha256.as_deref().unwrap());

    server.take_down();
    let degraded = load_with(&PlatformHelpRequest::Auto, &ctx);
    assert!(degraded_to_facts(degraded).contains("pinned corpus"));
    assert_eq!(server.request_count(), 1, "an invalid cache requires a new download");

    server.put(ASSET, &corpus_with_add_text("Expected text."));
    let loaded = load_with(&PlatformHelpRequest::Auto, &ctx);
    assert_eq!(add_text(loaded).as_deref(), Some("Expected text."));
    assert_eq!(slot.read().unwrap().sha256, pinned.sha256.unwrap());
    assert_eq!(server.request_count(), 2);
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: requires the pinned JSON input")]
fn the_compiled_in_digest_accepts_the_corpus_contract_input() {
    let input = std::env::var_os(bsl_platform::CORPUS_ENV)
        .map(PathBuf::from)
        .expect("the corpus-contract input must be supplied");
    let bytes = std::fs::read(&input).unwrap();
    let pinned = PinnedCorpus::compiled_in();
    assert_eq!(package::sha256_hex(&bytes), pinned.sha256.as_deref().unwrap());
    let server = Server::start();
    server.put(ASSET, &bytes);
    let pinned = PinnedCorpus { url: server.url(ASSET), ..pinned };
    // The full corpus cache belongs beside the external input, not in RAM-backed /tmp.
    let cache = tempfile::tempdir_in(input.parent().unwrap()).unwrap();
    let ctx = context(cache.path(), Some(pinned.clone()));
    let help = load_with(&PlatformHelpRequest::Auto, &ctx);
    assert_eq!(help.origin.as_ref().unwrap().digest, pinned.sha256);
    let data = PlatformData::from_help(help);
    let add = data.get_method("Массив", "Добавить").expect("pinned corpus method");
    assert_eq!(data.get_method("Array", "Add").unwrap().id, add.id);
    assert!(!data.get_method_docs(add.id).unwrap().description.trim().is_empty());
    assert_eq!(server.request_count(), 1);

    server.take_down();
    let cached = load_with(&PlatformHelpRequest::Auto, &ctx);
    assert_eq!(cached.origin.as_ref().unwrap().digest, pinned.sha256);
    assert!(add_text(cached).is_some());
    assert_eq!(server.request_count(), 1, "the compiled pin reuses its cache without HTTP");
}

#[test]
fn a_changed_pin_downloads_its_own_corpus() {
    let (server, first) = published("First pin.");
    let cache = tempfile::tempdir().unwrap();
    let help = load_with(&PlatformHelpRequest::Auto, &context(cache.path(), Some(first)));
    assert_eq!(add_text(help).as_deref(), Some("First pin."));

    let second_corpus = corpus_with_add_text("Second pin.");
    server.put("/corpus-2/platform_data.json", &second_corpus);
    let second = PinnedCorpus {
        url: server.url("/corpus-2/platform_data.json"),
        sha256: Some(package::sha256_hex(&second_corpus)),
    };
    let help = load_with(&PlatformHelpRequest::Auto, &context(cache.path(), Some(second)));
    assert_eq!(add_text(help).as_deref(), Some("Second pin."));
    assert_eq!(server.request_count(), 2);
}

#[test]
fn a_mirror_serves_its_own_file_without_a_digest_and_keeps_its_own_cache() {
    let (server, _) = published("Mirrored text.");
    let cache = tempfile::tempdir().unwrap();
    let mirror = PinnedCorpus { url: server.url(ASSET), sha256: None };
    let ctx = context(cache.path(), Some(mirror.clone()));
    let help = load_with(&PlatformHelpRequest::Auto, &ctx);
    assert_eq!(add_text(help).as_deref(), Some("Mirrored text."));

    // The cached copy serves later starts; another mirror gets its own.
    server.put("/other/platform_data.json", &corpus_with_add_text("Other mirror."));
    let other = PinnedCorpus { url: server.url("/other/platform_data.json"), sha256: None };
    let help = load_with(&PlatformHelpRequest::Auto, &context(cache.path(), Some(other)));
    assert_eq!(add_text(help).as_deref(), Some("Other mirror."));
    server.take_down();
    let help = load_with(&PlatformHelpRequest::Auto, &ctx);
    assert_eq!(add_text(help).as_deref(), Some("Mirrored text."));
    assert_eq!(server.request_count(), 2);

    // A mirror's file must still be a corpus.
    let junk = PinnedCorpus { url: server.url("/junk.json"), sha256: None };
    server.put("/junk.json", b"{\"types\": 1}");
    let help = load_with(&PlatformHelpRequest::Auto, &context(cache.path(), Some(junk)));
    assert!(help.missing_reason.as_deref().unwrap().contains("invalid help corpus"), "{help:?}");
}

#[test]
fn the_environment_names_a_mirror_without_a_digest_or_turns_the_download_off() {
    // Read in a child process: the variable is the process's own.
    let probe = |value: Option<&std::ffi::OsStr>| {
        let exe = std::env::current_exe().unwrap();
        let mut command = std::process::Command::new(exe);
        command.args(["--exact", "pinned_from_environment_probe", "--nocapture", "--ignored"]);
        match value {
            Some(value) => command.env(platform_help::pinned::PINNED_URL_ENV, value),
            None => command.env_remove(platform_help::pinned::PINNED_URL_ENV),
        };
        let output = command.output().unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        stdout
            .lines()
            .find_map(|l| l.split_once("PROBE ").map(|(_, probed)| probed))
            .unwrap()
            .to_owned()
    };
    assert_eq!(probe(Some("".as_ref())), "None");
    assert_eq!(
        probe(Some("https://mirror.example/corpus.json".as_ref())),
        format!(
            "{:?}",
            Some(PinnedCorpus { url: "https://mirror.example/corpus.json".into(), sha256: None })
        )
    );
    assert_eq!(probe(None), format!("{:?}", Some(PinnedCorpus::compiled_in())));
    #[cfg(unix)]
    {
        // A mirror the analyzer cannot read is never replaced by the pinned URL.
        use std::os::unix::ffi::OsStrExt;
        assert_eq!(
            probe(Some(std::ffi::OsStr::from_bytes(b"https://mirror.example/\xff"))),
            "None"
        );
    }
}

#[test]
#[ignore = "helper of the_environment_names_a_mirror_without_a_digest_or_turns_the_download_off"]
fn pinned_from_environment_probe() {
    println!("PROBE {:?}", PinnedCorpus::from_environment());
}

#[test]
fn a_slow_mirror_download_does_not_roll_back_a_newer_publication() {
    use std::io::{BufRead, BufReader, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/mirror.json", listener.local_addr().unwrap());
    let (release, released) = std::sync::mpsc::channel::<()>();
    let (accepted, on_accept) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        while reader.read_line(&mut line).is_ok_and(|n| n > 0) && line != "\r\n" {
            line.clear();
        }
        accepted.send(()).unwrap();
        released.recv().unwrap();
        let body = corpus_with_add_text("Older mirror text.");
        let mut stream = stream;
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(head.as_bytes()).unwrap();
        stream.write_all(&body).unwrap();
    });
    let cache = tempfile::tempdir().unwrap();
    let mirror = PinnedCorpus { url, sha256: None };
    let ctx = context(cache.path(), Some(mirror.clone()));
    let slow = {
        let ctx = ctx.clone();
        std::thread::spawn(move || load_with(&PlatformHelpRequest::Auto, &ctx))
    };
    // While the slow request waits, another start publishes a newer corpus.
    on_accept.recv().unwrap();
    let newer = corpus_with_add_text("Newer mirror text.");
    package::Slot::new(mirror.cache_dir(&ctx))
        .publish(&newer, "mirror", None, "published", &[])
        .unwrap();
    release.send(()).unwrap();
    assert_eq!(add_text(slow.join().unwrap()).as_deref(), Some("Older mirror text."));
    let help = load_with(&PlatformHelpRequest::Auto, &ctx);
    assert_eq!(add_text(help).as_deref(), Some("Newer mirror text."), "the newer copy stays");
}

fn installation(root: &Path, text: &str) -> PathBuf {
    let dir = root.join("8.3.27.9");
    let mut pages = fixture_hbk::shcntx_pages();
    pages[1].1 = pages[1].1.replace("appends one element", text);
    fixture_hbk::write_pair(&dir, &pages, 101);
    dir
}

#[test]
fn auto_with_an_installation_or_a_saved_snapshot_never_downloads() {
    let (server, pinned) = published("Pinned corpus text.");
    let root = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    installation(root.path(), "Installed text.");
    let ctx = LoadContext {
        discovery_roots: vec![root.path().to_path_buf()],
        ..context(cache.path(), Some(pinned.clone()))
    };
    let help = load_with(&PlatformHelpRequest::Auto, &ctx);
    assert!(add_text(help).unwrap().contains("Installed text."));

    // The installation is gone: the saved auto snapshot serves.
    let without_installation = context(cache.path(), Some(pinned));
    let help = load_with(&PlatformHelpRequest::Auto, &without_installation);
    assert!(add_text(help).unwrap().contains("Installed text."));
    assert_eq!(server.request_count(), 0);
}

#[test]
fn explicit_sources_and_a_disabled_download_never_reach_the_server() {
    let (server, pinned) = published("Pinned corpus text.");
    let cache = tempfile::tempdir().unwrap();
    let ctx = context(cache.path(), Some(pinned));
    let absent = cache.path().join("absent");
    for request in [
        PlatformHelpRequest::None,
        PlatformHelpRequest::Installed { path: None },
        PlatformHelpRequest::Installed { path: Some(absent.clone()) },
        PlatformHelpRequest::ExternalPath(absent.join("corpus.json")),
    ] {
        let help = load_with(&request, &ctx);
        assert!(help.origin.is_none(), "{request:?} never falls back to the pinned corpus");
    }
    // `bundled` is the facts by choice: no reason to record, nothing richer asked.
    let bundled = load_with(&PlatformHelpRequest::Bundled, &ctx);
    assert_eq!(bundled.origin.as_ref().map(|o| o.source), Some(PlatformHelpSourceKind::Bundled));
    assert!(bundled.missing_reason.is_none());
    assert_eq!(add_text(bundled).as_deref(), Some(""));

    let help = load_with(&PlatformHelpRequest::Auto, &context(cache.path(), None));
    assert!(
        degraded_to_facts(help).contains("turned off"),
        "a disabled download leaves auto with the facts and the reason"
    );
    assert_eq!(server.request_count(), 0);
    assert!(!cache.path().join("pinned").exists());
}

#[test]
fn the_compiled_in_pin_names_an_immutable_release_asset() {
    use platform_help::pinned::{PINNED_SHA256, PINNED_URL};
    assert!(
        PINNED_URL.starts_with("https://github.com/itrous/bsl-platform-help/releases/download/")
    );
    assert_eq!(
        PINNED_SHA256, "3c759994cbd82a1522b1c497d9f2ef68c77b776d5c6723ba5a72623b570cacce",
        "auto and corpus-contract must agree on the released corpus"
    );
    assert!(PINNED_URL.ends_with("/corpus-3c759994/platform_data.json"));
}
