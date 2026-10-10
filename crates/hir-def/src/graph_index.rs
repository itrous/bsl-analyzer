//! Resident, body-free "Pass-A" index for building the whole-config call graph
//! with bounded RAM, plus an index-backed projection that mirrors the Salsa
//! [`workspace_call_graph_query`](crate::queries::workspace_call_graph_query)
//! without lowering every module's bodies into one database at once.
//!
//! Resolving a qualified or manager call needs only the target module's method
//! table (folded name → first `{local_id, is_export}`); the rest — config
//! visibility and the path-based [`ModuleIndex`](crate::module_index::ModuleIndex)
//! — is already cheap and path-only. [`GraphIndex`] holds that method table for
//! every module so a streaming, batched build can resolve cross-batch targets
//! without keeping other modules' Salsa symbol trees resident.
//!
//! The index-backed resolution reuses the resolver's `locate_*` prefixes (config
//! visibility + path index, identical to the Salsa path) and swaps only the final
//! method lookup for a [`GraphIndex`] read. A golden-equivalence test
//! (`ide-db`) asserts the result is identical to the Salsa fold.

use intern::NormName;
use std::path::Path;
use stdx::case::CaseExt;

use rustc_hash::{FxHashMap, FxHashSet};

use bsl_metadata::MdoType;
use vfs::FileId;

use crate::{
    call_graph::{
        CallSite, EdgeKind, EdgeProvenance, GraphMethodEntry, GraphNode, MethodDispatch,
        ResolvedCallEdge, ResolvedModuleSummary, ResolvedTarget, WorkspaceCallEdge,
        WorkspaceCallGraph, CALL_SITE_NOT_RECORDED, NO_CALL_SITE,
    },
    call_hierarchy_index::MethodCallPair,
    configs::{BodySearch, ConfigsDatabase},
    module_index::{module_key_for_path, ModuleKey},
    name::Name,
    resolver::Resolver,
    CallHierarchyReverseIndex, MethodId, MethodKey, ModuleId,
};

/// A module's methods as seen from the item tree alone (no body lowering).
/// `by_name` serves resolution; `all` carries the declaration facts (name, export,
/// ranges) that node materialisation needs. Dispatch lives in
/// [`GraphIndex::node_dispatch`].
struct ModuleMethods {
    /// Folded name → first declaration, mirroring `SymbolTree::find_method`.
    by_name: FxHashMap<NormName, MethodRef>,
    /// Every method in declaration order.
    all: Vec<GraphMethodEntry>,
    /// Whether this module's body could be read, as answered by the database that
    /// actually read it. Recorded here because the index is what the batches share:
    /// a batch database registers inputs only for its own files, so asking IT about
    /// a module from another batch gets "readable" for everything.
    unread: bool,
}

#[derive(Clone, Copy)]
struct MethodRef {
    local_id: MethodKey,
    is_export: bool,
}

/// The compact, resident method index over a set of modules, plus the per-method
/// client/server dispatch table (the fold's Pass-1 data).
#[derive(Default)]
pub struct GraphIndex {
    methods: FxHashMap<ModuleId, ModuleMethods>,
    /// Per-method dispatch (module execution context wins, else annotation),
    /// resident so a batched build can flag client→server edges without rebuilding
    /// the whole graph's dispatch table per batch.
    node_dispatch: FxHashMap<MethodId, MethodDispatch>,
}

fn signature_hash(header: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    header.hash(&mut hasher);
    hasher.finish()
}

impl GraphIndex {
    /// An empty index; populate with [`Self::add_module`] (e.g. one batch's modules
    /// at a time in a fresh database) to build the whole-config index without ever
    /// holding every module's item tree resident.
    pub fn new() -> Self {
        Self::default()
    }

    /// Build the index for `modules` in one pass. See [`Self::add_module`]; this is
    /// the convenience path when every module's text is already in `db`.
    ///
    /// `modules` must cover **every** module that could be a resolution target,
    /// not just the ones whose edges are projected: a qualified/manager call into a
    /// module absent from the index falls into the method-absent arm (→ Unresolved
    /// / Mdo) instead of resolving. A batched build therefore indexes the whole
    /// configuration, even though it lowers bodies one batch at a time later.
    pub fn build(db: &dyn ConfigsDatabase, modules: &[ModuleId]) -> Self {
        let mut index = Self::new();
        for &module in modules {
            index.add_module(db, module);
        }
        index
    }

    /// Add one module's compact method table + dispatch from the item tree and
    /// module metadata only — no body lowering. The heavy `item_tree` is transient,
    /// so building the whole index batch-by-batch in fresh databases keeps peak RAM
    /// bounded.
    pub fn add_module(&mut self, db: &dyn ConfigsDatabase, module: ModuleId) {
        let (all, module_dispatch, unread) = Self::extract_module_data(db, module);
        self.insert_module_data(module, all, module_dispatch, unread);
    }

    /// Add a whole batch's modules, lowering each module's item tree + metadata in
    /// parallel on `pool` (the per-module cost of [`Self::add_module`], repeated
    /// across the config), then folding the results into the index sequentially. The
    /// index is a set of per-module maps, so insertion order does not affect it —
    /// only the read-only extraction is parallelised.
    pub fn add_batch<DB: ConfigsDatabase + Clone + Send>(
        &mut self,
        pool: &rayon::ThreadPool,
        db: &DB,
        batch: &[ModuleId],
    ) {
        let extracted = parallel_per_module(pool, db, batch, |db, module| {
            (module, Self::extract_module_data(db, module))
        });
        for (module, (all, module_dispatch, unread)) in extracted {
            self.insert_module_data(module, all, module_dispatch, unread);
        }
    }

    /// [`Self::add_batch`] fused with per-module extraction of the pair-relevant
    /// call-summary subset (see [`ModuleCallSummary::method_pair_subset`]), in the
    /// same parallel region — so each module's parse tree is forced once and serves
    /// both the item-tree extraction and the body lowering, instead of being
    /// re-parsed by a second workspace pass. Returns the retained subsets in
    /// `batch` order for deferred pair resolution against the completed index.
    pub fn add_batch_extracting_pair_intents<DB: ConfigsDatabase + Clone + Send>(
        &mut self,
        pool: &rayon::ThreadPool,
        db: &DB,
        batch: &[ModuleId],
    ) -> Vec<(ModuleId, crate::call_graph::ModuleCallSummary)> {
        extract_batch_index_and_pair_intents(pool, db, batch)
            .into_iter()
            .map(|extraction| self.insert_extraction(extraction))
            .collect()
    }

    /// The fold half of [`extract_batch_index_and_pair_intents`]: index one
    /// module's extracted declarations and hand back its retained pair intents.
    /// Order-independent across modules, so a pipelined build may fold batches in
    /// any completion order.
    pub fn insert_extraction(
        &mut self,
        extraction: ModuleIndexExtraction,
    ) -> (ModuleId, crate::call_graph::ModuleCallSummary) {
        let ModuleIndexExtraction { module, entries, module_dispatch, unread, pair_intents } =
            extraction;
        self.insert_module_data(module, entries, module_dispatch, unread);
        (module, pair_intents)
    }

    /// The read-only half of [`Self::add_module`]: force a module's item tree and
    /// metadata and extract its method entries + module-level dispatch. Touches no
    /// shared index state, so it is safe to run for many modules concurrently.
    fn extract_module_data(
        db: &dyn ConfigsDatabase,
        module: ModuleId,
    ) -> (Vec<GraphMethodEntry>, Option<MethodDispatch>, bool) {
        let item_tree = db.item_tree(module.file_id);
        let text = db.file_text(module.file_id);
        let mut all = crate::call_graph::extract_graph_methods(&item_tree);
        for entry in &mut all {
            let start = u32::from(entry.name_range.start()) as usize;
            let end = u32::from(entry.sig_end) as usize;
            entry.signature_hash = text.get(start..end).map_or(0, signature_hash);
        }
        let module_dispatch = db
            .module_metadata(module)
            .execution_context
            .and_then(MethodDispatch::from_execution_context);
        (all, module_dispatch, db.file_is_unread(module.file_id))
    }

    /// The mutating half of [`Self::add_module`]: fold one module's extracted methods
    /// + dispatch into the resident index. Order-independent across modules.
    fn insert_module_data(
        &mut self,
        module: ModuleId,
        all: Vec<GraphMethodEntry>,
        module_dispatch: Option<MethodDispatch>,
        unread: bool,
    ) {
        // First-wins folded map, matching `SymbolTree::find_method`.
        let mut by_name = FxHashMap::default();
        for entry in &all {
            by_name
                .entry(entry.local_id.name)
                .or_insert(MethodRef { local_id: entry.local_id, is_export: entry.is_export });
            // Pass-1 dispatch rule: module execution context wins, else annotation.
            self.node_dispatch.insert(
                MethodId { module, local_id: entry.local_id },
                module_dispatch.unwrap_or(entry.dispatch),
            );
        }
        self.methods.insert(module, ModuleMethods { by_name, all, unread });
    }

    /// The declaration facts (name, export, dispatch, ranges) for a method, for
    /// node materialisation. `None` if the module/method is not indexed.
    pub fn method_entry(&self, method: MethodId) -> Option<&GraphMethodEntry> {
        self.methods.get(&method.module)?.all.iter().find(|e| e.local_id == method.local_id)
    }

    /// A body-free signature hash of one module's methods: the ordered declaration
    /// headers (including parameters), export flags, and effective dispatch of every method in
    /// declaration order. This is exactly the cross-module resolution + identity
    /// surface — `find_method` resolves on the name, callers' edges/boundary flags
    /// depend on `is_export` + effective dispatch, and the durable method id embeds
    /// the original name spelling. Readability is hashed alongside them for the same
    /// reason: an unread body bars callers from resolving into any body behind it, and
    /// an empty readable body declares exactly the same nothing an unread one does — so
    /// without it the transition between them looks like no change at all. So if this
    /// hash is unchanged across an edit, no caller's resolved edge or stored node row
    /// can change and only this module's own rows need reprojecting. Deliberately excludes source ranges (they shift on any
    /// text edit) and bodies (a body edit not touching a signature keeps it stable).
    /// `None` if the module is not indexed.
    ///
    /// The hasher (std `DefaultHasher`) is stable within a build but not guaranteed
    /// across toolchain versions. That is safe by construction: a changed algorithm
    /// only makes the stored hashes mismatch on the next reload, which falls back to a
    /// full rebuild and re-persists fresh hashes — the same self-healing contract the
    /// workspace fingerprint already relies on for cache reuse.
    pub fn module_sig_hash(&self, module: ModuleId) -> Option<u64> {
        use std::hash::{Hash, Hasher};

        let methods = self.methods.get(&module)?;
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        methods.unread.hash(&mut hasher);
        for entry in &methods.all {
            entry.name.as_str().hash(&mut hasher);
            entry.signature_hash.hash(&mut hasher);
            entry.is_export.hash(&mut hasher);
            // The effective dispatch (module execution context wins, else annotation)
            // is what the stored node row and the client→server edge flag carry, so it
            // is the dispatch the parity surface sees — not the raw annotation.
            match self.node_dispatch.get(&MethodId { module, local_id: entry.local_id }) {
                Some(d) => {
                    true.hash(&mut hasher);
                    d.can_run_on_client.hash(&mut hasher);
                    d.can_run_on_server.hash(&mut hasher);
                    d.no_context.hash(&mut hasher);
                }
                None => false.hash(&mut hasher),
            }
        }
        Some(hasher.finish())
    }

    /// A body-free resident layout hash of one module's methods: the ordered
    /// (key, original-spelling name, `is_export`, effective dispatch) of every
    /// method in declaration order, plus readability. It deliberately includes the
    /// method key, unlike [`Self::module_sig_hash`], so resident identity can
    /// detect a change in which method is which (a namesake added above one)
    /// without widening the durable contract. A module variable added above the
    /// methods moves neither hash: it is not a method and renumbers none.
    /// Readability is in it because the call-hierarchy catch-up
    /// treats an unchanged hash as proof that the resident index still resolves other
    /// modules' calls the same way, and crossing the unread barrier breaks exactly
    /// that.
    /// Excludes source ranges and bodies. `None` if the module is not indexed.
    pub fn module_layout_hash(&self, module: ModuleId) -> Option<u64> {
        use std::hash::{Hash, Hasher};

        let methods = self.methods.get(&module)?;
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        methods.unread.hash(&mut hasher);
        for entry in &methods.all {
            entry.local_id.hash(&mut hasher);
            entry.name.as_str().hash(&mut hasher);
            entry.is_export.hash(&mut hasher);
            match self.node_dispatch.get(&MethodId { module, local_id: entry.local_id }) {
                Some(d) => {
                    true.hash(&mut hasher);
                    d.can_run_on_client.hash(&mut hasher);
                    d.can_run_on_server.hash(&mut hasher);
                    d.no_context.hash(&mut hasher);
                }
                None => false.hash(&mut hasher),
            }
        }
        Some(hasher.finish())
    }

    /// A module's methods as `(original-spelling name, is_export)` in declaration
    /// order, for the incremental caller-delta eligibility check (comparing the
    /// resolvable name surface across an edit). `None` if the module is not indexed.
    pub fn module_methods(&self, module: ModuleId) -> Option<Vec<(String, bool)>> {
        Some(
            self.methods
                .get(&module)?
                .all
                .iter()
                .map(|e| (e.name.as_str().to_string(), e.is_export))
                .collect(),
        )
    }

    /// Every indexed method node. This is exactly the fold's Pass-1 dispatch-seeded
    /// set (`node_dispatch.keys`), so a streaming build that materialises a node for
    /// each yields the same isolated (call-free) method nodes the in-memory graph's
    /// [`nodes`](WorkspaceCallGraph::nodes) exposes, not only edge endpoints.
    pub fn method_nodes(&self) -> impl Iterator<Item = MethodId> + '_ {
        self.node_dispatch.keys().copied()
    }

    /// One module's method declaration facts in declaration order, for a streaming
    /// consumer that needs each method's name/ranges/dispatch without re-scanning the
    /// whole-workspace node set per module. `None` if the module is not indexed.
    pub fn module_method_entries(&self, module: ModuleId) -> Option<&[GraphMethodEntry]> {
        self.methods.get(&module).map(|m| m.all.as_slice())
    }

    /// Method lookup mirroring `SymbolTree::find_method` (lowercased, first-wins).
    /// Returns the same `{local_id, is_export}` the Salsa symbol tree would, so the
    /// reconstructed `MethodId` is identical.
    fn find_method(&self, target: ModuleId, name: &Name) -> Option<MethodRef> {
        self.methods.get(&target)?.by_name.get(&NormName::intern(name.as_str())).copied()
    }

    /// Whether `target`'s body could be read, as recorded by the database that indexed
    /// it — `None` for a module this index never covered.
    ///
    /// The batched build asks this instead of the database it happens to hold: that
    /// database registered inputs only for its own batch, so it reports every module
    /// from another batch as readable, and resolution would depend on how the build
    /// was split rather than on the source root and configuration alone.
    pub fn is_unread(&self, target: ModuleId) -> Option<bool> {
        self.methods.get(&target).map(|m| m.unread)
    }

    /// Resident per-node dispatch — the same value the fold's seeded graph returns
    /// (`None` for non-method nodes), so the client→server boundary flag matches.
    pub fn dispatch(&self, node: &GraphNode) -> Option<MethodDispatch> {
        match node {
            GraphNode::Method(method_id) => self.node_dispatch.get(method_id).copied(),
            _ => None,
        }
    }

    /// Populate `graph`'s per-method dispatch table (the fold's Pass 1).
    fn seed_dispatch(&self, graph: &mut WorkspaceCallGraph) {
        for (&method_id, &dispatch) in &self.node_dispatch {
            graph.set_dispatch(GraphNode::Method(method_id), dispatch);
        }
    }
}

/// Resolve a module's raw call edges against `index`, producing the same
/// [`ResolvedModuleSummary`] the Salsa `resolved_module_summary_query` would — but
/// with method lookup served from the resident index rather than the target
/// modules' Salsa symbol trees. Forces only this module's
/// [`module_call_summary`](crate::call_graph::extract_call_summary) (its own
/// bodies); cross-module targets are resolved through `index`.
pub fn resolve_module_summary_via_index(
    db: &dyn ConfigsDatabase,
    module: ModuleId,
    index: &GraphIndex,
) -> ResolvedModuleSummary {
    let summary = db.module_call_summary(module);
    resolve_summary_via_index(db, module, &summary, index)
}

/// The resolution core of [`resolve_module_summary_via_index`], parameterized over
/// an already-extracted summary. This lets a deferred consumer (the fused
/// call-hierarchy build) resolve a summary subset retained from an earlier batch
/// pass long after that batch's database is gone: everything read from `db` here is
/// path/configuration state (module index, config metadata), never module texts, so
/// any database with the same source root and config inputs resolves identically.
pub fn resolve_summary_via_index(
    db: &dyn ConfigsDatabase,
    module: ModuleId,
    summary: &crate::call_graph::ModuleCallSummary,
    index: &GraphIndex,
) -> ResolvedModuleSummary {
    use crate::call_graph::CallTarget;

    let resolver = Resolver::with_workspace_scope(module);

    let mut edges = Vec::with_capacity(summary.call_edges.len());
    for edge in &summary.call_edges {
        let (target, provenance, kind) = match &edge.target {
            CallTarget::Local { callee_local_id } => (
                ResolvedTarget::Method(MethodId { module, local_id: *callee_local_id }),
                EdgeProvenance::Resolved,
                edge.kind,
            ),
            CallTarget::QualifiedModule { module_name, method_name } => {
                match resolver
                    .locate_common_module_candidates(db, module_name)
                    .map(|c| c.reflagged(|m| index.is_unread(m)))
                {
                    // Base body first, then the caller's own extension body, stopping
                    // where `resolve_qualified_method` stops — including at an unread
                    // body, or this route would resolve what the Salsa fold leaves
                    // unresolved on the same input.
                    Ok(candidates) => match candidates
                        .search(|m| index.find_method(m, method_name).map(|hit| (m, hit)))
                    {
                        BodySearch::Found((target_module, m)) if m.is_export => (
                            ResolvedTarget::Method(MethodId {
                                module: target_module,
                                local_id: m.local_id,
                            }),
                            EdgeProvenance::Resolved,
                            edge.kind,
                        ),
                        // Found but not exported → visible-but-unreachable.
                        BodySearch::Found(_) => (
                            ResolvedTarget::Unresolved(edge.target.clone()),
                            EdgeProvenance::VisibilityBlocked,
                            edge.kind,
                        ),
                        // Method absent from every readable body, or a body ahead of
                        // it could not be read at all.
                        BodySearch::Absent | BodySearch::Unread => (
                            ResolvedTarget::Unresolved(edge.target.clone()),
                            EdgeProvenance::Unresolved,
                            edge.kind,
                        ),
                    },
                    // Not visible / module not found.
                    Err(_) => (
                        ResolvedTarget::Unresolved(edge.target.clone()),
                        EdgeProvenance::Unresolved,
                        edge.kind,
                    ),
                }
            }
            CallTarget::ManagerAccess {
                manager_type,
                object_name,
                method_name: Some(method_name),
            } => {
                let to_mdo = || ResolvedTarget::Mdo {
                    mdo_type: manager_type.to_mdo_type(),
                    object_name: object_name.clone(),
                };
                match resolver
                    .locate_manager_module(db, *manager_type, object_name)
                    .map(|c| c.reflagged(|m| index.is_unread(m)))
                {
                    Ok(candidates) => match candidates
                        .search(|m| index.find_method(m, method_name).map(|hit| (m, hit)))
                    {
                        // A user manager-module method on a fully-literal
                        // `Коллекция.Объект.Метод()` path: the object name is a token and its
                        // manager module is uniquely determined, so locating the exported method
                        // is a direct lookup — as trustworthy as a qualified `Модуль.Метод()`
                        // call. The edge is about the method.
                        BodySearch::Found((target_module, m)) if m.is_export => (
                            ResolvedTarget::Method(MethodId {
                                module: target_module,
                                local_id: m.local_id,
                            }),
                            EdgeProvenance::Resolved,
                            edge.kind,
                        ),
                        BodySearch::Found(_) => (
                            ResolvedTarget::Unresolved(edge.target.clone()),
                            EdgeProvenance::VisibilityBlocked,
                            edge.kind,
                        ),
                        // No user method → a platform manager method touching the object.
                        // `Unread` joins `Absent` here, and this is where the manager route
                        // parts with `QualifiedModule` above: a manager call has a legitimate
                        // answer that does not depend on any body at all, so an unreadable
                        // module leaves the platform reading intact instead of erasing the
                        // edge. The qualified-module branch has no such fallback.
                        BodySearch::Absent | BodySearch::Unread => (
                            to_mdo(),
                            EdgeProvenance::Inferred,
                            crate::queries::manager_edge_kind(*manager_type, method_name.as_str()),
                        ),
                    },
                    // No manager module → a platform manager method.
                    Err(_) => (
                        to_mdo(),
                        EdgeProvenance::Inferred,
                        crate::queries::manager_edge_kind(*manager_type, method_name.as_str()),
                    ),
                }
            }
            CallTarget::ManagerAccess { manager_type, object_name, method_name: None } => (
                ResolvedTarget::Mdo {
                    mdo_type: manager_type.to_mdo_type(),
                    object_name: object_name.clone(),
                },
                EdgeProvenance::Inferred,
                EdgeKind::ManagerAccess,
            ),
            // A `Движения.<Регистр>` movement touch: resolve the register name to its
            // metadata type from config, identical to the Salsa fold so the graphs match.
            CallTarget::RegisterMovement { register_name } => {
                match resolver.resolve_register_by_name(db, register_name) {
                    Some((mdo_type, object_name)) => (
                        ResolvedTarget::Mdo { mdo_type, object_name },
                        EdgeProvenance::Inferred,
                        EdgeKind::RegisterMovement,
                    ),
                    None => (
                        ResolvedTarget::Unresolved(edge.target.clone()),
                        EdgeProvenance::Unresolved,
                        edge.kind,
                    ),
                }
            }
            CallTarget::ThisObjectMethod { .. } | CallTarget::Unresolved => (
                ResolvedTarget::Unresolved(edge.target.clone()),
                EdgeProvenance::Unresolved,
                edge.kind,
            ),
        };

        edges.push(ResolvedCallEdge {
            caller: edge.caller,
            target,
            kind,
            range: edge.range,
            provenance,
        });
    }

    // Resolve string-dispatched callbacks through the resident index, mirroring the
    // qualified-call strategy above so the result is byte-identical to the Salsa fold.
    let find_local = |name: &crate::name::Name| {
        index.find_method(module, name).map(|m| MethodId { module, local_id: m.local_id })
    };
    let find_qualified =
        |module_name: &crate::name::Name, method_name: &crate::name::Name| match resolver
            .locate_common_module_candidates(db, module_name)
            .map(|c| c.reflagged(|m| index.is_unread(m)))
        {
            Ok(candidates) => {
                match candidates.search(|m| index.find_method(m, method_name).map(|hit| (m, hit))) {
                    BodySearch::Found((target_module, m)) if m.is_export => {
                        crate::queries::QualifiedLookup::Resolved(MethodId {
                            module: target_module,
                            local_id: m.local_id,
                        })
                    }
                    BodySearch::Found(_) => crate::queries::QualifiedLookup::VisibilityBlocked,
                    BodySearch::Absent | BodySearch::Unread => {
                        crate::queries::QualifiedLookup::Absent
                    }
                }
            }
            Err(_) => crate::queries::QualifiedLookup::Absent,
        };
    let global_modules = resolver.global_common_module_names(db);
    edges.extend(crate::queries::resolve_callback_edges(
        summary,
        find_local,
        find_qualified,
        &global_modules,
    ));

    ResolvedModuleSummary { module, edges }
}

/// Projects a batch's resolved semantic calls to deterministic method-only pairs.
///
/// The index-backed resolver supplies direct, qualified-module, manager-user-method,
/// notify, and idle-handler targets. This projection intentionally retains only
/// method callers and method targets, so module code, unresolved calls, metadata,
/// query, form, subscription, role, subsystem, register, and `SetAction` facts do
/// not enter the call hierarchy.
///
/// Each module's summary resolution (which lowers that module's own bodies) runs
/// in parallel on `pool`; the fold below walks the results in `batch` order and
/// each module's pairs are already sorted, so the output is deterministic
/// regardless of completion order.
pub fn project_batch_method_call_pairs<DB: ConfigsDatabase + Clone + Send>(
    pool: &rayon::ThreadPool,
    db: &DB,
    index: &GraphIndex,
    batch: &[ModuleId],
) -> Vec<MethodCallPair> {
    let per_module = parallel_per_module(pool, db, batch, |db, module| {
        let summary = resolve_module_summary_via_index(db, module, index);
        resolved_summary_method_pairs(&summary)
    });

    let mut pairs = Vec::new();
    let mut seen = FxHashSet::default();
    for module_pairs in per_module {
        pairs.extend(module_pairs.into_iter().filter(|pair| seen.insert(*pair)));
    }

    pairs
}

/// One module's read-only extraction for the fused call-hierarchy build: its
/// index declarations plus the retained pair-relevant call-summary subset. Kept
/// opaque so the fold ([`GraphIndex::insert_extraction`]) stays the only way the
/// data enters an index.
pub struct ModuleIndexExtraction {
    pub module: ModuleId,
    entries: Vec<GraphMethodEntry>,
    module_dispatch: Option<MethodDispatch>,
    unread: bool,
    pair_intents: crate::call_graph::ModuleCallSummary,
}

/// The parallel half of the fused extraction (see
/// [`GraphIndex::add_batch_extracting_pair_intents`]): safe to run on a dedicated
/// pool/database while another batch's extraction is in flight elsewhere, because
/// it touches no shared index state.
pub fn extract_batch_index_and_pair_intents<DB: ConfigsDatabase + Clone + Send>(
    pool: &rayon::ThreadPool,
    db: &DB,
    batch: &[ModuleId],
) -> Vec<ModuleIndexExtraction> {
    parallel_per_module(pool, db, batch, |db, module| {
        let (entries, module_dispatch, unread) = GraphIndex::extract_module_data(db, module);
        let pair_intents = db.module_call_summary(module).method_pair_subset();
        ModuleIndexExtraction { module, entries, module_dispatch, unread, pair_intents }
    })
}

/// Resolve retained pair intents (from
/// [`GraphIndex::add_batch_extracting_pair_intents`]) for many modules in parallel
/// against the COMPLETED index, yielding each module's sorted, deduplicated method
/// pairs in `intents` order. Resolution reads only path/configuration state from
/// `db` (module index, config metadata) — never module texts — so the database
/// only needs every file registered, not loaded.
pub fn project_method_pairs_from_intents<DB: ConfigsDatabase + Clone + Send>(
    pool: &rayon::ThreadPool,
    db: &DB,
    index: &GraphIndex,
    intents: &[(ModuleId, crate::call_graph::ModuleCallSummary)],
) -> Vec<(ModuleId, Vec<MethodCallPair>)> {
    let warm: Vec<ModuleId> = intents.iter().map(|&(module, _)| module).collect();
    parallel_per_item(pool, db, intents, &warm, |db, (module, summary)| {
        let resolved = resolve_summary_via_index(db, *module, summary, index);
        (*module, resolved_summary_method_pairs(&resolved))
    })
}

fn resolved_summary_method_pairs(summary: &ResolvedModuleSummary) -> Vec<MethodCallPair> {
    let mut pairs = Vec::new();
    for edge in &summary.edges {
        if let Some(pair) = MethodCallPair::from_resolved_edge(summary.module, edge) {
            pairs.push(pair);
        }
    }
    pairs.sort_unstable_by_key(|pair| {
        (pair.caller.local_id, pair.target.module.file_id, pair.target.local_id)
    });
    pairs.dedup();
    pairs
}

#[cfg(test)]
mod method_only_call_pair_tests {
    use super::resolved_summary_method_pairs;
    use bsl_metadata::MdoType;
    use syntax::{TextRange, TextSize};
    use vfs::FileId;

    use crate::{
        call_graph::{
            CallTarget, CallerId, EdgeKind, EdgeProvenance, ResolvedCallEdge,
            ResolvedModuleSummary, ResolvedTarget,
        },
        call_hierarchy_index::MethodCallPair,
        name::Name,
        MethodId, ModuleId,
    };

    fn edge(caller: CallerId, target: ResolvedTarget) -> ResolvedCallEdge {
        ResolvedCallEdge {
            caller,
            target,
            kind: EdgeKind::DirectLocal,
            range: TextRange::empty(TextSize::from(0)),
            provenance: EdgeProvenance::Resolved,
        }
    }

    #[test]
    fn method_only_call_pairs_drop_non_method_edges_and_deduplicate() {
        // Given: resolved edges to methods, metadata, an unresolved target, and module code.
        let module = ModuleId::new(FileId(0));
        let local = MethodId { module, local_id: crate::MethodKey::first("М1") };
        let other =
            MethodId { module: ModuleId::new(FileId(1)), local_id: crate::MethodKey::first("М0") };
        let summary = ResolvedModuleSummary {
            module,
            edges: vec![
                edge(
                    CallerId::Method(crate::MethodKey::first("М0")),
                    ResolvedTarget::Method(other),
                ),
                edge(
                    CallerId::Method(crate::MethodKey::first("М0")),
                    ResolvedTarget::Method(local),
                ),
                edge(
                    CallerId::Method(crate::MethodKey::first("М0")),
                    ResolvedTarget::Method(other),
                ),
                edge(CallerId::ModuleCode, ResolvedTarget::Method(other)),
                edge(
                    CallerId::Method(crate::MethodKey::first("М0")),
                    ResolvedTarget::Mdo {
                        mdo_type: MdoType::Catalog,
                        object_name: Name::new("Контрагенты"),
                    },
                ),
                edge(
                    CallerId::Method(crate::MethodKey::first("М0")),
                    ResolvedTarget::Unresolved(CallTarget::Unresolved),
                ),
            ],
        };

        // When: the resolved summary is projected to hierarchy pairs.
        let pairs = resolved_summary_method_pairs(&summary);

        // Then: only unique method-to-method pairs remain in target order.
        assert_eq!(
            pairs,
            vec![
                MethodCallPair::new(
                    MethodId { module, local_id: crate::MethodKey::first("М0") },
                    local
                ),
                MethodCallPair::new(
                    MethodId { module, local_id: crate::MethodKey::first("М0") },
                    other
                )
            ],
        );
    }
}

/// Build the whole-config call graph over `modules` using the resident `index`
/// for resolution instead of the monolithic Salsa fold. Mirrors
/// `workspace_call_graph_query` pass-for-pass; the golden-equivalence test
/// guarantees an identical result. Each module's own bodies/SDBL are still
/// lowered (Pass 2/3), so a batched build that loads only a window of texts can
/// drive this over its slice.
pub fn workspace_call_graph_via_index(
    db: &dyn ConfigsDatabase,
    modules: &[ModuleId],
    index: &GraphIndex,
) -> WorkspaceCallGraph {
    let mut graph = WorkspaceCallGraph::default();
    let mut mdo_canonical = crate::queries::MdoCanonical::default();

    // Pass 1: dispatch table (cross-module endpoints need it for the boundary flag).
    index.seed_dispatch(&mut graph);

    // Pass 2: resolved call/manager edges via the index.
    for &module in modules {
        let summary = resolve_module_summary_via_index(db, module, index);
        let edges = {
            let dispatch = |node: &GraphNode| graph.dispatch(node);
            crate::queries::project_module_call_edges(&summary, &dispatch, &mut mdo_canonical)
        };
        for edge in edges {
            graph.insert(edge);
        }
    }

    // Pass 3: SDBL query_ref edges (config/metadata-resolved — no symbol trees;
    // identical to the Salsa path).
    let mut seen_query_ref: FxHashSet<(GraphNode, MdoType, String)> = FxHashSet::default();
    let mut seen_query_attr: FxHashSet<(GraphNode, MdoType, String, String)> = FxHashSet::default();
    for &module in modules {
        let edges = crate::queries::project_module_query_edges(
            db,
            module,
            &mut mdo_canonical,
            &mut seen_query_ref,
            &mut seen_query_attr,
        );
        for edge in edges {
            graph.insert(edge);
        }
    }

    graph
}

/// A qualified/manager call whose target MODULE resolved but whose METHOD did not
/// resolve to an exported method — the call sites a reverse index must remember so an
/// incremental rebuild can find the callers that would newly resolve if the target
/// gains (or exports) that method. Re-walks the Salsa-cached `module_call_summary`
/// (a memo hit in the build's batch db — no re-lowering) and re-runs only the cheap
/// locate+find resolution prefix, deliberately NOT touching the edge projection so
/// edge output stays byte-identical. The `Err` (module-not-found) cases are omitted:
/// a target module appearing requires a file/`.xml` add, which forces a full rebuild.
///
/// Returns `(target module, lowercased method name)`; the caller is `module`.
pub fn extract_unresolved_refs(
    db: &dyn ConfigsDatabase,
    module: ModuleId,
    index: &GraphIndex,
) -> Vec<(ModuleId, String)> {
    use crate::call_graph::CallTarget;

    let summary = db.module_call_summary(module);
    let resolver = Resolver::with_workspace_scope(module);
    let mut out = Vec::new();
    for edge in &summary.call_edges {
        match &edge.target {
            CallTarget::QualifiedModule { module_name, method_name } => {
                if let Ok(candidates) = resolver
                    .locate_common_module_candidates(db, module_name)
                    .map(|c| c.reflagged(|m| index.is_unread(m)))
                {
                    for target in
                        reference_targets(&candidates, |m| index.find_method(m, method_name))
                    {
                        out.push((target, method_name.as_str().fold_lower()));
                    }
                }
            }
            CallTarget::ManagerAccess {
                manager_type,
                object_name,
                method_name: Some(method_name),
            } => {
                if let Ok(candidates) = resolver
                    .locate_manager_module(db, *manager_type, object_name)
                    .map(|c| c.reflagged(|m| index.is_unread(m)))
                {
                    // An unread manager module is tracked unconditionally, for the same
                    // reason as an unread common-module body: its surface is unknown, so
                    // becoming readable can add the method, and without the reference the
                    // caller is never reprojected.
                    for target in
                        reference_targets(&candidates, |m| index.find_method(m, method_name))
                    {
                        out.push((target, method_name.as_str().fold_lower()));
                    }
                }
            }
            _ => {}
        }
    }
    // A `Новый ОписаниеОповещения("Метод", ОбщийМодуль)` whose handler is currently
    // missing or non-exported: record it so that exporting/adding the method later
    // triggers an incremental reproject of the callback edge. `ЭтотОбъект` and idle
    // handlers target the current module and are already covered by the module's own
    // `module_call_summary` invalidation.
    for reg in &summary.notify_regs {
        if let crate::call_graph::NotifyTarget::Module(module_name) = &reg.target {
            if let Ok(candidates) = resolver
                .locate_common_module_candidates(db, module_name)
                .map(|c| c.reflagged(|m| index.is_unread(m)))
            {
                for target in
                    reference_targets(&candidates, |m| index.find_method(m, &reg.callback_name))
                {
                    out.push((target, reg.callback_name.as_str().fold_lower()));
                }
            }
        }
    }
    out
}

/// The bodies of `candidates` that must hold a reverse reference to a call for `probe`.
///
/// A call nobody answers tracks EVERY body, so that declaring the method in any of them
/// — or making an unread one readable — reprojects the caller.
///
/// An ANSWERED call tracks nothing. Priority order already settled it: every body ahead
/// of the answer was readable and did not have the method, and a body behind it loses to
/// the answer whatever it later turns out to declare. Tracking those trailing bodies
/// would fan a healed sibling out over every caller that was never going to change.
fn reference_targets(
    candidates: &crate::resolver::CommonModuleCandidates,
    mut probe: impl FnMut(ModuleId) -> Option<MethodRef>,
) -> Vec<ModuleId> {
    // The walk stops at the first body that DECLARES the name, exported or not — that
    // is where resolution stops too, and a non-exported declaration is an answer of its
    // own (`VisibilityBlocked`), not a reason to keep looking. Filtering by export
    // inside the walk would step over it, find a lower body's export, and call the
    // call answered — leaving no reverse reference for the day the winning declaration
    // gains `Экспорт`.
    let answered = matches!(candidates.search(&mut probe), BodySearch::Found(hit) if hit.is_export);
    if answered {
        Vec::new()
    } else {
        candidates.all_for_reference().collect()
    }
}

/// A batch's call/manager edge projection plus the module-located-but-unresolved call
/// refs gathered in the same pass (caller, target module, lowercased method).
pub struct BatchCallProjection {
    pub edges: Vec<WorkspaceCallEdge>,
    pub unresolved: Vec<(ModuleId, ModuleId, String)>,
    /// Calls to a metadata-declared common module whose body file is not present in
    /// the current graph. The module name is retained so adding its BSL body can
    /// find and reproject callers without rescanning source text.
    pub unresolved_declared_common: Vec<(ModuleId, String, String)>,
}

/// Workspace-wide state threaded across batches: MDO spelling canonicalization and
/// query-ref dedup. Reuse ONE instance for the whole build — recreating it per
/// batch would split an object's `Mdo` node across spellings and re-emit duplicate
/// query_ref edges.
#[derive(Default)]
pub struct GraphBuildState {
    mdo_canonical: crate::queries::MdoCanonical,
    seen_query_ref: FxHashSet<(GraphNode, MdoType, String)>,
    seen_query_attr: FxHashSet<(GraphNode, MdoType, String, String)>,
    /// Form data-binding intents gathered by the form pass, resolved against
    /// `catalog_index` and emitted as `DataBinding` edges in the final binding pass.
    form_bindings: Vec<FormBinding>,
    /// The metadata catalog indexed for binding resolution: per object, its canonical
    /// spelling and its declared (non-standard) fields / tabular-section columns with
    /// their metadata casing. Populated by the catalog pass as it emits nodes, so a
    /// binding's resolved to-id byte-matches the node the catalog emitted.
    catalog_index: CatalogIndex,
}

/// A form data-binding intent: a form node bound to a metadata object's structure.
/// `field_path` is empty for an object-level binding (`form_attribute → mdo`), `[field]`
/// for an object attribute, `[section, column]` for a tabular-section column.
struct FormBinding {
    from: GraphNode,
    target_mdo: MdoType,
    target_obj: String,
    field_path: Vec<String>,
}

type CatalogIndex = FxHashMap<(MdoType, String), CatalogEntry>;

/// One object's structure as the catalog emitted it: the canonical object spelling and
/// its fields / tabular sections keyed by lowercased name, each mapped to the
/// metadata-cased name used in the emitted node id.
struct CatalogEntry {
    object_name: crate::name::Name,
    attrs: FxHashMap<String, crate::name::Name>,
    sections: FxHashMap<String, CatalogSection>,
}

struct CatalogSection {
    section_name: crate::name::Name,
    cols: FxHashMap<String, crate::name::Name>,
}

impl GraphBuildState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Objects seen with more than one casing during the build, as
    /// `(EnglishType, lowercased object)`. An incremental rebuild refuses the
    /// body-only fast path for these — their cross-module first-seen ordering is not
    /// reconstructable from the canonicalised store alone.
    pub fn casing_variant_keys(&self) -> Vec<(&'static str, String)> {
        self.mdo_canonical
            .casing_variants()
            .map(|(ty, obj)| (ty.english_name(), obj.clone()))
            .collect()
    }
}

/// Project the call/manager and SDBL query_ref edges for one `batch` of modules,
/// resolving cross-module targets and the client→server boundary flag through the
/// resident `index`. The batch database therefore needs only its own modules'
/// texts (plus the configuration metadata) — the foundation of the RAM-bounded
/// streaming build.
///
/// `state` carries the workspace-wide canonicalization/dedup across batches and
/// MUST be reused.
///
/// This projects a batch's call/manager edges **and** its query edges together. To
/// reproduce the fold's `Mdo`/`Attribute` node spelling byte-for-byte, a streaming
/// build must instead run [`project_batch_call_edges`] across ALL batches before
/// [`project_batch_query_edges`] across all batches (the fold's Pass-2-then-Pass-3
/// order): an object referenced only in code vs. only in a query would otherwise
/// get a different first-seen spelling. This combined helper is for callers that
/// process one batch in isolation and accept that per-batch ordering.
pub fn project_batch_edges<DB: ConfigsDatabase + Clone + Send>(
    pool: &rayon::ThreadPool,
    db: &DB,
    batch: &[ModuleId],
    index: &GraphIndex,
    state: &mut GraphBuildState,
) -> Vec<WorkspaceCallEdge> {
    let mut edges = project_batch_call_edges(pool, db, batch, index, state).edges;
    edges.extend(project_batch_query_edges(pool, db, batch, state));
    edges
}

/// A batch's resolved call/manager edges, resolving cross-module targets through
/// the resident `index` (so the batch database needs only its own texts). Run this
/// across every batch before [`project_batch_query_edges`] to match the fold's
/// global Pass-2-then-Pass-3 canonicalization order. `state.mdo_canonical` is
/// shared and updated as new metadata-object spellings are first seen.
pub fn project_batch_call_edges<DB: ConfigsDatabase + Clone + Send>(
    pool: &rayon::ThreadPool,
    db: &DB,
    batch: &[ModuleId],
    index: &GraphIndex,
    state: &mut GraphBuildState,
) -> BatchCallProjection {
    // Resolve every module's summary in parallel, then project edges sequentially in
    // `batch` order so the shared `mdo_canonical` sees objects first-seen in the
    // exact order the fold does — parallelising only the read-only resolution keeps
    // the canonicalization deterministic. The unresolved-call refs are gathered in the
    // same parallel pass (a `module_call_summary` memo hit), so the index upkeep adds
    // no extra body lowering; edge projection is unchanged → edge output is identical.
    let results: Vec<_> = parallel_per_module(pool, db, batch, |db, module| {
        let summary = resolve_module_summary_via_index(db, module, index);
        let unresolved = extract_unresolved_refs(db, module, index);
        let unresolved_declared_common = extract_missing_common_module_refs(db, module);
        (summary, unresolved, unresolved_declared_common)
    });

    let mut edges = Vec::new();
    let mut unresolved = Vec::new();
    let mut unresolved_declared_common = Vec::new();
    let dispatch = |node: &GraphNode| index.dispatch(node);
    for (summary, unres, declared) in &results {
        edges.extend(crate::queries::project_module_call_edges(
            summary,
            &dispatch,
            &mut state.mdo_canonical,
        ));
        for (target, method_lower) in unres {
            unresolved.push((summary.module, *target, method_lower.clone()));
        }
        unresolved_declared_common.extend(declared.iter().cloned());
    }
    BatchCallProjection { edges, unresolved, unresolved_declared_common }
}

/// Keep a reverse reference for a qualified call only when its common-module
/// receiver is declared by metadata but currently has no usable source body.
/// `CallTarget::QualifiedModule` is produced by semantic call lowering, so this
/// does not infer receivers from arbitrary field chains or source text.
fn extract_missing_common_module_refs(
    db: &dyn ConfigsDatabase,
    module: ModuleId,
) -> Vec<(ModuleId, String, String)> {
    use crate::call_graph::CallTarget;

    let Some(module_name_owner) = db.has_config_root(module.file_id).then_some(module.file_id)
    else {
        return Vec::new();
    };
    let resolver = Resolver::with_workspace_scope(module);
    let mut refs = Vec::new();
    for edge in &db.module_call_summary(module).call_edges {
        let CallTarget::QualifiedModule { module_name, method_name } = &edge.target else {
            continue;
        };
        if resolver.locate_common_module_candidates(db, module_name).is_ok()
            || db.resolve_common_module(module_name_owner, module_name.as_str()).is_none()
        {
            continue;
        }
        // Match the durable `common/<name>` scope used by `encode_scope`; folding
        // here allows source spelling to differ from metadata's casing.
        refs.push((
            module,
            format!("common/{}", module_name.as_str().fold_lower()),
            method_name.as_str().fold_lower(),
        ));
    }
    refs.sort_by(|a, b| (&a.1, &a.2).cmp(&(&b.1, &b.2)));
    refs.dedup();
    refs
}

/// Run `f` for every module in `batch` in parallel on `pool`, returning the results
/// in `batch` order. The database is `Send` but not `Sync` (a per-handle query
/// stack), so each rayon job works on its own cheap `db` clone — the clones share
/// the underlying memo storage. The work runs on the caller-supplied `pool`, never
/// the global one, so concurrent builds (each with its own pool and database) never
/// share a worker thread — Salsa attaches at most one database to any thread, and a
/// salsa query that itself parallelises (e.g. metadata loading) stays on this pool.
fn parallel_per_module<DB, R, F>(
    pool: &rayon::ThreadPool,
    db: &DB,
    batch: &[ModuleId],
    f: F,
) -> Vec<R>
where
    DB: ConfigsDatabase + Clone + Send,
    R: Send,
    F: Fn(&DB, ModuleId) -> R + Sync + Send,
{
    parallel_per_item(pool, db, batch, batch, |db, &module| f(db, module))
}

/// The generic core of [`parallel_per_module`]: run `f` over arbitrary
/// module-keyed items. `warm` names the modules the items will resolve against,
/// so the pre-pool warm-up can seed the configuration loader for every root they
/// reach (see the comment below).
fn parallel_per_item<DB, T, R, F>(
    pool: &rayon::ThreadPool,
    db: &DB,
    items: &[T],
    warm: &[ModuleId],
    f: F,
) -> Vec<R>
where
    DB: ConfigsDatabase + Clone + Send,
    T: Sync,
    R: Send,
    F: Fn(&DB, &T) -> R + Sync + Send,
{
    use rayon::prelude::*;

    // Warm the configuration loader ONCE on THIS thread before the parallel region.
    // `bsl_metadata::load_from_directory` (reached through the lru-cached
    // `load_configuration` query) fans out over its own `rayon::scope`; if it ran
    // inside a parallel job, a free worker could steal a sibling job — carrying a
    // different `db` clone — into that scope and attach a second database to a thread
    // mid-query, which Salsa forbids. The clones share the `Zalsa`, so the per-module
    // jobs below find the loader cached and never open a nested scope; their own
    // per-file metadata/visibility work runs in the pool. It is the only internally
    // parallel query the build reaches (no type inference), so this warm-up closes
    // the window.
    //
    // Two disjoint root sets have to be covered. `configurations_inventory` loads the
    // DECLARED roots, which is what the resolvers' visibility path reads. That leaves
    // the roots the items' own files attribute to on disk — the same set only when
    // discovery declared every configuration present, and short by exactly the nested
    // ones when it did not.
    if !warm.is_empty() {
        let _ = db.configurations_inventory();
        db.warm_config_roots(warm);
    }

    let seed = db.clone();
    pool.install(move || {
        items
            .par_iter()
            .map_with(seed, |db, item| {
                // Mark the job so internally-parallel entry points (the metadata
                // loader) can detect that the warm-up above failed to keep them off
                // this pool, instead of deadlocking silently.
                let _guard = stdx::par_guard::enter_no_nested_parallelism();
                f(&*db, item)
            })
            .collect()
    })
}

/// A batch's SDBL `query_ref` edges. Run across every batch only after
/// [`project_batch_call_edges`] has run across all of them, sharing the same
/// `state`, so query-only metadata objects inherit the spelling the fold's Pass 3
/// would give them (call sites win first, exactly as in the fold).
pub fn project_batch_query_edges<DB: ConfigsDatabase + Clone + Send>(
    pool: &rayon::ThreadPool,
    db: &DB,
    batch: &[ModuleId],
    state: &mut GraphBuildState,
) -> Vec<WorkspaceCallEdge> {
    // Collect each module's query reads from its SDBL HIR in parallel (read-only),
    // then project edges sequentially in `batch` order so the shared
    // canonicalization/dedup matches the fold byte-for-byte.
    let collected: Vec<_> = parallel_per_module(pool, db, batch, |db, module| {
        crate::queries::collect_module_query_refs(db, module)
    });

    let mut edges = Vec::new();
    for refs in &collected {
        edges.extend(crate::queries::project_collected_query_edges(
            refs,
            &mut state.mdo_canonical,
            &mut state.seen_query_ref,
            &mut state.seen_query_attr,
        ));
    }
    edges
}

/// One form module's structural facts, gathered read-only for the form pass: its
/// path-derived key (owner + name) and the declared element names (deduped
/// case-insensitively, declaration order preserved).
struct CollectedForm {
    key: crate::module_index::FormKey,
    /// One per UI element, in declaration order: its name, its own element id, and
    /// its parent's element id (`None` for a root element).
    items: Vec<CollectedItem>,
    /// Declared form-attribute names (the form's data model), deduped
    /// case-insensitively in declaration order.
    attributes: Vec<crate::name::Name>,
    /// Data-binding intents: a Ref-typed form attribute (object-level) or a data-bound
    /// UI element (field-level). Resolved against the catalog in the binding pass.
    bindings: Vec<CollectedBinding>,
}

struct CollectedItem {
    name: crate::name::Name,
    id: u32,
    parent_id: Option<u32>,
}

/// The form-side endpoint of a data binding, before the canonical owner/form is known.
enum BindingFrom {
    /// A Ref-typed form attribute (object-level binding to its backing object).
    Attr(crate::name::Name),
    /// A data-bound UI element (field-level binding via its data path).
    Item(crate::name::Name),
}

struct CollectedBinding {
    from: BindingFrom,
    mdo: MdoType,
    obj: String,
    /// Empty for an object-level binding; the data-path segments after the form
    /// attribute for a field-level one.
    field_path: Vec<String>,
}

/// A batch's `contains` edges derived from form metadata: `mdo → form` (object forms
/// only) and `form → form_item` for each declared element. Run across every batch
/// only AFTER the call/query passes, sharing the same `state`, so a form's owner
/// object inherits the canonical spelling the call/query passes assigned (code sites
/// win first; a divergent form-directory casing is recorded as a casing variant just
/// like any other).
///
/// Emitted as edges only — the `Form`/`FormItem`/`Mdo` nodes fall out as edge
/// endpoints in the build driver, exactly as `Mdo`/`Attribute` nodes do. A common
/// form (no owner) with no elements therefore produces no edges and is not
/// represented; such a form carries no structural information.
///
/// This pass is run ONLY by the full build. The incremental reprojection leaves form
/// nodes/edges untouched (form structure comes from form XML, so any form-structure
/// change is a metadata drift that already forces a full rebuild).
pub fn project_batch_form_edges<DB: ConfigsDatabase + Clone + Send>(
    pool: &rayon::ThreadPool,
    db: &DB,
    batch: &[ModuleId],
    paths: &FxHashMap<FileId, String>,
    state: &mut GraphBuildState,
) -> Vec<WorkspaceCallEdge> {
    let collected: Vec<Option<CollectedForm>> =
        parallel_per_module(pool, db, batch, |db, module| {
            let path = paths.get(&module.file_id)?;
            let key = crate::module_index::parse_form_module_path(path)?;
            let metadata = db.module_metadata(module);
            let form = metadata.form.as_ref()?;
            // Keep every element (no dedup here) so the parent-id map below can
            // resolve any `parent_id`, even one pointing at a same-named sibling.
            let items = form
                .elements
                .iter()
                .map(|element| CollectedItem {
                    name: crate::name::Name::new(&element.name),
                    id: element.id,
                    parent_id: element.parent_id,
                })
                .collect();
            let mut attributes = Vec::new();
            let mut seen_attr = FxHashSet::default();
            let mut bindings = Vec::new();
            // `Ref`-typed form attributes back an object; index the survivor of each
            // name so a data path's first segment can resolve to its object, and emit
            // an object-level binding for it.
            let mut attr_backing: FxHashMap<String, (MdoType, String)> = FxHashMap::default();
            for attr in &form.attributes {
                let name = crate::name::Name::new(&attr.name);
                let lower = name.as_str().fold_lower();
                if seen_attr.insert(lower.clone()) {
                    attributes.push(name.clone());
                    if let bsl_metadata::AttributeType::Ref { mdo_type, name: obj } =
                        &attr.attr_type
                    {
                        attr_backing.insert(lower, (*mdo_type, obj.clone()));
                        bindings.push(CollectedBinding {
                            from: BindingFrom::Attr(name),
                            mdo: *mdo_type,
                            obj: obj.clone(),
                            field_path: Vec::new(),
                        });
                    }
                }
            }
            // A UI element whose data path is `<реквизит>.<поле>[.<колонка>]` binds to
            // that field of the form attribute's backing object. A leading `~` marks a
            // broken path; bare `<реквизит>` has no field.
            for element in &form.elements {
                let Some(dp) = element.data_path.as_deref() else { continue };
                if dp.starts_with('~') {
                    continue;
                }
                let mut segs = dp.split('.');
                let Some(seg0) = segs.next() else { continue };
                let Some((mdo, obj)) = attr_backing.get(&seg0.fold_lower()) else { continue };
                let field_path: Vec<String> = segs.map(str::to_string).collect();
                if field_path.is_empty() {
                    continue;
                }
                bindings.push(CollectedBinding {
                    from: BindingFrom::Item(crate::name::Name::new(&element.name)),
                    mdo: *mdo,
                    obj: obj.clone(),
                    field_path,
                });
            }
            Some(CollectedForm { key, items, attributes, bindings })
        });

    let mut edges = Vec::new();
    for form in collected.iter().flatten() {
        let form_name = crate::name::Name::new(&form.key.form_name);
        // Canonicalise the owner object so the form's `mdo` parent unifies with the
        // call/query-derived `Mdo` node for the same object.
        let owner = form.key.owner.as_ref().map(|(mdo_type, object)| {
            (*mdo_type, state.mdo_canonical.canonical(*mdo_type, object))
        });
        let form_node = GraphNode::Form { owner: owner.clone(), form_name: form_name.clone() };
        if let Some((mdo_type, object_name)) = &owner {
            edges.push(contains_edge(
                GraphNode::Mdo { mdo_type: *mdo_type, object_name: object_name.clone() },
                form_node.clone(),
            ));
        }
        let form_item = |item_name: crate::name::Name| GraphNode::FormItem {
            owner: owner.clone(),
            form_name: form_name.clone(),
            item_name,
        };
        // `form_item` nodes are keyed by name, so the surviving node for a name is
        // the first element declaring it. Map id → name over every element, and
        // record which id is that survivor per lowercased name. A `parent_id` is
        // only honoured when it points at the surviving element for its name — if the
        // real parent was a same-named element that collapsed into another node, its
        // name now denotes a different element, so the child hangs off the form root.
        let id_to_name: FxHashMap<u32, &crate::name::Name> =
            form.items.iter().map(|item| (item.id, &item.name)).collect();
        let mut survivor_id: FxHashMap<String, u32> = FxHashMap::default();
        // The surviving `form_item` node for a name is the first element declaring it;
        // its spelling is the one the node carries. A field-level binding must point at
        // that survivor spelling, not a later same-name element's own casing.
        let mut survivor_name: FxHashMap<String, &crate::name::Name> = FxHashMap::default();
        for item in &form.items {
            let lower = item.name.as_str().fold_lower();
            survivor_id.entry(lower.clone()).or_insert(item.id);
            survivor_name.entry(lower).or_insert(&item.name);
        }
        let mut seen_item = FxHashSet::default();
        for item in &form.items {
            if !seen_item.insert(item.name.as_str().fold_lower()) {
                continue;
            }
            let parent = item.parent_id.and_then(|pid| Some((pid, *id_to_name.get(&pid)?))).filter(
                |(pid, parent_name)| {
                    !parent_name.as_str().eq_ignore_ascii_case(item.name.as_str())
                        && survivor_id.get(&parent_name.as_str().fold_lower()) == Some(pid)
                },
            );
            let parent_node = match parent {
                Some((_, parent_name)) => form_item(parent_name.clone()),
                None => form_node.clone(),
            };
            edges.push(contains_edge(parent_node, form_item(item.name.clone())));
        }
        for attr in &form.attributes {
            edges.push(contains_edge(
                form_node.clone(),
                GraphNode::FormAttribute {
                    owner: owner.clone(),
                    form_name: form_name.clone(),
                    attr_name: attr.clone(),
                },
            ));
        }
        // Stash data-binding intents with the from-node fully built; the binding pass
        // resolves their targets once the catalog index is complete.
        for binding in &form.bindings {
            let from = match &binding.from {
                BindingFrom::Attr(name) => GraphNode::FormAttribute {
                    owner: owner.clone(),
                    form_name: form_name.clone(),
                    attr_name: name.clone(),
                },
                BindingFrom::Item(name) => {
                    let survivor =
                        survivor_name.get(&name.as_str().fold_lower()).copied().unwrap_or(name);
                    form_item(survivor.clone())
                }
            };
            state.form_bindings.push(FormBinding {
                from,
                target_mdo: binding.mdo,
                target_obj: binding.obj.clone(),
                field_path: binding.field_path.clone(),
            });
        }
    }
    edges
}

/// One metadata object's declared structure, gathered for the catalog pass.
struct CatalogObject {
    mdo_type: MdoType,
    name: String,
    /// Top-level attribute names (object attributes, or register
    /// dimensions/resources/attributes), declaration order.
    attrs: Vec<String>,
    /// Tabular sections, each with its column names. Empty for registers.
    sections: Vec<(String, Vec<String>)>,
}

/// The whole metadata catalog as `contains` edges: `mdo → attribute` (object
/// attributes + register dimensions/resources/attributes), `mdo → tabular_section`,
/// and `tabular_section → attribute` (the section column). Driven by the metadata
/// catalog — **every** object in every visible configuration, whether or not code
/// references it — so the structural node set is stable under body edits and an
/// incremental update (which never runs this pass) stays byte-identical to a full
/// rebuild. Any metadata/`.xml` change already forces a full rebuild.
///
/// Run ONCE on the driver thread after the call/query/form passes, sharing `state`
/// so an object's `mdo` node inherits the canonical spelling code sites assigned (a
/// metadata-only object is first-seen here). Objects are visited in a deterministic
/// `(english type, lowercased name)` order so first-seen canonicalisation and the
/// emitted edge set never depend on configuration load order. The union across base +
/// extension configurations is by node identity: a duplicated object/attribute/column
/// dedups, so an extension that adds attributes to a base object contributes only its
/// new ones.
pub fn project_workspace_catalog_edges<DB: ConfigsDatabase>(
    db: &DB,
    _representative: FileId,
    state: &mut GraphBuildState,
) -> Vec<WorkspaceCallEdge> {
    // Platform standard attributes (Ссылка/Код/Наименование/…) are synthesised onto
    // every object and carry no configuration-specific structure; exclude them so the
    // catalog covers exactly the user-declared attributes (the same standard-field
    // exclusion the query-ref pass applies). `is_standard_attribute_name` is derived
    // from `StandardAttributeKind`, the enum the synthesiser builds them from.
    let is_standard = bsl_metadata::is_standard_attribute_name;

    let mut objects: Vec<CatalogObject> = Vec::new();
    for visible in db.configurations_inventory() {
        let config = &visible.configuration;
        for mdo in config.metadata_objects() {
            objects.push(CatalogObject {
                mdo_type: mdo.mdo_type,
                name: mdo.name.clone(),
                attrs: mdo
                    .attributes
                    .iter()
                    .map(|a| a.name.clone())
                    .filter(|n| !is_standard(n))
                    .collect(),
                sections: mdo
                    .tabular_sections
                    .iter()
                    .map(|ts| {
                        (
                            ts.name().to_string(),
                            ts.attributes().iter().map(|c| c.name().to_string()).collect(),
                        )
                    })
                    .collect(),
            });
        }
        for reg in config.registers() {
            let mut attrs = Vec::new();
            // Dimensions and resources are always user-declared; only the register's
            // `attributes` bucket can hold synthesised standard fields (Период, …).
            attrs.extend(reg.dimensions().iter().map(|d| d.name().to_string()));
            attrs.extend(reg.resources().iter().map(|r| r.name().to_string()));
            attrs.extend(
                reg.attributes().iter().map(|a| a.name().to_string()).filter(|n| !is_standard(n)),
            );
            objects.push(CatalogObject {
                mdo_type: reg.mdo_type(),
                name: reg.name().to_string(),
                attrs,
                sections: Vec::new(),
            });
        }
    }
    // Deterministic visitation regardless of configuration load order. The original
    // spelling is the final tiebreaker so that two objects sharing a lowercased name
    // across configs (e.g. base + extension with different casing) always yield the
    // same first-seen canonical spelling, independent of base-vs-extension load order.
    objects.sort_by(|a, b| {
        a.mdo_type
            .english_name()
            .cmp(b.mdo_type.english_name())
            .then_with(|| a.name.fold_lower().cmp(&b.name.fold_lower()))
            .then_with(|| a.name.cmp(&b.name))
    });

    let mut edges = Vec::new();
    let mut seen_attr: FxHashSet<(MdoType, String, String)> = FxHashSet::default();
    let mut seen_ts: FxHashSet<(MdoType, String, String)> = FxHashSet::default();
    let mut seen_ts_attr: FxHashSet<(MdoType, String, String, String)> = FxHashSet::default();
    for obj in &objects {
        let object_name = state.mdo_canonical.canonical(obj.mdo_type, &obj.name);
        let key = object_name.as_str().fold_lower();
        let mdo = GraphNode::Mdo { mdo_type: obj.mdo_type, object_name: object_name.clone() };
        // Index this object for form data-binding resolution. First-wins on each
        // lowercased name mirrors the dedup below, so the indexed (metadata-cased) name
        // is the one the emitted node carries.
        let entry = state.catalog_index.entry((obj.mdo_type, key.clone())).or_insert_with(|| {
            CatalogEntry {
                object_name: object_name.clone(),
                attrs: FxHashMap::default(),
                sections: FxHashMap::default(),
            }
        });
        for attr in &obj.attrs {
            entry.attrs.entry(attr.fold_lower()).or_insert_with(|| crate::name::Name::new(attr));
        }
        for (section, cols) in &obj.sections {
            let sec =
                entry.sections.entry(section.fold_lower()).or_insert_with(|| CatalogSection {
                    section_name: crate::name::Name::new(section),
                    cols: FxHashMap::default(),
                });
            for col in cols {
                sec.cols.entry(col.fold_lower()).or_insert_with(|| crate::name::Name::new(col));
            }
        }
        for attr in &obj.attrs {
            if seen_attr.insert((obj.mdo_type, key.clone(), attr.fold_lower())) {
                edges.push(contains_edge(
                    mdo.clone(),
                    GraphNode::Attribute {
                        mdo_type: obj.mdo_type,
                        object_name: object_name.clone(),
                        attr_name: crate::name::Name::new(attr),
                    },
                ));
            }
        }
        for (section, cols) in &obj.sections {
            let section_lower = section.fold_lower();
            let ts = GraphNode::TabularSection {
                mdo_type: obj.mdo_type,
                object_name: object_name.clone(),
                section_name: crate::name::Name::new(section),
            };
            if seen_ts.insert((obj.mdo_type, key.clone(), section_lower.clone())) {
                edges.push(contains_edge(mdo.clone(), ts.clone()));
            }
            for col in cols {
                if seen_ts_attr.insert((
                    obj.mdo_type,
                    key.clone(),
                    section_lower.clone(),
                    col.fold_lower(),
                )) {
                    edges.push(contains_edge(
                        ts.clone(),
                        GraphNode::TabularSectionAttribute {
                            mdo_type: obj.mdo_type,
                            object_name: object_name.clone(),
                            section_name: crate::name::Name::new(section),
                            attr_name: crate::name::Name::new(col),
                        },
                    ));
                }
            }
        }
    }
    edges
}

/// Project event-subscription handler edges: each `ПодпискаНаСобытие` links its
/// subscription node (`Mdo{EventSubscription, name}`) to the exported common-module
/// method named in its handler. Config-level and full-build only, like the catalog
/// pass — the only edits that can invalidate such an edge (the handler method being
/// added, removed, renamed, or its `Экспорт` toggled) all change the handler module's
/// [`GraphIndex::module_sig_hash`], which fails the body-only precondition and forces a
/// full rebuild rather than a body-only reproject. A handler that does not resolve to an
/// exported method yields no edge (and hence no subscription node), mirroring every
/// other unresolved target.
pub fn project_workspace_subscription_edges<DB: ConfigsDatabase>(
    db: &DB,
    representative: FileId,
    index: &GraphIndex,
    state: &mut GraphBuildState,
) -> Vec<WorkspaceCallEdge> {
    // Resolve the handler's common module by name through the source-root module index,
    // not the visibility-gated resolver: a subscription's handler is named by
    // configuration metadata and is referenced regardless of code-visibility scoping,
    // matching the `MissingEventSubscriptionHandler` diagnostic's "anywhere" lookup.
    let source_root_id = db.file_source_root_input(representative).source_root_id(db);
    let module_index = db.module_index(source_root_id);

    // Collect (subscription, handler module, handler method) deterministically so the
    // shared canonicalization sees a load-order-independent first-seen spelling.
    let mut subs: Vec<(String, crate::name::Name, crate::name::Name)> = Vec::new();
    for sub in db.enumerate_event_subscriptions(representative) {
        let Some(handler) = sub.parse_handler() else { continue };
        if handler.method_name.is_empty() {
            continue;
        }
        subs.push((
            sub.name().to_string(),
            crate::name::Name::new(&handler.module_name),
            crate::name::Name::new(&handler.method_name),
        ));
    }
    subs.sort();
    subs.dedup();

    let mut edges = Vec::new();
    let mut seen: FxHashSet<(GraphNode, GraphNode)> = FxHashSet::default();
    for (sub_name, module_name, method_name) in &subs {
        let Some(handler_file) = module_index.resolve_common_module(module_name) else { continue };
        let module_id = ModuleId::new(handler_file);
        let Some(m) = index.find_method(module_id, method_name) else { continue };
        if !m.is_export {
            continue;
        }
        let object_name = state.mdo_canonical.canonical(MdoType::EventSubscription, sub_name);
        let from = GraphNode::Mdo { mdo_type: MdoType::EventSubscription, object_name };
        let to = GraphNode::Method(MethodId { module: module_id, local_id: m.local_id });
        if seen.insert((from.clone(), to.clone())) {
            edges.push(WorkspaceCallEdge {
                from,
                to,
                kind: EdgeKind::EventSubscriptionRef,
                provenance: EdgeProvenance::StringResolved,
                call_site: CallSite::Structural,
                crosses_client_to_server: false,
            });
        }
    }
    edges
}

/// Project subsystem membership into edges: from each subsystem's `Mdo` node to every
/// member metadata object it contains and to every child subsystem. Pure config-driven
/// (like the catalog and subscription passes), no body lowering. Member/child node names
/// are canonicalized through the shared `mdo_canonical` so they coincide with the object's
/// own nodes from other passes.
pub fn project_workspace_subsystem_edges<DB: ConfigsDatabase>(
    db: &DB,
    representative: FileId,
    state: &mut GraphBuildState,
) -> Vec<WorkspaceCallEdge> {
    // Collect deterministically so canonicalization sees a load-order-independent
    // first-seen spelling.
    let mut members: Vec<(String, MdoType, String)> = Vec::new();
    let mut children: Vec<(String, String)> = Vec::new();
    for subsystem in db.enumerate_subsystems(representative) {
        for (mdo_type, member_name) in subsystem.content() {
            members.push((subsystem.name().to_string(), *mdo_type, member_name.clone()));
        }
        for child in subsystem.child_subsystems() {
            children.push((subsystem.name().to_string(), child.clone()));
        }
    }
    members.sort();
    members.dedup();
    children.sort();
    children.dedup();

    let mut edges = Vec::new();
    let mut seen: FxHashSet<(GraphNode, GraphNode)> = FxHashSet::default();

    for (sub_name, mdo_type, member_name) in &members {
        let from = GraphNode::Mdo {
            mdo_type: MdoType::Subsystem,
            object_name: state.mdo_canonical.canonical(MdoType::Subsystem, sub_name),
        };
        let to = GraphNode::Mdo {
            mdo_type: *mdo_type,
            object_name: state.mdo_canonical.canonical(*mdo_type, member_name),
        };
        if seen.insert((from.clone(), to.clone())) {
            edges.push(subsystem_membership_edge(from, to));
        }
    }
    for (sub_name, child_name) in &children {
        let from = GraphNode::Mdo {
            mdo_type: MdoType::Subsystem,
            object_name: state.mdo_canonical.canonical(MdoType::Subsystem, sub_name),
        };
        let to = GraphNode::Mdo {
            mdo_type: MdoType::Subsystem,
            object_name: state.mdo_canonical.canonical(MdoType::Subsystem, child_name),
        };
        if seen.insert((from.clone(), to.clone())) {
            edges.push(subsystem_membership_edge(from, to));
        }
    }
    edges
}

fn subsystem_membership_edge(from: GraphNode, to: GraphNode) -> WorkspaceCallEdge {
    WorkspaceCallEdge {
        from,
        to,
        kind: EdgeKind::SubsystemMembership,
        provenance: EdgeProvenance::Resolved,
        call_site: CallSite::Structural,
        crosses_client_to_server: false,
    }
}

/// Project register-records edges: from each document's `Mdo` node (type [`MdoType::Document`])
/// to every register it declares it posts in its `RegisterRecords` metadata. Pure config-driven
/// (like the subsystem and role passes), full-build only — a document metadata change is drift
/// that forces a full rebuild, so the incremental reproject never runs this and both paths stay
/// identical. Document and register names are canonicalized through the shared `mdo_canonical` so
/// they coincide with the objects' own nodes from other passes.
///
/// This is the declared "which documents post register X" relation, sound regardless of how the
/// posting code addresses the register (a literal `Движения.X`, a dynamic `Движения[…]` index, or
/// a string name into `РегистрыНакопления[…]` — only the first of which `register_movement` sees).
pub fn project_workspace_register_records_edges<DB: ConfigsDatabase>(
    db: &DB,
    _representative: FileId,
    state: &mut GraphBuildState,
) -> Vec<WorkspaceCallEdge> {
    // (document_name, register_type, register_name); gathered deterministically so
    // canonicalization sees a load-order-independent first-seen spelling.
    let mut records: Vec<(String, MdoType, String)> = Vec::new();
    for visible in db.configurations_inventory() {
        for object in visible.configuration.metadata_objects() {
            if object.mdo_type != MdoType::Document {
                continue;
            }
            for (register_type, register_name) in object.register_records() {
                records.push((object.name.clone(), *register_type, register_name.clone()));
            }
        }
    }
    records.sort();
    records.dedup();

    let mut edges = Vec::new();
    let mut seen: FxHashSet<(GraphNode, GraphNode)> = FxHashSet::default();
    for (doc_name, register_type, register_name) in &records {
        let from = GraphNode::Mdo {
            mdo_type: MdoType::Document,
            object_name: state.mdo_canonical.canonical(MdoType::Document, doc_name),
        };
        let to = GraphNode::Mdo {
            mdo_type: *register_type,
            object_name: state.mdo_canonical.canonical(*register_type, register_name),
        };
        if seen.insert((from.clone(), to.clone())) {
            edges.push(register_records_edge(from, to));
        }
    }
    edges
}

fn register_records_edge(from: GraphNode, to: GraphNode) -> WorkspaceCallEdge {
    WorkspaceCallEdge {
        from,
        to,
        kind: EdgeKind::RegisterRecords,
        provenance: EdgeProvenance::Resolved,
        call_site: CallSite::Structural,
        crosses_client_to_server: false,
    }
}

/// Project role reference edges: from each role's `Mdo` node (type [`MdoType::Role`]) to every
/// metadata object the role grants rights on. Pure config-driven (like the catalog and
/// subsystem passes), full-build only — a `Rights.xml` change is metadata drift that forces a
/// full rebuild, so the incremental reproject never runs this and both paths stay identical.
///
/// Two reference sources:
/// - **direct object-rights** (`<object>` entries) → provenance `resolved`;
/// - **RLS condition objects**: an object named only inside a `restrictionByCondition` query is
///   recovered by [`rls_condition_objects`] and emitted as `inferred`.
///
/// Direct edges are emitted first into a shared `(from,to)` seen set, so an RLS edge to an
/// already-linked object is suppressed and `resolved` wins. Role and object names are
/// canonicalized through the shared `mdo_canonical` so they coincide with the objects' own
/// nodes from other passes.
pub fn project_workspace_role_edges<DB: ConfigsDatabase>(
    db: &DB,
    representative: FileId,
    state: &mut GraphBuildState,
) -> Vec<WorkspaceCallEdge> {
    // Direct object-rights references and the RLS condition texts to resolve, gathered
    // deterministically so canonicalization sees a load-order-independent first-seen spelling.
    let mut direct: Vec<(String, MdoType, String)> = Vec::new();
    // (role_name, parent_type, parent_name, condition)
    let mut rls: Vec<(String, MdoType, String, String)> = Vec::new();
    for role in db.enumerate_roles(representative) {
        for obj in role.objects() {
            direct.push((role.name().to_string(), obj.mdo_type, obj.name.clone()));
            for condition in &obj.restrictions {
                rls.push((
                    role.name().to_string(),
                    obj.mdo_type,
                    obj.name.clone(),
                    condition.clone(),
                ));
            }
        }
    }
    direct.sort();
    direct.dedup();
    rls.sort();
    rls.dedup();

    // Direct object rights resolve entirely from the enumerated roles above. The whole
    // configuration is needed only as the SDBL lowering universe for RLS restriction
    // queries, which may reference *any* metadata object — a per-object resolver cannot
    // bound what a condition names. Roles without restrictions never touch it, so the
    // coarse dependency is taken only when a restriction is actually present.
    let merged =
        if rls.is_empty() { None } else { db.merged_visible_configuration(representative) };

    let mut edges = Vec::new();
    let mut seen: FxHashSet<(GraphNode, GraphNode)> = FxHashSet::default();

    let role_node = |state: &mut GraphBuildState, role_name: &str| GraphNode::Mdo {
        mdo_type: MdoType::Role,
        object_name: state.mdo_canonical.canonical(MdoType::Role, role_name),
    };

    // Direct object-rights edges first so `resolved` wins any (role, object) collision with RLS.
    for (role_name, mdo_type, obj_name) in &direct {
        let from = role_node(state, role_name);
        let to = GraphNode::Mdo {
            mdo_type: *mdo_type,
            object_name: state.mdo_canonical.canonical(*mdo_type, obj_name),
        };
        if seen.insert((from.clone(), to.clone())) {
            edges.push(role_reference_edge(from, to, EdgeProvenance::Resolved));
        }
    }

    // RLS condition objects: objects named inside the restriction query but not already granted
    // a direct right. The parent object itself is filtered out by `rls_condition_objects`.
    for (role_name, parent_type, parent_name, condition) in &rls {
        for (mdo_type, obj_name) in
            rls_condition_objects(*parent_type, parent_name, condition, &merged)
        {
            let from = role_node(state, role_name);
            let to = GraphNode::Mdo {
                mdo_type,
                object_name: state.mdo_canonical.canonical(mdo_type, &obj_name),
            };
            if seen.insert((from.clone(), to.clone())) {
                edges.push(role_reference_edge(from, to, EdgeProvenance::Inferred));
            }
        }
    }
    edges
}

/// Resolve the metadata objects named inside an RLS restriction `condition` by wrapping it in a
/// synthetic query over the restricted object and reading the resolved tables. The parent
/// object (the wrapper's own `ИЗ` table) is excluded — it is already covered by the direct
/// object-rights edge.
///
/// A bare condition fragment resolves no tables on its own (`parse_sdbl` needs a top-level
/// `ВЫБРАТЬ … ИЗ`), so wrapping is required. To avoid a malformed or injected condition leaking
/// spurious top-level tables, the result is taken only when the wrapper parses cleanly and
/// reduces to exactly one top-level query; a condition that is itself a full query (`ВЫБРАТЬ`/
/// `SELECT`) is skipped rather than spliced after `ГДЕ`. Legacy `#`-macro templates parse with
/// errors and are dropped here — harmless, since the restricted object is still linked directly.
fn rls_condition_objects(
    parent_type: MdoType,
    parent_name: &str,
    condition: &str,
    config: &Option<std::sync::Arc<bsl_metadata::Configuration>>,
) -> Vec<(MdoType, String)> {
    let cond = strip_leading_where(condition);
    if cond.is_empty() {
        return Vec::new();
    }
    // A condition that is itself a SELECT is not a boolean fragment; never splice it after `ГДЕ`.
    let head = cond.split_whitespace().next().unwrap_or("").fold_lower();
    if head == "выбрать" || head == "select" {
        return Vec::new();
    }

    let wrapped = format!(
        "ВЫБРАТЬ 1 ИЗ {}.{} КАК {} ГДЕ {}",
        parent_type.russian_name(),
        parent_name,
        parent_name,
        cond
    );
    let parse = parser::parse_sdbl(&wrapped);
    if parse.has_errors() {
        return Vec::new();
    }
    let package = sdbl_hir::lower_sdbl_to_hir(&parse, config.clone());
    // Exactly one top-level query: a `;`-separated injection would add another and is rejected.
    if package.queries().len() != 1 {
        return Vec::new();
    }

    let mut resolved = Vec::new();
    for query in package.queries() {
        query.hir.collect_resolved_tables(&mut resolved);
    }
    let parent_lower = parent_name.fold_lower();
    let mut out = Vec::new();
    let mut seen: FxHashSet<(MdoType, String)> = FxHashSet::default();
    for table in resolved {
        let (mdo_type, name) = match table {
            sdbl_hir::ResolvedTable::Metadata { mdo_type, name, .. }
            | sdbl_hir::ResolvedTable::Register { mdo_type, name, .. } => (*mdo_type, name.clone()),
            sdbl_hir::ResolvedTable::TempTable { .. } => continue,
        };
        // Skip the wrapper's own FROM table (the restricted object) — already linked directly.
        if mdo_type == parent_type && name.fold_lower() == parent_lower {
            continue;
        }
        if seen.insert((mdo_type, name.fold_lower())) {
            out.push((mdo_type, name));
        }
    }
    out
}

/// Strip a single leading `ГДЕ` / `WHERE` keyword (bilingual, case-insensitive) from an RLS
/// condition so it can be spliced after the wrapper's own `ГДЕ`.
fn strip_leading_where(condition: &str) -> &str {
    let trimmed = condition.trim();
    for kw in ["где", "where"] {
        let Some(at) = stdx::case::find_ignore_case(trimmed, kw).filter(|at| at.start == 0) else {
            continue;
        };
        // Only strip a standalone keyword (followed by whitespace), not an identifier prefix.
        if trimmed[at.end..].starts_with(char::is_whitespace) {
            return trimmed[at.end..].trim_start();
        }
    }
    trimmed
}

fn role_reference_edge(
    from: GraphNode,
    to: GraphNode,
    provenance: EdgeProvenance,
) -> WorkspaceCallEdge {
    WorkspaceCallEdge {
        from,
        to,
        kind: EdgeKind::RoleReference,
        provenance,
        call_site: CallSite::Structural,
        crosses_client_to_server: false,
    }
}

fn contains_edge(from: GraphNode, to: GraphNode) -> WorkspaceCallEdge {
    WorkspaceCallEdge {
        from,
        to,
        kind: EdgeKind::Contains,
        provenance: EdgeProvenance::Resolved,
        call_site: CallSite::Structural,
        crosses_client_to_server: false,
    }
}

fn data_binding_edge(from: GraphNode, to: GraphNode) -> WorkspaceCallEdge {
    WorkspaceCallEdge {
        from,
        to,
        kind: EdgeKind::DataBinding,
        provenance: EdgeProvenance::Resolved,
        call_site: CallSite::Structural,
        crosses_client_to_server: false,
    }
}

/// Resolve the form data-bindings gathered by the form pass against the catalog index
/// built by the catalog pass, emitting `DataBinding` edges. Pure (no database): run on
/// the driver thread AFTER the catalog pass so `state.catalog_index` is complete.
///
/// Each binding's target object is looked up in the catalog — only objects that the
/// catalog actually emitted (and, for a field/column binding, only declared non-standard
/// fields) produce an edge, so a `DataBinding` edge can never dangle. The catalog also
/// supplies the canonical object spelling and the metadata-cased field/section/column
/// names, so the to-id byte-matches the node the catalog pass emitted. Full-build only,
/// like the form and catalog passes it depends on.
pub fn project_form_binding_edges(state: &GraphBuildState) -> Vec<WorkspaceCallEdge> {
    let mut edges = Vec::new();
    let mut seen: FxHashSet<(GraphNode, GraphNode)> = FxHashSet::default();
    for binding in &state.form_bindings {
        let Some(entry) =
            state.catalog_index.get(&(binding.target_mdo, binding.target_obj.fold_lower()))
        else {
            continue;
        };
        let to = match binding.field_path.as_slice() {
            // Ref-typed form attribute → the whole backing object.
            [] => GraphNode::Mdo {
                mdo_type: binding.target_mdo,
                object_name: entry.object_name.clone(),
            },
            // `Объект.<поле>` → an object attribute.
            [field] => {
                let Some(attr) = entry.attrs.get(&field.fold_lower()) else { continue };
                GraphNode::Attribute {
                    mdo_type: binding.target_mdo,
                    object_name: entry.object_name.clone(),
                    attr_name: attr.clone(),
                }
            }
            // `Объект.<ТЧ>.<колонка>` → a tabular-section column.
            [section, column] => {
                let Some(sec) = entry.sections.get(&section.fold_lower()) else { continue };
                let Some(col) = sec.cols.get(&column.fold_lower()) else { continue };
                GraphNode::TabularSectionAttribute {
                    mdo_type: binding.target_mdo,
                    object_name: entry.object_name.clone(),
                    section_name: sec.section_name.clone(),
                    attr_name: col.clone(),
                }
            }
            // Deeper ref-chains are not resolved.
            _ => continue,
        };
        if seen.insert((binding.from.clone(), to.clone())) {
            edges.push(data_binding_edge(binding.from.clone(), to));
        }
    }
    edges
}

// ---- build-time durable-id encoding + row projection -----------------------

/// Encode a module key to the durable id scope segment. Shared with `ide::graph`'s
/// serving path so build-time ids and serve-time ids agree.
pub fn encode_scope(key: &ModuleKey) -> String {
    match key {
        ModuleKey::Common { name } => format!("common/{name}"),
        ModuleKey::Manager { mdo_type, name } => {
            format!("manager/{}/{name}", mdo_type.english_name())
        }
        ModuleKey::Object { mdo_type, name } => {
            format!("object/{}/{name}", mdo_type.english_name())
        }
        ModuleKey::RecordSet { mdo_type, name } => {
            format!("recordset/{}/{name}", mdo_type.english_name())
        }
    }
}

/// The human-facing qualified scope for a module key (e.g. `ОбщийМодуль.X`).
pub fn display_scope(key: &ModuleKey) -> String {
    match key {
        ModuleKey::Common { name } => format!("ОбщийМодуль.{name}"),
        ModuleKey::Manager { mdo_type, name } => {
            format!("{}.{name}.МодульМенеджера", mdo_type.russian_name())
        }
        // A constant's object slot holds its value-manager module.
        ModuleKey::Object { mdo_type: MdoType::Constant, name } => {
            format!("{}.{name}.МодульМенеджераЗначения", MdoType::Constant.russian_name())
        }
        ModuleKey::Object { mdo_type, name } => {
            format!("{}.{name}.МодульОбъекта", mdo_type.russian_name())
        }
        ModuleKey::RecordSet { mdo_type, name } => {
            format!("{}.{name}.МодульНабораЗаписей", mdo_type.russian_name())
        }
    }
}

fn basename(path: &str) -> Option<&str> {
    path.rsplit('/').next()
}

fn dispatch_labels(d: MethodDispatch) -> Vec<&'static str> {
    let mut labels = Vec::new();
    if d.can_run_on_client {
        labels.push("client");
    }
    if d.can_run_on_server {
        labels.push("server");
    }
    labels
}

fn edge_kind_label(kind: EdgeKind) -> &'static str {
    match kind {
        EdgeKind::DirectLocal | EdgeKind::DirectQualifiedModule => "call",
        EdgeKind::ManagerCreates => "manager_creates",
        EdgeKind::ManagerAccess => "manager_access",
        EdgeKind::QueryRef => "query_ref",
        EdgeKind::Contains => "contains",
        EdgeKind::DataBinding => "data_binding",
        EdgeKind::NotifyRef => "notify_ref",
        EdgeKind::IdleHandler => "idle_handler",
        EdgeKind::EventSubscriptionRef => "event_subscription",
        EdgeKind::RegisterMovement => "register_movement",
        EdgeKind::SubsystemMembership => "subsystem_membership",
        EdgeKind::RoleReference => "role_reference",
        EdgeKind::RegisterRecords => "register_records",
        EdgeKind::RegisterRecordSet => "register_record_set",
    }
}

/// The durable id scope segment for a form's owner: `<EnglishType>/<Object>` for an
/// object-owned form, or `common` for a common form. Shared with `ide::graph`'s
/// serving path so build-time ids and serve-time ids agree.
pub fn form_scope(owner: &Option<(MdoType, crate::name::Name)>) -> String {
    match owner {
        Some((mdo_type, object_name)) => {
            format!("{}/{}", mdo_type.english_name(), object_name.as_str())
        }
        None => "common".to_string(),
    }
}

/// The human-facing qualified-name prefix for a form's owner. Shared with
/// `ide::graph`'s serving path so build-time and serve-time `qualified` agree.
pub fn form_qualified_prefix(owner: &Option<(MdoType, crate::name::Name)>) -> String {
    match owner {
        Some((mdo_type, object_name)) => {
            format!("{}.{}", mdo_type.russian_name(), object_name.as_str())
        }
        None => "ОбщаяФорма".to_string(),
    }
}

fn provenance_label(p: EdgeProvenance) -> &'static str {
    match p {
        EdgeProvenance::Resolved => "resolved",
        EdgeProvenance::Inferred => "inferred",
        EdgeProvenance::VisibilityBlocked => "visibility_blocked",
        EdgeProvenance::Unresolved => "unresolved",
        EdgeProvenance::StringResolved => "string_resolved",
    }
}

/// A graph node projected for storage and serving. Source text is read on demand
/// from `file` + the ranges, never stored inline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeRow {
    pub id: String,
    pub kind: &'static str,
    pub name: String,
    pub qualified: String,
    pub module: Option<String>,
    /// Where the node is defined: a module's `.bsl`, a metadata object's own
    /// file. Absent for the kinds nothing discovers a file for (attributes,
    /// forms, an object whose family has no discovery). Never a predicate for
    /// "this node has BSL source" — ask `kind` for that.
    pub file: Option<String>,
    /// Byte offset of the declaration name token — the start of the signature.
    pub name_offset: Option<u32>,
    /// Byte offset of the declaration header end (closing `)` or export keyword) —
    /// the end of the full, possibly multi-line, signature slice (method nodes only).
    pub sig_end: Option<u32>,
    /// Method source byte range (method nodes only).
    pub src_start: Option<u32>,
    pub src_end: Option<u32>,
    pub dispatch: Vec<&'static str>,
    pub is_export: Option<bool>,
    /// Whether the id round-trips back to a node on its own.
    pub addressable: bool,
}

/// A resolved edge projected for storage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeRow {
    pub from_id: String,
    pub to_id: String,
    pub kind: &'static str,
    pub provenance: &'static str,
    /// The call site's byte range in the `from` node's file, when this build recorded one.
    /// One row per site: the store keeps edge multiplicity as the projection produced it,
    /// and serving groups the rows back into one edge carrying every span.
    pub call_start: Option<u32>,
    pub call_end: Option<u32>,
    /// Why there is no span, when there is none. Persisted rather than derived at read time:
    /// the two absences differ by which pass produced the row, and nothing in the row itself
    /// says which — least of all `kind`.
    pub call_site_absent: Option<&'static str>,
    pub crosses: bool,
}

/// Encodes graph nodes/edges to durable rows at build time — method names/ranges
/// Canonical target/caller rows used to compare call-hierarchy storage backends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MethodCallDigest {
    rows: Vec<(String, String)>,
}

impl MethodCallDigest {
    /// Sort and deduplicate durable `(target_method_id, caller_method_id)` rows.
    pub fn from_rows(rows: impl IntoIterator<Item = (String, String)>) -> Self {
        let mut rows: Vec<_> = rows.into_iter().collect();
        rows.sort_unstable();
        rows.dedup();
        Self { rows }
    }

    pub fn rows(&self) -> &[(String, String)] {
        &self.rows
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

/// Encode compact-index method pairs with the durable graph id rules used by SQLite.
pub fn call_hierarchy_method_digest(
    reverse_index: &CallHierarchyReverseIndex,
    graph_index: &GraphIndex,
    paths: &FxHashMap<FileId, String>,
    workspace_root: Option<&Path>,
) -> MethodCallDigest {
    // Method ids only; this digest never encodes a row, so it has no object to
    // place.
    let no_objects = MdoFiles::default();
    let encoder = GraphRowEncoder::new(graph_index, paths, workspace_root, &no_objects);
    let mut rows = Vec::new();
    for target in graph_index.method_nodes() {
        let target_id = encoder.encode(&GraphNode::Method(target)).0;
        rows.extend(reverse_index.callers(target).iter().map(|caller| {
            let caller_id = encoder.encode(&GraphNode::Method(*caller)).0;
            (target_id.clone(), caller_id)
        }));
    }
    MethodCallDigest::from_rows(rows)
}

/// Where each metadata object is defined, keyed the way the graph itself keys an
/// object: by kind and CASE-FOLDED name.
///
/// A folded key is not a convenience. An `Mdo` node carries the first spelling
/// the build saw, and the call/query projections run before the catalog pass —
/// so for every object mentioned in code that spelling comes from the code, not
/// from the file tree. An exact-spelling lookup would therefore miss precisely
/// the objects anyone searches for.
pub type MdoFiles = FxHashMap<(MdoType, String), String>;

/// The key an object goes under. Both sides — whoever fills the map and whoever
/// reads it — call this, so the folding cannot drift between them.
pub fn mdo_files_key(mdo_type: MdoType, object_name: &str) -> (MdoType, String) {
    (mdo_type, object_name.fold_lower())
}

/// Encodes durable graph rows from the resident [`GraphIndex`] and file-set paths,
/// with no database access. It produces the SAME durable ids as `ide::graph`, so ids
/// an agent holds survive the in-memory → SQLite switch.
pub struct GraphRowEncoder<'a> {
    index: &'a GraphIndex,
    paths: &'a FxHashMap<FileId, String>,
    workspace_root: Option<&'a Path>,
    mdo_files: &'a MdoFiles,
}

impl<'a> GraphRowEncoder<'a> {
    pub fn new(
        index: &'a GraphIndex,
        paths: &'a FxHashMap<FileId, String>,
        workspace_root: Option<&'a Path>,
        mdo_files: &'a MdoFiles,
    ) -> Self {
        Self { index, paths, workspace_root, mdo_files }
    }

    /// A metadata object's own file, when the caller's map knows it. A miss is
    /// ordinary — a kind nothing discovers has no file to give — and leaves the
    /// node addressable by its durable id, as it has always been.
    fn mdo_file(&self, mdo_type: MdoType, object_name: &Name) -> Option<String> {
        self.mdo_files.get(&mdo_files_key(mdo_type, object_name.as_str())).cloned()
    }

    fn path_for(&self, file: FileId) -> Option<String> {
        self.paths.get(&file).map(|p| p.replace('\\', "/"))
    }

    fn rel_path(&self, abs: &str) -> Option<String> {
        let root = self.workspace_root?;
        let root_str = root.to_str()?.replace('\\', "/");
        let stripped = abs.strip_prefix(&root_str)?;
        Some(stripped.trim_start_matches('/').to_string())
    }

    fn method_name(&self, method: MethodId) -> String {
        self.index.method_entry(method).map(|e| e.name.as_str().to_string()).unwrap_or_default()
    }

    fn module_display(&self, module: ModuleId) -> Option<String> {
        let path = self.path_for(module.file_id)?;
        match module_key_for_path(&path) {
            Some(key) => Some(display_scope(&key)),
            None => self.rel_path(&path).or_else(|| basename(&path).map(str::to_string)),
        }
    }

    /// The durable id and whether it round-trips on its own.
    pub fn encode(&self, node: &GraphNode) -> (String, bool) {
        match node {
            GraphNode::Method(method) => {
                let name = self.method_name(*method);
                let path = self.path_for(method.module.file_id);
                if let Some(key) = path.as_deref().and_then(module_key_for_path) {
                    (format!("method/{}/{name}", encode_scope(&key)), true)
                } else if let Some(rel) = path.as_deref().and_then(|p| self.rel_path(p)) {
                    (format!("method/file/{rel}::{name}"), true)
                } else {
                    let base = path.as_deref().and_then(basename).unwrap_or("?");
                    (format!("method/file/{base}::{name}"), false)
                }
            }
            GraphNode::ModuleCode(module) => {
                let path = self.path_for(module.file_id);
                if let Some(key) = path.as_deref().and_then(module_key_for_path) {
                    (format!("module/{}", encode_scope(&key)), true)
                } else if let Some(rel) = path.as_deref().and_then(|p| self.rel_path(p)) {
                    (format!("module/file/{rel}"), true)
                } else {
                    let base = path.as_deref().and_then(basename).unwrap_or("?");
                    (format!("module/file/{base}"), false)
                }
            }
            GraphNode::Mdo { mdo_type, object_name } => {
                (format!("mdo/{}/{}", mdo_type.english_name(), object_name.as_str()), true)
            }
            GraphNode::Attribute { mdo_type, object_name, attr_name } => (
                format!(
                    "attribute/{}/{}/{}",
                    mdo_type.english_name(),
                    object_name.as_str(),
                    attr_name.as_str()
                ),
                true,
            ),
            GraphNode::Form { owner, form_name } => {
                (format!("form/{}/{}", form_scope(owner), form_name.as_str()), true)
            }
            GraphNode::FormItem { owner, form_name, item_name } => (
                format!(
                    "form_item/{}/{}/{}",
                    form_scope(owner),
                    form_name.as_str(),
                    item_name.as_str()
                ),
                true,
            ),
            GraphNode::FormAttribute { owner, form_name, attr_name } => (
                format!(
                    "form_attr/{}/{}/{}",
                    form_scope(owner),
                    form_name.as_str(),
                    attr_name.as_str()
                ),
                true,
            ),
            GraphNode::TabularSection { mdo_type, object_name, section_name } => (
                format!(
                    "tabular_section/{}/{}/{}",
                    mdo_type.english_name(),
                    object_name.as_str(),
                    section_name.as_str()
                ),
                true,
            ),
            GraphNode::TabularSectionAttribute {
                mdo_type,
                object_name,
                section_name,
                attr_name,
            } => (
                format!(
                    "ts_attr/{}/{}/{}/{}",
                    mdo_type.english_name(),
                    object_name.as_str(),
                    section_name.as_str(),
                    attr_name.as_str()
                ),
                true,
            ),
        }
    }

    /// Project a node to its storage row.
    pub fn node_row(&self, node: &GraphNode) -> NodeRow {
        let (id, addressable) = self.encode(node);
        match node {
            GraphNode::Method(method) => {
                let entry = self.index.method_entry(*method);
                let name = entry.map(|e| e.name.as_str().to_string()).unwrap_or_default();
                let module = self.module_display(method.module);
                let qualified = match &module {
                    Some(scope) => format!("{scope}.{name}"),
                    None => name.clone(),
                };
                NodeRow {
                    id,
                    kind: "method",
                    name,
                    qualified,
                    module,
                    file: self.path_for(method.module.file_id),
                    name_offset: entry.map(|e| e.name_range.start().into()),
                    sig_end: entry.map(|e| e.sig_end.into()),
                    src_start: entry.map(|e| e.source_range.start().into()),
                    src_end: entry.map(|e| e.source_range.end().into()),
                    dispatch: self.index.dispatch(node).map(dispatch_labels).unwrap_or_default(),
                    is_export: entry.map(|e| e.is_export),
                    addressable,
                }
            }
            GraphNode::ModuleCode(module) => {
                let display = self.module_display(*module);
                let name = display.clone().unwrap_or_else(|| "<модуль>".to_string());
                NodeRow {
                    id,
                    kind: "module",
                    name: name.clone(),
                    qualified: name,
                    module: display,
                    file: self.path_for(module.file_id),
                    name_offset: None,
                    sig_end: None,
                    src_start: None,
                    src_end: None,
                    dispatch: self.index.dispatch(node).map(dispatch_labels).unwrap_or_default(),
                    is_export: None,
                    addressable,
                }
            }
            GraphNode::Mdo { mdo_type, object_name } => NodeRow {
                id,
                kind: "mdo",
                name: object_name.as_str().to_string(),
                qualified: format!("{}.{}", mdo_type.russian_name(), object_name.as_str()),
                module: None,
                file: self.mdo_file(*mdo_type, object_name),
                name_offset: None,
                sig_end: None,
                src_start: None,
                src_end: None,
                dispatch: Vec::new(),
                is_export: None,
                addressable,
            },
            GraphNode::Attribute { mdo_type, object_name, attr_name } => NodeRow {
                id,
                kind: "attribute",
                name: attr_name.as_str().to_string(),
                qualified: format!(
                    "{}.{}.{}",
                    mdo_type.russian_name(),
                    object_name.as_str(),
                    attr_name.as_str()
                ),
                module: None,
                file: None,
                name_offset: None,
                sig_end: None,
                src_start: None,
                src_end: None,
                dispatch: Vec::new(),
                is_export: None,
                addressable,
            },
            GraphNode::Form { owner, form_name } => NodeRow {
                id,
                kind: "form",
                name: form_name.as_str().to_string(),
                qualified: format!("{}.Форма.{}", form_qualified_prefix(owner), form_name.as_str()),
                module: None,
                file: None,
                name_offset: None,
                sig_end: None,
                src_start: None,
                src_end: None,
                dispatch: Vec::new(),
                is_export: None,
                addressable,
            },
            GraphNode::FormItem { owner, form_name, item_name } => NodeRow {
                id,
                kind: "form_item",
                name: item_name.as_str().to_string(),
                qualified: format!(
                    "{}.Форма.{}.{}",
                    form_qualified_prefix(owner),
                    form_name.as_str(),
                    item_name.as_str()
                ),
                module: None,
                file: None,
                name_offset: None,
                sig_end: None,
                src_start: None,
                src_end: None,
                dispatch: Vec::new(),
                is_export: None,
                addressable,
            },
            GraphNode::FormAttribute { owner, form_name, attr_name } => NodeRow {
                id,
                kind: "form_attribute",
                name: attr_name.as_str().to_string(),
                qualified: format!(
                    "{}.Форма.{}.Реквизит.{}",
                    form_qualified_prefix(owner),
                    form_name.as_str(),
                    attr_name.as_str()
                ),
                module: None,
                file: None,
                name_offset: None,
                sig_end: None,
                src_start: None,
                src_end: None,
                dispatch: Vec::new(),
                is_export: None,
                addressable,
            },
            GraphNode::TabularSection { mdo_type, object_name, section_name } => NodeRow {
                id,
                kind: "tabular_section",
                name: section_name.as_str().to_string(),
                qualified: format!(
                    "{}.{}.ТабличнаяЧасть.{}",
                    mdo_type.russian_name(),
                    object_name.as_str(),
                    section_name.as_str()
                ),
                module: None,
                file: None,
                name_offset: None,
                sig_end: None,
                src_start: None,
                src_end: None,
                dispatch: Vec::new(),
                is_export: None,
                addressable,
            },
            GraphNode::TabularSectionAttribute {
                mdo_type,
                object_name,
                section_name,
                attr_name,
            } => NodeRow {
                id,
                kind: "attribute",
                name: attr_name.as_str().to_string(),
                qualified: format!(
                    "{}.{}.{}.{}",
                    mdo_type.russian_name(),
                    object_name.as_str(),
                    section_name.as_str(),
                    attr_name.as_str()
                ),
                module: None,
                file: None,
                name_offset: None,
                sig_end: None,
                src_start: None,
                src_end: None,
                dispatch: Vec::new(),
                is_export: None,
                addressable,
            },
        }
    }

    /// Project a resolved edge to its storage row.
    pub fn edge_row(&self, edge: &WorkspaceCallEdge) -> EdgeRow {
        let (call_start, call_end, call_site_absent) = match edge.call_site {
            CallSite::Recorded(range) => {
                (Some(range.start().into()), Some(range.end().into()), None)
            }
            CallSite::NotRecorded => (None, None, Some(CALL_SITE_NOT_RECORDED)),
            CallSite::Structural => (None, None, Some(NO_CALL_SITE)),
        };
        EdgeRow {
            from_id: self.encode(&edge.from).0,
            to_id: self.encode(&edge.to).0,
            kind: edge_kind_label(edge.kind),
            provenance: provenance_label(edge.provenance),
            call_start,
            call_end,
            call_site_absent,
            crosses: edge.crosses_client_to_server,
        }
    }
}

#[cfg(test)]
mod role_rls_tests {
    use super::{rls_condition_objects, strip_leading_where};
    use bsl_metadata::{Configuration, MdoType, MetadataObject};
    use std::sync::Arc;

    fn config() -> Option<Arc<Configuration>> {
        let mut c = Configuration::new("Test");
        c.add_metadata_object(MetadataObject::new(MdoType::Catalog, "Контрагенты"));
        c.add_metadata_object(MetadataObject::new(MdoType::Catalog, "Организации"));
        Some(Arc::new(c))
    }

    fn objs(condition: &str) -> Vec<(MdoType, String)> {
        rls_condition_objects(MdoType::Catalog, "Контрагенты", condition, &config())
    }

    #[test]
    fn strip_leading_where_is_bilingual_and_keyword_bounded() {
        assert_eq!(strip_leading_where("ГДЕ Х = 1"), "Х = 1");
        assert_eq!(strip_leading_where("  где Х = 1"), "Х = 1");
        assert_eq!(strip_leading_where("WHERE X = 1"), "X = 1");
        assert_eq!(strip_leading_where("where X = 1"), "X = 1");
        // Not a standalone keyword — `Гдето` must not be truncated to `то`.
        assert_eq!(strip_leading_where("Гдето = 1"), "Гдето = 1");
    }

    #[test]
    fn rls_recovers_subquery_object_and_excludes_parent() {
        let found = objs("Контрагенты.Ссылка В (ВЫБРАТЬ Ссылка ИЗ Справочник.Организации)");
        // The subquery object is recovered; the wrapper's own FROM (the parent) is excluded.
        assert_eq!(found, vec![(MdoType::Catalog, "Организации".to_string())]);
    }

    #[test]
    fn rls_rejects_semicolon_injection() {
        // A second `;`-separated top-level query must not leak its table as a role reference.
        assert!(
            objs("Контрагенты.Удален = ЛОЖЬ; ВЫБРАТЬ Ссылка ИЗ Справочник.Организации").is_empty(),
            "a multi-statement injection is rejected wholesale"
        );
    }

    #[test]
    fn rls_skips_full_query_condition() {
        // A condition that is itself a SELECT is not spliced after `ГДЕ`.
        assert!(objs("ВЫБРАТЬ Ссылка ИЗ Справочник.Организации").is_empty());
    }

    #[test]
    fn rls_drops_macro_template_without_false_edge() {
        // A legacy `#`-macro template parses with errors → dropped; no spurious object.
        assert!(objs("#ПоЗначениям(\"Справочник.Организации\")").is_empty());
    }
}

#[cfg(test)]
mod module_layout_hash_tests {
    use super::*;

    fn hashes(source: &str) -> (u64, u64) {
        hashes_marked(source, false)
    }

    fn hashes_marked(source: &str, unread: bool) -> (u64, u64) {
        let module = ModuleId::new(FileId(0));
        let parse = parser::parse(source);
        let mut methods =
            crate::call_graph::extract_graph_methods(&crate::ItemTree::from_parse(&parse));
        for entry in &mut methods {
            let start = u32::from(entry.name_range.start()) as usize;
            let end = u32::from(entry.sig_end) as usize;
            entry.signature_hash = source.get(start..end).map_or(0, signature_hash);
        }
        let mut index = GraphIndex::new();
        index.insert_module_data(module, methods, None, unread);

        (
            index.module_sig_hash(module).expect("indexed module has a signature hash"),
            index.module_layout_hash(module).expect("indexed module has a layout hash"),
        )
    }

    /// The index answers the same method the symbol tree does, so both must fold
    /// a name the same way (per character, as `NormName`); a contextual fold
    /// puts Greek final sigma in another bucket.
    #[test]
    fn method_lookup_folds_names_like_the_symbol_tree() {
        let source = "Функция ΟΔΟΣ() Экспорт\n\tВозврат 1;\nКонецФункции\nФункция οδοσ() Экспорт\n\tВозврат 2;\nКонецФункции\n";
        let module = ModuleId::new(FileId(0));
        let item_tree = crate::ItemTree::from_parse(&parser::parse(source));
        let symbol_tree = crate::SymbolTree::from_item_tree_no_docs(&item_tree, module);
        let mut index = GraphIndex::new();
        index.insert_module_data(
            module,
            crate::call_graph::extract_graph_methods(&item_tree),
            None,
            false,
        );

        for spelling in ["ΟΔΟΣ", "οδοσ"] {
            let name = Name::new(spelling);
            let expected = symbol_tree.find_method(&name).map(|m| m.id.local_id);
            assert_eq!(expected.map(|key| key.ordinal), Some(0), "{spelling}: first declaration");
            assert_eq!(
                index.find_method(module, &name).map(|m| m.local_id),
                expected,
                "{spelling}"
            );
        }
    }

    /// A call blocked by a non-exported declaration is not answered — it resolves to
    /// nothing, and adding `Экспорт` to that very declaration makes an edge appear. The
    /// reverse references are the only index able to find its callers then, so the walk
    /// that decides whether to record them has to stop where resolution stops: at the
    /// first DECLARATION, not at the first exported one.
    #[test]
    fn a_call_blocked_by_a_non_exported_declaration_keeps_its_reverse_references() {
        fn targets(base_source: &str) -> Vec<ModuleId> {
            let base = ModuleId::new(FileId(0));
            let ext = ModuleId::new(FileId(1));
            let mut index = GraphIndex::new();
            for (module, source) in
                [(base, base_source), (ext, "Процедура П() Экспорт КонецПроцедуры")]
            {
                let parse = parser::parse(source);
                let methods =
                    crate::call_graph::extract_graph_methods(&crate::ItemTree::from_parse(&parse));
                index.insert_module_data(module, methods, None, false);
            }
            let candidates = crate::resolver::CommonModuleCandidates::new(
                vec![(base, false), (ext, false)],
                false,
            );
            let name = Name::new("П");
            super::reference_targets(&candidates, |m| index.find_method(m, &name))
        }

        // Control: a base body that exports the name really does answer the call, and
        // an answered call owes nobody a reference.
        assert_eq!(targets("Процедура П() Экспорт КонецПроцедуры"), Vec::<ModuleId>::new());

        let base = ModuleId::new(FileId(0));
        let ext = ModuleId::new(FileId(1));
        assert_eq!(
            targets("Процедура П() КонецПроцедуры"),
            vec![base, ext],
            "a non-exported declaration blocks the call, so both bodies stay tracked"
        );
    }

    /// Both hashes are read as proof that other modules' calls still resolve the same
    /// way. An unread body bars callers from any body behind it, so crossing that
    /// barrier has to move them — and it is invisible to everything else they hash,
    /// because an unread body and an empty readable one declare the same nothing.
    #[test]
    fn both_hashes_move_when_a_body_crosses_the_unread_barrier() {
        let readable = hashes_marked("", false);
        let unread = hashes_marked("", true);
        assert_ne!(readable.0, unread.0, "signature identity must see the barrier");
        assert_ne!(readable.1, unread.1, "resident layout identity must see it too");

        // Control: everything else about the two indexed modules is identical, so the
        // inequality above is the flag speaking and not two unrelated modules.
        assert_eq!(readable, hashes_marked("", false));
    }

    /// A module variable is not a method: adding one above the methods changes
    /// neither what the module declares to its callers nor which method is
    /// which, so the catch-up build has no reason to start over.
    #[test]
    fn a_top_level_variable_above_the_methods_moves_neither_hash() {
        let before = hashes("Процедура Выполнить() Экспорт\nКонецПроцедуры");
        let after = hashes("Перем Счетчик;\nПроцедура Выполнить() Экспорт\nКонецПроцедуры");

        assert_eq!(before.0, after.0, "durable signature identity");
        assert_eq!(before.1, after.1, "resident layout identity");
    }

    /// A new method is a new declaration: callers may now resolve to it, so
    /// both identities move — the control for the variable case above.
    #[test]
    fn a_method_inserted_above_moves_both_hashes() {
        let before = hashes("Процедура Выполнить() Экспорт\nКонецПроцедуры");
        let after = hashes(
            "Процедура Новая() Экспорт\nКонецПроцедуры\nПроцедура Выполнить() Экспорт\nКонецПроцедуры",
        );

        assert_ne!(before.0, after.0, "durable signature identity");
        assert_ne!(before.1, after.1, "resident layout identity");
    }

    #[test]
    fn exported_parameter_composition_moves_signature_hash() {
        let before = hashes("Процедура Выполнить(Знач А) Экспорт\nКонецПроцедуры");
        let after = hashes("Процедура Выполнить(Знач А, Б) Экспорт\nКонецПроцедуры");

        assert_ne!(before.0, after.0, "callers must be reconsidered when parameters change");
    }

    #[test]
    fn module_layout_hash_ignores_method_body_edits() {
        // Given: the same declaration surface with a body-only edit.
        let before = hashes("Процедура Выполнить() Экспорт\nСообщить(\"до\");\nКонецПроцедуры");
        let after = hashes("Процедура Выполнить() Экспорт\nСообщить(\"после\");\nКонецПроцедуры");

        // Then: neither durable signature nor resident layout identity changes.
        assert_eq!(before, after);
    }

    #[test]
    fn module_layout_hash_changes_with_signature_and_effective_dispatch() {
        // Given: a declaration whose spelling, export, and effective dispatch all change.
        let before = hashes("&НаКлиенте\nПроцедура Выполнить() Экспорт\nКонецПроцедуры");
        let after = hashes("&НаСервере\nПроцедура ВыполнитьНаСервере()\nКонецПроцедуры");

        // Then: both declaration contracts change.
        assert_ne!(before.0, after.0);
        assert_ne!(before.1, after.1);
    }
}
