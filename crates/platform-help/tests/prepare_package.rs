//! Both package inputs produce the same runtime-readable format, retaining the
//! unmodified corpus and a content notice separate from the code license.

use std::fs;

use bsl_platform::{PlatformData, PlatformHelpRequest};
use platform_help::{load_with, prepare_package, LoadContext, PackageInput, PACKAGE_NOTICE};

const FIXTURE: &[u8] = include_bytes!("../../bsl-platform/tests/fixtures/help/corpus.json");

#[test]
fn imported_corpus_stays_unmodified_and_carries_its_content_notice() {
    let dir = tempfile::tempdir().unwrap();
    let corpus = dir.path().join("input.json");
    fs::write(&corpus, FIXTURE).unwrap();
    let output = dir.path().join("package");
    let manifest = prepare_package(
        PackageInput::CorpusJson(&corpus),
        &output,
        "self-written-fixture",
        Some("8.3.27.1"),
    )
    .unwrap();
    assert_eq!(manifest.sha256, platform_help::package::sha256_hex(FIXTURE));
    assert_eq!(fs::read(output.join(&manifest.corpus_file)).unwrap(), FIXTURE);
    let notice = fs::read_to_string(output.join("NOTICE.md")).unwrap();
    assert_eq!(notice, PACKAGE_NOTICE);
    assert!(
        notice.contains("1С-Софт") && notice.contains("**not** covered by bsl-analyzer's"),
        "{notice}"
    );
    let context = LoadContext {
        cache_dir: dir.path().join("cache"),
        discovery_roots: Vec::new(),
        platform_path_env: None,
    };
    let help = load_with(&PlatformHelpRequest::ExternalPath(output), &context);
    assert_eq!(help.origin.as_ref().unwrap().platform_version.as_deref(), Some("8.3.27.1"));
    let data = PlatformData::from_help(help);
    assert!(
        data.get_property("ClientApplicationForm", "РежимОткрытияОкна").is_some(),
        "runtime overlays still apply"
    );
    assert!(data.get_method("Array", "Add").is_some());
}

#[test]
fn archive_input_uses_the_native_reader_and_produces_served_method_docs() {
    let dir = tempfile::tempdir().unwrap();
    let installation = dir.path().join("8.3.27.1");
    html_parser::fixture_hbk::write_pair(
        &installation,
        &html_parser::fixture_hbk::shcntx_pages(),
        101,
    );
    let output = dir.path().join("package");
    let manifest =
        prepare_package(PackageInput::Archives(&installation), &output, "fixture", None).unwrap();
    assert_eq!(manifest.extractor_version, html_parser::EXTRACTOR_VERSION);
    assert_eq!(manifest.platform_version.as_deref(), Some("8.3.27.1"));
    let context = LoadContext {
        cache_dir: dir.path().join("cache"),
        discovery_roots: Vec::new(),
        platform_path_env: None,
    };
    let data =
        PlatformData::from_help(load_with(&PlatformHelpRequest::ExternalPath(output), &context));
    let add = data.get_method("Array", "Add").expect("extracted method");
    assert!(data.get_method_docs(add.id).unwrap().description.contains("appends one element"));
}
