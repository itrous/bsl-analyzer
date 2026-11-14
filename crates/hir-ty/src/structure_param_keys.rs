//! Stage 2 — interprocedural structure keys.
//!
//! A method that inserts keys into one of its by-reference parameters (directly, or by forwarding
//! that parameter to a further helper) is summarised here as a per-parameter key projection. At a
//! construction site, passing a tracked structure local *whole* to such a method folds the callee's
//! summary into the local, so keys added inside helpers / child methods surface at the call site.
//!
//! Light syntactic analysis (body walk + call resolution), NOT full inference, and recursion is
//! handled with an explicit visited set rather than a salsa fixpoint. That keeps
//! [`structure_param_keys_query`] acyclic in salsa, so [`crate::infer`] — itself a fixpoint query —
//! can read it without coupling two fixpoints (which fails to converge). Soft: completion/hover
//! only, never a diagnostic. Ordinary modules only (effective/weaving inference is out of scope).

use std::cell::{OnceCell, RefCell};
use std::sync::Arc;

use rustc_hash::{FxHashMap, FxHashSet};
use stdx::case::CaseExt;

use bsl_types::kind::Projection;
use hir_def::body::Body;
use hir_def::hir::{Expr, Stmt};
use hir_def::resolver::Resolver;
use hir_def::{MethodId, MethodIdInput, ModuleId};

use crate::db::HirDatabase;
use crate::structure_keys::{
    collect_structure_shapes, shape_to_projection, SeedRoots, StructureShape, ValueSource,
};

/// Bounds the interprocedural forwarding DFS (helper → child helper → …). Beyond it a deeper
/// helper's keys are not pulled in (the visited set already breaks recursion; this is a backstop).
const MAX_SUMMARY_DEPTH: usize = 16;

/// Per-parameter inserted-key projections for one method. An absent index = no keys inserted or
/// forwarded for that parameter.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StructureParamSummary {
    pub per_param: FxHashMap<u32, Arc<Projection>>,
}

/// Approximate live heap of a memoised summary, for salsa's `heap_size` hook (otherwise only
/// the `Arc` pointer is visible in the memory report): the boxed struct, the per-param table,
/// and each projection's field slice with its owned field names. A projection `Arc` shared by
/// several summaries is counted once per holder — over-approximate by design, consistent with
/// the [`crate::infer::heap_estimate`] estimators.
pub(crate) fn structure_param_summary_heap(v: &Arc<StructureParamSummary>) -> usize {
    use std::mem::size_of;

    let mut b = size_of::<StructureParamSummary>();
    b += crate::infer::heap_estimate::map_table_bytes::<u32, Arc<Projection>>(v.per_param.len());
    for projection in v.per_param.values() {
        b += size_of::<Projection>();
        b += projection.fields.len() * size_of::<bsl_types::kind::ProjectionField>();
        for field in projection.fields.iter() {
            b += field.name.capacity();
        }
        if let Some(raw) = &projection.raw_sdbl_types {
            b += raw.len() * size_of::<bsl_types::facet::SdblTypeShadowFacet>();
        }
    }
    b
}

/// Memoised per-method summary. Acyclic in salsa: it never calls itself as a query — transitive
/// forwarding is resolved by [`compute_summary`]'s manual visited-set recursion — so a caller that
/// is itself a fixpoint query (inference) reads a stable value.
#[salsa::tracked(lru = 262144, heap_size = structure_param_summary_heap, returns(ref))]
pub fn structure_param_keys_query<'db>(
    db: &'db dyn HirDatabase,
    method: MethodIdInput<'db>,
) -> Arc<StructureParamSummary> {
    let mid = method.method_id(db);
    let _span = tracing::info_span!(
        "structure_param_keys",
        file_id = mid.module.file_id.0,
        local_id = ?mid.local_id,
    )
    .entered();

    let visited = RefCell::new(FxHashSet::default());
    Arc::new(compute_summary(db, mid, &visited, 0))
}

/// Compute a method's per-parameter summary, recursing into forwarded callees with `visited` to
/// break (mutual) recursion. A method already on the path contributes nothing further — the union
/// of the keys reachable without re-entry, which is the fixpoint.
fn compute_summary(
    db: &dyn HirDatabase,
    mid: MethodId,
    visited: &RefCell<FxHashSet<MethodId>>,
    depth: usize,
) -> StructureParamSummary {
    if depth > MAX_SUMMARY_DEPTH || !visited.borrow_mut().insert(mid) {
        return StructureParamSummary::default();
    }

    let body = db.method_body_ref(MethodIdInput::new(db, mid));
    let Some(msym) = db.interface_method(mid) else {
        return StructureParamSummary::default();
    };

    // By-reference params are tracked roots; `Знач` params are a caller-invisible copy (skip).
    let byref: Vec<String> =
        msym.params.iter().filter(|p| !p.is_val).map(|p| p.name.as_str().fold_lower()).collect();
    if byref.is_empty() {
        return StructureParamSummary::default();
    }

    // Forwarded callees are summarised by recursing here (NOT via the salsa query), so the whole
    // transitive summary is one acyclic query. Params are live from method entry, so forwarding
    // interleaves correctly in the collector's source-ordered pass.
    let resolver = Resolver::with_builtins_and_workspace(mid.module);
    let summarize = |callee: MethodId| Arc::new(compute_summary(db, callee, visited, depth + 1));
    let body_env = crate::method_environment::effective_method_env(db, mid, &db.env_options());
    let forwarder = Forwarder::new(db, &resolver, mid.module, body, body_env, &summarize);
    let shapes = collect_structure_shapes(body, SeedRoots::ParamNames(&byref), Some(&forwarder));

    let mut per_param: FxHashMap<u32, Arc<Projection>> = FxHashMap::default();
    for (i, param) in msym.params.iter().enumerate() {
        if param.is_val {
            continue;
        }
        let name = param.name.as_str().fold_lower();
        if let Some(shape) = shapes.get(&name) {
            if let Some(projection) = shape_to_projection(db, shape, 0) {
                per_param.insert(i as u32, projection);
            }
        }
    }

    StructureParamSummary { per_param }
}

/// Resolves forwarded calls within one body and folds callee summaries into tracked structure
/// shapes. Built once per body; resolution caches (shadowing locals, global exports) are computed at
/// most once. Shared by the Stage-1 construction-site fold (in `infer`, summarising callees via the
/// memoised query) and the Stage-2 summary's own forwarding (recursing via `compute_summary`).
pub(crate) struct Forwarder<'a> {
    db: &'a dyn HirDatabase,
    resolver: &'a Resolver,
    module: ModuleId,
    body_env: hir_def::execution_env::EnvFlags,
    /// Names (lowercased) that are body bindings or assignment targets — a same-named method is
    /// shadowed and must not be resolved as a callee (mirrors the inference dispatch guard).
    shadowing_locals: FxHashSet<String>,
    /// Names (lowercased) that are DECLARED bindings: parameters, `Перем`, loop variables.
    /// The narrower question, and the right one for a metadata-collection root: assigning to
    /// such a name is a refused write to a Global-context property, not a declaration, so it
    /// must not turn a manager chain into something else.
    declared_bindings: FxHashSet<String>,
    /// Lazily-built lowercased method-name → `MethodId` of the visible global common modules.
    global_methods: OnceCell<FxHashMap<String, MethodId>>,
    /// Produces a callee's summary — the memoised query (construction site) or recursive
    /// `compute_summary` (within a summary).
    summarize: &'a dyn Fn(MethodId) -> Arc<StructureParamSummary>,
}

impl<'a> Forwarder<'a> {
    pub(crate) fn new(
        db: &'a dyn HirDatabase,
        resolver: &'a Resolver,
        module: ModuleId,
        body: &Body,
        body_env: hir_def::execution_env::EnvFlags,
        summarize: &'a dyn Fn(MethodId) -> Arc<StructureParamSummary>,
    ) -> Self {
        let mut declared_bindings = FxHashSet::default();
        for (_, binding) in body.bindings_iter() {
            declared_bindings.insert(binding.name.as_str().fold_lower());
        }
        let mut shadowing_locals = declared_bindings.clone();
        for (_, stmt) in body.stmts_iter() {
            if let Stmt::Assign { target, .. } = stmt {
                if let Expr::Path(name) = body.expr_idx(*target) {
                    shadowing_locals.insert(name.as_str().fold_lower());
                }
            }
        }
        Self {
            db,
            resolver,
            module,
            body_env,
            shadowing_locals,
            declared_bindings,
            global_methods: OnceCell::new(),
            summarize,
        }
    }

    /// Fold a single call's callee summary into the tracked roots it is passed (whole). A no-op
    /// unless `call` is a resolvable call passing at least one tracked, still-live structure root.
    pub(crate) fn fold_call(
        &self,
        body: &Body,
        shapes: &mut FxHashMap<String, StructureShape>,
        frozen: &FxHashSet<String>,
        call: &Expr,
    ) {
        let Expr::Call { callee, args } = call else { return };

        // Arguments that are tracked, still-live roots passed whole (`F(С)`, not `F(С.Поле)`).
        let tracked: Vec<(usize, String)> = args
            .iter()
            .enumerate()
            .filter_map(|(j, arg)| match body.expr_idx(*arg) {
                Expr::Path(name) => {
                    let key = name.as_str().fold_lower();
                    (shapes.contains_key(&key) && !frozen.contains(&key)).then_some((j, key))
                }
                _ => None,
            })
            .collect();
        if tracked.is_empty() {
            return;
        }

        let Some(callee_mid) = self.resolve_callee(body, body.expr_idx(*callee)) else { return };
        let summary = (self.summarize)(callee_mid);
        if summary.per_param.is_empty() {
            return;
        }

        let callee_decl = self.db.interface_method(callee_mid);
        let callee_params = callee_decl.as_ref().map(|m| &m.params);

        for (j, root) in tracked {
            // A `Знач` callee param receives a copy — inserted keys never reach the caller.
            if callee_params.is_some_and(|ps| ps.get(j).is_some_and(|p| p.is_val)) {
                continue;
            }
            let Some(projection) = summary.per_param.get(&(j as u32)) else { continue };
            let shape = shapes.get_mut(&root).expect("root is tracked");
            for field in projection.fields.iter() {
                shape.upsert(&field.name, ValueSource::Resolved(field.ty));
            }
        }
    }

    /// Разрешается ли вызов в метод конфигурации. Только это и отличает переопределённое
    /// платформенное имя от самой платформенной функции.
    pub(crate) fn callee_is_user_method(&self, body: &Body, callee: &Expr) -> bool {
        self.resolve_callee(body, callee).is_some()
    }

    /// Whether this syntactic call resolves to a user method whose corresponding parameter is
    /// explicitly passed by value. Only that case proves that the caller's structure cannot be
    /// mutated through the argument.
    ///
    /// Not answerable from `argument_passing_of`: this pass runs inside `infer_all` before
    /// `infer_stmts`, which is what builds `call_arg_bindings`, so no bindings exist yet.
    pub(crate) fn argument_is_by_value(&self, body: &Body, call: &Expr, arg_index: usize) -> bool {
        let Expr::Call { callee, .. } = call else { return false };
        let Some(mid) = self.resolve_callee(body, body.expr_idx(*callee)) else { return false };
        self.db
            .interface_method(mid)
            .is_some_and(|method| method.params.get(arg_index).is_some_and(|param| param.is_val))
    }

    /// Resolve a call's callee to a `MethodId` using only syntactic forms (no inferred receiver):
    /// same-module bare, global common-module bare, common-module `Модуль.F`, three-level
    /// `Справочники.X.F`. A bare name shadowed by a local/param is not a method call → `None`.
    /// Uses raw resolver lookups (method id only) — never the `resolve_*_call` wrappers, which
    /// materialise signatures via inference.
    fn resolve_callee(&self, body: &Body, callee: &Expr) -> Option<MethodId> {
        match callee {
            Expr::Path(name) => {
                let lower = name.as_str().fold_lower();
                if self.shadowing_locals.contains(&lower) {
                    return None;
                }
                if let Some(method) = self.db.interface_method_named(self.module, name) {
                    return Some(method.id);
                }
                self.global_methods().get(&lower).copied()
            }
            Expr::Field { base, field } => match body.expr_idx(*base) {
                Expr::Path(module_name) => self
                    .resolver
                    .resolve_qualified_method(self.db, module_name, field)
                    .ok()
                    .map(|r| r.method_id),
                // `Справочники.Товары.Метод(С)` — three-level, in the Expr language of the
                // source since the fold was removed. This pass deliberately stays on syntax
                // (materialising signatures through inference is what it must avoid), so the
                // root-ownership question is answered here with the one fact it has: a
                // DECLARED binding of that name means the chain is not a manager call.
                //
                // Assignment targets are deliberately not consulted: writing to a metadata
                // collection name declares nothing — the platform refuses the write — and
                // treating it as a declaration would make the same name a collection for
                // inference and a local for completion.
                //
                // The two conditions below are exactly the two of
                // `Infer::manager_collection_shadowed` — a body binding, or a claim on the
                // bare name from anywhere else (module variable, module method, object or
                // form member, common module). Asking the SAME predicate is the point: a
                // second copy of the verdict is what made the layers disagree, and the
                // predicate itself needs no inference, only the resolver.
                Expr::Field { base: root, field: mdo_name } => {
                    let Expr::Path(plural) = body.expr_idx(*root) else { return None };
                    if self.declared_bindings.contains(&plural.as_str().fold_lower()) {
                        return None;
                    }
                    if crate::platform_global_lookup::bare_global_name_claim(
                        self.db,
                        self.resolver,
                        None,
                        plural,
                        self.body_env,
                    )
                    .is_some()
                    {
                        return None;
                    }
                    self.resolver
                        .resolve_three_level_method(self.db, plural, mdo_name, field)
                        .ok()
                        .map(|r| r.method_id)
                }
                _ => None,
            },
            _ => None,
        }
    }

    fn global_methods(&self) -> &FxHashMap<String, MethodId> {
        self.global_methods.get_or_init(|| {
            let mut map = FxHashMap::default();
            for entry in self.resolver.global_common_module_exports(self.db).entries {
                if let hir_def::resolver::GlobalExportDefinition::Method(method_id) =
                    entry.definition
                {
                    if entry.capabilities.callable == Some(true) {
                        map.entry(entry.name.as_str().fold_lower()).or_insert(method_id);
                    }
                }
            }
            map
        })
    }
}
