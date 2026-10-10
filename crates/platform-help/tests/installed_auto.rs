//! `installed` and `auto` on self-written help archives, with an isolated cache
//! and discovery root: extraction, cache reuse, re-extraction on change, the
//! saved `auto` snapshot, and failures that never fall back to another source
//! (`auto` alone degrades to the built-in interface facts).

use std::fs;
use std::path::{Path, PathBuf};

use bsl_platform::{PlatformData, PlatformHelp, PlatformHelpRequest, PlatformHelpSourceKind};
use html_parser::fixture_hbk;
use platform_help::{load_with, LoadContext};

struct Stand {
    _dir: tempfile::TempDir,
    root: PathBuf,
    cache: PathBuf,
}

impl Stand {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("1cv8");
        let cache = dir.path().join("cache");
        fs::create_dir_all(&root).unwrap();
        Self { root, cache, _dir: dir }
    }

    fn context(&self) -> LoadContext {
        LoadContext {
            cache_dir: self.cache.clone(),
            discovery_roots: vec![self.root.clone()],
            platform_path_env: None,
            pinned: None,
        }
    }

    fn install(&self, version: &str, add_text: &str) -> PathBuf {
        let dir = self.root.join(version);
        let mut pages = fixture_hbk::shcntx_pages();
        pages[1].1 = pages[1].1.replace("appends one element", add_text);
        fixture_hbk::write_pair(&dir, &pages, 101);
        dir
    }
}

fn add_description(help: PlatformHelp) -> Option<String> {
    let data = PlatformData::from_help(help);
    let add = data.get_method("Массив", "Добавить")?;
    data.get_method_docs(add.id).map(|docs| docs.description)
}

fn tree_snapshot(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut out = Vec::new();
    for entry in walk(dir) {
        out.push((entry.clone(), fs::read(&entry).unwrap()));
    }
    out.sort();
    out
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for entry in fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            files.extend(walk(&path));
        } else {
            files.push(path);
        }
    }
    files
}

#[test]
fn installed_extracts_reuses_cache_and_reextracts_changed_archives() {
    let stand = Stand::new();
    let platform = stand.install("8.3.27.9", "first wording");
    let before = tree_snapshot(&stand.root);
    let request = PlatformHelpRequest::Installed { path: None };

    let help = load_with(&request, &stand.context());
    let origin = help.origin.clone().unwrap_or_else(|| panic!("{:?}", help.missing_reason));
    assert_eq!(origin.source, PlatformHelpSourceKind::Installed);
    assert_eq!(origin.platform_version.as_deref(), Some("8.3.27.9"));
    assert_eq!(origin.location.as_deref(), Some(platform.display().to_string().as_str()));
    assert!(add_description(help).unwrap().contains("first wording"));
    assert_eq!(tree_snapshot(&stand.root), before, "the installation is never written");

    // Cached: the second load serves the cached package, which the test marks
    // so that a re-extraction could not produce the same answer.
    let packages: Vec<PathBuf> =
        fs::read_dir(stand.cache.join("installed")).unwrap().map(|e| e.unwrap().path()).collect();
    assert_eq!(packages.len(), 1);
    let slot = platform_help::package::Slot::new(packages[0].clone());
    let corpus = slot.read().unwrap();
    let marked =
        String::from_utf8(corpus.bytes).unwrap().replace("first wording", "cached wording");
    let manifest = corpus.manifest.unwrap();
    slot.publish(
        marked.as_bytes(),
        &manifest.corpus_id,
        manifest.platform_version.as_deref(),
        &manifest.extractor_version,
        &[],
    )
    .unwrap();
    let again = load_with(&request, &stand.context());
    assert!(add_description(again).unwrap().contains("cached wording"), "cache reused");

    // Changed archives: a new extraction with the new text.
    stand.install("8.3.27.9", "second wording");
    let changed = load_with(&request, &stand.context());
    assert_ne!(changed.origin.as_ref().unwrap().digest, origin.digest);
    assert_eq!(fs::read_dir(stand.cache.join("installed")).unwrap().count(), 2);
    assert!(add_description(changed).unwrap().contains("second wording"));
}

#[test]
fn changing_only_the_language_archive_invalidates_installed_and_auto_cache() {
    let stand = Stand::new();
    let platform = stand.install("8.3.27.9", "original wording");
    let request = PlatformHelpRequest::Installed { path: Some(platform.clone()) };
    assert!(add_description(load_with(&request, &stand.context()))
        .unwrap()
        .contains("original wording"));
    assert!(add_description(load_with(&PlatformHelpRequest::Auto, &stand.context()))
        .unwrap()
        .contains("original wording"));

    let slots: Vec<_> = fs::read_dir(stand.cache.join("installed"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .chain(std::iter::once(stand.cache.join("auto")))
        .collect();
    for dir in slots {
        let slot = platform_help::package::Slot::new(dir);
        let published = slot.current(&["source.json"]).unwrap();
        let identity = published.extra("source.json").map(<[u8]>::to_vec);
        let extra: Vec<_> =
            identity.as_ref().map(|bytes| ("source.json", bytes.as_slice())).into_iter().collect();
        let marked = String::from_utf8(published.corpus.bytes)
            .unwrap()
            .replace("original wording", "stale cache wording");
        let manifest = published.corpus.manifest.unwrap();
        slot.publish(
            marked.as_bytes(),
            &manifest.corpus_id,
            manifest.platform_version.as_deref(),
            &manifest.extractor_version,
            &extra,
        )
        .unwrap();
    }
    for source in [&request, &PlatformHelpRequest::Auto] {
        assert!(
            add_description(load_with(source, &stand.context()))
                .unwrap()
                .contains("stale cache wording"),
            "unchanged inputs reuse the marked cache"
        );
    }
    let context_bytes = fs::read(platform.join("shcntx_ru.hbk")).unwrap();
    let language_before = fs::read(platform.join("shlang_ru.hbk")).unwrap();
    fixture_hbk::write_pair(&platform, &fixture_hbk::shcntx_pages(), 37);
    fs::write(platform.join("shcntx_ru.hbk"), context_bytes).unwrap();
    assert_ne!(fs::read(platform.join("shlang_ru.hbk")).unwrap(), language_before);

    for source in [request, PlatformHelpRequest::Auto] {
        let text = add_description(load_with(&source, &stand.context())).unwrap();
        assert!(text.contains("original wording"), "changed shlang requires extraction: {text}");
    }
    assert_eq!(fs::read_dir(stand.cache.join("installed")).unwrap().count(), 2);
}

#[test]
fn none_does_not_read_a_discoverable_installation_or_create_a_cache() {
    let stand = Stand::new();
    stand.install("8.3.27.9", "available wording");
    let before = tree_snapshot(&stand.root);
    let help = load_with(&PlatformHelpRequest::None, &stand.context());
    assert!(help.snapshot.is_empty() && help.origin.is_none());
    assert!(!stand.cache.exists(), "none must not extract or publish anything");
    assert_eq!(tree_snapshot(&stand.root), before);
    assert!(
        add_description(load_with(&PlatformHelpRequest::Auto, &stand.context()))
            .unwrap()
            .contains("available wording"),
        "the same discoverable source is usable"
    );
}

#[test]
fn concurrent_first_loads_in_one_process_all_succeed() {
    let stand = Stand::new();
    stand.install("8.3.27.9", "parallel wording");
    let context = stand.context();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(4));
    let workers: Vec<_> = (0..4)
        .map(|index| {
            let context = context.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let request = if index % 2 == 0 {
                    PlatformHelpRequest::Installed { path: None }
                } else {
                    PlatformHelpRequest::Auto
                };
                barrier.wait();
                let help = load_with(&request, &context);
                help.missing_reason.clone().map_or_else(|| add_description(help), Some)
            })
        })
        .collect();
    for worker in workers {
        let text = worker.join().unwrap().unwrap();
        assert!(text.contains("parallel wording"), "{text}");
    }
}

#[test]
fn installed_failures_are_missing_and_never_another_installation() {
    let stand = Stand::new();
    stand.install("8.3.27.9", "discoverable");

    let wrong = stand.root.join("absent");
    let help = load_with(&PlatformHelpRequest::Installed { path: Some(wrong) }, &stand.context());
    assert!(help.origin.is_none() && help.snapshot.is_empty());
    assert!(help.missing_reason.unwrap().contains("absent"));

    let broken = stand.root.join("8.3.28.1");
    stand.install("8.3.28.1", "broken");
    let shcntx = broken.join("shcntx_ru.hbk");
    let bytes = fs::read(&shcntx).unwrap();
    fs::write(&shcntx, &bytes[..bytes.len() / 2]).unwrap();
    let help = load_with(&PlatformHelpRequest::Installed { path: Some(broken) }, &stand.context());
    assert!(help.origin.is_none(), "a damaged archive is not a corpus");
    assert!(help.missing_reason.unwrap().contains("extraction"), "the reason names the failure");

    let nothing = Stand::new();
    let help = load_with(&PlatformHelpRequest::Installed { path: None }, &nothing.context());
    assert!(help.missing_reason.unwrap().contains("no installed platform"));
}

#[test]
fn auto_saves_its_snapshot_and_serves_it_after_the_installation_is_gone() {
    let stand = Stand::new();
    let platform = stand.install("8.3.27.9", "auto wording");

    let first = load_with(&PlatformHelpRequest::Auto, &stand.context());
    assert_eq!(first.origin.as_ref().unwrap().source, PlatformHelpSourceKind::Installed);
    assert!(add_description(first).unwrap().contains("auto wording"));

    fs::remove_dir_all(&platform).unwrap();
    let later = load_with(&PlatformHelpRequest::Auto, &stand.context());
    let origin = later.origin.clone().expect("the saved auto snapshot serves");
    assert_eq!(origin.source, PlatformHelpSourceKind::Auto);
    assert!(add_description(later).unwrap().contains("auto wording"));

    // A changed installation replaces the saved snapshot.
    stand.install("8.3.27.9", "updated wording");
    let updated = load_with(&PlatformHelpRequest::Auto, &stand.context());
    assert!(add_description(updated).unwrap().contains("updated wording"));
}

#[test]
fn auto_ignores_other_sources_caches_and_serves_the_facts_without_its_own() {
    let stand = Stand::new();
    let other = stand.install("8.3.27.9", "installed only");
    // A successful explicit installed load fills the installed cache, not auto's.
    let installed =
        load_with(&PlatformHelpRequest::Installed { path: Some(other.clone()) }, &stand.context());
    assert!(installed.origin.is_some());
    fs::remove_dir_all(&other).unwrap();

    let help = load_with(&PlatformHelpRequest::Auto, &stand.context());
    let origin = help.origin.clone().expect("the built-in facts serve");
    assert_eq!(origin.source, PlatformHelpSourceKind::Bundled, "not another source's cache");
    assert!(help.missing_reason.clone().unwrap().contains("no saved auto snapshot"));
    assert_eq!(add_description(help).as_deref(), Some(""), "the facts carry no text");
}
