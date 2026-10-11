//! Library use has no discovery: an unselected process reads only its explicit
//! corpus environment variable, otherwise it serves the built-in interface
//! facts. A named corpus that fails is never replaced by the facts.

use std::process::Command;

use bsl_platform::{
    active_platform_help_request, install_platform_help, PlatformData, PlatformGlobalCatalog,
    PlatformHelp, PlatformHelpRequest, PlatformHelpSourceKind, PlatformHelpStatus, CORPUS_ENV,
};

const CHILD_MODE: &str = "BSL_HELP_LIBRARY_TEST_MODE";
const FIXTURE: &str = include_str!("fixtures/help/corpus.json");

#[test]
fn library_default_is_fixed_on_first_access_and_never_discovers_an_installation() {
    if let Ok(mode) = std::env::var(CHILD_MODE) {
        assert!(active_platform_help_request().is_none());
        if cfg!(corpus_contract) && mode != "fixture" {
            // The strict test profile must reject library use without its input,
            // whereas ordinary library use degrades to the built-in facts.
            let panic = std::panic::catch_unwind(PlatformGlobalCatalog::instance)
                .err()
                .expect("corpus-contract requires usable input");
            let reason = panic.downcast_ref::<String>().unwrap();
            assert!(reason.contains("corpus contract run without a usable corpus"), "{reason}");
            return;
        }
        // The catalog can be the first consumer; it must select the same help.
        assert!(!PlatformGlobalCatalog::instance().symbols().is_empty());
        let data = PlatformData::instance();
        if mode == "fixture" {
            assert_eq!(data.help_status_for_target(None), PlatformHelpStatus::Unverified);
            let add = data.get_method("array", "add").expect("environment-selected corpus");
            assert!(data
                .get_method_docs(add.id)
                .unwrap()
                .description
                .contains("appends one element"));
        } else if matches!(mode.as_str(), "url" | "malformed" | "blank_then_url") {
            // A URL is the application's input; the library neither reads a file
            // of that name nor serves something else in its place.
            assert_eq!(data.help_status_for_target(None), PlatformHelpStatus::Missing);
            let reason = data.help_missing_reason().unwrap();
            assert!(reason.contains("supported only by the bsl-analyzer application"), "{reason}");
            assert!(mode != "url" || reason.contains("host.invalid/m.json"), "{reason}");
            assert!(!reason.contains("secret") && !reason.contains("token"), "{reason}");
            assert!(data.all_types().is_empty() && data.all_methods().is_empty());
        } else if mode == "http" {
            let reason = data.help_missing_reason().unwrap();
            assert!(reason.contains("use `https` or a path"), "{reason}");
            assert!(!reason.contains("secret") && !reason.contains("token"), "{reason}");
        } else if mode == "absent" {
            // An explicitly named corpus that cannot load is an error to report,
            // not a reason to serve something else.
            assert_eq!(data.help_status_for_target(None), PlatformHelpStatus::Missing);
            assert!(data.all_types().is_empty() && data.all_methods().is_empty());
            assert!(data.help_missing_reason().unwrap().contains("absent.json"));
            assert!(data.get_keyword_docs("Если").is_some());
        } else {
            assert_eq!(data.help_request(), &PlatformHelpRequest::Unselected);
            let origin = data.help_origin().expect("the built-in facts serve");
            assert_eq!(origin.source, PlatformHelpSourceKind::Bundled);
            assert_eq!(data.help_status_for_target(Some("8.3.27")), PlatformHelpStatus::Available);
            assert!(data.help_missing_reason().is_none(), "nothing richer was asked for");
            let add = data.get_method("array", "add").expect("facts know the method");
            let docs = data.get_method_docs(add.id).expect("the signature line is a fact");
            assert!(docs.description.is_empty() && docs.examples.is_empty());
            assert!(!docs.syntax.is_empty());
            assert!(data.get_keyword_docs("Если").is_some());
        }
        let conflict =
            install_platform_help(PlatformHelp::without_io(&PlatformHelpRequest::None).unwrap())
                .unwrap_err();
        assert_eq!(&conflict.active, data.help_request());
        assert_eq!(conflict.requested, PlatformHelpRequest::None);
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let installation = dir.path().join("8.3.27.1");
    html_parser::fixture_hbk::write_pair(
        &installation,
        &html_parser::fixture_hbk::shcntx_pages(),
        101,
    );
    let corpus = dir.path().join("corpus.json");
    std::fs::write(&corpus, FIXTURE).unwrap();
    let absent = dir.path().join("absent.json");
    let url = std::path::PathBuf::from("https://user:secret@host.invalid/m.json?token=abc");
    let malformed = std::path::PathBuf::from("https:/user:secret@host.invalid/m.json?token=abc");
    let http = std::path::PathBuf::from("http://user:secret@host.invalid/m.json?token=abc");
    let blank_then_url =
        std::path::PathBuf::from("\nhttps://user:secret@host.invalid/m.json?token=abc");
    for (mode, path) in [
        ("unset", None),
        ("empty", Some(std::path::Path::new(""))),
        ("blank", Some(std::path::Path::new("   "))),
        ("fixture", Some(corpus.as_path())),
        ("absent", Some(absent.as_path())),
        ("url", Some(url.as_path())),
        ("malformed", Some(malformed.as_path())),
        ("blank_then_url", Some(blank_then_url.as_path())),
        ("http", Some(http.as_path())),
    ] {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "library_default_is_fixed_on_first_access_and_never_discovers_an_installation",
                "--nocapture",
            ])
            .env(CHILD_MODE, mode)
            .env_remove(CORPUS_ENV)
            .env("BSL_PLATFORM_PATH", &installation)
            .env("BSL_PLATFORM_HELP_CACHE_DIR", dir.path().join("cache"));
        if let Some(path) = path {
            command.env(CORPUS_ENV, path);
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{mode}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert!(!dir.path().join("cache").exists());
}
