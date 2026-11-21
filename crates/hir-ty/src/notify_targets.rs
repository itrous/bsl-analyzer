use std::sync::Arc;

use base_db::FileIdInput;
use bsl_types::kind::TypeKind;
use hir_def::body::Body;
use hir_def::call_graph::{CallerId, ModuleCallSummary, NotifyReceiverSyntax, NotifyTarget};
use hir_def::{DefWithBodyId, ExprId, ModuleId, StmtId};
use rustc_hash::{FxHashMap, FxHashSet};
use vfs::FileId;

use crate::db::HirDatabase;
use crate::infer::{infer_owner, BareNameOrigin, InferOwnerResult};

#[salsa::tracked(
    lru = 256,
    heap_size = module_call_summary_heap,
    returns(clone)
)]
pub fn semantic_module_call_summary_query<'db>(
    db: &'db dyn HirDatabase,
    file_id_input: FileIdInput<'db>,
) -> Arc<ModuleCallSummary> {
    let file_id = file_id_input.file_id(db);
    let raw = hir_def::queries::module_call_summary_query(db, file_id_input);
    if !raw.notify_regs.iter().any(|reg| is_syntactic_candidate(&reg.target)) {
        return raw;
    }

    let module_id = ModuleId::new(file_id);
    let mut summary = (*raw).clone();
    let mut registrations_by_caller: FxHashMap<CallerId, Vec<usize>> = FxHashMap::default();
    for (index, reg) in summary.notify_regs.iter().enumerate() {
        if is_syntactic_candidate(&reg.target) {
            registrations_by_caller.entry(reg.caller).or_default().push(index);
        }
    }

    let bodies = db.module_bodies_ref(module_id);
    for (local_id, lower) in bodies.iter_lower_results() {
        let caller = CallerId::Method(local_id);
        let Some(indices) = registrations_by_caller.remove(&caller) else { continue };
        let owner = DefWithBodyId::Method(local_id);
        normalize_owner_registrations(
            db,
            file_id,
            owner,
            lower.body(),
            &indices,
            &mut summary.notify_regs,
        );
    }

    if let Some(lower) = bodies.module_code_result() {
        if let Some(indices) = registrations_by_caller.remove(&CallerId::ModuleCode) {
            normalize_owner_registrations(
                db,
                file_id,
                DefWithBodyId::ModuleCode,
                lower.body(),
                &indices,
                &mut summary.notify_regs,
            );
        }
    }

    // A recovered/missing body cannot establish a receiver's owner. Do not let
    // its syntax-only candidate escape into graph or diagnostic consumers.
    for reg in &mut summary.notify_regs {
        if is_syntactic_candidate(&reg.target) {
            reg.target = NotifyTarget::Unsupported;
        }
    }
    filter_uncompiled_notify_name_literals(&mut summary);

    Arc::new(summary)
}

fn is_syntactic_candidate(target: &NotifyTarget) -> bool {
    matches!(target, NotifyTarget::BareName { .. } | NotifyTarget::Unresolved { .. })
}

fn filter_uncompiled_notify_name_literals(summary: &mut ModuleCallSummary) {
    let uncompiled_callbacks: FxHashSet<_> = summary
        .notify_regs
        .iter()
        .filter_map(|reg| match &reg.target {
            NotifyTarget::NotCompiled { callback_expr, .. } => Some((reg.caller, *callback_expr)),
            _ => None,
        })
        .collect();
    if uncompiled_callbacks.is_empty() {
        return;
    }

    let mut uncompiled_methods = FxHashSet::default();
    let mut live_methods = FxHashSet::default();
    for site in &summary.name_literal_ref_sites {
        let method_ids = if uncompiled_callbacks.contains(&(site.caller, site.expr)) {
            &mut uncompiled_methods
        } else {
            &mut live_methods
        };
        method_ids.insert(site.local_id);
    }
    summary
        .name_literal_refs
        .retain(|method| !uncompiled_methods.contains(method) || live_methods.contains(method));
}

fn module_call_summary_heap(summary: &Arc<ModuleCallSummary>) -> usize {
    hir_def::queries::module_call_summary_heap(summary)
}

fn normalize_owner_registrations(
    db: &dyn HirDatabase,
    file_id: FileId,
    owner: DefWithBodyId,
    body: &Body,
    registration_indices: &[usize],
    registrations: &mut [hir_def::call_graph::NotifyReg],
) {
    let inference = infer_owner(db, file_id, owner);
    let dead_stmts: FxHashSet<StmtId> = inference.dead_stmts().iter().copied().collect();

    for &index in registration_indices {
        let (constructor, callback_expr, bare_name) = match &registrations[index].target {
            NotifyTarget::BareName { name, constructor, callback_expr, .. } => {
                (*constructor, *callback_expr, Some(name))
            }
            NotifyTarget::Unresolved { constructor, callback_expr } => {
                (*constructor, *callback_expr, None)
            }
            _ => continue,
        };
        if constructor_is_not_compiled(body, &dead_stmts, &inference, constructor) {
            let receiver_syntax =
                bare_name.map_or(NotifyReceiverSyntax::Other, NotifyReceiverSyntax::for_bare_name);
            registrations[index].target =
                NotifyTarget::NotCompiled { callback_expr, receiver_syntax };
            continue;
        }

        let target = match &registrations[index].target {
            NotifyTarget::BareName { receiver, .. } => {
                normalize_receiver(db, &inference, *receiver)
            }
            NotifyTarget::Unresolved { .. } => NotifyTarget::Unsupported,
            _ => continue,
        };
        registrations[index].target = target;
    }
}

fn constructor_is_not_compiled(
    body: &Body,
    dead_stmts: &FxHashSet<StmtId>,
    inference: &InferOwnerResult,
    constructor: ExprId,
) -> bool {
    body.enclosing_stmt(constructor).is_some_and(|stmt| dead_stmts.contains(&stmt))
        || inference
            .notify_constructor_environment(constructor)
            .is_some_and(|environment| !environment.may_be_available())
}

fn normalize_receiver(
    db: &dyn HirDatabase,
    inference: &InferOwnerResult,
    receiver: ExprId,
) -> NotifyTarget {
    match inference.notify_receiver_resolution(receiver) {
        Some((BareNameOrigin::CommonModule, ty)) => match db.lookup_type(ty) {
            TypeKind::CommonModule(module) => {
                NotifyTarget::Module(hir_def::Name::new(&module.name))
            }
            _ => NotifyTarget::Unsupported,
        },
        Some((BareNameOrigin::SelfName, _)) => NotifyTarget::ThisObject,
        _ => NotifyTarget::Unsupported,
    }
}
