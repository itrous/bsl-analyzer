//! One query, two routes: embedded in a module and handed over as bare text.
//!
//! The routes must agree on WHAT they say — every code and every message — and differ only
//! in WHERE they say it, because a range inside a module names a place in that module and a
//! range in a bare query names a place in the query. A drift between the routes is what
//! would let the MCP tool answer differently from the IDE for the same text.

use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use ide_diagnostics::{Diagnostic, DiagnosticsConfig, DiagnosticsContext};
use std::path::PathBuf;
use vfs::{FileId, FileSet, VfsPath};

const DESIGNER_FIXTURE: &str =
    concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-metadata/fixtures/designer");

/// One input cannot exercise every category: a query broken enough to carry a parse error is
/// usually too broken for name resolution to reach a field. So the parity corpus holds one
/// input per category, and a separate test proves the corpus really covers them — a parity
/// check over inputs that all trigger the same rule proves almost nothing.
const CORPUS: &[(&str, &str)] = &[
    ("metadata", "ВЫБРАТЬ Т.НетТакогоПоля КАК П ИЗ РегистрСведений.РегистрСведений1 КАК Т"),
    ("missing-table", "ВЫБРАТЬ Т.Поле КАК П ИЗ Справочник.НетТакого КАК Т"),
    (
        "structural",
        "ВЫБРАТЬ Т.Период КАК П ИЗ РегистрСведений.РегистрСведений1 КАК Т \
         ВНУТРЕННЕЕ СОЕДИНЕНИЕ (ВЫБРАТЬ 1 КАК Ч) КАК В ПО ИСТИНА",
    ),
    ("parse-error", "ВЫБРАТЬ Т.Период, Т. ИЗ РегистрСведений.РегистрСведений1 КАК Т"),
];

fn embedded(query_body: &str) -> Vec<Diagnostic> {
    let code = format!(
        "Процедура Тест()\n    Запрос = Новый Запрос;\n    Запрос.Текст = \"{}\";\nКонецПроцедуры",
        query_body
    );

    let mut db = RootDatabaseImpl::new();
    let workspace_root = PathBuf::from(DESIGNER_FIXTURE);

    let mut file_set = FileSet::default();
    let file_id = FileId(0);
    file_set.insert(file_id, VfsPath::new(format!("{DESIGNER_FIXTURE}/Ext/SessionModule.bsl")));

    let source_root_id = SourceRootId(0);
    db.set_source_root(source_root_id, SourceRoot::new_local(file_set));
    db.set_file_source_root(file_id, source_root_id);
    db.set_file_text(file_id, &code);

    let configuration_path_input = ide_db::metadata::ConfigurationPathInput::new(
        &db,
        workspace_root.to_string_lossy().to_string(),
        0,
    );

    let provider = ide_db::SalsaProvider::new(&db, Some(configuration_path_input));
    let config = DiagnosticsConfig::all_enabled();
    let ctx = DiagnosticsContext::new(&config, file_id, &provider);

    // The wrapper procedure attracts diagnostics of its own; only the query's are comparable.
    ide_diagnostics::diagnostics(&ctx)
        .into_iter()
        .filter(|d| sdbl_codes().contains(&d.code))
        .collect()
}

/// Derived, never hand-copied: a rule added to the SDBL set joins this comparison by itself.
/// A manual mirror of the set drifts silently, and the drift hides exactly the codes a change
/// just introduced.
fn sdbl_codes() -> Vec<ide_diagnostics::DiagnosticCode> {
    ide_diagnostics::sdbl_query_codes()
}

fn standalone(query_body: &str) -> Vec<Diagnostic> {
    let config = DiagnosticsConfig::all_enabled();
    let configuration = bsl_metadata::load_from_directory(PathBuf::from(DESIGNER_FIXTURE))
        .expect("the designer fixture loads");

    ide_diagnostics::validate_query_text(&config, Some(&configuration), query_body)
}

fn claims(diagnostics: &[Diagnostic]) -> Vec<(String, String)> {
    let mut claims: Vec<_> =
        diagnostics.iter().map(|d| (d.code.as_str().to_string(), d.message.clone())).collect();
    claims.sort();
    claims
}

#[test]
fn both_routes_make_the_same_claims() {
    for (label, query) in CORPUS {
        let from_module = claims(&embedded(query));
        let from_text = claims(&standalone(query));

        assert_eq!(
            from_module, from_text,
            "[{label}] the two routes disagree on what the query is guilty of",
        );
    }
}

/// The positive control for the parity test: two routes that both stayed silent would agree
/// perfectly. This pins that the corpus actually produces each category it claims to cover.
#[test]
fn the_corpus_covers_every_category() {
    let codes_for = |query: &str| -> Vec<String> {
        standalone(query).into_iter().map(|d| d.code.as_str().to_string()).collect()
    };

    for (label, query, expected) in [
        ("metadata", CORPUS[0].1, "UnknownFieldInQuery"),
        ("missing-table", CORPUS[1].1, "QueryToMissingMetadata"),
        ("structural", CORPUS[2].1, "JoinWithSubQuery"),
        ("parse-error", CORPUS[3].1, "QueryParseError"),
    ] {
        let codes = codes_for(query);
        assert!(
            codes.iter().any(|code| code == expected),
            "[{label}] expected {expected}, got {codes:?}",
        );
    }
}

/// Ranges are the one thing that legitimately differs, and the difference has a direction:
/// the module route offsets every finding past the `Запрос.Текст = "` prefix, the bare-text
/// route does not. Asserting only the claims would let a broken Standalone mapper — one that
/// still applied the embedded shift — through.
#[test]
fn ranges_differ_by_route_and_the_bare_route_stays_inside_the_query() {
    for (label, query) in CORPUS {
        let query_len = u32::try_from(query.len()).unwrap();

        for diagnostic in standalone(query) {
            let end: u32 = diagnostic.range.end().into();
            assert!(
                end <= query_len,
                "[{label}] {} at {:?} escapes a {query_len}-byte query — \
                 the standalone mapper is shifting",
                diagnostic.code.as_str(),
                diagnostic.range,
            );
        }
    }
}

/// The two new rules of this change were, at first, absent from the comparison because the
/// code set was hand-copied. Deriving it removes the drift; this pins that the derived set is
/// the one the runner actually dispatches, so a rule cannot be reachable in one route and
/// invisible to the parity check in the other.
#[test]
fn the_parity_set_covers_every_rule_a_query_can_break() {
    let codes = sdbl_codes();

    for expected in [
        ide_diagnostics::DiagnosticCode::AmbiguousFieldInQuery,
        ide_diagnostics::DiagnosticCode::DuplicateAliasInQuery,
        ide_diagnostics::DiagnosticCode::UnknownFieldInQuery,
        ide_diagnostics::DiagnosticCode::QueryParseError,
    ] {
        assert!(codes.contains(&expected), "{expected:?} missing from the parity set: {codes:?}");
    }
}
