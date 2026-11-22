use std::sync::Arc;

use base_db::FileIdInput;
use bsl_platform::PlatformDataInner;
use bsl_types::kind::TypeKind;
use cfg_types::IdConversion;
use hir_def::body::Body;
use hir_def::call_graph::{
    CallerId, ModuleCallSummary, NotifyConstructorCandidate, NotifyReceiverSyntax, NotifyReg,
    NotifyTarget,
};
use hir_def::hir::{Expr, ExprIdx, Literal};
use hir_def::{DefWithBodyId, ExprId, MethodKey, ModuleId, Name, StmtId};
use intern::NormName;
use rustc_hash::{FxHashMap, FxHashSet};
use vfs::FileId;

use crate::db::HirDatabase;
use crate::infer::{BareNameOrigin, InferOwnerResult, infer_owner};
use crate::notify_array::{
    accepts_argument_count, callback_slots, collect_notify_array_registrations,
};

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
    if raw.notify_candidates.is_empty() {
        if raw.name_literal_ref_sites.is_empty() {
            return raw;
        }
        let mut summary = (*raw).clone();
        summary.name_literal_ref_sites.clear();
        return Arc::new(summary);
    }

    let module_id = ModuleId::new(file_id);
    let mut summary = (*raw).clone();
    let candidates = summary.notify_candidates.clone();
    let mut candidates_by_caller: FxHashMap<CallerId, Vec<usize>> = FxHashMap::default();
    for (index, candidate) in candidates.iter().enumerate() {
        candidates_by_caller.entry(candidate.caller).or_default().push(index);
    }

    let platform = PlatformDataInner::instance();
    let bodies = db.module_bodies_ref(module_id);
    for (local_id, lower) in bodies.iter_lower_results() {
        let caller = CallerId::Method(local_id);
        let Some(indices) = candidates_by_caller.remove(&caller) else { continue };
        normalize_owner_candidates(
            db,
            file_id,
            DefWithBodyId::Method(local_id),
            lower.body(),
            &indices,
            &candidates,
            platform,
            &mut summary.notify_regs,
        );
    }

    if let Some(lower) = bodies.module_code_result() {
        if let Some(indices) = candidates_by_caller.remove(&CallerId::ModuleCode) {
            normalize_owner_candidates(
                db,
                file_id,
                DefWithBodyId::ModuleCode,
                lower.body(),
                &indices,
                &candidates,
                platform,
                &mut summary.notify_regs,
            );
        }
    }

    // Candidates without a readable owner body cannot establish a registration.
    summary.notify_candidates.clear();
    retain_notify_name_literal_sites(&mut summary);
    filter_uncompiled_notify_name_literals(&mut summary);

    Arc::new(summary)
}

fn retain_notify_name_literal_sites(summary: &mut ModuleCallSummary) {
    let mut local_methods: FxHashMap<NormName, MethodKey> = FxHashMap::default();
    for method in &summary.methods {
        local_methods.entry(NormName::intern(method.name.as_str())).or_insert(method.local_id);
    }

    let mut notify_handler_methods = FxHashSet::default();
    let mut notify_callback_sites = FxHashSet::default();
    let mut local_callback_sites = FxHashSet::default();
    for registration in &summary.notify_regs {
        let site = (registration.caller, registration.callback_expr);
        notify_callback_sites.insert(site);
        if let Some(&local_id) =
            local_methods.get(&NormName::intern(registration.callback_name.as_str()))
        {
            notify_handler_methods.insert(local_id);
        }
        if matches!(&registration.target, NotifyTarget::ThisObject) {
            local_callback_sites.insert(site);
        }
    }

    let retained_local_methods: FxHashSet<_> = summary
        .name_literal_ref_sites
        .iter()
        .filter(|site| {
            let key = (site.caller, site.expr);
            !notify_callback_sites.contains(&key) || local_callback_sites.contains(&key)
        })
        .map(|site| site.local_id)
        .collect();
    summary.name_literal_refs.retain(|method| retained_local_methods.contains(method));
    summary.name_literal_ref_sites.retain(|site| notify_handler_methods.contains(&site.local_id));
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

#[allow(
    clippy::too_many_arguments,
    reason = "The projection keeps database context and caller-specific inputs separate from its output."
)]
fn normalize_owner_candidates(
    db: &dyn HirDatabase,
    file_id: FileId,
    owner: DefWithBodyId,
    body: &Body,
    candidate_indices: &[usize],
    candidates: &[NotifyConstructorCandidate],
    platform: &PlatformDataInner,
    registrations: &mut Vec<NotifyReg>,
) {
    let inference = infer_owner(db, file_id, owner);
    let dead_stmts: FxHashSet<StmtId> = inference.dead_stmts().iter().copied().collect();
    let array_registrations = collect_notify_array_registrations(body, platform);

    for &index in candidate_indices {
        let candidate = candidates[index];
        let Expr::New { type_name, args } = body.expr(candidate.constructor) else { continue };
        let dynamic = type_name.is_none();
        let platform_type_name = type_name
            .as_ref()
            .map(Name::as_str)
            .or_else(|| args.first().and_then(|&arg| string_literal(body, arg)));
        let Some(platform_type_name) = platform_type_name else { continue };
        if !platform.is_notify_description_type_name(platform_type_name) {
            continue;
        }
        let Some(slots) = callback_slots(platform, platform_type_name) else { continue };
        let not_compiled =
            constructor_is_not_compiled(body, &dead_stmts, &inference, candidate.constructor);

        if dynamic {
            let Some(array) = array_registrations
                .iter()
                .find(|registration| registration.constructor == candidate.constructor)
            else {
                continue;
            };
            for handler in &array.handlers {
                append_notify_registration(
                    db,
                    candidate,
                    handler.callback_name.as_str(),
                    handler.callback_expr,
                    handler.receiver,
                    handler.receiver_syntax,
                    not_compiled,
                    &inference,
                    registrations,
                );
            }
            continue;
        }

        if !accepts_argument_count(slots, args.len()) {
            continue;
        }
        for (handler_slot, receiver_slot) in
            [(slots.handler, slots.receiver), (slots.error_handler, slots.error_receiver)]
        {
            let Some(&callback_idx) = args.get(handler_slot) else { continue };
            let Some(callback_name) = string_literal(body, callback_idx) else { continue };
            let callback_expr = ExprId::from_idx(callback_idx);
            let receiver = args.get(receiver_slot).copied().map(ExprId::from_idx);
            let receiver_syntax = receiver_syntax(body, receiver);
            append_notify_registration(
                db,
                candidate,
                callback_name,
                callback_expr,
                receiver,
                receiver_syntax,
                not_compiled,
                &inference,
                registrations,
            );
        }
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "Source provenance and inferred receiver state are both needed to create each registration."
)]
fn append_notify_registration(
    db: &dyn HirDatabase,
    candidate: NotifyConstructorCandidate,
    callback_name: &str,
    callback_expr: ExprId,
    receiver: Option<ExprId>,
    receiver_syntax: NotifyReceiverSyntax,
    not_compiled: bool,
    inference: &InferOwnerResult,
    registrations: &mut Vec<NotifyReg>,
) {
    let target = if not_compiled {
        NotifyTarget::NotCompiled { callback_expr, receiver_syntax }
    } else {
        receiver.map_or(NotifyTarget::Unsupported, |receiver| {
            normalize_receiver(db, inference, receiver)
        })
    };
    let callback_name = Name::new(callback_name);
    registrations.push(NotifyReg {
        caller: candidate.caller,
        callback_expr,
        callback_name,
        receiver_syntax,
        target,
        range: candidate.range,
    });
}

fn string_literal(body: &Body, expr: ExprIdx) -> Option<&str> {
    match body.expr_idx(expr) {
        Expr::Literal(Literal::String(value)) => Some(value),
        _ => None,
    }
}

fn receiver_syntax(body: &Body, receiver: Option<ExprId>) -> NotifyReceiverSyntax {
    match receiver.map(|expr| body.expr(expr)) {
        Some(Expr::Path(name)) => NotifyReceiverSyntax::for_bare_name(name),
        _ => NotifyReceiverSyntax::Other,
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
            TypeKind::CommonModule(module) => NotifyTarget::Module(Name::new(&module.name)),
            _ => NotifyTarget::Unsupported,
        },
        Some((BareNameOrigin::SelfName, _)) => NotifyTarget::ThisObject,
        _ => NotifyTarget::Unsupported,
    }
}
