use std::sync::Arc;

use base_db::FileIdInput;
use bsl_metadata::MdObject;
use bsl_platform::PlatformDataInner;
use bsl_types::kind::TypeKind;
use hir_def::body::Body;
use hir_def::call_graph::{
    CallerId, ModuleCallSummary, NotifyConstructorCandidate, NotifyReceiverSyntax, NotifyReg,
    NotifyTarget,
};
use hir_def::hir::{Expr, ExprIdx, Literal};
use hir_def::{
    call_graph::NotifyRegistrationEnvironments, execution_env::EnvFlags, DefWithBodyId, ExprId,
    IdConversion, MethodKey, ModuleId, Name, StmtId,
};
use intern::NormName;
use rustc_hash::{FxHashMap, FxHashSet};
use vfs::FileId;

use crate::body_flow::BodyFlow;
use crate::db::HirDatabase;
use crate::infer::{infer_owner, BareNameOrigin, InferOwnerResult};
use crate::notify_array::{
    callback_arguments_for_binding, collect_notify_array_registrations, reachable_statements,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifyCandidateKind {
    Static,
    Dynamic,
    DynamicArray,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifyCallbackSlot {
    Handler,
    ErrorHandler,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotifyCandidateReceiver {
    ThisObject,
    ForeignModule(Name),
    Unknown,
    NotCompiled,
}

/// One callback slot attributed to a specific notification constructor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotifyCandidate {
    pub caller: CallerId,
    pub constructor: ExprId,
    pub expression: ExprId,
    pub slot: NotifyCallbackSlot,
    pub kind: NotifyCandidateKind,
    pub callback_name: Name,
    pub receiver_syntax: NotifyReceiverSyntax,
    pub receiver: NotifyCandidateReceiver,
    /// Whether this candidate conservatively counts as a local method reference.
    pub protects_local_method: bool,
    /// Whether the constructor is reachable in its owner body.
    pub site_reachable: bool,
    /// True when alias analysis for the containing dynamic constructor exhausted its budget.
    pub analysis_incomplete: bool,
    pub registration: Option<NotifyReg>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticNotifyFacts {
    pub summary: Arc<ModuleCallSummary>,
    pub notify_candidates: Arc<Vec<NotifyCandidate>>,
}

#[salsa::tracked(
    lru = 256,
    heap_size = semantic_notify_facts_heap,
    returns(clone)
)]
pub fn semantic_notify_facts_query<'db>(
    db: &'db dyn HirDatabase,
    file_id_input: FileIdInput<'db>,
) -> Arc<SemanticNotifyFacts> {
    let file_id = file_id_input.file_id(db);
    let raw = hir_def::queries::module_call_summary_query(db, file_id_input);
    if raw.notify_candidates.is_empty() {
        let mut summary = (*raw).clone();
        if !summary.name_literal_ref_sites.is_empty() {
            summary.name_literal_ref_sites.clear();
        }
        let mut notify_candidates = syntax_only_dynamic_candidates(
            &summary,
            &raw.notify_candidates,
            &[],
            &FxHashSet::default(),
        );
        notify_candidates.extend(incomplete_dynamic_candidates(&summary, &FxHashSet::default()));
        return Arc::new(SemanticNotifyFacts {
            summary: Arc::new(summary),
            notify_candidates: Arc::new(notify_candidates),
        });
    }

    let module_id = ModuleId::new(file_id);
    let mut summary = (*raw).clone();
    let candidates = summary.notify_candidates.clone();
    let mut notify_candidates = Vec::new();
    let mut reachable_dynamic_constructors: FxHashSet<(CallerId, ExprId)> = FxHashSet::default();
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
            &mut notify_candidates,
            &mut reachable_dynamic_constructors,
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
                &mut notify_candidates,
                &mut reachable_dynamic_constructors,
            );
        }
    }

    // Candidates without a readable owner body cannot establish a registration.
    summary.notify_candidates.clear();
    retain_notify_name_literal_sites(&mut summary, &notify_candidates);
    filter_uncompiled_notify_name_literals(&mut summary, &notify_candidates);
    notify_candidates.extend(syntax_only_static_candidates(
        &summary,
        &candidates,
        bodies,
        &notify_candidates,
    ));
    notify_candidates.extend(syntax_only_dynamic_candidates(
        &summary,
        &candidates,
        &notify_candidates,
        &reachable_dynamic_constructors,
    ));
    notify_candidates
        .extend(incomplete_dynamic_candidates(&summary, &reachable_dynamic_constructors));

    Arc::new(SemanticNotifyFacts {
        summary: Arc::new(summary),
        notify_candidates: Arc::new(notify_candidates),
    })
}

pub fn semantic_module_call_summary_query<'db>(
    db: &'db dyn HirDatabase,
    file_id_input: FileIdInput<'db>,
) -> Arc<ModuleCallSummary> {
    semantic_notify_facts_query(db, file_id_input).summary.clone()
}

pub fn semantic_notify_candidates_query<'db>(
    db: &'db dyn HirDatabase,
    file_id_input: FileIdInput<'db>,
) -> Arc<Vec<NotifyCandidate>> {
    semantic_notify_facts_query(db, file_id_input).notify_candidates.clone()
}

fn syntax_only_dynamic_candidates(
    summary: &ModuleCallSummary,
    constructors: &[NotifyConstructorCandidate],
    classified_candidates: &[NotifyCandidate],
    reachable_dynamic_constructors: &FxHashSet<(CallerId, ExprId)>,
) -> Vec<NotifyCandidate> {
    summary
        .notify_dynamic_signature_handler_sites
        .iter()
        .filter_map(|site| {
            let constructor =
                constructors.iter().find(|candidate| candidate.range == site.constructor_range)?;
            let site_reachable = reachable_dynamic_constructors
                .contains(&(constructor.caller, constructor.constructor));
            let already_classified = classified_candidates.iter().any(|candidate| {
                candidate.caller == constructor.caller
                    && candidate.constructor == constructor.constructor
                    && candidate.expression == site.callback_expr
            });
            (!already_classified).then(|| NotifyCandidate {
                caller: constructor.caller,
                constructor: constructor.constructor,
                expression: site.callback_expr,
                slot: NotifyCallbackSlot::Unknown,
                kind: NotifyCandidateKind::DynamicArray,
                callback_name: site.handler_name.clone(),
                receiver_syntax: NotifyReceiverSyntax::Other,
                receiver: NotifyCandidateReceiver::Unknown,
                protects_local_method: site_reachable,
                site_reachable,
                analysis_incomplete: false,
                registration: None,
            })
        })
        .collect()
}

fn syntax_only_static_candidates(
    summary: &ModuleCallSummary,
    constructors: &[NotifyConstructorCandidate],
    bodies: &hir_def::ModuleBodies,
    classified: &[NotifyCandidate],
) -> Vec<NotifyCandidate> {
    let protect_source_fallback = !bsl_platform::has_callback_description_constructor_facts(
        bsl_platform::PlatformData::instance(),
    );
    let slots = bsl_platform::bundled_callback_description_slots();
    let source_names = summary
        .notify_signature_handlers
        .iter()
        .chain(&summary.notify_additional_signature_handlers)
        .collect::<Vec<_>>();
    constructors
        .iter()
        .filter_map(|constructor| {
            let body = match constructor.caller {
                CallerId::Method(method) => bodies
                    .iter_body_arcs()
                    .find(|(key, _)| *key == method)
                    .map(|(_, body)| body.as_ref())?,
                CallerId::ModuleCode => bodies.module_code()?,
            };
            let Expr::New { type_name, args } = body.expr(constructor.constructor) else {
                return None;
            };
            let (type_name, slot_offset, kind) = match type_name {
                Some(type_name) => (type_name.as_str(), 0, NotifyCandidateKind::Static),
                None => {
                    let Expr::Literal(Literal::String(type_name)) = body.expr_idx(*args.first()?)
                    else {
                        return None;
                    };
                    (type_name.as_str(), 1, NotifyCandidateKind::Dynamic)
                }
            };
            if !bsl_platform::is_callback_description_type_name(type_name) {
                return None;
            }
            let mut fallback = Vec::new();
            for (slot, handler_arg, receiver_arg) in [
                (
                    NotifyCallbackSlot::Handler,
                    slots.handler + slot_offset,
                    slots.receiver + slot_offset,
                ),
                (
                    NotifyCallbackSlot::ErrorHandler,
                    slots.error_handler + slot_offset,
                    slots.error_receiver + slot_offset,
                ),
            ] {
                let Some(&handler_expr) = args.get(handler_arg) else { continue };
                let Expr::Literal(Literal::String(name)) = body.expr_idx(handler_expr) else {
                    continue;
                };
                let callback_name = Name::new(name);
                if !source_names.iter().any(|source| source.eq_ignore_case(&callback_name)) {
                    continue;
                }
                let expression = ExprId::from_idx(handler_expr);
                if classified.iter().any(|candidate| {
                    candidate.caller == constructor.caller
                        && candidate.constructor == constructor.constructor
                        && candidate.expression == expression
                }) {
                    continue;
                }
                let receiver_syntax =
                    args.get(receiver_arg).map_or(NotifyReceiverSyntax::Other, |&receiver| {
                        match body.expr_idx(receiver) {
                            Expr::Path(name) => NotifyReceiverSyntax::for_bare_name(name),
                            _ => NotifyReceiverSyntax::Other,
                        }
                    });
                if !matches!(
                    receiver_syntax,
                    NotifyReceiverSyntax::SelfName | NotifyReceiverSyntax::Other
                ) {
                    continue;
                }
                fallback.push(NotifyCandidate {
                    caller: constructor.caller,
                    constructor: constructor.constructor,
                    expression,
                    slot,
                    kind,
                    callback_name,
                    receiver_syntax,
                    receiver: NotifyCandidateReceiver::Unknown,
                    protects_local_method: protect_source_fallback,
                    site_reachable: true,
                    analysis_incomplete: false,
                    registration: None,
                });
            }
            Some(fallback)
        })
        .flatten()
        .collect()
}

fn incomplete_dynamic_candidates(
    summary: &ModuleCallSummary,
    reachable_dynamic_constructors: &FxHashSet<(CallerId, ExprId)>,
) -> Vec<NotifyCandidate> {
    summary
        .notify_dynamic_signature_analysis_incomplete
        .iter()
        .filter(|gap| reachable_dynamic_constructors.contains(&(gap.caller, gap.constructor)))
        .map(|gap| NotifyCandidate {
            caller: gap.caller,
            constructor: gap.constructor,
            expression: gap.constructor,
            slot: NotifyCallbackSlot::Unknown,
            kind: NotifyCandidateKind::DynamicArray,
            callback_name: Name::new(""),
            receiver_syntax: NotifyReceiverSyntax::Other,
            receiver: NotifyCandidateReceiver::Unknown,
            protects_local_method: true,
            site_reachable: true,
            analysis_incomplete: true,
            registration: None,
        })
        .collect()
}

fn retain_notify_name_literal_sites(
    summary: &mut ModuleCallSummary,
    candidates: &[NotifyCandidate],
) {
    let mut local_methods: FxHashMap<NormName, MethodKey> = FxHashMap::default();
    for method in &summary.methods {
        local_methods.entry(NormName::intern(method.name.as_str())).or_insert(method.local_id);
    }

    let mut notify_handler_methods = FxHashSet::default();
    let mut notify_callback_sites = FxHashSet::default();
    let mut local_callback_sites = FxHashSet::default();
    for registration in candidates.iter().filter_map(|candidate| candidate.registration.as_ref()) {
        let site = (registration.caller, registration.callback_expr);
        notify_callback_sites.insert(site);
        if matches!(&registration.target, NotifyTarget::Module(_)) {
            continue;
        }
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

fn filter_uncompiled_notify_name_literals(
    summary: &mut ModuleCallSummary,
    candidates: &[NotifyCandidate],
) {
    let uncompiled_callbacks: FxHashSet<_> = candidates
        .iter()
        .filter_map(|candidate| candidate.registration.as_ref())
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

fn semantic_notify_facts_heap(facts: &Arc<SemanticNotifyFacts>) -> usize {
    hir_def::queries::module_call_summary_heap(&facts.summary)
        + facts.notify_candidates.len() * std::mem::size_of::<NotifyCandidate>()
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
    notify_candidates: &mut Vec<NotifyCandidate>,
    reachable_dynamic_constructors: &mut FxHashSet<(CallerId, ExprId)>,
) {
    let current_common_module = db
        .module_metadata(ModuleId::new(file_id))
        .common_module
        .as_ref()
        .map(|module| Name::new(module.name()));
    let inference = infer_owner(db, file_id, owner);
    let dead_stmts: FxHashSet<StmtId> = inference.dead_stmts().iter().copied().collect();
    let flow = match owner {
        DefWithBodyId::Method(local_id) => {
            let method = hir_def::MethodId { module: ModuleId::new(file_id), local_id };
            let input = hir_def::MethodIdInput::new(db, method);
            BodyFlow::for_method(db, input, db.method_body_ref(input))
        }
        DefWithBodyId::ModuleCode => {
            let bodies = db.module_bodies_ref(ModuleId::new(file_id));
            bodies.module_code_arc().map_or_else(
                || BodyFlow::detached(std::sync::Arc::new(body.clone()), false),
                |body| BodyFlow::for_module_code(db, file_id, body),
            )
        }
    };
    let cfg = flow.cfg(db);
    let reachable_stmts = reachable_statements(&cfg);
    let reaching_defs = flow.executable_reaching_defs(db);
    let array_registrations = collect_notify_array_registrations(
        db,
        body,
        &cfg,
        reaching_defs.as_deref(),
        &inference,
        platform,
    );

    for &index in candidate_indices {
        let candidate = candidates[index];
        let Expr::New { type_name, args } = body.expr(candidate.constructor) else { continue };
        let dynamic = type_name.is_none();
        let not_compiled = constructor_is_not_compiled(
            body,
            &dead_stmts,
            &reachable_stmts,
            &inference,
            candidate.constructor,
        );
        if dynamic && !not_compiled {
            reachable_dynamic_constructors.insert((candidate.caller, candidate.constructor));
        }
        let platform_type_name = type_name
            .as_ref()
            .map(Name::as_str)
            .or_else(|| args.first().and_then(|&arg| string_literal(body, arg)));
        let Some(platform_type_name) = platform_type_name else { continue };
        if !platform.is_notify_description_type_name(platform_type_name) {
            continue;
        }
        let environments =
            notify_registration_environments(db, file_id, &inference, candidate, not_compiled);

        if dynamic {
            let Some(array) = array_registrations
                .iter()
                .find(|registration| registration.constructor == candidate.constructor)
            else {
                continue;
            };
            let mut candidate_regs = Vec::with_capacity(array.handlers.len());
            for handler in &array.handlers {
                let registration = build_notify_registration(
                    db,
                    candidate,
                    handler.callback_name.as_str(),
                    handler.callback_expr,
                    handler.receiver,
                    handler.receiver_syntax,
                    not_compiled,
                    environments,
                    &inference,
                );
                notify_candidates.push(classify_registration(
                    candidate.constructor,
                    handler.slot,
                    NotifyCandidateKind::DynamicArray,
                    registration.clone(),
                    current_common_module.as_ref(),
                ));
                candidate_regs.push(registration);
            }
            dedup_identical_callback_slots(&mut candidate_regs);
            registrations.extend(candidate_regs);
            continue;
        }

        let Some(binding) = inference
            .call_arg_bindings()
            .iter()
            .find(|binding| binding.call_expr == candidate.constructor)
        else {
            continue;
        };
        let Some(arguments) = callback_arguments_for_binding(binding) else { continue };
        let mut candidate_regs = Vec::with_capacity(2);
        for (handler_argument, receiver_argument, slot) in [
            (arguments.handler, arguments.receiver, NotifyCallbackSlot::Handler),
            (arguments.error_handler, arguments.error_receiver, NotifyCallbackSlot::ErrorHandler),
        ] {
            let Some(&callback_expr) = handler_argument.and_then(|index| binding.args.get(index))
            else {
                continue;
            };
            let Some(callback_name) = string_literal_id(body, callback_expr) else { continue };
            let receiver = receiver_argument.and_then(|index| binding.args.get(index)).copied();
            let receiver_syntax = receiver_syntax(body, receiver);
            let registration = build_notify_registration(
                db,
                candidate,
                callback_name,
                callback_expr,
                receiver,
                receiver_syntax,
                not_compiled,
                environments,
                &inference,
            );
            notify_candidates.push(classify_registration(
                candidate.constructor,
                slot,
                NotifyCandidateKind::Static,
                registration.clone(),
                current_common_module.as_ref(),
            ));
            candidate_regs.push(registration);
        }
        dedup_identical_callback_slots(&mut candidate_regs);
        registrations.extend(candidate_regs);
    }
}

fn classify_registration(
    constructor: ExprId,
    slot: NotifyCallbackSlot,
    kind: NotifyCandidateKind,
    registration: NotifyReg,
    current_common_module: Option<&Name>,
) -> NotifyCandidate {
    let receiver = match &registration.target {
        NotifyTarget::ThisObject => NotifyCandidateReceiver::ThisObject,
        NotifyTarget::Module(module)
            if current_common_module.is_some_and(|current| module.eq_ignore_case(current)) =>
        {
            NotifyCandidateReceiver::ThisObject
        }
        NotifyTarget::Module(module) => NotifyCandidateReceiver::ForeignModule(module.clone()),
        NotifyTarget::NotCompiled { .. } => NotifyCandidateReceiver::NotCompiled,
        NotifyTarget::Unsupported => NotifyCandidateReceiver::Unknown,
    };
    NotifyCandidate {
        caller: registration.caller,
        constructor,
        expression: registration.callback_expr,
        slot,
        kind,
        callback_name: registration.callback_name.clone(),
        receiver_syntax: registration.receiver_syntax,
        receiver,
        protects_local_method: match &registration.target {
            NotifyTarget::ThisObject => true,
            NotifyTarget::Module(module) => {
                current_common_module.is_some_and(|current| module.eq_ignore_case(current))
            }
            NotifyTarget::Unsupported => kind == NotifyCandidateKind::DynamicArray,
            NotifyTarget::NotCompiled { .. } => false,
        },
        site_reachable: !matches!(&registration.target, NotifyTarget::NotCompiled { .. }),
        analysis_incomplete: false,
        registration: Some(registration),
    }
}

/// The error-handler slot repeating the main handler on the same receiver names one platform
/// invocation target; summary registrations merge it, while `NotifyCandidate` retains both slots.
fn dedup_identical_callback_slots(regs: &mut Vec<NotifyReg>) {
    regs.dedup_by(|later, earlier| {
        NormName::intern(later.callback_name.as_str())
            == NormName::intern(earlier.callback_name.as_str())
            && later.receiver_syntax == earlier.receiver_syntax
            && same_normalized_target(&later.target, &earlier.target)
    });
}

/// Dedup merges only proven-identical invocation targets: `ThisObject` is the current
/// module itself, and equal folded module names are the same common module. `Unsupported`
/// and `NotCompiled` mean "unknown receiver", not "the same receiver" — different
/// unsupported expressions share the classification, so their registrations stay.
fn same_normalized_target(left: &NotifyTarget, right: &NotifyTarget) -> bool {
    match (left, right) {
        (NotifyTarget::ThisObject, NotifyTarget::ThisObject) => true,
        (NotifyTarget::Module(left), NotifyTarget::Module(right)) => {
            NormName::intern(left.as_str()) == NormName::intern(right.as_str())
        }
        _ => false,
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "Source provenance and inferred receiver state are both needed to create each registration."
)]
fn build_notify_registration(
    db: &dyn HirDatabase,
    candidate: NotifyConstructorCandidate,
    callback_name: &str,
    callback_expr: ExprId,
    receiver: Option<ExprId>,
    receiver_syntax: NotifyReceiverSyntax,
    not_compiled: bool,
    environments: NotifyRegistrationEnvironments,
    inference: &InferOwnerResult,
) -> NotifyReg {
    let target = if not_compiled {
        NotifyTarget::NotCompiled { callback_expr, receiver_syntax }
    } else {
        receiver.map_or(NotifyTarget::Unsupported, |receiver| {
            normalize_receiver(db, inference, receiver)
        })
    };
    let callback_name = Name::new(callback_name);
    NotifyReg {
        caller: candidate.caller,
        callback_expr,
        callback_name,
        receiver_syntax,
        target,
        environments,
        range: candidate.range,
    }
}

fn string_literal(body: &Body, expr: ExprIdx) -> Option<&str> {
    match body.expr_idx(expr) {
        Expr::Literal(Literal::String(value)) => Some(value),
        _ => None,
    }
}

fn string_literal_id(body: &Body, expr: ExprId) -> Option<&str> {
    match body.expr(expr) {
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
    reachable_stmts: &FxHashSet<StmtId>,
    inference: &InferOwnerResult,
    constructor: ExprId,
) -> bool {
    body.enclosing_stmt(constructor)
        .is_some_and(|stmt| dead_stmts.contains(&stmt) || !reachable_stmts.contains(&stmt))
        || inference
            .notify_constructor_environment(constructor)
            .is_some_and(|environment| !environment.may_be_available())
}

fn notify_registration_environments(
    db: &dyn HirDatabase,
    file_id: FileId,
    inference: &InferOwnerResult,
    candidate: NotifyConstructorCandidate,
    not_compiled: bool,
) -> NotifyRegistrationEnvironments {
    if not_compiled {
        return NotifyRegistrationEnvironments::default();
    }
    let (constructor_definite, constructor_possible) = inference
        .notify_constructor_registration_envs(candidate.constructor)
        .unwrap_or((EnvFlags::EMPTY, EnvFlags::ALL));
    let (site_definite, site_undecided) = hir_def::execution_env::conditional_env_tracking(
        &db.conditional_tree(file_id),
        candidate.range,
    );
    NotifyRegistrationEnvironments {
        definite: constructor_definite & site_definite,
        possible: constructor_possible & (site_definite | site_undecided),
    }
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
