use super::types::search_schema_version;
use crate::tools::location as loc;
use crate::tools::response::structured_with_text;
use bsl_search::{EmbeddingFailure, FusedHit, LexicalHit, SearchHit, SemanticHit};
use rmcp::model::CallToolResult;
use serde_json::{json, Value};
use std::fmt::Write;
use std::path::Path;

/// Durable graph id for a code-search hit, when it names a method. Returns `None` for
/// headers and non-method symbols. Module-keyed methods (common/object/manager) resolve
/// regardless of root; form/command/file-module methods fall back to the
/// `method/file/<rel>::<name>` id the graph also mints.
///
/// The call graph keys file paths against `graph_root` (the repo root it was built from), but a
/// search hit's path is relative to the root that OWNS it — the configuration for most hits, an
/// extension's own directory for the rest. So we re-anchor the hit to an absolute path through
/// `roots`, then let [`ide::method_graph_id`] strip `graph_root` back down, yielding the same
/// prefix the graph minted so a form/file method id resolves in `graph` instead of `not_found`.
///
/// Anchoring with one root for every hit — the configuration's — is what this replaced: under it
/// an extension's file id named a path that exists in neither root.
///
/// (Module-keyed ids are prefix-independent, so re-anchoring does not touch them. They are also
/// root-blind in the graph itself, which this cannot fix from here: the graph keys a common
/// module by name, so a configuration module and an extension module of the same name share one
/// id whatever path they were anchored with.)
pub(super) fn graph_id_for_hit(
    hit: &SearchHit,
    roots: Option<&bsl_search::WorkspaceRoots>,
    graph_root: Option<&ide::StripRoot>,
) -> Option<String> {
    let symbol = original_symbol(hit);
    if symbol.is_empty() {
        return None;
    }
    let kind = hit.kind.to_lowercase();
    let is_method =
        ["proc", "func", "процед", "функц", "метод"].iter().any(|marker| kind.contains(marker));
    if !is_method {
        return None;
    }
    if Path::new(&hit.file_path).is_absolute() {
        return ide::method_graph_id(&hit.file_path, symbol, graph_root);
    }
    // Anchoring is possible only when the hit's own root is registered here. It need not be:
    // a hit read from a shared baseline carries the identifier the PUBLISHER's table gave it,
    // and that checkout may declare extensions this reader does not. Then the answer is the
    // rootless one below — a module-keyed id, never a path-keyed one — because a path built
    // from someone else's root is a wrong file dressed as a right one.
    //
    // The root's WALKED spelling anchors it, not its declared one. Both halves of an id are
    // facts about the generation that registered this table, and the declared spelling is the
    // one that can still move under a link afterwards.
    let anchored = roots.and_then(|roots| {
        roots.resolve_walked(&bsl_search::FileKey::new(&hit.root_id, &hit.file_path))
    });
    match anchored {
        Some(abs) => ide::method_graph_id(&abs.to_string_lossy(), symbol, graph_root),
        None => ide::method_graph_id(&hit.file_path, symbol, None)
            .filter(|id| !id.starts_with("method/file/")),
    }
}

fn original_symbol(hit: &SearchHit) -> &str {
    hit.source_span.as_ref().map_or(&hit.symbol_name, |span| &span.parent_symbol)
}

/// How many leading snippet lines the listing shows. The structured view mirrors exactly
/// these lines rather than the whole body: a consumer that needs the rest fetches it by path
/// or `graph source`, and the output budget stays predictable either way.
const SNIPPET_LINES: usize = 5;

/// What the envelope around the hits costs: `schema_version`, `shown`, `total`, the optional
/// `budget_exhausted` / `degraded` keys, the `freshness` block, and the hit array's own
/// brackets. An upper bound, not a measurement — the point is that the wrapper is charged,
/// not that it is charged exactly.
///
/// `freshness` alone is ~144 bytes empty and ~216 with a reason, so a response that carries
/// it must be charged for it, or it would pass the ceiling while still reporting
/// `budget_exhausted: false` — exactly what this constant exists to prevent. Only the FIXED
/// part is charged here: a reason's `detail` is as long as the note it repeats, and a flat
/// constant cannot cover a value that grows, so the caller holding the note charges for it
/// (see `hybrid::hits_budget`).
///
/// The reference profile's documentation actions emit no envelope at all
/// ([`Envelope::No`]), so charging them for it would shrink their listings for a block they
/// never produce. Hence two values, picked by the caller that knows which shape it renders.
pub(super) const ENVELOPE_OVERHEAD_BYTES: usize = 128;

/// [`ENVELOPE_OVERHEAD_BYTES`] plus the `freshness` block, for the responses that carry one.
pub(super) const ENVELOPE_OVERHEAD_WITH_FRESHNESS_BYTES: usize = 352;

/// One hit rendered in both views: the text block a person reads and the JSON object a machine
/// reads. Rendering the pair up front lets [`budgeted_hits`] charge the budget for what the
/// response actually carries, and keeps the two views from ever disagreeing on which hits made
/// the cut.
pub(super) struct HitBlock {
    text: String,
    json: Value,
}

/// A budgeted hit listing: the text (truncation note already appended), the parallel JSON
/// array, and the counts that let a structured consumer tell a short answer from a cut one.
pub(super) struct RenderedHits {
    pub(super) text: String,
    pub(super) hits: Vec<Value>,
    pub(super) shown: usize,
    pub(super) total: usize,
    pub(super) budget_exhausted: bool,
}

impl RenderedHits {
    /// The listing as a whole response, its own text serving as the mirror. Callers that wrap
    /// the listing in extra prose — a leading legend, a trailing degradation note — compose
    /// that text themselves and call [`hits_response`] directly.
    ///
    /// Used by the `reference` profile's documentation actions only, which are outside the
    /// location contract — hence no envelope.
    pub(super) fn into_response(mut self, action: &str) -> CallToolResult {
        let text = std::mem::take(&mut self.text);
        hits_response(text, self, None, Envelope::No, action, None)
    }
}

/// The leading snippet lines and how many were dropped, borrowed from `text` so the text and
/// JSON views render from one split rather than two.
fn snippet_head(text: &str) -> (Vec<&str>, usize) {
    let head: Vec<&str> = text.lines().take(SNIPPET_LINES).collect();
    let dropped = text.lines().count() - head.len();
    (head, dropped)
}

/// Append the snippet to both views of one hit. `snippet` is already redacted by the caller.
fn push_snippet(text: &mut String, json: &mut Value, snippet: &str) {
    let (head, dropped) = snippet_head(snippet);
    for line in &head {
        let _ = writeln!(text, "  │ {line}");
    }
    if dropped > 0 {
        let _ = writeln!(text, "  │ ... ({dropped} more lines)");
    }
    if head.is_empty() {
        return;
    }
    json["snippet"] = json!(head.join("\n"));
    if dropped > 0 {
        json["snippet_truncated_lines"] = json!(dropped);
    }
}

/// A relevance score rounded to the 3 decimals the text listing prints, so the structured view
/// carries the same number a reader sees rather than the f32's full expansion. It rounds
/// through the very formatter the listing uses: `{:.3}` breaks a tie to even, while arithmetic
/// rounding breaks it away from zero, and `0.0625` would then print `0.062` next to `0.063`.
fn rounded_score(score: f32) -> Value {
    let text = format!("{score:.3}");
    json!(text.parse::<f64>().unwrap_or(f64::from(score)))
}

/// A symbol name for the text listing; the structured view omits the field instead of
/// inventing this placeholder, because a chunk with no symbol is a file header, not a symbol
/// named `<header>`.
fn display_name(symbol_name: &str) -> &str {
    if symbol_name.is_empty() {
        "<header>"
    } else {
        symbol_name
    }
}

/// Keep as many leading hits as fit `max_output_tokens` (~4 chars/token), shrinking at hit
/// boundaries so neither a positionally-parsed text block nor a JSON object is cut mid-way,
/// then append a one-line truncation note when hits were dropped. The note goes AFTER the
/// hits, so a client parsing `graph_id:` lines positionally from the top is never shifted.
///
/// The budget covers the text and the JSON array together: the response carries both, so
/// charging only the text would overshoot the caller's ceiling by roughly the JSON's size.
/// A hit is kept only when the text+JSON pair fits. If even the first does not, the caller
/// emits the published empty budget envelope rather than an oversized partial entity.
fn budgeted_hits(
    blocks: Vec<HitBlock>,
    max_output_tokens: usize,
    envelope: Envelope,
) -> RenderedHits {
    let budget = max_output_tokens.saturating_mul(4);
    let total = blocks.len();
    // The hits are not the whole response: they get wrapped in [`hits_response`]'s envelope
    // keys and the `[` + `]` of their own array. Charging for that up front is what keeps
    // `budget_exhausted: false` from appearing on a response that is already over its ceiling.
    let mut used = match envelope {
        Envelope::Yes => ENVELOPE_OVERHEAD_WITH_FRESHNESS_BYTES,
        Envelope::No => ENVELOPE_OVERHEAD_BYTES,
    };
    let mut shown = 0usize;
    for (i, block) in blocks.iter().enumerate() {
        let json_len = serde_json::to_string(&block.json).map(|s| s.len()).unwrap_or(0);
        let sep = usize::from(i > 0); // comma between JSON items
        let next = used + block.text.len() + json_len + sep;
        if next > budget {
            break;
        }
        used = next;
        shown = i + 1;
    }

    let mut text = String::new();
    let mut hits = Vec::with_capacity(shown);
    for block in blocks.into_iter().take(shown) {
        text.push_str(&block.text);
        hits.push(block.json);
    }
    // An empty listing cannot overflow anything: the `[` + `]` framing alone must not raise the
    // flag when a caller passes a budget of zero.
    let budget_exhausted = shown < total || (shown > 0 && used > budget);
    if budget_exhausted {
        let _ = writeln!(
            text,
            "-- showing {shown} of {total} results (truncated to fit max_output_tokens; raise the budget or narrow the query) --"
        );
    }
    RenderedHits { text, hits, shown, total, budget_exhausted }
}

/// The text a hit-listing action emits when the ranked list is empty. Kept verbatim: it is
/// the sentence clients have parsed since before the structured envelope existed.
const NO_HITS_TEXT: &str = "No results found.";

/// A retry envelope whose text mirrors its JSON keeps mirroring it; one that speaks a sentence
/// (the documentation index's) keeps its sentence, and the failure travels in the JSON only.
pub(crate) fn not_ready_with_failure(
    mut result: CallToolResult,
    failure: Option<EmbeddingFailure>,
) -> CallToolResult {
    let Some(failure) = failure else { return result };
    let body = result.structured_content.as_mut().expect("structured retry envelope");
    let old_mirror = serde_json::to_string(body).expect("JSON serializes");
    body["semantic_failure"] = json!(failure);
    let mirror = serde_json::to_string(body).expect("JSON serializes");
    for content in &mut result.content {
        if let rmcp::model::ContentBlock::Text(text) = content {
            if text.text == old_mirror {
                text.text.clone_from(&mirror);
            }
        }
    }
    result
}

pub(crate) fn embedding_mcp_error(failure: EmbeddingFailure) -> rmcp::ErrorData {
    rmcp::ErrorData::internal_error(
        "Semantic embedding failed; use lexical search or retry.",
        Some(json!({"semantic_failure": failure})),
    )
}

/// Size the actual envelope (including every escaped note copy) before choosing hits.
/// Only failure responses need this extension of the existing soft minimum.
#[allow(clippy::too_many_arguments)] // The response carries the same independent fields as hits_response.
pub(super) fn failure_hits_response(
    blocks: Vec<HitBlock>,
    prefix: &str,
    degraded: Option<&str>,
    envelope: Envelope,
    action: &str,
    failure: EmbeddingFailure,
    max_output_tokens: usize,
    facts: Option<&WorkspaceFacts>,
) -> CallToolResult {
    let total = blocks.len();
    let budget = max_output_tokens.saturating_mul(4);
    let wrap = |shown, budget_exhausted| {
        let text = if total == 0 {
            NO_HITS_TEXT.to_owned()
        } else {
            let mut text = prefix.to_owned();
            if let Some(note) = degraded {
                let _ = writeln!(text, "-- {note} --");
            }
            if budget_exhausted {
                let _ = writeln!(text, "-- showing {shown} of {total} results (truncated to fit max_output_tokens; raise the budget or narrow the query) --");
            }
            text
        };
        let mut result = hits_response(
            text,
            RenderedHits { text: String::new(), hits: Vec::new(), shown, total, budget_exhausted },
            degraded,
            envelope,
            action,
            facts,
        );
        result.structured_content.as_mut().unwrap()["semantic_failure"] = json!(failure);
        result
    };
    let size = |result: &CallToolResult| {
        result.content[0].as_text().unwrap().text.len()
            + serde_json::to_vec(result.structured_content.as_ref().unwrap()).unwrap().len()
    };
    let mut shown = 0;
    let mut hit_bytes = 0usize;
    for (i, block) in blocks.iter().enumerate() {
        hit_bytes = hit_bytes
            .saturating_add(block.text.len())
            .saturating_add(serde_json::to_vec(&block.json).unwrap().len())
            .saturating_add(usize::from(i > 0));
        let candidate = wrap(i + 1, i + 1 < total);
        if size(&candidate).saturating_add(hit_bytes) <= budget {
            shown = i + 1;
        }
    }
    let empty = wrap(0, total > 0);
    let mut result = wrap(shown, shown < total || (shown == 0 && size(&empty) > budget));
    if shown > 0 {
        let mut text = prefix.to_owned();
        let mut hits = Vec::with_capacity(shown);
        for block in blocks.into_iter().take(shown) {
            text.push_str(&block.text);
            hits.push(block.json);
        }
        let suffix = result.content[0].as_text().unwrap().text.strip_prefix(prefix).unwrap();
        text.push_str(suffix);
        result.content = vec![rmcp::model::ContentBlock::text(text)];
        result.structured_content.as_mut().unwrap()["hits"] = json!(hits);
    }
    result
}

/// Wrap a rendered listing as a tool result: the text listing stays the content block, and the
/// same hits go out as `structuredContent` so a consumer reads fields instead of parsing
/// columns. `total` is the ranked list's length before the budget cut it — already bounded by
/// the request's `limit`, so it is not the corpus-wide match count. `degraded` names a modality
/// that could not serve (semantic down, index warming); it is carried structurally even when
/// the listing is empty, because "nothing found" and "nothing found with half the search
/// working" are different answers.
pub(super) fn hits_response(
    text: String,
    rendered: RenderedHits,
    degraded: Option<&str>,
    envelope: Envelope,
    action: &str,
    facts: Option<&WorkspaceFacts>,
) -> CallToolResult {
    let mut body = json!({
        "action": action,
        "schema_version": search_schema_version(action),
        "hits": rendered.hits,
        "shown": rendered.shown,
        "total": rendered.total,
    });
    if rendered.budget_exhausted {
        body["budget_exhausted"] = json!(true);
        body["budget_hint"] = json!("increase max_output_tokens or narrow the query");
    }
    if let Some(reason) = degraded {
        body["degraded"] = json!(reason);
    }
    if matches!(envelope, Envelope::Yes) {
        // The search index has NO revision or topology of its own, so those fields are
        // null and the source is named instead. Stamping the resident's or the graph's
        // identity here would be exactly the borrowed freshness this contract exists to
        // prevent: a hit may come from a shared baseline neither of them ever saw.
        let completeness = loc::Completeness::complete()
            .when(
                rendered.budget_exhausted,
                loc::ReasonCode::OutputBudget,
                "hits trimmed to fit max_output_tokens",
            )
            .when(
                degraded.is_some(),
                loc::ReasonCode::ModalityDegraded,
                degraded.unwrap_or_default(),
            );
        let mut freshness = loc::Freshness::new(
            loc::FreshnessSource::SearchIndex,
            facts.map_or(completeness.clone(), |facts| facts.completeness(completeness)),
        );
        if let Some(watch) = facts.and_then(|facts| facts.drift_watch) {
            freshness = freshness.with_drift_watch(watch);
        }
        body["freshness"] = freshness.to_value();
    }
    structured_with_text(text, body)
}

/// What the workspace index knows about its own currency when it answers: who watches the
/// workspace for it, what its overlay still has to read back, and whether anyone is doing so.
/// Each fact that makes the answer partial becomes a reason from the closed vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct WorkspaceFacts {
    pub(crate) drift_watch: Option<loc::DriftWatch>,
    /// Changed files marked and not yet read back.
    pub(crate) pending: usize,
    /// Changed files proven present whose bytes could not be read.
    pub(crate) unread: usize,
    /// Why nobody is reading the marks back, when nobody is: the backlog owner's state.
    pub(crate) backlog_stalled: Option<&'static str>,
    /// The poll standing in for the watch is overdue.
    pub(crate) poll_overdue: bool,
    /// The answer was served without the workspace overlay, so what it still owes is not
    /// known — and not zero.
    pub(crate) overlay_unknown: bool,
}

impl WorkspaceFacts {
    fn reasons(&self) -> Vec<(loc::ReasonCode, String)> {
        let mut reasons = Vec::new();
        if self.overlay_unknown {
            reasons.push((
                loc::ReasonCode::IndexBuilding,
                "answered without the workspace overlay: changed files not read back yet are \
                 not in this answer"
                    .to_owned(),
            ));
        }
        if self.pending > 0 {
            reasons.push((
                loc::ReasonCode::IndexBuilding,
                format!(
                    "the workspace overlay is catching up: {} changed file(s) not read back yet",
                    self.pending
                ),
            ));
        }
        if self.drift_watch == Some(loc::DriftWatch::Starting) {
            reasons.push((
                loc::ReasonCode::ModalityDegraded,
                "workspace changes do not reach the index yet: its consumer is still starting"
                    .to_owned(),
            ));
        }
        if self.drift_watch == Some(loc::DriftWatch::Unobserved) {
            reasons.push((
                loc::ReasonCode::ModalityDegraded,
                "workspace changes are no longer tracked: the search consumer has stopped"
                    .to_owned(),
            ));
        }
        if let (Some(state), true) =
            (self.backlog_stalled, self.pending > 0 || self.overlay_unknown)
        {
            reasons.push((
                loc::ReasonCode::ModalityDegraded,
                format!(
                    "the overlay backlog is not being processed ({state}); the changed files \
                     wait for a fresh change"
                ),
            ));
        }
        if self.poll_overdue {
            reasons.push((
                loc::ReasonCode::ModalityDegraded,
                "the poll that stands in for the file watch is overdue".to_owned(),
            ));
        }
        if self.unread > 0 {
            reasons.push((
                loc::ReasonCode::UnreadableFiles,
                format!("{} changed file(s) could not be read", self.unread),
            ));
        }
        reasons
    }

    fn completeness(&self, base: loc::Completeness) -> loc::Completeness {
        self.reasons()
            .into_iter()
            .fold(base, |completeness, (code, detail)| completeness.when(true, code, detail))
    }

    /// What these facts add to the envelope, so the hit budget is charged for it.
    pub(crate) fn envelope_bytes(&self) -> usize {
        let watch = self
            .drift_watch
            .map_or(0, |watch| ",\"drift_watch\":\"\"".len() + watch.as_str().len());
        let reasons: usize = self
            .reasons()
            .iter()
            .map(|(code, detail)| {
                ",{\"code\":\"\",\"detail\":\"\"}".len() + code.as_str().len() + detail.len() * 2
            })
            .sum();
        watch + reasons
    }
}

/// Whether this response carries the contract envelope. The doc-search actions of the
/// `reference` profile share these two functions and are out of the contract's scope, so
/// the choice is made by the caller rather than guessed from the payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Envelope {
    Yes,
    No,
}

/// The empty-listing result, with the same envelope a populated one carries so a consumer
/// never has to tell "no hits" from "no structured output" by absence.
pub(super) fn no_hits_response(
    degraded: Option<&str>,
    envelope: Envelope,
    action: &str,
    facts: Option<&WorkspaceFacts>,
) -> CallToolResult {
    hits_response(
        NO_HITS_TEXT.to_owned(),
        RenderedHits {
            text: String::new(),
            hits: Vec::new(),
            shown: 0,
            total: 0,
            budget_exhausted: false,
        },
        degraded,
        envelope,
        action,
        facts,
    )
}

/// A code hit's place under the location contract.
///
/// The pair is COPIED from the hit, never re-derived: a hit read from a shared baseline
/// carries the identifier the publisher's table gave it, and this reader's table may not know
/// that root at all. Handing the hit's relative path to `key_of_path` instead would attach it
/// to the configuration root, so a configuration hit and an extension hit sharing a relative
/// path would both come back as the configuration's — while the hit's own `root_id` field kept
/// saying otherwise.
///
/// Ranges are line-granular: the search index stores no columns, so `range` (a symbol's name)
/// is never emitted and `enclosing_range` covers the chunk's whole lines. A header chunk gets
/// no ranges at all — its end line is computed by a different rule than a method chunk's, and
/// publishing it as an end-exclusive line would be a number this contract does not mean.
fn hit_location(hit: &SearchHit) -> Result<loc::Location, loc::LocationUnavailable> {
    if Path::new(&hit.file_path).is_absolute() {
        return Err(loc::LocationUnavailable::AbsolutePathWithoutPair);
    }
    let location = loc::Location::from_key(&hit.root_id, &hit.file_path);
    if original_symbol(hit).is_empty() {
        return Ok(location);
    }
    Ok(location.with_enclosing_range(Some(loc::PositionRange {
        start_line: hit.line_start,
        start_character: 0,
        end_line: hit.line_end,
        end_character: 0,
    })))
}

fn enrich_platform_reference(json: &mut Value, kind: &str, title: &str) {
    let Some(reference) = crate::tools::platform::platform_reference_for_document(kind, title)
    else {
        return;
    };
    json["reference_id"] = json!(reference.reference_id);
    json["owner"] = json!(reference.owner);
    json["name"] = json!(reference.name);
    json["english_name"] = json!(reference.english_name);
    json["description"] = json!(reference.description);
}

pub(super) fn format_code_hits(
    hits: &[FusedHit],
    roots: Option<&bsl_search::WorkspaceRoots>,
    max_output_tokens: usize,
) -> RenderedHits {
    budgeted_hits(code_hit_blocks(hits, roots), max_output_tokens, Envelope::Yes)
}

pub(super) fn code_hit_blocks(
    hits: &[FusedHit],
    roots: Option<&bsl_search::WorkspaceRoots>,
) -> Vec<HitBlock> {
    // The strip base is the root table's, not the daemon's live workspace path: the table came
    // with the index answer being rendered and read the disk once, when that index was
    // registered. Re-reading the workspace spelling here would strip an indexed generation's
    // paths against the tree as it stands now — and a workspace reached through a retargeted
    // link then mints nothing at all.
    //
    // Base and anchor therefore come from ONE table, which is what can be guaranteed from here:
    // which generation that table belongs to is settled by whoever assembled the hits.
    //
    // It follows that hits arriving with no table get no base either, which is what the
    // rootless branch of `graph_id_for_hit` already assumes: those come from a foreign
    // baseline, and this workspace's root is not theirs to be stripped against.
    let graph_root = roots.map(|roots| ide::StripRoot::pinned(roots.workspace_canonical()));
    let graph_root = graph_root.as_ref();
    let blocks = hits
        .iter()
        .enumerate()
        .map(|(i, fused)| {
            let rank = i + 1;
            let hit = &fused.hit;
            let symbol = original_symbol(hit);
            let mut text = String::new();
            // An extension repeats the configuration's directory layout wholesale, so the same
            // relative path under two roots is ordinary rather than exotic. The owning root is
            // therefore part of a hit's identity, not decoration: without it two hits read
            // identically and a caller cannot tell which file it is looking at. The
            // configuration's id is the reserved empty string and prints nothing.
            let root_marker =
                if hit.root_id.is_empty() { String::new() } else { format!("[{}] ", hit.root_id) };
            let _ = writeln!(
                text,
                "#{} [{}] {}{}:{}-{} :: {} ({})",
                rank,
                fused.modality.tag(),
                root_marker,
                hit.file_path,
                hit.line_start + 1,
                hit.line_end,
                display_name(symbol),
                hit.kind,
            );
            let mut json = json!({
                "rank": rank,
                "modality": fused.modality.tag(),
                "root_id": hit.root_id,
                "path": hit.file_path,
                "line_start": hit.line_start + 1,
                "line_end": hit.line_end,
                "kind": hit.kind,
            });
            if !symbol.is_empty() {
                json["symbol"] = json!(symbol);
            }
            if let Some(span) = &hit.source_span {
                json["source_span"] = json!({
                    "byte_start": span.byte_start,
                    "byte_end": span.byte_end,
                    "parent_byte_start": span.parent_byte_start,
                    "parent_byte_end": span.parent_byte_end,
                    "part_index": span.part_index,
                    "part_count": span.part_count,
                });
            }
            enrich_platform_reference(&mut json, &hit.kind, &hit.symbol_name);
            if let Some(id) = graph_id_for_hit(hit, roots, graph_root) {
                let _ = writeln!(text, "  graph_id: {id}");
                json["graph_id"] = json!(id);
            }
            match hit_location(hit) {
                Ok(location) => {
                    json["location"] = location.to_value();
                }
                Err(reason) => {
                    json["location_unavailable"] = json!(reason.code());
                }
            }
            push_snippet(&mut text, &mut json, &crate::tools::redact::redact_secrets(&hit.text));
            text.push('\n');
            HitBlock { text, json }
        })
        .collect();
    blocks
}

pub(super) fn format_doc_hits(hits: &[SearchHit], max_output_tokens: usize) -> RenderedHits {
    budgeted_hits(doc_hit_blocks(hits), max_output_tokens, Envelope::No)
}

pub(super) fn doc_hit_blocks(hits: &[SearchHit]) -> Vec<HitBlock> {
    let blocks = hits
        .iter()
        .enumerate()
        .map(|(i, hit)| {
            let rank = i + 1;
            let mut text = String::new();
            let _ =
                writeln!(text, "#{} [{:.3}] {} ({})", rank, hit.score, hit.symbol_name, hit.kind);
            let mut json = json!({
                "rank": rank,
                "score": rounded_score(hit.score),
                "path": hit.file_path,
                "line_start": hit.line_start + 1,
                "line_end": hit.line_end,
                "kind": hit.kind,
            });
            if !hit.symbol_name.is_empty() {
                json["symbol"] = json!(hit.symbol_name);
            }
            enrich_platform_reference(&mut json, &hit.kind, &hit.symbol_name);
            push_snippet(&mut text, &mut json, &hit.text);
            text.push('\n');
            HitBlock { text, json }
        })
        .collect();
    blocks
}

pub(super) fn format_lexical_doc_hits(
    hits: &[LexicalHit],
    max_output_tokens: usize,
) -> RenderedHits {
    budgeted_hits(lexical_doc_hit_blocks(hits), max_output_tokens, Envelope::No)
}

pub(super) fn lexical_doc_hit_blocks(hits: &[LexicalHit]) -> Vec<HitBlock> {
    let blocks = hits
        .iter()
        .enumerate()
        .map(|(i, hit)| {
            let rank = i + 1;
            let mut text = String::new();
            let _ = writeln!(
                text,
                "#{} [{:.3}] {}:{}-{} :: {} ({})",
                rank,
                hit.rank,
                hit.path,
                hit.line_start + 1,
                hit.line_end,
                display_name(&hit.symbol_name),
                hit.kind,
            );
            let mut json = json!({
                "rank": rank,
                "score": rounded_score(hit.rank),
                "path": hit.path,
                "line_start": hit.line_start + 1,
                "line_end": hit.line_end,
                "kind": hit.kind,
            });
            if !hit.symbol_name.is_empty() {
                json["symbol"] = json!(hit.symbol_name);
            }
            enrich_platform_reference(&mut json, &hit.kind, &hit.symbol_name);
            push_snippet(&mut text, &mut json, &hit.text);
            text.push('\n');
            HitBlock { text, json }
        })
        .collect();
    blocks
}

pub(super) fn format_semantic_doc_hits(
    hits: &[SemanticHit],
    max_output_tokens: usize,
) -> RenderedHits {
    let blocks = hits
        .iter()
        .enumerate()
        .map(|(i, hit)| {
            let rank = i + 1;
            let mut text = String::new();
            let _ = writeln!(
                text,
                "#{} [{:.3}] {}:{}-{} :: {} ({})",
                rank,
                hit.score,
                hit.path,
                hit.line_start + 1,
                hit.line_end,
                display_name(&hit.symbol_name),
                hit.kind,
            );
            text.push('\n');
            let mut json = json!({
                "rank": rank,
                "score": rounded_score(hit.score),
                "path": hit.path,
                "line_start": hit.line_start + 1,
                "line_end": hit.line_end,
                "kind": hit.kind,
            });
            if !hit.symbol_name.is_empty() {
                json["symbol"] = json!(hit.symbol_name);
            }
            HitBlock { text, json }
        })
        .collect();
    budgeted_hits(blocks, max_output_tokens, Envelope::No)
}

pub(super) fn format_baseline_ref(baseline: &bsl_search::BaselineRef) -> String {
    if let Some(snapshot_id) = &baseline.snapshot_id {
        return format!("snapshot {}", snapshot_id.0);
    }
    if let (Some(branch), Some(commit)) = (&baseline.branch, &baseline.commit) {
        return format!("branch {branch} @ {commit}");
    }
    if let Some(branch) = &baseline.branch {
        return format!("branch {branch}");
    }
    if let Some(commit) = &baseline.commit {
        return format!("commit {commit}");
    }
    format!("latest {}", baseline.corpus.as_str())
}

#[cfg(test)]
mod tests {
    use super::super::test_support::code_hit;
    use super::{
        format_code_hits, format_doc_hits, graph_id_for_hit, no_hits_response, Envelope,
        WorkspaceFacts,
    };
    use bsl_search::{FusedHit, Modality, SearchHit};
    use rmcp::model::CallToolResult;
    use serde_json::json;
    use std::path::Path;

    fn fused(symbol: &str, modality: Modality) -> FusedHit {
        FusedHit { hit: code_hit("CommonModules/М/Ext/Module.bsl", symbol, "procedure"), modality }
    }

    #[test]
    fn token_part_render_preserves_original_symbol_span_and_budget() {
        let mut part = fused("ПроверитьИНН (часть 2)", Modality::Both);
        part.hit.source_span = Some(bsl_search::SourceSpan {
            parent_symbol: "ПроверитьИНН".into(),
            byte_start: 20,
            byte_end: 40,
            parent_byte_start: 0,
            parent_byte_end: 60,
            part_index: 2,
            part_count: 3,
        });
        let out = format_code_hits(&[part.clone()], None, usize::MAX);
        let hit = &out.hits[0];
        assert_eq!(hit["symbol"], "ПроверитьИНН");
        assert_eq!(
            hit["source_span"],
            json!({
                "byte_start": 20, "byte_end": 40,
                "parent_byte_start": 0, "parent_byte_end": 60,
                "part_index": 2, "part_count": 3,
            })
        );
        assert_eq!(
            hit["graph_id"],
            json!(graph_id_for_hit(&fused("ПроверитьИНН", Modality::Both).hit, None, None))
        );
        assert!(!out.text.contains("часть 2"));
        let cut = format_code_hits(&[part], None, out.text.len().div_ceil(4));
        assert_eq!(cut.shown, 0, "source_span JSON contributes to the output budget");
        let legacy =
            format_code_hits(&[fused("ПроверитьИНН", Modality::Lexical)], None, usize::MAX);
        assert!(legacy.hits[0].get("source_span").is_none());
    }

    #[test]
    fn payload_mcp_contract_failure_budget_counts_the_complete_escaped_envelope() {
        use super::{code_hit_blocks, doc_hit_blocks, failure_hits_response};
        use bsl_search::{EmbeddingFailure, EmbeddingFailureCode};
        let failure = EmbeddingFailure {
            code: EmbeddingFailureCode::EmbeddingInputTooLarge,
            request_bytes: Some(usize::MAX),
            max_request_bytes: Some(1_048_576),
        };
        let mut hits: Vec<_> =
            (0..5).map(|i| fused(&format!("Проверка{i}"), Modality::Lexical)).collect();
        for hit in &mut hits {
            hit.hit.text = "Юникод\\\"\n\t".repeat(8);
        }
        // Long notes exercise all three copies and JSON escaping, not just ASCII size.
        for note in [None, Some("semantic skipped: embedding failed"), Some("\\\"\n\tЮникод")]
        {
            for envelope in [Envelope::Yes, Envelope::No] {
                let docs: Vec<_> = hits.iter().map(|hit| hit.hit.clone()).collect();
                let blocks = || {
                    if envelope == Envelope::Yes {
                        code_hit_blocks(&hits, None)
                    } else {
                        doc_hit_blocks(&docs)
                    }
                };
                let action = if envelope == Envelope::Yes { "search_code" } else { "find_docs" };
                let prefix = if envelope == Envelope::Yes { "Modality: [L] lexical\n" } else { "" };
                let render = |budget| {
                    failure_hits_response(
                        blocks(),
                        prefix,
                        note,
                        envelope,
                        action,
                        failure,
                        budget,
                        None,
                    )
                };
                let size = |result: &rmcp::model::CallToolResult| {
                    result.content[0].as_text().unwrap().text.len()
                        + serde_json::to_vec(result.structured_content.as_ref().unwrap())
                            .unwrap()
                            .len()
                };
                let minimum = render(0);
                let floor = size(&minimum);
                let mut saw_full = false;
                let mut saw_partial = false;
                for budget in (0..=2_500).step_by(7) {
                    let result = render(budget);
                    let body = result.structured_content.as_ref().unwrap();
                    assert_eq!(body["semantic_failure"], json!(failure));
                    assert_eq!(body["shown"], body["hits"].as_array().unwrap().len());
                    if budget * 4 >= floor {
                        assert!(size(&result) <= budget * 4, "{action}, {budget}: {body}");
                    } else {
                        assert_eq!(body["hits"], json!([]));
                        assert_eq!(body["budget_exhausted"], true);
                        assert_eq!(result, minimum);
                    }
                    saw_full |= body["shown"] == hits.len();
                    saw_partial |=
                        body["shown"].as_u64().unwrap() > 0 && body["budget_exhausted"] == true;
                }
                assert!(saw_full && saw_partial);
            }
        }
    }

    #[test]
    fn payload_mcp_contract_failure_keeps_each_envelope_text_form() {
        use super::not_ready_with_failure;
        use bsl_search::{EmbeddingFailure, EmbeddingFailureCode};
        let failure = EmbeddingFailure::new(EmbeddingFailureCode::EmbeddingTimeout);
        let docs = super::super::status::docs_not_ready("find_docs");
        let sentence = docs.content[0].as_text().unwrap().text.clone();
        let docs = not_ready_with_failure(docs, Some(failure));
        assert_eq!(docs.content[0].as_text().unwrap().text, sentence, "the sentence stays");
        assert_eq!(docs.structured_content.as_ref().unwrap()["semantic_failure"], json!(failure));
        let code = not_ready_with_failure(
            super::super::status::baseline_warming_not_ready(&bsl_search::IndexProgress::new()),
            Some(failure),
        );
        assert_eq!(
            code.content[0].as_text().unwrap().text,
            serde_json::to_string(code.structured_content.as_ref().unwrap()).unwrap(),
            "a mirrored envelope keeps mirroring"
        );
    }

    #[test]
    fn payload_mcp_contract_failure_below_one_hit_does_not_claim_no_results() {
        use super::{code_hit_blocks, failure_hits_response};
        use bsl_search::{EmbeddingFailure, EmbeddingFailureCode};
        let hits: Vec<_> = (0..3).map(|i| fused(&format!("Метод{i}"), Modality::Lexical)).collect();
        let result = failure_hits_response(
            code_hit_blocks(&hits, None),
            "Modality: [L] lexical\n",
            Some("semantic skipped: embedding failed"),
            Envelope::Yes,
            "search_code",
            EmbeddingFailure::new(EmbeddingFailureCode::EmbeddingInputTooLarge),
            0,
            None,
        );
        let text = &result.content[0].as_text().unwrap().text;
        let body = result.structured_content.as_ref().unwrap();
        assert_eq!((body["shown"].clone(), body["total"].clone()), (json!(0), json!(3)));
        assert_ne!(text, "No results found.", "ranked hits exist: {text}");
        assert!(text.contains("showing 0 of 3"), "{text}");
    }

    #[test]
    fn payload_mcp_contract_empty_failure_preserves_the_soft_minimum() {
        use super::failure_hits_response;
        use bsl_search::{EmbeddingFailure, EmbeddingFailureCode};
        let failure = EmbeddingFailure::new(EmbeddingFailureCode::EmbeddingProviderError);
        for action in ["find_docs", "search_code"] {
            let render = |budget| {
                failure_hits_response(
                    Vec::new(),
                    "",
                    None,
                    Envelope::No,
                    action,
                    failure,
                    budget,
                    None,
                )
            };
            let full = render(usize::MAX);
            let bytes = full.content[0].as_text().unwrap().text.len()
                + serde_json::to_vec(full.structured_content.as_ref().unwrap()).unwrap().len();
            assert!(full.structured_content.as_ref().unwrap().get("budget_exhausted").is_none());
            assert_eq!(render(bytes.div_ceil(4)), full);
            let tiny = render(0);
            let body = tiny.structured_content.as_ref().unwrap();
            assert_eq!(body["semantic_failure"], json!(failure));
            assert_eq!(body["budget_exhausted"], true);
            assert_eq!(body["shown"], 0);
            assert_eq!(tiny.content[0].as_text().unwrap().text, "No results found.");
        }
    }

    #[test]
    fn format_code_hits_shows_modality_tag() {
        let hits = vec![
            fused("Оба", Modality::Both),
            fused("Лекс", Modality::Lexical),
            fused("Сем", Modality::Semantic),
        ];
        let out = format_code_hits(&hits, None, usize::MAX);

        assert!(out.text.contains("#1 [L+S]"), "both-modality hit tagged L+S: {}", out.text);
        assert!(out.text.contains("#2 [L]"), "lexical-only hit tagged L: {}", out.text);
        assert!(out.text.contains("#3 [S]"), "semantic-only hit tagged S: {}", out.text);
        assert_eq!(out.hits[0]["modality"], "L+S");
        assert_eq!(out.hits[1]["modality"], "L");
        assert_eq!(out.hits[2]["modality"], "S");
    }

    #[test]
    fn hits_from_different_roots_are_told_apart() {
        // A `cfe` extension repeats the configuration's directory layout wholesale, so the same
        // relative path under two roots is the normal case, not a corner one. Two hits that read
        // identically are two hits a caller cannot act on.
        let mut configuration =
            code_hit("CommonModules/Общий/Ext/Module.bsl", "Обработать", "procedure");
        configuration.root_id = String::new();
        let mut extension =
            code_hit("CommonModules/Общий/Ext/Module.bsl", "Обработать", "procedure");
        extension.root_id = "ext-a".to_owned();
        let hits = vec![
            FusedHit { hit: configuration, modality: Modality::Lexical },
            FusedHit { hit: extension, modality: Modality::Lexical },
        ];

        let out = format_code_hits(&hits, None, usize::MAX);

        assert_eq!(out.hits[0]["root_id"], "", "the configuration keeps the reserved empty id");
        assert_eq!(out.hits[1]["root_id"], "ext-a", "the extension's hit names its root");
        // The location must not re-derive the root: both hits share a relative path, so a
        // table lookup would attach BOTH to the configuration and quietly disagree with the
        // hit's own `root_id` above.
        assert_eq!(out.hits[0]["location"]["root_id"], "");
        assert_eq!(out.hits[1]["location"]["root_id"], "ext-a");
        assert_eq!(
            out.hits[0]["location"]["path"], out.hits[1]["location"]["path"],
            "one relative path, two roots — the pair is what separates them",
        );
        assert!(
            out.text.contains("ext-a"),
            "the human-readable listing distinguishes them too: {}",
            out.text,
        );
    }

    #[test]
    fn code_hit_structure_carries_every_field_the_listing_prints() {
        let mut hit = code_hit("CommonModules/Утилиты/Ext/Module.bsl", "ПроверитьИНН", "procedure");
        hit.text = (1..=7).map(|i| format!("строка {i}")).collect::<Vec<_>>().join("\n");
        hit.line_start = 180;
        hit.line_end = 201;
        let hits = vec![FusedHit { hit, modality: Modality::Lexical }];

        let out = format_code_hits(&hits, None, usize::MAX);

        assert_eq!(
            out.hits[0],
            json!({
                "rank": 1,
                "modality": "L",
                "root_id": "",
                "path": "CommonModules/Утилиты/Ext/Module.bsl",
                "line_start": 181,
                "line_end": 201,
                "symbol": "ПроверитьИНН",
                "kind": "procedure",
                "graph_id": "method/common/Утилиты/ПроверитьИНН",
                "snippet": "строка 1\nстрока 2\nстрока 3\nстрока 4\nстрока 5",
                "snippet_truncated_lines": 2,
                // The contract location beside the legacy 1-based line pair: same chunk,
                // 0-based and end-exclusive, with no columns because the index has none.
                "location": {
                    "root_id": "",
                    "path": "CommonModules/Утилиты/Ext/Module.bsl",
                    "enclosing_range": {
                        "start_line": 180,
                        "start_character": 0,
                        "end_line": 201,
                        "end_character": 0,
                    },
                    "position_encoding": "utf-16",
                    "schema_version": "1",
                },
            }),
        );
        // The listing and the structure describe one answer: every value above is on screen.
        assert!(
            out.text.contains(
                "#1 [L] CommonModules/Утилиты/Ext/Module.bsl:181-201 :: ПроверитьИНН (procedure)"
            ),
            "{}",
            out.text
        );
        assert!(
            out.text.contains("  graph_id: method/common/Утилиты/ПроверитьИНН"),
            "{}",
            out.text
        );
        assert!(out.text.contains("  │ ... (2 more lines)"), "{}", out.text);
        assert!(!out.budget_exhausted);
        assert_eq!((out.shown, out.total), (1, 1));
    }

    #[test]
    fn code_hit_structure_omits_absent_symbol_graph_id_and_snippet() {
        let hits = vec![fused("", Modality::Semantic)];

        let out = format_code_hits(&hits, None, usize::MAX);

        assert_eq!(
            out.hits[0],
            json!({
                "rank": 1,
                "modality": "S",
                "root_id": "",
                "path": "CommonModules/М/Ext/Module.bsl",
                "line_start": 1,
                "line_end": 1,
                "kind": "procedure",
                // A header chunk still has a place — the file — but no ranges: its end line
                // is computed by another rule, and publishing it as end-exclusive would be a
                // number this contract does not mean.
                "location": {
                    "root_id": "",
                    "path": "CommonModules/М/Ext/Module.bsl",
                    "position_encoding": "utf-16",
                    "schema_version": "1",
                },
            }),
        );
        // The text needs a name in the column; the structure says "no symbol" by absence
        // rather than inventing the placeholder as a value.
        assert!(out.text.contains(":: <header> (procedure)"), "{}", out.text);
    }

    #[test]
    fn budget_covers_the_text_and_the_structure_together() {
        let hits: Vec<FusedHit> =
            (1..=5).map(|i| fused(&format!("Процедура{i}"), Modality::Lexical)).collect();

        let full = format_code_hits(&hits, None, usize::MAX);
        assert_eq!((full.shown, full.total), (5, 5));
        assert!(!full.budget_exhausted);

        // A budget that fits the text of all five but not the text plus the JSON array must
        // drop hits: the response carries both, so charging for the text alone would overshoot.
        let text_only_tokens = full.text.len().div_ceil(4);
        let cut = format_code_hits(&hits, None, text_only_tokens);
        assert!(cut.shown < 5, "structure must be charged too: shown={}", cut.shown);
        assert_eq!(cut.hits.len(), cut.shown);
        assert_eq!(cut.total, 5);
        assert!(cut.budget_exhausted);
        assert!(
            cut.text.contains(&format!("-- showing {} of 5 results", cut.shown)),
            "{}",
            cut.text
        );
    }

    #[test]
    fn one_oversized_hit_returns_the_empty_budget_envelope() {
        let hits = vec![fused("Процедура", Modality::Lexical)];

        let out = format_code_hits(&hits, None, 1);

        assert_eq!((out.shown, out.total), (0, 1));
        assert!(out.hits.is_empty());
        assert!(out.budget_exhausted, "an over-budget single hit must still say so");
    }

    #[test]
    fn doc_hit_structure_rounds_the_score_the_listing_prints() {
        let hits = vec![SearchHit {
            source_span: None,
            collection: "platform".to_owned(),
            root_id: bsl_search::CONFIGURATION_ROOT_ID.to_owned(),
            file_path: "Массив.html".to_owned(),
            symbol_name: "Массив".to_owned(),
            kind: "type".to_owned(),
            text: "Описание".to_owned(),
            line_start: 0,
            line_end: 4,
            score: 0.123_456_7,
        }];

        let out = format_doc_hits(&hits, usize::MAX);

        assert_eq!(out.hits[0]["score"], json!(0.123));
        assert!(out.text.contains("#1 [0.123] Массив (type)"), "{}", out.text);
        assert_eq!(out.hits[0]["snippet"], "Описание");
        assert_eq!(out.hits[0]["path"], "Массив.html");
    }

    #[test]
    fn score_ties_round_the_same_way_in_both_views() {
        // 0.0625 is exact in binary and lands exactly halfway at 3 decimals: `{:.3}` breaks the
        // tie to even (0.062), arithmetic rounding breaks it away from zero (0.063).
        let hits = vec![SearchHit {
            source_span: None,
            collection: "platform".to_owned(),
            root_id: bsl_search::CONFIGURATION_ROOT_ID.to_owned(),
            file_path: "Массив.html".to_owned(),
            symbol_name: "Массив".to_owned(),
            kind: "type".to_owned(),
            text: String::new(),
            line_start: 0,
            line_end: 1,
            score: 0.0625,
        }];

        let out = format_doc_hits(&hits, usize::MAX);

        assert!(out.text.contains("[0.062]"), "{}", out.text);
        assert_eq!(out.hits[0]["score"], json!(0.062));
    }

    fn reasons_of(result: &CallToolResult) -> Vec<(String, String)> {
        let body = result.structured_content.as_ref().expect("structured envelope");
        body["freshness"]["completeness"]["reasons"]
            .as_array()
            .unwrap()
            .iter()
            .map(|reason| {
                (
                    reason["code"].as_str().unwrap().to_owned(),
                    reason["detail"].as_str().unwrap().to_owned(),
                )
            })
            .collect()
    }

    /// `search_code` says who watches the workspace for the index and what its overlay still
    /// owes, each fact as a reason from the closed vocabulary — no new code is needed.
    #[test]
    fn search_code_names_the_watch_and_the_overlay_debt() {
        use crate::tools::location::DriftWatch;
        let catching_up = WorkspaceFacts {
            drift_watch: Some(DriftWatch::Watching),
            pending: 2,
            unread: 1,
            ..Default::default()
        };
        let result = no_hits_response(None, Envelope::Yes, "search_code", Some(&catching_up));
        let body = result.structured_content.clone().unwrap();
        assert_eq!(body["freshness"]["drift_watch"], "watching");
        assert_eq!(body["freshness"]["completeness"]["status"], "partial");
        let codes: Vec<String> = reasons_of(&result).into_iter().map(|(code, _)| code).collect();
        assert_eq!(codes, ["index_building", "unreadable_files"]);

        let unobserved =
            WorkspaceFacts { drift_watch: Some(DriftWatch::Unobserved), ..Default::default() };
        let result = no_hits_response(None, Envelope::Yes, "search_code", Some(&unobserved));
        assert_eq!(
            result.structured_content.as_ref().unwrap()["freshness"]["drift_watch"],
            "unobserved"
        );
        assert_eq!(reasons_of(&result)[0].0, "modality_degraded");

        let quiet =
            WorkspaceFacts { drift_watch: Some(DriftWatch::Watching), ..Default::default() };
        let result = no_hits_response(None, Envelope::Yes, "search_code", Some(&quiet));
        assert_eq!(
            result.structured_content.as_ref().unwrap()["freshness"]["completeness"]["status"],
            "complete"
        );
    }

    /// A backlog that is catching up and one nobody is processing are different answers: the
    /// first is an index still building, the second a subsystem down — distinguishable by the
    /// reason, not only by prose.
    #[test]
    fn a_stalled_backlog_reads_differently_from_one_catching_up() {
        use crate::tools::location::DriftWatch;
        let running = WorkspaceFacts {
            drift_watch: Some(DriftWatch::Watching),
            pending: 3,
            ..Default::default()
        };
        let stalled =
            WorkspaceFacts { backlog_stalled: Some("its retry budget ran out"), ..running };
        let running =
            reasons_of(&no_hits_response(None, Envelope::Yes, "search_code", Some(&running)));
        let stalled =
            reasons_of(&no_hits_response(None, Envelope::Yes, "search_code", Some(&stalled)));
        assert_eq!(
            running.iter().map(|(code, _)| code.as_str()).collect::<Vec<_>>(),
            ["index_building"]
        );
        assert!(stalled
            .iter()
            .any(|(code, detail)| code == "modality_degraded"
                && detail.contains("not being processed")));
    }

    /// Every answer passes the schema its profile publishes: `search_code` with the watch and
    /// its reasons under the workspace's, the documentation actions under both — and a
    /// `search_code` answer is not something the reference profile's schema admits.
    #[test]
    fn answers_pass_the_schema_their_profile_publishes() {
        use crate::tools::location::DriftWatch;
        let valid = |schema: std::sync::Arc<serde_json::Map<String, serde_json::Value>>,
                     body: &serde_json::Value| {
            jsonschema::validator_for(&serde_json::Value::Object((*schema).clone()))
                .expect("a usable schema")
                .is_valid(body)
        };
        let workspace = crate::tools::search::search_output_schema;
        let reference = crate::tools::search::reference_search_output_schema;
        let facts = WorkspaceFacts {
            drift_watch: Some(DriftWatch::Polling),
            pending: 3,
            backlog_stalled: Some("its retry budget ran out"),
            poll_overdue: true,
            unread: 1,
            overlay_unknown: true,
        };
        let mut code = no_hits_response(None, Envelope::Yes, "search_code", Some(&facts));
        let indexing = crate::indexing::Indexing::single(crate::indexing::Target::unknown(
            crate::indexing::Kind::Semantic,
        ));
        indexing.attach(&mut code);
        let code = code.structured_content.unwrap();
        assert!(valid(workspace(), &code), "{code}");
        assert!(!valid(reference(), &code), "the reference schema admits a v6 answer: {code}");
        for action in ["find_docs", "search_docs"] {
            let mut docs = no_hits_response(None, Envelope::No, action, None);
            crate::indexing::Indexing::single(crate::indexing::Target::unknown(
                crate::indexing::Kind::Reference,
            ))
            .attach(&mut docs);
            let docs = docs.structured_content.unwrap();
            assert!(valid(workspace(), &docs) && valid(reference(), &docs), "{docs}");
        }
    }

    /// An index whose consumer has not started applying workspace changes cannot vouch for
    /// them: the answer is partial, as the graph's is stale in the same state.
    #[test]
    fn an_index_whose_watch_is_starting_is_not_complete() {
        use crate::tools::location::DriftWatch;
        let facts =
            WorkspaceFacts { drift_watch: Some(DriftWatch::Starting), ..Default::default() };
        let result = no_hits_response(None, Envelope::Yes, "search_code", Some(&facts));
        let body = result.structured_content.clone().unwrap();
        assert_eq!(body["freshness"]["drift_watch"], "starting");
        assert_eq!(body["freshness"]["completeness"]["status"], "partial", "{body}");
        assert_eq!(reasons_of(&result)[0].0, "modality_degraded");
    }

    /// An answer served without the workspace overlay does not know what the overlay owes, and
    /// says so instead of claiming nothing is owed.
    #[test]
    fn an_answer_without_the_overlay_is_not_complete() {
        use crate::tools::location::DriftWatch;
        let facts = WorkspaceFacts {
            drift_watch: Some(DriftWatch::Watching),
            overlay_unknown: true,
            backlog_stalled: Some("its retry budget ran out"),
            ..Default::default()
        };
        let result = no_hits_response(None, Envelope::Yes, "search_code", Some(&facts));
        let body = result.structured_content.clone().unwrap();
        assert_eq!(body["freshness"]["completeness"]["status"], "partial");
        let codes: Vec<String> = reasons_of(&result).into_iter().map(|(code, _)| code).collect();
        assert_eq!(codes, ["index_building", "modality_degraded"]);
    }

    /// Reference answers carry no envelope, and a `search_code` answer without workspace facts
    /// writes no `drift_watch`: nothing outside the workspace sources changed.
    #[test]
    fn answers_without_workspace_facts_are_unchanged() {
        let docs = no_hits_response(None, Envelope::No, "find_docs", None);
        let body = docs.structured_content.unwrap();
        assert!(body.get("freshness").is_none());
        assert_eq!(body["schema_version"], "6");
        let code = no_hits_response(None, Envelope::Yes, "search_code", None);
        let body = code.structured_content.unwrap();
        assert!(body["freshness"].get("drift_watch").is_none());
        assert_eq!(body["schema_version"], "8");
    }

    #[test]
    fn empty_listing_still_carries_the_envelope() {
        let result = no_hits_response(
            Some("semantic skipped: runtime initialization failed"),
            Envelope::Yes,
            "search_code",
            None,
        );

        assert_eq!(result.content[0].as_text().expect("text").text, "No results found.");
        let body = result.structured_content.expect("structured envelope");
        assert_eq!(body["hits"], json!([]));
        assert_eq!(body["total"], 0);
        assert_eq!(body["degraded"], "semantic skipped: runtime initialization failed");
        assert!(body.get("budget_exhausted").is_none(), "nothing was cut: {body}");
        // A degraded modality is incompleteness, and an empty listing is exactly where it
        // would otherwise be indistinguishable from "there is nothing to find".
        assert_eq!(body["freshness"]["source"], "search-index");
        assert_eq!(body["freshness"]["completeness"]["status"], "partial");
        assert_eq!(body["freshness"]["completeness"]["reasons"][0]["code"], "modality_degraded");
        // The search index has no revision of its own and must not borrow one.
        assert!(body["freshness"]["revision"].is_null());
        assert!(body["freshness"]["topology_fingerprint"].is_null());
    }

    /// A hit is bridged against the roots ITS index generation registered, not against the
    /// workspace as the disk spells it by the time the listing is rendered.
    ///
    /// Both halves of a `method/file/<rel>::<name>` id are facts about one generation: the path
    /// is where that index's walk found the file, and the base is the workspace that walk
    /// descended from. Reading either off the live tree pairs them across generations — and a
    /// workspace reached through a link that has since been retargeted then yields no rel at
    /// all, so the hit loses the bridge it had a moment earlier.
    #[cfg(unix)]
    #[test]
    fn a_hit_is_bridged_against_the_roots_its_index_was_registered_with() {
        let dir = tempfile::tempdir().unwrap();
        let served = dir.path().join("served");
        let elsewhere = dir.path().join("elsewhere");
        let form = "CommonForms/Форма/Ext/Form/Module.bsl";
        std::fs::create_dir_all(served.join("cf").join(form).parent().unwrap()).unwrap();
        std::fs::write(served.join("cf").join(form), "").unwrap();
        std::fs::create_dir_all(&elsewhere).unwrap();
        let workspace = dir.path().join("workspace");
        std::os::unix::fs::symlink(&served, &workspace).unwrap();

        let (roots, rejected) =
            bsl_search::WorkspaceRoots::build(&workspace, &workspace.join("cf"), &[]);
        assert!(rejected.is_empty(), "the stand declares no extensions");
        let hits = vec![FusedHit {
            hit: code_hit(form, "ПриОткрытии", "procedure"),
            modality: Modality::Lexical,
        }];
        let expected = format!("method/file/cf/{form}::ПриОткрытии");

        let before = format_code_hits(&hits, Some(&roots), usize::MAX);
        assert_eq!(
            before.hits[0]["graph_id"], expected,
            "control: a form hit carries the path-fallback id the graph mints",
        );

        std::fs::remove_file(&workspace).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &workspace).unwrap();

        let after = format_code_hits(&hits, Some(&roots), usize::MAX);
        assert_eq!(
            after.hits[0]["graph_id"], expected,
            "these hits still belong to the index that registered those roots",
        );
    }

    /// The table a workspace with one declared extension has. Built from paths that need not
    /// exist: a root that resolves to nothing keeps its declared spelling, so anchoring answers
    /// from those, and the stand is about which root a hit is anchored with, not about what is
    /// on disk.
    fn roots_with_an_extension() -> bsl_search::WorkspaceRoots {
        let (roots, rejected) = bsl_search::WorkspaceRoots::build(
            Path::new("/repo"),
            Path::new("/repo/src/cf"),
            &[std::path::PathBuf::from("/repo/ext-a")],
        );
        assert!(rejected.is_empty(), "the extension is beside the configuration, not inside it");
        roots
    }

    /// A file-keyed id carries the path, so it is the only place where anchoring is visible at
    /// all — a module-keyed id looks the same whichever root it was built from, because the
    /// graph keys common modules by name. Anchored with the configuration, an extension's form
    /// module would be named `src/cf/<путь расширения>`: a path that exists in neither root.
    #[test]
    #[cfg_attr(
        windows,
        ignore = "the fixture spells POSIX absolute paths, which are not absolute on Windows"
    )]
    fn an_extension_hit_is_anchored_with_its_own_root() {
        let roots = roots_with_an_extension();
        let graph_root = ide::StripRoot::pinned(Path::new("/repo"));
        let mut hit = code_hit(
            "Catalogs/Контрагенты/Forms/Форма/Ext/Form/Module.bsl",
            "ПриОткрытии",
            "procedure",
        );
        hit.root_id = "ext-a".to_owned();

        assert_eq!(
            graph_id_for_hit(&hit, Some(&roots), Some(&graph_root)),
            Some(
                "method/file/ext-a/Catalogs/Контрагенты/Forms/Форма/Ext/Form/Module.bsl::ПриОткрытии"
                    .to_owned()
            ),
        );
    }

    /// A hit whose root this workspace does not declare — the ordinary case when the baseline
    /// was published from a checkout with more extensions. Anchoring it with the configuration
    /// would mint a path-keyed id for a file that is not there; the honest answer is the same
    /// one given with no table at all.
    #[test]
    fn a_hit_of_an_unregistered_root_is_not_anchored_at_all() {
        let roots = roots_with_an_extension();
        let graph_root = ide::StripRoot::pinned(Path::new("/repo"));
        let mut form = code_hit(
            "Catalogs/Контрагенты/Forms/Форма/Ext/Form/Module.bsl",
            "ПриОткрытии",
            "procedure",
        );
        form.root_id = "ext-неизвестное".to_owned();
        let mut module =
            code_hit("CommonModules/Утилиты/Ext/Module.bsl", "ПроверитьИНН", "procedure");
        module.root_id = "ext-неизвестное".to_owned();

        assert_eq!(
            graph_id_for_hit(&form, Some(&roots), Some(&graph_root)),
            None,
            "no path-keyed id is invented for a root this workspace has never seen",
        );
        assert_eq!(
            graph_id_for_hit(&module, Some(&roots), Some(&graph_root)),
            Some("method/common/Утилиты/ПроверитьИНН".to_owned()),
            "the module-keyed id still resolves: it never depended on a root",
        );
    }

    /// With no table but a base in hand, both arms answer exactly as they did before roots
    /// existed. This is a preserving guard, and it guards a real loss: an absolute hit path
    /// yields a file-keyed id here, and folding that arm into the rootless one would take it
    /// away.
    ///
    /// The two are separate arguments and this is where that shows. The listing derives its
    /// base FROM the table, so a listing with no table hands no base either — hits that reach
    /// it that way come from an external baseline serving while the engine is still building,
    /// and a path built from this workspace's root is not theirs to carry.
    #[test]
    #[cfg_attr(
        windows,
        ignore = "the fixture spells POSIX absolute paths, which are not absolute on Windows"
    )]
    fn without_a_root_table_both_spellings_answer_as_before() {
        let graph_root = ide::StripRoot::pinned(Path::new("/repo"));
        let mut absolute = code_hit(
            "/repo/src/cf/Catalogs/Контрагенты/Forms/Форма/Ext/Form/Module.bsl",
            "ПриОткрытии",
            "procedure",
        );
        absolute.root_id = "ext-a".to_owned();
        let mut relative =
            code_hit("CommonModules/Утилиты/Ext/Module.bsl", "ПроверитьИНН", "procedure");
        relative.root_id = "ext-a".to_owned();

        assert_eq!(
            graph_id_for_hit(&absolute, None, Some(&graph_root)),
            Some(
                "method/file/src/cf/Catalogs/Контрагенты/Forms/Форма/Ext/Form/Module.bsl::ПриОткрытии"
                    .to_owned()
            ),
            "an absolute hit path names its own file and keeps its file-keyed id",
        );
        assert_eq!(
            graph_id_for_hit(&relative, None, Some(&graph_root)),
            Some("method/common/Утилиты/ПроверитьИНН".to_owned()),
            "and a relative one still gets the module-keyed id, with no path guessed",
        );
    }

    #[test]
    #[cfg_attr(
        windows,
        ignore = "the fixture spells POSIX absolute paths, which are not absolute on Windows"
    )]
    fn graph_id_bridges_method_hits_in_modules() {
        let roots = roots_with_an_extension();
        let engine_root = &roots;
        let graph_root = ide::StripRoot::pinned(Path::new("/repo"));

        assert_eq!(
            graph_id_for_hit(
                &code_hit("CommonModules/Утилиты/Ext/Module.bsl", "ПроверитьИНН", "procedure"),
                Some(engine_root),
                Some(&graph_root),
            ),
            Some("method/common/Утилиты/ПроверитьИНН".to_owned()),
        );
        assert_eq!(
            graph_id_for_hit(
                &code_hit("CommonModules/Утилиты/Ext/Module.bsl", "МодульнаяПерем", "variable"),
                Some(engine_root),
                Some(&graph_root),
            ),
            None,
        );
        assert_eq!(
            graph_id_for_hit(
                &code_hit(
                    "Catalogs/Контрагенты/Forms/Форма/Ext/Form/Module.bsl",
                    "ПриОткрытии",
                    "procedure",
                ),
                Some(engine_root),
                Some(&graph_root),
            ),
            Some(
                "method/file/src/cf/Catalogs/Контрагенты/Forms/Форма/Ext/Form/Module.bsl::ПриОткрытии"
                    .to_owned()
            ),
        );
        assert_eq!(
            graph_id_for_hit(
                &code_hit(
                    "Catalogs/Контрагенты/Forms/Форма/Ext/Form/Module.bsl",
                    "ПриОткрытии",
                    "procedure",
                ),
                None,
                None,
            ),
            None,
        );
        assert_eq!(
            graph_id_for_hit(
                &code_hit("CommonModules/Утилиты/Ext/Module.bsl", "ПроверитьИНН", "procedure"),
                None,
                None,
            ),
            Some("method/common/Утилиты/ПроверитьИНН".to_owned()),
        );
    }
}
