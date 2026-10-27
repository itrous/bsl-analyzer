//! Agent-facing `references` tool: every occurrence of ONE symbol, with what
//! each occurrence does at its site.
//!
//! Thin projection over the three steps of [`ide::find_references_by_name`] —
//! [`ide::anchor_references`], [`ide::walk_reference_file`], [`ide::finish_references`].
//! Every semantic decision — which symbol an anchor names, whether it names more
//! than one, which occurrences count, and what kind each is — is taken in `ide`.
//! This module translates roots into file identities, hands the walk to the
//! resident so it runs in chunks with a trim between them, projects places under
//! the location contract, applies the display limit and the output budget, and
//! assembles the envelope.
//!
//! The one thing it must never do is answer an empty list where the search
//! could not run: `outcome` carries that fact, and the whole tool exists
//! because a bare zero is what a client renames on.

use std::path::Path;

use bsl_search::WorkspaceRoots;
use ide::{
    BodySource, FileIdSet, ReferenceAnchor, ReferenceArea, ReferenceHit, ReferenceKind,
    ReferencesOutcome, ReferencesRequest, ReferencesResult,
};
use rmcp::model::CallToolResult;
use rmcp::ErrorData as McpError;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::cancel::RequestCancel;
use crate::diagnostics_state::DiagnosticsResident;
use crate::tools::location as loc;
use crate::tools::name_answer::{insert_resolved_location, NameAnswer};
use crate::tools::response::{structured, trim_items_to_budget, DEFAULT_OUTPUT_BUDGET_TOKENS};

/// Version of THIS tool's response shape. Separate from the location block's
/// own version: the block travels across tools and cannot be versioned by one.
pub(crate) const SCHEMA_VERSION: &str = "1";

pub(crate) const DEFAULT_LIMIT: usize = 50;
pub(crate) const MAX_LIMIT: usize = 500;
pub(crate) const DEFAULT_MAX_FILES: usize = 2000;
pub(crate) const MAX_MAX_FILES: usize = 10_000;

/// The request as the tool's parameters spell it, before any of it is resolved
/// against a resident.
pub(crate) struct Params<'a> {
    pub symbol: Option<&'a str>,
    /// Narrows where the DECLARATION is looked for — the way out of `ambiguous`.
    pub anchor_root_id: Option<&'a str>,
    /// Positional anchor: the root `path` is spelled against.
    pub root_id: Option<&'a str>,
    pub path: Option<&'a str>,
    pub line: Option<u32>,
    pub column: Option<u32>,
    /// The text of the line the caller read, which certifies the anchor.
    pub line_content: Option<&'a str>,
    /// Narrows the REFERENCES shown, not the anchor.
    pub area_root_id: Option<&'a str>,
    pub area_path_prefix: Option<&'a str>,
    pub kinds: &'a [String],
    pub include_declaration: Option<bool>,
    pub limit: Option<usize>,
    pub max_files: Option<usize>,
    pub include_preview: Option<bool>,
}

/// What the tool answered, before it is stamped with the resident's identity.
pub(crate) struct Answer {
    body: Map<String, Value>,
    /// Whose data composes the body — decided in `ide`, echoed here.
    source: BodySource,
    completeness: loc::Completeness,
}

/// Whether this answer is worth one forced rescan and a retry.
///
/// The decision is read off the ANSWER — its outcome, and for a miss whether a name was
/// echoed — and nowhere else. Keeping it in the handler put it where nothing can observe
/// it: `read()` polls for drift before it computes anything, a live change hub applies a
/// disk edit into the resident on the way, and the first answer of an end-to-end test may
/// already be `resolved` without any rescan at all.
///
/// A stale text anchor and a name that matched nothing both carry evidence a second look
/// can check: text the caller quoted, or a name something declares somewhere. A positional
/// miss carries none — it says only "no name stands at these coordinates", and re-reading
/// the same coordinates says it again. So it stays out, and `path`+`line` answers exactly
/// as it did before the text anchor existed.
pub(crate) fn warrants_rescan(answer: &Answer) -> bool {
    match answer.body.get("outcome").and_then(Value::as_str) {
        Some("anchor_stale") => true,
        Some("not_found") => answer.body.contains_key("symbol"),
        _ => false,
    }
}

/// What the tool says when nothing addresses a symbol at all. Quoted in two places —
/// the handler bails before it touches a resident, the anchor builder after — and one
/// string so the two cannot drift into two different refusals for one mistake.
pub(crate) const NO_ANCHOR: &str =
    "one of 'symbol' or 'path' with 'line_content' and/or 'line' is required";

/// A quote certifies a line of ONE file, and without that file it names nothing.
pub(crate) const CONTENT_NEEDS_PATH: &str =
    "'line_content' quotes a line of one file, so it goes with 'path'; to look for text \
     across the workspace use `search`";

/// How many files a `(root_id, path_prefix)` selection matched, published so a
/// mistyped prefix reads as "nothing is there" rather than "no references".
struct Selection {
    files: FileIdSet,
    root_id: Option<String>,
    path_prefix: Option<String>,
}

/// Three phases on handles the resident hands out per phase, never on `resident.db()`:
/// every query runs on a handle whose salsa token this request can cancel, or a cancelled
/// call would keep computing on the master handle. The walk between the anchor and the
/// render is the resident's, in chunks with a trim between them, which is why the
/// resident is taken mutably: an answer about thousands of files must not keep every
/// file's trees and bodies until it is rendered.
pub(crate) fn answer(
    resident: &mut DiagnosticsResident,
    cancel: &RequestCancel,
    params: &Params<'_>,
    max_output_tokens: usize,
    external: &[&dyn ide::ExternalNameSource],
) -> Result<Answer, McpError> {
    let limit = params.limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT);

    let (request, area_selection, anchored) = resident
        .with_request_handle(cancel, |resident, analysis| {
            anchor(resident, analysis.database(), params, external)
        })?;

    let result = match anchored {
        ide::Anchored::Answered(result) => result,
        ide::Anchored::Walk(walk) => {
            let per_file = resident.walk_files_in_chunks(cancel, walk.scanned(), |analysis, id| {
                ide::walk_reference_file(analysis.database(), id, walk.symbol(), &request)
            });
            resident.with_request_handle(cancel, |_, analysis| {
                ide::finish_references(analysis.database(), walk, per_file.concat())
            })
        }
    };

    Ok(resident.with_request_handle(cancel, |resident, analysis| {
        render(
            analysis.database(),
            resident.workspace_roots(),
            &result,
            params,
            &request.anchor,
            area_selection,
            limit,
            max_output_tokens,
            resident.unread_count(),
        )
    }))
}

/// The first phase: the request as `ide` reads it, and what anchoring it settled.
fn anchor(
    resident: &DiagnosticsResident,
    db: &ide::RootDatabaseImpl,
    params: &Params<'_>,
    external: &[&dyn ide::ExternalNameSource],
) -> Result<(ReferencesRequest, Option<Selection>, ide::Anchored), McpError> {
    let max_files = params.max_files.unwrap_or(DEFAULT_MAX_FILES).min(MAX_MAX_FILES);

    let anchor = build_anchor(resident, params)?;
    // A position names its file outright, so there is no declaration to look for
    // in a root — and a parameter that is validated against the root table and
    // then dropped is worse than one that was refused: the caller is told its
    // root is wrong, never that its narrowing did nothing.
    // Written against the class and not one member of it: every anchor by `path` already
    // names its file, so there is no declaration left for a root to narrow. Spelling the
    // condition as `matches!(anchor, Position { .. })` would let the next anchor by file
    // through, and a root validated against the root table and then dropped is worse than
    // one refused — the caller is told its narrowing was understood while nothing narrowed.
    if params.anchor_root_id.is_some() && !matches!(anchor, ReferenceAnchor::Name(_)) {
        return Err(McpError::invalid_params(
            "'anchor_root_id' picks which root the DECLARATION is looked for in, so it goes \
             with 'symbol'; an anchor by 'path' already names one file. Use 'area_root_id' \
             to narrow which references are shown",
            None,
        ));
    }
    let anchor_selection = match params.anchor_root_id {
        Some(root_id) => Some(select(resident, db, Some(root_id), None)?),
        None => None,
    };
    let area_selection = match (params.area_root_id, params.area_path_prefix) {
        (None, None) => None,
        (root_id, prefix) => Some(select(resident, db, root_id, prefix)?),
    };

    let request = ReferencesRequest {
        anchor,
        anchor_files: anchor_selection.as_ref().map(|s| s.files.clone()),
        area: ReferenceArea { files: area_selection.as_ref().map(|s| s.files.clone()) },
        kinds: parse_kinds(params.kinds)?,
        include_declaration: params.include_declaration.unwrap_or(true),
        max_files,
    };

    let anchored = ide::anchor_references(db, &request, external);
    Ok((request, area_selection, anchored))
}

/// Publish the answer with the identity of whoever composed its body.
pub(crate) fn finish(answer: Answer, revision: u64, topology: u64, stale: bool) -> CallToolResult {
    let Answer { mut body, source, completeness } = answer;
    let freshness = match source {
        // A body of reference hits, or of a symbol the resident resolved, is the
        // resident's own answer at one revision — even when it was the name
        // dictionary that found the anchor.
        BodySource::Resident => loc::Freshness::new(loc::FreshnessSource::Resident, completeness)
            .with_revision(revision)
            .with_topology(topology)
            .with_stale(stale),
        // A body of dictionary candidates spans several artefacts with different
        // revisions; borrowing the resident's would claim an identity they do
        // not share.
        BodySource::NameDictionary => NameAnswer::freshness(completeness),
    };
    body.insert("freshness".into(), freshness.to_value());
    structured(Value::Object(body))
}

fn build_anchor(
    resident: &DiagnosticsResident,
    params: &Params<'_>,
) -> Result<ReferenceAnchor, McpError> {
    if let Some(symbol) = params.symbol.map(str::trim).filter(|s| !s.is_empty()) {
        if params.path.is_some() {
            return Err(McpError::invalid_params(
                "pass either 'symbol' or an anchor on 'path', not both: they anchor on \
                 different things and the answer would not say which one it followed",
                None,
            ));
        }
        // `root_id` qualifies `path` and nothing else. Accepting it beside a
        // symbol — and validating it against the root table, as `select` would —
        // tells the caller its narrowing was understood while the walk spans
        // every root. `anchor_root_id` is the parameter that narrows a name.
        if params.line.is_some() || params.column.is_some() {
            return Err(McpError::invalid_params(
                "'line'/'column' belong to a 'path' anchor; a 'symbol' is resolved by name \
                 and has no position to start from",
                None,
            ));
        }
        if params.line_content.is_some() {
            return Err(McpError::invalid_params(
                "'line_content' certifies a line of the file named by 'path'; a 'symbol' is \
                 resolved by name and quotes nothing",
                None,
            ));
        }
        if params.root_id.is_some() {
            return Err(McpError::invalid_params(
                "'root_id' spells out which root 'path' is relative to, so it needs a \
                 'path'; to look for a DECLARATION of 'symbol' in one root pass \
                 'anchor_root_id'",
                None,
            ));
        }
        return Ok(ReferenceAnchor::Name(symbol.to_string()));
    }

    let path = params.path.ok_or_else(|| {
        McpError::invalid_params(
            if params.line_content.is_some() { CONTENT_NEEDS_PATH } else { NO_ANCHOR },
            None,
        )
    })?;
    let resolved = resident
        .resolve_rooted_path(params.root_id, Path::new(path))
        .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
    let file_id = resident.file_id_for(resolved.as_path()).ok_or_else(|| {
        if resident.is_unread(resolved.as_path()) {
            McpError::invalid_params(
                format!(
                    "'{}' is a workspace .bsl file whose bytes could not be read; it is held \
                     out of service and re-read every drift window",
                    resolved.display()
                ),
                None,
            )
        } else {
            McpError::invalid_params(
                format!("'{}' is not a resident workspace .bsl file", resolved.display()),
                None,
            )
        }
    })?;
    // A quoted line makes `line` a narrowing rather than an address, so the requirement is
    // lifted exactly there. This is the main input of the promise: a caller that read the
    // file and does not trust its own line numbers has the text and nothing else.
    let Some(content) = params.line_content else {
        let line = params.line.ok_or_else(|| {
            McpError::invalid_params(
                "'line' is required with 'path' unless 'line_content' quotes the line",
                None,
            )
        })?;
        return Ok(ReferenceAnchor::Position { file_id, line, column: params.column.unwrap_or(0) });
    };
    let content = content.trim();
    if content.is_empty() {
        return Err(McpError::invalid_params(
            "'line_content' is empty once its surrounding whitespace is dropped, so it \
             certifies nothing; quote the text of the line you read",
            None,
        ));
    }
    Ok(ReferenceAnchor::Text {
        file_id,
        line: params.line,
        column: params.column,
        content: content.to_owned(),
    })
}

/// Translate a `(root_id, path_prefix)` selection into the set of files it
/// names.
///
/// Identity, not string comparison: a root is DECLARED with one spelling and
/// INDEXED with another whenever a link is involved, so a prefix built from the
/// declared spelling would match none of the indexed paths and the answer would
/// be an honest-looking zero. The attributor that mints every published pair is
/// asked instead, and the prefix is then compared inside the key — the same
/// address the histogram publishes.
///
/// The id must also still mean what the resident's own table minted it to mean: an id survives a
/// root's retarget, so a caller addressing a root the resident registered against another directory
/// is refused, not answered ([`WorkspaceRoots::registration_is_current`]).
fn select(
    resident: &DiagnosticsResident,
    db: &ide::RootDatabaseImpl,
    root_id: Option<&str>,
    path_prefix: Option<&str>,
) -> Result<Selection, McpError> {
    let roots = resident.workspace_roots();
    if let Some(root_id) = root_id {
        if !roots.contains_id(root_id) {
            return Err(McpError::invalid_params(
                format!(
                    "no source root is registered under '{root_id}'; `search` reports the \
                     roots this workspace knows"
                ),
                None,
            ));
        }
        // The resident's table is the one it was BUILT with, and an id survives a root's retarget
        // (`WorkspaceRoots::inheriting_root_ids`): the same `(root_id, path)` then names one file to
        // the resident and another to the workspace, and the selection below would be answered from
        // the directory the id no longer leads to. Refused rather than answered: the correlation
        // between the caller's address and the resident's corpus came apart.
        //
        // The refusal names the FACT and not a cause: the declared spelling failing to resolve is
        // what was observed, and a retarget is only one of the ways it happens (a target that is
        // merely unreachable — unmounted, not yet permitted — fails the same check).
        if !roots.registration_is_current(root_id) {
            return Err(McpError::invalid_params(
                format!(
                    "source root '{root_id}' can no longer be resolved to the directory it was \
                     registered with; the resident's view may be stale — reload the workspace"
                ),
                None,
            ));
        }
    }
    let prefix = path_prefix.map(|prefix| prefix.replace('\\', "/").trim_matches('/').to_string());

    let mut files = FileIdSet::default();
    for (path, file_id) in resident.files() {
        // Nothing in this loop is a salsa query, so the request's token is observed
        // here or not at all: on a large configuration this walks tens of thousands
        // of files doing pure path arithmetic.
        salsa::Database::unwind_if_revision_cancelled(db);
        // `root_of` and not `key_of_path`: the latter canonicalises what it is
        // given, and a resident path is canonical already — one `canonicalize`
        // syscall per workspace file, on a configuration with tens of thousands
        // of them, for an answer that is in hand. Both spellings are passed as
        // the one the resident holds, so the attribution is the same and the
        // declared-spelling fallback still catches a path canonicalisation could
        // not resolve.
        let Some(key) = roots.root_of(path, path) else { continue };
        if root_id.is_some_and(|wanted| key.root_id != wanted) {
            continue;
        }
        if let Some(prefix) = &prefix {
            let key_path = key.path.replace('\\', "/");
            let under = key_path == *prefix
                || key_path.starts_with(&format!("{prefix}/"))
                || prefix.is_empty();
            if !under {
                continue;
            }
        }
        files.insert(file_id);
    }

    Ok(Selection {
        files,
        root_id: root_id.map(str::to_string),
        path_prefix: prefix.filter(|p| !p.is_empty()),
    })
}

fn parse_kinds(kinds: &[String]) -> Result<Option<Vec<ReferenceKind>>, McpError> {
    if kinds.is_empty() {
        return Ok(None);
    }
    let mut out = Vec::with_capacity(kinds.len());
    for kind in kinds {
        let parsed = ReferenceKind::parse(kind.trim()).ok_or_else(|| {
            McpError::invalid_params(
                format!("unknown reference kind '{kind}'; expected {}", KIND_VOCABULARY),
                None,
            )
        })?;
        out.push(parsed);
    }
    Ok(Some(out))
}

pub(crate) const KIND_VOCABULARY: &str = "declaration | call | write | read";

fn outcome_str(outcome: &ReferencesOutcome) -> &'static str {
    match outcome {
        ReferencesOutcome::Resolved => "resolved",
        ReferencesOutcome::Ambiguous => "ambiguous",
        ReferencesOutcome::NotFound => "not_found",
        ReferencesOutcome::UnsupportedSymbol { .. } => "unsupported_symbol",
        ReferencesOutcome::AnchorStale { .. } => "anchor_stale",
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "the projection needs the database, the root table, the answer, the request that \
              produced it and both caps; grouping them would be a struct built at the one call \
              site and taken apart in the first statement"
)]
fn render(
    db: &ide::RootDatabaseImpl,
    // The root table of the resident that answered. Its workspace is the base a form handler's
    // path-fallback id is stripped against, so the id comes out byte-identical to the graph
    // builder's — and to the one `symbol_info` and `diagnostics` mint for the same method,
    // because all three take it from the generation rather than from the disk.
    roots: &WorkspaceRoots,
    result: &ReferencesResult,
    params: &Params<'_>,
    // Which anchor the walk actually ran on, after trimming and validation.
    anchor: &ReferenceAnchor,
    area: Option<Selection>,
    limit: usize,
    max_output_tokens: usize,
    // Files the resident holds out of service because their bytes could not be read. A
    // walk over "the workspace" that quietly means "the part of it that could be read" is
    // the silent incompleteness this tool exists to end.
    unread_files: usize,
) -> Answer {
    let mut body = Map::new();
    body.insert("schema_version".into(), json!(SCHEMA_VERSION));
    body.insert("outcome".into(), json!(outcome_str(&result.outcome)));
    // The name the walk ran on, not the one that arrived: `build_anchor` trims it, and an
    // echo that still carries the caller's whitespace is a different string from the one the
    // answer is about.
    if let Some(symbol) = params.symbol.map(str::trim).filter(|s| !s.is_empty()) {
        body.insert("symbol".into(), json!(symbol));
    }
    let mut anchor_block = anchor_value(anchor, result);
    // The graph id of the ANCHOR, one per answer — not one per occurrence: the occurrences
    // are places in files, while the id names the node they all refer to. Present only when
    // the anchor settled on a single declaration the graph's grammar can address; a name that
    // stayed ambiguous has no one node to name, and a variable is not a node at all.
    if let [declaration] = result.declarations.as_slice() {
        let graph_root = ide::StripRoot::pinned(roots.workspace_canonical());
        if let Some(graph_id) = ide::graph_id_of_declaration(db, declaration, Some(&graph_root)) {
            if let Some(block) = anchor_block.as_object_mut() {
                block.insert("graph_id".into(), json!(graph_id));
            }
        }
    }
    body.insert("anchor".into(), anchor_block);
    if let Some(area) = &area {
        body.insert(
            "area".into(),
            json!({
                "root_id": area.root_id,
                "path_prefix": area.path_prefix,
                // A prefix that names nothing says so here, instead of being
                // read off an empty reference list as "no references".
                "files_in_area": area.files.len(),
            }),
        );
    }

    let total = result.hits.len();
    let mut completeness = loc::Completeness::complete();

    match &result.outcome {
        ReferencesOutcome::Resolved => {
            let shown: Vec<&ReferenceHit> = result.hits.iter().take(limit).collect();
            // Resolved once and kept: the preview needs the very line and column the
            // location publishes, and resolving a second time would be a second reading of
            // where the occurrence is.
            let places: Vec<ide::ResolvedPlace> = shown
                .iter()
                .map(|hit| {
                    ide::resolve_file_range(db, hit.file_id, Some(hit.range), hit.enclosing_range)
                })
                .collect();
            let mut items: Vec<Value> = places
                .iter()
                .zip(&shown)
                .map(|(place, hit)| hit_value(place, roots, hit))
                .collect();
            let budget_cut = trim_items_to_budget(&mut items, max_output_tokens);
            let capped = total > shown.len();

            body.insert("references".into(), Value::Array(items));
            body.insert("total".into(), json!(total));
            body.insert("total_is_lower_bound".into(), json!(result.files_capped));
            // Two answers taken with a truncated walk count different candidate
            // sets, so the narrowing strategy that replaces a cursor does not
            // apply to them — said in a field rather than left to be inferred.
            body.insert("narrowing_comparable".into(), json!(!result.files_capped));
            body.insert("files_scanned".into(), json!(result.files_scanned));

            let used_tokens = items_tokens(&body);
            let histogram_budget = max_output_tokens.saturating_sub(used_tokens);
            let mut buckets: Vec<Value> = result
                .per_file
                .iter()
                .map(|(file_id, count)| {
                    // A bucket names only its file, so `resolve_file_range` returns
                    // before it reads any text — the one place in rendering where no
                    // salsa query runs and the token would go unread. The loop is
                    // unbounded (every file with a hit) and pays a `canonicalize`
                    // syscall per bucket, so it is also the one that must not run on.
                    salsa::Database::unwind_if_revision_cancelled(db);
                    let mut entry = Map::new();
                    insert_resolved_location(
                        &ide::resolve_file_range(db, *file_id, None, None),
                        Some(roots),
                        &mut entry,
                    );
                    entry.insert("count".into(), json!(count));
                    Value::Object(entry)
                })
                .collect();
            let buckets_before = buckets.len();
            let histogram_over_budget = trim_items_to_budget(&mut buckets, histogram_budget);
            // `trim_items_to_budget` keeps one item even when that item alone
            // exceeds the budget, and reports the overflow. Reporting THAT as a
            // truncated histogram would claim whole files were lost while the
            // counts still add up to `total` — the flag exists to say the
            // opposite, and I10 ties it to a strictly smaller sum.
            let histogram_cut = buckets.len() < buckets_before;
            body.insert("files".into(), Value::Array(buckets));
            // A shortened histogram loses whole files, and the caller walks it
            // to see everything the limit hid — so its truncation is named
            // apart from the body's.
            body.insert("histogram_truncated".into(), json!(histogram_cut));

            // Last, and only out of what the answer left. `include_preview` decorates; it
            // never changes what the answer contains, and the histogram is the only way
            // past `limit` in a tool with no cursor — paying for an optional caption with
            // it would let a decoration shorten the answer.
            // The completeness is finished BEFORE previews are measured, and it has to be:
            // the reserve below is only an upper bound on the envelope if it is built from
            // the reasons that envelope will actually carry. Computing it from an empty
            // completeness and then adding four reasons underneath would understate it by a
            // hundred bytes apiece — and previews would spend budget the envelope has to.
            completeness = completeness
                .when(capped, loc::ReasonCode::ResultCap, RESULT_CAP_LIMIT)
                .when(result.files_capped, loc::ReasonCode::ResultCap, RESULT_CAP_FILES)
                .when(budget_cut || histogram_over_budget, loc::ReasonCode::OutputBudget, TRIMMED);

            let previews_omitted = if params.include_preview.unwrap_or(false) {
                // The budget previews are spent from is the one the CALLER receives, and this
                // layer has not written the freshness envelope yet — `finish` adds it once
                // the resident's identity is known. Reserving for it is what keeps a
                // decoration from being the thing that carries an otherwise fitting answer
                // past its own ceiling. The basis carries every reason still to come,
                // including the one previews themselves may add, so the reserve is never
                // smaller than the envelope written against it.
                let basis = completeness
                    .clone()
                    .when(true, loc::ReasonCode::OutputBudget, PREVIEWS_OMITTED)
                    .when(
                        result.anchor_candidates_capped,
                        loc::ReasonCode::ResultCap,
                        anchor_cap_detail(anchor),
                    )
                    .when(unread_files > 0, loc::ReasonCode::UnreadableFiles, UNREADABLE);
                let reserved = max_output_tokens.saturating_sub(envelope_tokens(&basis));
                attach_previews(db, &mut body, &places, &shown, reserved)
            } else {
                0
            };
            if previews_omitted > 0 {
                body.insert("previews_omitted".into(), json!(previews_omitted));
            }

            completeness = completeness.when(
                previews_omitted > 0,
                loc::ReasonCode::OutputBudget,
                PREVIEWS_OMITTED,
            );
        }
        ReferencesOutcome::Ambiguous | ReferencesOutcome::NotFound => {
            let mut budget_cut = false;
            if !result.anchor_sites.is_empty() {
                let mut sites: Vec<Value> = result
                    .anchor_sites
                    .iter()
                    .map(|site| anchor_site_value(db, roots, site))
                    .collect();
                budget_cut |=
                    trim_items_to_budget(&mut sites, remaining_budget(&body, max_output_tokens));
                body.insert("anchor_sites".into(), Value::Array(sites));
                // The axes named are the ones that still work. A `line` beside a quote is
                // NOT among them: it stopped choosing when the pin was dropped — it marks the
                // place it stood at and nothing more — so advertising it would send the
                // caller round a trip that cannot arrive. What does work is a narrower quote,
                // or dropping the quote and taking a place by the coordinates this very
                // answer just published, which are of this revision by construction.
                body.insert(
                    "resolution_hint".into(),
                    json!(
                        "narrow `line_content` to text that names only the symbol you mean; \
                         a `line` beside a quote marks a place (`pointed_by_line`) but never \
                         chooses one. To take a place as it stands here, drop `line_content` \
                         and anchor on its `location` with the PAIR `root_id`+`path` plus \
                         `line`+`column` — a path without its root is spelled against the \
                         workspace, not against the root the place belongs to"
                    ),
                );
            }
            if !result.declarations.is_empty() {
                let candidates: Vec<Value> = result
                    .declarations
                    .iter()
                    .map(|declaration| {
                        let mut entry = Map::new();
                        insert_resolved_location(
                            &ide::resolve_file_range(
                                db,
                                declaration.file_id,
                                Some(declaration.name_range),
                                declaration.enclosing_range,
                            ),
                            Some(roots),
                            &mut entry,
                        );
                        entry.insert("kind".into(), json!(declaration.kind.symbol_kind()));
                        Value::Object(entry)
                    })
                    .collect();
                // The hint names the axis that can actually separate THESE
                // declarations. `anchor_root_id` separates roots and nothing
                // else, and an object module and its manager module sit in one
                // root — pointing an agent at it there would send it down a road
                // that cannot arrive.
                let roots_differ = distinct_roots(&candidates) > 1;
                let mut candidates = candidates;
                budget_cut |= trim_items_to_budget(&mut candidates, max_output_tokens);
                body.insert("declarations".into(), Value::Array(candidates));
                body.insert(
                    "resolution_hint".into(),
                    json!(if roots_differ {
                        "pass `anchor_root_id` to pick one declaration"
                    } else {
                        "these declarations share a root: anchor on the one you want with its `root_id`+`path` pair and `line_content`, the text of the line it is declared on"
                    }),
                );
            }
            if let Some(lookup) = &result.candidates {
                let (lookup_body, lookup_completeness, cut) =
                    lookup_value(db, roots, lookup, remaining_budget(&body, max_output_tokens));
                completeness = lookup_completeness;
                budget_cut |= cut;
                body.insert("lookup".into(), lookup_body);
                // A dictionary ambiguity is not separated by a root — its
                // candidates are spread over kinds, not roots — and the axis
                // that ends it is their own qualified name, passed back as
                // `symbol`. Leaving the field out here would send the agent to
                // the tool description, which can only name a general axis.
                if matches!(result.outcome, ReferencesOutcome::Ambiguous)
                    && !body.contains_key("resolution_hint")
                {
                    body.insert(
                        "resolution_hint".into(),
                        json!("pass one `lookup.candidates[].address.symbol` back as `symbol`"),
                    );
                }
            }
            completeness = completeness.when(
                budget_cut,
                loc::ReasonCode::OutputBudget,
                "candidate list trimmed to fit `max_output_tokens`",
            );
        }
        ReferencesOutcome::AnchorStale { reason, line_matches } => {
            let mut stale = Map::new();
            stale.insert("reason".into(), json!(reason.as_str()));
            stale.insert("line_content_matches".into(), json!(line_matches));
            // What stands at the coordinate the caller sent, so it can see which of the two
            // pictures moved without a second call.
            if let ReferenceAnchor::Text { file_id, line: Some(line), .. } = anchor {
                if let Some(evidence) = evidence_line(db, *file_id, *line) {
                    stale.insert("actual_line".into(), json!(evidence.snippet));
                    if evidence.truncated {
                        stale.insert("actual_line_truncated".into(), json!(true));
                    }
                    if evidence.redacted {
                        stale.insert("actual_line_redacted".into(), json!(true));
                    }
                }
            }
            body.insert("anchor_stale".into(), Value::Object(stale));
        }
        ReferencesOutcome::UnsupportedSymbol { category } => {
            body.insert(
                "unsupported".into(),
                json!({
                    "category": category.as_str(),
                    "reason": "this symbol has no reference walk; an empty list would be \
                               indistinguishable from a proven zero",
                }),
            );
            // The dictionary composed this answer too when the category came from
            // one of its candidates, and the envelope says so — `name-dictionary`
            // has no revision of its own, and the per-source truth is the
            // `providers` array. Dropping it here would leave an envelope that
            // names a source and shows nothing of what it could and could not
            // consult.
            if let Some(lookup) = &result.candidates {
                let (lookup_body, lookup_completeness, cut) =
                    lookup_value(db, roots, lookup, remaining_budget(&body, max_output_tokens));
                completeness = lookup_completeness.when(
                    cut,
                    loc::ReasonCode::OutputBudget,
                    "candidate list trimmed to fit `max_output_tokens`",
                );
                body.insert("lookup".into(), lookup_body);
            }
        }
    }

    Answer {
        body,
        source: result.body_source,
        completeness: completeness
            .when(
                result.anchor_candidates_capped,
                loc::ReasonCode::ResultCap,
                // The axis named is the one this anchor HAS. Telling a caller that quoted a
                // line to pass a qualified `symbol` sends it down a road it did not come by:
                // the thing it holds is text, and text is what narrows it.
                anchor_cap_detail(anchor),
            )
            .when(unread_files > 0, loc::ReasonCode::UnreadableFiles, UNREADABLE),
    }
}

/// Which anchor the answer followed, and — for the two that stand somewhere — where it
/// actually stood.
///
/// There is no `verified` flag beside it on purpose: a name has nothing to verify, and
/// `verified: false` would teach a caller that its `symbol` request was somehow the lesser
/// one. The distinction the caller needs is which anchor answered, and `mode` carries it.
fn anchor_value(anchor: &ReferenceAnchor, result: &ReferencesResult) -> Value {
    let mut block = Map::new();
    match anchor {
        ReferenceAnchor::Name(_) => {
            block.insert("mode".into(), json!("symbol"));
        }
        ReferenceAnchor::Position { line, .. } => {
            block.insert("mode".into(), json!("position"));
            block.insert("line".into(), json!(line));
        }
        ReferenceAnchor::Text { line, .. } => {
            block.insert("mode".into(), json!("line_content"));
            if let Some(landed) = result.anchor_line {
                block.insert("line".into(), json!(landed));
                // Where "any occurrence will do" stops being silent: the answer is still
                // resolved, and it says it stood somewhere other than where it was sent.
                if line.is_some_and(|asked| asked != landed) {
                    block.insert("relocated_from_line".into(), json!(line));
                }
            }
        }
    }
    Value::Object(block)
}

/// One place a quoted line could have meant: where it is, what it is called when it has a
/// name that works, and the line itself as evidence.
fn anchor_site_value(
    db: &ide::RootDatabaseImpl,
    roots: &WorkspaceRoots,
    site: &ide::AnchorSite,
) -> Value {
    let mut entry = Map::new();
    let place = ide::resolve_file_range(db, site.file_id, Some(site.range), site.enclosing_range);
    if let Some(range) = place.range.as_ref() {
        if let Some(preview) =
            preview_of(db, site.file_id, range, statement_floor(site.enclosing_range))
        {
            preview.insert_into(&mut entry);
        }
    }
    insert_resolved_location(&place, Some(roots), &mut entry);
    if let Some(symbol) = &site.symbol {
        entry.insert("symbol".into(), json!(symbol));
    }
    // Only where it is true: a flag that is present and false on every other place is a
    // field a reader stops looking at.
    if site.pointed_by_line {
        entry.insert("pointed_by_line".into(), json!(true));
    }
    Value::Object(entry)
}

/// Hang a preview on every reference the leftover budget still pays for, and report how
/// many went without.
///
/// Runs after the list and the histogram are both final, so the count of references, the
/// histogram and every location are exactly what the same request answers with previews
/// switched off.
fn attach_previews(
    db: &ide::RootDatabaseImpl,
    body: &mut Map<String, Value>,
    places: &[ide::ResolvedPlace],
    shown: &[&ReferenceHit],
    max_output_tokens: usize,
) -> usize {
    let Some(Value::Array(mut items)) = body.remove("references") else { return 0 };

    // The line that reports what did not fit is body too, and it is written once this loop
    // ends. Its widest form is known already — the count cannot exceed the records in hand —
    // so it comes out of the budget rather than landing on top of it.
    let max_output_tokens = max_output_tokens
        .saturating_sub(format!(r#","previews_omitted":{}"#, items.len()).len().div_ceil(4));

    // The body without any preview, measured once. Everything after this is a per-record
    // delta, so the accounting stays linear in the number of records shown.
    let base = body_bytes(body, &items);
    let mut spent = 0usize;
    let mut omitted = 0usize;
    let mut exhausted = false;
    for (index, item) in items.iter_mut().enumerate() {
        let (Some(place), Some(hit)) = (places.get(index), shown.get(index)) else { continue };
        let Some(range) = place.range.as_ref() else { continue };
        let Some(preview) =
            preview_of(db, hit.file_id, range, statement_floor(hit.enclosing_range))
        else {
            continue;
        };
        if exhausted {
            omitted += 1;
            continue;
        }
        let Some(entry) = item.as_object_mut() else { continue };
        // Measured, not estimated — and measured on THIS record, which is the only thing that
        // changed. An estimate misses two deterministic terms (the conditional flag keys, and
        // every quote or backslash a snippet escapes into two bytes); re-serializing the whole
        // body instead would be exact and quadratic, and `max_output_tokens` has no ceiling to
        // keep the record count small. A record's own delta is exact because a JSON array's
        // length is the sum of its elements' plus separators, and no separator moves here.
        let before = entry_bytes(entry);
        preview.insert_into(entry);
        spent += entry_bytes(entry) - before;
        // Once one preview does not fit, the rest go too: previews are of a size, and letting
        // a short one slip past a long one would publish a set whose membership depends on
        // how long the lines happen to be.
        if (base + spent).div_ceil(4) > max_output_tokens {
            exhausted = true;
            omitted += 1;
            let before = entry_bytes(entry);
            for key in PREVIEW_KEYS {
                entry.remove(key);
            }
            spent -= before - entry_bytes(entry);
        }
    }
    body.insert("references".into(), Value::Array(items));
    omitted
}

/// What the body costs in bytes with its reference list held apart.
fn body_bytes(body: &Map<String, Value>, items: &[Value]) -> usize {
    let rest = serde_json::to_string(body).map(|text| text.len()).unwrap_or(0);
    let list = serde_json::to_string(items).map(|text| text.len()).unwrap_or(0);
    rest + list + r#","references":"#.len()
}

/// What one record costs in bytes.
fn entry_bytes(entry: &Map<String, Value>) -> usize {
    serde_json::to_string(entry).map(|text| text.len()).unwrap_or(0)
}

/// How wide a preview may be, in the UTF-16 units the location contract counts columns in.
///
/// The number is `outline`'s; the UNIT is not. `cap_chars` there cuts Unicode scalars, and a
/// preview whose caller indexes it by `range.start_character` has to be cut in the unit that
/// column is published in — on the BMP the two agree, on a surrogate pair they part.
const PREVIEW_CAP: u32 = 200;

/// How much of the line before the occurrence a window keeps, so the name arrives with the
/// context that says what the line does.
const PREVIEW_LEAD: u32 = 40;

/// One line of source, ready to travel beside the occurrence it describes.
struct Preview {
    snippet: String,
    /// The UTF-16 column the snippet STARTS at, so the occurrence's own column indexes it.
    start_character: u32,
    truncated: bool,
    redacted: bool,
}

impl Preview {
    fn insert_into(&self, entry: &mut Map<String, Value>) {
        entry.insert("snippet".into(), json!(self.snippet));
        entry.insert("snippet_start_character".into(), json!(self.start_character));
        // Written only when true, the way `search` writes its own truncation counters: a
        // field that is always there and almost always false is a field a reader stops
        // looking at.
        if self.truncated {
            entry.insert("snippet_truncated".into(), json!(true));
        }
        if self.redacted {
            entry.insert("snippet_redacted".into(), json!(true));
        }
    }
}

/// The line an occurrence sits on, windowed around it and sanitized.
///
/// Read out of the resident's own text through `ide::line_text`, never off disk: the disk
/// may already be ahead of the revision this answer is signed with, and a preview from it
/// would describe a file the ranges beside it do not.
fn preview_of(
    db: &ide::RootDatabaseImpl,
    file_id: vfs::FileId,
    range: &line_index::LineColRange,
    // The earliest byte a statement covering this line can begin at — the enclosing method,
    // or the file. Redaction is armed by a marker standing before its literal in the same
    // statement, and BSL wraps long assignments freely: handed one physical line, the filter
    // never sees a `Пароль =` that ended up on the line above, and the secret goes out in
    // clear text. It is the caller's business to say where the statement can start, because
    // only the caller knows which method the occurrence sits in.
    context: u32,
) -> Option<Preview> {
    let (source, line_at) = ide::line_with_context(db, file_id, range.start_line, context)?;

    // Masked BEFORE the line is cut out of it, for the reason above and for one more: the
    // marker may also stand earlier on the same line, outside any window drawn around the
    // occurrence. What masking costs is the byte correspondence between the published columns
    // and the snippet, and `snippet_redacted` withdraws exactly that.
    //
    // Where the line and the occurrence END UP is asked of the masker, not guessed. Masking
    // moves them either way — left when the secret was longer than `***`, right when it was
    // shorter — and any search for the name near its old column settles on a namesake
    // whenever one stands closer than the shift, publishing a window around a different place
    // that reads as if it were the right one.
    let occurrence_at = line_at + byte_of_utf16_col(&source[line_at..], range.start_character);
    let (sanitized, moved) = crate::tools::redact::redact_secrets_tracking(
        &source,
        &[line_at, occurrence_at.min(source.len())],
    );
    let (text, line_from, occurrence_from) = (&sanitized, moved[0], moved[1]);

    // Trailing whitespace and the `\r` a CRLF line keeps go; leading whitespace never does,
    // because the published columns are counted from the start of the line.
    let line = text.get(line_from..)?.trim_end();
    // The flag describes THIS line, not the context it was masked in. A secret two lines
    // above changes the slice and not the caption, and a record marked redacted for that
    // would withdraw a byte correspondence its own line still keeps.
    let redacted = line != source.get(line_at..)?.trim_end();
    let start = utf16_len(&text[line_from..occurrence_from.clamp(line_from, text.len())]);

    // The occurrence's own text, so a window has something to aim at. Taken after masking,
    // because that is the text the snippet is cut from.
    let name_width = if range.end_line == range.start_line {
        range.end_character.saturating_sub(range.start_character)
    } else {
        utf16_len(line).saturating_sub(start)
    };

    let width = utf16_len(line);
    let window_start = if width <= PREVIEW_CAP {
        0
    } else {
        let lead = start.saturating_sub(PREVIEW_LEAD).min(width - PREVIEW_CAP);
        // A name wider than the window would be pushed off the end by its own lead, and a
        // preview without the occurrence in it is the plausible-looking wrong answer this
        // tool exists to end.
        if start.saturating_sub(lead) + name_width > PREVIEW_CAP {
            start.min(width)
        } else {
            lead
        }
    };
    let window_end = window_start.saturating_add(PREVIEW_CAP).min(width);

    let (start_character, snippet) = utf16_window(line, window_start, window_end);
    Some(Preview {
        snippet,
        start_character,
        truncated: window_start > 0 || window_end < width,
        redacted,
    })
}

/// The earliest byte a statement covering an occurrence can begin at.
///
/// A statement cannot cross a method boundary, so the method's own start is a tight and safe
/// floor; module-level code has none, and the file is the floor there.
fn statement_floor(enclosing: Option<syntax::TextRange>) -> u32 {
    enclosing.map_or(0, |range| u32::from(range.start()))
}

/// The byte offset a UTF-16 column names, clamped to the end of the text.
fn byte_of_utf16_col(text: &str, column: u32) -> usize {
    let mut units = 0u32;
    for (at, ch) in text.char_indices() {
        if units >= column {
            return at;
        }
        units += ch.len_utf16() as u32;
    }
    text.len()
}

fn utf16_len(text: &str) -> u32 {
    text.encode_utf16().count() as u32
}

/// The `[from, to)` slice of a line in UTF-16 units, with the column it actually starts at.
///
/// A boundary inside a surrogate pair snaps outward to the character that holds it, and the
/// column is reported as the snapped one: a start column that does not name where the text
/// begins would make the published offset index the wrong character.
fn utf16_window(line: &str, from: u32, to: u32) -> (u32, String) {
    let mut column = 0u32;
    let mut start_byte = line.len();
    let mut end_byte = line.len();
    let mut start_column = from;
    for (byte, ch) in line.char_indices() {
        if column >= from && start_byte == line.len() {
            start_byte = byte;
            start_column = column;
        }
        if column >= to {
            end_byte = byte;
            break;
        }
        column += ch.len_utf16() as u32;
    }
    if start_byte == line.len() {
        start_column = column;
    }
    (start_column, line[start_byte..end_byte.max(start_byte)].to_owned())
}

/// A line quoted as evidence rather than as a caption: what stands where the caller aimed.
/// Unconditional — evidence is not decoration and is not paid for out of the preview budget.
///
/// It carries the same two flags a preview does, and it has to. The field exists so a caller
/// can hold this line against its own buffer and see WHICH of the two pictures moved; a line
/// silently capped or silently masked turns that comparison into a false difference — a tail
/// that never existed, or `***` where the file has a literal.
fn evidence_line(db: &ide::RootDatabaseImpl, file_id: vfs::FileId, line: u32) -> Option<Preview> {
    // The file is the context here: this line is named by a coordinate and not by an
    // occurrence, so there is no method around it to bound the statement by. One such line is
    // quoted per refusal, so the cost is paid once.
    let (source, line_at) = ide::line_with_context(db, file_id, line, 0)?;
    let (sanitized, moved) = crate::tools::redact::redact_secrets_tracking(&source, &[line_at]);
    let line = sanitized.get(moved[0]..)?.trim_end();
    // As above: the flag is about this line, not about the statement it was masked within.
    let redacted = line != source.get(line_at..)?.trim_end();
    let width = utf16_len(line);
    let (start_character, snippet) = utf16_window(line, 0, PREVIEW_CAP);
    Some(Preview { snippet, start_character, truncated: width > PREVIEW_CAP, redacted })
}

/// The dictionary's own answer, under its own key: this tool's `total` counts
/// occurrences and the dictionary's counts candidates, and one key carrying two
/// incomparable numbers is the confusion `outcome` exists to end.
fn lookup_value(
    db: &ide::RootDatabaseImpl,
    roots: &WorkspaceRoots,
    lookup: &ide::NameLookupResult,
    budget: usize,
) -> (Value, loc::Completeness, bool) {
    let mut answer = NameAnswer::render(db, Some(roots), lookup);
    let before = answer.candidates.len();
    let cut = trim_items_to_budget(&mut answer.candidates, budget);
    // `truncated` is the dictionary's own promise that the list is complete
    // against `total`. Trimming the list here and leaving the flag alone would
    // break that promise from the outside, where nothing but the two counts
    // could betray it.
    answer.truncated |= answer.candidates.len() < before;
    let mut lookup_body = Map::new();
    let completeness = answer.insert_into(&mut lookup_body);
    (Value::Object(lookup_body), completeness, cut)
}

/// What the budget has left after the body already written — the same
/// ~4-chars-a-token unit the trimming measures in.
fn remaining_budget(body: &Map<String, Value>, max_output_tokens: usize) -> usize {
    max_output_tokens.saturating_sub(items_tokens(body))
}

/// How many distinct roots a declaration list spans — the question that decides
/// whether narrowing by root can separate them at all.
fn distinct_roots(declarations: &[Value]) -> usize {
    let mut seen: Vec<&str> = Vec::new();
    for declaration in declarations {
        let Some(root_id) = declaration["location"]["root_id"].as_str() else { continue };
        if !seen.contains(&root_id) {
            seen.push(root_id);
        }
    }
    seen.len()
}

/// The completeness details this tool writes. Named once because the preview reserve has to
/// measure the very strings the answer will carry: a reserve built from one spelling and an
/// envelope written with another is a bound that holds only by luck.
const RESULT_CAP_LIMIT: &str = "more references than `limit`; narrow with `area_root_id`/\
                                `area_path_prefix`/`kinds`, or walk `files`";
const RESULT_CAP_FILES: &str = "the walk stopped at `max_files`, so `total` is a lower bound; \
                                raise `max_files` or narrow the area before the walk";
const TRIMMED: &str = "trimmed to fit `max_output_tokens`";
const PREVIEWS_OMITTED: &str = "`max_output_tokens` left no room for some previews; the \
                                references themselves are all here";
const ANCHOR_CAP: &str = "the anchor was chosen from a capped candidate list, so the outcome \
                          itself may miss a declaration; pass a qualified `symbol` to decide \
                          it exactly";
const ANCHOR_CAP_TEXT: &str = "the anchor was chosen from a capped candidate list, so the \
                               outcome itself may miss a symbol; narrow `line_content` to \
                               text that names only what you mean";
const UNREADABLE: &str = "some workspace files could not be read and were left out of the walk";

/// Which capped-anchor detail this answer will carry. One function, because the preview
/// reserve has to weigh the very string the envelope will hold, and the two differ in length.
fn anchor_cap_detail(anchor: &ReferenceAnchor) -> &'static str {
    match anchor {
        ReferenceAnchor::Text { .. } => ANCHOR_CAP_TEXT,
        _ => ANCHOR_CAP,
    }
}

/// An upper bound on what `finish` still has to add to this body.
///
/// Built, not guessed: the widest revision and topology a `u64` can spell, a stale mark, and
/// exactly the completeness it is handed — which the caller has already filled with every
/// reason still to come. Counting one of those reasons again here would reserve for a line
/// written once, and the previews would pay for it.
fn envelope_tokens(completeness: &loc::Completeness) -> usize {
    let freshness = loc::Freshness::new(loc::FreshnessSource::Resident, completeness.clone())
        .with_revision(u64::MAX)
        .with_topology(u64::MAX)
        // `false` is the LONGER spelling of the two, and the bound has to be the widest form.
        .with_stale(false)
        .to_value();
    let key = r#","freshness":"#.len();
    serde_json::to_string(&freshness).map(|text| text.len() + key).unwrap_or(0).div_ceil(4)
}

/// Every key a preview writes, so one that did not fit can be taken back off the record it
/// was measured on.
const PREVIEW_KEYS: [&str; 4] =
    ["snippet", "snippet_start_character", "snippet_truncated", "snippet_redacted"];

fn items_tokens(body: &Map<String, Value>) -> usize {
    serde_json::to_string(body).map(|text| text.len()).unwrap_or(0).div_ceil(4)
}

fn hit_value(place: &ide::ResolvedPlace, roots: &WorkspaceRoots, hit: &ReferenceHit) -> Value {
    let mut entry = Map::new();
    insert_resolved_location(place, Some(roots), &mut entry);
    entry.insert("kind".into(), json!(hit.kind.as_str()));
    Value::Object(entry)
}

/// The default budget, so the tool and its schema quote one number.
pub(crate) const DEFAULT_BUDGET: usize = DEFAULT_OUTPUT_BUDGET_TOKENS;

/// One occurrence: where it is, what it is, and — with `include_preview` — the line it sits
/// on.
///
/// Typed, while `anchor`, `area`, `files[]`, `declarations[]`, `unsupported` and `lookup`
/// beside it stay `Value`. The reason is not that a place matters more: it is that a place
/// is the block a consumer ADDRESSES with — feed `root_id` and `path` back and you get the
/// file — so a renamed key inside it turns a working address into a plausible wrong one. The
/// blocks left untyped are read, not fed back. Their keys are unvalidated, and a gate that
/// reads this type must say so rather than let "not checked" pass for "checked".
#[derive(Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code, reason = "schema-only shape published by tools/list")]
pub(crate) struct ReferenceEntry {
    /// Where the occurrence is, under the location contract.
    location: Option<crate::tools::location::WireLocation>,
    /// Why there is no place, when there is none: a code from the location contract's own
    /// vocabulary. Never both this and `location`, and never neither.
    location_unavailable: Option<String>,
    /// What the occurrence is: `declaration` | `call` | `write` | `read`.
    kind: String,
    /// `include_preview` only: the source line, masked and capped.
    snippet: Option<String>,
    /// The UTF-16 column the snippet starts at, so `location.range` indexes into it.
    snippet_start_character: Option<u32>,
    /// The line was windowed around the occurrence rather than shown from its head.
    snippet_truncated: Option<bool>,
    /// A literal in the statement was masked, after which the columns no longer index the
    /// snippet byte for byte.
    snippet_redacted: Option<bool>,
}

/// What the tool serves: an answer, or the envelope that says the resident is not ready.
///
/// A union, not one flat object with optional fields. The two shapes are different facts:
/// an answer names an `outcome` and stamps `freshness`, while an envelope has neither —
/// there is no answer yet to be fresh or stale about. Publishing the envelope through the
/// answer's own type would mean either inventing those two values or making them optional
/// for real answers too, and a consumer could no longer tell "no occurrences" from "not
/// looked yet".
#[derive(Deserialize, JsonSchema, Serialize)]
#[serde(untagged)]
#[schemars(transform = crate::tools::symbol_info::union_as_one_of)]
#[allow(
    dead_code,
    clippy::large_enum_variant,
    reason = "schema-only union published by tools/list is never instantiated"
)]
pub(crate) enum ReferencesOutput {
    Answer(ReferencesResponse),
    Loading(ReferencesLoading),
}

/// The resident is still building, so there is no answer yet — only the lifecycle snapshot
/// a poller needs to tell progress from a stall.
#[derive(Deserialize, JsonSchema, Serialize)]
#[allow(dead_code, reason = "schema-only shape published by tools/list")]
pub(crate) struct ReferencesLoading {
    /// Version of this response shape. Always `"1"`.
    schema_version: LoadingSchemaVersion,
    /// What this envelope says about readiness: `loading` while a build runs, `failed` when
    /// the resident could not be built (with `error`, `retry_after_ms`, `attempts`,
    /// `failed_age_ms` beside it). The two values are how a consumer tells this envelope from
    /// an answer — and from an eternal wait.
    status: LoadingStatus,
    /// A human sentence beside the machine fields; never the thing to match on.
    detail: String,
    /// Resident lifecycle state: `disabled | idle | loading | ready | failed`.
    state: String,
    /// Which build generation this snapshot is from.
    generation: u64,
    /// How long the current build has been running.
    elapsed_ms: Option<u64>,
    /// Why the build failed, when it did.
    error: Option<String>,
    /// Milliseconds until the next attempt at a failed build; `None` while one is running.
    retry_after_ms: Option<u64>,
    /// Consecutive failed attempts behind the failure being retried, when there is one.
    attempts: Option<u64>,
    /// How long ago that failure was published.
    failed_age_ms: Option<u64>,
}

crate::wire_enum!(LoadingSchemaVersion { V1 => "1" });
// `loading` while a build runs; `failed` when the resident this envelope is about could not
// be built (it is retried on its backoff, and the surrounding fields say when). Publishing
// only `loading` would make the schema itself claim work that is not happening.
crate::wire_enum!(LoadingStatus {
    Loading => "loading",
    Failed => "failed",
});

/// The published schema: the union, not the answer alone.
pub(crate) fn references_output_schema(
) -> std::sync::Arc<serde_json::Map<String, serde_json::Value>> {
    let mut schema = (*rmcp::handler::server::tool::schema_for_type::<ReferencesOutput>()).clone();
    crate::contract::ensure_object_root(&mut schema);
    std::sync::Arc::new(schema)
}

/// The envelope itself, carrying the version its schema publishes.
pub(crate) fn loading(report: &crate::diagnostics_state::StatusReport) -> CallToolResult {
    let mut body = crate::tools::resident::loading_body(
        report,
        "резидентная база ссылок ещё строится, повторите запрос через несколько секунд",
    );
    body["schema_version"] = json!("1");
    structured(body)
}

#[derive(Deserialize, JsonSchema, Serialize)]
#[allow(
    dead_code,
    reason = "the type exists to publish `outputSchema`; the body it describes is built as a \
              `Value` so the location contract stays the only serializer of a place"
)]
pub(crate) struct ReferencesResponse {
    /// Version of this response shape. Always `"1"`.
    schema_version: String,
    /// What happened, from the closed vocabulary `resolved` | `ambiguous` | `not_found` |
    /// `unsupported_symbol` | `anchor_stale`.
    outcome: String,
    /// The `symbol` the request anchored on, echoed back.
    symbol: Option<String>,
    /// Which anchor the answer followed: `mode` from the closed vocabulary `symbol` |
    /// `position` | `line_content`; the `line` it stood on, whenever it stood anywhere — always
    /// for `position`, which stands where it was told to, and for `line_content` once the quote
    /// picked out one symbol; and `relocated_from_line` when the quoted text was found
    /// somewhere other than the line the request named. An `ambiguous` or `anchor_stale` answer
    /// by quote carries no `line`, because the anchor landed on nothing — which is what those
    /// two outcomes say.
    anchor: Option<Value>,
    /// `anchor_stale` only: why the quoted line anchors nothing any more — `reason` from the
    /// closed vocabulary `not_in_file` | `column_outside_quoted_text` |
    /// `no_name_in_quoted_text` | `name_not_resolved` — with `line_content_matches`, how many
    /// lines still carry the quote, and `actual_line`, what stands at the line that was asked
    /// for, flagged `actual_line_truncated`/`actual_line_redacted` when it was capped or
    /// masked, so holding it against your own buffer cannot show a difference the file does
    /// not have.
    anchor_stale: Option<Value>,
    /// `ambiguous` by quoted line: the places that line could have meant, each with a
    /// `location`, the line itself as evidence, a `symbol` when a qualified name addresses
    /// it, and `pointed_by_line` on the place the request's own `line` stood at. What
    /// `declarations` are to an ambiguity by name.
    anchor_sites: Option<Vec<Value>>,
    /// The area filter as it was applied, with how many files it selected.
    area: Option<Value>,
    /// `resolved` only: the occurrences, each with a `location` (or
    /// `location_unavailable`) and a `kind`. With `include_preview`, each also carries
    /// `snippet` and the `snippet_start_character` it begins at, plus `snippet_truncated`
    /// when the line was windowed and `snippet_redacted` when a literal was masked — after
    /// which the columns no longer index the snippet byte for byte.
    references: Option<Vec<ReferenceEntry>>,
    /// `resolved` only: how many occurrences passed the area and kind filters,
    /// counted before `limit`.
    total: Option<usize>,
    /// `resolved` only: the walk stopped at `max_files`, so `total` counts only
    /// what was walked.
    total_is_lower_bound: Option<bool>,
    /// `resolved` only: whether this answer may be compared with a narrower one.
    /// False once the walk was truncated: the two counted different candidate sets.
    narrowing_comparable: Option<bool>,
    /// `resolved` only: how many candidate files were walked.
    files_scanned: Option<usize>,
    /// `resolved` only: per-file counts over the whole `total`, most-populated
    /// first — what a caller walks instead of a cursor.
    files: Option<Vec<Value>>,
    /// `resolved` only: the histogram itself was cut by the output budget, so
    /// whole files are missing from it.
    histogram_truncated: Option<bool>,
    /// `include_preview` only: how many references went without a preview because the budget
    /// left after the answer itself ran out. The references are all there — a preview is a
    /// caption, and it is never paid for with the answer.
    previews_omitted: Option<usize>,
    /// `ambiguous` by qualified name: the declarations to choose between.
    declarations: Option<Vec<Value>>,
    /// How to turn an `ambiguous` answer into a resolved one.
    resolution_hint: Option<String>,
    /// `unsupported_symbol` only: what the name turned out to be — `category` from
    /// the closed vocabulary `common_module` | `metadata_object` |
    /// `metadata_member` | `form` | `platform_member` | `unknown_scope`, and why
    /// there is no list.
    unsupported: Option<Value>,
    /// `ambiguous`/`not_found` by name: the name dictionary's answer —
    /// `candidates`, its own `total`/`total_exact`/`truncated` over THEM, and the
    /// `providers` it could and could not consult. Nested because its `total`
    /// counts candidates while this tool's counts occurrences. The call graph is
    /// among the sources the anchor is looked for in, so a name only it holds can
    /// be anchored on here, and its state travels in `providers` rather than being
    /// reported as never consulted.
    lookup: Option<Value>,
    /// Who answered, at which revision and topology, and whether the answer is whole.
    freshness: Value,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn building_report() -> crate::diagnostics_state::StatusReport {
        crate::diagnostics_state::StatusReport {
            state: "loading",
            generation: 3,
            files: None,
            unread_files: None,
            reload: "none",
            elapsed_ms: Some(1_200),
            error: None,
            failed_age_ms: None,
            attempts: 0,
            retry_after_ms: None,
            watch: None,
        }
    }

    /// Every body this tool serves — at any outcome AND at any resident state — parses into
    /// the type it publishes, and the envelope parses into a branch of its OWN.
    ///
    /// The second half is what keeps the first honest. The cheap way to make a "still
    /// building" body parse is to bolt `schema_version`, `outcome` and a `freshness` onto the
    /// answer's flat shape; then the envelope validates while claiming an outcome nothing
    /// produced and a freshness for an answer that does not exist. This gate reads which
    /// branch matched, so that shortcut fails here instead of reaching a client.
    #[test]
    fn a_building_envelope_parses_as_an_envelope_and_not_as_an_answer() {
        let report = building_report();
        let served = loading(&report);
        let body = served.structured_content.expect("the envelope is structured content");

        let parsed = serde_json::from_value::<ReferencesOutput>(body.clone()).unwrap_or_else(|e| {
            panic!("the envelope must parse into the published type: {e}: {body}")
        });
        assert!(
            matches!(parsed, ReferencesOutput::Loading(_)),
            "the envelope is its own branch, not an answer wearing default values: {body}",
        );

        // The rejected shortcut, spelled out: the same envelope with the answer's required
        // keys bolted on. It must NOT read as an envelope any more.
        let mut disguised = body.clone();
        disguised["schema_version"] = json!("1");
        disguised["outcome"] = json!("not_found");
        disguised["freshness"] = json!({
            "source": "resident",
            "revision": 0,
            "topology_fingerprint": "0000000000000000",
            "stale": false,
            "completeness": {"status": "complete", "reasons": []},
        });
        let parsed = serde_json::from_value::<ReferencesOutput>(disguised.clone())
            .expect("the disguised body still parses — that is the point");
        assert!(
            matches!(parsed, ReferencesOutput::Answer(_)),
            "a body carrying an outcome and a freshness is an ANSWER by shape; if it still \
             reads as an envelope, the two are indistinguishable and the union proves nothing",
        );
    }

    /// The kinds this tool publishes are spelled the way `symbol_info` spells them.
    ///
    /// Two tools describing the same declaration must not teach a consumer two vocabularies:
    /// a client switching on `kind` cannot tell a word it has never seen from one that
    /// quietly changed spelling. The gate is set membership against the card's own list, so
    /// a kind added here without a place in that list fails, and so does a spelling that
    /// drifts on either side.
    #[test]
    fn every_published_declaration_kind_is_a_kind_the_card_publishes() {
        for kind in [ide::DeclarationKind::Method, ide::DeclarationKind::Variable] {
            let published = kind.symbol_kind();
            assert!(
                ide::SYMBOL_KINDS.contains(&published),
                "`{published}` is not among the kinds `symbol_info` serves ({:?}) — one of \
                 the two vocabularies moved without the other",
                ide::SYMBOL_KINDS,
            );
        }

        // Control: a spelling the card does not serve must fail this membership, or the
        // assertion above would hold for any string at all.
        assert!(
            !ide::SYMBOL_KINDS.contains(&"variable"),
            "the card calls a module-level variable `module variable`; if a bare `variable` \
             is also served, this gate can no longer catch the two spellings drifting apart",
        );
    }

    /// The per-file histogram is the one part of rendering that runs without a salsa
    /// query: a bucket names only its file, so `resolve_file_range` returns before it
    /// reads any text. The input says so — `limit: 0` empties the list of shown
    /// occurrences, whose own `resolve_file_range` DOES read text and would unwind on
    /// its own, hiding whether the histogram observes anything at all.
    #[test]
    fn the_histogram_observes_a_cancelled_request() {
        // Walking references moves the process-global span counter the cancellation
        // gates measure with; taking their gate keeps this test out of their numbers.
        let _serialised = crate::walk_probe::WALK_GATE.blocking_lock();
        use base_db::{SourceDatabase, SourceRoot, SourceRootId};
        use std::panic::AssertUnwindSafe;
        use vfs::{FileId, FileSet, VfsPath};

        let mut db = ide::RootDatabaseImpl::new();
        let module = FileId(0);
        let mut file_set = FileSet::default();
        file_set.insert(module, VfsPath::new("/ws/src/cf/CommonModules/Продажи/Ext/Module.bsl"));
        db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
        db.set_file_source_root(module, SourceRootId(0));
        db.set_file_text(module, "Процедура Расчёт() Экспорт\nКонецПроцедуры\n");

        let (roots, _) = bsl_search::WorkspaceRoots::build(
            std::path::Path::new("/ws"),
            std::path::Path::new("/ws/src/cf"),
            &[],
        );
        let request = ide::ReferencesRequest {
            anchor: ide::ReferenceAnchor::Name("Продажи.Расчёт".to_owned()),
            anchor_files: None,
            area: ide::ReferenceArea::default(),
            kinds: None,
            include_declaration: true,
            max_files: DEFAULT_MAX_FILES,
        };
        let result = ide::find_references_by_name(&db, &request, &[]);
        assert!(!result.per_file.is_empty(), "the stand must produce a bucket to render");
        let params = Params {
            symbol: Some("Продажи.Расчёт"),
            anchor_root_id: None,
            root_id: None,
            path: None,
            line: None,
            column: None,
            area_root_id: None,
            area_path_prefix: None,
            line_content: None,
            kinds: &[],
            include_declaration: None,
            limit: None,
            max_files: None,
            include_preview: None,
        };
        let anchor = ide::ReferenceAnchor::Name("Продажи.Расчёт".to_owned());

        salsa::Database::cancellation_token(&db).cancel();
        let caught = salsa::Cancelled::catch(AssertUnwindSafe(|| {
            render(&db, &roots, &result, &params, &anchor, None, 0, DEFAULT_BUDGET, 0)
        }));
        assert!(
            matches!(caught, Err(salsa::Cancelled::Local)),
            "the histogram was built for a cancelled request"
        );
    }

    /// The area filter walks EVERY resident file doing pure path arithmetic — not one
    /// salsa query in the loop — so a cancelled request is observed there by the
    /// checkpoint or not at all. Decisive for that reason: an unwind out of `select`
    /// can have come from nothing else.
    #[test]
    fn the_area_filter_observes_a_cancelled_request() {
        use crate::diagnostics_state::{DiagnosticsState, ResidentOutcome};
        use std::panic::AssertUnwindSafe;

        let dir = tempfile::tempdir().unwrap();
        crate::diagnostics_state::test_support::write_common_module(
            dir.path(),
            "Модуль",
            true,
            "&НаСервере\nПроцедура П() Экспорт\nКонецПроцедуры\n",
        );
        crate::diagnostics_state::test_support::write(
            dir.path(),
            "bsl-analyzer.toml",
            "[source]\nroot = \".\"\n",
        );
        let state = DiagnosticsState::for_workspace(dir.path().to_path_buf());
        state.ensure_loading();
        crate::diagnostics_state::test_support::wait_ready(&state);

        let out = state.read(|resident, _| {
            let db = resident.db().clone();
            salsa::Database::cancellation_token(&db).cancel();
            let caught = salsa::Cancelled::catch(AssertUnwindSafe(|| {
                select(resident, &db, None, Some("")).map(|s| s.files.len())
            }));
            matches!(caught, Err(salsa::Cancelled::Local))
        });
        match out {
            ResidentOutcome::Ready(unwound, _) => {
                assert!(unwound, "the area filter walked on with the request cancelled");
            }
            _ => panic!("expected Ready outcome"),
        }
    }

    /// A `root_id` a reader can find is not yet a `root_id` it may answer through (M6/M12).
    ///
    /// The resident's table is the one it was BUILT with, and an id survives its root's retarget: a
    /// link declared as an extension can lead elsewhere while the id — and the rows keyed by it —
    /// stays. A selection naming that id then addressed one file to the caller and another to the
    /// resident, so it is refused before the walk, with the fact that was observed named (the
    /// spelling no longer resolving to the registered directory — a retarget is one of its causes,
    /// an unreachable target another).
    ///
    /// The stand is the production one — a real workspace whose extension is declared through a
    /// link, a real `DiagnosticsState` over it — because the site reads the table the resident
    /// holds, not one a test hands it. The retarget happens INSIDE the session read lock: a reload
    /// needs the other side of that lock, so the resident cannot quietly re-register the root
    /// between the two selections.
    #[cfg(unix)]
    #[test]
    fn a_retargeted_root_id_is_refused_by_the_selection() {
        use crate::diagnostics_state::{test_support, DiagnosticsState, ResidentOutcome};

        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("ws");
        let targets = dir.path().join("targets");
        for (target, symbol) in [("one", "ПерваяЦель"), ("two", "ВтораяЦель")] {
            let root = targets.join(target);
            std::fs::create_dir_all(&root).unwrap();
            std::fs::write(
                root.join("Configuration.xml"),
                format!("<Configuration><Name>{target}</Name></Configuration>"),
            )
            .unwrap();
            test_support::write_common_module(
                &root,
                "Общий",
                true,
                &format!("&НаСервере\nФункция {symbol}() Экспорт Возврат 1; КонецФункции\n"),
            );
        }
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("Configuration.xml"), "<Configuration/>").unwrap();
        test_support::write_common_module(
            &workspace,
            "Конфигурация",
            true,
            "&НаСервере\nФункция Конфигурации() Экспорт Возврат 1; КонецФункции\n",
        );
        let link = workspace.join("ext");
        std::os::unix::fs::symlink(targets.join("one"), &link).unwrap();
        test_support::write(
            &workspace,
            "bsl-analyzer.toml",
            "[source]\nroot = \".\"\nextensions = [\"ext\"]\n",
        );

        let state = DiagnosticsState::for_workspace(workspace.clone());
        state.ensure_loading();
        test_support::wait_ready(&state);

        let outcome = state.read(|resident, _| {
            let db = resident.db().clone();
            let ext_id =
                resident.workspace_roots().ids().find(|id| !id.is_empty()).map(str::to_owned)?;
            let before = select(resident, &db, Some(ext_id.as_str()), None)
                .map(|selection| selection.root_id)
                .map_err(|error| error.message.to_string());
            // The link leads elsewhere now, and the table this resident holds was minted against
            // the old target.
            std::fs::remove_file(&link).unwrap();
            std::os::unix::fs::symlink(targets.join("two"), &link).unwrap();
            let after = select(resident, &db, Some(ext_id.as_str()), None)
                .map(|selection| selection.root_id)
                .map_err(|error| error.message.to_string());
            Some((ext_id, before, after))
        });
        let (ext_id, before, after) = match outcome {
            ResidentOutcome::Ready(pair, _) => pair.expect("the linked extension is registered"),
            _ => panic!("expected Ready outcome"),
        };

        assert_eq!(
            before,
            Ok(Some(ext_id.clone())),
            "control: the selection is answered while the registration is current",
        );
        let refusal = after.expect_err("a stale root must not be answered through");
        assert!(
            refusal.contains("can no longer be resolved") && refusal.contains(ext_id.as_str()),
            "the refusal must name the root and what was observed: {refusal}",
        );
    }

    /// The section of the tools document that describes this tool.
    ///
    /// The bound is not decoration: `call` and `read` occur throughout that file in other
    /// senses, so a gate reading the whole document would pass on a vocabulary this tool's
    /// own section never names — and that section is what a consumer writes its closed list
    /// from.
    fn references_section() -> &'static str {
        let document = include_str!("../../../../docs/mcp/TOOLS_AND_EXTENSION.md");
        let after_heading = document
            .split_once("\n## Ссылки на символ\n")
            .expect("the document describes this tool under its own heading")
            .1;
        match after_heading.split_once("\n## ") {
            Some((section, _)) => section,
            None => after_heading,
        }
    }

    /// Every field of this tool whose values are published as a closed vocabulary.
    ///
    /// The list is what makes the reading unambiguous: a vocabulary belongs to the field
    /// named NEAREST before the phrase that introduces it, so two vocabularies in one
    /// paragraph cannot be read into each other. A new field added without a row here
    /// publishes a vocabulary nothing checks — which is why the rows are named, not sniffed.
    const VOCABULARY_FIELDS: [&str; 4] = ["outcome", "category", "mode", "reason"];

    /// The codes a closed-vocabulary sentence lists for one field, read out of the text that
    /// publishes it.
    ///
    /// Found by the phrase that INTRODUCES the vocabulary and by the field standing closest
    /// before it — never by one of the values: an anchor on a live code cannot see a dead one
    /// written to the left of it, which is the same hard-wired knowledge of one value that a
    /// list of dead codes was.
    fn closed_vocabulary_of(text: &str, field: &str) -> Vec<String> {
        let mut codes = Vec::new();
        for phrase in ["closed vocabulary", "закрытый словарь"] {
            for at in text.match_indices(phrase).map(|(at, _)| at) {
                let lead_start =
                    text[..at].char_indices().rev().nth(120).map_or(0, |(index, _)| index);
                let lead = &text[lead_start..at];
                let nearest = VOCABULARY_FIELDS
                    .iter()
                    .filter_map(|candidate| lead.rfind(candidate).map(|at| (at, *candidate)))
                    .max_by_key(|(at, _)| *at);
                if nearest.map(|(_, name)| name) != Some(field) {
                    continue;
                }
                let mut rest = text[at + phrase.len()..].trim_start_matches(SEPARATORS);
                while let Some(stripped) = rest.strip_prefix('`') {
                    let Some((code, tail)) = stripped.split_once('`') else { break };
                    if code.is_empty() || !code.chars().all(|c| c.is_ascii_lowercase() || c == '_')
                    {
                        break;
                    }
                    codes.push(code.to_owned());
                    rest = tail.trim_start_matches(SEPARATORS);
                    let Some(next) = rest.strip_prefix('|') else { break };
                    rest = next.trim_start_matches(SEPARATORS);
                }
            }
        }
        codes
    }

    /// What may stand between two values of a published vocabulary: spaces, the line breaks
    /// of a document, the `\` and `n` a serialized schema writes them as, and the `///` a
    /// doc comment carries into it.
    const SEPARATORS: [char; 6] = [' ', '\n', '\r', '\\', 'n', '/'];

    /// A `ReferencesResult` that carries no answer — enough to read an anchor block out of
    /// `anchor_value`, which is the only place the anchor modes are spelled.
    fn no_answer() -> ReferencesResult {
        ReferencesResult {
            outcome: ReferencesOutcome::NotFound,
            body_source: BodySource::Resident,
            hits: Vec::new(),
            per_file: Vec::new(),
            declarations: Vec::new(),
            candidates: None,
            files_scanned: 0,
            files_capped: false,
            anchor_candidates_capped: false,
            anchor_line: None,
            anchor_sites: Vec::new(),
        }
    }

    /// Four closed vocabularies leave this tool on the wire, and their only worth over
    /// free-form strings is that a consumer may match on them. That holds exactly while the
    /// two texts a consumer reads — the section of the tools document and the published
    /// `outputSchema` — carry every value the surface serves and no value it does not.
    ///
    /// Set equality, not one inclusion: the forward half catches a value added to the code
    /// and forgotten in the text, and the backward half catches one left in the text (or in
    /// a doc comment that becomes the schema, and through it the published
    /// `output_schema_fingerprint`) after the surface stopped serving it. A closed
    /// vocabulary with an unreachable value teaches a state a client waits for forever.
    #[test]
    fn every_published_value_is_named_where_the_tool_is_described() {
        let section = references_section();
        let schema = rmcp::handler::server::tool::schema_for_type::<ReferencesResponse>();
        let published = serde_json::to_string(&schema).expect("the schema serializes");

        let outcomes: Vec<String> = [
            ReferencesOutcome::Resolved,
            ReferencesOutcome::Ambiguous,
            ReferencesOutcome::NotFound,
            ReferencesOutcome::UnsupportedSymbol {
                category: ide::UnsupportedCategory::UnknownScope,
            },
            ReferencesOutcome::AnchorStale {
                reason: ide::AnchorStaleReason::NotInFile,
                line_matches: 0,
            },
        ]
        .iter()
        .map(|outcome| outcome_str(outcome).to_owned())
        .collect();

        let categories: Vec<String> =
            ide::UnsupportedCategory::ALL.iter().map(|c| c.as_str().to_owned()).collect();
        let reasons: Vec<String> =
            ide::AnchorStaleReason::ALL.iter().map(|r| r.as_str().to_owned()).collect();

        // Read back out of the projection that spells them, not copied here: a mode this
        // test knows and `anchor_value` does not would be a vocabulary the gate invented.
        let modes: Vec<String> = [
            ReferenceAnchor::Name("Модуль.Метод".to_owned()),
            ReferenceAnchor::Position { file_id: vfs::FileId(0), line: 0, column: 0 },
            ReferenceAnchor::Text {
                file_id: vfs::FileId(0),
                line: None,
                column: None,
                content: "Метод".to_owned(),
            },
        ]
        .iter()
        .map(|anchor| {
            anchor_value(anchor, &no_answer())["mode"].as_str().expect("a mode").to_owned()
        })
        .collect();

        for (field, served) in
            [("outcome", outcomes), ("category", categories), ("reason", reasons), ("mode", modes)]
        {
            let mut served: Vec<String> = served;
            served.sort();
            served.dedup();
            for (origin, source) in
                [("the tool's section", section), ("the published schema", published.as_str())]
            {
                let mut listed = closed_vocabulary_of(source, field);
                listed.sort();
                listed.dedup();
                assert!(
                    !listed.is_empty(),
                    "the `{field}` vocabulary was not found in {origin}, so this gate read \
                     nothing there",
                );
                assert_eq!(
                    listed, served,
                    "the `{field}` vocabulary in {origin} is not the one the surface serves",
                );
            }
        }

        for kind in [
            ReferenceKind::Declaration,
            ReferenceKind::Call,
            ReferenceKind::Write,
            ReferenceKind::Read,
        ] {
            let code = kind.as_str();
            assert!(
                section.contains(&format!("`{code}`")),
                "kind `{code}` is served but the tool's section does not name it",
            );
            assert!(
                KIND_VOCABULARY.contains(code),
                "kind `{code}` is served but the published vocabulary omits it",
            );
        }
    }

    /// A window whose edge lands inside a surrogate pair snaps outward, and the column it
    /// reports is the one it actually starts at.
    ///
    /// The input is a line with an astral character in it, because that is the only input on
    /// which a UTF-16 window and a character window differ at all: over the BMP — every BSL
    /// identifier, Cyrillic included — the two agree, and a gate written on Cyrillic alone
    /// would pass on an implementation that counted the wrong unit.
    #[test]
    fn a_window_edge_inside_a_surrogate_pair_snaps_to_the_character() {
        // `𝕏` is one character and TWO UTF-16 units, so unit 1 falls in the middle of it.
        let line = "аб𝕏вг";
        assert_eq!(utf16_len(line), 6, "two ordinary characters, one pair, two more");

        let (start, text) = utf16_window(line, 3, 6);
        assert_eq!(start, 4, "the edge inside the pair moved past it, and said so");
        assert_eq!(text, "вг");

        let (start, text) = utf16_window(line, 2, 4);
        assert_eq!(start, 2, "an edge on a boundary stays put");
        assert_eq!(text, "𝕏");

        // The control: the same reading over a line with no pair in it, where a character
        // window and a UTF-16 window cannot disagree.
        let plain = "абвгд";
        assert_eq!(utf16_window(plain, 2, 4), (2, "вг".to_owned()));

        // The whole contract in one line: whatever comes back is the slice of the ORIGINAL
        // line starting at the column reported, in the units the column is counted in.
        for from in 0..=utf16_len(line) {
            for to in from..=utf16_len(line) {
                let (start, text) = utf16_window(line, from, to);
                let units: Vec<u16> = line.encode_utf16().collect();
                let taken: Vec<u16> = text.encode_utf16().collect();
                assert_eq!(
                    &units[start as usize..start as usize + taken.len()],
                    &taken[..],
                    "window {from}..{to} does not sit at the column it published",
                );
            }
        }
    }

    /// Gate I11 — which answers earn one forced rescan is decided by the ANSWER, and the
    /// list is exhaustive over the outcomes this surface serves.
    ///
    /// The cell that matters is the last one. `NotFound` is common to all three anchor
    /// forms, so a predicate written as "any miss" would switch a rescan on for
    /// `path`+`line`, where it has never been on — and a rescan can turn that miss into a
    /// hit, which changes an answer this stage promised not to touch.
    #[test]
    fn a_rescan_is_earned_by_the_outcome_and_not_by_the_shape_of_the_anchor() {
        let answer = |outcome: &str, symbol: Option<&str>| {
            let mut body = Map::new();
            body.insert("outcome".into(), json!(outcome));
            if let Some(symbol) = symbol {
                body.insert("symbol".into(), json!(symbol));
            }
            Answer {
                body,
                source: BodySource::Resident,
                completeness: loc::Completeness::complete(),
            }
        };

        assert!(
            warrants_rescan(&answer("anchor_stale", None)),
            "a quote the resident does not carry may be a resident that fell behind",
        );
        assert!(
            warrants_rescan(&answer("not_found", Some("Продажи.Расчёт"))),
            "a name nothing declares may be a name declared since the last scan",
        );
        assert!(
            !warrants_rescan(&answer("not_found", None)),
            "a positional miss says only that no name stands at those coordinates, and a \
             second look at the same coordinates says it again",
        );
        for settled in ["resolved", "ambiguous", "unsupported_symbol"] {
            assert!(
                !warrants_rescan(&answer(settled, Some("Продажи.Расчёт"))),
                "`{settled}` is an answer, not a miss",
            );
        }
    }

    /// Every kind the classifier can answer round-trips through the wire spelling — the one
    /// a `kinds` filter is written with. A value that serialized to a string the parser
    /// refuses would make its own filter unusable.
    #[test]
    fn every_kind_parses_back_from_its_wire_spelling() {
        for kind in [
            ReferenceKind::Declaration,
            ReferenceKind::Call,
            ReferenceKind::Write,
            ReferenceKind::Read,
        ] {
            assert_eq!(ReferenceKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(ReferenceKind::parse("handler"), None, "an undeclared kind is refused");
    }

    /// A workspace the resident could not read whole is not a workspace-wide enumeration,
    /// and the answer has to say so — `symbol_info` already stamps the same code on the same
    /// universe of files. The control is the identical answer with no unread files: same
    /// body, same anchor, one number apart.
    #[test]
    fn an_unread_file_makes_the_walk_partial() {
        // Walking references moves the process-global span counter the cancellation
        // gates measure with; taking their gate keeps this test out of their numbers.
        let _serialised = crate::walk_probe::WALK_GATE.blocking_lock();
        use base_db::{SourceDatabase, SourceRoot, SourceRootId};
        use vfs::{FileId, FileSet, VfsPath};

        let mut db = ide::RootDatabaseImpl::new();
        let module = FileId(0);
        let mut file_set = FileSet::default();
        file_set.insert(module, VfsPath::new("/ws/src/cf/CommonModules/Продажи/Ext/Module.bsl"));
        db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
        db.set_file_source_root(module, SourceRootId(0));
        db.set_file_text(module, "Процедура Расчёт() Экспорт\nКонецПроцедуры\n");

        let (roots, _) = bsl_search::WorkspaceRoots::build(
            std::path::Path::new("/ws"),
            std::path::Path::new("/ws/src/cf"),
            &[],
        );
        let request = ide::ReferencesRequest {
            anchor: ide::ReferenceAnchor::Name("Продажи.Расчёт".to_owned()),
            anchor_files: None,
            area: ide::ReferenceArea::default(),
            kinds: None,
            include_declaration: true,
            max_files: DEFAULT_MAX_FILES,
        };
        let result = ide::find_references_by_name(&db, &request, &[]);
        let params = Params {
            symbol: Some("Продажи.Расчёт"),
            anchor_root_id: None,
            root_id: None,
            path: None,
            line: None,
            column: None,
            area_root_id: None,
            area_path_prefix: None,
            line_content: None,
            kinds: &[],
            include_declaration: None,
            limit: None,
            max_files: None,
            include_preview: None,
        };

        let anchor = ide::ReferenceAnchor::Name("Продажи.Расчёт".to_owned());
        let read_whole =
            render(&db, &roots, &result, &params, &anchor, None, DEFAULT_LIMIT, DEFAULT_BUDGET, 0);
        assert!(
            read_whole.completeness.is_complete(),
            "control: nothing was held out of service: {:?}",
            read_whole.completeness.to_value(),
        );

        let with_a_hole =
            render(&db, &roots, &result, &params, &anchor, None, DEFAULT_LIMIT, DEFAULT_BUDGET, 1);
        let reasons = with_a_hole.completeness.to_value();
        assert!(
            reasons["reasons"]
                .as_array()
                .expect("reasons")
                .iter()
                .any(|reason| reason["code"] == "unreadable_files"),
            "a file the resident could not read is a gap in the walk: {reasons}",
        );
    }
}
