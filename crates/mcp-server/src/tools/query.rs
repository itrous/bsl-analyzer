use crate::diagnostics_state::{CallOutcome, ResidentOutcome};
use crate::state::SharedState;
use crate::tools::response::{structured, structured_with_text, text_within_budget};
use rmcp::model::CallToolResult;
use rmcp::ErrorData as McpError;
use serde_json::json;
use std::collections::HashMap;
use std::fmt::Write;

/// Static contract for cold-start discovery, mirroring `graph`/`diagnostics` schema so the
/// query tool is self-describing instead of revealing its actions only through an error.
pub fn schema() -> CallToolResult {
    structured(json!({
        "schema_version": "2",
        "actions": ["validate", "execute", "schema"],
        "validate": "check an SDBL query. Offline it runs the parser AND the workspace query rules; with the metadata substrate ready the metadata-aware rules (unknown field, missing table) run too. The answer names its own completeness per block: `workspace_semantics` | `parser` | `platform`, and the local block says it in words too — `metadata_checked` (bool) and `status` (`checked_against_metadata` | `not_checked_against_metadata`). An empty `diagnostics` under `not_checked_against_metadata` is NOT a verdict on fields and tables; `degraded_reason` says whether repeating the call can help. With --onec-url the platform's verdict is added as a second block, never replacing the local one.",
        "execute": "run a SELECT query against the live 1C base (requires --onec-url). `limit` caps rows; `parameters` binds named query parameters.",
        "params": {
            "query": "the SDBL text (required for validate and execute)",
            "root_id": "validate: the configuration root the query is meant for — \"\" for the configuration, an extension id otherwise. An assertion about context, not a filter: an unregistered id fails the call, a registered one is echoed in `context.asserted_root_id`, while `context.roots` names the configuration roots resolution actually drew on",
            "limit": "max rows for execute (optional)",
            "parameters": "object of name → value bindings for execute (optional)",
            "max_output_tokens": "output budget in tokens (~4 chars each) for execute, on top of `limit` — the rendered table is cut at a row boundary with a note (default 6000)"
        },
        "prerequisites": "validate needs nothing to run; it reports `parser` instead of `workspace_semantics` while the metadata substrate is not ready. execute (and the platform block) need --onec-url / --onec-user / --onec-password"
    }))
}

/// A malformed query yields one diagnostic line per error node, so the listing is bounded by
/// the output budget like every other unbounded body.
const VALIDATE_NOTE: &str =
    "\n-- список ошибок усечён под max_output_tokens; исправьте показанные ошибки и повторите \
     проверку или повысьте бюджет --\n";

/// Static validation of a query, and — when a live connection is configured — the platform's
/// verdict beside it.
///
/// Three steps, and the boundary between the first and the rest is the design decision: a
/// parameter that cannot be resolved fails the call before any work happens, while a live
/// check that fails at run time becomes a block in the answer. Otherwise an unreachable
/// platform would discard the local result exactly when it is the only one available.
pub async fn validate_query(
    state: &SharedState,
    query: &str,
    root_id: Option<&str>,
    connection: Option<&str>,
    ct: tokio_util::sync::CancellationToken,
    max_output_tokens: usize,
) -> Result<CallToolResult, McpError> {
    let selected = validate_parameters(state, query, root_id, connection)?;

    let local = local_block(state, query, ct).await;

    let platform = match selected {
        Some(client_key) => Some(platform_block(state, &client_key, query).await),
        None => None,
    };

    Ok(render_validation(query, local, root_id, platform, max_output_tokens))
}

/// Step 1. Everything that makes the REQUEST wrong, checked before anything is computed.
///
/// An unresolvable `root_id` and an unknown `connection` name are the same class — the caller
/// named something that does not exist — and so get the same outcome. Neither is a reason to
/// degrade the answer: degradation describes the workspace not being ready, not the request
/// being wrong.
fn validate_parameters(
    state: &SharedState,
    query: &str,
    root_id: Option<&str>,
    connection: Option<&str>,
) -> Result<Option<String>, McpError> {
    if query.trim().is_empty() {
        return Err(McpError::invalid_params("Пустой запрос", None));
    }

    if let Some(root_id) = root_id {
        let roots = state
            .workspace_root()
            .and_then(|root| crate::project::at(root).ok())
            .map(|project| crate::project::workspace_roots(&project, &[]).0);
        if !roots.as_ref().is_some_and(|roots| roots.contains_id(root_id)) {
            return Err(McpError::invalid_params(format!("Неизвестный root_id: {root_id}"), None));
        }
        // The id must also still be CURRENT in the table it is matched against. The table here is
        // built at request time from the project on disk, so its canonical spellings were read just
        // now and this cannot fire while the project is read fresh — it is stated because every
        // path that takes a caller's `root_id` asks the same question of its own table, and the
        // ones that hold a table built earlier (the diagnostics resident) CAN be stale
        // (`WorkspaceRoots::registration_is_current`). An id survives a root's retarget, so an
        // `unknown_root_id` refusal no longer covers that case by itself.
        if roots.as_ref().is_some_and(|roots| !roots.registration_is_current(root_id)) {
            return Err(McpError::invalid_params(
                format!(
                    "root_id '{root_id}' больше не разрешается в каталог, с которым он был \
                     зарегистрирован; представление резидента может быть устаревшим — \
                     перезагрузите workspace"
                ),
                None,
            ));
        }
    }

    // Resolved here, not at use, so an unknown name fails the request instead of the platform
    // block — and so the local block is never computed only to be thrown away.
    //
    // An empty string is "not named", not a connection whose name is empty: tool-calling
    // clients routinely send `""` for an unset optional string, and reading it literally would
    // refuse a request that omitting the field would have served.
    match connection.filter(|name| !name.is_empty()) {
        Some(name) => {
            state.onec_connection(Some(name)).map_err(|e| McpError::invalid_params(e, None))?;
            Ok(Some(name.to_string()))
        }
        None => Ok(state.onec_connection(None).ok().map(|_| String::new())),
    }
}

/// What the local static check produced, and how complete it is.
struct LocalBlock {
    backend: &'static str,
    diagnostics: Vec<ide::DiagnosticOutput>,
    degraded_reason: Option<String>,
    /// The configuration roots resolution actually drew on, by their configuration labels.
    /// Kept apart from the caller's asserted `root_id`: the two live in different id spaces
    /// (`WorkspaceRoots` ids against `all_config_paths` labels), and folding an assertion into
    /// the list of participants would report a root as having contributed when it only got
    /// named.
    roots: Vec<String>,
}

impl LocalBlock {
    fn is_metadata_checked(&self) -> bool {
        self.backend == "workspace_semantics"
    }
}

/// Step 2. Always runs, always exactly one block.
async fn local_block(
    state: &SharedState,
    query: &str,
    ct: tokio_util::sync::CancellationToken,
) -> LocalBlock {
    let config = match project_diagnostics_config(state) {
        Some(config) => config,
        None => {
            return LocalBlock {
                backend: "parser",
                diagnostics: parse_only(&ide::DiagnosticsConfig::default(), query),
                degraded_reason: Some(
                    "конфигурация проекта не прочитана: применены умолчания анализатора"
                        .to_string(),
                ),
                roots: Vec::new(),
            }
        }
    };

    let diag = state.diagnostics().clone();
    // Kick, then read without waiting. Skipping the kick would be a different decision than
    // "do not wait": after an idle eviction nothing else moves the resident out of Idle, so
    // this tool would degrade to parse-only for the rest of the process's life.
    diag.ensure_loading();

    let query_text = query.to_string();
    let config_for_read = config.clone();
    let outcome = crate::diagnostics_state::resident_call(diag, ct, move |session| {
        session.read(|_resident, analysis, _generation| {
            let db = analysis.database();
            let resolver = ide::AcrossRootsQueryResolver::new(db);
            let roots: Vec<String> = db
                .designer_config_paths()
                .into_iter()
                .map(|(label, _)| label.unwrap_or_default())
                .collect();
            let diagnostics =
                ide::validate_query_text(&config_for_read, Some(&resolver), &query_text);
            (to_output(&query_text, diagnostics), roots)
        })
    })
    .await;

    let degraded = match outcome {
        CallOutcome::Ready(ResidentOutcome::Ready((diagnostics, roots), _)) => {
            return LocalBlock {
                backend: "workspace_semantics",
                diagnostics,
                degraded_reason: None,
                roots,
            };
        }
        CallOutcome::Ready(ResidentOutcome::Loading) => {
            "метаданные workspace ещё не готовы: сборка запущена, повторите вызов".to_string()
        }
        CallOutcome::Ready(ResidentOutcome::Disabled) => {
            "профиль без workspace: метаданные недоступны".to_string()
        }
        CallOutcome::Ready(ResidentOutcome::Failed(msg)) => {
            format!("сборка метаданных workspace завершилась ошибкой: {msg}")
        }
        CallOutcome::Cancelled => "вызов отменён до чтения метаданных".to_string(),
        CallOutcome::Superseded => {
            "метаданные изменились во время чтения, повторите вызов".to_string()
        }
        CallOutcome::Panicked => "чтение метаданных workspace прервано".to_string(),
    };

    LocalBlock {
        backend: "parser",
        diagnostics: parse_only(&config, query),
        degraded_reason: Some(degraded),
        roots: Vec::new(),
    }
}

/// The same rules, minus the ones that cannot speak without metadata. Not "syntax only": the
/// structural query rules do not consult metadata and are reported here too.
fn parse_only(config: &ide::DiagnosticsConfig, query: &str) -> Vec<ide::DiagnosticOutput> {
    to_output(query, ide::validate_query_text(config, None, query))
}

fn to_output(query: &str, diagnostics: Vec<ide::Diagnostic>) -> Vec<ide::DiagnosticOutput> {
    diagnostics.iter().map(|d| d.to_output(query)).collect()
}

/// The project's effective diagnostics settings, read independently of the resident.
///
/// Taking them from the resident would mean a degraded answer silently falls back to analyzer
/// defaults — resurrecting rules the project turned off, in the very state the tool is
/// supposed to be honest about. Only rules and locale are taken: the `diff_base` scope and the
/// author filter select by file and line, and a bare query has neither.
fn project_diagnostics_config(state: &SharedState) -> Option<ide::DiagnosticsConfig> {
    let root = state.workspace_root()?;
    let project = crate::project::at(root).ok()?;
    Some(ide::DiagnosticsConfig::from_project_json(
        &project.config.diagnostics.rules_json(),
        project.config.output.resolve_locale().unwrap_or_default(),
    ))
}

/// What the platform said, or why it could not say anything. Its errors carry no code, no
/// severity and no span, so they keep their own shape instead of being dressed as diagnostics.
struct PlatformBlock {
    valid: Option<bool>,
    errors: Vec<String>,
    error: Option<String>,
}

/// Step 3. A failure here is content, not an outcome.
async fn platform_block(state: &SharedState, connection: &str, query: &str) -> PlatformBlock {
    let name = (!connection.is_empty()).then_some(connection);
    let selected = match state.onec_connection(name) {
        Ok(selected) => selected,
        Err(e) => return PlatformBlock { valid: None, errors: Vec::new(), error: Some(e) },
    };

    let request = onec_client::ValidateQueryRequest { query: query.to_string() };
    match selected.client().validate_query(&request).await {
        Ok(result) => {
            PlatformBlock { valid: Some(result.valid), errors: result.errors, error: None }
        }
        Err(e) => PlatformBlock {
            valid: None,
            errors: Vec::new(),
            error: Some(format!("Ошибка проверки запроса в 1С: {e}")),
        },
    }
}

/// One envelope, an array of blocks — not one scalar `backend`, because the two results have
/// different natures and one field cannot label both.
///
/// The budget covers the WHOLE answer: `structured_with_text` ships a readable rendering
/// beside the envelope, and budgeting either half alone would leave the other unbounded. The
/// platform block is trimmed first — the local static check is what the tool was called for.
fn render_validation(
    query: &str,
    local: LocalBlock,
    asserted_root_id: Option<&str>,
    platform: Option<PlatformBlock>,
    max_output_tokens: usize,
) -> CallToolResult {
    let budget_chars = max_output_tokens.saturating_mul(4);

    // Each item is measured ONCE. Dropping one item and re-serializing the whole envelope to
    // see whether it fits is quadratic, and the cost is not theoretical: a generated query with
    // 2000 unaliased fields produced 2000 findings and took eleven seconds to render.
    let local_items: Vec<(serde_json::Value, String)> = local
        .diagnostics
        .iter()
        .map(|d| {
            let line =
                format!("  [{}] {}:{} {}\n", d.code, d.start_line, d.start_column, d.message);
            (serde_json::to_value(d).unwrap_or(serde_json::Value::Null), line)
        })
        .collect();
    let platform_items: Vec<(serde_json::Value, String)> = platform
        .as_ref()
        .map(|p| p.errors.iter().map(|e| (json!(e), format!("  {e}\n"))).collect())
        .unwrap_or_default();

    // Two chars of slack per item for the JSON separator, so the estimate never under-counts.
    let cost = |item: &(serde_json::Value, String)| item.0.to_string().len() + item.1.len() + 2;
    let local_costs: Vec<usize> = local_items.iter().map(cost).collect();
    let platform_costs: Vec<usize> = platform_items.iter().map(cost).collect();

    let skeleton = envelope_and_text(
        query,
        &local,
        asserted_root_id,
        platform.as_ref(),
        &local_items[..0],
        &platform_items[..0],
        false,
        false,
    );
    let mut total = skeleton.0.to_string().len() + skeleton.1.len();

    let mut local_kept = local_items.len();
    let mut platform_kept = platform_items.len();
    total += local_costs.iter().sum::<usize>() + platform_costs.iter().sum::<usize>();

    // The note announcing the truncation is printed only when something was dropped, so it is
    // absent from the sum above — and it is the one string the ceiling must not be broken by.
    // Reserving for every block that could be trimmed costs at most one extra dropped item;
    // not reserving pushed the answer ~200 bytes past a budget the tool promises to honour.
    let budget_chars = if total > budget_chars {
        let blocks_that_may_truncate = 1 + usize::from(platform.is_some());
        budget_chars.saturating_sub((VALIDATE_NOTE.len() + 1) * blocks_that_may_truncate)
    } else {
        budget_chars
    };

    // The platform block goes first: the local static check is what the tool was called for.
    while total > budget_chars && platform_kept > 0 {
        platform_kept -= 1;
        total -= platform_costs[platform_kept];
    }
    while total > budget_chars && local_kept > 0 {
        local_kept -= 1;
        total -= local_costs[local_kept];
    }

    let (envelope, text) = envelope_and_text(
        query,
        &local,
        asserted_root_id,
        platform.as_ref(),
        &local_items[..local_kept],
        &platform_items[..platform_kept],
        local_kept < local_items.len(),
        platform_kept < platform_items.len(),
    );

    structured_with_text(text, envelope)
}

#[allow(clippy::too_many_arguments)]
fn envelope_and_text(
    query: &str,
    local: &LocalBlock,
    asserted_root_id: Option<&str>,
    platform: Option<&PlatformBlock>,
    local_items: &[(serde_json::Value, String)],
    platform_items: &[(serde_json::Value, String)],
    local_truncated: bool,
    platform_truncated: bool,
) -> (serde_json::Value, String) {
    let mut results = Vec::new();

    // Stated in words beside `backend`: a caller that checks only for an empty `diagnostics`
    // must not be able to read a parse-only answer as "the query is correct".
    let metadata_checked = local.is_metadata_checked();
    let mut local_json = json!({
        "backend": local.backend,
        "metadata_checked": metadata_checked,
        "status": if metadata_checked {
            "checked_against_metadata"
        } else {
            "not_checked_against_metadata"
        },
        "diagnostics": local_items.iter().map(|(v, _)| v.clone()).collect::<Vec<_>>(),
        "truncated": local_truncated,
    });
    if let Some(reason) = &local.degraded_reason {
        local_json["degraded_reason"] = json!(reason);
    }
    results.push(local_json);

    if let Some(platform) = platform {
        let mut platform_json = json!({
            "backend": "platform",
            "errors": platform_items.iter().map(|(v, _)| v.clone()).collect::<Vec<_>>(),
            "truncated": platform_truncated,
        });
        if let Some(valid) = platform.valid {
            platform_json["valid"] = json!(valid);
        }
        if let Some(error) = &platform.error {
            platform_json["error"] = json!(error);
        }
        results.push(platform_json);
    }

    let mut context = json!({ "roots": local.roots });
    if let Some(id) = asserted_root_id {
        context["asserted_root_id"] = json!(id);
    }

    let envelope = json!({
        "schema_version": "2",
        "context": context,
        "results": results,
    });

    let text = render_text(
        query,
        local,
        local_items,
        local_truncated,
        platform,
        platform_items,
        platform_truncated,
    );

    (envelope, text)
}

#[allow(clippy::too_many_arguments)]
fn render_text(
    query: &str,
    local: &LocalBlock,
    local_items: &[(serde_json::Value, String)],
    local_truncated: bool,
    platform: Option<&PlatformBlock>,
    platform_items: &[(serde_json::Value, String)],
    platform_truncated: bool,
) -> String {
    let _ = query;
    let mut out = String::new();

    if local.is_metadata_checked() {
        out.push_str("Локальная проверка по метаданным workspace\n");
    } else {
        // Every byte here is outside the budget's reach — trimming drops findings, never this
        // line. Whether a repeated call can help depends on the cause, so that advice is left
        // to the reason line below.
        out.push_str("Локальная проверка без метаданных workspace: НЕ ПРОВЕРЕНО по метаданным\n");
        if let Some(reason) = &local.degraded_reason {
            let _ = writeln!(out, "  причина: {reason}");
        }
    }

    if local.diagnostics.is_empty() {
        if local.is_metadata_checked() {
            out.push_str("  замечаний нет\n");
        } else {
            // "No findings" here would read as a verdict on the whole query, and it is one only
            // on the text: the rules that need metadata never ran.
            out.push_str("  замечаний по тексту запроса нет\n");
        }
    } else {
        for (_, line) in local_items {
            out.push_str(line);
        }
        if local_truncated {
            let _ = writeln!(out, "{VALIDATE_NOTE}");
        }
    }

    if let Some(platform) = platform {
        out.push_str("\nПроверка платформой 1С\n");
        if let Some(error) = &platform.error {
            let _ = writeln!(out, "  проверка не выполнена: {error}");
        } else if platform.valid == Some(true) {
            out.push_str("  запрос принят платформой\n");
        } else {
            for (_, line) in platform_items {
                out.push_str(line);
            }
            if platform_truncated {
                let _ = writeln!(out, "{VALIDATE_NOTE}");
            }
        }
    }

    out
}

const DEFAULT_QUERY_LIMIT: u32 = 100;
const MAX_QUERY_LIMIT: u32 = 1000;

/// Whether `execute` may run this query: the first member of the parsed package must be
/// a `SELECT` — `SDBL_SELECT_QUERY` whose first token really spells the keyword.
///
/// The check reads the parse rather than a prefix of the text: a leading comment or a
/// run of blank lines must not hide the member's kind, and a non-SELECT behind the same
/// prefix must not slip through. Only the first member is checked — the same scope the
/// removed prefix gate had.
///
/// The node kind alone is not enough: a member opened by a clause keyword (`ГДЕ 1`)
/// still produces a `SDBL_SELECT_QUERY` — empty and marked with an error — so the token
/// inside has to spell `ВЫБРАТЬ`/`SELECT`.
fn first_member_is_select(query: &str) -> bool {
    use syntax::{SyntaxElement, SyntaxKind};

    let parsed = parser::parse_sdbl(query);
    let root = parsed.syntax_node();
    let Some(SyntaxElement::Node(member)) =
        root.children_with_tokens().find(|element| !element.kind().is_trivia())
    else {
        return false;
    };
    if member.kind() != SyntaxKind::SDBL_SELECT_QUERY {
        return false;
    }
    member
        .descendants_with_tokens()
        .filter_map(|element| element.into_token())
        .find(|token| !token.kind().is_trivia())
        .is_some_and(|token| {
            stdx::case::eq_ignore_case(token.text(), "ВЫБРАТЬ")
                || stdx::case::eq_ignore_case(token.text(), "SELECT")
        })
}

pub async fn execute_query(
    state: &SharedState,
    query: &str,
    limit: Option<u32>,
    parameters: Option<HashMap<String, serde_json::Value>>,
    connection: Option<&str>,
    max_output_tokens: usize,
) -> Result<CallToolResult, McpError> {
    // Same reading of an empty name as `validate`: `connection` is one parameter of one tool,
    // and two actions must not disagree about what an unset value means.
    let selected = state
        .onec_connection(connection.filter(|name| !name.is_empty()))
        .map_err(|e| McpError::invalid_params(e, None))?;

    if query.trim().is_empty() {
        return Err(McpError::invalid_params("Пустой запрос", None));
    }

    if !first_member_is_select(query) {
        return Err(McpError::invalid_params("Только SELECT/ВЫБРАТЬ запросы разрешены", None));
    }

    let limit = limit.unwrap_or(DEFAULT_QUERY_LIMIT).min(MAX_QUERY_LIMIT);

    let request = onec_client::QueryRequest {
        query: query.to_string(),
        limit,
        parameters: parameters.unwrap_or_default(),
    };

    let result = selected.client().execute_query(&request).await.map_err(|e| {
        McpError::internal_error(format!("Ошибка выполнения запроса в 1С: {e}"), None)
    })?;

    Ok(render_query_result(&result, max_output_tokens))
}

/// A row cap bounds how MANY rows come back, nothing bounds how WIDE they are, so the
/// rendered table gets an output budget on top of `limit`. Truncation cuts at a line (row)
/// boundary, keeping the header and the leading rows.
fn render_query_result(
    result: &onec_client::QueryResult,
    max_output_tokens: usize,
) -> CallToolResult {
    let note = if result.truncated {
        // The row cap already fired: raising only the token budget stops at the same rows.
        "\n-- вывод усечён под max_output_tokens, и строки уже ограничены `limit`; сузьте выборку (меньше колонок/строк) либо поднимите ОБА: max_output_tokens и limit --\n"
    } else {
        "\n-- вывод усечён под max_output_tokens; сузьте выборку (меньше колонок/строк) или повысьте бюджет --\n"
    };
    text_within_budget(format_query_result(result), max_output_tokens, note)
}

#[cfg(test)]
fn test_shared_state() -> crate::SharedState {
    crate::SharedState::shared()
}

fn format_query_result(result: &onec_client::QueryResult) -> String {
    if result.columns.is_empty() {
        return "Запрос выполнен, результат пуст.".to_string();
    }

    let mut out = format!("## Результат запроса ({} записей", result.total);
    if result.truncated {
        out.push_str(", результат усечён");
    }
    out.push_str(")\n\n");

    let _ = write!(out, "|");
    for col in &result.columns {
        let _ = write!(out, " {col} |");
    }
    out.push('\n');

    let _ = write!(out, "|");
    for _ in &result.columns {
        let _ = write!(out, "-----|");
    }
    out.push('\n');

    for row in &result.rows {
        let _ = write!(out, "|");
        for val in row {
            // A multiline string value would otherwise break the row into fragments that read
            // as separate (malformed) table rows — and would turn the budget's row-boundary
            // cut into a mid-row cut.
            let s = match val {
                serde_json::Value::Null => "—".to_string(),
                serde_json::Value::String(s) => s.replace(['\n', '\r'], " "),
                other => other.to_string(),
            };
            let _ = write!(out, " {s} |");
        }
        out.push('\n');
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extract_text(result: &CallToolResult) -> &str {
        result.content[0].as_text().expect("expected text content").text.as_str()
    }

    #[test]
    fn schema_advertises_every_action() {
        let result = schema();
        let body = result.structured_content.expect("schema is structured");
        let actions = body["actions"].as_array().expect("actions array");
        for action in ["validate", "execute", "schema"] {
            assert!(
                actions.iter().any(|a| a == action),
                "schema must advertise `{action}`: {body}",
            );
        }
    }

    /// The gate reads the parse, not a prefix: leading comments and blank lines must
    /// not hide the first member's kind — and must not let a non-SELECT through. The
    /// clause-keyword inputs pin the second half of the check: their node kind is a
    /// select, but the first token inside it is not the keyword.
    #[test]
    fn only_a_select_first_member_passes_the_execute_gate() {
        for allowed in [
            "ВЫБРАТЬ 1",
            "выбрать 1",
            "SELECT 1",
            "// отчёт по остаткам\nВЫБРАТЬ 1",
            "// один\n// два\n\n  \nВЫБРАТЬ 1",
            "ВЫБРАТЬ 1; УНИЧТОЖИТЬ ВремТабл;",
        ] {
            assert!(first_member_is_select(allowed), "must be allowed: {allowed:?}");
        }

        for rejected in [
            "// отчёт\nУНИЧТОЖИТЬ ВремТабл;",
            "УНИЧТОЖИТЬ ВремТабл;",
            "// один\n// два\nУНИЧТОЖИТЬ ВремТабл",
            "ИЗ Т",
            "ГДЕ 1",
            "ОБЪЕДИНИТЬ ВЫБРАТЬ 1",
        ] {
            assert!(!first_member_is_select(rejected), "must be rejected: {rejected:?}");
        }
    }

    /// `schema` is a hand-written contract, not a projection of `QueryParams` — adding a serde
    /// field does not update it. So the discovery surface is asserted separately from the
    /// wire surface, and the stale claim "syntax-check only" is asserted gone rather than
    /// merely replaced.
    #[test]
    fn schema_describes_the_parameters_and_completeness_validate_really_has() {
        let result = schema();
        let body = result.structured_content.expect("schema is structured");

        assert!(
            body["params"]["root_id"].is_string(),
            "a parameter the tool accepts must be discoverable: {body}",
        );

        let validate = body["validate"].as_str().expect("validate description");
        for backend in ["workspace_semantics", "parser", "platform"] {
            assert!(
                validate.contains(backend),
                "the description must name the `{backend}` completeness level: {validate}",
            );
        }
        assert!(
            !validate.contains("syntax-check an SDBL query"),
            "the description must no longer promise a syntax-only check: {validate}",
        );
    }

    /// The schema names where `root_id` comes back, and it has to be the field the envelope
    /// really uses. A caller reading only the schema — the point of having one — would
    /// otherwise look for its echo in a field that never carries it.
    #[test]
    fn the_schema_names_the_field_that_actually_echoes_root_id() {
        let described = schema().structured_content.expect("schema is structured")["params"]
            ["root_id"]
            .as_str()
            .expect("root_id is described")
            .to_string();

        let local = LocalBlock {
            backend: "parser",
            diagnostics: Vec::new(),
            degraded_reason: Some("тест".to_string()),
            roots: vec![String::new()],
        };
        let envelope = render_validation("ВЫБРАТЬ 1", local, Some("ext1"), None, 6000)
            .structured_content
            .expect("validate is structured");

        assert_eq!(envelope["context"]["asserted_root_id"], "ext1");
        assert!(
            described.contains("context.asserted_root_id"),
            "the schema must name the field the envelope uses: {described}",
        );
    }

    /// A tool-calling client that sends `""` for an unset optional string must be served the
    /// same answer as one that omits the field. Reading the empty name literally would refuse
    /// the request over a connection nobody asked for.
    ///
    /// `connection` is one parameter of one tool, so `execute` is asserted beside `validate`:
    /// fixing one action alone would leave the two disagreeing about the same input.
    #[test]
    fn an_empty_connection_name_is_read_as_no_connection_by_both_actions() {
        let state = test_shared_state();
        let rt = tokio::runtime::Runtime::new().unwrap();

        let omitted = rt.block_on(validate_query(
            &state,
            "ВЫБРАТЬ 1",
            None,
            None,
            tokio_util::sync::CancellationToken::new(),
            6000,
        ));
        let empty = rt.block_on(validate_query(
            &state,
            "ВЫБРАТЬ 1",
            None,
            Some(""),
            tokio_util::sync::CancellationToken::new(),
            6000,
        ));

        assert!(omitted.is_ok(), "omitting `connection` must be served");
        assert!(empty.is_ok(), "an empty `connection` must be served the same way");

        // `execute` needs a live connection, so neither call can succeed here — but they must
        // fail for the SAME reason. An empty name that resolves differently from an omitted one
        // is the asymmetry this guards.
        let exec_omitted = rt.block_on(execute_query(&state, "ВЫБРАТЬ 1", None, None, None, 6000));
        let exec_empty =
            rt.block_on(execute_query(&state, "ВЫБРАТЬ 1", None, None, Some(""), 6000));
        assert_eq!(
            exec_omitted.map(|_| ()).map_err(|e| e.to_string()),
            exec_empty.map(|_| ()).map_err(|e| e.to_string()),
            "`execute` must read an empty connection name exactly as an omitted one",
        );
    }

    /// The local block is what the tool is for, so it is asserted through the envelope the
    /// caller actually receives rather than through an internal helper.
    fn local_block_of(query: &str) -> serde_json::Value {
        let config = ide::DiagnosticsConfig::all_enabled();
        let diagnostics = parse_only(&config, query);
        let block = LocalBlock {
            backend: "parser",
            diagnostics,
            degraded_reason: Some("тест: метаданные не подключены".to_string()),
            roots: Vec::new(),
        };
        let result = render_validation(query, block, None, None, 6000);
        result.structured_content.expect("validate is structured")
    }

    fn codes_in(envelope: &serde_json::Value) -> Vec<String> {
        envelope["results"][0]["diagnostics"]
            .as_array()
            .expect("diagnostics array")
            .iter()
            .map(|d| d["code"].as_str().unwrap_or_default().to_string())
            .collect()
    }

    #[test]
    fn a_well_formed_query_carries_no_parse_error() {
        for query in ["ВЫБРАТЬ 1", "SELECT 1"] {
            let codes = codes_in(&local_block_of(query));
            assert!(
                !codes.iter().any(|c| c == "QueryParseError"),
                "`{query}` must parse cleanly, got {codes:?}",
            );
        }
    }

    #[test]
    fn broken_input_carries_a_parse_error() {
        for query in [
            "}{}{}{",
            "это вообще не запрос",
            "ВЫБРАТЬ Наименование ИЗ Справочник.Номенклатура ГДЕ",
        ] {
            let codes = codes_in(&local_block_of(query));
            assert!(
                codes.iter().any(|c| c == "QueryParseError"),
                "`{query}` must be reported as broken, got {codes:?}",
            );
        }
    }

    /// The degraded answer must not end on a sentence that reads as a verdict on the query: an
    /// agent that stops at "замечаний нет" takes a parse-only answer for a full check. Asserted
    /// on both halves, because a caller may read either.
    #[test]
    fn an_answer_without_metadata_says_the_query_was_not_checked_against_it() {
        let query = "ВЫБРАТЬ Т.НетТакогоПоля КАК П ИЗ Справочник.Номенклатура КАК Т";
        let local = LocalBlock {
            backend: "parser",
            diagnostics: parse_only(&ide::DiagnosticsConfig::all_enabled(), query),
            degraded_reason: Some("метаданные workspace ещё не готовы".to_string()),
            roots: Vec::new(),
        };
        let render = render_validation(query, local, None, None, 6000);
        let text = extract_text(&render).to_string();
        let envelope = render.structured_content.expect("structured");

        assert!(text.contains("НЕ ПРОВЕРЕНО по метаданным"), "{text}");
        assert!(!text.contains("замечаний нет"), "{text}");
        assert_eq!(envelope["results"][0]["metadata_checked"], false, "{envelope}");
        assert_eq!(envelope["results"][0]["status"], "not_checked_against_metadata");

        let local = LocalBlock {
            backend: "workspace_semantics",
            diagnostics: Vec::new(),
            degraded_reason: None,
            roots: vec![String::new()],
        };
        let render = render_validation(query, local, None, None, 6000);
        let text = extract_text(&render).to_string();
        let envelope = render.structured_content.expect("structured");
        assert!(text.contains("замечаний нет"), "{text}");
        assert_eq!(envelope["results"][0]["metadata_checked"], true, "{envelope}");
        assert_eq!(envelope["results"][0]["status"], "checked_against_metadata");
    }

    #[test]
    fn the_local_block_is_always_present_and_labelled() {
        let envelope = local_block_of("ВЫБРАТЬ 1");
        let results = envelope["results"].as_array().expect("results array");
        assert_eq!(results.len(), 1, "no connection means exactly one block: {envelope}");
        assert_eq!(results[0]["backend"], "parser");
        assert!(
            results[0]["degraded_reason"].is_string(),
            "a parser-backed block must say why: {envelope}",
        );
    }

    /// Structural rules do not consult metadata, so they are reachable on the degraded path —
    /// and the metadata-dependent ones are not. Conflating the two is what would let the tool
    /// pass off a parse-only answer as a full one.
    #[test]
    fn the_degraded_block_carries_structural_findings_but_no_metadata_ones() {
        let query = "ВЫБРАТЬ Т.Поле КАК П ИЗ Справочник.НетТакого КАК Т \
                     ВНУТРЕННЕЕ СОЕДИНЕНИЕ (ВЫБРАТЬ 1 КАК Ч) КАК В ПО ИСТИНА";
        let codes = codes_in(&local_block_of(query));

        assert!(
            codes.iter().any(|c| c == "JoinWithSubQuery"),
            "a structural rule must still fire without metadata, got {codes:?}",
        );
        for code in ide::METADATA_DEPENDENT_CODES {
            assert!(
                !codes.iter().any(|c| c == code.as_str()),
                "{} cannot be produced without a resolver, got {codes:?}",
                code.as_str(),
            );
        }
    }

    /// Step 1 of `validate`: what makes the REQUEST wrong fails the call outright, before
    /// anything is computed. Asserted through `validate_query` rather than the gate helper,
    /// because the promise is about the tool's outcome, not an internal function's.
    #[test]
    fn a_request_that_names_nothing_real_fails_the_call() {
        let state = test_shared_state();
        let rt = tokio::runtime::Runtime::new().unwrap();

        for (label, query, root_id) in [
            ("empty", "", None),
            ("whitespace", "   ", None),
            ("unknown root_id", "ВЫБРАТЬ 1", Some("нет-такого-корня")),
        ] {
            let result = rt.block_on(validate_query(
                &state,
                query,
                root_id,
                None,
                tokio_util::sync::CancellationToken::new(),
                6000,
            ));
            assert!(result.is_err(), "[{label}] must fail the call, not degrade the answer");
        }
    }

    /// The other half of the gate, and the half a test is likely to forget: a request that
    /// names nothing at all must pass it. Without this the gate could reject everything and
    /// the test above would still be green.
    #[test]
    fn a_request_naming_no_root_passes_the_gate() {
        let state = test_shared_state();
        let rt = tokio::runtime::Runtime::new().unwrap();

        let result = rt.block_on(validate_query(
            &state,
            "ВЫБРАТЬ 1",
            None,
            None,
            tokio_util::sync::CancellationToken::new(),
            6000,
        ));
        assert!(result.is_ok(), "a query with no root_id must be answered, not refused");
    }

    /// The promise "the local block is always present" is only worth something in the case
    /// that would break it: the live check failing. Today's code path turns that into `Err`
    /// and the whole answer disappears — with the local block already computed.
    #[test]
    fn an_unreachable_platform_does_not_take_the_local_block_with_it() {
        let query = "ВЫБРАТЬ 1";
        let local = LocalBlock {
            backend: "workspace_semantics",
            diagnostics: parse_only(&ide::DiagnosticsConfig::all_enabled(), query),
            degraded_reason: None,
            roots: vec![String::new()],
        };
        let platform = PlatformBlock {
            valid: None,
            errors: Vec::new(),
            error: Some("Ошибка проверки запроса в 1С: соединение отклонено".to_string()),
        };

        let result = render_validation(query, local, None, Some(platform), 6000);
        let envelope = result.structured_content.expect("validate is structured");
        let results = envelope["results"].as_array().expect("results array");

        assert_eq!(results.len(), 2, "both blocks must be present: {envelope}");
        assert_eq!(results[0]["backend"], "workspace_semantics");
        assert_eq!(results[1]["backend"], "platform");
        assert!(
            results[1]["error"].is_string(),
            "the platform failure belongs in its block: {envelope}",
        );
        assert!(
            results[1].get("diagnostics").is_none(),
            "platform errors carry no code or span and must not pose as diagnostics: {envelope}",
        );
    }

    /// Rendering cost must stay proportional to the number of findings.
    ///
    /// It was not: the first version dropped one item and re-serialized the whole envelope to
    /// see whether it now fit, so 2000 findings took eleven seconds inside one tool call. The
    /// bound below has ~60x headroom over the measured cost, which is wide enough not to flake
    /// on a slow machine and far too tight for a return to quadratic. The input is built
    /// outside the timer: parsing 2000 fields costs more than rendering them.
    #[test]
    fn rendering_cost_stays_proportional_to_the_number_of_findings() {
        use std::time::{Duration, Instant};

        let fields: Vec<String> = (0..2000).map(|i| format!("Поле{i}")).collect();
        let query = format!("ВЫБРАТЬ {} ИЗ Справочник.Т КАК Т", fields.join(", "));
        let config = ide::DiagnosticsConfig::all_enabled();
        let diagnostics = parse_only(&config, &query);
        assert!(
            diagnostics.len() >= 1000,
            "the input must actually produce a long listing, got {}",
            diagnostics.len(),
        );

        let mut best = Duration::MAX;
        for _ in 0..3 {
            let local = LocalBlock {
                backend: "parser",
                diagnostics: diagnostics.clone(),
                degraded_reason: None,
                roots: Vec::new(),
            };
            let started = Instant::now();
            let _ = render_validation(&query, local, None, None, 6000);
            best = best.min(started.elapsed());
        }

        if cfg!(timing_contract) {
            assert!(
                best < Duration::from_millis(500),
                "rendering {} findings took {best:?} — the cost is not proportional",
                diagnostics.len(),
            );
        }
    }

    /// The note that announces a truncation is itself output, and it appears exactly when the
    /// ceiling is under pressure. Measuring the items but not the note put the answer ~140
    /// bytes past a budget the tool promises to honour — with short items the per-item slack is
    /// a few bytes and cannot absorb a 200-byte note.
    #[test]
    fn the_truncation_note_fits_inside_the_budget_it_announces() {
        let errors: Vec<String> = (0..500).map(|_| "e".to_string()).collect();
        let local = LocalBlock {
            backend: "parser",
            diagnostics: Vec::new(),
            degraded_reason: None,
            roots: Vec::new(),
        };
        let platform = PlatformBlock { valid: Some(false), errors, error: None };
        let budget_tokens = 200usize;

        let render = render_validation("ВЫБРАТЬ 1", local, None, Some(platform), budget_tokens);
        let envelope = render.structured_content.clone().expect("structured");
        let text = extract_text(&render).to_string();

        assert_eq!(
            envelope["results"][1]["truncated"], true,
            "the input must actually be truncated, else the note is never printed",
        );
        assert!(
            text.contains("усечён"),
            "the note must be present — it is what the budget has to cover: {text}",
        );

        let total = envelope.to_string().len() + text.len();
        assert!(total <= budget_tokens * 4, "budget={}, got {total}", budget_tokens * 4,);
    }

    fn wide_result(rows: usize, truncated: bool) -> onec_client::QueryResult {
        onec_client::QueryResult {
            columns: vec!["Ссылка".into(), "Наименование".into()],
            rows: (0..rows)
                .map(|i| {
                    vec![serde_json::json!(format!("row{i}")), serde_json::json!("ш".repeat(300))]
                })
                .collect(),
            total: rows as u32,
            truncated,
        }
    }

    #[test]
    fn platform_error_listing_is_bounded_by_the_budget() {
        let errors: Vec<String> =
            (0..200).map(|i| format!("Ошибка {i}: {}", "подробности ".repeat(10))).collect();
        let render = |budget: usize| {
            let local = LocalBlock {
                backend: "parser",
                diagnostics: Vec::new(),
                degraded_reason: Some("тест".to_string()),
                roots: Vec::new(),
            };
            let platform =
                PlatformBlock { valid: Some(false), errors: errors.clone(), error: None };
            render_validation("ВЫБРАТЬ 1", local, None, Some(platform), budget)
        };

        let full = render(100_000);
        // Above the envelope's irreducible skeleton, so the cap is something the trimming can
        // actually meet. Below it the marker matters more than the ceiling and the loop stops.
        let clipped = render(300);
        let full_text = extract_text(&full).to_string();
        let clipped_text = extract_text(&clipped).to_string();

        assert!(clipped_text.len() < full_text.len(), "a 100-token budget must clip the listing");

        let envelope = clipped.structured_content.expect("validate is structured");
        let platform = &envelope["results"][1];
        assert_eq!(platform["backend"], "platform");
        assert_eq!(platform["truncated"], true, "the clipped block must say so: {envelope}");
        assert_eq!(
            envelope["results"][0]["truncated"], false,
            "the local block is trimmed last: {envelope}",
        );

        // The budget covers the pair, not the text alone: `structured_with_text` ships both.
        let total = envelope.to_string().len() + clipped_text.len();
        assert!(total <= 300 * 4, "envelope and text together must fit the budget: {total}");
    }

    #[test]
    fn multiline_cell_never_breaks_a_table_row() {
        let result = onec_client::QueryResult {
            columns: vec!["Комментарий".into()],
            rows: vec![vec![serde_json::json!("первая\nвторая\r\nтретья")]],
            total: 1,
            truncated: false,
        };
        let text = extract_text(&render_query_result(&result, 6000)).to_string();
        // Header, separator and exactly one data row — the embedded newlines must not split it.
        assert_eq!(text.lines().filter(|l| l.starts_with('|')).count(), 3, "{text}");
        assert!(text.contains("| первая вторая  третья |"), "{text}");
    }

    #[test]
    fn execute_result_within_budget_is_untouched() {
        let result = render_query_result(&wide_result(2, false), 6000);
        let text = extract_text(&result);
        assert!(text.contains("row1"), "both rows must survive: {text}");
        assert!(!text.contains("усечён"), "nothing to note: {text}");
    }

    #[test]
    fn execute_result_over_budget_is_cut_at_a_row_boundary() {
        let result = render_query_result(&wide_result(200, false), 600);
        let text = extract_text(&result);
        assert!(text.contains("| Ссылка |"), "the header must survive: {text}");
        assert!(text.contains("row0"), "the leading rows must survive: {text}");
        assert!(text.contains("усечён под max_output_tokens"), "must carry the note: {text}");
        assert!(!text.contains("row199"), "the trailing rows must be dropped");
        // Truncation never leaves half a row before the note.
        let body = text.split("\n-- вывод усечён").next().unwrap();
        assert!(body.ends_with(" |\n"), "cut must land on a row boundary: {body:?}");
    }

    #[test]
    fn execute_note_says_raising_the_budget_alone_will_not_help_when_limit_also_capped() {
        let result = render_query_result(&wide_result(200, true), 600);
        let text = extract_text(&result);
        assert!(
            text.contains("ОБА: max_output_tokens и limit"),
            "note must name both caps: {text}"
        );
    }
}
