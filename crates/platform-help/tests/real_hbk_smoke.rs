//! Acceptance smoke on the real help archives of an installed platform.
//!
//! Needs a directory holding unmodified `shcntx_ru.hbk` and `shlang_ru.hbk`
//! named by its platform version, e.g. copied out of the Docker platform
//! (`docs/contributing/DEVELOPMENT_RULES.md`). Run:
//!
//! ```text
//! BSL_PLATFORM_HELP_SMOKE_DIR=$HOME/.cache/bsl-analyzer/platform-help-smoke/8.3.27.2214 \
//!   cargo test -p platform-help --test real_hbk_smoke -- --ignored --nocapture
//! ```
//!
//! Ignored in the offline suite; without its input it fails rather than skips.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use bsl_platform::{
    PlatformData, PlatformHelpRequest, PlatformHelpSourceKind, PlatformHelpStatus, PlatformSnapshot,
};
use platform_help::{load_with, package, LoadContext};

const SMOKE_DIR_ENV: &str = "BSL_PLATFORM_HELP_SMOKE_DIR";

fn smoke_dir() -> PathBuf {
    let dir = std::env::var_os(SMOKE_DIR_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|| panic!("{SMOKE_DIR_ENV} must name the real HBK directory"));
    for name in ["shcntx_ru.hbk", "shlang_ru.hbk"] {
        assert!(dir.join(name).is_file(), "{} lacks {name}", dir.display());
    }
    dir
}

fn context(cache: &Path) -> LoadContext {
    LoadContext {
        cache_dir: cache.to_path_buf(),
        discovery_roots: Vec::new(),
        platform_path_env: None,
        pinned: None,
    }
}

fn assert_array_add(data: &PlatformData) {
    let add = data.get_method("Массив", "Добавить").expect("Массив.Добавить by RU names");
    let by_en = data.get_method("Array", "Add").expect("Array.Add by EN names");
    assert_eq!(add.id, by_en.id);
    let docs = data.get_method_docs(add.id).expect("Массив.Добавить docs");
    assert!(!docs.description.trim().is_empty(), "non-empty description");
    assert!(docs.syntax.contains("Добавить"), "syntax: {}", docs.syntax);
    println!(
        "Массив.Добавить: syntax {:?}; description {} chars",
        docs.syntax,
        docs.description.len()
    );
}

#[test]
#[ignore = "real HBK smoke: set BSL_PLATFORM_HELP_SMOKE_DIR and run with --ignored"]
fn native_reader_extracts_the_real_archive_pair() {
    let dir = smoke_dir();
    for name in ["shcntx_ru.hbk", "shlang_ru.hbk"] {
        let bytes = fs::read(dir.join(name)).unwrap();
        println!("{name}: sha256 {}", package::sha256_hex(&bytes));
    }
    let scratch = tempfile::tempdir_in(dir.parent().unwrap()).unwrap();
    let started = Instant::now();
    let data = html_parser::extract_corpus_from_hbk(
        &dir.join("shcntx_ru.hbk"),
        &dir.join("shlang_ru.hbk"),
        &scratch.path().join("work"),
    )
    .expect("the native reader extracts the real pair");
    println!(
        "extracted {} types, {} methods in {:?} (extractor {})",
        data.type_count(),
        data.method_count(),
        started.elapsed(),
        html_parser::EXTRACTOR_VERSION
    );
    let json = data.to_json().unwrap();
    if let Some(out) = std::env::var_os("BSL_PLATFORM_HELP_SMOKE_OUT") {
        fs::write(out, &json).unwrap();
    }

    // The language archive is unpacked as well: its pages, not the static
    // keyword texts, prove that it was read.
    let shlang = scratch.path().join("shlang");
    let files =
        html_parser::hbk::extract_file_storage(&dir.join("shlang_ru.hbk"), &shlang).unwrap();
    println!("shlang_ru.hbk: {files} files");
    assert!(files > 0 && shlang.join("struct_For.st").is_file());

    let snapshot = PlatformSnapshot::from_corpus_json(&json).expect("corpus decodes with overlays");
    assert!(snapshot.types.len() > 1000 && snapshot.methods.len() > 1000);
    let data = PlatformData::from_help(bsl_platform::PlatformHelp::loaded(
        PlatformHelpRequest::Installed { path: Some(dir.clone()) },
        snapshot,
        bsl_platform::PlatformHelpOrigin {
            source: PlatformHelpSourceKind::Installed,
            location: None,
            platform_version: None,
            digest: None,
        },
    ));
    assert_array_add(&data);
}

#[test]
#[ignore = "real HBK smoke: set BSL_PLATFORM_HELP_SMOKE_DIR and run with --ignored"]
fn installed_mode_serves_the_real_pair_and_reuses_its_cache() {
    let dir = smoke_dir();
    let cache = tempfile::tempdir_in(dir.parent().unwrap()).unwrap();
    let request = PlatformHelpRequest::Installed { path: Some(dir.clone()) };

    let started = Instant::now();
    let help = load_with(&request, &context(cache.path()));
    let cold = started.elapsed();
    let origin = help.origin.clone().unwrap_or_else(|| panic!("{:?}", help.missing_reason));
    assert_eq!(origin.source, PlatformHelpSourceKind::Installed);
    let version = dir.file_name().unwrap().to_string_lossy().into_owned();
    assert_eq!(origin.platform_version.as_deref(), Some(version.as_str()));
    let data = PlatformData::from_help(help);
    assert_eq!(data.help_status_for_target(Some(&version)), PlatformHelpStatus::Available);
    assert_array_add(&data);

    let started = Instant::now();
    let again = load_with(&request, &context(cache.path()));
    let warm = started.elapsed();
    assert_eq!(again.origin.unwrap().digest, origin.digest, "same corpus from the cache");
    println!("installed: cold {cold:?}, cached {warm:?}, corpus sha256 {:?}", origin.digest);
    assert!(warm < cold, "the second load reuses the extracted package");
}

#[test]
#[ignore = "real HBK smoke: set BSL_PLATFORM_HELP_SMOKE_DIR and run with --ignored"]
fn damaged_copies_of_each_archive_are_missing() {
    let dir = smoke_dir();
    for damaged in ["shcntx_ru.hbk", "shlang_ru.hbk"] {
        let scratch = tempfile::tempdir_in(dir.parent().unwrap()).unwrap();
        let copy = scratch.path().join(dir.file_name().unwrap());
        fs::create_dir_all(&copy).unwrap();
        for name in ["shcntx_ru.hbk", "shlang_ru.hbk"] {
            let mut bytes = fs::read(dir.join(name)).unwrap();
            if name == damaged {
                // Clobber the container's descriptor block.
                for byte in &mut bytes[16..64] {
                    *byte = b'#';
                }
            }
            fs::write(copy.join(name), bytes).unwrap();
        }
        let help = load_with(
            &PlatformHelpRequest::Installed { path: Some(copy) },
            &context(&scratch.path().join("cache")),
        );
        assert!(help.origin.is_none(), "damaged {damaged} must not load");
        println!("damaged {damaged}: {}", help.missing_reason.unwrap());
    }
}
