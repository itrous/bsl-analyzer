//! The `diagnostics snippet` action: analyzer findings for BSL text that is not a file.
//!
//! An agent writes code it is about to run in a live session or paste into a module, and
//! wants the analyzer's verdict BEFORE that. The text is analysed the only way the analyzer
//! analyses anything — as a module of the workspace: it is placed into a HOST module (the one
//! the caller names, or a server/client common module of the configuration), statements are
//! wrapped into a procedure, and the host is overlaid in memory for the duration of the call
//! ([`crate::diagnostics_state::ResidentSession::read_overlaid`]). The host's own findings are
//! dropped and the snippet's findings come back with ranges relative to the snippet.
//!
//! Nothing is written to disk and the overlay never outlives the call, so the real file's
//! findings, the workspace index and every other answer stay as they were.

use std::path::{Path, PathBuf};

use ide::{DiagnosticCode, SeverityBucket, TextRange};
use serde_json::{json, Value};
use syntax::{SyntaxKind, TextSize};
use vfs::FileId;

use super::{
    budgeted_diagnostics_response, finding_value, fit_diagnostics_response_budget, Counts,
};
use crate::diagnostics_state::DiagnosticsResident;
use crate::tools::file_request::FileError;
use crate::tools::location as loc;
use crate::tools::response::trim_items_to_budget;

/// Upper bound on the snippet text. A fragment is meant to be read by a person before it
/// runs; a larger text is a module and belongs in a file.
pub(crate) const MAX_SNIPPET_BYTES: usize = 1 << 20;

/// Name of the procedure that wraps statements; suffixed when the host or the snippet
/// already uses it.
const WRAPPER_NAME: &str = "ПроверяемыйФрагмент";

/// Rules about the layout of a whole MODULE (regions, code before methods, methods nobody
/// calls). A fragment is not a module: these findings describe the wrapper and the host,
/// or code that is not meant to stand alone. They are dropped unless `codes` asks for them.
const MODULE_STRUCTURE_CODES: &[DiagnosticCode] = &[
    DiagnosticCode::CodeOutOfRegion,
    DiagnosticCode::EmptyRegion,
    DiagnosticCode::DuplicateRegion,
    DiagnosticCode::NonStandardRegion,
    DiagnosticCode::NonExportMethodsInApiRegion,
    DiagnosticCode::CodeBlockBeforeSub,
    DiagnosticCode::UnusedLocalMethod,
];

/// Where the snippet's code runs, for a host the caller did not name or a form module.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Environment {
    Server,
    Client,
}

impl Environment {
    pub(crate) fn parse(value: Option<&str>) -> Result<Option<Self>, String> {
        match value {
            None => Ok(None),
            Some("server") => Ok(Some(Self::Server)),
            Some("client") => Ok(Some(Self::Client)),
            Some(other) => Err(format!("unknown environment '{other}'; expected server|client")),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Server => "server",
            Self::Client => "client",
        }
    }

    fn directive(self) -> &'static str {
        match self {
            Self::Server => "&НаСервере",
            Self::Client => "&НаКлиенте",
        }
    }
}

/// How the text is placed into the host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CodeKind {
    /// Statements: wrapped into a procedure (a function when it returns a value).
    Statements,
    /// Methods, variables and directives of its own: appended to the host as they are.
    Module,
}

impl CodeKind {
    pub(crate) fn parse(value: Option<&str>) -> Result<Option<Self>, String> {
        match value {
            None => Ok(None),
            Some("statements") => Ok(Some(Self::Statements)),
            Some("module") => Ok(Some(Self::Module)),
            Some(other) => Err(format!("unknown code_kind '{other}'; expected statements|module")),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Statements => "statements",
            Self::Module => "module",
        }
    }
}

/// A validated `snippet` request.
#[derive(Debug)]
pub(crate) struct SnippetRequest {
    /// The text, without a leading BOM.
    pub(crate) code: String,
    pub(crate) code_kind: CodeKind,
    pub(crate) environment: Option<Environment>,
    pub(crate) known_variables: Vec<String>,
    pub(crate) path: Option<PathBuf>,
    pub(crate) root_id: Option<String>,
}

impl SnippetRequest {
    /// Check the request before anything touches the resident: every refusal here is the
    /// caller's to fix, and none of them depends on the workspace.
    pub(crate) fn new(
        code: String,
        code_kind: Option<CodeKind>,
        environment: Option<Environment>,
        known_variables: Vec<String>,
        path: Option<String>,
        root_id: Option<String>,
    ) -> Result<Self, String> {
        let code = code.strip_prefix('\u{feff}').map(str::to_owned).unwrap_or(code);
        if code.trim().is_empty() {
            return Err("'code' must not be empty for action 'snippet'".to_owned());
        }
        if code.len() > MAX_SNIPPET_BYTES {
            return Err(format!(
                "'code' is {} bytes; action 'snippet' accepts at most {MAX_SNIPPET_BYTES}",
                code.len()
            ));
        }
        let code_kind = code_kind.unwrap_or_else(|| {
            if crate::tools::check_executor::has_module_syntax(&code) || declares_variables(&code) {
                CodeKind::Module
            } else {
                CodeKind::Statements
            }
        });
        if let Some(bad) = known_variables.iter().find(|name| !is_identifier(name)) {
            return Err(format!("known_variables: '{bad}' is not a BSL identifier"));
        }
        if code_kind == CodeKind::Module && !known_variables.is_empty() {
            return Err("known_variables apply to code_kind 'statements' only: module code \
                        declares its own variables"
                .to_owned());
        }
        Ok(Self {
            code,
            code_kind,
            environment,
            known_variables,
            path: path.map(PathBuf::from),
            root_id,
        })
    }
}

/// Filters applied to the snippet's findings — the `file` action's, minus the line range.
pub(crate) struct SnippetFilters {
    pub(crate) min_severity: SeverityBucket,
    pub(crate) codes: Vec<String>,
    pub(crate) max_findings: usize,
    pub(crate) max_output_tokens: Option<usize>,
    pub(crate) detailed: bool,
}

/// What the plan worked out for the analysis step.
pub(crate) struct Prepared {
    /// Byte span of the snippet in the overlaid host text.
    snippet: TextRange,
    /// Where the host's own parse errors after the insertion point land in the overlaid
    /// text: any other parse error past the snippet is the snippet's (an unclosed block).
    host_parse_errors: Vec<TextSize>,
    context: Value,
}

/// An answer the plan gives instead of an analysis.
pub(crate) type Refusal = (Value, loc::Completeness);

/// Pick the host, build the overlaid text and say where the snippet sits in it.
pub(crate) fn plan(
    resident: &DiagnosticsResident,
    analysis: &ide::Analysis,
    request: &SnippetRequest,
    max_output_tokens: Option<usize>,
) -> Result<(FileId, String, Prepared), Refusal> {
    let refuse = |error: FileError, detail: &str, path: &Path| -> Refusal {
        let completeness = error.completeness();
        (
            budgeted_diagnostics_response(error.to_value(detail, path), max_output_tokens),
            completeness,
        )
    };

    let (file_id, host_path, chosen) = match &request.path {
        Some(requested) => {
            let resolved = match resident.resolve_rooted_path(request.root_id.as_deref(), requested)
            {
                Ok(resolved) => resolved,
                Err(error) => {
                    let detail = error.to_string();
                    return Err(refuse(FileError::Rooted(error), &detail, requested));
                }
            };
            let Some(file_id) = resident.file_id_for(&resolved) else {
                if resident.is_unread(&resolved) {
                    return Err(refuse(
                        FileError::Unreadable,
                        "workspace .bsl file exists but its bytes could not be read; it cannot \
                         host a snippet",
                        &resolved,
                    ));
                }
                return Err(refuse(
                    FileError::NotInWorkspace,
                    "path is not a resident workspace .bsl file",
                    &resolved,
                ));
            };
            (file_id, resident.abs_path_for(&resolved), "path")
        }
        None => {
            let environment = request.environment.unwrap_or(Environment::Server);
            match default_host(resident, analysis, environment) {
                Some((file_id, path)) => (file_id, path, "default"),
                None => {
                    let body = json!({
                        "error": "no_host_module",
                        "detail": format!(
                            "the configuration has no {} common module to analyse the snippet \
                             in; pass `path` to name a host module",
                            environment.as_str()
                        ),
                    });
                    return Err((
                        budgeted_diagnostics_response(body, max_output_tokens),
                        loc::Completeness::complete(),
                    ));
                }
            }
        }
    };

    let host_kind = HostKind::of(&host_path);
    let environment = match (chosen, host_kind, request.environment) {
        ("default", _, environment) => Some(environment.unwrap_or(Environment::Server)),
        (_, HostKind::Form, environment) if request.code_kind == CodeKind::Statements => {
            Some(environment.unwrap_or(Environment::Server))
        }
        (_, _, None) => None,
        (_, HostKind::Form, Some(_)) => {
            return Err(invalid(
                "environment cannot be applied to code_kind 'module': the code carries its own \
                 compilation directives",
                max_output_tokens,
            ));
        }
        (_, _, Some(_)) => {
            return Err(invalid(
                "environment applies to a form module host or to the default host; the \
                 environment of this host is fixed by its module",
                max_output_tokens,
            ));
        }
    };

    let host_text = analysis.file_text(file_id);
    let insert_at = insertion_offset(&host_text);
    let (prefix, suffix, wrapper) = match request.code_kind {
        CodeKind::Module => ("\n".to_owned(), "\n".to_owned(), "none"),
        CodeKind::Statements => {
            let name = wrapper_name(&host_text, &request.code);
            let returns_value = returns_value(&request.code);
            let (keyword, end_keyword) = if returns_value {
                ("Функция", "КонецФункции")
            } else {
                ("Процедура", "КонецПроцедуры")
            };
            let directive = match (host_kind, environment) {
                (HostKind::Form, Some(environment)) => format!("{}\n", environment.directive()),
                _ => String::new(),
            };
            let variables = if request.known_variables.is_empty() {
                String::new()
            } else {
                format!(" Перем {};", request.known_variables.join(", "))
            };
            (
                format!("\n{directive}{keyword} {name}(){variables}\n"),
                format!("\n{end_keyword}\n"),
                if returns_value { "function" } else { "procedure" },
            )
        }
    };

    let mut text = String::with_capacity(host_text.len() + prefix.len() + request.code.len() + 32);
    text.push_str(&host_text[..insert_at]);
    text.push_str(&prefix);
    let snippet_start = text.len();
    text.push_str(&request.code);
    let snippet_end = text.len();
    text.push_str(&suffix);
    let wrapper_end = text.len();
    text.push_str(&host_text[insert_at..]);
    // An unclosed block surfaces its missing closer anywhere up to the end of the text,
    // module code having no wrapper line to stop it. The text after the snippet is the host
    // tail shifted by the insertion, so the host's own errors there move by the same amount.
    let shift = offset(wrapper_end - insert_at);
    let host_parse_errors = parser::parse(&host_text)
        .errors()
        .iter()
        .map(|error| error.range().start())
        .filter(|start| usize::from(*start) >= insert_at)
        .map(|start| start + shift)
        .collect();

    let relative = host_path
        .strip_prefix(resident.workspace_root())
        .map(Path::to_path_buf)
        .unwrap_or_else(|_| host_path.clone());
    let context = json!({
        "path": relative.to_string_lossy().replace('\\', "/"),
        "chosen": chosen,
        "module": host_kind.as_str(),
        "environment": environment.map(Environment::as_str),
        "code_kind": request.code_kind.as_str(),
        "wrapper": wrapper,
    });
    Ok((
        file_id,
        text,
        Prepared {
            snippet: TextRange::new(offset(snippet_start), offset(snippet_end)),
            host_parse_errors,
            context,
        },
    ))
}

/// The findings of the snippet, from the overlaid host.
pub(crate) fn findings(
    resident: &DiagnosticsResident,
    analysis: &ide::Analysis,
    file_id: FileId,
    prepared: Prepared,
    code: &str,
    filters: &SnippetFilters,
) -> (Value, loc::Completeness) {
    // The project's own rule set and thresholds, but not its diff scope: the scope names
    // changed lines of real files, and the snippet's lines are in none of them.
    let mut config = resident.config().clone();
    config.scope = None;
    let result = analysis.diagnostics_result(file_id, &config);
    // A bounded rule projection that hit its work limit yields a subset of the host's
    // findings; the answer must not read as a complete verdict on the code.
    let analysis_incomplete = result.completeness.incomplete_reason().is_some();
    let diagnostics = result.diagnostics;

    let snippet = prepared.snippet;
    let explicitly_asked = |code: DiagnosticCode| filters.codes.iter().any(|c| c == code.as_str());
    let mut counts = Counts::default();
    let mut findings: Vec<Value> = Vec::new();
    let mut count_capped = false;
    let mut structure_omitted = 0usize;
    let mut structure_omitted_codes = std::collections::BTreeSet::new();

    for diag in diagnostics {
        let Some(range) = snippet_range(&diag, snippet, &prepared.host_parse_errors) else {
            continue;
        };
        if MODULE_STRUCTURE_CODES.contains(&diag.code) && !explicitly_asked(diag.code) {
            structure_omitted += 1;
            structure_omitted_codes.insert(diag.code.as_str());
            continue;
        }
        let local = ide::Diagnostic { range, ..diag };
        let out = local.to_output(code);
        if !filters.codes.is_empty() && !filters.codes.iter().any(|c| c == &out.code) {
            continue;
        }
        let bucket = SeverityBucket::from(local.severity);
        counts.add(bucket);
        if bucket < filters.min_severity {
            continue;
        }
        if findings.len() >= filters.max_findings {
            count_capped = true;
            continue;
        }
        findings.push(finding_value(&local, &out, bucket, None, filters.detailed, None));
    }

    let budget_exhausted = filters
        .max_output_tokens
        .map(|budget| trim_items_to_budget(&mut findings, budget))
        .unwrap_or(false);
    let mut body = json!({
        "kind": "snippet",
        "context": prepared.context,
        "counts": counts.to_value(),
        "truncated": count_capped || budget_exhausted,
        "findings": findings,
    });
    if structure_omitted > 0 {
        body["module_structure_omitted"] = json!(structure_omitted);
        body["module_structure_omitted_codes"] = json!(structure_omitted_codes);
    }
    if budget_exhausted {
        body["budget_exhausted"] = json!(true);
        body["budget_hint"] = json!(if count_capped {
            "findings truncated by both max_output_tokens and the max_findings cap; narrow with `codes`/`min_severity`, or raise BOTH max_output_tokens and max_findings"
        } else {
            "findings truncated to fit max_output_tokens; narrow with `codes`/`min_severity` or raise the budget"
        });
    }
    let budget_exhausted =
        fit_diagnostics_response_budget(&mut body, filters.max_output_tokens, "findings")
            || budget_exhausted;
    let completeness = loc::Completeness::complete()
        .when(
            analysis_incomplete,
            loc::ReasonCode::ModalityDegraded,
            "the notify projection hit its work limit; some rules did not contribute",
        )
        .when(
            budget_exhausted,
            loc::ReasonCode::OutputBudget,
            "findings trimmed to fit max_output_tokens",
        )
        .when(count_capped, loc::ReasonCode::ResultCap, "findings capped by max_findings");
    (body, completeness)
}

fn invalid(detail: &str, max_output_tokens: Option<usize>) -> Refusal {
    (
        budgeted_diagnostics_response(
            json!({ "error": "invalid_snippet_context", "detail": detail }),
            max_output_tokens,
        ),
        loc::Completeness::complete(),
    )
}

/// The snippet-relative range of a finding, or `None` when it belongs to the host or the
/// wrapper. A parse error past the snippet that the host does not have by itself is the
/// snippet's own unclosed block, and is pinned to the snippet's end.
fn snippet_range(
    diag: &ide::Diagnostic,
    snippet: TextRange,
    host_parse_errors: &[TextSize],
) -> Option<TextRange> {
    let start = diag.range.start();
    let end = diag.range.end();
    let (start, end) = if snippet.contains_inclusive(start) {
        (start, end.min(snippet.end()).max(start))
    } else if diag.code == DiagnosticCode::ParseError
        && start > snippet.end()
        && !host_parse_errors.contains(&start)
    {
        (snippet.end(), snippet.end())
    } else {
        return None;
    };
    Some(TextRange::new(start - snippet.start(), end - snippet.start()))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HostKind {
    CommonModule,
    Form,
    Other,
}

impl HostKind {
    fn of(path: &Path) -> Self {
        use bsl_conventions::{conventional_of, path_ends_with_ext_child, ConventionalName};
        let path = path.to_string_lossy().replace('\\', "/");
        // `…/Ext/Form/Module.bsl`, with at least one segment above `Ext`.
        let mut tail = path.rsplit('/');
        let (module, form, ext) = (tail.next(), tail.next(), tail.next());
        let is_form_module = tail.next().is_some()
            && module.and_then(conventional_of) == Some(ConventionalName::Module)
            && form.and_then(conventional_of) == Some(ConventionalName::Form)
            && ext.and_then(conventional_of) == Some(ConventionalName::Ext);
        if is_form_module {
            Self::Form
        } else if path.to_lowercase().contains("/commonmodules/")
            && path_ends_with_ext_child(&path, ConventionalName::Module)
        {
            Self::CommonModule
        } else {
            Self::Other
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::CommonModule => "common_module",
            Self::Form => "form",
            Self::Other => "other",
        }
    }
}

/// A common module of the main configuration whose code runs ONLY where `environment`
/// says, and is not global: a global module puts its methods into every other module's
/// scope, which is not the context a fragment asks about. The smallest one wins — its
/// whole text is analysed together with the snippet — and the path breaks ties, so the
/// choice is stable between calls.
fn default_host(
    resident: &DiagnosticsResident,
    analysis: &ide::Analysis,
    environment: Environment,
) -> Option<(FileId, PathBuf)> {
    let db = analysis.database();
    let roots = db.workspace_configs_snapshot();
    let mut best: Option<(u64, PathBuf, FileId)> = None;
    for (path, file_id) in resident.files() {
        if HostKind::of(path) != HostKind::CommonModule {
            continue;
        }
        if matches!(
            roots.owner_kind_of_path(path),
            Some(ide::WorkspaceRootKind::Extension | ide::WorkspaceRootKind::External(_))
        ) {
            continue;
        }
        let Some(module) = db.common_module_for_file_id(file_id) else { continue };
        let fits = !module.is_global()
            && !module.is_privileged()
            && match environment {
                Environment::Server => {
                    module.is_server()
                        && !module.is_client_managed_application()
                        && !module.is_client_ordinary_application()
                }
                Environment::Client => {
                    module.is_client_managed_application() && !module.is_server()
                }
            };
        if !fits {
            continue;
        }
        let size = std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(u64::MAX);
        let candidate = (size, path.to_path_buf(), file_id);
        if best.as_ref().is_none_or(|current| (candidate.0, &candidate.1) < (current.0, &current.1))
        {
            best = Some(candidate);
        }
    }
    best.map(|(_, path, file_id)| (file_id, path))
}

/// Where the snippet goes: right after the host's last method, so the methods before it
/// keep their offsets and a main-program section after it stays last. A host without
/// methods takes it after its module variables, or at the very start.
fn insertion_offset(host: &str) -> usize {
    let root = parser::parse(host).syntax_node();
    let inside_method = |node: &syntax::SyntaxNode| {
        node.ancestors()
            .skip(1)
            .any(|a| matches!(a.kind(), SyntaxKind::PROCEDURE_DEF | SyntaxKind::FUNCTION_DEF))
    };
    let last_method = root
        .descendants()
        .filter(|n| matches!(n.kind(), SyntaxKind::PROCEDURE_DEF | SyntaxKind::FUNCTION_DEF))
        .filter(|n| !inside_method(n))
        .map(|n| n.text_range().end())
        .max();
    let last_variable = || {
        root.descendants()
            .filter(|n| n.kind() == SyntaxKind::VAR_DEF && !inside_method(n))
            .map(|n| n.text_range().end())
            .max()
    };
    last_method.or_else(last_variable).map(usize::from).unwrap_or(0)
}

/// Whether the text declares module variables: `has_module_syntax` sees methods and
/// directives only, and a variable section is module code too.
fn declares_variables(code: &str) -> bool {
    parser::parse(code).syntax_node().descendants().any(|n| n.kind() == SyntaxKind::VAR_DEF)
}

/// Whether the statements return a value, which a procedure cannot.
fn returns_value(code: &str) -> bool {
    parser::parse(code)
        .syntax_node()
        .descendants()
        .filter(|n| n.kind() == SyntaxKind::RETURN_STMT)
        .any(|n| n.children().next().is_some())
}

fn wrapper_name(host: &str, code: &str) -> String {
    let taken = |name: &str| {
        let name = name.to_lowercase();
        host.to_lowercase().contains(&name) || code.to_lowercase().contains(&name)
    };
    let mut name = WRAPPER_NAME.to_owned();
    let mut n = 1;
    while taken(&name) {
        n += 1;
        name = format!("{WRAPPER_NAME}{n}");
    }
    name
}

fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_alphabetic() || c == '_')
        && chars.all(|c| c.is_alphanumeric() || c == '_')
}

fn offset(byte: usize) -> TextSize {
    TextSize::from(u32::try_from(byte).expect("a snippet host fits in u32 offsets"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insertion_goes_after_the_last_method() {
        let host = "Перем А;\nПроцедура П1()\nКонецПроцедуры\n\nА = 1;\n";
        let at = insertion_offset(host);
        assert_eq!(&host[..at], "Перем А;\nПроцедура П1()\nКонецПроцедуры");
        assert_eq!(insertion_offset("Перем А;\nА = 1;\n"), "Перем А;".len());
        assert_eq!(insertion_offset("А = 1;\n"), 0);
    }

    #[test]
    fn a_returned_value_needs_a_function() {
        assert!(returns_value("Если Истина Тогда\n Возврат 1;\nКонецЕсли;"));
        assert!(!returns_value("Возврат;"));
        assert!(!returns_value("А = 1;"));
    }

    #[test]
    fn the_wrapper_name_avoids_names_in_use() {
        assert_eq!(wrapper_name("", "А = 1;"), WRAPPER_NAME);
        assert_eq!(
            wrapper_name("Процедура ПроверяемыйФрагмент()\nКонецПроцедуры", ""),
            format!("{WRAPPER_NAME}2")
        );
    }

    #[test]
    fn request_validation_names_the_bad_field() {
        let request = |code: &str, vars: Vec<&str>| {
            SnippetRequest::new(
                code.to_owned(),
                None,
                None,
                vars.into_iter().map(str::to_owned).collect(),
                None,
                None,
            )
        };
        assert!(request("  \n", vec![]).unwrap_err().contains("must not be empty"));
        assert!(request("А = 1;", vec!["1А"]).unwrap_err().contains("'1А'"));
        assert!(request("Процедура П()\nКонецПроцедуры", vec!["Результат"])
            .unwrap_err()
            .contains("'statements' only"));
        let ok = request("\u{feff}Результат = 1;", vec!["Результат"]).unwrap();
        assert_eq!(ok.code, "Результат = 1;");
        assert_eq!(ok.code_kind, CodeKind::Statements);
        let variables = request("Перем Кэш Экспорт;\nКэш = 0;", vec![]).unwrap();
        assert_eq!(variables.code_kind, CodeKind::Module);
    }

    #[test]
    fn host_kind_ignores_the_case_of_conventional_segments_only() {
        let kind = |p: &str| HostKind::of(Path::new(p));
        assert_eq!(kind("src/Catalogs/A/Forms/F/Ext/Form/Module.bsl"), HostKind::Form);
        assert_eq!(kind("src\\Catalogs\\A\\Forms\\F\\EXT\\form\\module.BSL"), HostKind::Form);
        assert_eq!(kind("src/CommonModules/A/Ext/Module.bsl"), HostKind::CommonModule);
        assert_eq!(kind("src/commonmodules/A/ext/MODULE.bsl"), HostKind::CommonModule);
        assert_eq!(kind("src/Catalogs/A/Ext/ObjectModule.bsl"), HostKind::Other);
        assert_eq!(kind("src/Catalogs/A/Ext/Module.bsl"), HostKind::Other);
        assert_eq!(kind("Ext/Form/Module.bsl"), HostKind::Other);
    }
}
