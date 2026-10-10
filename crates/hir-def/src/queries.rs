use std::sync::Arc;
use stdx::case::CaseExt;

use base_db::FileIdInput;
use bsl_metadata::MdoType;
use rustc_hash::{FxHashMap, FxHashSet};

use vfs::FileId;

use crate::{
    body::ExternalRef,
    call_graph::{GraphNode, MethodDispatch, WorkspaceCallEdge},
    module_index::ModuleIndex,
    DefDatabase, ModuleBodies, ModuleData, ModuleId, WorkspaceMembers,
};

pub use crate::conditional_tree::conditional_tree_query;
pub use crate::item_tree::item_tree_query;
pub use crate::region_tree::region_tree_query;
pub use crate::symbol_tree::symbol_tree_query;
pub use crate::workspace_index::workspace_index_query;

/// `heap_size` estimators wired into Salsa's `memory_usage` report. Each returns an
/// approximate live-heap byte count for the query's memoised output: hashbrown
/// table capacity is derived from length (load factor 7/8), owned `String`/`Vec`
/// payloads are summed, and `Name`s contribute their non-inlined `SmolStr` length.
/// Over-approximate by design — the goal is a per-ingredient memory map.
mod heap_estimate {
    use super::*;
    use crate::call_graph::{CallTarget, ModuleCallSummary};
    use crate::heap_estimate::{name_bytes, vec_bytes};

    pub(super) fn module_data_heap(v: &Arc<ModuleData>) -> usize {
        let d = &**v;
        std::mem::size_of::<ModuleData>()
            + d.name.as_ref().map_or(0, name_bytes)
            + vec_bytes::<crate::MethodId>(d.procedures.len())
            + vec_bytes::<crate::MethodId>(d.functions.len())
            + vec_bytes::<crate::VariableId>(d.variables.len())
    }

    pub(super) fn module_bodies_heap(v: &Arc<ModuleBodies>) -> usize {
        v.estimated_heap()
    }

    fn call_target_name_heap(target: &CallTarget) -> usize {
        match target {
            CallTarget::QualifiedModule { module_name, method_name } => {
                name_bytes(module_name) + name_bytes(method_name)
            }
            CallTarget::ManagerAccess { object_name, method_name, .. } => {
                name_bytes(object_name) + method_name.as_ref().map_or(0, name_bytes)
            }
            CallTarget::ThisObjectMethod { method_name } => name_bytes(method_name),
            CallTarget::RegisterMovement { register_name } => name_bytes(register_name),
            CallTarget::Local { .. } | CallTarget::Unresolved => 0,
        }
    }

    pub(super) fn module_call_summary_heap(v: &Arc<ModuleCallSummary>) -> usize {
        use crate::call_graph::{
            CallEdge, FormEventEntry, IdleReg, MethodSummary, NotifyReg, NotifyTarget, SetActionReg,
        };

        let s = &**v;
        let mut bytes = std::mem::size_of::<ModuleCallSummary>();

        bytes += vec_bytes::<MethodSummary>(s.methods.len());
        for m in &s.methods {
            bytes += name_bytes(&m.name);
        }
        bytes += vec_bytes::<CallEdge>(s.call_edges.len());
        for e in &s.call_edges {
            bytes += call_target_name_heap(&e.target);
        }
        bytes += vec_bytes::<NotifyReg>(s.notify_regs.len());
        for r in &s.notify_regs {
            bytes += name_bytes(&r.callback_name);
            if let NotifyTarget::Module(name) = &r.target {
                bytes += name_bytes(name);
            }
        }
        bytes += vec_bytes::<IdleReg>(s.idle_handler_regs.len());
        for r in &s.idle_handler_regs {
            bytes += name_bytes(&r.handler_name);
        }
        bytes += vec_bytes::<SetActionReg>(s.set_action_regs.len());
        for r in &s.set_action_regs {
            bytes += name_bytes(&r.handler_name);
        }
        bytes += vec_bytes::<u32>(s.name_literal_refs.len());
        bytes += vec_bytes::<FormEventEntry>(s.form_entries.len());
        for f in &s.form_entries {
            bytes += f.event_type.capacity() + name_bytes(&f.handler_name);
        }
        bytes
    }

    pub(super) fn file_external_refs_heap(v: &Arc<Vec<ExternalRef>>) -> usize {
        let mut bytes = vec_bytes::<ExternalRef>(v.len());
        for ext in v.iter() {
            bytes += crate::external_ref_name_heap(ext);
        }
        bytes
    }

    fn resolved_target_name_heap(target: &crate::call_graph::ResolvedTarget) -> usize {
        use crate::call_graph::ResolvedTarget;
        match target {
            ResolvedTarget::Method(_) => 0,
            ResolvedTarget::Mdo { object_name, .. } => name_bytes(object_name),
            ResolvedTarget::Unresolved(t) => call_target_name_heap(t),
        }
    }

    pub(super) fn resolved_module_summary_heap(
        v: &Arc<crate::call_graph::ResolvedModuleSummary>,
    ) -> usize {
        use crate::call_graph::ResolvedCallEdge;

        let s = &**v;
        let mut bytes = std::mem::size_of::<crate::call_graph::ResolvedModuleSummary>();
        bytes += vec_bytes::<ResolvedCallEdge>(s.edges.len());
        for e in &s.edges {
            bytes += resolved_target_name_heap(&e.target);
        }
        bytes
    }

    pub(super) fn file_dependencies_heap(v: &Arc<Vec<FileId>>) -> usize {
        vec_bytes::<FileId>(v.len())
    }
}

// Condensed per-module data (built from item_tree, no green-tree pin): on the
// cross-module resolution path. The cap holds a whole workspace, like `item_tree`'s,
// so a trim never costs a later request a re-derivation. (`module_bodies` below
// stays low — it is the heavy lowered HIR, needed only while a module's own file is
// analyzed.)
#[salsa::tracked(lru = 65536, heap_size = heap_estimate::module_data_heap, returns(ref))]
pub fn module_data_query<'db>(
    db: &'db dyn DefDatabase,
    file_id_input: FileIdInput<'db>,
) -> Arc<ModuleData> {
    let _span = tracing::info_span!("module_data", ?file_id_input).entered();
    let file_id = file_id_input.file_id(db);
    let tree = db.item_tree(file_id);
    let module_id = ModuleId::new(file_id);
    Arc::new(ModuleData::from_item_tree(module_id, tree))
}

fn module_code_lower_heap(v: &Arc<crate::body::LowerResult>) -> usize {
    crate::body::lower_result_heap(&Some(Arc::clone(v)))
}

/// Module-level code (statements outside any method) lowered from the file
/// root with the file's own line index; its positions are file positions.
#[salsa::tracked(lru = 1024, heap_size = module_code_lower_heap, returns(ref))]
pub fn module_code_lower_query<'db>(
    db: &'db dyn DefDatabase,
    file_id_input: FileIdInput<'db>,
) -> Arc<crate::body::LowerResult> {
    let file_id = file_id_input.file_id(db);
    let _span = tracing::info_span!("module_code_lower", ?file_id_input).entered();
    let parse = db.parse_ref(file_id);
    let text = db.file_text_ref(file_id);
    let line_index = Arc::new(line_index::LineIndex::new(text));
    Arc::new(crate::body::lower_module_code(&parse.syntax_node(), Some(line_index)))
}

#[salsa::tracked(lru = 128, heap_size = heap_estimate::module_bodies_heap, returns(ref))]
pub fn module_bodies_query<'db>(
    db: &'db dyn DefDatabase,
    file_id_input: FileIdInput<'db>,
) -> Arc<ModuleBodies> {
    let _span = tracing::info_span!("module_bodies", ?file_id_input).entered();
    let file_id = file_id_input.file_id(db);
    let module_id = ModuleId::new(file_id);

    let result = crate::lower_module_bodies(db, module_id);

    Arc::new(result)
}

/// Switch the lowering memos' LRU caps between the interactive profile and a
/// small sweep profile. Lowered bodies are the second-largest retained value after
/// parse trees (megabytes per module), and during a chunked whole-workspace sweep a
/// closed file's bodies are needed only while its own chunk is analyzed — so the
/// sweep shrinks the caps and restores the interactive ones when it ends. The
/// interactive values must stay equal to the `lru` literals on the queries; the
/// per-method chain keeps its interactive retention order (`method_syntax` ≥
/// `method_lower` ≥ `method_body` ≥ `infer_method`), which a sweep, having no
/// edits to validate against, does not need. The new caps take effect at the next
/// LRU trim; they evict nothing by themselves. Like any salsa write, this cancels
/// in-flight snapshots — call it only from points that may already trim.
pub fn set_lowering_lru_sweep_mode(db: &mut dyn crate::ConfigsDatabase, sweep: bool) {
    const FILE_INTERACTIVE: usize = 128;
    const FILE_SWEEP: usize = 16;
    const MODULE_CODE_INTERACTIVE: usize = 1024;
    const METHOD_INTERACTIVE: usize = 8192;
    const METHOD_SWEEP: usize = 2048;
    let file_cap = if sweep { FILE_SWEEP } else { FILE_INTERACTIVE };
    let module_code_cap = if sweep { FILE_SWEEP } else { MODULE_CODE_INTERACTIVE };
    let method_cap = if sweep { METHOD_SWEEP } else { METHOD_INTERACTIVE };
    module_bodies_query::set_lru_capacity(db, file_cap);
    module_code_lower_query::set_lru_capacity(db, module_code_cap);
    crate::method_syntax::set_lru_capacity(db, method_cap);
    crate::method_slab::set_lru_capacity(db, method_cap);
    crate::method_body::set_lru_capacity(db, method_cap);
    crate::sdbl_cache::set_method_sdbl_hir_lru_capacity(db, method_cap);
}

#[salsa::tracked(lru = 16, heap_size = crate::workspace::module_members_heap, returns(clone))]
pub fn module_members_query(
    db: &dyn DefDatabase,
    source_root_input: base_db::SourceRootInput,
) -> Arc<WorkspaceMembers> {
    let source_root = source_root_input.root(db);
    let file_set = source_root.file_set();
    let files: Vec<_> = source_root
        .iter()
        .filter(|&file_id| crate::workspace::is_bsl_source(file_set, file_id))
        .collect();
    let _span = tracing::info_span!("module_members", file_count = files.len()).entered();
    Arc::new(crate::workspace::module_members(db, &files))
}

#[salsa::tracked(lru = 256, heap_size = heap_estimate::module_call_summary_heap, returns(clone))]
pub fn module_call_summary_query<'db>(
    db: &'db dyn DefDatabase,
    file_id_input: FileIdInput<'db>,
) -> Arc<crate::call_graph::ModuleCallSummary> {
    let _span = tracing::info_span!("module_call_summary", ?file_id_input).entered();
    let file_id = file_id_input.file_id(db);
    let module_id = ModuleId::new(file_id);

    let item_tree = db.item_tree_ref(file_id);
    let module_bodies = db.module_bodies_ref(module_id);
    let module_metadata = db.module_metadata(module_id);

    let form_handlers: &[bsl_metadata::FormEventHandler] =
        module_metadata.form.as_ref().map(|f| f.event_handlers.as_slice()).unwrap_or(&[]);

    Arc::new(crate::call_graph::extract_call_summary(item_tree, module_bodies, form_handlers))
}

#[salsa::tracked(
    lru = 256,
    heap_size = heap_estimate::resolved_module_summary_heap,
    returns(clone)
)]
pub fn resolved_module_summary_query<'db>(
    db: &'db dyn crate::configs::ConfigsDatabase,
    file_id_input: FileIdInput<'db>,
) -> Arc<crate::call_graph::ResolvedModuleSummary> {
    use crate::call_graph::{
        CallTarget, EdgeKind, EdgeProvenance, ResolvedCallEdge, ResolvedTarget,
    };

    let _span = tracing::info_span!("resolved_module_summary", ?file_id_input).entered();
    let file_id = file_id_input.file_id(db);
    let module_id = ModuleId::new(file_id);

    let summary = db.module_call_summary(module_id);
    let resolver = crate::resolver::Resolver::with_workspace_scope(module_id);

    let mut edges = Vec::with_capacity(summary.call_edges.len());
    for edge in &summary.call_edges {
        // Each branch yields the resolved target, its provenance, and the edge
        // kind. The kind defaults to the extraction-time kind, but manager
        // accesses that land on a metadata object override it with
        // `ManagerCreates`/`ManagerAccess` (create-vs-touch is a semantic call).
        let (target, provenance, kind) = match &edge.target {
            CallTarget::Local { callee_local_id } => (
                ResolvedTarget::Method(crate::MethodId {
                    module: module_id,
                    local_id: *callee_local_id,
                }),
                EdgeProvenance::Resolved,
                edge.kind,
            ),
            CallTarget::QualifiedModule { module_name, method_name } => {
                match resolver.resolve_qualified_method(db, module_name, method_name) {
                    Ok(r) if r.is_export => {
                        (ResolvedTarget::Method(r.method_id), EdgeProvenance::Resolved, edge.kind)
                    }
                    Ok(_) => (
                        ResolvedTarget::Unresolved(edge.target.clone()),
                        EdgeProvenance::VisibilityBlocked,
                        edge.kind,
                    ),
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
            } => match resolver.resolve_manager_method(db, *manager_type, object_name, method_name)
            {
                // A user manager-module method on a fully-literal `Коллекция.Объект.Метод()`
                // path: the object name is a token and its manager module is uniquely
                // determined, so locating the exported method is a direct lookup — as
                // trustworthy as a qualified `Модуль.Метод()` call. The edge is about the method.
                Ok(r) if r.is_export => {
                    (ResolvedTarget::Method(r.method_id), EdgeProvenance::Resolved, edge.kind)
                }
                Ok(_) => (
                    ResolvedTarget::Unresolved(edge.target.clone()),
                    EdgeProvenance::VisibilityBlocked,
                    edge.kind,
                ),
                // A platform manager method (create/find/…): the edge is about
                // the metadata object it touches, not a user node.
                Err(_) => (
                    ResolvedTarget::Mdo {
                        mdo_type: manager_type.to_mdo_type(),
                        object_name: object_name.clone(),
                    },
                    EdgeProvenance::Inferred,
                    manager_edge_kind(*manager_type, method_name.as_str()),
                ),
            },
            // A bare `Справочники.X` reference (no method) touches the object.
            CallTarget::ManagerAccess { manager_type, object_name, method_name: None } => (
                ResolvedTarget::Mdo {
                    mdo_type: manager_type.to_mdo_type(),
                    object_name: object_name.clone(),
                },
                EdgeProvenance::Inferred,
                EdgeKind::ManagerAccess,
            ),
            // A `Движения.<Регистр>` movement touch: resolve the register name to its
            // metadata type from config. The edge is about the register object it touches.
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
            // A `ЭтотОбъект` call that reached here is a platform object method
            // (local user methods were already resolved at extraction time).
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

    let find_local = |name: &crate::name::Name| resolver.resolve_module_method(db, name);
    let find_qualified =
        |module_name: &crate::name::Name, method_name: &crate::name::Name| match resolver
            .resolve_qualified_method(db, module_name, method_name)
        {
            Ok(r) if r.is_export => QualifiedLookup::Resolved(r.method_id),
            Ok(_) => QualifiedLookup::VisibilityBlocked,
            Err(_) => QualifiedLookup::Absent,
        };
    let global_modules = resolver.global_common_module_names(db);
    edges.extend(resolve_callback_edges(&summary, find_local, find_qualified, &global_modules));

    Arc::new(crate::call_graph::ResolvedModuleSummary { module: module_id, edges })
}

/// Outcome of resolving a callback handler named by a string literal to a method in a
/// (common) module: exported method found, found-but-not-exported, or absent.
pub(crate) enum QualifiedLookup {
    Resolved(crate::MethodId),
    VisibilityBlocked,
    Absent,
}

/// Project a module's string-dispatched callback registrations (`Новый
/// ОписаниеОповещения`, `ПодключитьОбработчикОжидания`) into resolved graph edges.
///
/// The two callers (the Salsa fold and the resident-index build) resolve methods by
/// different mechanisms but must produce byte-identical edges, so the lookup strategy
/// is injected: `find_local` resolves a handler in the current module, `find_qualified`
/// resolves a handler in a named common module. `global_module_names` lists the
/// configuration's global common modules (shared by both callers), used to resolve a
/// global-context idle handler that names no module. A handler that does not resolve
/// yields no edge — `Unresolved` targets are dropped in projection anyway, and inventing
/// edges for unknown receivers is worse than omitting them.
pub(crate) fn resolve_callback_edges(
    summary: &crate::call_graph::ModuleCallSummary,
    find_local: impl Fn(&crate::name::Name) -> Option<crate::MethodId>,
    find_qualified: impl Fn(&crate::name::Name, &crate::name::Name) -> QualifiedLookup,
    global_module_names: &[crate::name::Name],
) -> Vec<crate::call_graph::ResolvedCallEdge> {
    use crate::call_graph::{
        CallTarget, EdgeKind, EdgeProvenance, NotifyTarget, ResolvedCallEdge, ResolvedTarget,
    };

    let mut out = Vec::new();

    for reg in &summary.notify_regs {
        let resolved = match &reg.target {
            NotifyTarget::ThisObject => find_local(&reg.callback_name)
                .map(|m| (ResolvedTarget::Method(m), EdgeProvenance::StringResolved)),
            NotifyTarget::Module(module_name) => {
                match find_qualified(module_name, &reg.callback_name) {
                    QualifiedLookup::Resolved(m) => {
                        Some((ResolvedTarget::Method(m), EdgeProvenance::StringResolved))
                    }
                    // Surfaced as a visible-but-unreachable gap, mirroring qualified
                    // calls; the projection drops it, so no workspace edge appears.
                    QualifiedLookup::VisibilityBlocked => Some((
                        ResolvedTarget::Unresolved(CallTarget::QualifiedModule {
                            module_name: module_name.clone(),
                            method_name: reg.callback_name.clone(),
                        }),
                        EdgeProvenance::VisibilityBlocked,
                    )),
                    QualifiedLookup::Absent => None,
                }
            }
            NotifyTarget::Unsupported => None,
        };
        if let Some((target, provenance)) = resolved {
            out.push(ResolvedCallEdge {
                caller: reg.caller,
                target,
                kind: EdgeKind::NotifyRef,
                range: reg.range,
                provenance,
            });
        }
    }

    for reg in &summary.idle_handler_regs {
        // `ПодключитьОбработчикОжидания` names an exported procedure of the current module
        // (a form/object module) OR — in the global context — of a global common module.
        // The current module wins; otherwise resolve across the global common modules and
        // accept only a UNIQUE match, since the name carries no module qualifier.
        let target = find_local(&reg.handler_name).or_else(|| {
            unique_global_handler(global_module_names, &find_qualified, &reg.handler_name)
        });
        if let Some(m) = target {
            out.push(ResolvedCallEdge {
                caller: reg.caller,
                target: ResolvedTarget::Method(m),
                kind: EdgeKind::IdleHandler,
                range: reg.range,
                provenance: EdgeProvenance::StringResolved,
            });
        }
    }

    out
}

/// Resolve an unqualified idle-handler name across the configuration's global common
/// modules, returning the method only when EXACTLY ONE global module exports it. The name
/// has no module qualifier, so an ambiguous match (two distinct exporting modules) is left
/// unresolved rather than guessed — consistent with the "don't invent edges" rule.
fn unique_global_handler(
    global_module_names: &[crate::name::Name],
    find_qualified: &impl Fn(&crate::name::Name, &crate::name::Name) -> QualifiedLookup,
    handler: &crate::name::Name,
) -> Option<crate::MethodId> {
    let mut found: Option<crate::MethodId> = None;
    for module_name in global_module_names {
        if let QualifiedLookup::Resolved(m) = find_qualified(module_name, handler) {
            match found {
                Some(prev) if prev != m => return None,
                _ => found = Some(m),
            }
        }
    }
    found
}

/// Classify a platform manager method into a metadata-object edge kind. A register
/// record-set creator (`СоздатьНаборЗаписей`/`СоздатьМенеджерЗаписи` or English
/// `CreateRecordSet`/`CreateRecordManager`) on a register manager produces a
/// `RegisterRecordSet` edge — register write-capable access, kept out of the generic
/// `ManagerCreates` bucket. Other creation methods (`СоздатьЭлемент`/`СоздатьГруппу`/…
/// or English `Create…`) produce a `ManagerCreates` edge; everything else (find/select/…)
/// a `ManagerAccess` edge. Only platform methods reach here — user manager-module methods
/// resolve to a `Method` node earlier — so the name prefix is a reliable creation signal.
pub(crate) fn manager_edge_kind(
    manager_type: crate::body::ManagerType,
    method_name: &str,
) -> crate::call_graph::EdgeKind {
    use crate::call_graph::EdgeKind;
    let lower = method_name.fold_lower();
    if manager_type.is_register() && is_record_set_creator(&lower) {
        EdgeKind::RegisterRecordSet
    } else if lower.starts_with("создать") || lower.starts_with("create") {
        EdgeKind::ManagerCreates
    } else {
        EdgeKind::ManagerAccess
    }
}

/// A register manager method that builds the record-set write engine: `СоздатьНаборЗаписей`
/// (record set) or `СоздатьМенеджерЗаписи` (single-record manager), in either language.
fn is_record_set_creator(method_lower: &str) -> bool {
    matches!(
        method_lower,
        "создатьнаборзаписей" | "createrecordset" | "создатьменеджерзаписи" | "createrecordmanager"
    )
}

/// Canonical-spelling map for metadata objects. BSL identifiers are
/// case-insensitive, so different spellings of the same object across call sites
/// and query texts must collapse to a single `Mdo`/`Attribute` node. First-seen
/// spelling wins; shared between the call-edge and query-ref projections.
/// First-seen canonical object spelling per `(type, lowercased object)`, plus the
/// objects seen with more than one casing. The canonical spelling (first-seen in the
/// build's deterministic order) becomes part of the durable node id; a casing variant
/// (a later, case-insensitively-equal but exact-different spelling) is recorded so an
/// incremental rebuild can refuse the body-only fast path for that object — the fast
/// path cannot reproduce the cross-module first-seen ordering for inconsistently-cased
/// objects.
#[derive(Default)]
pub(crate) struct MdoCanonical {
    map: FxHashMap<(MdoType, String), crate::name::Name>,
    variants: FxHashSet<(MdoType, String)>,
}

impl MdoCanonical {
    /// The canonical spelling for `spelling`'s object: the first one seen wins; a
    /// later differently-cased spelling of the same object is recorded as a variant
    /// and the first-seen spelling is returned (matching the build's first-wins rule).
    pub(crate) fn canonical(&mut self, mdo_type: MdoType, spelling: &str) -> crate::name::Name {
        let key = (mdo_type, spelling.fold_lower());
        match self.map.get(&key) {
            Some(existing) => {
                if existing.as_str() != spelling {
                    self.variants.insert(key);
                }
                existing.clone()
            }
            None => {
                let name = crate::name::Name::new(spelling);
                self.map.insert(key, name.clone());
                name
            }
        }
    }

    /// Objects (`type`, lowercased object) seen with more than one casing.
    pub(crate) fn casing_variants(&self) -> impl Iterator<Item = &(MdoType, String)> + '_ {
        self.variants.iter()
    }
}

/// Project a module's resolved call/manager edges (`summary`) into workspace
/// graph edges. `dispatch` supplies per-node client/server capability — including
/// callees in other modules — so the client→server boundary flag can be set
/// without the whole graph being materialised. `mdo_canonical` is updated as new
/// metadata-object spellings are seen and is shared with
/// [`project_module_query_edges`].
///
/// Takes the summary by reference rather than fetching it, so the same projection
/// serves both the Salsa fold (`db.resolved_module_summary`) and the graph-index
/// build (a summary resolved against the resident `GraphIndex`).
pub(crate) fn project_module_call_edges(
    summary: &crate::call_graph::ResolvedModuleSummary,
    dispatch: &dyn Fn(&GraphNode) -> Option<MethodDispatch>,
    mdo_canonical: &mut MdoCanonical,
) -> Vec<WorkspaceCallEdge> {
    use crate::call_graph::{CallerId, ResolvedTarget};

    let module = summary.module;
    let mut edges = Vec::with_capacity(summary.edges.len());
    for edge in &summary.edges {
        let to = match &edge.target {
            ResolvedTarget::Method(method_id) => GraphNode::Method(*method_id),
            ResolvedTarget::Mdo { mdo_type, object_name } => {
                let canon = mdo_canonical.canonical(*mdo_type, object_name.as_str());
                GraphNode::Mdo { mdo_type: *mdo_type, object_name: canon }
            }
            ResolvedTarget::Unresolved(_) => continue,
        };
        let from = match edge.caller {
            CallerId::Method(local_id) => GraphNode::Method(crate::MethodId { module, local_id }),
            CallerId::ModuleCode => GraphNode::ModuleCode(module),
        };
        // Mdo nodes have no dispatch, so the boundary flag falls out `false`.
        let crosses_client_to_server = dispatch(&from).is_some_and(|d| d.can_run_on_client)
            && dispatch(&to).is_some_and(|d| d.is_server_only());
        // An empty span is what `extract_from_body` writes when the source map had no range
        // for the call expression; a real call expression is never empty. Reading the state
        // off the span itself keeps the classification out of reach of `EdgeKind`, which is
        // reassigned below this point for manager, notify and idle edges.
        let call_site = if edge.range.is_empty() {
            crate::call_graph::CallSite::NotRecorded
        } else {
            crate::call_graph::CallSite::Recorded(edge.range)
        };
        edges.push(WorkspaceCallEdge {
            from,
            to,
            kind: edge.kind,
            provenance: edge.provenance,
            call_site,
            crosses_client_to_server,
        });
    }
    edges
}

/// Project one module's SDBL query references into `query_ref` graph edges: a
/// method (or module body) that runs a query reading a metadata object links to
/// that object's `Mdo` node (coarse) and to each read attribute's `Attribute`
/// node (precise). `mdo_canonical` is shared with [`project_module_call_edges`]
/// so query- and call-derived `Mdo` nodes are the same node. The `seen_*` sets
/// dedup across the whole workspace ("this method reads Catalog X" once), so they
/// are threaded through every module's projection rather than reset per module.
pub(crate) fn project_module_query_edges(
    db: &dyn crate::configs::ConfigsDatabase,
    module: ModuleId,
    mdo_canonical: &mut MdoCanonical,
    seen_query_ref: &mut FxHashSet<(GraphNode, MdoType, String)>,
    seen_query_attr: &mut FxHashSet<(GraphNode, MdoType, String, String)>,
) -> Vec<WorkspaceCallEdge> {
    let refs = collect_module_query_refs(db, module);
    project_collected_query_edges(&refs, mdo_canonical, seen_query_ref, seen_query_attr)
}

/// One query-reading site's metadata reads, lifted out of the SDBL HIR without
/// touching any cross-module canonicalization or dedup state. This is the
/// parallel-safe half of query-edge projection: it only reads the database, so a
/// streaming build can collect it for many modules concurrently, then feed the
/// results to [`project_collected_query_edges`] sequentially to assign canonical
/// `Mdo`/`Attribute` spellings in a deterministic order.
pub(crate) struct ModuleQueryRefs {
    sites: Vec<QueryRefSite>,
}

struct QueryRefSite {
    from: GraphNode,
    /// Coarse reads: object X is read (survives unresolved columns, e.g. `ВЫБРАТЬ *`).
    tables: Vec<(MdoType, String)>,
    /// Precise reads: object X's attribute Y is read.
    attrs: Vec<(MdoType, String, String)>,
}

/// Collect a module's query reads from its SDBL HIR. Database reads only — no
/// shared graph-build state — so it is safe to run for many modules in parallel.
pub(crate) fn collect_module_query_refs(
    db: &dyn crate::configs::ConfigsDatabase,
    module: ModuleId,
) -> ModuleQueryRefs {
    let file_id_input = base_db::FileIdInput::new(db, module.file_id);
    let sdbl_entries = crate::sdbl_cache::sdbl_hir_for_file_query(db, file_id_input);
    let mut sites = Vec::new();
    for (sdbl_expr_id, package) in sdbl_entries.iter() {
        let from = match sdbl_expr_id.owner {
            crate::DefWithBodyId::Method(local_id) => {
                GraphNode::Method(crate::MethodId { module, local_id })
            }
            crate::DefWithBodyId::ModuleCode => GraphNode::ModuleCode(module),
        };
        let mut resolved = Vec::new();
        let mut attrs = Vec::new();
        for query in package.queries() {
            query.hir.collect_resolved_tables(&mut resolved);
            query.hir.collect_resolved_attributes(&mut attrs);
        }
        let tables = resolved
            .into_iter()
            .filter_map(|table| match table {
                sdbl_hir::ResolvedTable::Metadata { mdo_type, name, .. }
                | sdbl_hir::ResolvedTable::Register { mdo_type, name, .. } => {
                    Some((*mdo_type, name.clone()))
                }
                sdbl_hir::ResolvedTable::TempTable { .. } => None,
            })
            .collect();
        sites.push(QueryRefSite { from, tables, attrs });
    }
    ModuleQueryRefs { sites }
}

/// Project collected query reads ([`collect_module_query_refs`]) into `query_ref`
/// graph edges, assigning canonical `Mdo`/`Attribute` spellings and deduping across
/// the workspace. This is the order-sensitive, sequential half: `mdo_canonical` and
/// the `seen_*` sets are shared across modules and the first-seen spelling wins, so
/// it must run in a deterministic module order to match the fold byte-for-byte.
pub(crate) fn project_collected_query_edges(
    refs: &ModuleQueryRefs,
    mdo_canonical: &mut MdoCanonical,
    seen_query_ref: &mut FxHashSet<(GraphNode, MdoType, String)>,
    seen_query_attr: &mut FxHashSet<(GraphNode, MdoType, String, String)>,
) -> Vec<WorkspaceCallEdge> {
    use crate::call_graph::{EdgeKind, EdgeProvenance};

    let mut edges = Vec::new();
    for site in &refs.sites {
        let from = &site.from;
        for (mdo_type, name) in &site.tables {
            let name_lower = name.fold_lower();
            if !seen_query_ref.insert((from.clone(), *mdo_type, name_lower)) {
                continue;
            }
            let canon = mdo_canonical.canonical(*mdo_type, name);
            edges.push(WorkspaceCallEdge {
                from: from.clone(),
                to: GraphNode::Mdo { mdo_type: *mdo_type, object_name: canon },
                kind: EdgeKind::QueryRef,
                provenance: EdgeProvenance::Inferred,
                // The read is written in a query, and this pass keeps no span for it: the
                // SDBL facts it folds carry none, and one edge stands for every read of the
                // object in the module.
                call_site: crate::call_graph::CallSite::NotRecorded,
                crosses_client_to_server: false,
            });
        }
        for (mdo_type, object, attr) in &site.attrs {
            if !seen_query_attr.insert((
                from.clone(),
                *mdo_type,
                object.fold_lower(),
                attr.fold_lower(),
            )) {
                continue;
            }
            let canon = mdo_canonical.canonical(*mdo_type, object);
            edges.push(WorkspaceCallEdge {
                from: from.clone(),
                to: GraphNode::Attribute {
                    mdo_type: *mdo_type,
                    object_name: canon,
                    attr_name: crate::name::Name::new(attr),
                },
                kind: EdgeKind::QueryRef,
                provenance: EdgeProvenance::Inferred,
                call_site: crate::call_graph::CallSite::NotRecorded,
                crosses_client_to_server: false,
            });
        }
    }
    edges
}

/// A metadata object a method touches through a manager (creation / find /
/// bare reference), as resolved by the call graph. `creates` marks a
/// `СоздатьЭлемент`-style call apart from plain access.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagerRef {
    pub mdo_type: MdoType,
    pub object_name: String,
    pub creates: bool,
}

/// A method's OUTBOUND graph facts — everything derivable from the method's own
/// module summaries, with no whole-config fold. This is the per-method projection
/// the call graph would emit for one node: what it calls, what it touches through a
/// manager, and what metadata it reads via SDBL queries. Because every fact comes
/// from `resolved_module_summary` + the module's query refs, the result invalidates
/// exactly when those do (the method's own body and its callees' export tables),
/// not when the rest of the workspace changes — the property that makes it safe to
/// fold into an embedding cache key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MethodOutboundFacts {
    /// Client/server dispatch, by the same rule the fold seeds (module execution
    /// context wins, else the method's own annotation).
    pub dispatch: MethodDispatch,
    /// Resolved user-method calls.
    pub callees: Vec<crate::MethodId>,
    /// Metadata objects touched through a manager.
    pub manager_refs: Vec<ManagerRef>,
    /// Metadata objects read by an SDBL query (coarse — survives `ВЫБРАТЬ *`).
    pub query_reads: Vec<(MdoType, String)>,
    /// Metadata attributes read by an SDBL query (precise).
    pub query_attr_reads: Vec<(MdoType, String, String)>,
}

/// Project one method's outbound graph facts for embedding enrichment. Pure per
/// the method's module summaries; performs no whole-config fold and no inbound
/// (caller) lookup. See [`MethodOutboundFacts`].
pub fn method_outbound_facts(
    db: &dyn crate::configs::ConfigsDatabase,
    method: crate::MethodId,
) -> MethodOutboundFacts {
    use crate::call_graph::{CallerId, EdgeKind, ResolvedTarget};

    let module = method.module;
    let local_id = method.local_id;

    // Dispatch — the fold's Pass-1 rule: module execution context wins, else the
    // method's own annotation (`graph_index::extract_module_data`/`insert_module_data`).
    let item_tree = db.item_tree(module.file_id);
    let entries = crate::call_graph::extract_graph_methods(&item_tree);
    let module_dispatch = db
        .module_metadata(module)
        .execution_context
        .and_then(MethodDispatch::from_execution_context);
    let dispatch = entries
        .iter()
        .find(|e| e.local_id == local_id)
        .map(|e| module_dispatch.unwrap_or(e.dispatch))
        .or(module_dispatch)
        .unwrap_or(MethodDispatch {
            can_run_on_client: true,
            can_run_on_server: true,
            no_context: false,
        });

    // Calls + manager refs — from the resolved module summary, filtered to this method.
    let file_id_input = FileIdInput::new(db, module.file_id);
    let summary = resolved_module_summary_query(db, file_id_input);
    let mut callees = Vec::new();
    let mut manager_refs = Vec::new();
    for edge in &summary.edges {
        if edge.caller != CallerId::Method(local_id) {
            continue;
        }
        match &edge.target {
            ResolvedTarget::Method(callee) => callees.push(*callee),
            ResolvedTarget::Mdo { mdo_type, object_name } => manager_refs.push(ManagerRef {
                mdo_type: *mdo_type,
                object_name: object_name.as_str().to_string(),
                creates: edge.kind == EdgeKind::ManagerCreates,
            }),
            ResolvedTarget::Unresolved(_) => {}
        }
    }

    // SDBL reads — from the per-module query refs, the site owned by this method.
    let refs = collect_module_query_refs(db, module);
    let mut query_reads = Vec::new();
    let mut query_attr_reads = Vec::new();
    let owner = GraphNode::Method(method);
    for site in refs.sites.iter().filter(|s| s.from == owner) {
        for (ty, name) in &site.tables {
            query_reads.push((*ty, name.clone()));
        }
        for (ty, object, attr) in &site.attrs {
            query_attr_reads.push((*ty, object.clone(), attr.clone()));
        }
    }

    // Deterministic order + dedup — the embedding cache key must be run-stable.
    callees.sort_by_key(|m| (m.module.file_id, m.local_id));
    callees.dedup();
    manager_refs.sort_by(|a, b| {
        (a.mdo_type.english_name(), &a.object_name, a.creates).cmp(&(
            b.mdo_type.english_name(),
            &b.object_name,
            b.creates,
        ))
    });
    manager_refs.dedup();
    query_reads.sort_by(|a, b| (a.0.english_name(), &a.1).cmp(&(b.0.english_name(), &b.1)));
    query_reads.dedup();
    query_attr_reads
        .sort_by(|a, b| (a.0.english_name(), &a.1, &a.2).cmp(&(b.0.english_name(), &b.1, &b.2)));
    query_attr_reads.dedup();

    MethodOutboundFacts { dispatch, callees, manager_refs, query_reads, query_attr_reads }
}

#[salsa::tracked(
    lru = 16,
    heap_size = crate::call_graph::workspace_call_graph_heap,
    returns(clone)
)]
pub fn workspace_call_graph_query(
    db: &dyn crate::configs::ConfigsDatabase,
    source_root_input: base_db::SourceRootInput,
) -> Arc<crate::call_graph::WorkspaceCallGraph> {
    use crate::call_graph::WorkspaceCallGraph;

    let source_root = source_root_input.root(db);
    let file_set = source_root.file_set();
    let modules: Vec<ModuleId> = source_root
        .iter()
        .filter(|&file_id| crate::workspace::is_bsl_source(file_set, file_id))
        .map(ModuleId::new)
        .collect();
    let _span = tracing::info_span!("workspace_call_graph", module_count = modules.len()).entered();

    let mut graph = WorkspaceCallGraph::default();
    let mut mdo_canonical = MdoCanonical::default();

    // Pass 1: per-method client/server dispatch, needed before edges so the
    // boundary flag can consult a callee that lives in another module. Common
    // modules dispatch at the module level (execution context); method-level
    // `&НаКлиенте`/`&НаСервере` annotations only apply where the module context
    // is unknown (form/command modules).
    for &module in &modules {
        let summary = db.module_call_summary(module);
        let module_dispatch = db
            .module_metadata(module)
            .execution_context
            .and_then(MethodDispatch::from_execution_context);
        for method in &summary.methods {
            graph.set_dispatch(
                GraphNode::Method(crate::MethodId { module, local_id: method.local_id }),
                module_dispatch.unwrap_or(method.dispatch),
            );
        }
    }

    // Pass 2: resolved call/manager edges, projected per module.
    for &module in &modules {
        let summary = db.resolved_module_summary(module);
        let edges = {
            let dispatch = |node: &GraphNode| graph.dispatch(node);
            project_module_call_edges(&summary, &dispatch, &mut mdo_canonical)
        };
        for edge in edges {
            graph.insert(edge);
        }
    }

    // Pass 3: SDBL query_ref edges, projected per module. Built after the call
    // edges so it shares the populated `mdo_canonical`; the `seen_*` sets dedup
    // across the whole workspace.
    let mut seen_query_ref: FxHashSet<(GraphNode, MdoType, String)> = FxHashSet::default();
    let mut seen_query_attr: FxHashSet<(GraphNode, MdoType, String, String)> = FxHashSet::default();
    for &module in &modules {
        let edges = project_module_query_edges(
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

    Arc::new(graph)
}

#[salsa::tracked(lru = 512, heap_size = heap_estimate::file_external_refs_heap, returns(clone))]
pub fn file_external_refs_query<'db>(
    db: &'db dyn DefDatabase,
    file_id_input: FileIdInput<'db>,
) -> Arc<Vec<ExternalRef>> {
    let _span = tracing::info_span!("file_external_refs", ?file_id_input).entered();
    let file_id = file_id_input.file_id(db);
    let module_id = ModuleId::new(file_id);

    tracing::debug!(file_id = file_id.0, "file_external_refs: calling module_bodies");
    let bodies = db.module_bodies(module_id);

    let method_count = bodies.iter_lower_results().count();
    tracing::debug!(file_id = file_id.0, method_count, "file_external_refs: got module_bodies");

    let mut refs = Vec::new();
    for (method_id, lower_result) in bodies.iter_lower_results() {
        let ref_count = lower_result.result.external_refs.len();
        if ref_count > 0 {
            tracing::debug!(
                file_id = file_id.0,
                method_id = ?method_id,
                ref_count,
                "file_external_refs: found refs in method"
            );
        }
        refs.extend(lower_result.external_refs());
    }

    if let Some(module_code) = bodies.module_code_result() {
        let ref_count = module_code.result.external_refs.len();
        if ref_count > 0 {
            tracing::debug!(
                file_id = file_id.0,
                ref_count,
                "file_external_refs: found refs in module code"
            );
        }
        refs.extend(module_code.external_refs());
    }

    tracing::debug!(file_id = file_id.0, total_refs = refs.len(), "file_external_refs: done");
    Arc::new(refs)
}

#[salsa::tracked(lru = 16, heap_size = crate::module_index::module_index_heap, returns(clone))]
pub fn module_index_query(
    _db: &dyn DefDatabase,
    source_root_input: base_db::SourceRootInput,
) -> Arc<ModuleIndex> {
    let source_root = source_root_input.root(_db);
    let _span =
        tracing::info_span!("module_index", file_count = source_root.iter().count()).entered();

    let file_set = source_root.file_set();
    let paths: Vec<(FileId, String)> = source_root
        .iter()
        .filter_map(|file_id| {
            let vfs_path = file_set.path_for_file(&file_id)?;
            let path = vfs_path.as_path();
            let path_str = path.to_str()?;
            Some((file_id, path_str.to_string()))
        })
        .collect();

    let index = ModuleIndex::build_from_paths(paths.iter().map(|(id, p)| (*id, p.as_str())));

    Arc::new(index)
}

#[salsa::tracked(lru = 512, heap_size = heap_estimate::file_dependencies_heap, returns(clone))]
pub fn file_dependencies_query<'db>(
    db: &'db dyn DefDatabase,
    file_id_input: FileIdInput<'db>,
) -> Arc<Vec<FileId>> {
    let _span = tracing::info_span!("file_dependencies", ?file_id_input).entered();
    let file_id = file_id_input.file_id(db);

    let source_root_id = db.file_source_root_input(file_id).source_root_id(db);
    let source_root_input = db.source_root_input(source_root_id);

    let index = module_index_query(db, source_root_input);
    tracing::debug!(
        file_id = file_id.0,
        common_modules = index.common_module_count(),
        managers = index.manager_count(),
        "file_dependencies: got module_index"
    );

    let file_id_input = base_db::FileIdInput::new(db, file_id);
    let refs = file_external_refs_query(db, file_id_input);
    tracing::debug!(
        file_id = file_id.0,
        refs_count = refs.len(),
        "file_dependencies: got external_refs"
    );

    let mut deps: Vec<FileId> = refs.iter().filter_map(|r| index.resolve(r)).collect();
    tracing::debug!(
        file_id = file_id.0,
        resolved = deps.len(),
        unresolved = refs.len() - deps.len(),
        "file_dependencies: resolved refs"
    );

    deps.sort_by_key(|f| f.index());
    deps.dedup();

    Arc::new(deps)
}

#[cfg(test)]
mod tests {
    use super::{unique_global_handler, QualifiedLookup};
    use crate::name::Name;
    use crate::{MethodId, ModuleId};
    use vfs::FileId;

    /// The unqualified idle-handler name has no module, so a name exported by two distinct
    /// global common modules is ambiguous and must NOT be guessed; a single exporter resolves.
    #[test]
    fn unique_global_handler_resolves_only_an_unambiguous_match() {
        let names = [Name::new("МодульА"), Name::new("МодульБ")];
        let m_a =
            MethodId { module: ModuleId::new(FileId(1)), local_id: crate::MethodKey::first("М0") };
        let m_b =
            MethodId { module: ModuleId::new(FileId(2)), local_id: crate::MethodKey::first("М0") };

        let both = |module: &Name, _h: &Name| {
            if module.as_str() == "МодульА" {
                QualifiedLookup::Resolved(m_a)
            } else {
                QualifiedLookup::Resolved(m_b)
            }
        };
        assert_eq!(
            unique_global_handler(&names, &both, &Name::new("Обработчик")),
            None,
            "two distinct global exporters → ambiguous → no edge"
        );

        let one = |module: &Name, _h: &Name| {
            if module.as_str() == "МодульА" {
                QualifiedLookup::Resolved(m_a)
            } else {
                QualifiedLookup::Absent
            }
        };
        assert_eq!(
            unique_global_handler(&names, &one, &Name::new("Обработчик")),
            Some(m_a),
            "a single global exporter resolves"
        );

        // Non-exported (visibility-blocked) candidates are not matches.
        let blocked = |_m: &Name, _h: &Name| QualifiedLookup::VisibilityBlocked;
        assert_eq!(unique_global_handler(&names, &blocked, &Name::new("Обработчик")), None);

        // The SAME method seen via two module names (e.g. an extension overlay listing the
        // module twice) is not ambiguous — dedup by MethodId keeps it resolved.
        let same = |_m: &Name, _h: &Name| QualifiedLookup::Resolved(m_a);
        assert_eq!(
            unique_global_handler(&names, &same, &Name::new("Обработчик")),
            Some(m_a),
            "the same method via two names is one method, not an ambiguous pair"
        );
    }
}
