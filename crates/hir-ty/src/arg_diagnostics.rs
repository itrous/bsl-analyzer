use std::sync::Arc;
use std::time::{Duration, Instant};

use base_db::FileIdInput;
use hir_def::body::Body;
use hir_def::hir::Expr;
use hir_def::{DefWithBodyId, ExprId, IdConversion, MethodIdInput, ModuleId};
use vfs::FileId;

use bsl_types::builders::Builders;
use bsl_types::intern::TypeKernelDb;
use bsl_types::kind::TypeId;

use crate::call_resolution::{resolve_candidates, ArgumentParameter, CallRejection, CallSelection};
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
    expr_types: &rustc_hash::FxHashMap<ExprId, TypeId>,
) -> Vec<InferenceDiagnostic> {
    if bindings.is_empty() {
        return Vec::new();
    }
    let narrowing_enabled = db.type_narrowing_enabled();
    let start = Instant::now();

    // Narrowing is asked for once per body, and only once a call passes a
    // bare name as an argument — the only shape narrowing can refine.
    let mut narrow: Option<Option<Arc<dataflow::DataflowResult<NarrowState>>>> = None;
    let mut out = Vec::new();
    for binding in bindings {
        // A branch no environment of this body compiles never reaches the
        // platform's argument check, so neither arity nor types are judged
        // there — `#Если Сервер И Не Сервер Тогда` typing hints are the common shape.
        if binding.in_uncompiled_branch {
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

        if let Some(diagnostic) = diagnostic_for_binding(db, binding, &post_types) {
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

fn diagnostic_for_binding(
    db: &dyn TypeKernelDb,
    binding: &CallArgBinding,
    post_types: &[TypeId],
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
