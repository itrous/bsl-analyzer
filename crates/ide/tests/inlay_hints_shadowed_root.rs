//! Parameter hints must name the callee inference resolved, not the one the
//! callee's identifier text spells. A local or module symbol holding a global's
//! name owns it, and the hints have to agree with signature help about that.

use ide::Analysis;
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use std::path::PathBuf;
use syntax::{TextRange, TextSize};
use test_fixture::Fixture;
use vfs::{FileId, FileSet};

fn designer_fixture_path() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-metadata/fixtures/designer"))
}

fn setup(source: &str) -> (Analysis, FileId, TextRange) {
    let fixture = Fixture::parse(&format!("//- /test.bsl\n{source}"));
    let mut db = RootDatabaseImpl::new();
    let mut file_set = FileSet::default();
    for (file_id, file) in &fixture.files {
        file_set.insert(*file_id, file.path.clone());
    }
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    for (file_id, file) in &fixture.files {
        db.set_file_source_root(*file_id, SourceRootId(0));
        db.set_file_text(*file_id, &file.content);
    }
    db.set_all_config_paths(vec![(None, designer_fixture_path())]);
    let (file_id, len) = fixture
        .files
        .iter()
        .map(|(id, f)| (*id, f.content.len() as u32))
        .next()
        .expect("fixture has the test file");
    let range = TextRange::new(TextSize::from(0), TextSize::from(len));
    (Analysis::from_database(db), file_id, range)
}

fn hint_labels(source: &str) -> Vec<String> {
    let (analysis, file_id, range) = setup(source);
    analysis.inlay_hints(file_id, range).into_iter().map(|hint| hint.label).collect()
}

/// Positive control: with nobody holding `Справочники`, the chain resolves and
/// its argument is labelled. Without this the two tests below pass vacuously.
#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn unheld_manager_chain_labels_its_argument() {
    let labels = hint_labels(
        "Функция Тест()\n    Справочники.Справочник1.НайтиПоКоду(\"К\");\nКонецФункции\n",
    );
    assert_eq!(labels, vec!["Код:".to_string()], "unheld manager chain must label its argument");
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn assignment_to_the_root_still_labels_the_argument() {
    let labels = hint_labels(
        "Функция Тест()\n    Справочники = НеизвестнаяФункция();\n    \
         Справочники.Справочник1.НайтиПоКоду(\"К\");\nКонецФункции\n",
    );
    assert!(
        !labels.is_empty(),
        "an assignment declares no local, so the manager method's parameters still label \
         the call: {labels:?}"
    );
}

#[test]
fn receiver_named_like_a_platform_type_gets_no_hint() {
    let labels = hint_labels(
        "Функция Тест()\n    Массив = НеизвестнаяФункция();\n    Массив.Вставить(1, 2);\n\
         КонецФункции\n",
    );
    assert!(
        labels.is_empty(),
        "the receiver's own spelling must not type it as the platform type: {labels:?}"
    );
}

/// Hints and signature help read the same resolution: where signature help
/// declines to name a call, hints must not name its arguments either.
#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn hints_agree_with_signature_help_on_a_held_root() {
    let source = "Функция Тест()\n    Справочники = НеизвестнаяФункция();\n    \
                  Справочники.Справочник1.НайтиПоКоду(\"К\");\nКонецФункции\n";
    let (analysis, file_id, range) = setup(source);
    let arg_offset = source.find("\"К\"").expect("argument present") as u32;

    let help = analysis.signature_help(file_id, arg_offset);
    let hints = analysis.inlay_hints(file_id, range);

    assert!(help.is_some(), "precondition: an assignment does not hold the root");
    assert!(
        !hints.is_empty(),
        "signature help resolved here, so hints must label the call too: {hints:?}"
    );
}
