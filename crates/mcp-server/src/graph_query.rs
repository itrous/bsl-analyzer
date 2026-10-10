//! Read-only serving of the workspace call graph from the SQLite store.
//!
//! Mirrors the agent-facing shapes of `ide::Analysis::graph_*` (the same response
//! structs, hence the same JSON), but every fact comes from the on-disk database in
//! the workspace cache root built by [`crate::graph_db`] rather than from a resident
//! Salsa database — so a
//! 25k-module config can be served without holding the whole graph in RAM.
//!
//! Source text is read on demand from the file + byte ranges stored per node, so
//! method bodies stay out of the database.

use std::path::Path;

use anyhow::Context;
use ide::{
    classify_graph_id, Direction, EdgeRef, GraphContext, GraphDetail, GraphError, GraphIdKind,
    GraphOverview, NeighborsParams, NeighborsResult, NodeRef, NodeResult, SourceItem, SourceResult,
    MAX_DROPPED_SAMPLE,
};
use line_index::TextRange;
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};

use crate::graph_db::SCHEMA_VERSION;
use crate::tools::location as loc;
use bsl_search::FileKey;

/// A node as stored, before projection to a [`NodeRef`].
struct StoredNode {
    id: String,
    kind: String,
    name: String,
    qualified: String,
    module: Option<String>,
    file: Option<FileKey>,
    name_offset: Option<u32>,
    sig_end: Option<u32>,
    src_start: Option<u32>,
    src_end: Option<u32>,
    dispatch: Option<String>,
    is_export: Option<bool>,
    addressable: bool,
}

/// The full declaration signature, from the keyword line containing `name_offset` through
/// the header end `sig_end` (the closing `)` or export keyword). Internal runs of
/// whitespace — including the newlines of a wrapped parameter list — are collapsed to
/// single spaces so a multi-line declaration reads as one line.
fn signature_in(text: &str, name_offset: u32, sig_end: u32) -> Option<String> {
    let name = (name_offset as usize).min(text.len());
    let end = (sig_end as usize).min(text.len());
    if name > end || !text.is_char_boundary(name) || !text.is_char_boundary(end) {
        return None;
    }
    let start = text[..name].rfind('\n').map_or(0, |i| i + 1);
    let slice = text.get(start..end)?;
    Some(slice.split_whitespace().collect::<Vec<_>>().join(" "))
}

/// Line endings are normalized to LF before redaction and budget clamping: consumers read
/// the source, they do not need byte-exact CRLF, and the CR would only inflate the JSON
/// escaping.
fn slice_in(text: &str, start: u32, end: u32) -> Option<String> {
    text.get(start as usize..end as usize).map(|s| s.replace("\r\n", "\n"))
}

/// Whether `haystack` names `needle` as a WHOLE identifier rather than merely containing it.
/// Both are expected lowercased; case folding never turns a letter into a separator, so the
/// boundary test is the same on either casing.
///
/// A substring test would confirm a whole class of drifted spans instead of catching them:
/// `Считать` sits inside `СчитатьИное`, so a span that slid onto a call to a DIFFERENT method
/// whose name merely contains the old one would certify itself, and the place would cut
/// someone else's call. The node place compares its name slice for EQUALITY; a call
/// expression carries more than the name, so the closest available analogue is an occurrence
/// with no identifier character on either side of it.
fn names_whole(haystack: &str, needle: &str) -> bool {
    let is_ident = |c: char| c.is_alphanumeric() || c == '_';
    haystack.match_indices(needle).any(|(at, _)| {
        let before = haystack[..at].chars().next_back();
        let after = haystack[at + needle.len()..].chars().next();
        !before.is_some_and(is_ident) && !after.is_some_and(is_ident)
    })
}

/// Whether the stored declaration span still ends where a declaration ends.
///
/// A name check alone does not license the enclosing range: an edit INSIDE the body leaves
/// the name where it was and still moves `src_end`, so a span built from it would end at the
/// wrong bytes — plausible, and wrong exactly where a consumer cuts text. A declaration ends
/// with its closing keyword, so that is what the stored end must land on.
///
/// Shared by the node place and the call-site place: both publish the same method's
/// enclosing range, and one of them trusting the offsets the other rejected would put two
/// answers about one declaration in one response.
fn declaration_end_confirmed(text: &str, src_start: u32, src_end: u32) -> bool {
    text.get(src_start as usize..src_end as usize)
        .map(|slice| slice.trim_end().to_lowercase())
        .is_some_and(|slice| {
            ["конецпроцедуры", "конецфункции", "endprocedure", "endfunction"]
                .iter()
                .any(|keyword| slice.ends_with(keyword))
        })
}

/// One node's file text, read AT THE POINT OF USE and at most once.
///
/// Which consumer needs the bytes is decided by that consumer: the place needs them only
/// after the pair is built and the row turns out to carry offsets, the signature and the
/// body only for a method at `signatures`/`bodies`. A predicate deciding this up front
/// restates those conditions and then drifts away from them — `usages` walks up to 200
/// callers at `names` with no root table and cannot use a single byte, yet a
/// detail-and-offsets predicate had it read every caller's file in full.
///
/// Memoized per node and NEVER across nodes: a handle is pooled and outlives edits to the
/// files, so a text remembered across nodes would make the staleness check in
/// [`GraphDb::node_location`] compare the artefact against itself.
struct NodeText<'a> {
    file: Option<&'a FileKey>,
    roots: Option<&'a bsl_search::WorkspaceRoots>,
    read: Option<Option<String>>,
}

impl<'a> NodeText<'a> {
    fn new(file: Option<&'a FileKey>, roots: Option<&'a bsl_search::WorkspaceRoots>) -> Self {
        Self { file, roots, read: None }
    }

    fn get(&mut self) -> Option<&str> {
        let file = self.file?;
        let roots = self.roots?;
        let path = roots.resolve(file)?;
        self.read.get_or_insert_with(|| std::fs::read_to_string(path).ok()).as_deref()
    }
}

const NODE_COLUMNS: &str =
    "id, kind, name, qualified, module, file_root_id, file_path, name_offset, sig_end, src_start, src_end, dispatch, is_export, addressable";

/// Reverse lookup of the files whose methods read a given object or its attributes. A
/// `UNION` of two single-predicate arms so each rides the `edges_to` index (see
/// [`GraphDb::referencing_files`]); shared with the query-plan test so the plan assertion
/// pins the exact executed SQL. `INDEXED BY edges_to` forces the driving table to be `edges`
/// via that index on BOTH arms — without it the planner (which sees no table stats on a
/// freshly opened build) flips the range arm to a full `nodes` scan + `edges_from` probe,
/// i.e. O(nodes). The hint makes the O(inbound-edges) plan a guarantee, not a planner guess.
/// Params: `?1` = the `mdo/…` node id, `?2`/`?3` = the half-open `attribute/…` id range bounds.
///
/// The source has to be a node with BSL source, and that is asked of its KIND.
/// A non-null `file` used to imply it, back when only methods and modules
/// carried one; an object carries its own file now, and a `contains` edge would
/// otherwise report an object's XML as a file that references it.
const REFERENCING_FILES_SQL: &str = "\
    SELECT n.file_root_id, n.file_path FROM edges e INDEXED BY edges_to JOIN nodes n ON e.from_id = n.id \
     WHERE n.kind IN ('method','module') AND n.file_path IS NOT NULL AND e.to_id = ?1 \
    UNION \
    SELECT n.file_root_id, n.file_path FROM edges e INDEXED BY edges_to JOIN nodes n ON e.from_id = n.id \
     WHERE n.kind IN ('method','module') AND n.file_path IS NOT NULL \
       AND e.to_id >= ?2 AND e.to_id < ?3";

fn row_to_stored(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredNode> {
    Ok(StoredNode {
        id: row.get(0)?,
        kind: row.get(1)?,
        name: row.get(2)?,
        qualified: row.get(3)?,
        module: row.get(4)?,
        file: match (row.get::<_, Option<String>>(5)?, row.get::<_, Option<String>>(6)?) {
            (Some(root_id), Some(path)) => Some(FileKey::new(root_id, path)),
            _ => None,
        },
        name_offset: row.get::<_, Option<i64>>(7)?.map(|v| v as u32),
        sig_end: row.get::<_, Option<i64>>(8)?.map(|v| v as u32),
        src_start: row.get::<_, Option<i64>>(9)?.map(|v| v as u32),
        src_end: row.get::<_, Option<i64>>(10)?.map(|v| v as u32),
        dispatch: row.get(11)?,
        is_export: row.get::<_, Option<i64>>(12)?.map(|v| v != 0),
        addressable: row.get::<_, i64>(13)? != 0,
    })
}

/// The `[lo, hi)` id range that selects a module's member methods. A `module/<scope>`
/// id's methods are `method/<scope>/<name>`; a `module/file/<rel>` id's methods are
/// `method/file/<rel>::<name>` (the `::` member separator). The half-open upper bound is
/// the prefix with its last (ASCII separator) byte incremented, so the scan rides the
/// `id` primary-key index and never matches a sibling scope. `None` for a non-module id.
fn method_id_range(module_id: &str) -> Option<(String, String)> {
    let scope = module_id.strip_prefix("module/")?;
    if scope.is_empty() {
        return None;
    }
    let sep = if scope.starts_with("file/") { "::" } else { "/" };
    let prefix = format!("method/{scope}{sep}");
    let mut upper = prefix.clone();
    let last = upper.pop()?; // ASCII '/' or ':'
    upper.push(((last as u8) + 1) as char);
    Some((prefix, upper))
}

/// The graph, as a name provider for [`ide::lookup_names`].
///
/// It selects and counts; ranking, folding and the limit belong to the merge.
/// Whether the graph is built, still building or failed is the host's fact, not
/// the store's, so the caller states it — an absent graph still gets a named
/// verdict rather than silently contributing nothing.
pub struct GraphNameSource<'a> {
    graph: Option<&'a GraphDb>,
    roots: Option<&'a bsl_search::WorkspaceRoots>,
    state: ide::ProviderState,
}

impl<'a> GraphNameSource<'a> {
    pub fn answering(graph: &'a GraphDb, roots: Option<&'a bsl_search::WorkspaceRoots>) -> Self {
        Self { graph: Some(graph), roots, state: ide::ProviderState::Answered }
    }

    /// The graph cannot answer, and the reason travels into the report.
    pub fn absent(state: ide::ProviderState) -> Self {
        debug_assert_ne!(
            state,
            ide::ProviderState::Answered,
            "an absent graph cannot be reported as having answered",
        );
        Self { graph: None, roots: None, state }
    }
}

/// Everything the graph holds. A durable id is matched whatever it names, so a
/// narrowed question must not silently skip the store that owns the id.
const GRAPH_CATEGORIES: &[ide::NameCategory] = &[
    ide::NameCategory::CommonModule,
    ide::NameCategory::Module,
    ide::NameCategory::ModuleMethod,
    ide::NameCategory::MetadataObject,
    ide::NameCategory::MetadataMember,
    ide::NameCategory::Form,
];

/// Which category a stored node belongs to. The kind alone does not settle a
/// module node: only a `common/` scope is callable by name.
fn node_category(kind: &str, id: &str) -> ide::NameCategory {
    match kind {
        "module" if id.starts_with("module/common/") => ide::NameCategory::CommonModule,
        "module" => ide::NameCategory::Module,
        // A subsystem's `<Content>` lists common modules, so the graph holds an
        // `mdo` node for one. It is the same entity the resident publishes as a
        // common module, and answering `metadata_object` here would leave the
        // two rows unable to meet.
        "mdo" if id.starts_with("mdo/CommonModule/") => ide::NameCategory::CommonModule,
        "mdo" => ide::NameCategory::MetadataObject,
        "attribute" | "tabular_section" => ide::NameCategory::MetadataMember,
        "form" | "form_item" | "form_attribute" => ide::NameCategory::Form,
        _ => ide::NameCategory::ModuleMethod,
    }
}

impl ide::ExternalNameSource for GraphNameSource<'_> {
    fn provider(&self) -> ide::ProviderId {
        ide::ProviderId::Graph
    }

    fn state(&self) -> ide::ProviderState {
        self.state
    }

    fn categories(&self) -> &'static [ide::NameCategory] {
        GRAPH_CATEGORIES
    }

    /// A stored node knows the file it is written in, and handing it over is
    /// what lets the dictionary recognise the graph's row and the resident's row
    /// as ONE thing instead of guessing at it from the name. The ranges stay
    /// with `graph action=node`: two answers about a node's exact span would be
    /// two answers to one question, while the file is the identity itself.
    fn supplies_location(&self) -> bool {
        true
    }

    fn candidates(&self, query: &str, limit: usize) -> Result<ide::ProviderHits, String> {
        let Some(graph) = self.graph else {
            return Ok(ide::ProviderHits::new(Vec::new(), 0));
        };
        let result = graph.resolve(query, limit).map_err(|e| e.to_string())?;
        let candidates = result
            .candidates
            .iter()
            .map(|c| {
                // The tier is the ranker's own verdict, carried across rather
                // than recomputed: recomputing it here would be a second
                // opinion on the same question, free to disagree.
                let tier = ide::NameMatchTier::from_code(c.match_kind)
                    .unwrap_or(ide::NameMatchTier::Substring);
                let candidate = ide::NameCandidate::new(
                    ide::resolve_name_segment(&c.id),
                    node_category(c.kind, &c.id),
                    tier,
                    ide::ProviderId::Graph,
                )
                .with_graph_id(&c.id);
                // Looked up per DELIVERED candidate, never per match: the ranker
                // has already cut the list to `limit`, and the file of a node
                // nobody will see is a query for nothing.
                match graph.node_file(&c.id, self.roots) {
                    Ok(Some(file)) => candidate.with_source_path(file),
                    // A node whose file the store does not know is still an
                    // answer, addressed by its id alone.
                    Ok(None) => candidate,
                    Err(error) => {
                        tracing::warn!(id = %c.id, %error, "graph node file lookup failed");
                        candidate
                    }
                }
            })
            .collect();
        // `total` is the ranker's pre-cap count, so a name matching thousands of
        // nodes is not handed over as if twenty were all of them.
        Ok(ide::ProviderHits::new(candidates, result.total))
    }
}

/// Map a stored node kind to the agent-facing static label `NodeRef` expects.
fn node_kind(kind: &str) -> &'static str {
    match kind {
        "module" => "module",
        "mdo" => "mdo",
        "attribute" => "attribute",
        "form" => "form",
        "form_item" => "form_item",
        "form_attribute" => "form_attribute",
        "tabular_section" => "tabular_section",
        _ => "method",
    }
}

fn dispatch_labels(stored: &Option<String>) -> Vec<&'static str> {
    let mut labels = Vec::new();
    if let Some(d) = stored {
        if d.split(',').any(|t| t == "client") {
            labels.push("client");
        }
        if d.split(',').any(|t| t == "server") {
            labels.push("server");
        }
    }
    labels
}

fn edge_kind(kind: &str) -> &'static str {
    match kind {
        "manager_creates" => "manager_creates",
        "manager_access" => "manager_access",
        "query_ref" => "query_ref",
        "contains" => "contains",
        "data_binding" => "data_binding",
        "notify_ref" => "notify_ref",
        "idle_handler" => "idle_handler",
        "event_subscription" => "event_subscription",
        "register_movement" => "register_movement",
        "subsystem_membership" => "subsystem_membership",
        "role_reference" => "role_reference",
        "register_records" => "register_records",
        "register_record_set" => "register_record_set",
        _ => "call",
    }
}

fn provenance(p: &str) -> &'static str {
    match p {
        "inferred" => "inferred",
        "visibility_blocked" => "visibility_blocked",
        "unresolved" => "unresolved",
        "string_resolved" => "string_resolved",
        _ => "resolved",
    }
}

/// A read-only handle to a built graph database. Handles onto the published file are lent
/// by [`crate::graph::GraphStore`]; nothing else opens that file.
pub(crate) struct GraphDb {
    conn: Connection,
}

/// The graph-derived usage summary for a symbol: total inbound edges and the top calling
/// modules (module id → number of calling methods), produced by [`GraphDb::usages`].
pub(crate) struct SymbolUsages {
    pub(crate) count: usize,
    pub(crate) top_modules: Vec<(String, usize)>,
    /// `true` when the caller cap dropped some inbound callers, so `top_modules` is aggregated
    /// from a sample of `count` rather than every caller.
    pub(crate) top_modules_sampled: bool,
}

impl GraphDb {
    /// Open `path` read-only and validate it is a complete build of the current
    /// schema. A truncated build (e.g. a crash mid-write, which leaves no `meta`
    /// rows because they are written last) or a stale schema version is rejected so
    /// the caller rebuilds rather than serving a partial graph.
    pub(crate) fn open(path: &Path) -> anyhow::Result<Self> {
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .with_context(|| format!("opening graph database at {}", path.display()))?;
        let db = Self::from_connection(conn);
        db.validate_meta()?;
        Ok(db)
    }

    fn meta(&self, key: &str) -> anyhow::Result<Option<String>> {
        self.conn
            .query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| r.get(0))
            .optional()
            .with_context(|| format!("reading meta key {key}"))
    }

    fn validate_meta(&self) -> anyhow::Result<()> {
        let version = self
            .meta("schema_version")?
            .context("graph database has no schema_version (incomplete build)")?;
        match version.parse::<u32>() {
            Ok(version) if version == SCHEMA_VERSION => {}
            Ok(version) if version > SCHEMA_VERSION => anyhow::bail!(
                "graph database schema_version {version} is newer than this program's \
                 {SCHEMA_VERSION}"
            ),
            _ => anyhow::bail!(
                "graph database schema_version {version} != expected {SCHEMA_VERSION}"
            ),
        }
        anyhow::ensure!(
            self.meta("publication_id")?.is_some_and(|id| !id.is_empty()),
            "graph database has no publication_id"
        );
        // `nodes`/`edges` are the last meta rows finalize writes; their presence
        // means the build ran to completion.
        anyhow::ensure!(
            self.meta("nodes")?.is_some() && self.meta("edges")?.is_some(),
            "graph database is missing node/edge counts (incomplete build)"
        );
        Ok(())
    }

    /// SQLite's own consistency check of the whole file.
    pub(crate) fn quick_check(&self) -> anyhow::Result<()> {
        let verdict: String = self.conn.query_row("PRAGMA quick_check", [], |row| row.get(0))?;
        anyhow::ensure!(verdict == "ok", "quick_check: {verdict}");
        Ok(())
    }

    /// The build's freshness token — `(revision, fingerprint, force_stale)` — read
    /// from the file's own `meta`, so a served response's revision/staleness always
    /// describe the exact build being served (never a torn mix where a concurrent
    /// reload renamed a newer file in after the generation was captured elsewhere).
    /// Both fingerprint components are required rows (`schema_version` gates out
    /// builds that predate `topology_fp`); `force_stale` defaults to false when absent.
    pub fn freshness_token(&self) -> anyhow::Result<(u64, crate::graph_db::GraphFp, bool)> {
        let revision = self
            .meta("revision")?
            .and_then(|v| v.parse().ok())
            .context("graph database meta.revision missing or unparsable")?;
        let files = self
            .meta("fingerprint")?
            .and_then(|v| v.parse().ok())
            .context("graph database meta.fingerprint missing or unparsable")?;
        let topology = self
            .meta("topology_fp")?
            .and_then(|v| v.parse().ok())
            .context("graph database meta.topology_fp missing or unparsable")?;
        let force_stale = self.meta("force_stale")?.map(|v| v == "1").unwrap_or(false);
        Ok((revision, crate::graph_db::GraphFp { files, topology }, force_stale))
    }

    /// The identity of the publication this database holds.
    pub(crate) fn publication_id(&self) -> anyhow::Result<String> {
        self.meta("publication_id")?.context("graph database has no publication_id")
    }

    /// The indexed `.bsl` file count recorded at build time, for status display.
    /// Defaults to 0 when absent (an older build without the row).
    pub fn files(&self) -> anyhow::Result<usize> {
        Ok(self.meta("files")?.and_then(|v| v.parse().ok()).unwrap_or(0))
    }

    /// The strict module total used to distinguish a valid published empty graph
    /// from an older/incomplete artefact with no fingerprint rows.
    pub(crate) fn file_count_strict(&self) -> anyhow::Result<usize> {
        self.meta("files")?
            .and_then(|v| v.parse().ok())
            .context("graph database meta.files missing or unparsable")
    }

    /// How many modules this artefact was built (or last patched) without being able
    /// to read. Those modules contributed no nodes and no edges, so the graph is
    /// incomplete in a way no fingerprint comparison reveals — `stat` needs no read
    /// permission.
    ///
    /// A count is derived here rather than stored, because the union the patch
    /// computes needs the paths themselves. Absent key → 0, which is honest only
    /// because `SCHEMA_VERSION` gates out artefacts built before the key existed.
    pub fn unread_files(&self) -> usize {
        crate::graph_db::read_unread_paths(&self.conn).len()
    }

    /// The structured modules themselves, for the probe that asks whether any of them can be
    /// read again. A count cannot answer that question: the probe has to resolve each key
    /// through the roots of the candidate publication and read strictly — an error is an error,
    /// not an empty set. Only this form may speak for what a publication still owes.
    pub fn unread_keys_strict(&self) -> anyhow::Result<Vec<bsl_search::FileKey>> {
        crate::graph_db::read_unread_keys_strict(&self.conn)
    }

    /// The content hash stored for every indexed file, by durable key; empty when the rows
    /// will not read.
    pub(crate) fn stored_fingerprints(
        &self,
    ) -> std::collections::HashMap<bsl_search::FileKey, [u8; 32]> {
        crate::graph_db::stored_fingerprints_in(&self.conn)
    }

    /// The signature hash stored for every indexed file, by durable key; empty when the rows
    /// will not read.
    pub(crate) fn stored_sig_hashes(
        &self,
    ) -> std::collections::HashMap<bsl_search::FileKey, Option<u64>> {
        crate::graph_db::stored_sig_hashes_in(&self.conn)
    }

    /// The content hashes recorded with the stat identity they were read under.
    pub(crate) fn stored_observations(
        &self,
        roots: &bsl_search::WorkspaceRoots,
    ) -> Vec<(std::path::PathBuf, crate::graph::content_hash::Observation)> {
        crate::graph_db::read_stored_observations_in(&self.conn, roots)
    }

    /// The callers a signature change must re-project with, or `None` when a point patch
    /// cannot be proven equal to a full rebuild. See [`crate::graph_db::caller_delta_plan_in`].
    pub(crate) fn caller_delta_plan(
        &self,
        sig_changed: &[(&str, &crate::graph_db::ModuleProfile)],
        roots: Option<&bsl_search::WorkspaceRoots>,
    ) -> anyhow::Result<Option<Vec<std::path::PathBuf>>> {
        crate::graph_db::caller_delta_plan_in(&self.conn, sig_changed, roots)
    }

    fn count(&self, sql: &str) -> anyhow::Result<usize> {
        let n: i64 = self.conn.query_row(sql, [], |r| r.get(0)).context("counting graph rows")?;
        Ok(n as usize)
    }

    fn fetch_node(&self, id: &str) -> anyhow::Result<Option<StoredNode>> {
        self.conn
            .query_row(
                &format!("SELECT {NODE_COLUMNS} FROM nodes WHERE id = ?1"),
                params![id],
                row_to_stored,
            )
            .optional()
            .with_context(|| format!("fetching node {id}"))
    }

    /// Resolve a durable id to a stored node, mirroring the in-memory resolver:
    /// a malformed id is [`GraphError::BadId`]; metadata (`mdo`/`attribute`) ids
    /// match case-insensitively on the object/attribute name (BSL is
    /// case-insensitive), while path/scope ids match exactly (the canonical form
    /// agents receive from the graph). A well-formed but absent id is `NotFound`.
    fn resolve_stored(&self, id: &str) -> anyhow::Result<Result<StoredNode, GraphError>> {
        let kind = match classify_graph_id(id) {
            Ok(k) => k,
            Err(bad) => return Ok(Err(bad)),
        };
        if let Some(node) = self.fetch_node(id)? {
            return Ok(Ok(node));
        }
        // A `module/<scope>` id has no stored row unless the module happened to be a
        // module-level edge endpoint. Synthesize it from its member methods (addressed by
        // the `method/<scope>/…` id prefix) so `node(module/…)` resolves and lists members
        // — without polluting the graph with module nodes/edges.
        if matches!(&kind, GraphIdKind::Module { .. } | GraphIdKind::ModuleFile { .. }) {
            return Ok(match self.synthesize_module_node(id)? {
                Some(node) => Ok(node),
                None => Err(GraphError::NotFound { id: id.to_string() }),
            });
        }
        // Case-insensitive fallback for metadata ids only. Both the prefix and the
        // comparison target are rebuilt from the PARSED type's English name (not the
        // raw id segment), so a localized type spelling (`Справочник` → `Catalog`)
        // still matches the stored canonical id. The object/attribute name may be
        // Cyrillic, which SQL cannot fold, so the final compare is done in Rust.
        let (sql_kind, prefix, target) = match &kind {
            GraphIdKind::Mdo { mdo_type, object } => {
                let eng = mdo_type.english_name();
                ("mdo", format!("mdo/{eng}/"), format!("mdo/{eng}/{object}").to_lowercase())
            }
            GraphIdKind::Attribute { mdo_type, object, attr } => {
                let eng = mdo_type.english_name();
                (
                    "attribute",
                    format!("attribute/{eng}/"),
                    format!("attribute/{eng}/{object}/{attr}").to_lowercase(),
                )
            }
            GraphIdKind::Form { owner, form_name } => match owner {
                Some((mdo_type, object)) => {
                    let eng = mdo_type.english_name();
                    (
                        "form",
                        format!("form/{eng}/"),
                        format!("form/{eng}/{object}/{form_name}").to_lowercase(),
                    )
                }
                None => (
                    "form",
                    "form/common/".to_string(),
                    format!("form/common/{form_name}").to_lowercase(),
                ),
            },
            GraphIdKind::FormItem { owner, form_name, item_name } => match owner {
                Some((mdo_type, object)) => {
                    let eng = mdo_type.english_name();
                    (
                        "form_item",
                        format!("form_item/{eng}/"),
                        format!("form_item/{eng}/{object}/{form_name}/{item_name}").to_lowercase(),
                    )
                }
                None => (
                    "form_item",
                    "form_item/common/".to_string(),
                    format!("form_item/common/{form_name}/{item_name}").to_lowercase(),
                ),
            },
            GraphIdKind::FormAttribute { owner, form_name, attr_name } => match owner {
                Some((mdo_type, object)) => {
                    let eng = mdo_type.english_name();
                    (
                        "form_attribute",
                        format!("form_attr/{eng}/"),
                        format!("form_attr/{eng}/{object}/{form_name}/{attr_name}").to_lowercase(),
                    )
                }
                None => (
                    "form_attribute",
                    "form_attr/common/".to_string(),
                    format!("form_attr/common/{form_name}/{attr_name}").to_lowercase(),
                ),
            },
            GraphIdKind::TabularSection { mdo_type, object, section } => {
                let eng = mdo_type.english_name();
                (
                    "tabular_section",
                    format!("tabular_section/{eng}/"),
                    format!("tabular_section/{eng}/{object}/{section}").to_lowercase(),
                )
            }
            GraphIdKind::TabularSectionAttribute { mdo_type, object, section, attr } => {
                let eng = mdo_type.english_name();
                // Stored as an `attribute`-kind node with a `ts_attr/` id.
                (
                    "attribute",
                    format!("ts_attr/{eng}/"),
                    format!("ts_attr/{eng}/{object}/{section}/{attr}").to_lowercase(),
                )
            }
            _ => return Ok(Err(GraphError::NotFound { id: id.to_string() })),
        };
        let mut stmt = self
            .conn
            .prepare(&format!("SELECT {NODE_COLUMNS} FROM nodes WHERE kind = ?1 AND id LIKE ?2"))?;
        let rows = stmt.query_map(params![sql_kind, format!("{prefix}%")], row_to_stored)?;
        for row in rows {
            let node = row?;
            if node.id.to_lowercase() == target {
                return Ok(Ok(node));
            }
        }
        Ok(Err(GraphError::NotFound { id: id.to_string() }))
    }

    /// Synthesize a `module` node from its member methods. A module has a stored row only
    /// when it was an edge endpoint, but its methods are always present as
    /// `method/<scope>/…` rows; the first member supplies the module's file and display
    /// name. `None` when the module has no methods (then `node` reports `not_found`).
    fn synthesize_module_node(&self, id: &str) -> anyhow::Result<Option<StoredNode>> {
        let Some((lo, hi)) = method_id_range(id) else { return Ok(None) };
        let first: Option<(Option<String>, Option<String>, Option<String>)> = self
            .conn
            .query_row(
                "SELECT file_root_id, file_path, module FROM nodes WHERE kind = 'method' AND id >= ?1 AND id < ?2 \
                 ORDER BY id LIMIT 1",
                params![lo, hi],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .context("probing module members")?;
        let Some((file_root_id, file_path, module_display)) = first else { return Ok(None) };
        let file = match (file_root_id, file_path) {
            (Some(root_id), Some(path)) => Some(FileKey::new(root_id, path)),
            _ => None,
        };
        let name = module_display.clone().unwrap_or_else(|| id.to_string());
        Ok(Some(StoredNode {
            id: id.to_string(),
            kind: "module".to_string(),
            name: name.clone(),
            qualified: name,
            module: module_display,
            file,
            name_offset: None,
            sig_end: None,
            src_start: None,
            src_end: None,
            dispatch: None,
            is_export: None,
            addressable: true,
        }))
    }

    /// The member methods of a `module/<scope>` node, addressed by the `method/<scope>/…`
    /// id prefix (the durable scope, NOT the `module` display column).
    fn module_members(&self, module_id: &str) -> anyhow::Result<Vec<ide::ModuleMethod>> {
        let Some((lo, hi)) = method_id_range(module_id) else { return Ok(Vec::new()) };
        let mut stmt = self.conn.prepare(
            "SELECT id, name, is_export FROM nodes WHERE kind = 'method' AND id >= ?1 AND id < ?2 \
             ORDER BY name",
        )?;
        let rows = stmt.query_map(params![lo, hi], |r| {
            Ok(ide::ModuleMethod {
                id: r.get(0)?,
                name: r.get(1)?,
                is_export: r.get::<_, Option<i64>>(2)?.map(|v| v != 0),
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>().context("listing module members")
    }

    /// The true distinct-module count: every module that owns a method (derived from the
    /// `method/<scope>/…` id prefix via [`ide::module_id_of_method`]) unioned with any
    /// `module`-kind row (a module body persisted only because it was an edge endpoint).
    /// Counting `kind='module'` rows alone undercounts, since module nodes are synthesized
    /// on demand and not generally stored — the symptom the agent saw as `modules=13`.
    fn count_modules(&self) -> anyhow::Result<usize> {
        let mut modules: std::collections::HashSet<String> = std::collections::HashSet::new();
        {
            let mut stmt = self.conn.prepare("SELECT id FROM nodes WHERE kind='module'")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            for id in rows {
                modules.insert(id?);
            }
        }
        {
            let mut stmt = self.conn.prepare("SELECT id FROM nodes WHERE kind='method'")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            for id in rows {
                if let Some(module) = ide::module_id_of_method(&id?) {
                    modules.insert(module);
                }
            }
        }
        Ok(modules.len())
    }

    /// Near-miss id lookup: rank every node's durable id against an imprecise `query`
    /// (wrong casing, bare method/object name, or partial id), capped at `limit`, through the
    /// shared [`ide::rank_resolve_candidates`] ranker.
    ///
    /// Stored nodes alone are not enough: module nodes are synthesized on demand and generally
    /// absent from the table (only persisted when they happen to be an edge endpoint), so a
    /// wrong-cased `module/common/<name>` query would find no candidate even though
    /// `graph(node)` recovers it. So we ALSO derive each owning-module id from its method rows
    /// (the same union [`Self::count_modules`] uses), deduped against the stored set — matching
    /// the in-memory `Analysis::graph_resolve`, which sees module nodes directly.
    pub fn resolve(&self, query: &str, limit: usize) -> anyhow::Result<ide::ResolveResult> {
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut candidates: Vec<(String, &'static str)> = Vec::new();
        {
            let mut stmt = self.conn.prepare("SELECT id, kind FROM nodes")?;
            let rows = stmt
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()
                .context("scanning nodes for resolve")?;
            for (id, kind) in rows {
                if seen.insert(id.clone()) {
                    candidates.push((id, node_kind(&kind)));
                }
            }
        }
        {
            let mut stmt = self.conn.prepare("SELECT id FROM nodes WHERE kind='method'")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            for id in rows {
                if let Some(module) = ide::module_id_of_method(&id?) {
                    if seen.insert(module.clone()) {
                        candidates.push((module, node_kind("module")));
                    }
                }
            }
        }
        let (candidates, total) =
            ide::rank_resolve_candidates(candidates.into_iter(), query, limit);
        Ok(ide::ResolveResult::new(query, candidates, total))
    }

    /// The file a node is written in, for the name dictionary's identity.
    ///
    /// A module node is usually absent from the table — it is persisted only
    /// when it happens to be an edge endpoint — so the synthesized form answers
    /// for it, deriving the file from the module's own method rows.
    pub(crate) fn node_file(
        &self,
        id: &str,
        roots: Option<&bsl_search::WorkspaceRoots>,
    ) -> anyhow::Result<Option<String>> {
        let resolve = |file: Option<FileKey>| {
            roots
                // The graph's public file reads use the declared spelling, but this path is
                // only the identity handed to the resident name dictionary.  Its VFS stores
                // the canonical spelling of the source root, so use the frozen walk spelling
                // from the same root snapshot here.  Do not canonicalize through the live
                // filesystem: that would follow a retargeted link and mix generations.
                .and_then(|roots| file.as_ref().and_then(|key| roots.resolve_walked(key)))
                .map(|path| path.to_string_lossy().into_owned())
        };
        if let Some(node) = self.fetch_node(id)? {
            return Ok(resolve(node.file));
        }
        Ok(self.synthesize_module_node(id)?.and_then(|node| resolve(node.file)))
    }

    /// Usage summary for a durable node id: the inbound-edge count plus the top calling
    /// modules. Enrichment for the `symbol_info` tool — a resident-resolved symbol is bridged
    /// to the graph by id, and this returns its fan-in and where it is called from. `top_modules`
    /// caps the returned module aggregate. Returns `None` for an id absent from the graph.
    pub(crate) fn usages(
        &self,
        id: &str,
        top_modules: usize,
    ) -> anyhow::Result<Option<SymbolUsages>> {
        if self.fetch_node(id)?.is_none() && self.synthesize_module_node(id)?.is_none() {
            return Ok(None);
        }
        let count = self.in_degree(id)?;
        let params = NeighborsParams {
            id,
            dir: Direction::In,
            depth: 1,
            max_nodes: 200,
            detail: GraphDetail::Names,
            provenance_filter: Vec::new(),
            edge_kind_filter: Vec::new(),
            // A fan-in tally counts calling modules; it never shows an edge, so asking where
            // the calls are written would read up to 200 files for nothing.
            call_sites: false,
            max_call_sites: 0,
        };
        let mut by_module: std::collections::BTreeMap<String, usize> =
            std::collections::BTreeMap::new();
        let mut sampled = false;
        // The usages summary counts calling MODULES; the nodes themselves never leave this
        // function, so no root table is needed to place them.
        if let Ok(result) = self.neighbors(&params, None)? {
            sampled = result.dropped_count > 0;
            for node in &result.nodes {
                if let Some(module) = &node.module {
                    *by_module.entry(module.clone()).or_default() += 1;
                }
            }
        }
        let mut modules: Vec<(String, usize)> = by_module.into_iter().collect();
        modules.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        modules.truncate(top_modules);
        Ok(Some(SymbolUsages { count, top_modules: modules, top_modules_sampled: sampled }))
    }

    fn in_degree(&self, id: &str) -> anyhow::Result<usize> {
        let d: Option<i64> = self
            .conn
            .query_row("SELECT degree FROM in_degree WHERE id = ?1", params![id], |r| r.get(0))
            .optional()
            .context("reading in_degree")?;
        Ok(d.unwrap_or(0) as usize)
    }

    fn slice_checked(
        &self,
        file: &FileKey,
        roots: Option<&bsl_search::WorkspaceRoots>,
        start: u32,
        end: u32,
    ) -> Result<String, &'static str> {
        let roots = roots.ok_or("roots_unavailable")?;
        let path = roots.resolve(file).ok_or("source_path_unavailable")?;
        let text = std::fs::read_to_string(path).map_err(|_| "source_drifted")?;
        slice_in(&text, start, end).ok_or("source_drifted")
    }

    /// Project a stored node to its agent-facing [`NodeRef`] at `detail`.
    ///
    /// `roots` is the answering snapshot's root table, passed down rather than owned: the
    /// table belongs to the publication, and a `GraphDb` outlives any one of them. `None`
    /// is a real serving state (a cached graph published before the project loaded), and
    /// the node then names `roots_unavailable` instead of quietly dropping its place.
    fn node_ref(
        &self,
        n: &StoredNode,
        detail: GraphDetail,
        roots: Option<&bsl_search::WorkspaceRoots>,
    ) -> NodeRef {
        let kind = node_kind(&n.kind);
        let mut node = NodeRef {
            id: n.id.clone(),
            kind,
            name: n.name.clone(),
            // For code nodes the stored qualified merely restates module + name, so it
            // is not served; metadata nodes keep their russified display path.
            qualified: (!matches!(kind, "method" | "module")).then(|| n.qualified.clone()),
            module: n.module.clone(),
            signature: None,
            source: None,
            truncated: false,
            dispatch: dispatch_labels(&n.dispatch),
            is_export: n.is_export,
            // Populated by `node()` for a `module` node (the member list); a separate
            // query, so it is not done in this projection helper.
            methods: None,
            addressable: n.addressable,
            location: None,
            location_unavailable: None,
        };
        // The file is read at most once per node and only where a consumer actually reaches
        // for it: the place, the signature and the body all describe the same bytes, and
        // reading them twice invites two answers.
        let mut text = NodeText::new(n.file.as_ref(), roots);

        match self.node_location(n, roots, &mut text) {
            Ok(location) => node.location = Some(location),
            Err(reason) => node.location_unavailable = Some(reason),
        }
        if n.kind == "method" && matches!(detail, GraphDetail::Signatures | GraphDetail::Bodies) {
            if let (Some(off), Some(end)) = (n.name_offset, n.sig_end) {
                node.signature = text.get().and_then(|text| signature_in(text, off, end));
            }
            if detail == GraphDetail::Bodies {
                if let (Some(s), Some(e)) = (n.src_start, n.src_end) {
                    node.source = text.get().and_then(|text| slice_in(text, s, e));
                }
            }
        }
        node
    }

    /// A stored node's place under the location contract, or the machine reason there is
    /// none.
    ///
    /// The name range is VERIFIED by slicing rather than trusted: the database stores where
    /// a name starts and where the HEADER ends (`sig_end` — the closing `)` or the export
    /// keyword), not where the name ends. Publishing the header end as the name's would put
    /// the parameter list inside a field the contract says is the name, so the range is
    /// built from the name's own length and dropped unless the text there really is that
    /// name.
    fn node_location(
        &self,
        n: &StoredNode,
        roots: Option<&bsl_search::WorkspaceRoots>,
        text: &mut NodeText<'_>,
    ) -> Result<serde_json::Value, &'static str> {
        if !matches!(n.kind.as_str(), "method" | "module") {
            return Err(loc::LocationUnavailable::NoSourceLocation.code());
        }
        let (Some(file), Some(roots)) = (n.file.as_ref(), roots) else {
            // A method whose row has no file is a path-fallback node seen only as an edge
            // endpoint; a missing table is the boot window. Neither may answer "no place".
            // Two different facts, two different codes: a method whose row carries no path
            // HAS a file (it is a path-fallback node seen only as an edge endpoint) — its
            // address was lost, not absent. `no_source_location` is reserved for entities
            // that have no file at all, and saying it here would send a consumer looking in
            // the wrong direction.
            return Err(if n.file.is_none() {
                loc::LocationUnavailable::SourcePathUnavailable.code()
            } else {
                loc::LocationUnavailable::RootsUnavailable.code()
            });
        };
        let Some(path) = roots.resolve(file) else {
            return Err(loc::LocationUnavailable::SourcePathUnavailable.code());
        };
        let location = loc::Location::from_path(roots, &path).map_err(|reason| reason.code())?;

        // The pair costs no I/O; only the ranges do, and everything above this line is
        // decided without touching the disk. A node with no offsets — a synthesized `module`
        // row — leaves here, so `overview` does not read a module's file for a signature it
        // will never build out of it.
        let (Some(name_offset), Some(src_start), Some(src_end)) =
            (n.name_offset, n.src_start, n.src_end)
        else {
            return Ok(location.to_value());
        };
        let Some(text) = text.get() else {
            // The row's offsets describe a file we cannot read now; the pair is still true.
            return Ok(location.to_value());
        };

        // Offsets come from the artefact, the text from disk NOW. Between a build and its
        // catch-up reload those disagree, and an offset that stayed inside the file yields a
        // plausible but wrong span. The name is the one span we can verify by slicing, so it
        // gates BOTH ranges: an unverifiable place is published as the file alone rather than
        // as a range a consumer would happily cut text with.
        let name_end = name_offset as usize + n.name.len();
        let named = text
            .get(name_offset as usize..name_end)
            .is_some_and(|slice| slice.to_lowercase() == n.name.to_lowercase());
        if !named {
            return Ok(location.to_value());
        }

        let declared_end = declaration_end_confirmed(text, src_start, src_end);

        let index = line_index::LineIndex::new(text);
        let name = index
            .utf16_line_col_range(
                text,
                TextRange::new(name_offset.into(), (name_end as u32).into()),
            )
            .map(loc::PositionRange::from);
        let enclosing = declared_end
            .then(|| {
                index.utf16_line_col_range(text, TextRange::new(src_start.into(), src_end.into()))
            })
            .flatten()
            .map(loc::PositionRange::from);

        Ok(location.with_range(name).with_enclosing_range(enclosing).to_value())
    }

    /// Build the places for the rows one served edge was grouped from.
    ///
    /// Returns either every place or none: a site that fails verification takes the whole
    /// list with it. A list shortened by a dropped site would be indistinguishable from one
    /// shortened by the cap, and only the second is something `call_sites_truncated` may
    /// claim — so the honest answer for a file that no longer confirms its offsets is that
    /// this edge has no place, named as such.
    #[allow(
        clippy::too_many_arguments,
        reason = "the inputs are the row set and the four things a place is built from; \
                  bundling them into a struct would name the same list twice"
    )]
    fn call_site_places(
        &self,
        rows: &[&StoredEdge],
        roots: Option<&bsl_search::WorkspaceRoots>,
        from: Option<&StoredNode>,
        to_name: Option<&str>,
        texts: &mut AnswerTexts,
        max_sites: usize,
    ) -> CallSitePlaces {
        let mut spans: Vec<(u32, u32)> = rows
            .iter()
            .filter_map(|row| row.call_start.zip(row.call_end))
            .filter(|(start, end)| start < end)
            .collect();

        if spans.is_empty() {
            // Which absence it is was decided by the pass that wrote the rows; "not recorded"
            // wins a mixed group, because it is the one a later build can turn into a place.
            let absent = rows
                .iter()
                .filter_map(|row| row.call_absent.as_deref())
                .find(|code| *code == ide::CALL_SITE_NOT_RECORDED)
                .or_else(|| rows.iter().filter_map(|row| row.call_absent.as_deref()).next())
                .unwrap_or(ide::NO_CALL_SITE);
            return CallSitePlaces::unavailable(absence_code(absent));
        }

        // One order, decided here and not by the store: the rows come back in whatever order
        // the index walks them, and two answers about one edge must not disagree about which
        // call comes first.
        spans.sort_unstable();
        spans.dedup();
        let total = spans.len();

        let Some(roots) = roots else {
            return CallSitePlaces::unavailable(loc::LocationUnavailable::RootsUnavailable.code());
        };
        let (Some(from), Some(file)) = (from, from.and_then(|n| n.file.as_ref())) else {
            return CallSitePlaces::unavailable(
                loc::LocationUnavailable::SourcePathUnavailable.code(),
            );
        };
        let Some(path) = roots.resolve(file) else {
            return CallSitePlaces::unavailable(
                loc::LocationUnavailable::SourcePathUnavailable.code(),
            );
        };
        let location = match loc::Location::from_path(roots, &path) {
            Ok(location) => location,
            Err(reason) => return CallSitePlaces::unavailable(reason.code()),
        };
        let Some(text) = texts.get(file, roots) else {
            // The offsets describe a file we cannot read now, so nothing confirms them.
            return CallSitePlaces::unavailable(ide::SOURCE_DRIFTED);
        };

        // Offsets come from the artefact, the text from disk NOW. The one thing a call site
        // can be checked against is the name it calls: BSL is case-insensitive, and a handler
        // named in a string literal sits inside the same expression. An unnameable target
        // leaves nothing to check against, which is not the same as checking and passing.
        let confirmed = to_name.is_some_and(|name| {
            let needle = name.to_lowercase();
            !needle.is_empty()
                && spans.iter().all(|(start, end)| {
                    text.get(*start as usize..*end as usize)
                        .is_some_and(|slice| names_whole(&slice.to_lowercase(), &needle))
                })
        });
        if !confirmed {
            return CallSitePlaces::unavailable(ide::SOURCE_DRIFTED);
        }

        let index = line_index::LineIndex::new(text);
        let enclosing = from
            .src_start
            .zip(from.src_end)
            .filter(|(start, end)| declaration_end_confirmed(text, *start, *end))
            .and_then(|(start, end)| {
                index.utf16_line_col_range(text, TextRange::new(start.into(), end.into()))
            })
            .map(loc::PositionRange::from);

        let places: Vec<serde_json::Value> = spans
            .iter()
            .take(max_sites)
            .map(|(start, end)| {
                let range = index
                    .utf16_line_col_range(text, TextRange::new((*start).into(), (*end).into()))
                    .map(loc::PositionRange::from);
                location.clone().with_range(range).with_enclosing_range(enclosing).to_value()
            })
            .collect();

        CallSitePlaces {
            truncated: places.len() < total,
            places: Some(places),
            total: Some(total),
            unavailable: None,
        }
    }

    /// Wrap an open connection.
    fn from_connection(conn: Connection) -> Self {
        Self { conn }
    }

    /// Cold-start overview: node/edge tallies, the most-called nodes, and the
    /// provenance/dispatch profile.
    pub fn overview(
        &self,
        top_n: usize,
        roots: Option<&bsl_search::WorkspaceRoots>,
    ) -> anyhow::Result<GraphOverview> {
        let nodes = self.count("SELECT COUNT(*) FROM nodes")?;
        let methods = self.count("SELECT COUNT(*) FROM nodes WHERE kind='method'")?;
        let modules = self.count_modules()?;
        let mdos = self.count("SELECT COUNT(*) FROM nodes WHERE kind='mdo'")?;
        let attributes = self.count("SELECT COUNT(*) FROM nodes WHERE kind='attribute'")?;
        let tabular_sections =
            self.count("SELECT COUNT(*) FROM nodes WHERE kind='tabular_section'")?;
        let forms = self.count("SELECT COUNT(*) FROM nodes WHERE kind='form'")?;
        let form_items = self.count("SELECT COUNT(*) FROM nodes WHERE kind='form_item'")?;
        let form_attributes =
            self.count("SELECT COUNT(*) FROM nodes WHERE kind='form_attribute'")?;
        let edges = self.count("SELECT COUNT(*) FROM edges")?;
        let client_to_server_edges = self.count("SELECT COUNT(*) FROM edges WHERE crosses=1")?;

        let mut edge_provenance = std::collections::BTreeMap::new();
        {
            let mut stmt =
                self.conn.prepare("SELECT provenance, COUNT(*) FROM edges GROUP BY provenance")?;
            let rows =
                stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as usize)))?;
            for row in rows {
                let (p, c) = row?;
                edge_provenance.insert(provenance(&p), c);
            }
        }

        let top_ids: Vec<String> = {
            let mut stmt = self
                .conn
                .prepare("SELECT id FROM in_degree ORDER BY degree DESC, id ASC LIMIT ?1")?;
            let rows = stmt.query_map(params![top_n as i64], |r| r.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        let mut top_by_centrality = Vec::with_capacity(top_ids.len());
        for id in top_ids {
            if let Some(n) = self.fetch_node(&id)? {
                top_by_centrality.push(self.node_ref(&n, GraphDetail::Signatures, roots));
            }
        }

        Ok(GraphOverview {
            modules,
            methods,
            mdos,
            attributes,
            tabular_sections,
            forms,
            form_items,
            form_attributes,
            nodes,
            edges,
            top_by_centrality,
            edge_provenance,
            client_to_server_edges,
        })
    }

    /// Resolve a durable id to a single node at `detail`. The id must match a
    /// stored node exactly (the ids agents receive from the graph are canonical).
    pub fn node(
        &self,
        id: &str,
        detail: GraphDetail,
        roots: Option<&bsl_search::WorkspaceRoots>,
    ) -> anyhow::Result<Result<NodeResult, GraphError>> {
        let stored = match self.resolve_stored(id)? {
            Ok(n) => n,
            Err(e) => return Ok(Err(e)),
        };
        let mut node = self.node_ref(&stored, detail, roots);
        // A `module` node lists its members so an agent discovers them from `node(module/…)`
        // directly, without a traversal.
        if stored.kind == "module" {
            node.methods = Some(self.module_members(&stored.id)?);
        }
        Ok(Ok(NodeResult { node }))
    }

    fn directed_edges(
        &self,
        node_id: &str,
        dir: Direction,
        provenance_filter: &[String],
        kind_filter: &[String],
    ) -> anyhow::Result<Vec<StoredEdge>> {
        let mut edges = Vec::new();
        if matches!(dir, Direction::Out | Direction::Both) {
            self.collect_edges("from_id", node_id, provenance_filter, kind_filter, &mut edges)?;
        }
        if matches!(dir, Direction::In | Direction::Both) {
            self.collect_edges("to_id", node_id, provenance_filter, kind_filter, &mut edges)?;
        }
        Ok(edges)
    }

    fn collect_edges(
        &self,
        column: &str,
        node_id: &str,
        provenance_filter: &[String],
        kind_filter: &[String],
        out: &mut Vec<StoredEdge>,
    ) -> anyhow::Result<()> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT from_id, to_id, kind, provenance, call_start, call_end, call_absent, crosses \
             FROM edges WHERE {column} = ?1",
        ))?;
        let rows = stmt.query_map(params![node_id], |r| {
            Ok(StoredEdge {
                from: r.get(0)?,
                to: r.get(1)?,
                kind: r.get(2)?,
                provenance: r.get(3)?,
                call_start: r.get(4)?,
                call_end: r.get(5)?,
                call_absent: r.get(6)?,
                crosses: r.get::<_, i64>(7)? != 0,
            })
        })?;
        for row in rows {
            let edge = row?;
            let prov_ok = provenance_filter.is_empty()
                || provenance_filter.iter().any(|p| *p == provenance(&edge.provenance));
            let kind_ok =
                kind_filter.is_empty() || kind_filter.iter().any(|k| *k == edge_kind(&edge.kind));
            if prov_ok && kind_ok {
                out.push(edge);
            }
        }
        Ok(())
    }

    /// Traverse callers/callees from a node up to `depth`, bounded by `max_nodes`
    /// (the lowest-centrality discovered nodes are the ones dropped past the cap).
    pub fn neighbors(
        &self,
        params: &NeighborsParams<'_>,
        roots: Option<&bsl_search::WorkspaceRoots>,
    ) -> anyhow::Result<Result<NeighborsResult, GraphError>> {
        let root = match self.resolve_stored(params.id)? {
            Ok(n) => n,
            Err(err) => return Ok(Err(err)),
        };
        let depth = params.depth.max(1);

        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        seen.insert(root.id.clone());
        let mut discovered: Vec<String> = Vec::new();
        let mut out_edges: Vec<StoredEdge> = Vec::new();
        // Distinct non-root nodes reached downstream vs upstream (mirrors the in-memory
        // path) so a `Both` traversal reports each direction's fan-out.
        let mut out_reached: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut in_reached: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut frontier = vec![root.id.clone()];

        for _ in 0..depth {
            let mut next = Vec::new();
            for node_id in &frontier {
                for edge in self.directed_edges(
                    node_id,
                    params.dir,
                    &params.provenance_filter,
                    &params.edge_kind_filter,
                )? {
                    let downstream = &edge.from == node_id;
                    let other = if downstream { edge.to.clone() } else { edge.from.clone() };
                    if other != root.id {
                        if downstream {
                            out_reached.insert(other.clone());
                        } else {
                            in_reached.insert(other.clone());
                        }
                    }
                    out_edges.push(edge);
                    if seen.insert(other.clone()) {
                        next.push(other.clone());
                        discovered.push(other);
                    }
                }
            }
            if next.is_empty() {
                break;
            }
            frontier = next;
        }
        let out_total =
            matches!(params.dir, Direction::Out | Direction::Both).then_some(out_reached.len());
        let in_total =
            matches!(params.dir, Direction::In | Direction::Both).then_some(in_reached.len());

        // Centrality-ranked tail-drop of discovered (non-root) nodes.
        let mut ranked: Vec<(usize, String)> = Vec::with_capacity(discovered.len());
        for id in discovered {
            ranked.push((self.in_degree(&id)?, id));
        }
        ranked.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        let total = ranked.len();
        let mut dropped = Vec::new();
        if ranked.len() > params.max_nodes {
            for (_, id) in ranked.split_off(params.max_nodes).into_iter().take(MAX_DROPPED_SAMPLE) {
                dropped.push(id);
            }
        }
        let kept: std::collections::HashSet<&String> = ranked.iter().map(|(_, id)| id).collect();

        let mut nodes = Vec::with_capacity(ranked.len());
        for (_, id) in &ranked {
            if let Some(n) = self.fetch_node(id)? {
                nodes.push(self.node_ref(&n, params.detail, roots));
            }
        }

        // Distribution + connector-loss over the deduped full neighbourhood (every
        // discovered edge, before the node-cap edge-survival filter), mirroring the
        // in-memory serve path so the counts are byte-identical.
        let mut counted: std::collections::HashSet<(String, String, String)> =
            std::collections::HashSet::new();
        let mut by_kind: std::collections::BTreeMap<&'static str, usize> =
            std::collections::BTreeMap::new();
        let mut by_provenance: std::collections::BTreeMap<&'static str, usize> =
            std::collections::BTreeMap::new();
        let mut connectors_dropped = false;
        for e in &out_edges {
            if !counted.insert((e.from.clone(), e.to.clone(), e.kind.clone())) {
                continue;
            }
            *by_kind.entry(edge_kind(&e.kind)).or_default() += 1;
            *by_provenance.entry(provenance(&e.provenance)).or_default() += 1;
            let survives = (e.from == root.id || kept.contains(&e.from))
                && (e.to == root.id || kept.contains(&e.to));
            if !survives {
                connectors_dropped = true;
            }
        }

        // Keep only edges whose endpoints both survived; group by (from, to, kind) so a
        // `Both` sweep that meets an edge from each end emits it once. The store keeps one
        // row per call site, so grouping is also what gives a served edge every place it has.
        // An endpoint equal to the root is omitted (the response carries the root once).
        let mut grouped: std::collections::HashMap<(&str, &str, &str), Vec<&StoredEdge>> =
            std::collections::HashMap::new();
        let mut order: Vec<(&str, &str, &str)> = Vec::new();
        for e in out_edges.iter().filter(|e| {
            (e.from == root.id || kept.contains(&e.from))
                && (e.to == root.id || kept.contains(&e.to))
        }) {
            let key = (e.from.as_str(), e.to.as_str(), e.kind.as_str());
            grouped
                .entry(key)
                .or_insert_with(|| {
                    order.push(key);
                    Vec::new()
                })
                .push(e);
        }

        let mut texts = AnswerTexts::default();
        let mut edges = Vec::with_capacity(order.len());
        for key in order {
            let rows = &grouped[&key];
            let first = rows[0];
            let sites = if params.call_sites {
                let from = self.fetch_node(first.from.as_str())?;
                // The name the edge calls is the only thing a recorded span can be checked
                // against, so an endpoint the artefact cannot name leaves the span
                // unverifiable — never verified-by-default against an empty needle.
                let to_name = self.fetch_node(first.to.as_str())?.map(|node| node.name);
                self.call_site_places(
                    rows,
                    roots,
                    from.as_ref(),
                    to_name.as_deref(),
                    &mut texts,
                    params.max_call_sites,
                )
            } else {
                CallSitePlaces { places: None, total: None, truncated: false, unavailable: None }
            };
            edges.push(EdgeRef {
                kind: edge_kind(&first.kind),
                provenance: provenance(&first.provenance),
                crosses_client_to_server: first.crosses,
                from: (first.from != root.id).then(|| first.from.clone()),
                to: (first.to != root.id).then(|| first.to.clone()),
                call_sites: sites.places,
                call_sites_total: sites.total,
                call_sites_truncated: sites.truncated,
                call_sites_unavailable: sites.unavailable,
            });
        }

        let returned = nodes.len();
        let confidence = (!by_provenance.is_empty()).then(|| ide::confidence_label(&by_provenance));
        Ok(Ok(NeighborsResult {
            root: self.node_ref(&root, params.detail, roots),
            nodes,
            edges,
            total,
            returned,
            dropped_count: total - returned,
            dropped,
            by_kind,
            by_provenance,
            confidence,
            connectors_dropped,
            out_total,
            in_total,
        }))
    }

    /// Render a method's outbound graph context (dispatch, signature, calls, metadata
    /// reads) from the stored graph — the SQLite-backed twin of
    /// [`ide::Analysis::graph_context_for_method`]. Returns byte-identical text to the
    /// in-memory renderer (guarded by a parity test), so a chunk enriched from either
    /// source keys the same embedding. `None` for a non-method id or one absent from
    /// the graph.
    pub fn graph_context(
        &self,
        id: &str,
        roots: Option<&bsl_search::WorkspaceRoots>,
    ) -> anyhow::Result<Option<String>> {
        let node = match self.fetch_node(id)? {
            Some(n) if n.kind == "method" => n,
            _ => return Ok(None),
        };
        // Not serialized as a node: this projection feeds a text renderer for embedding
        // enrichment, so it needs no place and takes no table.
        let nref = self.node_ref(&node, GraphDetail::Signatures, roots);

        // Mirror the in-memory renderer's facts exactly by EDGE kind, not just target
        // kind: calls come only from `call` edges, reads only from a method's
        // metadata-touch edges (`manager_*` / `query_ref`). A method never has
        // `contains`/`data_binding` outbound edges (those originate at mdo/form nodes),
        // but gating on the kind keeps this equivalent even if that changes.
        let is_read_edge =
            |kind: &str| matches!(kind, "manager_creates" | "manager_access" | "query_ref");
        let mut calls = Vec::new();
        let mut reads = Vec::new();
        for edge in self.directed_edges(id, Direction::Out, &[], &[])? {
            match classify_graph_id(&edge.to) {
                Ok(GraphIdKind::Method { name, .. }) | Ok(GraphIdKind::MethodFile { name, .. })
                    if edge.kind == "call" =>
                {
                    calls.push(name);
                }
                Ok(GraphIdKind::Mdo { mdo_type, object }) if is_read_edge(&edge.kind) => {
                    reads.push(format!("{}.{}", mdo_type.russian_name(), object));
                }
                Ok(GraphIdKind::Attribute { mdo_type, object, attr })
                    if is_read_edge(&edge.kind) =>
                {
                    reads.push(format!("{}.{}.{}", mdo_type.russian_name(), object, attr));
                }
                _ => {}
            }
        }
        calls.sort();
        calls.dedup();
        reads.sort();
        reads.dedup();

        let ctx = GraphContext { dispatch: nref.dispatch, signature: nref.signature, calls, reads };
        Ok(Some(ctx.render()))
    }

    /// The distinct source files (`nodes.file_root_id`, `nodes.file_path`) of every method whose stored outbound
    /// read edges target `mdo_id` or any of its attributes — i.e. the modules whose
    /// rendered `graph_context` embeds a metadata read of this object (see
    /// [`Self::graph_context`], which renders `mdo/…` and `attribute/…` reads). Those
    /// stored contexts go stale when the object's `.xml` changes, so this is the reverse
    /// lookup that finds the REFERENCING modules to re-context — owned modules resolve by
    /// path convention, but a module that merely reads the object is only discoverable
    /// through the persisted read edges.
    ///
    /// Index-backed on `edges_to`: a `UNION` of two arms, each constraining `edges.to_id`
    /// with a SINGLE indexable predicate — an equality on the object node, and a half-open
    /// range over its `attribute/<Type>/<Object>/…` ids — so each arm drives from the
    /// `edges_to` index and the scan is O(inbound read edges), never a table scan. (A single
    /// `to_id = ? OR to_id BETWEEN ? AND ?` defeats the planner: it flips to scanning
    /// `nodes` instead.) The half-open upper bound bumps the trailing `/` (0x2F) to `0`
    /// (0x30), the same trick [`method_id_range`] uses. `mdo_id` without the `mdo/` prefix
    /// yields an empty set.
    pub fn referencing_files(
        &self,
        mdo_id: &str,
        roots: Option<&bsl_search::WorkspaceRoots>,
    ) -> anyhow::Result<Vec<String>> {
        let Some(object) = mdo_id.strip_prefix("mdo/") else { return Ok(Vec::new()) };
        let attr_lo = format!("attribute/{object}/");
        let mut attr_hi = attr_lo.clone();
        let last = attr_hi.pop().expect("attribute prefix ends in '/'"); // ASCII '/'
        attr_hi.push(((last as u8) + 1) as char);
        let mut stmt = self.conn.prepare(REFERENCING_FILES_SQL)?;
        let rows = stmt
            .query_map(params![mdo_id, attr_lo, attr_hi], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("collecting referencing files")?;
        Ok(rows
            .into_iter()
            .filter_map(|(root_id, path)| match roots {
                Some(roots) => roots
                    .resolve(&FileKey::new(root_id, path))
                    .map(|path| path.to_string_lossy().into_owned()),
                None => None,
            })
            .collect())
    }

    /// Fetch method source for a set of ids, stopping once the rough output budget
    /// (`max_output_tokens`, ~4 chars/token) is reached.
    pub fn source(
        &self,
        ids: &[String],
        max_output_tokens: usize,
        roots: Option<&bsl_search::WorkspaceRoots>,
    ) -> anyhow::Result<SourceResult> {
        let budget_chars = max_output_tokens.saturating_mul(4).max(1);
        let mut used = 0usize;
        let mut budget_exhausted = false;
        let mut items = Vec::with_capacity(ids.len());

        for id in ids {
            let item = match self.resolve_stored(id)? {
                Err(err) => SourceItem {
                    id: id.clone(),
                    source: None,
                    error: Some(err),
                    truncated: false,
                    skipped_budget_exhausted: false,
                },
                Ok(n) if n.kind != "method" => {
                    let reason = if n.kind == "module" {
                        "module-body source is not served; request a method"
                    } else {
                        "a metadata node has no source; request a method"
                    };
                    SourceItem {
                        id: id.clone(),
                        source: None,
                        error: Some(GraphError::Unsupported {
                            id: id.clone(),
                            reason: reason.into(),
                        }),
                        truncated: false,
                        skipped_budget_exhausted: false,
                    }
                }
                Ok(n) => match (n.file.as_ref(), n.src_start, n.src_end) {
                    (Some(file), Some(s), Some(e)) => match self.slice_checked(file, roots, s, e) {
                        Ok(_) if used >= budget_chars => {
                            budget_exhausted = true;
                            SourceItem {
                                id: id.clone(),
                                source: None,
                                error: None,
                                truncated: true,
                                skipped_budget_exhausted: true,
                            }
                        }
                        Ok(src) => {
                            let remaining = budget_chars - used;
                            let (text, truncated) = clamp_source(src, remaining);
                            used += text.len();
                            budget_exhausted |= truncated;
                            SourceItem {
                                id: id.clone(),
                                source: Some(text),
                                error: None,
                                truncated,
                                skipped_budget_exhausted: false,
                            }
                        }
                        Err(reason) => SourceItem {
                            id: id.clone(),
                            source: None,
                            error: Some(GraphError::Unsupported {
                                id: id.clone(),
                                reason: reason.into(),
                            }),
                            truncated: false,
                            skipped_budget_exhausted: false,
                        },
                    },
                    _ => SourceItem {
                        id: id.clone(),
                        source: None,
                        error: Some(GraphError::NotFound { id: id.clone() }),
                        truncated: false,
                        skipped_budget_exhausted: false,
                    },
                },
            };
            items.push(item);
        }

        Ok(SourceResult { items, budget_exhausted })
    }
}

/// A [`bsl_search::GraphContextProvider`] backed by the published on-disk graph. This is
/// the production source for bulk index enrichment: reading a method's outbound facts from
/// the prebuilt `bsl-graph.db` is RAM-bounded and shares the graph's freshness, unlike
/// rendering from a whole-workspace `Analysis`.
///
/// It holds no handle of its own. Every render borrows one from the [`GraphStore`] for the
/// generation the provider was made for and returns it before the search engine goes on, so
/// no graph handle is held across embedding, queueing or a write to the search store.
///
/// [`GraphStore`]: crate::graph::GraphStore
pub(crate) struct GraphDbContextProvider {
    store: crate::graph::GraphStore,
    generation: u64,
    roots: Option<bsl_search::WorkspaceRoots>,
    owed: Option<crate::graph::OwedContextMarks>,
}

impl GraphDbContextProvider {
    /// `owed` is where marks for renders this provider failed are reported; without it they
    /// wait in the search store for the next consumption of leftover marks.
    pub(crate) fn new(
        store: crate::graph::GraphStore,
        generation: u64,
        roots: Option<&bsl_search::WorkspaceRoots>,
        owed: Option<crate::graph::OwedContextMarks>,
    ) -> Self {
        Self { store, generation, roots: roots.cloned(), owed }
    }
}

impl bsl_search::GraphContextProvider for GraphDbContextProvider {
    fn graph_context(&self, rel_path: &str, symbol_name: &str, kind: &str) -> Option<String> {
        self.try_graph_context(rel_path, symbol_name, kind).ok().flatten()
    }

    fn context_marks_owed(&self, mark_high: i64) {
        if let Some(owed) = &self.owed {
            owed.record(mark_high);
        }
    }

    fn try_graph_context(
        &self,
        rel_path: &str,
        symbol_name: &str,
        _kind: &str,
    ) -> Result<Option<String>, bsl_search::GraphContextError> {
        // Methods in metadata-keyed modules resolve to a durable id; form/command
        // modules (path-fallback ids) are not enriched here — a legitimate `None`, not a
        // failure.
        let Some(id) = ide::method_id_for_path(rel_path, symbol_name) else {
            return Ok(None);
        };
        // An unavailable graph, a newer publication or a graph-DB read error is a transient
        // FAILURE: surface it as `Err` so the context refresh keeps the dirty mark and
        // retries on the next publish, rather than clearing the mark against a render that
        // never ran or ran against another publication's roots.
        self.store
            .read(Some(self.generation), crate::graph::BACKGROUND_READ_WAIT, |snapshot| {
                snapshot.graph.graph_context(&id, self.roots.as_ref())
            })
            .map_err(|e| bsl_search::GraphContextError(e.to_string()))?
            .map_err(|e| bsl_search::GraphContextError(e.to_string()))
    }
}

struct StoredEdge {
    from: String,
    to: String,
    kind: String,
    provenance: String,
    /// The call's byte range in the `from` node's file, when this row records one.
    call_start: Option<u32>,
    call_end: Option<u32>,
    /// The contract code for why there is no range, when there is none.
    call_absent: Option<String>,
    crosses: bool,
}

/// What one served edge answers when its caller asked where its call is written.
struct CallSitePlaces {
    places: Option<Vec<serde_json::Value>>,
    total: Option<usize>,
    truncated: bool,
    unavailable: Option<&'static str>,
}

impl CallSitePlaces {
    fn unavailable(code: &'static str) -> Self {
        Self { places: None, total: None, truncated: false, unavailable: Some(code) }
    }
}

/// Map a stored absence code back to the contract's own `'static` spelling, so nothing but a
/// vocabulary member ever reaches a consumer. An unrecognised code cannot come from this
/// binary's projection — the schema version gate rejects an older artefact outright — and the
/// fallback is the reading that promises less: "not recorded" leaves a later build free to
/// supply the place, where "no call site" would tell the consumer to stop asking.
fn absence_code(stored: &str) -> &'static str {
    loc::LocationUnavailable::ALL
        .iter()
        .find(|reason| reason.code() == stored)
        .map_or(ide::CALL_SITE_NOT_RECORDED, |reason| reason.code())
}

/// The file texts ONE answer reads, kept for that answer alone.
///
/// Distinct from [`NodeText`], which memoizes one node's file: a `callees` answer takes every
/// call site out of the same file — the traversal root's — and re-reading it once per edge is
/// the waste this exists to avoid. It lives on the stack of a single `neighbors` call and
/// dies with it, so nothing is remembered across answers and the offsets are still checked
/// against bytes read during THIS one. Bounded by construction: an answer only reaches files
/// of nodes it kept.
#[derive(Default)]
struct AnswerTexts {
    read: std::collections::HashMap<FileKey, Option<String>>,
}

impl AnswerTexts {
    fn get(&mut self, file: &FileKey, roots: &bsl_search::WorkspaceRoots) -> Option<&str> {
        if !self.read.contains_key(file) {
            let text = roots.resolve(file).and_then(|path| std::fs::read_to_string(path).ok());
            self.read.insert(file.clone(), text);
        }
        self.read.get(file).and_then(|text| text.as_deref())
    }
}

/// Truncate `src` to at most `max_chars` bytes on a char boundary.
fn clamp_source(src: String, max_chars: usize) -> (String, bool) {
    if src.len() <= max_chars {
        return (src, false);
    }
    let mut end = max_chars;
    while end > 0 && !src.is_char_boundary(end) {
        end -= 1;
    }
    (src[..end].to_string(), true)
}

#[cfg(test)]
mod tests {
    use super::{edge_kind, method_id_range, node_category, provenance, GraphDb, GraphNameSource};
    use crate::graph_db::{GraphDbWriter, GraphMeta};
    use rusqlite::{params, Connection};

    #[test]
    fn rejects_graph_from_prior_projection_schema_version() {
        // Given: a complete graph created by the current writer.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bsl-graph.db");
        GraphDbWriter::create(&path)
            .unwrap()
            .finalize(&GraphMeta {
                revision: 1,
                fingerprint: crate::graph_db::GraphFp::default(),
                files: 0,
                built_at: "t".to_string(),
                publication_id: "test-1".to_owned(),
            })
            .unwrap();
        Connection::open(&path)
            .unwrap()
            .execute("UPDATE meta SET value = '12' WHERE key = 'schema_version'", [])
            .unwrap();

        // When: the graph is opened through the serving path.
        let result = GraphDb::open(&path);

        // Then: a graph from the prior projection schema is rejected for rebuilding.
        assert!(result.is_err(), "prior projection graphs must be rebuilt");
    }

    /// A database that cannot say which publication it holds is not stamped with an invented
    /// identity: it is rebuilt. One of a newer format is refused as newer, not as broken.
    #[test]
    fn a_database_without_identity_or_of_a_newer_format_is_not_served() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bsl-graph.db");
        let create = || {
            let _ = std::fs::remove_file(&path);
            GraphDbWriter::create(&path)
                .unwrap()
                .finalize(&GraphMeta {
                    revision: 1,
                    fingerprint: crate::graph_db::GraphFp::default(),
                    files: 0,
                    built_at: "t".to_string(),
                    publication_id: "test-1".to_owned(),
                })
                .unwrap();
        };

        create();
        assert_eq!(GraphDb::open(&path).unwrap().publication_id().unwrap(), "test-1");

        Connection::open(&path)
            .unwrap()
            .execute("DELETE FROM meta WHERE key = 'publication_id'", [])
            .unwrap();
        let missing = GraphDb::open(&path).err().expect("no identity, no service").to_string();
        assert!(missing.contains("publication_id"), "{missing}");

        create();
        Connection::open(&path)
            .unwrap()
            .execute(
                "UPDATE meta SET value = ?1 WHERE key = 'schema_version'",
                params![(crate::graph_db::SCHEMA_VERSION + 1).to_string()],
            )
            .unwrap();
        let newer = GraphDb::open(&path).err().expect("a newer format is refused").to_string();
        assert!(newer.contains("newer"), "{newer}");
    }

    /// A subsystem's `<Content>` puts a common module into the graph as an `mdo`
    /// node. It is the same entity the resident publishes as a common module, and
    /// only a shared category lets the two rows meet.
    #[test]
    fn a_common_module_reached_through_a_subsystem_keeps_its_own_category() {
        assert_eq!(
            node_category("mdo", "mdo/CommonModule/Настройки"),
            ide::NameCategory::CommonModule,
        );
        assert_eq!(node_category("mdo", "mdo/Catalog/Товары"), ide::NameCategory::MetadataObject,);
    }

    /// `file` says where a node is defined, not that it holds BSL source. An
    /// object carries one now, and the reverse lookup must still answer with the
    /// files that REFERENCE the object — never with the object's own XML, which a
    /// `contains` edge would otherwise hand back.
    #[test]
    fn an_objects_own_file_is_not_a_file_that_references_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bsl-graph.db");
        let mut writer = GraphDbWriter::create(&path).unwrap();
        writer
            .write_nodes(&[
                ide::graph_index::NodeRow {
                    id: "method/common/Вызов/Читать".to_string(),
                    kind: "method",
                    name: "Читать".to_string(),
                    qualified: "ОбщийМодуль.Вызов.Читать".to_string(),
                    module: Some("ОбщийМодуль.Вызов".to_string()),
                    file: Some("CommonModules/Вызов/Ext/Module.bsl".to_string()),
                    name_offset: None,
                    sig_end: None,
                    src_start: None,
                    src_end: None,
                    dispatch: Vec::new(),
                    is_export: Some(true),
                    addressable: true,
                },
                ide::graph_index::NodeRow {
                    id: "mdo/Catalog/Товары".to_string(),
                    kind: "mdo",
                    name: "Товары".to_string(),
                    qualified: "Справочник.Товары".to_string(),
                    module: None,
                    file: Some("Catalogs/Товары.xml".to_string()),
                    name_offset: None,
                    sig_end: None,
                    src_start: None,
                    src_end: None,
                    dispatch: Vec::new(),
                    is_export: None,
                    addressable: true,
                },
            ])
            .unwrap();
        writer
            .write_edges(&[
                ide::graph_index::EdgeRow {
                    from_id: "method/common/Вызов/Читать".to_string(),
                    to_id: "mdo/Catalog/Товары".to_string(),
                    kind: "read",
                    provenance: "resolved",
                    call_start: None,
                    call_end: None,
                    call_site_absent: Some(ide::NO_CALL_SITE),
                    crosses: false,
                },
                // The object contains its own attribute, and the edge points back
                // at the object exactly as the catalog pass writes it.
                ide::graph_index::EdgeRow {
                    from_id: "mdo/Catalog/Товары".to_string(),
                    to_id: "attribute/Catalog/Товары/Код".to_string(),
                    kind: "contains",
                    provenance: "resolved",
                    call_start: None,
                    call_end: None,
                    call_site_absent: Some(ide::NO_CALL_SITE),
                    crosses: false,
                },
            ])
            .unwrap();
        writer
            .finalize(&GraphMeta {
                revision: 1,
                fingerprint: crate::graph_db::GraphFp::default(),
                files: 0,
                built_at: "t".to_string(),
                publication_id: "test-1".to_owned(),
            })
            .unwrap();

        let db = GraphDb::open(&path).unwrap();
        let roots = bsl_search::WorkspaceRoots::build(dir.path(), dir.path(), &[]).0;
        assert!(
            db.referencing_files("mdo/Catalog/Товары", None).unwrap().is_empty(),
            "without roots a relative stored path must fail closed"
        );
        assert_eq!(
            db.referencing_files("mdo/Catalog/Товары", Some(&roots)).unwrap(),
            vec![dir
                .path()
                .join("CommonModules/Вызов/Ext/Module.bsl")
                .to_string_lossy()
                .into_owned()],
        );
    }

    #[test]
    fn stored_callback_edge_kinds_round_trip_not_collapsed_to_call() {
        // Regression: the normalizers fell through to "call"/"resolved" for unknown
        // stored strings, so persisted callback edges served as plain calls.
        assert_eq!(edge_kind("notify_ref"), "notify_ref");
        assert_eq!(edge_kind("idle_handler"), "idle_handler");
        assert_eq!(edge_kind("event_subscription"), "event_subscription");
        assert_eq!(provenance("string_resolved"), "string_resolved");
        // The catch-all still normalizes a genuinely unknown string.
        assert_eq!(edge_kind("call"), "call");
        assert_eq!(provenance("resolved"), "resolved");
    }

    /// A minimal in-memory graph holding only the `nodes` columns `resolve` reads.
    fn graph_db_with_nodes(rows: &[(&str, &str)]) -> GraphDb {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE nodes (id TEXT NOT NULL, kind TEXT NOT NULL);").unwrap();
        for (id, kind) in rows {
            conn.execute("INSERT INTO nodes (id, kind) VALUES (?1, ?2)", params![id, kind])
                .unwrap();
        }
        GraphDb::from_connection(conn)
    }

    /// The reverse lookup must ride the `edges_to` index (equality + half-open range on
    /// `to_id`), never a full table scan — a metadata edit on a hot object would otherwise
    /// scan every edge in a 25k-module graph. `EXPLAIN QUERY PLAN` proves the planner uses
    /// `edges_to` for the driving edge search.
    #[test]
    fn referencing_files_query_uses_the_edges_to_index() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE nodes (id TEXT NOT NULL, kind TEXT NOT NULL, file_root_id TEXT, file_path TEXT);\
             CREATE TABLE edges (from_id TEXT NOT NULL, to_id TEXT NOT NULL, kind TEXT NOT NULL);\
             CREATE INDEX edges_to ON edges(to_id);\
             CREATE INDEX edges_from ON edges(from_id);",
        )
        .unwrap();
        let db = GraphDb::from_connection(conn);
        let plan: Vec<String> = db
            .conn
            .prepare(&format!("EXPLAIN QUERY PLAN {}", super::REFERENCING_FILES_SQL))
            .unwrap()
            .query_map(
                params![
                    "mdo/Catalog/Товары",
                    "attribute/Catalog/Товары/",
                    "attribute/Catalog/Товары0"
                ],
                |r| r.get::<_, String>(3),
            )
            .unwrap()
            .map(Result::unwrap)
            .collect();
        let plan_text = plan.join("\n");
        // Both UNION arms drive from the edges_to index; neither full-scans a table.
        assert_eq!(
            plan_text.matches("edges_to").count(),
            2,
            "both inbound-edge arms must use the edges_to index: {plan_text}"
        );
        assert!(
            !plan_text.contains("SCAN edges") && !plan_text.contains("SCAN nodes"),
            "neither the edges nor the nodes table may be full-scanned: {plan_text}"
        );
    }

    /// Real inbound read edges → the referencing methods' distinct files; a range over the
    /// object's `attribute/…` ids is included, and an unrelated object's edges are excluded.
    #[test]
    fn referencing_files_returns_readers_of_mdo_and_its_attributes() {
        let dir = tempfile::tempdir().unwrap();
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE nodes (id TEXT NOT NULL, kind TEXT NOT NULL, file_root_id TEXT, file_path TEXT);\
             CREATE TABLE edges (from_id TEXT NOT NULL, to_id TEXT NOT NULL, kind TEXT NOT NULL);\
             CREATE INDEX edges_to ON edges(to_id);",
        )
        .unwrap();
        let node = |id: &str, file: &str| {
            conn.execute(
                "INSERT INTO nodes (id, kind, file_root_id, file_path) VALUES (?1, 'method', '', ?2)",
                params![id, file],
            )
            .unwrap();
        };
        node("method/common/Б/Ч", "CommonModules/Б/Ext/Module.bsl");
        node("method/common/Г/Ч", "CommonModules/Г/Ext/Module.bsl");
        node("method/common/В/Н", "CommonModules/В/Ext/Module.bsl");
        let edge = |from: &str, to: &str, kind: &str| {
            conn.execute(
                "INSERT INTO edges (from_id, to_id, kind) VALUES (?1, ?2, ?3)",
                params![from, to, kind],
            )
            .unwrap();
        };
        // Б reads the object directly; Г reads one of its attributes; В reads a DIFFERENT
        // object entirely and must not surface.
        edge("method/common/Б/Ч", "mdo/Catalog/Товары", "manager_access");
        edge("method/common/Г/Ч", "attribute/Catalog/Товары/Код", "query_ref");
        edge("method/common/В/Н", "mdo/Catalog/Другой", "manager_access");

        let db = GraphDb::from_connection(conn);
        let roots = bsl_search::WorkspaceRoots::build(dir.path(), dir.path(), &[]).0;
        assert!(
            db.referencing_files("mdo/Catalog/Товары", None).unwrap().is_empty(),
            "without roots a relative stored path must fail closed"
        );
        let mut files = db.referencing_files("mdo/Catalog/Товары", Some(&roots)).unwrap();
        files.sort();
        assert_eq!(
            files,
            vec![
                dir.path()
                    .join("CommonModules/Б/Ext/Module.bsl")
                    .to_string_lossy()
                    .into_owned(),
                dir.path()
                    .join("CommonModules/Г/Ext/Module.bsl")
                    .to_string_lossy()
                    .into_owned(),
            ],
            "readers of the object and its attributes are returned; an unrelated object's reader is not"
        );
        assert!(
            db.referencing_files("method/common/Б/Ч", Some(&roots)).unwrap().is_empty(),
            "a non-mdo id yields nothing"
        );
    }

    #[test]
    fn resolve_recovers_wrong_cased_module_id_from_member_methods() {
        // Module nodes are synthesized on demand and not stored; only the methods are. A
        // wrong-cased `module/...` query must still resolve via the owning-module id derived
        // from a member method (the bug: it previously found nothing).
        let db = graph_db_with_nodes(&[(
            "method/common/СтроковыеФункцииКлиентСервер/ПодставитьПараметрыВСтроку",
            "method",
        )]);
        let res = db.resolve("module/common/строковыефункцииклиентсервер", 10).unwrap();
        let module = res
            .candidates
            .iter()
            .find(|c| c.kind == "module")
            .expect("a module candidate is derived from the member method");
        assert_eq!(module.id, "module/common/СтроковыеФункцииКлиентСервер");
        assert_eq!(module.match_kind, "case_insensitive");
    }

    #[test]
    fn resolve_does_not_duplicate_a_stored_module() {
        // A module that is BOTH stored (an edge endpoint) and derivable from its methods must
        // appear once, not twice — the derived id is deduped against the stored set.
        let db = graph_db_with_nodes(&[
            ("module/common/Сервер", "module"),
            ("method/common/Сервер/Считать", "method"),
        ]);
        let res = db.resolve("module/common/Сервер", 10).unwrap();
        let modules: Vec<_> =
            res.candidates.iter().filter(|c| c.id == "module/common/Сервер").collect();
        assert_eq!(modules.len(), 1);
        assert_eq!(modules[0].match_kind, "exact");
    }

    /// The dictionary must not narrow what `resolve` accepts: all four id tiers
    /// keep working when the graph answers through the merge instead of
    /// serialising its own result.
    #[test]
    fn every_id_tier_survives_the_trip_through_the_name_source() {
        use ide::ExternalNameSource;

        let db = graph_db_with_nodes(&[(
            "method/common/СтроковыеФункцииКлиентСервер/ПодставитьПараметрыВСтроку",
            "method",
        )]);
        let source = GraphNameSource::answering(&db, None);

        let tier_of = |query: &str| {
            source
                .candidates(query, 10)
                .unwrap()
                .candidates
                .first()
                .map(|c| c.match_tier)
                .unwrap_or_else(|| panic!("`{query}` found nothing"))
        };

        assert_eq!(
            tier_of("method/common/СтроковыеФункцииКлиентСервер/ПодставитьПараметрыВСтроку"),
            ide::NameMatchTier::Exact,
        );
        assert_eq!(
            tier_of("method/common/строковыефункцииклиентсервер/подставитьпараметрывстроку"),
            ide::NameMatchTier::CaseInsensitive,
        );
        assert_eq!(tier_of("ПодставитьПараметрыВСтроку"), ide::NameMatchTier::Name);
        assert_eq!(tier_of("ПараметрыВСтр"), ide::NameMatchTier::Substring);
    }

    /// A module node is a common module only when its scope says so; calling an
    /// object module "callable by name" would publish a category its consumer
    /// would act on.
    #[test]
    fn a_module_node_is_categorised_by_its_scope_not_its_kind() {
        assert_eq!(
            node_category("module", "module/common/Сервер"),
            ide::NameCategory::CommonModule,
        );
        assert_eq!(
            node_category("module", "module/object/Catalog/Товары"),
            ide::NameCategory::Module,
        );
        assert_eq!(node_category("mdo", "mdo/Catalog/Товары"), ide::NameCategory::MetadataObject);
        assert_eq!(
            node_category("attribute", "attribute/Catalog/Товары/Код"),
            ide::NameCategory::MetadataMember,
        );
        assert_eq!(
            node_category("form_item", "form_item/Catalog/Товары/Форма/Кнопка"),
            ide::NameCategory::Form,
        );
    }

    /// The count the merge relies on: what the graph withheld has to be visible,
    /// or an answer capped at the limit reads as exhaustive.
    #[test]
    fn the_source_reports_what_it_did_not_hand_over() {
        use ide::ExternalNameSource;

        let rows: Vec<(String, &str)> = (0..8)
            .map(|i| (format!("method/common/М{i}/ПриСозданииНаСервере"), "method"))
            .collect();
        let borrowed: Vec<(&str, &str)> =
            rows.iter().map(|(id, kind)| (id.as_str(), *kind)).collect();
        let db = graph_db_with_nodes(&borrowed);
        let source = GraphNameSource::answering(&db, None);

        let hits = source.candidates("ПриСозданииНаСервере", 3).unwrap();
        assert_eq!(hits.candidates.len(), 3);
        assert_eq!(hits.total, 8, "the pre-cap count, not the delivered count");
    }

    /// An absent graph reports its reason instead of an empty answer that would
    /// read as a proven zero.
    #[test]
    fn an_absent_graph_names_its_state() {
        use ide::ExternalNameSource;

        let source = GraphNameSource::absent(ide::ProviderState::NotReady);
        assert_eq!(source.state(), ide::ProviderState::NotReady);
        assert_eq!(source.provider(), ide::ProviderId::Graph);
    }

    #[test]
    fn method_id_range_covers_each_module_form() {
        // Common/manager/object modules use the `/` member separator.
        let (lo, hi) = method_id_range("module/common/Сервер").unwrap();
        assert_eq!(lo, "method/common/Сервер/");
        assert_eq!(hi, "method/common/Сервер0"); // '/' (0x2F) bumped to '0' (0x30)
        assert!("method/common/Сервер/Считать" >= lo.as_str());
        assert!("method/common/Сервер/Считать" < hi.as_str());
        // A sibling scope (longer name sharing the prefix) is NOT in range.
        assert!("method/common/СерверДва/М" >= hi.as_str());

        let (lo, _) = method_id_range("module/manager/Catalog/Товары").unwrap();
        assert_eq!(lo, "method/manager/Catalog/Товары/");

        // File modules use the `::` member separator.
        let (lo, hi) = method_id_range("module/file/src/cf/Forms/A/Module.bsl").unwrap();
        assert_eq!(lo, "method/file/src/cf/Forms/A/Module.bsl::");
        assert_eq!(hi, "method/file/src/cf/Forms/A/Module.bsl:;"); // ':' bumped to ';'
        assert!("method/file/src/cf/Forms/A/Module.bsl::ПриОткрытии" >= lo.as_str());
        assert!("method/file/src/cf/Forms/A/Module.bsl::ПриОткрытии" < hi.as_str());

        // Not a module id (no `module/` prefix), and an empty scope.
        assert!(method_id_range("method/common/X/Y").is_none());
        assert!(method_id_range("module/").is_none());
    }
}
