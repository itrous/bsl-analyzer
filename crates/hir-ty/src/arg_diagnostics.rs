use std::sync::Arc;
use std::time::{Duration, Instant};

use base_db::FileIdInput;
use dataflow::reaching_defs::{DefSite, ReachingDefsResult};
use hir_def::body::Body;
use hir_def::hir::{Expr, Stmt};
use hir_def::{
    BindingId, DefWithBodyId, ExprId, IdConversion, MethodId, MethodIdInput, ModuleId, StmtId,
};
use rustc_hash::FxHashMap;
use stdx::case::CaseExt;
use vfs::FileId;

use bsl_types::builders::Builders;
use bsl_types::intern::TypeKernelDb;
use bsl_types::kind::TypeId;

use crate::call_resolution::{
    resolve_candidates, ArgumentApplicability, ArgumentParameter, CallRejection, CallResolution,
    CallSelection, CandidateDisposition, CandidateProvenance, CandidateRejection,
};
use crate::db::HirDatabase;
use crate::infer::{CallArgBinding, InferenceDiagnostic};
use crate::narrow::{narrowed_type_at, refine_by_ternary_guard, NarrowState};

#[cfg(test)]
mod tests;

/// Argument-shape diagnostics of one method: arity and type mismatches at
/// its call sites, judged after flow narrowing. Keyed by the method so a
/// body edit re-judges the edited body only; retained at the inference cap.
#[salsa::tracked(lru = 8192, heap_size = arg_diagnostics_heap, returns(clone))]
pub fn method_arg_diagnostics_query<'db>(
    db: &'db dyn HirDatabase,
    method: MethodIdInput<'db>,
) -> Arc<Vec<InferenceDiagnostic>> {
    let _span = tracing::info_span!("method_arg_diagnostics", ?method).entered();
    let method_id = method.method_id(db);
    let infer = db.infer_method_ref(method);
    let body = db.method_body_ref(method);
    Arc::new(body_arg_diagnostics(
        db,
        method_id.module.file_id,
        DefWithBodyId::Method(method_id.local_id),
        body,
        &infer.call_arg_bindings,
        &infer.expr_types,
        &infer.binding_types,
    ))
}

/// Sweep-mode retention for the per-method judgement, kept in step with the
/// rest of the per-method dataflow chain by `ide-db`.
pub fn set_arg_diagnostics_lru_capacity(db: &mut dyn crate::db::HirDatabase, cap: usize) {
    method_arg_diagnostics_query::set_lru_capacity(db, cap);
}

/// The same judgement for the module-level code.
#[salsa::tracked(lru = 128, heap_size = arg_diagnostics_heap, returns(clone))]
pub fn module_code_arg_diagnostics_query<'db>(
    db: &'db dyn HirDatabase,
    file_id_input: FileIdInput<'db>,
) -> Arc<Vec<InferenceDiagnostic>> {
    let file_id = file_id_input.file_id(db);
    let _span = tracing::info_span!("module_code_arg_diagnostics", ?file_id).entered();
    let infer = db.infer_module_code_ref(file_id);
    let module_bodies = db.module_bodies_ref(ModuleId { file_id });
    let Some(body) = module_bodies.module_code() else {
        return Arc::new(Vec::new());
    };
    Arc::new(body_arg_diagnostics(
        db,
        file_id,
        DefWithBodyId::ModuleCode,
        body,
        &infer.call_arg_bindings,
        &infer.expr_types,
        &infer.binding_types,
    ))
}

/// Every body's argument diagnostics of a file, paired with their owners —
/// the file view over the per-body memos, for readers that judge a whole
/// module at once.
pub fn file_arg_diagnostics(
    db: &dyn HirDatabase,
    file_id: FileId,
) -> Vec<(DefWithBodyId, InferenceDiagnostic)> {
    let module_id = ModuleId { file_id };
    let mut out = Vec::new();
    for decl in db.module_interface_ref(module_id).methods() {
        let owner = DefWithBodyId::Method(decl.id.local_id);
        let method = MethodIdInput::new(db, decl.id);
        out.extend(db.method_arg_diagnostics(method).iter().cloned().map(|d| (owner, d)));
    }
    out.extend(
        db.module_code_arg_diagnostics(file_id)
            .iter()
            .cloned()
            .map(|d| (DefWithBodyId::ModuleCode, d)),
    );
    out
}

fn arg_diagnostics_heap(v: &Arc<Vec<InferenceDiagnostic>>) -> usize {
    stdx::heap::vec_bytes::<InferenceDiagnostic>(v.len())
}

fn body_arg_diagnostics(
    db: &dyn HirDatabase,
    file_id: FileId,
    owner: DefWithBodyId,
    body: &Body,
    bindings: &[CallArgBinding],
    expr_types: &FxHashMap<ExprId, TypeId>,
    binding_types: &FxHashMap<BindingId, TypeId>,
) -> Vec<InferenceDiagnostic> {
    if bindings.is_empty() {
        return Vec::new();
    }
    let narrowing_enabled = db.type_narrowing_enabled();
    let start = Instant::now();

    // Narrowing is asked for once per body, and only once a call passes a
    // bare name as an argument — the only shape narrowing can refine.
    let mut narrow: Option<Option<Arc<dataflow::DataflowResult<NarrowState>>>> = None;
    let mut values = DocumentedValues::new(db, file_id, owner, body, bindings, binding_types);
    let mut out = Vec::new();
    for binding in bindings {
        // A branch no environment of this body compiles never reaches the
        // platform's argument check, so neither arity nor types are judged
        // there — `#Если Сервер И Не Сервер Тогда` typing hints are the common shape.
        if binding.in_dead_branch {
            continue;
        }
        if narrowing_enabled && narrow.is_none() {
            let any_path_arg =
                binding.args.iter().any(|arg_id| matches!(body.expr(*arg_id), Expr::Path(_)));
            if any_path_arg {
                narrow = Some(db.narrow(file_id, owner));
            }
        }
        let narrow_result = narrow.as_ref().and_then(|n| n.as_deref());

        let post_types: Vec<TypeId> = binding
            .args
            .iter()
            .map(|arg_id| {
                let base = expr_types.get(arg_id).copied().unwrap_or_else(|| db.unknown());
                narrow_arg(db, narrowing_enabled, narrow_result, body, *arg_id, base)
            })
            .collect();

        let mut documented = |index: usize| {
            binding.args.get(index).is_some_and(|arg| values.is_documented(*arg, binding.call_expr))
        };
        if let Some(diagnostic) = diagnostic_for_binding(db, binding, &post_types, &mut documented)
        {
            out.push(diagnostic);
        }
    }

    let elapsed = start.elapsed();
    if elapsed >= Duration::from_millis(100) {
        tracing::info!(
            ?owner,
            elapsed_ms = elapsed.as_millis() as u64,
            bindings = bindings.len(),
            diagnostics = out.len(),
            "Slow argument diagnostics of a body"
        );
    }
    out
}

fn narrow_arg(
    db: &dyn HirDatabase,
    narrowing_enabled: bool,
    narrow: Option<&dataflow::DataflowResult<NarrowState>>,
    body: &Body,
    expr_id: ExprId,
    base: TypeId,
) -> TypeId {
    if !narrowing_enabled {
        return base;
    }
    let Expr::Path(name) = body.expr(expr_id) else {
        return base;
    };
    let flow = narrow
        .and_then(|result| narrowed_type_at(db, result, body, expr_id.to_idx(), name))
        .unwrap_or(base);
    refine_by_ternary_guard(db, body, expr_id.to_idx(), name, flow)
}

/// Whether the value of an argument expression is one the documentation typed:
/// the return of a function documented with a type, a parameter the body was
/// given its documented type for, or a local every reaching assignment of which
/// is such a value. Answered lazily, and only for arguments a call was rejected on.
struct DocumentedValues<'a> {
    db: &'a dyn HirDatabase,
    file_id: FileId,
    owner: DefWithBodyId,
    body: &'a Body,
    bindings: &'a [CallArgBinding],
    binding_types: &'a FxHashMap<BindingId, TypeId>,
    reaching: Option<Option<Arc<ReachingDefsResult>>>,
    memo: FxHashMap<(ExprId, ExprId), bool>,
}

impl<'a> DocumentedValues<'a> {
    fn new(
        db: &'a dyn HirDatabase,
        file_id: FileId,
        owner: DefWithBodyId,
        body: &'a Body,
        bindings: &'a [CallArgBinding],
        binding_types: &'a FxHashMap<BindingId, TypeId>,
    ) -> Self {
        Self {
            db,
            file_id,
            owner,
            body,
            bindings,
            binding_types,
            reaching: None,
            memo: FxHashMap::default(),
        }
    }

    /// `call_expr` is the call `expr` is an argument of, as its binding names it.
    fn is_documented(&mut self, expr: ExprId, call_expr: ExprId) -> bool {
        if let Some(known) = self.memo.get(&(expr, call_expr)) {
            return *known;
        }
        // A value defined through itself (`Х = Х;` in a loop) proves nothing.
        self.memo.insert((expr, call_expr), false);
        let documented = match self.body.expr(expr) {
            // Inference binds a call under its callee expression, not the call itself.
            Expr::Call { callee, .. } => self.is_documented_call(ExprId::from_idx(*callee)),
            Expr::Path(name) => self.is_documented_name(expr, name.as_str(), call_expr),
            _ => false,
        };
        self.memo.insert((expr, call_expr), documented);
        documented
    }

    /// Every candidate of the call is a user function whose documentation gives
    /// the return a type: without one the signature takes the body's inferred
    /// return, which is computed.
    fn is_documented_call(&self, call_expr: ExprId) -> bool {
        let Some(binding) = self.bindings.iter().find(|binding| binding.call_expr == call_expr)
        else {
            return false;
        };
        let candidates = binding.candidate.candidates.as_slice();
        !candidates.is_empty()
            && candidates.iter().all(|candidate| match candidate.provenance {
                CandidateProvenance::UserMethod(id) => {
                    self.db.interface_method(MethodId::from(id)).is_some_and(|decl| {
                        decl.is_function
                            && crate::method_resolution::materialise_signature(self.db, &decl).ret
                                != self.db.unknown()
                    })
                }
                CandidateProvenance::PlatformMethod { .. }
                | CandidateProvenance::Builtin(_)
                | CandidateProvenance::FunctionValue => false,
            })
    }

    fn is_documented_name(&mut self, expr: ExprId, name: &str, call_expr: ExprId) -> bool {
        let Some(stmt) = self.body.enclosing_stmt(expr) else {
            return false;
        };
        // A write reaching definitions do not see leaves a value nothing documented.
        // shortcut: every call the local is passed to counts as a possible writer, platform
        // callables included (`ТипЗнч(Х)`, `Формат(Х, …)`), as inference counts them; telling
        // apart callees that cannot rebind an argument would relieve those values as well.
        let key = name.fold_lower();
        if crate::narrow::local_may_change_unseen(self.body, &key, stmt.to_idx(), Some(call_expr)) {
            return false;
        }
        let Some(defs) =
            self.reaching().and_then(|reaching| reaching.defs_for_var_at_stmt(name, stmt))
        else {
            return false;
        };
        !defs.is_empty()
            && defs.iter().all(|def| match def.def_site {
                DefSite::Parameter(binding) => self.binding_types.contains_key(&binding),
                DefSite::Assignment(stmt_raw) => match self.body.stmt(StmtId::from_raw(stmt_raw)) {
                    Stmt::Assign { value, .. } => {
                        self.is_documented(ExprId::from_idx(*value), call_expr)
                    }
                    _ => false,
                },
                DefSite::VarDecl(_)
                | DefSite::ForLoop(_)
                | DefSite::ForEachLoop(_)
                | DefSite::Unknown => false,
            })
    }

    fn reaching(&mut self) -> Option<Arc<ReachingDefsResult>> {
        self.reaching
            .get_or_insert_with(|| match self.owner {
                DefWithBodyId::Method(local_id) => {
                    let module = ModuleId::new(self.file_id);
                    self.db.method_reaching_definitions(MethodIdInput::new(
                        self.db,
                        MethodId { module, local_id },
                    ))
                }
                DefWithBodyId::ModuleCode => self.db.module_code_reaching_definitions(self.file_id),
            })
            .clone()
    }
}

/// A call rejected on types is still sound when a rejected candidate failed only
/// on documented union values each of which has a member the slot admits.
fn relieved_by_documentation(
    db: &dyn TypeKernelDb,
    resolution: &CallResolution,
    documented: &mut dyn FnMut(usize) -> bool,
) -> bool {
    resolution.candidates.iter().any(|fact| {
        let CandidateDisposition::Rejected(CandidateRejection::TypeIncompatible { arguments }) =
            &fact.disposition
        else {
            return false;
        };
        arguments
            .iter()
            .filter(|argument| argument.applicability == ArgumentApplicability::Incompatible)
            .all(|argument| match argument.parameter {
                ArgumentParameter::Declared { ty, .. } => {
                    crate::subtype::is_coercible_documented_union(db, argument.argument_ty, ty)
                        && documented(argument.index)
                }
                ArgumentParameter::MissingMetadata => false,
            })
    })
}

fn diagnostic_for_binding(
    db: &dyn TypeKernelDb,
    binding: &CallArgBinding,
    post_types: &[TypeId],
    documented: &mut dyn FnMut(usize) -> bool,
) -> Option<InferenceDiagnostic> {
    let resolution = resolve_candidates(db, &binding.candidate.candidates, post_types);
    match resolution.selection {
        CallSelection::Unique { .. } | CallSelection::Ambiguous { .. } => None,
        CallSelection::Rejected(CallRejection::NoCandidates) => None,
        CallSelection::Rejected(CallRejection::Arity { fallback }) => {
            let signature = binding
                .candidate
                .candidates
                .as_slice()
                .iter()
                .find(|candidate| candidate.id == fallback.candidate)?;
            Some(InferenceDiagnostic::MismatchedArgCount {
                call_expr: binding.call_expr,
                required_count: signature.required_args,
                total_count: signature.params.len(),
                found: post_types.len(),
            })
        }
        CallSelection::Rejected(CallRejection::Type) => {
            if relieved_by_documentation(db, &resolution, documented) {
                return None;
            }
            let fallback = resolution.type_fallback()?;
            let argument = fallback.argument;
            let ArgumentParameter::Declared { ty: expected, .. } = argument.parameter else {
                return None;
            };
            let signature = binding
                .candidate
                .candidates
                .as_slice()
                .iter()
                .find(|candidate| candidate.id == fallback.candidate)?;
            Some(InferenceDiagnostic::TypeMismatch {
                expr: *binding.args.get(argument.index)?,
                expected,
                actual: argument.argument_ty,
                from_doc_comment: signature.from_doc_comment,
            })
        }
    }
}
