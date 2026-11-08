use std::sync::Arc;

use bsl_types::builders::Builders;
use bsl_types::kind::TypeId;
use hir_def::{DefWithBodyId, MethodIdInput};

use crate::db::HirDatabase;
use crate::infer::{BodyInferenceResult, InferenceContext};

// A callee's inferred return type, projected from its single full body inference
// (`infer_method_query`) rather than a second, throwaway `infer_all`. Holding just
// a `TypeId` per entry keeps a high cap cheap, so the return type stays resident
// across the batch's chunk-boundary LRU trims even after the projected
// `infer_method` cell is evicted — a later chunk reads the cached type instead of
// re-inferring the whole body.
//
// Fixpoint recovery is retained because this query is the cycle head whenever a
// cross-module call enters a recursive return-type SCC first; the union below is
// the fixpoint that resolves mutually recursive return types.
#[salsa::tracked(
    lru = 262144,
    heap_size = stdx::heap::zero,
    cycle_fn = method_return_type_cycle,
    cycle_initial = method_return_type_initial,
    returns(copy),
)]
pub fn method_return_type_query<'db>(
    db: &'db dyn HirDatabase,
    method: MethodIdInput<'db>,
) -> TypeId {
    let mid = method.method_id(db);
    let _span = tracing::info_span!(
        "method_return_type",
        file_id = mid.module.file_id.0,
        local_id = ?mid.local_id,
    )
    .entered();

    let result = infer_method_query(db, method);

    let unknown = db.unknown();
    let return_tys: Vec<TypeId> = result
        .return_expr_ids
        .iter()
        .filter_map(|id| result.expr_types.get(id).copied())
        .filter(|tid| *tid != unknown)
        .collect();

    let ty = if return_tys.is_empty() { unknown } else { db.union(return_tys) };

    tracing::debug!(
        ?mid,
        ?ty,
        return_exprs = result.return_expr_ids.len(),
        "method_return_type complete",
    );
    ty
}

#[allow(
    clippy::needless_lifetimes,
    reason = "Salsa callback signature requires explicit lifetimes"
)]
pub fn method_return_type_initial<'db>(
    db: &'db dyn HirDatabase,
    _id: salsa::Id,
    _method: MethodIdInput<'db>,
) -> TypeId {
    db.unknown()
}

#[allow(
    clippy::needless_lifetimes,
    reason = "Salsa callback signature requires explicit lifetimes"
)]
pub fn method_return_type_cycle<'db>(
    db: &'db dyn HirDatabase,
    _cycle: &salsa::Cycle,
    last_provisional: &TypeId,
    value: TypeId,
    _method: MethodIdInput<'db>,
) -> TypeId {
    let unknown = db.unknown();
    match (*last_provisional, value) {
        (_, v) if v == unknown => *last_provisional,
        (l, _) if l == unknown => value,
        (l, v) if l == v => value,
        (l, v) => db.union(vec![l, v]),
    }
}

// Single source of truth for a method's body inference. `method_return_type_query`
// projects this instead of running its own `infer_all`, so an un-annotated callee
// that is both batch-inferred and called cross-module is inferred once, not twice.
//
// Fixpoint recovery is required because the projection closes a recursion edge:
// inferring a recursive method `A` resolves its self/mutual call through
// `materialise_signature_enriched -> method_return_type(A) -> infer_method(A)`. When
// the batch loop enters `infer_method(A)` first, `infer_method(A)` is the re-entered
// cycle head and must recover rather than panic. The `cycle_initial` sentinel is the
// empty body result; it is sound ONLY because a provisional `infer_method` value is
// consumed exclusively as a return-type projection (via `method_return_type`), never
// read directly for its `expr_types`/diagnostics. A future direct consumer of a
// provisional result would observe the empty sentinel and must not assume otherwise.
#[salsa::tracked(
    lru = 8192,
    heap_size = crate::infer::heap_estimate::body_inference_result_heap,
    cycle_fn = infer_method_cycle,
    cycle_initial = infer_method_initial,
    returns(ref),
)]
pub fn infer_method_query<'db>(
    db: &'db dyn HirDatabase,
    method: MethodIdInput<'db>,
) -> Arc<BodyInferenceResult> {
    let mid = method.method_id(db);
    let _span = tracing::info_span!(
        "infer_method",
        file_id = mid.module.file_id.0,
        local_id = ?mid.local_id,
    )
    .entered();

    let body = db.method_body_ref(method);
    let mut ctx = InferenceContext::new_for_method(db, method, body);
    ctx.infer_all();
    Arc::new(ctx.finish())
}

/// Switch `infer_method_query`'s LRU cap between the interactive profile and the small
/// sweep profile, in step with the per-method lowering chain it is inferred from
/// (`hir_def::set_lowering_lru_sweep_mode`): files a batch walks once need their
/// inference only while their chunk is analyzed. The interactive value must stay equal
/// to the `lru` literal on the query. Takes effect at the next LRU trim.
pub fn set_infer_method_lru_sweep_mode(db: &mut dyn HirDatabase, sweep: bool) {
    const METHOD_INTERACTIVE: usize = 8192;
    const METHOD_SWEEP: usize = 2048;
    infer_method_query::set_lru_capacity(db, if sweep { METHOD_SWEEP } else { METHOD_INTERACTIVE });
}

#[allow(
    clippy::needless_lifetimes,
    reason = "Salsa callback signature requires explicit lifetimes"
)]
pub fn infer_method_initial<'db>(
    db: &'db dyn HirDatabase,
    _id: salsa::Id,
    method: MethodIdInput<'db>,
) -> Arc<BodyInferenceResult> {
    let mid = method.method_id(db);
    Arc::new(BodyInferenceResult::empty_for(DefWithBodyId::Method(mid.local_id)))
}

#[allow(
    clippy::needless_lifetimes,
    reason = "Salsa callback signature requires explicit lifetimes"
)]
pub fn infer_method_cycle<'db>(
    _db: &'db dyn HirDatabase,
    _cycle: &salsa::Cycle,
    _last_provisional: &Arc<BodyInferenceResult>,
    value: Arc<BodyInferenceResult>,
    _method: MethodIdInput<'db>,
) -> Arc<BodyInferenceResult> {
    value
}
