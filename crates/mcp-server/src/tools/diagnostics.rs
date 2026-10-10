//! Agent-facing diagnostics tool actions.
//!
//! Diagnostics are the second non-grep-able semantic primitive beside the call
//! `graph`: grep cannot tell unreachable code, a type mismatch, an unresolved call,
//! or an unused variable from ordinary text, but the analyzer can.
//!
//! This module ships the `catalog` action — the static, database-free list of every
//! registered diagnostic code with the metadata an agent needs for cold-start
//! discovery and the request→narrow→request entry point. Rule prose lives here,
//! keyed by `code`, so a later per-file action's findings never repeat it. The
//! `schema` action advertises the contract. Both are computed from compile-time
//! metadata, so no resident analysis database is required.

use std::borrow::Cow;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::str::FromStr;

use ide::diagnostics_baseline::{
    classify_diagnostics, BaselineDiagnosticCandidate, DiagnosticsBaselineCoverage,
    DiagnosticsBaselineRange, DiagnosticsBaselineSummary,
};
use ide::{
    catalog_entry, diagnostic_catalog, DiagnosticCode, DocumentSymbol, Locale, SeverityBucket,
    SymbolKind, TextRange,
};
use rmcp::model::CallToolResult;
use schemars::{json_schema, JsonSchema, Schema, SchemaGenerator};
use serde_json::{json, Value};

use crate::diagnostics_state::{DiagnosticsResident, Freshness, StatusReport, WorkspaceSweep};
// Aliased: the resident has a `Freshness` of its own, and the contract's is a different
// type serving a different purpose — the two must not be confusable at a glance.
use crate::tools::file_request::{answer_location, FileError};
use crate::tools::location as loc;
use crate::tools::response::{structured, trim_items_to_budget};

/// Server-side cap on returned findings, honouring Anthropic's tool-response budget.
/// `counts` still reports the full severity histogram, so a capped response is honest.
pub(crate) const DEFAULT_MAX_FINDINGS: usize = 200;
pub(crate) const MIN_OUTPUT_TOKENS: usize = 256;

/// Default cap on files swept by the opt-in `workspace` action. Bounds the cost of a
/// whole-config pass; the agent can raise it up to [`MAX_SWEEP_FILES_CEILING`].
pub(crate) const DEFAULT_MAX_SWEEP_FILES: usize = 1000;

/// Hard ceiling on `max_files`: the sweep holds the resident lock for its whole
/// duration (blocking other diagnostics calls), so an agent cannot request an
/// unbounded whole-config pass that stalls the server for minutes. A larger config is
/// reported as `truncated` with the true `files_total`.
pub(crate) const MAX_SWEEP_FILES_CEILING: usize = 5000;

pub struct DiagnosticsResponseSchema;

impl JsonSchema for DiagnosticsResponseSchema {
    fn schema_name() -> Cow<'static, str> {
        "DiagnosticsResponse".into()
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        // `anyOf`, not `oneOf`: the shapes are untagged and legitimately overlap — a
        // `loading` body carries both `status` and `state`, matching two branches at
        // once, and `oneOf` demands EXACTLY one. The schema describes what a response
        // may look like; it is not a discriminator.
        json_schema!({
            "type": "object",
            "anyOf": [
                generator.subschema_for::<DiagnosticsBaselineSuccessEnvelope>(),
                generator.subschema_for::<DiagnosticsBaselineErrorEnvelope>(),
                generator.subschema_for::<DiagnosticsActionResponse>(),
                generator.subschema_for::<DiagnosticsSchemaResponse>(),
                generator.subschema_for::<DiagnosticsStateResponse>(),
                generator.subschema_for::<DiagnosticsLoadingResponse>(),
            ]
        })
    }
}

#[derive(JsonSchema)]
#[allow(dead_code)]
pub struct DiagnosticsBaselineSuccessEnvelope {
    pub revision: u64,
    pub stale: bool,
    pub reload: String,
    pub result: DiagnosticsBaselineSuccessResult,
}

#[derive(JsonSchema)]
#[allow(dead_code)]
pub struct DiagnosticsBaselineErrorEnvelope {
    pub revision: u64,
    pub stale: bool,
    pub reload: String,
    pub result: DiagnosticsBaselineErrorResult,
}

#[derive(JsonSchema)]
#[allow(dead_code)]
pub struct DiagnosticsBaselineSuccessResult {
    pub baseline: DiagnosticsBaselineSuccess,
    #[schemars(flatten)]
    pub additional: std::collections::BTreeMap<String, serde_json::Value>,
}

#[derive(JsonSchema)]
#[allow(dead_code)]
pub struct DiagnosticsBaselineErrorResult {
    pub baseline: DiagnosticsBaselineError,
    #[schemars(flatten)]
    pub additional: std::collections::BTreeMap<String, serde_json::Value>,
}

#[derive(JsonSchema)]
#[allow(dead_code)]
pub struct DiagnosticsBaselineSuccess {
    pub state: DiagnosticsBaselineSuccessState,
    pub complete: bool,
    pub selection: Option<String>,
    pub partitions_enabled: Option<usize>,
    pub partitions_unsuppressed: Option<usize>,
    pub unsuppressed: Option<usize>,
    pub new: Option<usize>,
    pub known: Option<usize>,
    pub resolved: Option<usize>,
    pub path: Option<String>,
    pub schema_version: Option<u32>,
    pub manifest_schema_version: Option<u32>,
    pub partitions_total: usize,
    pub partitions_returned: usize,
    pub partitions_truncated: bool,
    pub partitions: Vec<serde_json::Value>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "snake_case")]
#[allow(dead_code)]
pub enum DiagnosticsBaselineSuccessState {
    Disabled,
    Full,
    Partial,
}

#[derive(JsonSchema)]
#[allow(dead_code)]
pub struct DiagnosticsBaselineError {
    pub state: DiagnosticsBaselineErrorState,
    pub complete: bool,
    pub selection: Option<String>,
    pub partitions_enabled: Option<usize>,
    pub partitions_unsuppressed: Option<usize>,
    pub unsuppressed: Option<usize>,
    pub error_code: String,
    pub detail: String,
    pub path: Option<String>,
    pub schema_version: Option<u32>,
    pub manifest_schema_version: Option<u32>,
    pub partitions_total: usize,
    pub partitions_returned: usize,
    pub partitions_truncated: bool,
    pub partitions: Vec<serde_json::Value>,
    pub errors_total: usize,
    pub errors: Vec<serde_json::Value>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "snake_case")]
#[allow(dead_code)]
pub enum DiagnosticsBaselineErrorState {
    Error,
}

#[derive(JsonSchema)]
#[allow(dead_code)]
pub struct DiagnosticsActionResponse {
    pub action: String,
}

#[derive(JsonSchema)]
#[allow(dead_code)]
pub struct DiagnosticsSchemaResponse {
    pub schema_version: String,
}

#[derive(JsonSchema)]
#[allow(dead_code)]
pub struct DiagnosticsStateResponse {
    pub state: String,
}

#[derive(JsonSchema)]
#[allow(dead_code)]
pub struct DiagnosticsLoadingResponse {
    pub status: String,
}

/// Filters applied to a file's diagnostics before they become findings.
pub(crate) struct FileFilters {
    /// Inclusive severity floor (findings below it are dropped from `findings`, but
    /// still counted in `counts`).
    pub min_severity: SeverityBucket,
    /// Keep only these codes (empty = all).
    pub codes: Vec<String>,
    /// Keep only findings intersecting this inclusive 0-based line range (None = all).
    pub range: Option<(usize, usize)>,
    /// Cap on `findings` length.
    pub max_findings: usize,
    /// Optional output budget in tokens (~4 chars each). When set, `findings` is trimmed
    /// to fit after the `max_findings` cap, and the response flags `budget_exhausted`.
    pub max_output_tokens: Option<usize>,
    /// Detailed mode adds `internal_severity` (7-grade) and the fix label per finding.
    pub detailed: bool,
}

/// The static catalog of diagnostic codes in `locale`, optionally narrowed to
/// `codes`. Unparseable / unknown requested codes are reported back in
/// `unknown_codes` rather than silently dropped, so the agent can correct itself.
pub fn catalog(
    locale: Locale,
    codes: &[String],
    max_output_tokens: Option<usize>,
) -> CallToolResult {
    let (entries, unknown): (Vec<_>, Vec<String>) = if codes.is_empty() {
        (diagnostic_catalog(locale), Vec::new())
    } else {
        let mut entries = Vec::with_capacity(codes.len());
        let mut unknown = Vec::new();
        for raw in codes {
            match DiagnosticCode::from_str(raw).ok().and_then(|c| catalog_entry(c, locale)) {
                Some(entry) => entries.push(entry),
                None => unknown.push(raw.clone()),
            }
        }
        (entries, unknown)
    };

    // `diagnostic_catalog` / the `codes` walk both emit a deterministic, stable order, so a
    // budget-trimmed catalog drops a stable tail rather than a random subset.
    let total = entries.len();
    let entries: Vec<Value> =
        entries.iter().map(|e| serde_json::to_value(e).unwrap_or(Value::Null)).collect();

    let mut body = json!({
        "action": "catalog",
        "locale": locale_str(locale),
        "count": entries.len(),
        "entries": entries,
    });
    if !unknown.is_empty() {
        body["unknown_codes"] = json!(unknown);
    }
    if let Some(limit) = max_output_tokens.map(|tokens| tokens.saturating_mul(4)) {
        if serde_json::to_vec(&body).is_ok_and(|bytes| bytes.len() > limit) {
            let all_entries =
                body["entries"].as_array_mut().map(std::mem::take).unwrap_or_default();
            let all_unknown =
                body["unknown_codes"].as_array_mut().map(std::mem::take).unwrap_or_default();
            body["total"] = json!(total);
            body["budget_exhausted"] = json!(true);
            body["budget_hint"] = json!(
                "catalog truncated to fit max_output_tokens; narrow with `codes` or raise the budget"
            );
            let mut used = serde_json::to_vec(&body).map_or(limit, |bytes| bytes.len());
            let mut keep = |items: Vec<Value>| {
                let mut kept = Vec::new();
                for item in items {
                    let extra = serde_json::to_vec(&item).map_or(0, |bytes| bytes.len())
                        + usize::from(!kept.is_empty());
                    if used.saturating_add(extra) > limit {
                        break;
                    }
                    used += extra;
                    kept.push(item);
                }
                kept
            };
            // Unknown codes are budgeted FIRST: they are the answer to "are my codes
            // real?", and dropping them silently leaves an empty list that reads as
            // "everything is known" — the opposite of what happened.
            let unknown = keep(all_unknown);
            let entries = keep(all_entries);
            body["count"] = json!(entries.len());
            body["entries"] = json!(entries);
            if body.get("unknown_codes").is_some() {
                body["unknown_codes"] = json!(unknown);
            }
        }
    }
    structured(body)
}

/// Static contract for cold-start discovery, mirroring `graph schema`. `schema_version`
/// is bumped in lockstep with any response-shape change.
pub fn schema() -> CallToolResult {
    structured(schema_json())
}

/// A transient "still building the resident database" result, emitted while the
/// background load runs. Not an error — the agent should retry shortly. Carries the
/// lifecycle snapshot (`state`/`generation`/`elapsed_ms`) so the agent can tell a
/// progressing build from a stuck or failed one instead of polling a flat `loading`.
pub fn loading(report: &StatusReport) -> CallToolResult {
    crate::tools::resident::loading(report, "diagnostics database is building; retry shortly")
}

/// Who answers a `diagnostics` request — every branch of it, including the in-band errors.
///
/// Named once and read by both the envelope and the `schema` action, because a tool's schema
/// says what THAT tool can return: publishing the contract's whole vocabulary there would have
/// a consumer write branches for sources this tool never produces.
const ANSWERED_BY: loc::FreshnessSource = loc::FreshnessSource::Resident;

/// Wrap a `file` result in the freshness envelope, matching the `graph` tool.
///
/// `completeness` is passed in rather than derived here: the facts that make an answer
/// partial (a budget trim, a count cap, an unread file) are known where the body is built,
/// and recovering them from the rendered JSON would be a guess about our own behaviour.
pub fn envelope(
    freshness: Freshness,
    completeness: loc::Completeness,
    mut result: Value,
) -> CallToolResult {
    if (freshness.stale || freshness.reload != "none")
        && matches!(result["baseline"]["state"].as_str(), Some("full" | "partial"))
    {
        result["baseline"]["state"] = json!("partial");
        result["baseline"]["complete"] = json!(false);
        result["baseline"]["resolved"] = json!(0);
        if let Some(partitions) = result["baseline"]["partitions"].as_array_mut() {
            for partition in partitions {
                partition["state"] = json!("partial");
                partition["complete"] = json!(false);
                partition["resolved"] = json!(0);
            }
        }
    }
    structured(json!({
        "revision": freshness.revision,
        "stale": freshness.stale,
        "reload": freshness.reload,
        "freshness": loc::Freshness::new(ANSWERED_BY, completeness)
            .with_revision(freshness.revision)
            .with_topology(freshness.topology)
            .with_stale(freshness.stale)
            .to_value(),
        "result": result,
    }))
}

/// Parse a `min_severity` floor, defaulting to `warning` (drops info/hint noise by
/// default). An unrecognised label is an error so the agent learns the vocabulary.
pub(crate) fn parse_min_severity(s: Option<&str>) -> Result<SeverityBucket, String> {
    match s {
        None => Ok(SeverityBucket::Warning),
        Some(label) => SeverityBucket::parse(label).ok_or_else(|| {
            format!("unknown min_severity '{label}'; expected error|warning|info|hint")
        }),
    }
}

/// Parse the `file` action's `detail` enum into the `detailed` flag. `None`/`concise` keep
/// the compact view; `detailed` adds internal severity + fix. An unknown value is an error
/// rather than a silent default, so a caller is never served a different view than it asked
/// for (mirrors [`parse_min_severity`] and the `graph` enum validators).
pub(crate) fn parse_detail(s: Option<&str>) -> Result<bool, String> {
    match s {
        None | Some("concise") => Ok(false),
        Some("detailed") => Ok(true),
        Some(other) => Err(format!("unknown detail '{other}'; expected concise|detailed")),
    }
}

/// Whether `path` lies in an external object's root while the project has no base
/// to resolve that object's calls against. Read from the registered roots, not
/// from the disk: the external root carries no marker a walk could find.
fn lacks_owning_configuration(db: &ide::RootDatabaseImpl, path: &Path) -> bool {
    let snapshot = db.workspace_configs_snapshot();
    !snapshot.has_base()
        && matches!(snapshot.owner_kind_of_path(path), Some(ide::WorkspaceRootKind::External(_)))
}

/// Compute the `file` action's result body from the resident database: resolve the
/// path to its FileId, run diagnostics, then filter (codes / range / floor), bucket
/// severity, and shape findings + the full `counts` histogram + a content-hash
/// `result_id`. Runs inside the resident read lock, on the calling thread.
pub(crate) fn file_findings(
    resident: &DiagnosticsResident,
    analysis: &ide::Analysis,
    root_id: Option<&str>,
    path: &Path,
    filters: &FileFilters,
    generation: u64,
) -> (Value, loc::Completeness) {
    // Resolved ONCE, here, and bound to the same name: the request path is read again by the
    // unreadable branch, the blame filter, every finding's graph id, the result id and the
    // scope verdict, and each of them reads it against the workspace on its own. Shadowing is
    // what makes the root-relative spelling unreachable below rather than merely unwanted —
    // the compiler cannot tell the two apart, they are both `&Path`.
    // Bound before the shadowing below: this is the only place the caller's own spelling is
    // still visible, and it is what the published location must echo.
    let requested_spelling = path;
    let path = match resident.resolve_rooted_path(root_id, path) {
        Ok(resolved) => resolved,
        Err(error) => {
            let detail = error.to_string();
            let error = FileError::Rooted(error);
            let completeness = error.completeness();
            let mut body = error.to_value(&detail, requested_spelling);
            body["baseline"] = bounded_baseline_value(&unclassified_baseline(resident), None);
            return (budgeted_diagnostics_response(body, filters.max_output_tokens), completeness);
        }
    };
    let path = path.as_path();
    let Some(file_id) = resident.file_id_for(path) else {
        // A workspace file whose bytes could not be read is NOT "not in workspace":
        // that answer would be a second lie about an existing file, replacing the
        // first one ("no findings"). The two are kept apart so an agent can tell
        // "wrong path" from "this module is unanalysed".
        if resident.is_unread(path) {
            let error = FileError::Unreadable;
            let mut body = error.to_value(
                "workspace .bsl file exists but its bytes could not be read; \
                     it is held out of service and re-read every drift window",
                path,
            );
            body["baseline"] = bounded_baseline_value(&unclassified_baseline(resident), None);
            return (
                budgeted_diagnostics_response(body, filters.max_output_tokens),
                error.completeness(),
            );
        }
        let error = FileError::NotInWorkspace;
        let mut body = error.to_value("path is not a resident workspace .bsl file", path);
        body["baseline"] = bounded_baseline_value(&unclassified_baseline(resident), None);
        return (
            budgeted_diagnostics_response(body, filters.max_output_tokens),
            error.completeness(),
        );
    };
    // The caller's handle, not a fresh clone: inside a cancellable read the request's
    // salsa scope is attached to THAT handle, and a second one would be a different
    // database to salsa.
    let file_text = analysis.file_text(file_id);
    // Analyse against the project's effective config (the single source of truth shared
    // with LSP and CLI), so disabled rules and tuned thresholds are honoured.
    let diagnostics = analysis.diagnostics(file_id, resident.config());
    let (diagnostics, baseline) =
        match classify_file_baseline(resident, path, &file_text, diagnostics) {
            Ok(result) => result,
            Err(baseline) => {
                return (
                    budgeted_diagnostics_response(
                        json!({
                            "result_id": format!(
                                "{}@{}",
                                result_id(path, generation, &file_text),
                                resident.diagnostics_baseline().epoch(),
                            ),
                            "kind": "full",
                            "baseline": bounded_baseline_value(
                                &baseline,
                                preferred_partition(resident, path),
                            ),
                        }),
                        filters.max_output_tokens,
                    ),
                    loc::Completeness::partial(
                        loc::ReasonCode::ModalityDegraded,
                        "diagnostics baseline could not classify this file",
                    ),
                );
            }
        };

    // Author filter before any histogram or shaping, so `counts` reflects what
    // the response can actually show. Blame failures keep everything (fail-open).
    let mut findings_ignored_by_author = 0usize;
    let diagnostics = match resident.author_filter() {
        Some(filter) if !diagnostics.is_empty() => {
            match filter.lines_kept_cached(&resident.abs_path_for(path), file_text.as_bytes()) {
                Ok(keep) => {
                    let index = line_index::LineIndex::new(&file_text);
                    let before = diagnostics.len();
                    let kept: Vec<_> = diagnostics
                        .into_iter()
                        .filter(|d| {
                            crate::diagnostics_state::diagnostic_survives_authors(
                                &keep, &index, d.range,
                            )
                        })
                        .collect();
                    findings_ignored_by_author = before - kept.len();
                    kept
                }
                Err(error) => {
                    tracing::warn!(%error, "blame failed; keeping every finding for the file");
                    diagnostics
                }
            }
        }
        _ => diagnostics,
    };
    // Method spans for the graph bridge: each finding inside a method carries the
    // method's durable graph id so the agent can pivot to `graph callers`.
    let methods = method_ranges(&analysis.document_symbols(file_id));
    // The base comes from THIS resident's own root table, which read the disk once, before the
    // walk that gave the resident its files. Reading it again here would strip paths belonging
    // to the resident's generation against whatever the workspace spelling names now — and a
    // root reached through a link that has since been retargeted names another directory, so
    // every path-keyed id would silently stop being minted.
    let graph_root = ide::StripRoot::pinned(resident.workspace_roots().workspace_canonical());
    let file_location = answer_location(
        resident.workspace_roots(),
        root_id,
        requested_spelling,
        &resident.abs_path_for(path),
    );
    let line_index = line_index::LineIndex::new(&file_text);

    let mut counts = Counts::default();
    let mut findings: Vec<Value> = Vec::new();
    // `count_capped`: the `max_findings` count cap dropped findings — distinct from the byte
    // budget, because raising `max_output_tokens` alone cannot recover past the count cap.
    let mut count_capped = false;

    for diag in &diagnostics {
        let out = diag.to_output(&file_text);
        if !filters.codes.is_empty() && !filters.codes.iter().any(|c| c == &out.code) {
            continue;
        }
        if let Some((start, end)) = filters.range {
            // Keep a finding whose line span intersects the requested range.
            if out.end_line < start || out.start_line > end {
                continue;
            }
        }
        let bucket = SeverityBucket::from(diag.severity);
        // `counts` is the full histogram of what passed codes/range, before the floor.
        counts.add(bucket);
        if bucket < filters.min_severity {
            continue;
        }
        if findings.len() >= filters.max_findings {
            count_capped = true;
            continue;
        }
        let graph_id = graph_id_for(diag.range, &methods, path, &graph_root);
        let place = FindingPlace {
            file: &file_location,
            range: line_index.utf16_line_col_range(&file_text, diag.range),
            enclosing: enclosing_method_range(diag.range, &methods)
                .and_then(|range| line_index.utf16_line_col_range(&file_text, range)),
        };
        findings.push(finding_value(diag, &out, bucket, graph_id, filters.detailed, &place));
    }

    // Byte budget on top of the `max_findings` count cap: a file of small findings stays
    // within `max_findings`, but a handful of long messages can still blow the response
    // budget, so trim to fit and flag it. `counts` stays the full honest histogram.
    let budget_exhausted = filters
        .max_output_tokens
        .map(|budget| trim_items_to_budget(&mut findings, budget))
        .unwrap_or(false);

    // The author filter attributes against pinned inputs (HEAD + mailmap);
    // folding them into the result id makes a filter rebuild after a ref move
    // or mailmap edit observable without a content change.
    let mut rid = result_id(path, generation, &file_text);
    if let Some(filter) = resident.author_filter() {
        rid = format!("{rid}@{}", filter.short_identity());
    }
    rid = format!("{rid}@{}", resident.diagnostics_baseline().epoch());
    let mut body = json!({
        "result_id": rid,
        "kind": "full",
        "counts": counts.to_value(),
        "truncated": count_capped || budget_exhausted,
        "findings": findings,
        "baseline": bounded_baseline_value(
            &baseline,
            preferred_partition(resident, path),
        ),
    });
    // An empty report for a file outside the vendor-diff scope must not read as
    // "no findings": say explicitly that the file was not analyzed.
    if !resident.path_in_scope(path) {
        body["out_of_scope"] = json!(true);
        body["scope_hint"] =
            json!("file has no changed lines vs [analysis].diff_base and was not analyzed");
    }
    if findings_ignored_by_author > 0 {
        body["findings_ignored_by_author"] = json!(findings_ignored_by_author);
        body["author_hint"] =
            json!("findings on lines authored by [analysis].ignored_authors were suppressed");
    }
    if budget_exhausted {
        body["budget_exhausted"] = json!(true);
        // When the count cap also fired, say so: raising `max_output_tokens` alone stops at
        // `max_findings`, so the agent must lift both to see the rest.
        body["budget_hint"] = json!(if count_capped {
            "findings truncated by both max_output_tokens and the max_findings cap; narrow with `codes`/`min_severity`/range, or raise BOTH max_output_tokens and max_findings"
        } else {
            "findings truncated to fit max_output_tokens; narrow with `codes`/`min_severity`/range or raise the budget"
        });
    }
    let budget_exhausted =
        fit_diagnostics_response_budget(&mut body, filters.max_output_tokens, "findings")
            || budget_exhausted;

    let completeness = loc::Completeness::complete()
        .when(
            budget_exhausted,
            loc::ReasonCode::OutputBudget,
            "findings trimmed to fit max_output_tokens",
        )
        .when(count_capped, loc::ReasonCode::ResultCap, "findings capped by max_findings")
        .when(
            !resident.path_in_scope(path),
            loc::ReasonCode::OutOfAnalysisScope,
            "file has no changed lines vs [analysis].diff_base and was not analyzed",
        )
        .when(
            lacks_owning_configuration(analysis.database(), &resident.abs_path_for(path)),
            loc::ReasonCode::OwningConfigurationMissing,
            "the file belongs to an external data processor or report and the project \
             declares no main configuration; calls into it are reported as unresolved",
        )
        // Author suppression narrows the analysed set exactly as `[analysis].diff_base`
        // does; leaving it out of the envelope would let a consumer that reads only the
        // envelope call a pruned answer whole.
        .when(
            findings_ignored_by_author > 0,
            loc::ReasonCode::OutOfAnalysisScope,
            "findings on lines authored by [analysis].ignored_authors were suppressed",
        );
    // Note what is NOT here: the workspace-wide unread counter. This answer is about ONE
    // file, and a hole in an unrelated module does not make it any less whole — the file
    // that IS a hole gets its own `unreadable` branch above. Marking every answer partial
    // while any file anywhere is unreadable would teach a consumer to ignore the field.

    (body, completeness)
}

fn unclassified_baseline(resident: &DiagnosticsResident) -> DiagnosticsBaselineSummary {
    use ide::diagnostics_baseline::DiagnosticsBaselineState;
    use ide_host_core::diagnostics_baseline::DiagnosticsBaselineSnapshot;

    match resident.diagnostics_baseline() {
        DiagnosticsBaselineSnapshot::Disabled => DiagnosticsBaselineSummary::disabled(),
        snapshot @ DiagnosticsBaselineSnapshot::Error { .. } => {
            snapshot.error_summary().expect("error snapshot has a summary")
        }
        DiagnosticsBaselineSnapshot::Ready { baseline, project_path, .. } => {
            DiagnosticsBaselineSummary {
                state: DiagnosticsBaselineState::Partial,
                selection: None,
                partitions_enabled: None,
                partitions_unsuppressed: None,
                unsuppressed: None,
                new: Some(0),
                known: Some(0),
                resolved: Some(0),
                path: Some(project_path.clone()),
                schema_version: Some(baseline.schema_version),
                manifest_schema_version: None,
                complete: false,
                error_code: None,
                detail: None,
                partitions: vec![],
                errors: vec![],
            }
        }
        DiagnosticsBaselineSnapshot::ReadySet { baseline, plan, project_path, .. } => {
            DiagnosticsBaselineSummary {
                state: DiagnosticsBaselineState::Partial,
                selection: Some(plan.selection),
                partitions_enabled: Some(plan.enabled_partition_ids.len()),
                partitions_unsuppressed: Some(
                    plan.partitions.len() - plan.enabled_partition_ids.len(),
                ),
                unsuppressed: Some(0),
                new: Some(0),
                known: Some(0),
                resolved: Some(0),
                path: Some(project_path.clone()),
                schema_version: Some(
                    ide::partitioned_diagnostics_baseline::DIAGNOSTICS_BASELINE_PARTITION_SCHEMA_VERSION,
                ),
                manifest_schema_version: Some(baseline.manifest.schema_version),
                complete: false,
                error_code: None,
                detail: None,
                partitions: plan
                    .partitions
                    .iter()
                    .map(|expected| {
                        let policy = plan.policy_for_partition(&expected.id).unwrap();
                        let loaded = baseline.partitions.get(&expected.id);
                        ide::diagnostics_baseline::DiagnosticsBaselinePartitionSummary {
                            id: expected.id.clone(),
                            identity: expected.identity.clone(),
                            policy,
                            path: loaded.map(|partition| partition.file.to_string()),
                            schema_version: loaded.map(|_| ide::partitioned_diagnostics_baseline::DIAGNOSTICS_BASELINE_PARTITION_SCHEMA_VERSION),
                            state: DiagnosticsBaselineState::Partial,
                            new: 0,
                            known: 0,
                            resolved: 0,
                            unsuppressed: 0,
                            complete: false,
                        }
                    })
                    .collect(),
                errors: vec![],
            }
        }
    }
}

/// A file's path as the baseline spells it: relative to the workspace root, with `/`
/// separators.
///
/// Both sides are canonicalised first. The request may spell the path as `./x`, through
/// a symlinked component, or in another case on a case-insensitive file system, and a
/// literal `strip_prefix` then fails — silently leaving the request's own spelling in
/// the fingerprint, so the very same finding of the very same file comes back as `new`
/// instead of `known`. The root itself may equally be given through a symlink.
fn baseline_project_path(workspace_root: &Path, path: &Path) -> String {
    let canonical = |path: &Path| path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let root = canonical(workspace_root);
    let file = canonical(path);
    file.strip_prefix(&root)
        .unwrap_or(&file)
        .to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "/")
}

fn preferred_partition<'a>(resident: &'a DiagnosticsResident, path: &Path) -> Option<&'a str> {
    let (_, plan, _) = resident.diagnostics_baseline().ready_set()?;
    let relative = baseline_project_path(resident.workspace_root(), path);
    plan.owner_for_project_path(&relative)
}

fn bounded_baseline_value(summary: &DiagnosticsBaselineSummary, preferred: Option<&str>) -> Value {
    let mut partitions = summary.partitions.clone();
    partitions.sort_by(|left, right| left.id.cmp(&right.id));
    if let Some(preferred) = preferred {
        if let Some(index) = partitions.iter().position(|partition| partition.id == preferred) {
            let preferred = partitions.remove(index);
            partitions.insert(0, preferred);
        }
    }
    let partitions_total = partitions.len();
    let values = serde_json::to_value(partitions).expect("partition summaries serialize");
    let mut value = serde_json::to_value(summary).expect("baseline summary serializes");
    value["partitions"] = values;
    value["partitions_total"] = json!(partitions_total);
    value["partitions_returned"] = json!(partitions_total);
    value["partitions_truncated"] = json!(false);
    if summary.state == ide::diagnostics_baseline::DiagnosticsBaselineState::Error {
        value["errors_total"] = json!(summary.errors.len().max(1));
        // The error branch of the output schema requires `errors`, and the summary omits
        // an empty list when serialising. A state that reports an error with no error in
        // it would then match no branch of the tool's own schema.
        if !value["errors"].is_array() {
            value["errors"] = json!([{
                "partition_id": Value::Null,
                "code": summary.error_code.clone().unwrap_or_default(),
                "detail": summary.detail.clone().unwrap_or_default(),
            }]);
        }
    }
    value
}

fn classify_file_baseline(
    resident: &DiagnosticsResident,
    path: &Path,
    file_text: &str,
    diagnostics: Vec<ide::Diagnostic>,
) -> Result<(Vec<ide::Diagnostic>, DiagnosticsBaselineSummary), Box<DiagnosticsBaselineSummary>> {
    use ide_host_core::diagnostics_baseline::DiagnosticsBaselineSnapshot;
    let relative = baseline_project_path(resident.workspace_root(), path);
    let source_lines: Vec<_> = file_text.lines().collect();
    let candidates: Vec<_> = diagnostics
        .into_iter()
        .enumerate()
        .map(|(index, diagnostic)| {
            let output = diagnostic.to_output(file_text);
            BaselineDiagnosticCandidate {
                diagnostic: (index, diagnostic),
                path: relative.clone(),
                code: output.code,
                snippet: Some(ide::diagnostics_baseline::diagnostic_line_snippet(
                    &source_lines,
                    output.start_line,
                )),
                message: output.message,
                severity: output.severity,
                range: DiagnosticsBaselineRange {
                    start_line: output.start_line as u32,
                    start_column: output.start_column as u32,
                    end_line: output.end_line as u32,
                    end_column: output.end_column as u32,
                },
            }
        })
        .collect();
    let snapshot = resident.diagnostics_baseline();
    if matches!(snapshot, DiagnosticsBaselineSnapshot::Disabled) {
        return Ok((
            candidates.into_iter().map(|item| item.diagnostic.1).collect(),
            DiagnosticsBaselineSummary::disabled(),
        ));
    }
    if matches!(snapshot, DiagnosticsBaselineSnapshot::Error { .. }) {
        return Err(Box::new(snapshot.error_summary().expect("error snapshot has a summary")));
    }
    let completed_files = if resident.config().scope.is_none() {
        std::collections::BTreeSet::from([relative.clone()])
    } else {
        std::collections::BTreeSet::new()
    };
    match snapshot {
        DiagnosticsBaselineSnapshot::Ready { baseline, project_path, .. } => {
            let classified = classify_diagnostics(
                baseline,
                project_path.clone(),
                candidates,
                &DiagnosticsBaselineCoverage::Partial { completed_files },
            )
            .map_err(|error| {
                Box::new(DiagnosticsBaselineSummary {
                    state: ide::diagnostics_baseline::DiagnosticsBaselineState::Error,
                    selection: None,
                    partitions_enabled: None,
                    partitions_unsuppressed: None,
                    unsuppressed: None,
                    new: None,
                    known: None,
                    resolved: None,
                    path: Some(project_path.clone()),
                    schema_version: Some(baseline.schema_version),
                    manifest_schema_version: None,
                    complete: false,
                    error_code: Some("missing_snippet".to_owned()),
                    detail: Some(error.to_string()),
                    partitions: vec![],
                    errors: vec![],
                })
            })?;
            Ok((
                classified.new.into_iter().map(|item| item.diagnostic.1).collect(),
                classified.summary,
            ))
        }
        DiagnosticsBaselineSnapshot::ReadySet { baseline, plan, project_path, .. } => {
            let owner = plan.owner_for_project_path(&relative);
            let Some(owner) = owner else {
                let mut summary = unclassified_baseline(resident);
                summary.state = ide::diagnostics_baseline::DiagnosticsBaselineState::Error;
                summary.complete = false;
                summary.error_code = Some("unowned_diagnostic".to_owned());
                // Without this the agent gets a bare error code: no file, no reason, and
                // nothing to act on.
                summary.detail = Some(format!(
                    "no partition owns {relative}; check [diagnostics.baseline] groups \
                     and [source] roots"
                ));
                return Err(Box::new(summary));
            };
            let wrapped = candidates
                .into_iter()
                .map(|candidate| {
                    ide::partitioned_diagnostics_baseline::PartitionedBaselineDiagnosticCandidate {
                        partition_id: owner.to_owned(),
                        candidate,
                    }
                })
                .collect();
            let coverage = plan
                .partitions
                .iter()
                .map(|partition| {
                    let files = if partition.id == owner {
                        completed_files.clone()
                    } else {
                        std::collections::BTreeSet::new()
                    };
                    (
                        partition.id.clone(),
                        DiagnosticsBaselineCoverage::Partial { completed_files: files },
                    )
                })
                .collect();
            let classified =
                ide::partitioned_diagnostics_baseline::classify_partitioned_diagnostics(
                    baseline,
                    plan,
                    project_path.clone(),
                    wrapped,
                    &coverage,
                )
                .map_err(|error| {
                    let mut summary = unclassified_baseline(resident);
                    summary.state = ide::diagnostics_baseline::DiagnosticsBaselineState::Error;
                    summary.complete = false;
                    summary.error_code = Some(
                        match &error {
                            ide::partitioned_diagnostics_baseline::PartitionedDiagnosticsClassificationError::MissingSnippet(_) => "missing_snippet",
                            ide::partitioned_diagnostics_baseline::PartitionedDiagnosticsClassificationError::MissingEnabledPartition(_) => "invalid_set",
                        }
                        .to_owned(),
                    );
                    summary.detail = Some(error.to_string());
                    Box::new(summary)
                })?;
            Ok((
                classified
                    .new
                    .into_iter()
                    .chain(classified.unsuppressed)
                    .map(|item| item.diagnostic.1)
                    .collect(),
                classified.summary,
            ))
        }
        DiagnosticsBaselineSnapshot::Disabled | DiagnosticsBaselineSnapshot::Error { .. } => {
            unreachable!()
        }
    }
}

/// The `workspace` action's result body: whole-config diagnostics aggregated per code
/// (`{code, severity, count, files_affected}`), never per-finding — an opt-in, bounded
/// overview. `result_id` is generation-keyed (no per-file content hash applies).
/// Formats an already-computed sweep; the caller runs `workspace_aggregates` (with its
/// cancellation bridge) and owns the cancel logging.
pub(crate) fn workspace_findings(
    sweep: &WorkspaceSweep,
    generation: u64,
    max_output_tokens: Option<usize>,
) -> (Value, loc::Completeness) {
    let mut aggregates: Vec<Value> = sweep
        .aggregates
        .iter()
        .map(|a| {
            json!({
                "code": a.code,
                "severity": a.severity.as_str(),
                "count": a.count,
                "files_affected": a.files_affected,
            })
        })
        .collect();
    // `aggregates` is already sorted most-severe-first; a budget trim drops the least-severe
    // tail. `files_swept`/`files_total`/`truncated` still describe the whole sweep.
    let budget_exhausted = max_output_tokens
        .map(|budget| trim_items_to_budget(&mut aggregates, budget))
        .unwrap_or(false);

    // Fold the author filter's pinned identity (HEAD + mailmap) into the id:
    // a filter rebuild changes the aggregates without bumping the generation.
    let rid = match sweep.author_head.as_deref() {
        Some(identity) => format!("workspace@{generation}@{identity}"),
        None => format!("workspace@{generation}"),
    };
    let rid = format!("{rid}@{}", sweep.baseline_epoch);
    let mut body = json!({
        "result_id": rid,
        "files_swept": sweep.files_swept,
        "files_total": sweep.files_total,
        "truncated": sweep.truncated || budget_exhausted,
        "aggregates": aggregates,
        "baseline": bounded_baseline_value(&sweep.baseline, None),
    });
    if sweep.baseline.state == ide::diagnostics_baseline::DiagnosticsBaselineState::Error {
        body.as_object_mut().expect("workspace body is an object").remove("aggregates");
    }
    if sweep.files_unread > 0 {
        body["files_unread"] = json!(sweep.files_unread);
    }
    if sweep.files_out_of_scope > 0 {
        body["files_out_of_scope"] = json!(sweep.files_out_of_scope);
        body["scope_hint"] =
            json!("files with no changed lines vs [analysis].diff_base were not analyzed");
    }
    if sweep.findings_ignored_by_author > 0 {
        body["findings_ignored_by_author"] = json!(sweep.findings_ignored_by_author);
        body["author_hint"] =
            json!("findings on lines authored by [analysis].ignored_authors were suppressed");
    }
    if budget_exhausted {
        body["budget_exhausted"] = json!(true);
        body["budget_hint"] =
            json!("aggregates truncated to fit max_output_tokens; narrow with `codes`/`min_severity` or raise the budget");
    }
    let budget_exhausted =
        fit_diagnostics_response_budget(&mut body, max_output_tokens, "aggregates")
            || budget_exhausted;

    let completeness = loc::Completeness::complete()
        .when(
            budget_exhausted,
            loc::ReasonCode::OutputBudget,
            "aggregates trimmed to fit max_output_tokens",
        )
        .when(
            sweep.truncated,
            loc::ReasonCode::ResultCap,
            "the sweep stopped at max_files; files_total names the whole config",
        )
        .when(
            sweep.files_unread > 0,
            loc::ReasonCode::UnreadableFiles,
            "some workspace files could not be read and were not analyzed",
        )
        .when(
            sweep.files_out_of_scope > 0,
            loc::ReasonCode::OutOfAnalysisScope,
            "files with no changed lines vs [analysis].diff_base were not analyzed",
        )
        .when(
            sweep.findings_ignored_by_author > 0,
            loc::ReasonCode::OutOfAnalysisScope,
            "findings on lines authored by [analysis].ignored_authors were suppressed",
        )
        // An unreadable baseline empties the sweep entirely, and a consumer that reads
        // only the envelope would take that emptiness for a clean configuration.
        .when(
            sweep.baseline.state == ide::diagnostics_baseline::DiagnosticsBaselineState::Error,
            loc::ReasonCode::ModalityDegraded,
            "the diagnostics baseline could not be read; the sweep produced no aggregates",
        )
        .when(
            sweep.lacks_owning_configuration,
            loc::ReasonCode::OwningConfigurationMissing,
            "the project analyzes external data processors or reports and declares no main \
             configuration; calls into it are counted as unresolved",
        );
    (body, completeness)
}

/// Where one finding sits: the file's pair (or why there is none) plus the finding's own
/// span and its enclosing method, already in contract units.
struct FindingPlace<'a> {
    file: &'a Result<loc::Location, loc::LocationUnavailable>,
    range: Option<line_index::LineColRange>,
    enclosing: Option<line_index::LineColRange>,
}

/// Shrinks `body` to the budget, returning whether anything was dropped. The verdict
/// matters to the caller: `completeness` is computed from a flag taken BEFORE this runs,
/// and a body that trims itself here would otherwise be announced as complete.
#[must_use]
fn fit_diagnostics_response_budget(
    body: &mut Value,
    max_output_tokens: Option<usize>,
    items_key: &str,
) -> bool {
    let Some(limit) = max_output_tokens.map(|tokens| tokens.saturating_mul(4)) else {
        return false;
    };
    let mut trimmed = false;
    while serde_json::to_vec(body).is_ok_and(|bytes| bytes.len() > limit) {
        let partitions = body["baseline"]["partitions"].as_array_mut();
        if partitions.is_some_and(|partitions| !partitions.is_empty()) {
            let partitions = body["baseline"]["partitions"].as_array_mut().unwrap();
            partitions.pop();
            let returned = partitions.len();
            body["baseline"]["partitions_returned"] = json!(returned);
            body["baseline"]["partitions_truncated"] = json!(true);
            trimmed = true;
            continue;
        }
        let errors = body["baseline"]["errors"].as_array_mut();
        if errors.is_some_and(|errors| errors.len() > 1) {
            body["baseline"]["errors"].as_array_mut().unwrap().pop();
            trimmed = true;
            continue;
        }
        let items = body[items_key].as_array_mut();
        if items.is_some_and(|items| !items.is_empty()) {
            body[items_key].as_array_mut().unwrap().pop();
            body["truncated"] = json!(true);
            body["budget_exhausted"] = json!(true);
            trimmed = true;
            continue;
        }
        if truncate_response_string(body, limit) {
            trimmed = true;
            continue;
        }
        break;
    }
    trimmed
}

fn budgeted_diagnostics_response(mut body: Value, max_output_tokens: Option<usize>) -> Value {
    // The envelope for these responses carries no completeness verdict, so the
    // trimming verdict has no reader here; the body flags itself.
    let _ = fit_diagnostics_response_budget(&mut body, max_output_tokens, "findings");
    body
}

fn truncate_response_string(body: &mut Value, limit: usize) -> bool {
    const POINTERS: &[&str] = &[
        "/detail",
        "/path",
        "/baseline/detail",
        "/baseline/path",
        "/baseline/errors/0/detail",
        "/scope_hint",
        "/author_hint",
        "/budget_hint",
    ];
    for pointer in POINTERS {
        let current = serde_json::to_vec(body).map_or(limit, |bytes| bytes.len());
        let Some(value) = body.pointer_mut(pointer) else { continue };
        let Some(text) = value.as_str() else { continue };
        if text.len() <= 1 {
            continue;
        }
        let keep = text.len().saturating_sub(current.saturating_sub(limit)).max(1);
        let mut boundary = keep.min(text.len());
        while !text.is_char_boundary(boundary) {
            boundary -= 1;
        }
        *value = json!(&text[..boundary]);
        return true;
    }
    false
}

fn finding_value(
    diag: &ide::Diagnostic,
    out: &ide::DiagnosticOutput,
    bucket: SeverityBucket,
    graph_id: Option<String>,
    detailed: bool,
    place: &FindingPlace<'_>,
) -> Value {
    let mut v = json!({
        "code": out.code,
        "severity": bucket.as_str(),
        "message": out.message,
        "range": {
            "start_line": out.start_line,
            "start_column": out.start_column,
            "end_line": out.end_line,
            "end_column": out.end_column,
        },
        "has_fix": !diag.fixes.is_empty(),
    });
    // `range` above stays as it was — 0-based lines, columns in CODE POINTS — and the
    // contract location travels beside it. In a location, `range` is the finding's own
    // span (a diagnostic has no name of its own to point at) and `enclosing_range` is the
    // method that holds it.
    match place.file {
        Ok(location) => {
            v["location"] = location
                .clone()
                .with_range(place.range.map(loc::PositionRange::from))
                .with_enclosing_range(place.enclosing.map(loc::PositionRange::from))
                .to_value();
        }
        Err(reason) => {
            v["location_unavailable"] = json!(reason.code());
        }
    }
    if !out.tags.is_empty() {
        v["tags"] = json!(out.tags);
    }
    // Агенту структура полезнее суффикса в тексте: тот же факт в `message` стоил
    // бы токенов дважды, а текстовый блок ответа и так зеркалит этот JSON.
    let standards = ide::standards(diag.code);
    if !standards.is_empty() {
        v["standards"] = Value::Array(
            standards.iter().map(|id| json!({ "id": id, "url": ide::standard_url(*id) })).collect(),
        );
    }
    if let Some(graph_id) = graph_id {
        v["graph_id"] = json!(graph_id);
    }
    if detailed {
        v["internal_severity"] = json!(diag.severity.as_str());
        if let Some(fix) = diag.fixes.first() {
            v["fix"] = json!({ "label": fix.label });
        }
    }
    v
}

/// Flatten document symbols into `(method_name, source_range)` for every procedure
/// and function, descending through `#Область` regions (whose method children are
/// nested). Module-level vars and the regions themselves are skipped.
fn method_ranges(symbols: &[DocumentSymbol]) -> Vec<(String, TextRange)> {
    let mut out = Vec::new();
    collect_methods(symbols, &mut out);
    out
}

fn collect_methods(symbols: &[DocumentSymbol], out: &mut Vec<(String, TextRange)>) {
    for s in symbols {
        if matches!(s.kind(), SymbolKind::Procedure | SymbolKind::Function) {
            out.push((s.name.clone(), s.range));
        }
        if !s.children.is_empty() {
            collect_methods(&s.children, out);
        }
    }
}

/// The durable graph id of the method whose span contains `range`, if any. `None` when the
/// finding is not inside a method (module body). Module-keyed methods (common/object/manager)
/// resolve regardless of `workspace_root`; form/command/file-module methods fall back to the
/// `method/file/<rel>::<name>` id the graph also mints — `workspace_root` strips an absolute
/// request path to the encoder's rel. `graph_id` is best-effort decoration.
fn graph_id_for(
    range: TextRange,
    methods: &[(String, TextRange)],
    path: &Path,
    workspace_root: &ide::StripRoot,
) -> Option<String> {
    let name = methods.iter().find(|(_, r)| r.contains_range(range)).map(|(n, _)| n.as_str())?;
    ide::method_graph_id(&path.to_string_lossy(), name, Some(workspace_root))
}

/// The span of the method containing `range` — the same lookup [`graph_id_for`] does, by
/// the same rule, so a finding's `graph_id` and its `enclosing_range` can never name
/// different methods.
fn enclosing_method_range(range: TextRange, methods: &[(String, TextRange)]) -> Option<TextRange> {
    methods.iter().find(|(_, r)| r.contains_range(range)).map(|(_, r)| *r)
}

/// The pull-model freshness handle: `<path>@<generation>@<content-hash>`. The content
/// hash MUST be present so a body edit inside the drift-scan throttle window cannot be
/// masked by an unchanged generation (a stale `unchanged` would otherwise be possible
/// once `previous_result_id` lands).
fn result_id(path: &Path, generation: u64, text: &str) -> String {
    let mut hasher = DefaultHasher::new();
    path.hash(&mut hasher);
    text.hash(&mut hasher);
    format!("{}@{generation}@{:016x}", path.to_string_lossy(), hasher.finish())
}

/// The four-bucket severity histogram, always emitted so the agent knows what a
/// floor or a cap hid.
#[derive(Default)]
struct Counts {
    error: usize,
    warning: usize,
    info: usize,
    hint: usize,
}

impl Counts {
    fn add(&mut self, bucket: SeverityBucket) {
        match bucket {
            SeverityBucket::Error => self.error += 1,
            SeverityBucket::Warning => self.warning += 1,
            SeverityBucket::Info => self.info += 1,
            SeverityBucket::Hint => self.hint += 1,
        }
    }

    fn to_value(&self) -> Value {
        json!({ "error": self.error, "warning": self.warning, "info": self.info, "hint": self.hint })
    }
}

fn schema_json() -> Value {
    json!({
        "schema_version": "16",
        "actions": ["catalog", "schema", "status", "file", "workspace"],
        "severities": ["error", "warning", "info", "hint"],
        "status_result": {
            "state": "disabled | idle | loading | ready | failed — resident lifecycle",
            "generation": "u64 — bumped on each build/reload (independent of the graph tool's revision)",
            "files": "usize — SERVED resident .bsl count (present when ready); excludes files counted in unread_files",
            "unread_files": "usize — workspace .bsl files that exist but could not be read (present when ready); they are held out of service, answered as unreadable, and re-read every drift window",
            "reload": "none | running | failed — background reload state",
            "elapsed_ms": "u64 — ms since the current build started (present while loading)",
            "error": "string — failure message (present when failed)",
            "standalone_extension": "string — present when the project is analyzed without its main configuration: the workspace root is itself a configuration extension, or the project declares external data processors/reports (EPF/ERF) and no base; calls into that configuration are reported unresolved. Both conditions can hold at once, one line each",
            "owns_caches": "bool — false when a newer daemon generation owns this workspace's derived caches; this backend still answers from what it holds but produces no new derived state. Absent until this daemon's ownership verdict has been established at all, which a background check does on its own"
        },
        "catalog_entry": {
            "code": "string — stable diagnostic code (e.g. CyclomaticComplexity)",
            "title": "string — localized name",
            "default_severity": "error | warning | info | hint",
            "type": "error | code_smell | vulnerability | security_hotspot",
            "activated_by_default": "bool — whether enabled under the default config",
            "clean_code_attribute": "consistent | intentional | adaptable | responsible",
            "tags": "string[] — omitted when empty"
        },
        "catalog_params": {
            "codes": "string[] — narrow the catalog to these codes (optional)",
            "locale": "ru | en (default ru) — title language"
        },
        "file_params": {
            "path": "string — absolute or workspace-relative .bsl path, or a path relative to root_id (required)",
            "root_id": "string — the source root `path` is spelled against, as carried by every `search` code hit; omit for a workspace-relative path, \"\" names the configuration (optional)",
            "min_severity": "error | warning | info | hint (default warning) — inclusive floor",
            "codes": "string[] — keep only these codes (optional)",
            "range_start": "usize — 0-based first line to include (optional)",
            "range_end": "usize — 0-based last line to include (optional)",
            "detail": "concise | detailed (default concise) — detailed adds internal_severity + fix",
            "max_findings": "usize — cap on findings (default 200)"
        },
        "finding": {
            "code": "string",
            "severity": "error | warning | info | hint (4-bucket)",
            "message": "string",
            "range": "{ start_line, start_column, end_line, end_column } — 0-based; DEPRECATED, columns are counted in code POINTS. Use `location.range`, whose units are declared",
            "location": "the location contract v1: { root_id, path, range, enclosing_range, position_encoding, schema_version }. `range` is the finding's own span (a diagnostic has no name of its own), `enclosing_range` the method holding it; 0-based, end-exclusive, columns in UTF-16 code units",
            "location_unavailable": "string — machine reason, present instead of `location` when the file's (root_id, path) pair could not be formed",
            "standards": "[{ id: u16, url }] — 1C development standards the rule enforces, checked requirement first; omitted when the diagnostic has no standard behind it",
            "tags": "string[] — omitted when empty",
            "has_fix": "bool — whether an automatic fix is attached",
            "graph_id": "durable id of the containing method — pass to `graph callers`/`node` (omitted when not in a method or not an indexable module)",
            "internal_severity": "7-grade name (detailed only)",
            "fix": "{ label } (detailed only, when present)"
        },
        "baseline_result": {
            "state": "disabled | full | partial | error",
            "complete": "bool — true only when the result proves complete baseline coverage (disabled is complete)",
            "selection": "all | selective — effective partition selection (partitioned mode only)",
            "partitions_enabled": "usize — owners using baseline policy (partitioned mode only)",
            "partitions_unsuppressed": "usize — owners using unsuppressed policy (partitioned mode only)",
            "unsuppressed": "usize — active findings from unsuppressed owners (partitioned mode only)",
            "new": "usize — current findings absent from the baseline (omitted for disabled/error)",
            "known": "usize — current findings matched by the baseline (omitted for disabled/error)",
            "resolved": "usize — baseline entries proven absent in completed files (omitted for disabled/error)",
            "path": "string — configured project-relative baseline path (omitted when disabled)",
            "schema_version": "u32 — loaded baseline file schema version (when known)",
            "manifest_schema_version": "u32 — partition-set manifest schema version (partitioned mode only)",
            "partitions_total": "usize — total partition summaries before response-budget shaping",
            "partitions_returned": "usize — partition summaries present in partitions",
            "partitions_truncated": "bool — one or more partition summaries were omitted by max_output_tokens",
            "partitions": "partition summary[] — sorted by id; file owner first when any detail fits",
            "errors_total": "usize — number of deterministic set errors (error only)",
            "errors": "set error[] — deterministic partition/code/detail/epoch entries bounded by max_output_tokens (error only)",
            "error_code": "string — stable load/validation error code (error only)",
            "detail": "string — actionable load/validation error detail (error only)"
        },
        "file_result": {
            "result_id": "<path>@<generation>@<content-hash>[@<blame-head>]@<baseline-epoch> — pull-model freshness handle; the blame-head component appears when [analysis].ignored_authors is active",
            "kind": "full — (unchanged reserved for a future previous_result_id round-trip)",
            "baseline": "baseline_result — classification summary, present on success and baseline errors",
            "counts": "{ error, warning, info, hint } — full histogram before the floor/cap (after the author filter)",
            "truncated": "bool — the findings cap was hit; counts still complete",
            "findings": "finding[]",
            "out_of_scope": "bool — file has no changed lines vs [analysis].diff_base and was NOT analyzed; an empty findings list is not 'clean' (present only under a configured scope)",
            "scope_hint": "string — present with out_of_scope",
            "findings_ignored_by_author": "usize — findings suppressed because every covered line is blamed to [analysis].ignored_authors; present only when > 0",
            "author_hint": "string — present with findings_ignored_by_author"
        },
        "workspace_params": {
            "min_severity": "error | warning | info | hint (default warning) — inclusive floor",
            "codes": "string[] — keep only these codes (optional)",
            "max_files": "usize — cap on files swept (default 1000, hard ceiling 5000); a larger config surfaces as truncated with the true files_total"
        },
        "workspace_result": {
            "result_id": "workspace@<generation>[@<blame-head>]@<baseline-epoch> — the blame-head component appears when [analysis].ignored_authors is active",
            "baseline": "baseline_result — classification summary, present on success and baseline errors",
            "findings_ignored_by_author": "usize — findings suppressed by [analysis].ignored_authors across the sweep; present only when > 0",
            "author_hint": "string — present with findings_ignored_by_author",
            "files_swept": "usize — files actually analyzed",
            "files_total": "usize — ALL workspace files, including ones that could not be read; files_swept can trail it because of the file cap (truncated), the analysis scope (files_out_of_scope) AND/OR unreadable bytes (files_unread)",
            "files_unread": "usize — files counted in files_total that could not be read and so were not swept; present only when > 0",
            "files_out_of_scope": "usize — files excluded by [analysis].diff_base (no changed lines); present only when > 0",
            "scope_hint": "string — present with files_out_of_scope",
            "truncated": "bool — the file cap was hit",
            "aggregates": "{ code, severity, count, files_affected }[] — per-code, most-severe-then-most-frequent first; NO per-finding detail"
        },
        "envelope": {
            "revision": "u64 — resident-db generation the answer was computed at; DEPRECATED in favour of freshness.revision, which carries the same value",
            "stale": "bool — this answer is not current: the workspace drifted on disk since this generation, and/or some workspace file could not be read (see unread_files); DEPRECATED in favour of freshness.stale",
            "reload": "none | running | failed — background reload state",
            "freshness": format!(
                "the location contract's envelope: {{ source, revision, topology_fingerprint, \
                 stale, completeness }}. `source` is always `{}` here — this tool answers from \
                 the resident database and from nothing else; the contract's full vocabulary of \
                 sources ({}) is in docs/mcp/LOCATION_CONTRACT.md. `completeness` is \
                 {{ status: complete | partial, reasons: [{{ code, detail }}] }} with code one \
                 of {}",
                ANSWERED_BY.as_str(),
                loc::FreshnessSource::vocabulary(),
                loc::ReasonCode::vocabulary(),
            ),
            "result": "the action payload (or an {error} object)"
        }
    })
}

fn locale_str(locale: Locale) -> &'static str {
    match locale {
        Locale::Ru => "ru",
        Locale::En => "en",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body_of(result: &CallToolResult) -> &Value {
        result.structured_content.as_ref().expect("structuredContent must be populated")
    }

    /// The text content block must parse back to exactly the `structuredContent`
    /// field, so structured-aware and plain clients see byte-identical JSON.
    fn assert_structured_mirrors_text(result: &CallToolResult) {
        let structured = body_of(result);
        let text = result.content[0].as_text().expect("text mirror").text.as_str();
        let parsed: Value = serde_json::from_str(text).expect("text mirror must be valid JSON");
        assert_eq!(&parsed, structured, "text mirror must match structuredContent");
    }

    #[test]
    fn schema_advertises_the_catalog_contract() {
        let result = schema();
        assert_structured_mirrors_text(&result);
        let body = body_of(&result);
        assert_eq!(body["schema_version"], "16");
        let actions = body["actions"].as_array().unwrap();
        assert!(actions.iter().any(|a| a == "catalog"));
        assert!(actions.iter().any(|a| a == "status"));
        assert!(actions.iter().any(|a| a == "file"));
        assert!(actions.iter().any(|a| a == "workspace"));
        assert!(body["status_result"]["state"].is_string(), "status action advertised");
        assert!(body["finding"]["graph_id"].is_string(), "graph bridge advertised");
        assert!(body["finding"]["standards"].is_string(), "standard refs advertised");
        assert!(body["workspace_result"]["aggregates"].is_string(), "workspace advertised");
        // Asserting the DESCRIPTIONS, not just the version: a bumped number over an
        // unchanged text certifies a contract the server no longer honours.
        assert!(
            body["status_result"]["unread_files"].is_string(),
            "unreadable-file count advertised"
        );
        assert!(
            body["workspace_result"]["files_unread"].is_string(),
            "sweep coverage gap advertised"
        );
        assert!(
            body["envelope"]["stale"].as_str().unwrap().contains("could not be read"),
            "stale no longer means drift alone, and the contract must say so"
        );
        assert!(
            body["status_result"]["files"].as_str().unwrap().contains("SERVED"),
            "`files` and `files_total` are now different numbers"
        );
        let sev = body["severities"].as_array().unwrap();
        assert_eq!(sev.len(), 4);
        assert!(sev.iter().any(|s| s == "error"));
        assert!(sev.iter().any(|s| s == "hint"));
        // The file contract is advertised for cold-start discovery.
        assert!(body["file_params"]["path"].is_string());
        assert!(body["file_result"]["result_id"].is_string());
        assert!(body["file_result"]["baseline"].is_string());
        assert!(body["workspace_result"]["baseline"].is_string());
        let baseline = &body["baseline_result"];
        for field in [
            "state",
            "complete",
            "selection",
            "partitions_enabled",
            "partitions_unsuppressed",
            "unsuppressed",
            "new",
            "known",
            "resolved",
            "path",
            "schema_version",
            "manifest_schema_version",
            "partitions_total",
            "partitions_returned",
            "partitions_truncated",
            "partitions",
            "errors_total",
            "error_code",
            "detail",
        ] {
            assert!(baseline[field].is_string(), "baseline field {field} is not advertised");
        }
        for state in ["disabled", "full", "partial", "error"] {
            assert!(baseline["state"].as_str().unwrap().contains(state));
        }
        assert!(body["envelope"]["revision"].is_string());
        // The version bump above is only honest if the new keys are described too.
        assert!(body["finding"]["location"].is_string(), "the location contract advertised");
        assert!(body["envelope"]["freshness"].is_string(), "the freshness envelope advertised");
        assert!(
            body["finding"]["range"].as_str().unwrap().contains("DEPRECATED"),
            "the legacy range is marked, not silently kept",
        );
    }

    /// The `schema` action exists so a consumer can write one branch per value it may see.
    /// Two halves to that, and they pull in opposite directions:
    ///
    /// - the reason codes are the contract's whole closed list, read out of the enum rather
    ///   than retyped, so a new one cannot ship with the prose lagging behind it;
    /// - the source is the ONE this tool serves, checked against what the envelope actually
    ///   stamps. Publishing the contract's full list here would have a consumer write branches
    ///   for sources `diagnostics` never produces.
    #[test]
    fn the_schema_names_the_source_this_tool_serves_and_every_reason_it_may_carry() {
        let body = schema().structured_content.expect("structuredContent");
        let prose = body["envelope"]["freshness"].as_str().expect("prose");

        let stamped = envelope(
            Freshness { revision: 1, topology: 2, stale: false, reload: "none" },
            loc::Completeness::complete(),
            json!({}),
        )
        .structured_content
        .expect("structuredContent");
        assert_eq!(
            stamped["freshness"]["source"], "resident",
            "the tool's own answers are what the schema describes",
        );
        assert!(
            prose.contains(&format!("`{}`", stamped["freshness"]["source"].as_str().unwrap())),
            "the schema must name the source the envelope stamps: {prose}",
        );

        for reason in loc::ReasonCode::ALL {
            assert!(
                prose.contains(reason.as_str()),
                "{} can appear in an envelope but the schema does not name it: {prose}",
                reason.as_str(),
            );
        }
    }

    #[test]
    fn diagnostics_selective_baseline_response_advertises_schema_15() {
        let body = schema_json();
        assert_eq!(body["schema_version"], "16");
        for field in ["selection", "partitions_enabled", "partitions_unsuppressed", "unsuppressed"]
        {
            assert!(body["baseline_result"][field].is_string());
        }
    }

    #[test]
    fn diagnostics_schema_json() {
        let body = schema_json();
        assert_eq!(body["schema_version"], "16");
        assert!(body["baseline_result"]["selection"].is_string());
        assert!(body["baseline_result"]["unsuppressed"].is_string());
    }

    #[test]
    fn min_severity_defaults_to_warning_and_rejects_unknown() {
        assert_eq!(parse_min_severity(None).unwrap(), SeverityBucket::Warning);
        assert_eq!(parse_min_severity(Some("error")).unwrap(), SeverityBucket::Error);
        assert_eq!(parse_min_severity(Some("hint")).unwrap(), SeverityBucket::Hint);
        assert!(parse_min_severity(Some("blocker")).is_err());
    }

    #[test]
    fn detail_defaults_to_concise_and_rejects_unknown() {
        assert!(!parse_detail(None).unwrap());
        assert!(!parse_detail(Some("concise")).unwrap());
        assert!(parse_detail(Some("detailed")).unwrap());
        // An unknown value errors rather than silently falling back to concise.
        let err = parse_detail(Some("bogus")).unwrap_err();
        assert!(err.contains("bogus") && err.contains("concise|detailed"), "{err}");
    }

    #[test]
    fn catalog_lists_every_code_with_required_fields() {
        let result = catalog(Locale::Ru, &[], None);
        assert_structured_mirrors_text(&result);
        let body = body_of(&result);
        assert_eq!(body["action"], "catalog");
        assert_eq!(body["locale"], "ru");
        let count = body["count"].as_u64().unwrap();
        assert!(count >= 170, "expected the full catalog, got {count}");
        let entries = body["entries"].as_array().unwrap();
        assert_eq!(entries.len() as u64, count);
        let first = &entries[0];
        for field in ["code", "title", "default_severity", "type", "activated_by_default"] {
            assert!(!first[field].is_null(), "entry missing `{field}`");
        }
        assert!(body.get("unknown_codes").is_none(), "no unknown codes for full catalog");
    }

    #[test]
    fn catalog_filters_to_requested_codes() {
        let result = catalog(Locale::En, &["CyclomaticComplexity".to_string()], None);
        let body = body_of(&result);
        assert_eq!(body["count"], 1);
        assert_eq!(body["entries"][0]["code"], "CyclomaticComplexity");
        assert!(body.get("unknown_codes").is_none());
    }

    #[test]
    fn catalog_reports_unknown_codes() {
        let result = catalog(
            Locale::Ru,
            &["CyclomaticComplexity".to_string(), "NoSuchCode".to_string()],
            None,
        );
        let body = body_of(&result);
        assert_eq!(body["count"], 1, "only the valid code yields an entry");
        let unknown = body["unknown_codes"].as_array().unwrap();
        assert_eq!(unknown.len(), 1);
        assert_eq!(unknown[0], "NoSuchCode");
    }

    #[test]
    fn catalog_budget_covers_unknown_codes_and_envelope() {
        let codes = (0..4_096)
            .map(|index| format!("Unknown{index}{}", "x".repeat(200)))
            .collect::<Vec<_>>();
        let result = catalog(Locale::Ru, &codes, Some(MIN_OUTPUT_TOKENS));
        let body = body_of(&result);
        assert!(serde_json::to_vec(body).unwrap().len() <= MIN_OUTPUT_TOKENS * 4);
        assert_eq!(body["budget_exhausted"], true);
    }

    mod file_action {
        use super::*;
        use crate::cancel::RequestCancel;
        use crate::diagnostics_state::{DiagnosticsState, ResidentOutcome, SweepOptions};
        use std::fs;
        use std::path::{Path, PathBuf};

        fn write(root: &Path, rel: &str, text: &str) {
            let path = root.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }

        fn write_common_module(root: &Path, name: &str, body: &str) {
            let xml = format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:v8="http://v8.1c.ru/8.1/data/core">
	<CommonModule uuid="00000000-0000-0000-0000-0000000000{id:02}">
		<Properties>
			<Name>{name}</Name>
			<Global>false</Global>
			<ClientManagedApplication>false</ClientManagedApplication>
			<Server>true</Server>
			<ExternalConnection>false</ExternalConnection>
			<ClientOrdinaryApplication>false</ClientOrdinaryApplication>
			<ServerCall>false</ServerCall>
			<Privileged>false</Privileged>
			<ReturnValuesReuse>DontUse</ReturnValuesReuse>
		</Properties>
	</CommonModule>
</MetaDataObject>"#,
                id = name.len(),
            );
            write(root, &format!("CommonModules/{name}.xml"), &xml);
            write(root, &format!("CommonModules/{name}/Ext/Module.bsl"), body);
        }

        fn sample_workspace(root: &Path) {
            write_common_module(
                root,
                "Сервер",
                "&НаСервере\nФункция Считать() Экспорт Возврат 1; КонецФункции\n",
            );
        }

        fn ready_state(root: &Path) -> DiagnosticsState {
            let state = DiagnosticsState::for_workspace(root.to_path_buf());
            state.ensure_loading();
            crate::diagnostics_state::test_support::wait_ready(&state);
            state
        }

        fn run(state: &DiagnosticsState, path: &Path, filters: &FileFilters) -> Value {
            run_rooted(state, None, path, filters)
        }

        /// The rooted form: `root_id` is the source root `path` is spelled against, exactly as
        /// a `search` code hit carries it.
        fn run_rooted(
            state: &DiagnosticsState,
            root_id: Option<&str>,
            path: &Path,
            filters: &FileFilters,
        ) -> Value {
            // `read` supplies the generation under the lock, so the closure must not
            // re-lock `&self`.
            match state.read(|resident, generation| {
                file_findings(resident, &resident.analysis(), root_id, path, filters, generation)
            }) {
                ResidentOutcome::Ready((body, _completeness), _) => body,
                _ => panic!("expected Ready outcome"),
            }
        }

        fn default_filters() -> FileFilters {
            FileFilters {
                min_severity: SeverityBucket::Hint, // keep everything for shape assertions
                codes: Vec::new(),
                range: None,
                max_findings: DEFAULT_MAX_FINDINGS,
                max_output_tokens: None,
                detailed: false,
            }
        }

        /// A form module, whose methods are addressed by the `method/file/<rel>::<name>`
        /// path-fallback id — the only id form a strip root takes part in.
        #[cfg(unix)]
        const FORM_MODULE_REL: &str = "CommonForms/Форма/Ext/Form/Module.bsl";

        /// Every finding's graph bridge, in the order the response lists them.
        #[cfg(unix)]
        fn graph_ids(body: &Value) -> Vec<&str> {
            body["findings"]
                .as_array()
                .expect("a findings array")
                .iter()
                .filter_map(|finding| finding["graph_id"].as_str())
                .collect()
        }

        /// The card `symbol_info` builds for the method a finding sits in, by the same
        /// resident. `None` when the request does not resolve or the card carries no id.
        #[cfg(unix)]
        fn card_graph_id(state: &DiagnosticsState, path: &Path, line: u32) -> Option<String> {
            match state.read(|resident, _| {
                crate::tools::symbol_info::resolve_card(
                    resident,
                    resident.db(),
                    None,
                    None,
                    path.to_str(),
                    Some(line),
                    Some(10),
                    crate::tools::symbol_info::sections_from(&[]),
                    ide::Locale::default(),
                )
            }) {
                ResidentOutcome::Ready(card, _) => {
                    card.expect("the request resolves").and_then(|card| card.graph_id)
                }
                _ => panic!("expected Ready outcome"),
            }
        }

        /// A finding is bridged against the root ITS resident was built over, not against
        /// whatever the workspace path names by the time the answer is rendered — and the card
        /// for the same method is bridged against that same root.
        ///
        /// The path half of a `method/file/<rel>::<name>` id comes from one generation's walk,
        /// so the base it is stripped against belongs to that same generation. A workspace
        /// reached through a link that is retargeted while the resident still serves would
        /// otherwise have its old paths stripped against the new tree: no rel is found, and
        /// every path-keyed id quietly stops being minted — a form handler addressable or not
        /// depending on when the request happened to arrive.
        ///
        /// Both tools are asked because a base each of them read for itself would let them
        /// answer differently about one method, and two tools disagreeing about a symbol they
        /// both resolved is worse than neither of them naming it.
        #[cfg(unix)]
        #[test]
        fn a_finding_is_bridged_against_the_root_its_resident_was_built_over() {
            let dir = tempfile::tempdir().unwrap();
            let served = dir.path().join("served");
            let elsewhere = dir.path().join("elsewhere");
            fs::create_dir_all(&elsewhere).unwrap();
            write(&served, FORM_MODULE_REL, "Процедура ПриОткрытии()\n\t;\nКонецПроцедуры\n");
            let workspace = dir.path().join("workspace");
            std::os::unix::fs::symlink(&served, &workspace).unwrap();

            let state = ready_state(&workspace);
            // The walk's own spelling of the module — the one the resident holds it under, and
            // the one a hit or a card hands back.
            let module = served.canonicalize().unwrap().join(FORM_MODULE_REL);
            let expected = format!("method/file/{FORM_MODULE_REL}::ПриОткрытии");

            let served_body = run(&state, &module, &default_filters());
            let before = graph_ids(&served_body);
            assert!(
                !before.is_empty() && before.iter().all(|id| *id == expected),
                "control: this module's findings carry the path-fallback id, got {before:?}",
            );

            let card_before = card_graph_id(&state, &module, 0);
            assert_eq!(
                card_before.as_deref(),
                Some(expected.as_str()),
                "control: the card names the method the findings sit in the same way",
            );

            fs::remove_file(&workspace).unwrap();
            std::os::unix::fs::symlink(&elsewhere, &workspace).unwrap();

            let retargeted_body = run(&state, &module, &default_filters());
            let after = graph_ids(&retargeted_body);
            assert_eq!(
                after, before,
                "the resident still serves the files it walked, so it still names them the same",
            );
            assert_eq!(
                card_graph_id(&state, &module, 0),
                card_before,
                "and the card cannot start naming it otherwise while the finding does not",
            );
        }

        #[test]
        fn diagnostics_partitioned_baseline_response_covers_schema_errors_and_minimum_budget() {
            let schema = Value::Object(
                rmcp::handler::server::tool::schema_for_type::<DiagnosticsResponseSchema>()
                    .as_ref()
                    .clone(),
            );
            assert_eq!(schema["type"], "object");
            assert!(schema["anyOf"].as_array().is_some_and(|branches| branches.len() >= 2));
            let encoded = schema.to_string();
            for required in [
                "baseline",
                "state",
                "complete",
                "error_code",
                "detail",
                "partitions_total",
                "partitions_returned",
                "partitions_truncated",
                "errors_total",
                "errors",
            ] {
                assert!(encoded.contains(&format!("\"{required}\"")), "missing {required}");
            }

            let disabled_dir = tempfile::tempdir().unwrap();
            sample_workspace(disabled_dir.path());
            let disabled_state = ready_state(disabled_dir.path());
            let mut filters = default_filters();
            filters.max_output_tokens = Some(1);
            let disabled = run(
                &disabled_state,
                &disabled_dir.path().join("CommonModules/Сервер/Ext/Module.bsl"),
                &filters,
            );
            assert_eq!(disabled["baseline"]["state"], "disabled");
            assert_eq!(disabled["baseline"]["partitions_total"], 0);
            assert_eq!(disabled["baseline"]["partitions_returned"], 0);
            assert_eq!(disabled["baseline"]["partitions_truncated"], false);
            let response = envelope(
                Freshness { revision: 1, topology: 0, stale: false, reload: "none" },
                loc::Completeness::complete(),
                disabled,
            );
            assert_structured_mirrors_text(&response);
            assert!(body_of(&response)["result"]["baseline"].is_object());

            let error_dir = tempfile::tempdir().unwrap();
            sample_workspace(error_dir.path());
            write(
                error_dir.path(),
                "bsl-analyzer.toml",
                "[diagnostics.baseline]\npath = \"baseline.json\"\n",
            );
            write(error_dir.path(), "baseline.json", "{broken");
            let error_state = ready_state(error_dir.path());
            let error = run(
                &error_state,
                &error_dir.path().join("CommonModules/Сервер/Ext/Module.bsl"),
                &default_filters(),
            );
            assert_eq!(error["baseline"]["state"], "error");
            assert!(error["baseline"]["error_code"].is_string());
            assert!(error["baseline"]["detail"].is_string());
            assert_eq!(error["baseline"]["errors_total"], 1);
            assert!(error.get("findings").is_none());
            assert!(error.get("counts").is_none());

            let partition =
                |id: &str| ide::diagnostics_baseline::DiagnosticsBaselinePartitionSummary {
                    id: id.to_owned(),
                    identity: project_model::DiagnosticsBaselinePartitionIdentity::Main {
                        path: String::new(),
                    },
                    policy: project_model::DiagnosticsBaselinePartitionPolicy::Baseline,
                    path: Some(format!("objects/{id}.json")),
                    schema_version: Some(2),
                    state: ide::diagnostics_baseline::DiagnosticsBaselineState::Full,
                    new: 0,
                    known: 0,
                    resolved: 0,
                    unsuppressed: 0,
                    complete: true,
                };
            let mut summary = DiagnosticsBaselineSummary::disabled();
            summary.state = ide::diagnostics_baseline::DiagnosticsBaselineState::Full;
            summary.partitions = vec![partition("main"), partition("extension:z")];
            let mut minimum =
                json!({"baseline": bounded_baseline_value(&summary, Some("extension:z"))});
            let _ = fit_diagnostics_response_budget(&mut minimum, Some(1), "findings");
            let minimum = &minimum["baseline"];
            assert_eq!(minimum["partitions_total"], 2);
            assert_eq!(minimum["partitions_returned"], 0);
            assert_eq!(minimum["partitions_truncated"], true);
            let owner_first = bounded_baseline_value(&summary, Some("extension:z"));
            assert_eq!(owner_first["partitions"][0]["id"], "extension:z");
            let stale = envelope(
                Freshness { revision: 2, topology: 0, stale: true, reload: "running" },
                loc::Completeness::complete(),
                json!({"baseline": owner_first.clone()}),
            );
            let stale = &body_of(&stale)["result"]["baseline"];
            assert!(stale["partitions"].as_array().unwrap().iter().all(|partition| {
                partition["state"] == "partial"
                    && partition["complete"] == false
                    && partition["resolved"] == 0
            }));

            summary.state = ide::diagnostics_baseline::DiagnosticsBaselineState::Error;
            summary.errors = ["extension:a", "extension:b"]
                .into_iter()
                .map(|id| ide::diagnostics_baseline::DiagnosticsBaselineErrorSummary {
                    partition_id: Some(id.to_owned()),
                    code: "missing_partition".to_owned(),
                    detail: format!("missing partition: {id}"),
                    epoch: id.to_owned(),
                })
                .collect();
            let errors = bounded_baseline_value(&summary, None);
            assert_eq!(errors["errors_total"], 2);
            assert_eq!(errors["errors"].as_array().unwrap().len(), 2);

            let mut whole = json!({
                "result_id": "x",
                "truncated": false,
                "findings": (0..20).map(|index| json!({"message": "x".repeat(80), "index": index})).collect::<Vec<_>>(),
                "baseline": owner_first,
            });
            let _ = fit_diagnostics_response_budget(&mut whole, Some(100), "findings");
            assert!(serde_json::to_vec(&whole).unwrap().len() <= 400);
            assert_eq!(whole["baseline"]["partitions_total"], 2);
            assert_eq!(whole["baseline"]["partitions_truncated"], true);

            let first_partition = "p".repeat(64);
            let early_error = budgeted_diagnostics_response(
                json!({
                    "error": "invalid_path",
                    "kind": "full",
                    "result_id": "1@".to_owned() + &"a".repeat(64) + "@" + &"b".repeat(64),
                    "detail": "d".repeat(2_000),
                    "path": "p".repeat(2_000),
                    "baseline": {
                        "state": "error",
                        "complete": false,
                        "path": "baselines",
                        "error_code": "missing_partition",
                        "detail": "set error".repeat(1_000),
                        "partitions": [],
                        "partitions_total": 0,
                        "partitions_returned": 0,
                        "partitions_truncated": false,
                        "errors_total": 2,
                        "errors": [
                            {"code": "missing_partition", "detail": "first".repeat(1_000), "partition_id": first_partition, "epoch": "c".repeat(64)},
                            {"code": "orphan_partition", "detail": "second", "epoch": "d".repeat(64)}
                        ]
                    }
                }),
                Some(MIN_OUTPUT_TOKENS),
            );
            assert!(serde_json::to_vec(&early_error).unwrap().len() <= MIN_OUTPUT_TOKENS * 4);
            assert_eq!(early_error["baseline"]["errors"].as_array().unwrap().len(), 1);
            assert_eq!(early_error["baseline"]["errors"][0]["code"], "missing_partition");
            assert_eq!(early_error["baseline"]["errors"][0]["partition_id"], first_partition);
        }

        #[test]
        fn file_findings_shapes_the_result() {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            sample_workspace(root);
            let state = ready_state(root);
            let path = root.join("CommonModules/Сервер/Ext/Module.bsl");

            let body = run(&state, &path, &default_filters());

            assert_eq!(body["kind"], "full");
            assert!(body["truncated"].is_boolean());
            for sev in ["error", "warning", "info", "hint"] {
                assert!(body["counts"][sev].is_u64(), "counts.{sev} present");
            }
            assert!(body["findings"].is_array());
            // result_id = <generation>@<content-hash>@<baseline-epoch>.
            let id = body["result_id"].as_str().unwrap();
            let parts: Vec<&str> = id.rsplitn(3, '@').collect();
            assert_eq!(parts.len(), 3, "result_id carries the baseline epoch: {id}");
            assert_eq!(parts[0], "disabled");
            assert_eq!(parts[1].len(), 16, "content hash is 16 hex chars");
        }

        #[test]
        fn unknown_path_is_an_in_band_error() {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            sample_workspace(root);
            let state = ready_state(root);

            let body = run(&state, &PathBuf::from("/nope/Missing.bsl"), &default_filters());
            assert_eq!(body["error"], "not_in_workspace");
        }

        /// A hit's path is spelled against the root that owns it, so the pair is what names
        /// the file. Answered without the root, this request lands on the configuration's
        /// module of the same name — a wrong answer in the shape of a right one, which is
        /// why the stand gives the two modules different text: identical bytes would make
        /// the two answers indistinguishable however wrong the reading.
        #[test]
        fn a_rooted_path_is_answered_from_its_own_root() {
            use crate::diagnostics_state::test_support::{
                extension_root_id, workspace_with_an_outside_extension, SHARED_MODULE_REL,
            };
            let (_dir, workspace, extension) = workspace_with_an_outside_extension();
            let state = ready_state(&workspace);
            let root_id = extension_root_id(&workspace, &extension);

            let rooted = run_rooted(
                &state,
                Some(&root_id),
                Path::new(SHARED_MODULE_REL),
                &default_filters(),
            );
            let from_the_extension =
                run(&state, &extension.join(SHARED_MODULE_REL), &default_filters());
            let from_the_configuration =
                run(&state, &workspace.join(SHARED_MODULE_REL), &default_filters());

            assert_eq!(
                rooted["result_id"], from_the_extension["result_id"],
                "the pair names the extension's module: {rooted}",
            );
            assert_ne!(
                rooted["result_id"], from_the_configuration["result_id"],
                "and not the configuration's module of the same name: {rooted}",
            );
        }

        /// The reading that makes an extension's path work is the same reading a nested
        /// configuration needs: its hits are spelled against the configuration root, while a
        /// bare relative path is read against the project root. The two differ exactly when
        /// the configuration sits in a subdirectory, and no extension is involved.
        #[test]
        fn the_configuration_root_id_reads_a_path_against_the_configuration() {
            use crate::diagnostics_state::test_support::{
                workspace_with_a_nested_configuration, SHARED_MODULE_REL,
            };
            let (_dir, workspace) = workspace_with_a_nested_configuration();
            let state = ready_state(&workspace);

            let rooted =
                run_rooted(&state, Some(""), Path::new(SHARED_MODULE_REL), &default_filters());
            assert!(
                rooted["error"].is_null(),
                "the empty root id names the configuration, wherever it sits: {rooted}",
            );
            let direct = run(
                &state,
                &workspace.join("src").join("cf").join(SHARED_MODULE_REL),
                &default_filters(),
            );
            assert_eq!(rooted["result_id"], direct["result_id"], "and names that file: {rooted}");
        }

        /// An unregistered root is the caller's error, and saying so is the whole point:
        /// falling back to the old reading would answer from the configuration's namesake
        /// and call it the extension's file.
        #[test]
        fn an_unregistered_root_is_refused_rather_than_read_as_the_configuration() {
            use crate::diagnostics_state::test_support::{
                workspace_with_an_outside_extension, SHARED_MODULE_REL,
            };
            let (_dir, workspace, _extension) = workspace_with_an_outside_extension();
            let state = ready_state(&workspace);

            let body = run_rooted(
                &state,
                Some("нет-такого-корня"),
                Path::new(SHARED_MODULE_REL),
                &default_filters(),
            );
            assert_eq!(body["error"], "unknown_root", "{body}");
            assert!(
                body["detail"].as_str().unwrap_or_default().contains("нет-такого-корня"),
                "the refusal names the root it could not find: {body}",
            );
        }

        /// Resolution has to reach every reader of the request path, not just the one that
        /// picks the FileId. This asks the branch NEXT to it: a file held out of service
        /// answers `unreadable`, and that answer comes from a separate lookup. Resolve only
        /// under `file_id_for` and this request reports the CONFIGURATION's readable module
        /// as "not in workspace" instead.
        #[cfg(unix)]
        #[test]
        fn resolution_reaches_the_unreadable_branch_too() {
            use crate::diagnostics_state::test_support::{
                extension_root_id, workspace_with_an_outside_extension, SHARED_MODULE_REL,
            };
            use std::os::unix::fs::PermissionsExt;

            let (_dir, workspace, extension) = workspace_with_an_outside_extension();
            let root_id = extension_root_id(&workspace, &extension);
            let closed = extension.join(SHARED_MODULE_REL);
            std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o000)).unwrap();
            if std::fs::read(&closed).is_ok() {
                std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o644)).unwrap();
                return; // running as root: the mode says nothing about readability
            }
            let state = ready_state(&workspace);
            let body = run_rooted(
                &state,
                Some(&root_id),
                Path::new(SHARED_MODULE_REL),
                &default_filters(),
            );
            std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o644)).unwrap();

            assert_eq!(body["error"], "unreadable", "{body}");
        }

        /// A module whose third line puts a NON-BMP character before an unresolved call, so
        /// the finding's column differs in every candidate unit: bytes, code points, UTF-16.
        const MIXED_MODULE: &str =
            "&НаСервере\nПроцедура Тест() Экспорт\n\tСообщить(\"\u{1D54F}\"); НетТакогоМодуля.Метод();\nКонецПроцедуры\n";

        fn finding_with_code<'a>(body: &'a Value, code: &str) -> &'a Value {
            body["findings"]
                .as_array()
                .expect("findings")
                .iter()
                .find(|f| f["code"] == code)
                .unwrap_or_else(|| panic!("no {code} in {body}"))
        }

        /// The unit the contract publishes is UTF-16 code units, and the legacy `range`
        /// keeps counting code points. Asserted together on ONE finding: an implementation
        /// that copies the old encoding into the new field makes the two equal and fails
        /// here — which is the whole reason this step exists.
        #[test]
        fn a_location_range_counts_utf16_where_the_legacy_range_counts_code_points() {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            sample_workspace(root);
            write_common_module(root, "Проба", MIXED_MODULE);
            let state = ready_state(root);
            let path = root.join("CommonModules/Проба/Ext/Module.bsl");

            let body = run(&state, &path, &default_filters());
            let finding = finding_with_code(&body, "UnresolvedMethodCall");

            assert_eq!(finding["range"]["start_column"], 16, "legacy column counts code points");
            assert_eq!(
                finding["location"]["range"]["start_character"], 17,
                "the contract column counts UTF-16 code units: {finding}",
            );
            assert_ne!(
                finding["range"]["start_column"], finding["location"]["range"]["start_character"],
                "the two units must not coincide on this input, or the test proves nothing",
            );
            assert_eq!(finding["location"]["position_encoding"], "utf-16");
            // The enclosing method, not just the finding's own span. It starts at line 0
            // because the compilation directive is part of the method node, and ends past
            // the finding's line — so it is the method's span and not the finding's again.
            let enclosing = &finding["location"]["enclosing_range"];
            assert_eq!(enclosing["start_line"], 0, "{finding}");
            assert_eq!(enclosing["end_line"], 3, "{finding}");
            assert!(
                enclosing["end_line"].as_u64()
                    > finding["location"]["range"]["start_line"].as_u64(),
                "the enclosing span must cover the finding: {finding}",
            );
        }

        /// One relative path, two roots: the location must name the root the file actually
        /// came from. The stand gives both modules the same relative path on purpose.
        #[test]
        fn a_location_names_the_root_the_file_came_from() {
            use crate::diagnostics_state::test_support::{
                extension_root_id, workspace_with_an_outside_extension,
            };
            let (_dir, workspace, extension) = workspace_with_an_outside_extension();
            write_common_module(&workspace, "Проба", MIXED_MODULE);
            write_common_module(&extension, "Проба", MIXED_MODULE);
            let state = ready_state(&workspace);
            let root_id = extension_root_id(&workspace, &extension);
            let rel = Path::new("CommonModules/Проба/Ext/Module.bsl");

            let from_extension = run_rooted(&state, Some(&root_id), rel, &default_filters());
            let from_configuration = run_rooted(&state, Some(""), rel, &default_filters());

            let ext_location =
                &finding_with_code(&from_extension, "UnresolvedMethodCall")["location"];
            let cfg_location =
                &finding_with_code(&from_configuration, "UnresolvedMethodCall")["location"];

            assert_eq!(ext_location["root_id"], root_id.as_str(), "{ext_location}");
            assert_eq!(cfg_location["root_id"], "", "{cfg_location}");
            assert_eq!(
                ext_location["path"], cfg_location["path"],
                "the relative path is the same in both roots — the root_id is what separates them",
            );
        }

        /// A link inside one root may point at a file that physically lives under another.
        /// The request named a root and was served under it, so the published pair must be
        /// that one: canonicalizing the resolved path instead renames the answer into the
        /// target's root, and a consumer feeding the pair back reaches a different copy.
        #[cfg(unix)]
        #[test]
        fn a_location_echoes_the_root_the_request_named() {
            use crate::diagnostics_state::test_support::{
                extension_root_id, workspace_with_an_outside_extension,
            };
            let (_dir, workspace, extension) = workspace_with_an_outside_extension();
            write_common_module(&workspace, "Проба", MIXED_MODULE);
            // A file OF the extension root whose bytes live under the configuration root.
            std::os::unix::fs::symlink(
                workspace.join("CommonModules/Проба/Ext/Module.bsl"),
                extension.join("Alias.bsl"),
            )
            .expect("the stand needs a link inside the extension root");

            let state = ready_state(&workspace);
            let root_id = extension_root_id(&workspace, &extension);

            let body =
                run_rooted(&state, Some(&root_id), Path::new("Alias.bsl"), &default_filters());
            let location = &finding_with_code(&body, "UnresolvedMethodCall")["location"];

            assert_eq!(location["root_id"], root_id.as_str(), "{location}");
            assert_eq!(location["path"], "Alias.bsl", "{location}");
        }

        /// A hole elsewhere in the workspace must not make an answer ABOUT ANOTHER FILE
        /// partial. The envelope describes this answer; a consumer taught that "partial"
        /// means "some unrelated module is unreadable" learns to ignore the field.
        #[test]
        fn an_unrelated_hole_does_not_make_a_clean_file_partial() {
            use std::io::Write;

            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            sample_workspace(root);
            write_common_module(root, "Проба", MIXED_MODULE);
            // A module whose bytes are not valid UTF-8 is held out of service.
            let broken = root.join("CommonModules/Битый/Ext/Module.bsl");
            write_common_module(
                root,
                "Битый",
                "&НаСервере\nПроцедура П() Экспорт КонецПроцедуры\n",
            );
            let mut f = std::fs::File::create(&broken).unwrap();
            f.write_all(&[0xFF, 0xFE, 0xFF]).unwrap();
            drop(f);

            let state = ready_state(root);
            let clean = root.join("CommonModules/Проба/Ext/Module.bsl");

            let (unread, completeness) = match state.read(|resident, generation| {
                (
                    resident.unread_count(),
                    file_findings(
                        resident,
                        &resident.analysis(),
                        None,
                        &clean,
                        &default_filters(),
                        generation,
                    )
                    .1,
                )
            }) {
                ResidentOutcome::Ready(pair, _) => pair,
                _ => panic!("expected Ready outcome"),
            };

            assert_eq!(unread, 1, "the stand must actually hold a hole, or it proves nothing");
            assert!(
                !has_reason(&completeness.to_value(), "unreadable_files"),
                "the answer is about a readable file: {}",
                completeness.to_value(),
            );
        }

        /// The envelope must be able to SAY the answer was cut, and to stop saying it when
        /// it was not: a pair of calls, one budget each.
        #[test]
        fn completeness_reports_a_budget_cut_and_only_then() {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            sample_workspace(root);
            write_common_module(root, "Проба", MIXED_MODULE);
            let state = ready_state(root);
            let path = root.join("CommonModules/Проба/Ext/Module.bsl");

            let tight = completeness_of(&state, &path, Some(1));
            let ample = completeness_of(&state, &path, Some(100_000));

            assert!(
                has_reason(&tight, "output_budget"),
                "a one-token budget cannot fit these findings: {tight}",
            );
            assert!(
                !has_reason(&ample, "output_budget"),
                "and an ample budget must not claim it did: {ample}",
            );
        }

        fn completeness_of(
            state: &DiagnosticsState,
            path: &Path,
            max_output_tokens: Option<usize>,
        ) -> Value {
            let filters = FileFilters { max_output_tokens, ..default_filters() };
            match state.read(|resident, generation| {
                file_findings(resident, &resident.analysis(), None, path, &filters, generation)
            }) {
                ResidentOutcome::Ready((_body, completeness), _) => completeness.to_value(),
                _ => panic!("expected Ready outcome"),
            }
        }

        fn has_reason(completeness: &Value, code: &str) -> bool {
            completeness["reasons"].as_array().expect("reasons").iter().any(|r| r["code"] == code)
        }

        #[test]
        fn codes_filter_narrows_findings() {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            sample_workspace(root);
            let state = ready_state(root);
            let path = root.join("CommonModules/Сервер/Ext/Module.bsl");

            // A code that cannot match drops every finding while counts stay structural.
            let filters =
                FileFilters { codes: vec!["NoSuchCodeFilter".to_string()], ..default_filters() };
            let body = run(&state, &path, &filters);
            assert_eq!(body["findings"].as_array().unwrap().len(), 0);
        }

        #[test]
        fn findings_carry_standards_only_when_a_standard_backs_the_rule() {
            use std::str::FromStr;

            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            sample_workspace(root);
            // Модуль стенда даёт только диагностики со стандартом, поэтому
            // ветка «поля нет» осталась бы непроверенной. Функция без Возврата
            // на одном из путей добавляет диагностику без нормативного
            // источника — второй класс, без которого проверка холостая.
            write_common_module(
                root,
                "БезСтандарта",
                "&НаСервере\nФункция Проверить() Экспорт\n\tЕсли Истина Тогда\n\t\tВозврат 1;\n\tКонецЕсли;\nКонецФункции\n",
            );
            let state = ready_state(root);
            let path = root.join("CommonModules/БезСтандарта/Ext/Module.bsl");

            let body = run(&state, &path, &default_filters());
            let findings = body["findings"].as_array().expect("findings");

            let (mut backed, mut bare) = (0, 0);
            for finding in findings {
                let code = finding["code"].as_str().expect("code");
                let code = ide::DiagnosticCode::from_str(code).expect("known code");
                let expected = ide::standards(code);
                assert!(
                    !finding["message"].as_str().expect("message").contains("v8std.ru"),
                    "суффикс просочился в MCP: агенту тот же факт достаётся структурой"
                );
                match expected.first() {
                    None => {
                        // Отсутствие ключа, а не пустой массив: иначе
                        // реализация «всегда пустой список» проходит.
                        assert!(finding.get("standards").is_none(), "ссылка выдумана у {code:?}");
                        bare += 1;
                    }
                    Some(primary) => {
                        let listed = finding["standards"].as_array().expect("standards");
                        assert_eq!(listed.len(), expected.len());
                        assert_eq!(listed[0]["id"], *primary);
                        assert_eq!(listed[0]["url"], ide::standard_url(*primary));
                        backed += 1;
                    }
                }
            }
            assert!(
                backed > 0 && bare > 0,
                "стенд дал {backed} со стандартом и {bare} без — проверка холостая, \
                 нужен вход с обоими классами"
            );
        }

        fn run_git(dir: &Path, args: &[&str]) {
            let mut cmd = std::process::Command::new("git");
            // Inherited GIT_* variables (e.g. GIT_AUTHOR_NAME when the test
            // suite runs inside a `git commit` pre-commit hook) override the
            // `-c` identity below and would reattribute the fixture commits.
            for (key, _) in std::env::vars_os() {
                if key.to_string_lossy().starts_with("GIT_") {
                    cmd.env_remove(&key);
                }
            }
            let output = cmd
                .arg("-C")
                .arg(dir)
                .args(["-c", "user.name=Фирма Тест", "-c", "user.email=vendor@example.com"])
                .args(args)
                .output()
                .expect("run git");
            assert!(
                output.status.success(),
                "git {:?} failed:\n{}",
                args,
                String::from_utf8_lossy(&output.stderr)
            );
        }

        #[test]
        fn author_filter_suppresses_vendor_findings_in_file_and_sweep() {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            // An unclosed procedure reliably yields findings on the vendor lines.
            write_common_module(root, "Сервер", "Процедура Тест(\n");
            write(root, "bsl-analyzer.toml", "[analysis]\nignored_authors = [\"Фирма Тест\"]\n");
            run_git(root, &["init", "-q"]);
            run_git(root, &["add", "."]);
            run_git(root, &["commit", "-q", "-m", "vendor"]);

            let state = ready_state(root);
            let path = root.join("CommonModules/Сервер/Ext/Module.bsl");

            let body = run(&state, &path, &default_filters());
            assert_eq!(
                body["findings"].as_array().unwrap().len(),
                0,
                "vendor-authored findings must be suppressed: {body}"
            );
            let ignored = body["findings_ignored_by_author"].as_u64().unwrap_or(0);
            assert!(ignored > 0, "suppressed findings must be counted: {body}");
            assert!(body["author_hint"].is_string());
            // counts reflect the post-filter view.
            for sev in ["error", "warning", "info", "hint"] {
                assert_eq!(body["counts"][sev], 0, "counts.{sev} must be filtered: {body}");
            }
            // result_id = <gen>@<hash>@<blame-head>@<baseline-epoch>.
            let id = body["result_id"].as_str().unwrap();
            assert_eq!(id.rsplitn(4, '@').count(), 4, "result_id carries both epochs: {id}");

            let sweep = match state.read_mut(|resident, _| {
                resident.workspace_aggregates(
                    &resident.config().clone(),
                    &SweepOptions {
                        min_severity: SeverityBucket::Hint,
                        codes: Vec::new(),
                        max_files: 100,
                    },
                    &RequestCancel::default(),
                )
            }) {
                ResidentOutcome::Ready(sweep, _) => sweep,
                _ => panic!("expected Ready outcome"),
            };
            assert!(
                sweep.findings_ignored_by_author > 0,
                "the sweep must count author-suppressed findings"
            );
            assert!(sweep.aggregates.is_empty(), "nothing should aggregate after the filter");
            assert!(sweep.author_head.is_some());
        }

        /// The project's diagnostics settings reach the findings end-to-end, not just
        /// the resident's stored config: the same module yields a LineLength finding
        /// under a tiny threshold and none under a huge one.
        #[test]
        fn file_findings_honor_project_threshold() {
            let body = "Функция Ф() Экспорт\n\tВозврат 1;\nКонецФункции\n";
            let has_line_length = |v: &Value| {
                v["findings"].as_array().unwrap().iter().any(|f| f["code"] == "LineLength")
            };

            // A tiny maxLineLength fires LineLength on ordinary lines.
            let tight = tempfile::tempdir().unwrap();
            write_common_module(tight.path(), "Модуль", body);
            write(
                tight.path(),
                "bsl-analyzer.toml",
                "[diagnostics.parameters.LineLength]\nmaxLineLength = 5\n",
            );
            let tight_state = ready_state(tight.path());
            let tight_body = run(
                &tight_state,
                &tight.path().join("CommonModules/Модуль/Ext/Module.bsl"),
                &default_filters(),
            );
            assert!(has_line_length(&tight_body), "tiny threshold fires LineLength");

            // A huge maxLineLength suppresses it for the same module.
            let loose = tempfile::tempdir().unwrap();
            write_common_module(loose.path(), "Модуль", body);
            write(
                loose.path(),
                "bsl-analyzer.toml",
                "[diagnostics.parameters.LineLength]\nmaxLineLength = 5000\n",
            );
            let loose_state = ready_state(loose.path());
            let loose_body = run(
                &loose_state,
                &loose.path().join("CommonModules/Модуль/Ext/Module.bsl"),
                &default_filters(),
            );
            assert!(!has_line_length(&loose_body), "huge threshold suppresses LineLength");
        }

        #[test]
        fn diagnostics_baseline_snapshot_filters_file_and_workspace_without_rebuilding_salsa() {
            use ide::diagnostics_baseline::{
                diagnostic_fingerprint, diagnostics_baseline_json, DiagnosticsBaseline,
                DiagnosticsBaselineEntry, DiagnosticsBaselineRange, DiagnosticsBaselineScope,
                DIAGNOSTICS_BASELINE_SCHEMA_VERSION,
            };

            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            sample_workspace(root);
            write(root, "bsl-analyzer.toml", "[diagnostics.baseline]\npath = \"baseline.json\"\n");
            let baseline_path = root.join("baseline.json");
            let mut baseline = DiagnosticsBaseline {
                schema_version: DIAGNOSTICS_BASELINE_SCHEMA_VERSION,
                scope: DiagnosticsBaselineScope { source_root: None, extensions: vec![] },
                diagnostics: vec![],
            };
            fs::write(&baseline_path, diagnostics_baseline_json(&baseline).unwrap()).unwrap();

            let state = ready_state(root);
            let path = root.join("CommonModules/Сервер/Ext/Module.bsl");
            let filters = FileFilters { detailed: true, ..default_filters() };
            let first = run(&state, &path, &filters);
            let finding = first["findings"].as_array().unwrap().first().unwrap();
            let relative = "CommonModules/Сервер/Ext/Module.bsl";
            let line = finding["range"]["start_line"].as_u64().unwrap() as usize;
            let text = fs::read_to_string(&path).unwrap();
            let snippet = ide::diagnostics_baseline::normalize_diagnostic_snippet(
                text.lines().nth(line).unwrap(),
            );
            let code = finding["code"].as_str().unwrap().to_owned();
            baseline.diagnostics.push(DiagnosticsBaselineEntry {
                fingerprint: diagnostic_fingerprint(relative, &code, &snippet, 0),
                path: relative.to_owned(),
                code,
                snippet,
                occurrence: 0,
                message: finding["message"].as_str().unwrap().to_owned(),
                severity: finding["internal_severity"].as_str().unwrap().to_owned(),
                range: DiagnosticsBaselineRange {
                    start_line: finding["range"]["start_line"].as_u64().unwrap() as u32,
                    start_column: finding["range"]["start_column"].as_u64().unwrap() as u32,
                    end_line: finding["range"]["end_line"].as_u64().unwrap() as u32,
                    end_column: finding["range"]["end_column"].as_u64().unwrap() as u32,
                },
            });
            let generation = state.generation();
            fs::write(&baseline_path, diagnostics_baseline_json(&baseline).unwrap()).unwrap();

            let second = run(&state, &path, &filters);
            assert_eq!(second["baseline"]["known"], 1);
            assert_eq!(
                second["findings"].as_array().unwrap().len() + 1,
                first["findings"].as_array().unwrap().len()
            );
            assert_eq!(state.generation(), generation, "baseline reload must not rebuild Salsa");
            assert_ne!(second["result_id"], first["result_id"]);

            let sweep = match state.read_mut(|resident, _| {
                resident.workspace_aggregates(
                    &resident.config().clone(),
                    &SweepOptions {
                        min_severity: SeverityBucket::Hint,
                        codes: Vec::new(),
                        max_files: 100,
                    },
                    &crate::cancel::RequestCancel::default(),
                )
            }) {
                ResidentOutcome::Ready(sweep, _) => sweep,
                _ => panic!("expected Ready outcome"),
            };
            assert_eq!(sweep.baseline.known, Some(1));
            assert!(sweep.baseline.complete);
        }

        #[test]
        fn diagnostics_baseline_file_in_active_diff_scope_resolves_nothing() {
            use ide::diagnostics_baseline::{
                diagnostic_fingerprint, diagnostics_baseline_json, DiagnosticsBaseline,
                DiagnosticsBaselineEntry, DiagnosticsBaselineRange, DiagnosticsBaselineScope,
                DIAGNOSTICS_BASELINE_SCHEMA_VERSION,
            };

            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            sample_workspace(root);
            write(
                root,
                "bsl-analyzer.toml",
                "[analysis]\ndiff_base = \"HEAD\"\n[diagnostics.baseline]\npath = \"baseline.json\"\n",
            );
            let relative = "CommonModules/Сервер/Ext/Module.bsl";
            let baseline = DiagnosticsBaseline {
                schema_version: DIAGNOSTICS_BASELINE_SCHEMA_VERSION,
                scope: DiagnosticsBaselineScope { source_root: None, extensions: vec![] },
                diagnostics: vec![DiagnosticsBaselineEntry {
                    fingerprint: diagnostic_fingerprint(relative, "UnreachableCode", "Возврат;", 0),
                    path: relative.to_owned(),
                    code: "UnreachableCode".to_owned(),
                    snippet: "Возврат;".to_owned(),
                    occurrence: 0,
                    message: "known outside diff".to_owned(),
                    severity: "warning".to_owned(),
                    range: DiagnosticsBaselineRange {
                        start_line: 0,
                        start_column: 0,
                        end_line: 0,
                        end_column: 8,
                    },
                }],
            };
            fs::write(root.join("baseline.json"), diagnostics_baseline_json(&baseline).unwrap())
                .unwrap();
            run_git(root, &["init", "-q"]);
            run_git(root, &["add", "."]);
            run_git(root, &["commit", "-q", "-m", "baseline"]);
            fs::write(root.join(relative), "Процедура Изменена()\nКонецПроцедуры\n").unwrap();

            let state = ready_state(root);
            let body = run(&state, &root.join(relative), &default_filters());
            assert_ne!(body["out_of_scope"], true);
            assert_eq!(body["baseline"]["state"], "partial");
            assert_eq!(body["baseline"]["resolved"], 0);
        }

        #[test]
        fn diagnostics_baseline_reload_observes_write_replace_and_delete_without_rebuilding_salsa()
        {
            use ide::diagnostics_baseline::{
                diagnostics_baseline_json, DiagnosticsBaseline, DiagnosticsBaselineScope,
                DIAGNOSTICS_BASELINE_SCHEMA_VERSION,
            };
            use std::io::Write;

            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            sample_workspace(root);
            write(root, "bsl-analyzer.toml", "[diagnostics.baseline]\npath = \"baseline.json\"\n");
            let baseline_path = root.join("baseline.json");
            let baseline = DiagnosticsBaseline {
                schema_version: DIAGNOSTICS_BASELINE_SCHEMA_VERSION,
                scope: DiagnosticsBaselineScope { source_root: None, extensions: vec![] },
                diagnostics: vec![],
            };
            let bytes = diagnostics_baseline_json(&baseline).unwrap();
            fs::write(&baseline_path, &bytes).unwrap();

            let state = ready_state(root);
            let path = root.join("CommonModules/Сервер/Ext/Module.bsl");
            let generation = state.generation();
            let first = run(&state, &path, &default_filters());

            let mut rewritten = bytes.clone();
            rewritten.push(b'\n');
            fs::write(&baseline_path, &rewritten).unwrap();
            let changed = run(&state, &path, &default_filters());
            assert_ne!(changed["result_id"], first["result_id"]);
            assert_eq!(state.generation(), generation);

            let mut replacement = tempfile::NamedTempFile::new_in(root).unwrap();
            replacement.write_all(b" \n").unwrap();
            replacement.write_all(&bytes).unwrap();
            replacement.flush().unwrap();
            replacement.as_file().sync_all().unwrap();
            replacement.persist(&baseline_path).unwrap();
            let replaced = run(&state, &path, &default_filters());
            assert_ne!(replaced["result_id"], changed["result_id"]);
            assert_eq!(state.generation(), generation);

            fs::remove_file(&baseline_path).unwrap();
            let removed = run(&state, &path, &default_filters());
            assert_eq!(removed["baseline"]["state"], "error");
            assert_eq!(removed["baseline"]["error_code"], "missing");
            assert_ne!(removed["result_id"], replaced["result_id"]);
            assert_eq!(state.generation(), generation);
        }

        /// An unreadable baseline leaves the sweep with nothing to report, and the
        /// envelope must say so — a consumer reading only the envelope would otherwise
        /// take the empty answer for a clean configuration.
        #[test]
        fn workspace_envelope_reports_a_broken_baseline() {
            use crate::diagnostics_state::CodeAggregate;
            let summary = |state| ide::diagnostics_baseline::DiagnosticsBaselineSummary {
                state,
                complete: false,
                error_code: Some("invalid_file".to_owned()),
                ..ide::diagnostics_baseline::DiagnosticsBaselineSummary::disabled()
            };
            let sweep = |state| WorkspaceSweep {
                aggregates: vec![CodeAggregate {
                    code: "LineLength".to_owned(),
                    severity: ide::SeverityBucket::Warning,
                    count: 1,
                    files_affected: 1,
                }],
                files_swept: 1,
                files_total: 1,
                files_out_of_scope: 0,
                files_unread: 0,
                findings_ignored_by_author: 0,
                author_head: None,
                truncated: false,
                cancelled: false,
                baseline: summary(state),
                baseline_epoch: "e".to_owned(),
                lacks_owning_configuration: false,
            };

            let (_, healthy) = workspace_findings(
                &sweep(ide::diagnostics_baseline::DiagnosticsBaselineState::Full),
                1,
                None,
            );
            assert_eq!(
                healthy.to_value()["status"],
                "complete",
                "positive control: an intact baseline keeps the envelope complete"
            );

            let (_, broken) = workspace_findings(
                &sweep(ide::diagnostics_baseline::DiagnosticsBaselineState::Error),
                1,
                None,
            );
            assert_ne!(broken.to_value()["status"], "complete", "{}", broken.to_value());
        }

        /// The baseline keys findings by the project-relative path, so the request's own
        /// spelling must not reach the fingerprint: `./x` and `x` are the same file, and
        /// a mismatch turns a suppressed finding back into a new one.
        #[cfg(unix)]
        #[test]
        fn baseline_project_path_ignores_request_spelling() {
            let root = tempfile::tempdir().unwrap();
            let nested = root.path().join("CommonModules/Сервер/Ext");
            std::fs::create_dir_all(&nested).unwrap();
            let module = nested.join("Module.bsl");
            std::fs::write(&module, "Процедура Тест()\nКонецПроцедуры\n").unwrap();

            let plain = baseline_project_path(root.path(), &module);
            assert_eq!(plain, "CommonModules/Сервер/Ext/Module.bsl");

            let dotted = root.path().join("./CommonModules/Сервер/./Ext/Module.bsl");
            assert_eq!(baseline_project_path(root.path(), &dotted), plain);

            // A root reached through a symlink names the same files.
            let link = root
                .path()
                .parent()
                .unwrap()
                .join(format!("{}-link", root.path().file_name().unwrap().to_string_lossy()));
            let _ = std::fs::remove_file(&link);
            std::os::unix::fs::symlink(root.path(), &link).unwrap();
            assert_eq!(baseline_project_path(&link, &module), plain);
            let _ = std::fs::remove_file(&link);
        }

        /// The body is shaped to the budget AFTER the item-level trim, so a response
        /// whose items fit but whose envelope does not is trimmed by the shaper. The
        /// completeness verdict must see that; otherwise the envelope calls a trimmed
        /// body complete. The fixture is exactly such a case: items fit, body does not.
        #[test]
        fn workspace_completeness_reflects_the_budget_shaper() {
            use crate::diagnostics_state::CodeAggregate;
            let aggregate = |index: usize| CodeAggregate {
                code: format!("{index}{}", "C".repeat(260)),
                severity: ide::SeverityBucket::Error,
                count: 1,
                files_affected: 1,
            };
            let sweep = WorkspaceSweep {
                aggregates: vec![aggregate(1), aggregate(2), aggregate(3)],
                files_swept: 3,
                files_total: 3,
                files_out_of_scope: 0,
                files_unread: 0,
                findings_ignored_by_author: 0,
                author_head: None,
                truncated: false,
                cancelled: false,
                baseline: ide::diagnostics_baseline::DiagnosticsBaselineSummary::disabled(),
                baseline_epoch: "disabled".to_owned(),
                lacks_owning_configuration: false,
            };

            let (unbudgeted, _) = workspace_findings(&sweep, 1, None);
            assert_eq!(
                unbudgeted["aggregates"].as_array().unwrap().len(),
                3,
                "positive control: without a budget nothing is dropped"
            );

            let (body, completeness) = workspace_findings(&sweep, 1, Some(256));
            assert_eq!(body["budget_exhausted"], true, "the shaper must have trimmed the body");
            assert_ne!(
                completeness.to_value()["status"],
                "complete",
                "a trimmed body must not be announced as complete: {}",
                completeness.to_value()
            );
        }

        fn run_workspace(state: &DiagnosticsState, opts: &SweepOptions) -> Value {
            let outcome = state.read_mut(|resident, gen| {
                let sweep = resident.workspace_aggregates(
                    &resident.config().clone(),
                    opts,
                    &RequestCancel::default(),
                );
                workspace_findings(&sweep, gen, None)
            });
            match outcome {
                ResidentOutcome::Ready((body, _completeness), _) => body,
                _ => panic!("expected Ready outcome"),
            }
        }

        fn sweep_opts(max_files: usize) -> SweepOptions {
            SweepOptions { min_severity: SeverityBucket::Hint, codes: Vec::new(), max_files }
        }

        #[test]
        fn workspace_sweep_shapes_the_result() {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            sample_workspace(root);
            let state = ready_state(root);

            let body = run_workspace(&state, &sweep_opts(DEFAULT_MAX_SWEEP_FILES));
            assert_eq!(body["files_total"], 1);
            assert_eq!(body["files_swept"], 1);
            assert_eq!(body["truncated"], false);
            assert!(body["aggregates"].is_array(), "per-code aggregates, no per-finding detail");
            assert!(body["result_id"].as_str().unwrap().starts_with("workspace@"));
        }

        #[test]
        fn workspace_sweep_honors_the_file_cap() {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            write_common_module(root, "Сервер", "Функция Ф() Экспорт Возврат 1; КонецФункции\n");
            write_common_module(root, "Клиент", "Процедура П() Экспорт КонецПроцедуры\n");
            let state = ready_state(root);

            let body = run_workspace(&state, &sweep_opts(1));
            assert_eq!(body["files_total"], 2);
            assert_eq!(body["files_swept"], 1, "capped at one file");
            assert_eq!(body["truncated"], true);
        }
    }

    #[test]
    fn graph_id_maps_a_finding_to_its_containing_method() {
        let method = DocumentSymbol {
            name: "Считать".to_string(),
            range: TextRange::new(10u32.into(), 50u32.into()),
            selection_range: TextRange::new(10u32.into(), 20u32.into()),
            detail: ide::SymbolDetail::Function(ide::MethodDetail {
                is_export: true,
                directives: Vec::new(),
                params: Vec::new(),
            }),
            children: Vec::new(),
        };
        let methods = method_ranges(std::slice::from_ref(&method));
        let root = ide::StripRoot::pinned(std::path::Path::new("/ws"));
        let module = std::path::Path::new("/ws/CommonModules/Сервер/Ext/Module.bsl");

        // A finding inside the method span resolves to the method's durable graph id.
        let inside = TextRange::new(30u32.into(), 35u32.into());
        assert_eq!(
            graph_id_for(inside, &methods, module, &root).as_deref(),
            Some("method/common/Сервер/Считать")
        );
        // A module-body finding (outside any method) carries no graph id.
        let outside = TextRange::new(0u32.into(), 5u32.into());
        assert_eq!(graph_id_for(outside, &methods, module, &root), None);
        // A form module: a finding inside a method now falls back to the
        // `method/file/<rel>::<name>` id the graph mints, with the rel stripped to `root`.
        let form = std::path::Path::new("/ws/CommonForms/Форма/Ext/Form/Module.bsl");
        assert_eq!(
            graph_id_for(inside, &methods, form, &root).as_deref(),
            Some("method/file/CommonForms/Форма/Ext/Form/Module.bsl::Считать")
        );
    }

    #[test]
    fn method_ranges_descends_into_regions() {
        // A function nested inside an `#Область` region (a non-method symbol with
        // children) is still collected.
        let region = DocumentSymbol {
            name: "Служебные".to_string(),
            range: TextRange::new(0u32.into(), 100u32.into()),
            selection_range: TextRange::new(0u32.into(), 10u32.into()),
            detail: ide::SymbolDetail::Region,
            children: vec![DocumentSymbol {
                name: "Внутренняя".to_string(),
                range: TextRange::new(20u32.into(), 80u32.into()),
                selection_range: TextRange::new(20u32.into(), 30u32.into()),
                detail: ide::SymbolDetail::Procedure(ide::MethodDetail {
                    is_export: false,
                    directives: Vec::new(),
                    params: Vec::new(),
                }),
                children: Vec::new(),
            }],
        };
        let methods = method_ranges(std::slice::from_ref(&region));
        assert_eq!(methods.len(), 1, "the region itself is not a method, its child is");
        assert_eq!(methods[0].0, "Внутренняя");
    }

    #[test]
    fn result_id_folds_the_content_hash() {
        let path = std::path::Path::new("/ws/Mod.bsl");
        let a = result_id(path, 3, "Процедура А() КонецПроцедуры");
        let a_again = result_id(path, 3, "Процедура А() КонецПроцедуры");
        let b = result_id(path, 3, "Процедура Б() КонецПроцедуры");
        assert_eq!(a, a_again, "same path+gen+content → same id");
        assert_ne!(a, b, "different content → different id even at the same generation");
        assert!(a.starts_with("/ws/Mod.bsl@3@"), "id is <path>@<gen>@<hash>: {a}");
    }
}
