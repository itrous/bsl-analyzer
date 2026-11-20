//! `UnavailableAliasInQuery`: the alias of the enclosing query named inside `ИЗ (...) КАК П`.
//!
//! The platform rejects it with `Поле не найдено "Внеш.Ссылка"` and accepts the same reference
//! in `ГДЕ ... В (ВЫБРАТЬ ...)` (issue #82). The ticket's two queries are fixtures here, byte for
//! byte: D, which must be reported, and C, which must not. Both go through the two routes of the
//! product — bare query text (the MCP tool, the CLI) and a module that holds the query in a
//! multi-line string — and a route that stayed silent on D, or loud on C, fails.
//!
//! The bare route is checked with and without a metadata resolver: the rule is structural and
//! must not wait for a configuration.

use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use ide_diagnostics::{
    Diagnostic, DiagnosticCode, DiagnosticTag, DiagnosticsConfig, DiagnosticsContext, Severity,
};
use std::path::PathBuf;
use vfs::{FileId, FileSet, VfsPath};

const CODE: DiagnosticCode = DiagnosticCode::UnavailableAliasInQuery;

const D: &str = include_str!("../fixtures/derived_table_outer_alias/d.sdbl");
const C: &str = include_str!("../fixtures/derived_table_outer_alias/c.sdbl");
const D_MODULE: &str = include_str!("../fixtures/derived_table_outer_alias/d.bsl");
const C_MODULE: &str = include_str!("../fixtures/derived_table_outer_alias/c.bsl");

const DESIGNER_FIXTURE: &str =
    concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-metadata/fixtures/designer");

const MESSAGE: &str = "Псевдоним \"Внеш\" внешнего запроса недоступен внутри производной таблицы";

/// A configuration the designer fixture loads into: it has no `АктИнвентаризацииСАТУРН`, which
/// is the point — an unknown configuration is no reason to stay silent about a name the query
/// itself proves unreachable.
fn configuration() -> bsl_metadata::Configuration {
    bsl_metadata::load_from_directory(PathBuf::from(DESIGNER_FIXTURE))
        .expect("the designer fixture loads")
}

fn bare(query: &str, config: &DiagnosticsConfig) -> Vec<Diagnostic> {
    ide_diagnostics::validate_query_text(config, None, query)
}

fn bare_with_metadata(query: &str, config: &DiagnosticsConfig) -> Vec<Diagnostic> {
    ide_diagnostics::validate_query_text(config, Some(&configuration()), query)
}

/// A module as a plain file: the production pipeline with no configuration behind it.
fn module(source: &str, config: &DiagnosticsConfig) -> Vec<Diagnostic> {
    let mut db = RootDatabaseImpl::new();
    let file_id = FileId(0);
    let mut files = FileSet::default();
    files.insert(file_id, VfsPath::new("/test.bsl".to_string()));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(files));
    db.set_file_source_root(file_id, SourceRootId(0));
    db.set_file_text(file_id, source);
    ide_diagnostics::file_diagnostics(&db, file_id, config)
}

/// The same module in a project whose configuration is loaded.
fn module_with_metadata(source: &str, config: &DiagnosticsConfig) -> Vec<Diagnostic> {
    let mut db = RootDatabaseImpl::new();
    let file_id = FileId(0);
    let mut files = FileSet::default();
    files.insert(file_id, VfsPath::new(format!("{DESIGNER_FIXTURE}/Ext/SessionModule.bsl")));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(files));
    db.set_file_source_root(file_id, SourceRootId(0));
    db.set_file_text(file_id, source);

    let configuration_path_input = ide_db::metadata::ConfigurationPathInput::new(
        &db,
        PathBuf::from(DESIGNER_FIXTURE).to_string_lossy().to_string(),
        0,
    );
    let provider = ide_db::SalsaProvider::new(&db, Some(configuration_path_input));
    let ctx = DiagnosticsContext::new(config, file_id, &provider);
    ide_diagnostics::diagnostics(&ctx)
}

fn only(diagnostics: &[Diagnostic], code: DiagnosticCode) -> Vec<&Diagnostic> {
    diagnostics.iter().filter(|d| d.code == code).collect()
}

fn text_of<'a>(source: &'a str, diagnostic: &Diagnostic) -> &'a str {
    &source[usize::from(diagnostic.range.start())..usize::from(diagnostic.range.end())]
}

/// Zero-based line and character column of the first `needle` in `source`.
fn position_of(source: &str, needle: &str) -> (usize, usize) {
    let before = &source[..source.find(needle).expect("the needle is in the source")];
    let line = before.matches('\n').count();
    let column = before.rsplit('\n').next().unwrap_or("").chars().count();
    (line, column)
}

/// The finding D must carry in `source`: one, on the `Внеш` of the derived table's `ГДЕ`.
/// `Внеш.Ссылка` is written twice in D; the first is the inner one, the second sits in the
/// outer `ПО` and is legal.
fn assert_d_is_reported_once(label: &str, source: &str, diagnostics: &[Diagnostic]) {
    let found = only(diagnostics, CODE);
    assert_eq!(found.len(), 1, "[{label}] D must be reported exactly once: {diagnostics:?}");
    let finding = found[0];

    assert_eq!(finding.message, MESSAGE, "[{label}]");
    assert_eq!(finding.severity, Severity::Blocker, "[{label}]");
    assert_eq!(text_of(source, finding), "Внеш", "[{label}] the range is the alias token only");
    assert_eq!(
        usize::from(finding.range.start()),
        source.find("Внеш.Ссылка").expect("D names the alias"),
        "[{label}] the inner reference, not the legal one in the outer `ПО`",
    );
    assert!(finding.fixes.is_empty(), "[{label}] moving a condition can change the result");
}

fn assert_c_is_quiet(label: &str, diagnostics: &[Diagnostic]) {
    assert!(
        only(diagnostics, CODE).is_empty(),
        "[{label}] a correlated subquery in a condition is legal: {diagnostics:?}",
    );
}

// ---- I1, I2, I10: D and C, both routes ------------------------------------------------------

#[test]
fn d_is_reported_in_bare_text_with_and_without_metadata() {
    let config = DiagnosticsConfig::all_enabled();

    assert_d_is_reported_once("bare, no resolver", D, &bare(D, &config));
    assert_d_is_reported_once("bare, designer fixture", D, &bare_with_metadata(D, &config));
}

#[test]
fn d_is_reported_in_a_module_with_and_without_metadata() {
    let config = DiagnosticsConfig::all_enabled();

    assert_d_is_reported_once("module, plain file", D_MODULE, &module(D_MODULE, &config));
    assert_d_is_reported_once(
        "module, designer fixture",
        D_MODULE,
        &module_with_metadata(D_MODULE, &config),
    );
}

#[test]
fn c_is_quiet_on_every_route() {
    let config = DiagnosticsConfig::all_enabled();

    assert_c_is_quiet("bare, no resolver", &bare(C, &config));
    assert_c_is_quiet("bare, designer fixture", &bare_with_metadata(C, &config));
    assert_c_is_quiet("module, plain file", &module(C_MODULE, &config));
    assert_c_is_quiet("module, designer fixture", &module_with_metadata(C_MODULE, &config));
}

/// Silence on C proves nothing alone: a route that loses the rule is silent on both. This pins
/// that the same routes, in the same state, DO answer on D.
#[test]
fn the_silence_on_c_is_not_a_route_that_lost_the_rule() {
    let config = DiagnosticsConfig::all_enabled();

    for (label, d, c) in [
        ("bare", bare(D, &config), bare(C, &config)),
        ("module", module(D_MODULE, &config), module(C_MODULE, &config)),
    ] {
        assert_eq!(only(&d, CODE).len(), 1, "[{label}] D: {d:?}");
        assert_eq!(only(&c, CODE).len(), 0, "[{label}] C: {c:?}");
    }
}

// ---- I10: coordinates -----------------------------------------------------------------------

#[test]
fn each_route_reports_the_finding_in_its_own_coordinates() {
    let config = DiagnosticsConfig::all_enabled();

    // The bare route counts from the start of the query text.
    let bare_found = bare(D, &config);
    let bare_output = only(&bare_found, CODE)[0].to_output(D);
    let (line, column) = position_of(D, "Внеш.Ссылка");
    assert_eq!(
        (bare_output.start_line, bare_output.start_column),
        (line, column),
        "{bare_output:?}",
    );
    assert_eq!(
        (bare_output.end_line, bare_output.end_column),
        (line, column + "Внеш".chars().count())
    );

    // The module route counts in the module, past the `Запрос.Текст = "` prefix and the `|` that
    // opens every continuation line of the string.
    let module_found = module(D_MODULE, &config);
    let module_output = only(&module_found, CODE)[0].to_output(D_MODULE);
    let (line, column) = position_of(D_MODULE, "Внеш.Ссылка");
    assert_eq!(
        (module_output.start_line, module_output.start_column),
        (line, column),
        "{module_output:?}",
    );
    assert_eq!(
        (module_output.end_line, module_output.end_column),
        (line, column + "Внеш".chars().count())
    );

    assert_ne!(
        (bare_output.start_line, bare_output.start_column),
        (module_output.start_line, module_output.start_column),
        "the two routes must not report the same coordinates: one of them is not mapped",
    );
}

/// The module fixture is the ticket's query written the way a module holds it. Were it to
/// drift from the bare fixture, the two routes would be compared on different queries.
#[test]
fn the_module_fixtures_hold_the_bare_queries_byte_for_byte() {
    fn query_of(module: &str) -> String {
        let start = module.find("Запрос.Текст = \"").expect("the module sets the text")
            + "Запрос.Текст = \"".len();
        let end = start + module[start..].find("\";").expect("the string ends");
        module[start..end]
            .split('\n')
            .enumerate()
            .map(|(index, line)| {
                if index == 0 {
                    line.to_string()
                } else {
                    line.trim_start().strip_prefix('|').expect("continuation line").to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    assert_eq!(query_of(D_MODULE), D);
    assert_eq!(query_of(C_MODULE), C);
}

// ---- I10: the routes agree on what they say -------------------------------------------------

fn claims(diagnostics: &[Diagnostic]) -> Vec<(String, String)> {
    let query_codes = ide_diagnostics::sdbl_query_codes();
    let mut claims: Vec<_> = diagnostics
        .iter()
        .filter(|d| query_codes.contains(&d.code))
        .map(|d| (d.code.as_str().to_string(), d.message.clone()))
        .collect();
    claims.sort();
    claims
}

#[test]
fn both_routes_make_the_same_claims_on_d_and_on_c() {
    let config = DiagnosticsConfig::all_enabled();

    for (label, bare_text, module_text) in [("D", D, D_MODULE), ("C", C, C_MODULE)] {
        let from_text = claims(&bare_with_metadata(bare_text, &config));
        let from_module = claims(&module_with_metadata(module_text, &config));

        assert_eq!(from_text, from_module, "[{label}] the routes disagree");
        assert_eq!(
            from_text.iter().any(|(code, _)| code == CODE.as_str()),
            label == "D",
            "[{label}] the parity set is vacuous unless D carries the code and C does not: \
             {from_text:?}",
        );
    }
}

#[test]
fn the_code_belongs_to_the_query_set_and_needs_no_metadata() {
    assert!(ide_diagnostics::sdbl_query_codes().contains(&CODE));
    assert!(
        !ide_diagnostics::METADATA_DEPENDENT_CODES.contains(&CODE),
        "the name is unreachable by the query's own text; waiting for metadata would silence it",
    );
}

// ---- I7: no cascade ------------------------------------------------------------------------

/// With metadata behind the query, a hidden alias that still resolved would also report its
/// missing field as an unknown one. The reference is the unavailable alias and nothing more.
#[test]
fn a_hidden_alias_does_not_cascade_into_an_unknown_field_finding() {
    let config = DiagnosticsConfig::all_enabled();
    let hidden = "ВЫБРАТЬ П.К КАК К ИЗ Справочник.Справочник1 КАК Внеш \
                  ВНУТРЕННЕЕ СОЕДИНЕНИЕ (ВЫБРАТЬ 1 КАК К ИЗ Справочник.Справочник1 КАК Т \
                  ГДЕ Внеш.НетТакогоПоля = 1) КАК П ПО ИСТИНА";
    let control = "ВЫБРАТЬ Внеш.НетТакогоПоля КАК К ИЗ Справочник.Справочник1 КАК Внеш";
    let in_module = format!(
        "Процедура Тест()\n    Запрос = Новый Запрос;\n    Запрос.Текст = \"{hidden}\";\nКонецПроцедуры"
    );

    for (label, found) in [
        ("bare", bare_with_metadata(hidden, &config)),
        ("module", module_with_metadata(&in_module, &config)),
    ] {
        assert_eq!(only(&found, CODE).len(), 1, "[{label}] {found:?}");
        assert!(
            only(&found, DiagnosticCode::UnknownFieldInQuery).is_empty(),
            "[{label}] the field of a source the reference cannot reach is not judged: {found:?}",
        );
    }

    // The control: the same field of the same catalog IS unknown where the alias is reachable.
    let found = bare_with_metadata(control, &config);
    assert_eq!(only(&found, DiagnosticCode::UnknownFieldInQuery).len(), 1, "{found:?}");
    assert!(only(&found, CODE).is_empty(), "{found:?}");
}

// ---- I9: findings that are not new ----------------------------------------------------------

#[test]
fn d_keeps_the_join_with_a_subquery_warning() {
    let config = DiagnosticsConfig::all_enabled();

    for (label, found) in [
        ("bare", bare(D, &config)),
        ("module", module(D_MODULE, &config)),
        ("module, designer fixture", module_with_metadata(D_MODULE, &config)),
    ] {
        assert_eq!(
            only(&found, DiagnosticCode::JoinWithSubQuery).len(),
            1,
            "[{label}] the performance warning is independent of the new error: {found:?}",
        );
        assert_eq!(only(&found, CODE).len(), 1, "[{label}] {found:?}");
    }
}

// ---- I11: configuration ---------------------------------------------------------------------

fn project_config(section: serde_json::Value) -> DiagnosticsConfig {
    DiagnosticsConfig::from_project_json(&section, base_db::Locale::default())
}

#[test]
fn the_code_is_enabled_by_default_and_is_a_blocking_error() {
    let metadata = ide_diagnostics::get_metadata(CODE).expect("the code has metadata");

    assert!(metadata.activated_by_default);
    assert_eq!(metadata.diagnostic_type, ide_diagnostics::DiagnosticType::Error);
    assert_eq!(metadata.effective_severity(), Severity::Blocker);

    let config = DiagnosticsConfig::default();
    assert_d_is_reported_once("bare, default config", D, &bare(D, &config));
    assert_d_is_reported_once("module, default config", D_MODULE, &module(D_MODULE, &config));
}

#[test]
fn disabling_the_code_silences_it_on_both_routes_and_nothing_else() {
    let config = project_config(serde_json::json!({
        "parameters": { "UnavailableAliasInQuery": false }
    }));

    for (label, found) in [
        ("bare", bare(D, &config)),
        ("bare, designer fixture", bare_with_metadata(D, &config)),
        ("module", module(D_MODULE, &config)),
        ("module, designer fixture", module_with_metadata(D_MODULE, &config)),
    ] {
        assert!(only(&found, CODE).is_empty(), "[{label}] a disabled code reports: {found:?}");
        assert_eq!(
            only(&found, DiagnosticCode::JoinWithSubQuery).len(),
            1,
            "[{label}] only the named code goes silent: {found:?}",
        );
    }
}

/// The selective run (`--only-diagnostic`): the collector of the module route is entered only
/// when some enabled code is one of its own, so the code must be among them.
#[test]
fn a_run_selecting_only_the_code_still_reports_it() {
    let mut only_new = DiagnosticsConfig::all_enabled();
    only_new.only_enabled = Some(vec![CODE]);
    assert_d_is_reported_once("bare, only the code", D, &bare(D, &only_new));
    assert_d_is_reported_once("module, only the code", D_MODULE, &module(D_MODULE, &only_new));

    // The control: selecting another rule of the same collector leaves D unreported, so the
    // two runs above selected something.
    let mut only_other = DiagnosticsConfig::all_enabled();
    only_other.only_enabled = Some(vec![DiagnosticCode::DuplicateAliasInQuery]);
    assert!(only(&bare(D, &only_other), CODE).is_empty());
    assert!(only(&module(D_MODULE, &only_other), CODE).is_empty());
}

#[test]
fn severity_and_tags_come_from_the_configuration() {
    let config = project_config(serde_json::json!({
        "metadataOverrides": {
            "UnavailableAliasInQuery": { "lspSeverity": "hint", "tags": ["Unused"] }
        }
    }));

    for (label, source, found) in
        [("bare", D, bare(D, &config)), ("module", D_MODULE, module(D_MODULE, &config))]
    {
        let found = only(&found, CODE);
        assert_eq!(found.len(), 1, "[{label}]");
        assert_eq!(found[0].severity, Severity::Hint, "[{label}] the override replaces Blocker");
        assert_eq!(found[0].tags, vec![DiagnosticTag::Unnecessary], "[{label}]");
        assert_eq!(text_of(source, found[0]), "Внеш", "[{label}]");
    }

    // The control: with no override the same finding is a Blocker and carries no tag, so the
    // override above changed something.
    let default_found = bare(D, &DiagnosticsConfig::all_enabled());
    let default_finding = only(&default_found, CODE)[0];
    assert_eq!(default_finding.severity, Severity::Blocker);
    assert!(default_finding.tags.is_empty(), "{:?}", default_finding.tags);
}
