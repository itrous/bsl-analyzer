//! Proof-oriented extraction of notification handlers from a locally-built argument array.
//!
//! Dynamic `Новый("Тип", МассивПараметров)` receives one array, not positional callback arguments.
//! This module only exposes handlers when a body-local array has a unique declaration, a known
//! `Массив` construction, and an unbroken straight-line sequence of platform `Добавить` calls up
//! to the exact constructor read. Any other use that could alias or mutate the array opens the
//! state. Type and constructor facts come from the platform snapshot, not syntax lowering.

use bsl_platform::PlatformDataInner;
#[cfg(test)]
use cfg::{CfgEdgeType, CfgVertex, ControlFlowGraph};
use cfg_types::{ExprId, IdConversion};
use hir_def::body::events::{for_each_stmt_expr_root, EvalPoint};
use hir_def::body::Body;
use hir_def::call_graph::NotifyReceiverSyntax;
use hir_def::execution_env::EnvFlags;
use hir_def::hir::{Expr, ExprIdx, Literal, Stmt, StmtIdx};
use hir_def::Name;
use intern::NormName;
use rustc_hash::{FxHashMap, FxHashSet};
use smol_str::SmolStr;
use stdx::case::{eq_ignore_case, CaseExt};

use crate::call_resolution::{
    CallParamMode, CallRejection, CallSelection, CandidateApplicability, CandidateDisposition,
    CandidateEvaluation, CandidateRejection,
};
use crate::infer::{CallArgBinding, InferOwnerResult};
use crate::notify_targets::{NotifyCallbackSlot, NotifyWorkBudget};
use bsl_types::builders::Builders;
use bsl_types::intern::TypeKernelDb;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NotifyArrayHandler {
    pub constructor: ExprId,
    pub callback_name: Name,
    pub callback_expr: ExprId,
    pub receiver: Option<ExprId>,
    pub receiver_syntax: NotifyReceiverSyntax,
    pub slot: NotifyCallbackSlot,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NotifyArrayRegistration {
    pub constructor: ExprId,
    pub handlers: Vec<NotifyArrayHandler>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ArrayState {
    Uninitialized,
    Known(Vec<ExprId>),
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CallbackArguments {
    pub handler: Option<usize>,
    pub receiver: Option<usize>,
    pub error_handler: Option<usize>,
    pub error_receiver: Option<usize>,
}

/// Test-only equivalent of the reachable statement set production receives from
/// `notify_targets::reachable_statements_with_budget`.
///
/// Production charges its own traversal to the shared budget and passes the resulting set in;
/// this mirror is uncharged, so the array pass is exercised with the same input shape. The
/// `reachable_statements_match_the_production_traversal` test pins the two traversals together.
#[cfg(test)]
pub(crate) fn reachable_statements(cfg: &ControlFlowGraph) -> FxHashSet<cfg_types::StmtId> {
    let Some(start) = cfg.start() else { return FxHashSet::default() };
    cfg.reachable_vertices(start, CfgEdgeType::is_executable)
        .into_iter()
        .flat_map(|vertex| {
            let mut statements = cfg.source_stmt_id(vertex).into_iter().collect::<Vec<_>>();
            if let Some(CfgVertex::BasicBlock(block)) = cfg.vertex(vertex) {
                statements.extend_from_slice(block.statements());
            }
            statements
        })
        .collect()
}

/// Collect dynamic notification constructors whose argument array is exact at the read point.
///
/// The current proof boundary is deliberately narrow: the array must be an explicitly declared
/// local, initialized by a zero-argument platform `Array` constructor, and extended only by direct
/// one-argument `Array.Add`/`Массив.Добавить` expression statements. Constructor reads must be the
/// direct right-hand side of an assignment. Aliases, other calls, reassignment, indexed/field
/// writes, and control-flow blocks invalidate the proof. These constraints trade recall for a
/// defensible receiver expression and parameter order.
#[cfg(test)]
pub(crate) fn collect_notify_array_registrations(
    db: &dyn TypeKernelDb,
    body: &Body,
    reachable_stmts: &FxHashSet<cfg_types::StmtId>,
    reaching_defs: Option<&dataflow::reaching_defs::ReachingDefsResult>,
    inference: &InferOwnerResult,
    platform: &PlatformDataInner,
) -> Vec<NotifyArrayRegistration> {
    let mut cancel = || {};
    let mut budget = NotifyWorkBudget::with_limit(&mut cancel, usize::MAX);
    collect_notify_array_registrations_with_budget(
        db,
        body,
        reachable_stmts,
        reaching_defs,
        inference,
        platform,
        &mut budget,
    )
}

/// Budgeted collection entry point.
///
/// Every index entry, statement visit, write scan (one pass over the write events the statement
/// makes), constructor visit and handler visit spends from `budget`, so the shared cancellation
/// callback runs periodically while the pass works. Exhaustion is a hard stop: the returned
/// vector holds only the registrations proven before it and can therefore be partial. It is a
/// final answer solely while `NotifyWorkBudget::exhausted` is false; a caller that observes
/// exhaustion must treat the owner's notification facts as incomplete instead of publishing
/// these registrations as complete. A missing reaching-definitions solution is such a stop: the
/// solver published nothing, so the possible writes are unknown and the pass marks the budget
/// incomplete rather than answering with the registrations it happens to keep.
pub(crate) fn collect_notify_array_registrations_with_budget(
    db: &dyn TypeKernelDb,
    body: &Body,
    reachable_stmts: &FxHashSet<cfg_types::StmtId>,
    reaching_defs: Option<&dataflow::reaching_defs::ReachingDefsResult>,
    inference: &InferOwnerResult,
    platform: &PlatformDataInner,
    budget: &mut NotifyWorkBudget<'_>,
) -> Vec<NotifyArrayRegistration> {
    if body.is_lowering_incomplete() {
        return Vec::new();
    }
    let Some(reaching_defs) = reaching_defs else {
        // The solver stopped before publishing a complete solution, so a write the body
        // makes may rebind any tracked array invisibly. Without those writes no array
        // stays proven, and an empty answer here must not pass for a complete projection.
        budget.mark_incomplete();
        return Vec::new();
    };

    let Some(local_declarations) = unique_local_declarations(body, budget) else {
        return Vec::new();
    };
    let Some(folded_tracked) = folded_tracked_names(body, &local_declarations, budget) else {
        return Vec::new();
    };
    let mut states: FxHashMap<NormName, ArrayState> = FxHashMap::default();
    for &name in local_declarations.keys() {
        if !budget.spend() {
            return Vec::new();
        }
        states.insert(name, ArrayState::Uninitialized);
    }
    let mut used_before_initialization = FxHashSet::default();
    let mut registrations = Vec::new();

    for (position, &stmt_idx) in body.body_stmts_typed().iter().enumerate() {
        if !budget.spend() {
            return registrations;
        }
        let stmt_id = cfg_types::StmtId::from_idx(stmt_idx);
        if !reachable_stmts.contains(&stmt_id) {
            continue;
        }
        let stmt = body.stmt_idx(stmt_idx);
        if is_control_flow(stmt) {
            let Some(mentioned) = stmt_path_names(body, stmt_idx, budget) else {
                return registrations;
            };
            for name in mentioned {
                if !budget.spend() {
                    return registrations;
                }
                if matches!(states.get(&name), Some(ArrayState::Uninitialized)) {
                    used_before_initialization.insert(name);
                }
            }
            for state in states.values_mut() {
                if !budget.spend() {
                    return registrations;
                }
                if matches!(state, ArrayState::Known(_)) {
                    *state = ArrayState::Unknown;
                }
            }
            continue;
        }

        if let Stmt::Assign { target, value } = stmt {
            if let Some(target_name) = path_name(body, *target) {
                let key = NormName::intern(target_name.as_str());
                if let Some((declaration_position, _)) = local_declarations.get(&key) {
                    let is_empty_array = is_empty_array_constructor(body, *value, platform, budget);
                    if budget.exhausted() {
                        return registrations;
                    }
                    if is_empty_array {
                        let state = if *declaration_position < position
                            && !used_before_initialization.contains(&key)
                            && matches!(states.get(&key), Some(ArrayState::Uninitialized))
                        {
                            ArrayState::Known(Vec::new())
                        } else {
                            ArrayState::Unknown
                        };
                        states.insert(key, state);
                        if !invalidate_possible_writes(
                            &mut states,
                            &folded_tracked,
                            reaching_defs,
                            stmt_id,
                            budget,
                        ) {
                            return registrations;
                        }
                        continue;
                    }
                }

                if let Some((constructor, array_name)) =
                    dynamic_notify_array_call(body, *value, platform)
                {
                    let array_key = NormName::intern(array_name.as_str());
                    if let Some(ArrayState::Known(elements)) = states.get(&array_key) {
                        let binding = dynamic_callback_binding(
                            db,
                            inference,
                            body,
                            constructor,
                            elements,
                            platform,
                            budget,
                        );
                        if budget.exhausted() {
                            return registrations;
                        }
                        if let Some(binding) = binding {
                            let handlers =
                                handlers_from_binding(body, constructor, &binding, budget);
                            if budget.exhausted() {
                                return registrations;
                            }
                            if !handlers.is_empty() {
                                registrations
                                    .push(NotifyArrayRegistration { constructor, handlers });
                            }
                        }
                    }
                }

                if let Some(state) = states.get_mut(&key) {
                    *state = ArrayState::Unknown;
                }

                let Some(mut mentioned) = stmt_path_names(body, stmt_idx, budget) else {
                    return registrations;
                };
                mentioned.remove(&key);
                if let Some((_, array_name)) = dynamic_notify_array_call(body, *value, platform) {
                    mentioned.remove(&NormName::intern(array_name.as_str()));
                }
                if !invalidate_or_record_uses(
                    &mut states,
                    &mut used_before_initialization,
                    mentioned,
                    budget,
                ) {
                    return registrations;
                }
                if !invalidate_possible_writes(
                    &mut states,
                    &folded_tracked,
                    reaching_defs,
                    stmt_id,
                    budget,
                ) {
                    return registrations;
                }
                continue;
            }

            if let Some((constructor, array_name)) =
                dynamic_notify_array_call(body, *value, platform)
            {
                let array_key = NormName::intern(array_name.as_str());
                if let Some(ArrayState::Known(elements)) = states.get(&array_key) {
                    let binding = dynamic_callback_binding(
                        db,
                        inference,
                        body,
                        constructor,
                        elements,
                        platform,
                        budget,
                    );
                    if budget.exhausted() {
                        return registrations;
                    }
                    if let Some(binding) = binding {
                        let handlers = handlers_from_binding(body, constructor, &binding, budget);
                        if budget.exhausted() {
                            return registrations;
                        }
                        if !handlers.is_empty() {
                            registrations.push(NotifyArrayRegistration { constructor, handlers });
                        }
                    }
                }

                let Some(mut mentioned) = stmt_path_names(body, stmt_idx, budget) else {
                    return registrations;
                };
                mentioned.remove(&array_key);
                if !invalidate_or_record_uses(
                    &mut states,
                    &mut used_before_initialization,
                    mentioned,
                    budget,
                ) {
                    return registrations;
                }
                if !invalidate_possible_writes(
                    &mut states,
                    &folded_tracked,
                    reaching_defs,
                    stmt_id,
                    budget,
                ) {
                    return registrations;
                }
                continue;
            }
        }

        if let Stmt::Expr(expr) = stmt {
            if let Some((receiver_name, argument)) =
                array_add_statement(body, *expr, platform, budget)
            {
                if budget.exhausted() {
                    return registrations;
                }
                let key = NormName::intern(receiver_name.as_str());
                let Some(argument_names) = expression_path_names(body, argument, budget) else {
                    return registrations;
                };
                if !budget.spend_by(argument_names.len()) {
                    return registrations;
                }
                let argument_escapes = argument_names.iter().any(|name| states.contains_key(name));
                if !argument_escapes {
                    if let Some(ArrayState::Known(elements)) = states.get_mut(&key) {
                        elements.push(ExprId::from_idx(argument));
                    } else if matches!(states.get(&key), Some(ArrayState::Uninitialized)) {
                        used_before_initialization.insert(key);
                    }
                } else if let Some(state) = states.get_mut(&key) {
                    *state = ArrayState::Unknown;
                }

                if !invalidate_or_record_uses(
                    &mut states,
                    &mut used_before_initialization,
                    argument_names,
                    budget,
                ) {
                    return registrations;
                }
                let Some(mut other_names) = stmt_path_names(body, stmt_idx, budget) else {
                    return registrations;
                };
                other_names.remove(&key);
                let Some(argument_names) = expression_path_names(body, argument, budget) else {
                    return registrations;
                };
                for name in argument_names {
                    if !budget.spend() {
                        return registrations;
                    }
                    other_names.remove(&name);
                }
                if !invalidate_or_record_uses(
                    &mut states,
                    &mut used_before_initialization,
                    other_names,
                    budget,
                ) {
                    return registrations;
                }
                if !invalidate_possible_writes(
                    &mut states,
                    &folded_tracked,
                    reaching_defs,
                    stmt_id,
                    budget,
                ) {
                    return registrations;
                }
                continue;
            }
        }

        let Some(mentioned) = stmt_path_names(body, stmt_idx, budget) else {
            return registrations;
        };
        if !invalidate_or_record_uses(
            &mut states,
            &mut used_before_initialization,
            mentioned,
            budget,
        ) {
            return registrations;
        }
        if !invalidate_possible_writes(&mut states, &folded_tracked, reaching_defs, stmt_id, budget)
        {
            return registrations;
        }
    }

    registrations
}

/// Opens every tracked array that a write event of this statement may rebind.
///
/// One pass over the writes the point makes replaces the old query-per-tracked-name scan: the
/// cost follows the statement's write events, not the tracked-local count. Written names match
/// by their folded key ([`Definition::var_name`]) against the folded spellings of the tracked
/// locals.
fn invalidate_possible_writes(
    states: &mut FxHashMap<NormName, ArrayState>,
    folded_tracked: &FxHashMap<SmolStr, Vec<NormName>>,
    reaching_defs: &dataflow::reaching_defs::ReachingDefsResult,
    stmt: cfg_types::StmtId,
    budget: &mut NotifyWorkBudget<'_>,
) -> bool {
    for def in reaching_defs.writes_at_point(EvalPoint::Stmt(stmt)) {
        if !budget.spend() {
            return false;
        }
        let Some(names) = folded_tracked.get(&def.var_name) else { continue };
        for name in names {
            if !budget.spend() {
                return false;
            }
            if matches!(states.get(name), Some(ArrayState::Known(_))) {
                states.insert(*name, ArrayState::Unknown);
            }
        }
    }
    true
}

/// Folded spellings of the tracked locals: the keys that written names match against.
///
/// Each spelling contributes its own folded key because `fold_lower` applies contextual
/// mappings (final sigma): `eq_ignore_case`-equal spellings can fold differently, and the
/// written side folds each written spelling the same way.
fn folded_tracked_names(
    body: &Body,
    tracked: &FxHashMap<NormName, (usize, Name)>,
    budget: &mut NotifyWorkBudget<'_>,
) -> Option<FxHashMap<SmolStr, Vec<NormName>>> {
    let mut folded: FxHashMap<SmolStr, Vec<NormName>> = FxHashMap::default();
    for (_, binding) in body.bindings_iter() {
        if !budget.spend() || !add_folded_spelling(&mut folded, tracked, &binding.name) {
            return None;
        }
    }
    for (_, expr) in body.exprs_iter() {
        if !budget.spend() {
            return None;
        }
        if let Expr::Path(name) = expr {
            if !add_folded_spelling(&mut folded, tracked, name) {
                return None;
            }
        }
    }
    Some(folded)
}

fn add_folded_spelling(
    folded: &mut FxHashMap<SmolStr, Vec<NormName>>,
    tracked: &FxHashMap<NormName, (usize, Name)>,
    name: &Name,
) -> bool {
    let normalized = NormName::intern(name.as_str());
    if !tracked.contains_key(&normalized) {
        return true;
    }
    let key = SmolStr::new(name.as_str().fold_lower());
    let names = folded.entry(key).or_default();
    if !names.contains(&normalized) {
        names.push(normalized);
    }
    true
}

fn unique_local_declarations(
    body: &Body,
    budget: &mut NotifyWorkBudget<'_>,
) -> Option<FxHashMap<NormName, (usize, Name)>> {
    let mut binding_counts: FxHashMap<NormName, usize> = FxHashMap::default();
    for (_, binding) in body.bindings_iter() {
        if !budget.spend() {
            return None;
        }
        *binding_counts.entry(NormName::intern(binding.name.as_str())).or_default() += 1;
    }

    let mut parameters = FxHashSet::default();
    for id in body.params() {
        if !budget.spend() {
            return None;
        }
        parameters.insert(NormName::intern(body.binding(id).name.as_str()));
    }
    let mut declarations: FxHashMap<NormName, Vec<(usize, Name)>> = FxHashMap::default();
    for (position, &stmt_idx) in body.body_stmts_typed().iter().enumerate() {
        if !budget.spend() {
            return None;
        }
        let Stmt::VarDecl { bindings } = body.stmt_idx(stmt_idx) else { continue };
        for &binding in bindings.iter() {
            if !budget.spend() {
                return None;
            }
            let binding = body.binding_idx(binding);
            let name = NormName::intern(binding.name.as_str());
            declarations.entry(name).or_default().push((position, binding.name.clone()));
        }
    }

    let mut unique = FxHashMap::default();
    for (name, declarations) in declarations {
        if !budget.spend() {
            return None;
        }
        if declarations.len() == 1
            && binding_counts.get(&name) == Some(&1)
            && !parameters.contains(&name)
        {
            unique.insert(name, declarations.into_iter().next().expect("one declaration"));
        }
    }
    Some(unique)
}

fn is_empty_array_constructor(
    body: &Body,
    expr: ExprIdx,
    platform: &PlatformDataInner,
    budget: &mut NotifyWorkBudget<'_>,
) -> bool {
    let Expr::New { type_name: Some(type_name), args } = body.expr_idx(expr) else {
        return false;
    };
    if !args.is_empty() || !resolves_to_type(platform, type_name.as_str(), "Array") {
        return false;
    }
    let constructors = platform.get_constructors(type_name.as_str());
    for constructor in constructors {
        if !budget.spend() {
            return false;
        }
        let mut all_optional = true;
        for param in &constructor.parameters {
            if !budget.spend() {
                return false;
            }
            all_optional &= param.is_optional || param.is_variadic;
        }
        if all_optional {
            return true;
        }
    }
    false
}

fn dynamic_notify_array_call(
    body: &Body,
    expr: ExprIdx,
    platform: &PlatformDataInner,
) -> Option<(ExprId, Name)> {
    let Expr::New { type_name: None, args } = body.expr_idx(expr) else { return None };
    if args.len() != 2 {
        return None;
    }
    let Expr::Literal(Literal::String(type_name)) = body.expr_idx(args[0]) else {
        return None;
    };
    if !platform.is_notify_description_type_name(type_name) {
        return None;
    }
    let Expr::Path(array_name) = body.expr_idx(args[1]) else { return None };
    Some((ExprId::from_idx(expr), array_name.clone()))
}

fn dynamic_callback_binding(
    db: &dyn TypeKernelDb,
    inference: &InferOwnerResult,
    body: &Body,
    constructor: ExprId,
    elements: &[ExprId],
    platform: &PlatformDataInner,
    budget: &mut NotifyWorkBudget<'_>,
) -> Option<CallArgBinding> {
    if !budget.spend() {
        return None;
    }
    let Expr::New { type_name: None, args } = body.expr(constructor) else { return None };
    let Expr::Literal(Literal::String(type_name)) = body.expr_idx(*args.first()?) else {
        return None;
    };
    let constructor_refs = platform.get_constructors(type_name);
    for constructor in &constructor_refs {
        if !budget.spend_by(constructor.parameters.len().saturating_add(1)) {
            return None;
        }
    }
    let (definite, possible) = inference
        .notify_constructor_registration_envs(constructor)
        .unwrap_or((EnvFlags::EMPTY, EnvFlags::ALL));
    let environment = definite | possible;
    let environment = if environment.is_empty() { EnvFlags::ALL } else { environment };
    let candidates =
        crate::builtin::candidates::constructor_candidates(db, &constructor_refs, environment)
            .ok()?;
    if !budget.spend_by(candidates.as_slice().len()) {
        return None;
    }
    let mut argument_types = Vec::with_capacity(elements.len());
    for &expr in elements {
        if !budget.spend() {
            return None;
        }
        argument_types.push(inference.type_id_of_expr(expr).unwrap_or_else(|| db.unknown()));
    }
    // Resolution compares every argument with each applicable signature. Charge the
    // cross-product before entering the shared, non-cancellable binding engine.
    let binding_work = candidates.as_slice().len().saturating_mul(argument_types.len().max(1));
    if !budget.spend_by(binding_work) {
        return None;
    }
    let candidate = crate::call_binding::resolve_binding(db, candidates, &argument_types).semantic;
    if !budget.spend_by(candidate.resolution.candidates.len()) {
        return None;
    }
    Some(CallArgBinding {
        owner: inference.owner(),
        call_expr: constructor,
        args: elements.to_vec(),
        candidate,
        in_dead_branch: false,
    })
}

#[cfg(test)]
pub(crate) fn callback_arguments_for_binding(
    binding: &CallArgBinding,
) -> Option<CallbackArguments> {
    let mut cancel = || {};
    let mut budget = NotifyWorkBudget::with_limit(&mut cancel, usize::MAX);
    callback_arguments_for_binding_with_budget(binding, &mut budget)
}

pub(crate) fn callback_arguments_for_binding_with_budget(
    binding: &CallArgBinding,
    budget: &mut NotifyWorkBudget<'_>,
) -> Option<CallbackArguments> {
    if !budget.spend() {
        return None;
    }
    let resolution = &binding.candidate.resolution;
    let candidate_ids = match &resolution.selection {
        CallSelection::Unique { candidate } => vec![*candidate],
        CallSelection::Ambiguous { candidates } => {
            if !budget.spend_by(candidates.len()) {
                return None;
            }
            candidates.to_vec()
        }
        CallSelection::Rejected(CallRejection::Type) => {
            let mut candidate_ids = Vec::new();
            for fact in resolution.candidates.iter() {
                if !budget.spend() {
                    return None;
                }
                if matches!(
                    &fact.disposition,
                    CandidateDisposition::Rejected(CandidateRejection::TypeIncompatible { .. })
                ) {
                    candidate_ids.push(fact.id);
                }
            }
            candidate_ids
        }
        CallSelection::Rejected(CallRejection::Arity { .. } | CallRejection::NoCandidates) => {
            return None;
        }
    };

    // Index both sides by identity once: a per-id linear scan of every candidate turns an
    // overloaded constructor into a quadratic search.
    let mut facts_by_id = FxHashMap::<crate::call_resolution::CandidateId, _>::default();
    for fact in resolution.candidates.iter() {
        if !budget.spend() {
            return None;
        }
        facts_by_id.insert(fact.id, fact);
    }
    let mut signatures_by_id = FxHashMap::<crate::call_resolution::CandidateId, _>::default();
    for signature in binding.candidate.candidates.as_slice() {
        if !budget.spend() {
            return None;
        }
        signatures_by_id.insert(signature.id, signature);
    }
    let mut projection = None;
    for candidate_id in candidate_ids {
        if !budget.spend() {
            return None;
        }
        let fact = *facts_by_id.get(&candidate_id)?;
        let signature = *signatures_by_id.get(&candidate_id)?;
        let current = callback_argument_projection(fact, signature, budget)?;
        if projection.is_some_and(|previous| previous != current) {
            return None;
        }
        projection = Some(current);
    }
    projection
}

fn callback_argument_projection(
    fact: &crate::call_resolution::CandidateFact,
    signature: &crate::call_resolution::CallSignature,
    budget: &mut NotifyWorkBudget<'_>,
) -> Option<CallbackArguments> {
    if !budget.spend() {
        return None;
    }
    let CandidateEvaluation::ArityCompatible { applicability, .. } = &fact.evaluation else {
        return None;
    };
    let evaluations = match applicability {
        CandidateApplicability::Applicable { arguments }
        | CandidateApplicability::Indeterminate { arguments }
        | CandidateApplicability::Incompatible { arguments } => arguments.as_ref(),
    };
    let mut parameters = Vec::with_capacity(signature.params.len());
    for parameter in signature.params.iter() {
        if !budget.spend() {
            return None;
        }
        parameters.push(bsl_platform::MethodParam {
            name: parameter.name.clone(),
            param_type: None,
            is_optional: parameter.has_default,
            is_variadic: parameter.mode == CallParamMode::Variadic,
            passing: parameter.passing,
            result_follows_argument_text: parameter.result_follows_argument_text,
        });
    }
    if !budget.spend() {
        return None;
    }
    let slots = bsl_platform::callback_description_slots_for_parameters(&parameters)?;
    let mut argument_for_parameter = |parameter_index| {
        if !budget.spend() {
            return None;
        }
        let mut found = None;
        for argument in evaluations {
            if !budget.spend() {
                return None;
            }
            if matches!(argument.parameter, crate::call_resolution::ArgumentParameter::Declared { index, .. } if index == parameter_index)
            {
                if found.is_some() {
                    return None;
                }
                found = Some(argument.index);
            }
        }
        found
    };
    let handler = argument_for_parameter(slots.handler);
    let receiver = argument_for_parameter(slots.receiver);
    let error_handler = argument_for_parameter(slots.error_handler);
    let error_receiver = argument_for_parameter(slots.error_receiver);
    Some(CallbackArguments { handler, receiver, error_handler, error_receiver })
}

fn handlers_from_binding(
    body: &Body,
    constructor: ExprId,
    binding: &CallArgBinding,
    budget: &mut NotifyWorkBudget<'_>,
) -> Vec<NotifyArrayHandler> {
    let Some(arguments) = callback_arguments_for_binding_with_budget(binding, budget) else {
        return Vec::new();
    };
    let mut handlers = Vec::with_capacity(2);
    for (handler, receiver, slot) in [
        (arguments.handler, arguments.receiver, NotifyCallbackSlot::Handler),
        (arguments.error_handler, arguments.error_receiver, NotifyCallbackSlot::ErrorHandler),
    ] {
        if !budget.spend() {
            return Vec::new();
        }
        if let Some(handler) =
            handler_from_bound_arguments(body, constructor, binding, handler, receiver, slot)
        {
            handlers.push(handler);
        }
    }
    handlers
}

fn handler_from_bound_arguments(
    body: &Body,
    constructor: ExprId,
    binding: &CallArgBinding,
    handler_argument: Option<usize>,
    receiver_argument: Option<usize>,
    slot: NotifyCallbackSlot,
) -> Option<NotifyArrayHandler> {
    let callback_expr = *binding.args.get(handler_argument?)?;
    let Expr::Literal(Literal::String(callback_name)) = body.expr(callback_expr) else {
        return None;
    };
    let receiver = receiver_argument.and_then(|index| binding.args.get(index)).copied();
    let receiver_syntax = match receiver.map(|expr| body.expr(expr)) {
        Some(Expr::Path(name)) => NotifyReceiverSyntax::for_bare_name(name),
        _ => NotifyReceiverSyntax::Other,
    };
    Some(NotifyArrayHandler {
        constructor,
        callback_name: Name::new(callback_name),
        callback_expr,
        receiver,
        receiver_syntax,
        slot,
    })
}

fn array_add_statement(
    body: &Body,
    expr: ExprIdx,
    platform: &PlatformDataInner,
    budget: &mut NotifyWorkBudget<'_>,
) -> Option<(Name, ExprIdx)> {
    let (receiver, method_name, args) = match body.expr_idx(expr) {
        Expr::MethodCall { receiver, method, args } => (*receiver, method.as_str(), args.as_ref()),
        Expr::Call { callee, args } => {
            let Expr::Field { base, field } = body.expr_idx(*callee) else { return None };
            (*base, field.as_str(), args.as_ref())
        }
        _ => return None,
    };
    let Expr::Path(receiver_name) = body.expr_idx(receiver) else { return None };
    if args.len() != 1 {
        return None;
    }
    let method = platform.get_method("Array", method_name)?;
    if !budget.spend_by(method.parameters.len().saturating_add(1)) {
        return None;
    }
    if !eq_ignore_case(method.english_name.as_str(), "Add")
        || method.parameters.len() != 1
        || !accepts_method_argument_count(method, args.len())
    {
        return None;
    }
    Some((receiver_name.clone(), args[0]))
}

fn accepts_method_argument_count(method: &bsl_platform::PlatformMethod, count: usize) -> bool {
    count <= method.parameters.len()
        && method
            .parameters
            .iter()
            .enumerate()
            .skip(count)
            .all(|(_, parameter)| parameter.is_optional || parameter.is_variadic)
}

fn resolves_to_type(platform: &PlatformDataInner, name: &str, canonical: &str) -> bool {
    if platform.is_ambiguous_type_name(name) {
        return false;
    }
    let Some(resolved) = platform.get_type(name) else { return false };
    let Some(expected) = platform.get_type(canonical) else { return false };
    std::ptr::eq(resolved, expected)
}

fn path_name(body: &Body, expr: ExprIdx) -> Option<&Name> {
    match body.expr_idx(expr) {
        Expr::Path(name) => Some(name),
        _ => None,
    }
}

fn is_control_flow(stmt: &Stmt) -> bool {
    matches!(
        stmt,
        Stmt::If(_)
            | Stmt::PreprocIf(_)
            | Stmt::While { .. }
            | Stmt::For { .. }
            | Stmt::ForEach { .. }
            | Stmt::Try { .. }
            | Stmt::Goto(_)
            | Stmt::Label(_)
            | Stmt::Return { .. }
            | Stmt::Raise { .. }
            | Stmt::Break
            | Stmt::Continue
    )
}

fn invalidate_or_record_uses(
    states: &mut FxHashMap<NormName, ArrayState>,
    used_before_initialization: &mut FxHashSet<NormName>,
    mentioned: FxHashSet<NormName>,
    budget: &mut NotifyWorkBudget<'_>,
) -> bool {
    for name in mentioned {
        if !budget.spend() {
            return false;
        }
        match states.get_mut(&name) {
            Some(ArrayState::Uninitialized) => {
                used_before_initialization.insert(name);
            }
            Some(ArrayState::Known(_)) => {
                states.insert(name, ArrayState::Unknown);
            }
            Some(ArrayState::Unknown) | None => {}
        }
    }
    true
}

fn stmt_path_names(
    body: &Body,
    stmt: StmtIdx,
    budget: &mut NotifyWorkBudget<'_>,
) -> Option<FxHashSet<NormName>> {
    if !budget.spend() {
        return None;
    }
    let mut names = FxHashSet::default();
    let complete = match body.stmt_idx(stmt) {
        Stmt::If(if_stmt) => {
            if !collect_expression_paths(body, if_stmt.condition, &mut names, budget) {
                return None;
            }
            for (condition, branch) in if_stmt.elsif_branches.iter() {
                if !collect_expression_paths(body, *condition, &mut names, budget)
                    || !collect_statements_paths(body, branch, &mut names, budget)
                {
                    return None;
                }
            }
            if !collect_statements_paths(body, &if_stmt.then_branch, &mut names, budget) {
                return None;
            }
            if let Some(branch) = &if_stmt.else_branch {
                if !collect_statements_paths(body, branch, &mut names, budget) {
                    return None;
                }
            }
            true
        }
        Stmt::PreprocIf(if_stmt) => {
            for branch in if_stmt.branches() {
                if !collect_statements_paths(body, branch.stmts, &mut names, budget) {
                    return None;
                }
            }
            true
        }
        Stmt::While { condition, body: branch } => {
            collect_expression_paths(body, *condition, &mut names, budget)
                && collect_statements_paths(body, branch, &mut names, budget)
        }
        Stmt::For { from, to, body: branch, .. } => {
            collect_expression_paths(body, *from, &mut names, budget)
                && collect_expression_paths(body, *to, &mut names, budget)
                && collect_statements_paths(body, branch, &mut names, budget)
        }
        Stmt::ForEach { collection, body: branch, .. } => {
            collect_expression_paths(body, *collection, &mut names, budget)
                && collect_statements_paths(body, branch, &mut names, budget)
        }
        Stmt::Try { body: attempt, except } => {
            collect_statements_paths(body, attempt, &mut names, budget)
                && collect_statements_paths(body, except, &mut names, budget)
        }
        _ => {
            let mut complete = true;
            for_each_stmt_expr_root(body, stmt, &mut |root| {
                if !complete {
                    return;
                }
                if !budget.spend() || !collect_expression_paths(body, root, &mut names, budget) {
                    complete = false;
                }
            });
            complete
        }
    };
    complete.then_some(names)
}

fn collect_statements_paths(
    body: &Body,
    statements: &[StmtIdx],
    names: &mut FxHashSet<NormName>,
    budget: &mut NotifyWorkBudget<'_>,
) -> bool {
    for &stmt in statements {
        let Some(found) = stmt_path_names(body, stmt, budget) else { return false };
        for name in found {
            if !budget.spend() {
                return false;
            }
            names.insert(name);
        }
    }
    true
}

fn expression_path_names(
    body: &Body,
    expr: ExprIdx,
    budget: &mut NotifyWorkBudget<'_>,
) -> Option<FxHashSet<NormName>> {
    let mut names = FxHashSet::default();
    collect_expression_paths(body, expr, &mut names, budget).then_some(names)
}

fn collect_expression_paths(
    body: &Body,
    expr: ExprIdx,
    names: &mut FxHashSet<NormName>,
    budget: &mut NotifyWorkBudget<'_>,
) -> bool {
    if !budget.spend() {
        return false;
    }
    if let Expr::Path(name) = body.expr_idx(expr) {
        names.insert(NormName::intern(name.as_str()));
        return true;
    }
    let mut visit_child =
        |child| budget.spend() && collect_expression_paths(body, child, names, budget);
    match body.expr_idx(expr) {
        Expr::Missing | Expr::Literal(_) | Expr::Path(_) => true,
        Expr::BinaryOp { lhs, rhs, .. } => visit_child(*lhs) && visit_child(*rhs),
        Expr::UnaryOp { expr, .. } | Expr::Await { expr } => visit_child(*expr),
        Expr::Ternary { condition, then_expr, else_expr } => {
            visit_child(*condition) && visit_child(*then_expr) && visit_child(*else_expr)
        }
        Expr::Call { callee, args } => {
            if !visit_child(*callee) {
                return false;
            }
            for &arg in args.iter() {
                if !visit_child(arg) {
                    return false;
                }
            }
            true
        }
        Expr::MethodCall { receiver, args, .. } => {
            if !visit_child(*receiver) {
                return false;
            }
            for &arg in args.iter() {
                if !visit_child(arg) {
                    return false;
                }
            }
            true
        }
        Expr::Index { base, index } => visit_child(*base) && visit_child(*index),
        Expr::Field { base, .. } => visit_child(*base),
        Expr::New { args, .. } | Expr::Array(args) => {
            for &arg in args.iter() {
                if !visit_child(arg) {
                    return false;
                }
            }
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::call_resolution::{
        ArgumentApplicability, ArgumentEvaluation, ArgumentParameter, ArityFallback, ArityMismatch,
        ArityUsage, CallCandidateSet, CallParam, CallRejection, CallResolution, CallSelection,
        CallSignature, CandidateApplicability, CandidateDisposition, CandidateEvaluation,
        CandidateFact, CandidateId, CandidateOrigin, CandidateProvenance, PlatformSignatureSlot,
    };
    use bsl_types::testing::InMemoryDb;

    fn parsed(source: &str) -> (hir_def::ModuleBodies, Vec<NotifyArrayRegistration>) {
        let parse = parser::parse(source);
        let bodies = hir_def::ModuleBodies::from_parse(&parse);
        let body = bodies.iter_bodies().next().expect("the fixture declares a method").1;
        let cfg = cfg::CfgBuilder::new().build_graph_from_hir(body.body_stmts_typed(), body);
        let body_arc = std::sync::Arc::new(body.clone());
        let events = std::sync::Arc::new(hir_def::body::events::BodyEvents::collect(body));
        let reaching_defs = dataflow::reaching_defs::solve_executable_reaching_definitions(
            std::sync::Arc::new(cfg.clone()),
            &body_arc,
            events,
            true,
            dataflow::reaching_defs::RecoveredBody::Solve,
        );
        let db = bsl_types::testing::InMemoryDb::new();
        let inference = crate::infer::InferOwnerResult::ModuleCode(std::sync::Arc::new(
            crate::infer::ModuleCodeInferenceResult::default(),
        ));
        let registrations = collect_notify_array_registrations(
            &db,
            body,
            &reachable_statements(&cfg),
            reaching_defs.as_deref(),
            &inference,
            PlatformDataInner::instance(),
        );
        (bodies, registrations)
    }

    fn registrations(source: &str) -> Vec<NotifyArrayRegistration> {
        parsed(source).1
    }

    #[test]
    fn reachable_statements_match_the_production_traversal() {
        let sources = [
            r#"Процедура Test(Флаг)
    А = Новый Массив;
    Если Флаг Тогда
        А.Добавить("X");
    Иначе
        А.Добавить("Y");
    КонецЕсли;
    Пока Флаг Цикл
        Прервать;
    КонецЦикла;
КонецПроцедуры"#,
            r#"Процедура Test()
    Перейти ~Конец;
    А = Новый Массив;
    ~Конец:
    А.Добавить("X");
КонецПроцедуры"#,
            r#"Процедура Test()
    А = Новый Массив;
    Возврат;
    А.Добавить("X");
КонецПроцедуры"#,
        ];
        for source in sources {
            let parse = parser::parse(source);
            let bodies = hir_def::ModuleBodies::from_parse(&parse);
            let body = bodies.iter_bodies().next().expect("fixture declares a method").1;
            let cfg = cfg::CfgBuilder::new().build_graph_from_hir(body.body_stmts_typed(), body);
            let mut cancel = || {};
            let mut budget = NotifyWorkBudget::with_limit(&mut cancel, usize::MAX);
            assert_eq!(
                reachable_statements(&cfg),
                crate::notify_targets::reachable_statements_with_budget(&cfg, &mut budget)
                    .expect("unlimited budget"),
                "{source}"
            );
        }
    }

    fn collection_under_budget(
        source: &str,
        work_limit: usize,
        cancel: &mut dyn FnMut(),
    ) -> (Vec<NotifyArrayRegistration>, bool) {
        let parse = parser::parse(source);
        let bodies = hir_def::ModuleBodies::from_parse(&parse);
        let body = bodies.iter_bodies().next().expect("fixture declares a method").1;
        let cfg = cfg::CfgBuilder::new().build_graph_from_hir(body.body_stmts_typed(), body);
        let body_arc = std::sync::Arc::new(body.clone());
        let events = std::sync::Arc::new(hir_def::body::events::BodyEvents::collect(body));
        let reaching_defs = dataflow::reaching_defs::solve_executable_reaching_definitions(
            std::sync::Arc::new(cfg.clone()),
            &body_arc,
            events,
            true,
            dataflow::reaching_defs::RecoveredBody::Solve,
        );
        let db = InMemoryDb::new();
        let inference = crate::infer::InferOwnerResult::ModuleCode(std::sync::Arc::new(
            crate::infer::ModuleCodeInferenceResult::default(),
        ));
        let mut budget = NotifyWorkBudget::with_limit(cancel, work_limit);
        let registrations = collect_notify_array_registrations_with_budget(
            &db,
            body,
            &reachable_statements(&cfg),
            reaching_defs.as_deref(),
            &inference,
            PlatformDataInner::instance(),
            &mut budget,
        );
        (registrations, budget.exhausted())
    }

    fn budgeted_collection_exhausts(source: &str, work_limit: usize) -> bool {
        collection_under_budget(source, work_limit, &mut || {}).1
    }

    /// A body with `variables` tracked arrays initialized before `statements` reads.
    fn tracked_array_body(statements: usize, variables: usize) -> String {
        let mut source = String::from("Процедура Test()\n");
        for index in 0..variables {
            source.push_str(&format!("    Перем A{index};\n"));
        }
        for index in 0..variables {
            source.push_str(&format!("    A{index} = Новый Массив;\n"));
        }
        for _ in 0..statements {
            source.push_str("    Значение = A0;\n");
        }
        source.push_str("КонецПроцедуры\n");
        source
    }

    #[test]
    fn wide_tracked_variable_sets_do_not_multiply_the_statement_scan() {
        let wide = tracked_array_body(16, 64);

        assert!(
            !budgeted_collection_exhausts(&wide, 2_000),
            "the write scan must follow a statement's write events, not the tracked-variable count"
        );
    }

    #[test]
    fn statement_writes_charge_the_scan_per_written_name() {
        // Each `Выполнить` makes a possible write for every name of the body, so the
        // scan charges per written name even though no array is ever read here.
        let mut source = String::from("Процедура Test()\n");
        for index in 0..64 {
            source.push_str(&format!("    Перем A{index};\n"));
        }
        for index in 0..64 {
            source.push_str(&format!("    A{index} = Новый Массив;\n"));
        }
        for _ in 0..32 {
            source.push_str("    Выполнить(\"А = 1\");\n");
        }
        source.push_str("КонецПроцедуры\n");

        assert!(
            budgeted_collection_exhausts(&source, 2_000),
            "implicit writes must still spend from the shared budget"
        );
    }

    #[test]
    fn exhausted_collection_returns_only_the_proofs_before_the_stop() {
        let mut source = String::from(
            r#"Процедура Test()
    Перем Параметры;
    Перем Оповещение;
    Параметры = Новый Массив;
    Параметры.Добавить("Готово");
    Параметры.Добавить(ЭтотОбъект);
    Оповещение = Новый("CallbackDescription", Параметры);
"#,
        );
        for index in 0..128 {
            source.push_str(&format!("    Перем A{index};\n"));
        }
        for index in 0..128 {
            source.push_str(&format!("    A{index} = Новый Массив;\n"));
        }
        source.push_str("КонецПроцедуры\n");

        let (registrations, exhausted) = collection_under_budget(&source, 1_500, &mut || {});
        assert!(exhausted, "the tail must exhaust the budget");
        assert_eq!(registrations.len(), 1, "the head proof must survive the later stop");
        assert_eq!(registrations[0].handlers.len(), 1);
        assert_eq!(registrations[0].handlers[0].callback_name.as_str(), "Готово");
    }

    #[test]
    fn array_collection_checks_cancellation_while_spending() {
        let source = tracked_array_body(256, 2);
        let mut cancellations = 0;
        let (_, exhausted) = collection_under_budget(&source, 100_000, &mut || cancellations += 1);

        assert!(!exhausted, "the fixture must fit the budget");
        assert!(
            cancellations > 0,
            "the array pass must reach the shared cancellation check while spending"
        );
    }

    #[test]
    fn exact_local_array_produces_a_callback_handler() {
        let (bodies, found) = parsed(
            r#"
Процедура Test()
    Перем Параметры;
    Перем Оповещение;
    Параметры = Новый Массив;
    Параметры.Добавить("Готово");
    Параметры.Добавить(ЭтотОбъект);
    Оповещение = Новый("CallbackDescription", Параметры);
КонецПроцедуры
"#,
        );

        let body = bodies.iter_bodies().next().expect("the fixture declares a method").1;
        assert_eq!(found.len(), 1, "{found:#?}");
        assert_eq!(found[0].handlers.len(), 1, "{found:#?}");
        assert_eq!(found[0].handlers[0].callback_name.as_str(), "Готово");
        assert_eq!(found[0].handlers[0].receiver_syntax, NotifyReceiverSyntax::SelfName);
        let receiver = found[0].handlers[0].receiver.expect("receiver comes from the array");
        assert!(matches!(
            body.expr(receiver),
            Expr::Path(name) if name.as_str() == "ЭтотОбъект"
        ));
    }

    #[test]
    fn exact_array_uses_the_platform_fourth_and_fifth_constructor_parameters() {
        let found = registrations(
            r#"
Procedure Test()
    Перем Parameters;
    Перем Notification;
    Parameters = New Array;
    Parameters.Add("Done");
    Parameters.Add(ThisObject);
    Parameters.Add(Extra);
    Parameters.Add("Failed");
    Parameters.Add(ErrorModule);
    Notification = New("NotifyDescription", Parameters);
EndProcedure
"#,
        );

        assert_eq!(found.len(), 1, "{found:#?}");
        assert_eq!(found[0].handlers.len(), 2, "{found:#?}");
        assert_eq!(found[0].handlers[0].callback_name.as_str(), "Done");
        assert_eq!(found[0].handlers[0].receiver_syntax, NotifyReceiverSyntax::SelfName);
        assert_eq!(found[0].handlers[1].callback_name.as_str(), "Failed");
        assert_eq!(found[0].handlers[1].receiver_syntax, NotifyReceiverSyntax::BareName);
    }

    #[test]
    fn receiver_syntax_comes_from_the_value_added_to_the_array() {
        let found = registrations(
            r#"
Процедура Test()
    Перем Параметры;
    Перем Оповещение;
    Параметры = Новый Массив;
    Параметры.Добавить("Готово");
    Параметры.Добавить(ПолучитьОбъект());
    Оповещение = Новый("ОписаниеОповещения", Параметры);
КонецПроцедуры
"#,
        );

        assert_eq!(found.len(), 1, "{found:#?}");
        assert_eq!(found[0].handlers[0].receiver_syntax, NotifyReceiverSyntax::Other);
    }

    #[test]
    fn later_mutation_does_not_change_the_array_at_the_constructor_read() {
        let found = registrations(
            r#"
Процедура Test()
    Перем Параметры;
    Перем Оповещение;
    Параметры = Новый Массив;
    Параметры.Добавить("Готово");
    Параметры.Добавить(ЭтотОбъект);
    Оповещение = Новый("CallbackDescription", Параметры);
    Параметры.Добавить("Позже");
КонецПроцедуры
"#,
        );

        assert_eq!(found.len(), 1, "{found:#?}");
    }

    #[test]
    fn aliases_reassignments_unknown_mutations_and_unknown_calls_open_the_array() {
        let cases = [
            (
                "array parameter is unknown",
                r#"Процедура Test(Параметры)
Параметры.Добавить("Готово");
О = Новый("CallbackDescription", Параметры);
КонецПроцедуры"#,
            ),
            (
                "array without a unique local declaration is unknown",
                r#"Процедура Test()
Параметры = Новый Массив;
Параметры.Добавить("Готово");
О = Новый("CallbackDescription", Параметры);
КонецПроцедуры"#,
            ),
            (
                "alias escapes the tracked identity",
                r#"Процедура Test()
Перем Параметры;
Перем Алиас;
Параметры = Новый Массив;
Параметры.Добавить("Готово");
Параметры.Добавить(ЭтотОбъект);
Алиас = Параметры;
О = Новый("CallbackDescription", Параметры);
КонецПроцедуры"#,
            ),
            (
                "reassignment invalidates prior contents",
                r#"Процедура Test()
Перем Параметры;
Параметры = Новый Массив;
Параметры.Добавить("Готово");
Параметры.Добавить(ЭтотОбъект);
Параметры = ПолучитьМассив();
О = Новый("CallbackDescription", Параметры);
КонецПроцедуры"#,
            ),
            (
                "unknown mutation invalidates prior contents",
                r#"Процедура Test()
Перем Параметры;
Параметры = Новый Массив;
Параметры.Добавить("Готово");
Параметры.Добавить(ЭтотОбъект);
Параметры.Изменить();
О = Новый("CallbackDescription", Параметры);
КонецПроцедуры"#,
            ),
            (
                "passing the array to an unknown call may mutate it by reference",
                r#"Процедура Test()
Перем Параметры;
Параметры = Новый Массив;
Параметры.Добавить("Готово");
Параметры.Добавить(ЭтотОбъект);
Изменить(Параметры);
О = Новый("CallbackDescription", Параметры);
КонецПроцедуры"#,
            ),
            (
                "a possible write through a nested call invalidates the array",
                r#"Процедура Test()
Перем Параметры;
Параметры = Новый Массив;
Параметры.Добавить("Готово");
Параметры.Добавить(ЭтотОбъект);
Результат = Изменить(Параметры);
О = Новый("CallbackDescription", Параметры);
КонецПроцедуры"#,
            ),
            (
                "a possible write through a differently cased Greek name invalidates the array",
                r#"Процедура Test()
Перем ΟΣ;
ΟΣ = Новый Массив;
ΟΣ.Добавить("Готово");
ΟΣ.Добавить(ЭтотОбъект);
Результат = Изменить(οσ);
О = Новый("CallbackDescription", ΟΣ);
КонецПроцедуры"#,
            ),
        ];

        for (reason, source) in cases {
            assert!(registrations(source).is_empty(), "{reason}");
        }
    }

    #[test]
    fn greek_sigma_case_variants_share_array_identity() {
        let found = registrations(
            r#"
Процедура Test()
    Перем ΟΣ;
    ΟΣ = Новый Массив;
    ΟΣ.Добавить("Нет");
    ΟΣ.Добавить(ЭтотОбъект);
    οσ = Новый Массив;
    О = Новый("CallbackDescription", ΟΣ);
КонецПроцедуры
"#,
        );

        assert!(found.is_empty(), "a reassignment through οσ must invalidate ΟΣ contents");
    }

    #[test]
    fn execute_may_write_a_tracked_array_without_naming_it_syntactically() {
        let source = r#"
Процедура Test()
    Перем Параметры;
    Параметры = Новый Массив;
    Параметры.Добавить("Готово");
    Параметры.Добавить(ЭтотОбъект);
    Выполнить("Параметры = Новый Массив");
    О = Новый("CallbackDescription", Параметры);
КонецПроцедуры
"#;

        assert!(registrations(source).is_empty());
    }

    #[test]
    fn execute_may_write_array_under_contextual_case_fold_spelling() {
        let source = r#"
Процедура Test()
    Перем ΟΣ;
    ΟΣ = Новый Массив;
    ΟΣ.Добавить("Готово");
    ΟΣ.Добавить(ЭтотОбъект);
    Выполнить("ОС = Новый Массив");
    О = Новый("CallbackDescription", ΟΣ);
КонецПроцедуры
"#;

        assert!(registrations(source).is_empty());
    }

    #[test]
    fn one_execute_opens_every_array_it_may_write() {
        // The literal names no array syntactically, yet the write events of one `Выполнить`
        // cover both tracked arrays: the point scan must open each of them.
        let source = r#"
Процедура Test()
    Перем П1;
    Перем П2;
    П1 = Новый Массив;
    П1.Добавить("Первый");
    П1.Добавить(ЭтотОбъект);
    П2 = Новый Массив;
    П2.Добавить("Второй");
    П2.Добавить(ЭтотОбъект);
    Выполнить("П1 = Неопределено");
    О1 = Новый("CallbackDescription", П1);
    О2 = Новый("CallbackDescription", П2);
КонецПроцедуры
"#;

        assert!(registrations(source).is_empty());
    }

    #[test]
    fn an_argument_write_hidden_in_an_add_argument_opens_that_array() {
        // `П1.Добавить(Изменить(П2))`: П2 is part of the Add argument, hidden from the
        // syntactic use scan, so only its write event at this point can open it.
        let source = r#"
Процедура Test()
    Перем П1;
    Перем П2;
    П1 = Новый Массив;
    П2 = Новый Массив;
    П2.Добавить("Второй");
    П2.Добавить(ЭтотОбъект);
    П1.Добавить(Изменить(П2));
    О2 = Новый("CallbackDescription", П2);
КонецПроцедуры
"#;

        assert!(registrations(source).is_empty());
    }

    #[test]
    fn an_earlier_registration_is_not_changed_by_later_execute() {
        let found = registrations(
            r#"
Процедура Test()
    Перем Параметры;
    Перем Оповещение;
    Параметры = Новый Массив;
    Параметры.Добавить("Готово");
    Параметры.Добавить(ЭтотОбъект);
    Оповещение = Новый("CallbackDescription", Параметры);
    Выполнить("Параметры.Добавить(1)");
КонецПроцедуры
"#,
        );

        assert_eq!(found.len(), 1, "{found:#?}");
    }

    #[test]
    fn conditional_loop_and_try_mutations_are_unknown() {
        let cases = [
            r#"Процедура Test()
Перем Параметры;
Параметры = Новый Массив;
Если Истина Тогда
    Параметры.Добавить("Готово");
КонецЕсли;
О = Новый("CallbackDescription", Параметры);
КонецПроцедуры"#,
            r#"Процедура Test()
Перем Параметры;
Параметры = Новый Массив;
Пока Ложь Цикл
    Параметры.Добавить("Готово");
КонецЦикла;
О = Новый("CallbackDescription", Параметры);
КонецПроцедуры"#,
            r#"Процедура Test()
Перем Параметры;
Параметры = Новый Массив;
Попытка
    Параметры.Добавить("Готово");
Исключение
КонецПопытки;
О = Новый("CallbackDescription", Параметры);
КонецПроцедуры"#,
        ];
        for source in cases {
            assert!(registrations(source).is_empty(), "{source}");
        }
    }

    #[test]
    fn goto_and_labels_conservatively_break_array_proofs() {
        let cases = [
            r#"Процедура Test()
Перем Параметры;
Параметры = Новый Массив;
Параметры.Добавить("Готово");
Параметры.Добавить(ЭтотОбъект);
Перейти ~Готово;
Параметры.Добавить("Недостижимо");
~Готово:
О = Новый("CallbackDescription", Параметры);
КонецПроцедуры"#,
            r#"Процедура Test()
Перем Параметры;
Перейти ~Продолжение;
Параметры = Новый Массив;
~Продолжение:
Параметры.Добавить("Готово");
Параметры.Добавить(ЭтотОбъект);
О = Новый("CallbackDescription", Параметры);
КонецПроцедуры"#,
        ];
        for source in cases {
            assert!(registrations(source).is_empty(), "{source}");
        }
    }

    #[test]
    fn statements_after_return_do_not_change_reachable_array_contents() {
        let found = registrations(
            r#"
Процедура Test()
    Перем Параметры;
    Перем Оповещение;
    Параметры = Новый Массив;
    Параметры.Добавить("Готово");
    Параметры.Добавить(ЭтотОбъект);
    Оповещение = Новый("CallbackDescription", Параметры);
    Возврат;
    Параметры.Изменить();
КонецПроцедуры
"#,
        );
        assert_eq!(found.len(), 1, "{found:#?}");
    }

    #[test]
    fn no_notify_candidate_is_created_for_an_unresolved_type_or_extra_constructor_argument() {
        let cases = [
            r#"Процедура Test()
Перем Параметры;
Параметры = Новый Массив;
Параметры.Добавить("Готово");
Параметры.Добавить(ЭтотОбъект);
О = Новый("UnknownDescription", Параметры);
КонецПроцедуры"#,
            r#"Процедура Test()
Перем Параметры;
Параметры = Новый Массив;
Параметры.Добавить("Готово");
Параметры.Добавить(ЭтотОбъект);
О = Новый("CallbackDescription", Параметры, Лишний);
КонецПроцедуры"#,
            r#"Процедура Test()
Перем Параметры;
Параметры = Новый Массив;
Параметры.Добавить("Готово");
Параметры.Добавить(ЭтотОбъект);
Параметры.Добавить(Парам);
Параметры.Добавить(Ошибка);
Параметры.Добавить(ЭтотОбъект);
Параметры.Добавить(Лишний);
О = Новый("CallbackDescription", Параметры);
КонецПроцедуры"#,
        ];
        for source in cases {
            assert!(registrations(source).is_empty(), "{source}");
        }
    }

    #[test]
    fn callback_arguments_follow_evaluation_mapping_for_unique_and_rejected_type_calls() {
        let schema = [
            "ДополнительныеПараметры",
            "Модуль",
            "ИмяПроцедуры",
            "МодульОбработкиОшибки",
            "ИмяПроцедурыОбработкиОшибки",
        ];
        let unique = binding_for_schemas(&[schema], TestResolution::Unique);
        let expected = CallbackArguments {
            handler: Some(2),
            receiver: Some(1),
            error_handler: Some(4),
            error_receiver: Some(3),
        };
        assert_eq!(callback_arguments_for_binding(&unique), Some(expected));

        let rejected = binding_for_schemas(&[schema], TestResolution::TypeRejected);
        assert_eq!(callback_arguments_for_binding(&rejected), Some(expected));
    }

    #[test]
    fn callback_argument_projection_uses_the_callers_budget() {
        let schema = [
            "ДополнительныеПараметры",
            "Модуль",
            "ИмяПроцедуры",
            "МодульОбработкиОшибки",
            "ИмяПроцедурыОбработкиОшибки",
        ];
        let binding = binding_for_schemas(&[schema; 32], TestResolution::Ambiguous);
        let mut checks = 0;
        let mut cancel = || checks += 1;
        let mut budget = NotifyWorkBudget::with_limit(&mut cancel, 200);
        let _ = callback_arguments_for_binding_with_budget(&binding, &mut budget);
        assert!(budget.exhausted());
        assert!(checks > 0, "long candidate projections must check cancellation");
    }

    #[test]
    fn ambiguous_constructor_schemas_require_the_same_argument_projection() {
        let schema = [
            "ДополнительныеПараметры",
            "Модуль",
            "ИмяПроцедуры",
            "МодульОбработкиОшибки",
            "ИмяПроцедурыОбработкиОшибки",
        ];
        let same_schema = binding_for_schemas(&[schema, schema], TestResolution::Ambiguous);
        assert_eq!(
            callback_arguments_for_binding(&same_schema),
            Some(CallbackArguments {
                handler: Some(2),
                receiver: Some(1),
                error_handler: Some(4),
                error_receiver: Some(3),
            })
        );

        let different_schema = binding_for_schemas(
            &[
                schema,
                [
                    "ИмяПроцедуры",
                    "Модуль",
                    "ДополнительныеПараметры",
                    "ИмяПроцедурыОбработкиОшибки",
                    "МодульОбработкиОшибки",
                ],
            ],
            TestResolution::Ambiguous,
        );
        assert_eq!(callback_arguments_for_binding(&different_schema), None);
    }

    #[test]
    fn arity_rejected_constructor_has_no_callback_argument_projection() {
        let schema = [
            "ИмяПроцедуры",
            "Модуль",
            "ДополнительныеПараметры",
            "ИмяПроцедурыОбработкиОшибки",
            "МодульОбработкиОшибки",
        ];
        let rejected = binding_for_schemas(&[schema], TestResolution::ArityRejected);
        assert_eq!(callback_arguments_for_binding(&rejected), None);
    }

    #[derive(Clone, Copy)]
    enum TestResolution {
        Unique,
        Ambiguous,
        TypeRejected,
        ArityRejected,
    }

    fn binding_for_schemas(
        schemas: &[[&str; 5]],
        resolution_kind: TestResolution,
    ) -> CallArgBinding {
        let db = InMemoryDb::new();
        let unknown = db.unknown();
        let ids = schemas
            .iter()
            .enumerate()
            .map(|(index, _)| CandidateId::Platform {
                method_id: index as u32 + 1,
                signature: PlatformSignatureSlot::Base,
            })
            .collect::<Vec<_>>();
        let signatures = schemas
            .iter()
            .enumerate()
            .map(|(index, schema)| CallSignature {
                id: ids[index],
                params: schema
                    .iter()
                    .map(|name| CallParam {
                        name: (*name).into(),
                        ty: unknown,
                        has_default: true,
                        mode: CallParamMode::Positional,
                        passing: None,
                        result_follows_argument_text: false,
                    })
                    .collect::<Vec<_>>()
                    .into_boxed_slice(),
                required_args: 0,
                max_args: Some(5),
                return_ty: unknown,
                origin: CandidateOrigin::Platform,
                environment: EnvFlags::ALL,
                provenance: CandidateProvenance::PlatformMethod {
                    method_id: index as u32 + 1,
                    signature: PlatformSignatureSlot::Base,
                },
                from_doc_comment: false,
            })
            .collect::<Vec<_>>();
        let candidates = CallCandidateSet::try_from(signatures).expect("unique candidate ids");
        let facts = ids
            .iter()
            .map(|&id| {
                let (evaluation, disposition) = match resolution_kind {
                    TestResolution::ArityRejected => {
                        let mismatch = ArityMismatch::TooMany { actual: 6, maximum: 5 };
                        (
                            CandidateEvaluation::ArityIncompatible(mismatch),
                            CandidateDisposition::Rejected(
                                crate::call_resolution::CandidateRejection::Arity(mismatch),
                            ),
                        )
                    }
                    TestResolution::TypeRejected => (
                        CandidateEvaluation::ArityCompatible {
                            usage: ArityUsage {
                                actual: 5,
                                defaults_used: 0,
                                variadic_arguments: 0,
                            },
                            applicability: CandidateApplicability::Incompatible {
                                arguments: (0..5)
                                    .map(|index| ArgumentEvaluation {
                                        index,
                                        argument_ty: unknown,
                                        parameter: ArgumentParameter::Declared {
                                            index,
                                            mode: CallParamMode::Positional,
                                            ty: unknown,
                                        },
                                        applicability: ArgumentApplicability::Incompatible,
                                    })
                                    .collect(),
                            },
                        },
                        CandidateDisposition::Rejected(
                            crate::call_resolution::CandidateRejection::TypeIncompatible {
                                arguments: (0..5)
                                    .map(|index| ArgumentEvaluation {
                                        index,
                                        argument_ty: unknown,
                                        parameter: ArgumentParameter::Declared {
                                            index,
                                            mode: CallParamMode::Positional,
                                            ty: unknown,
                                        },
                                        applicability: ArgumentApplicability::Incompatible,
                                    })
                                    .collect(),
                            },
                        ),
                    ),
                    TestResolution::Unique | TestResolution::Ambiguous => (
                        CandidateEvaluation::ArityCompatible {
                            usage: ArityUsage {
                                actual: 5,
                                defaults_used: 0,
                                variadic_arguments: 0,
                            },
                            applicability: CandidateApplicability::Applicable {
                                arguments: (0..5)
                                    .map(|index| ArgumentEvaluation {
                                        index,
                                        argument_ty: unknown,
                                        parameter: ArgumentParameter::Declared {
                                            index,
                                            mode: CallParamMode::Positional,
                                            ty: unknown,
                                        },
                                        applicability: ArgumentApplicability::Exact,
                                    })
                                    .collect(),
                            },
                        },
                        CandidateDisposition::Survivor,
                    ),
                };
                CandidateFact { id, return_ty: unknown, evaluation, score: None, disposition }
            })
            .collect::<Vec<_>>()
            .into_boxed_slice();
        let selection = match resolution_kind {
            TestResolution::Unique => CallSelection::Unique { candidate: ids[0] },
            TestResolution::Ambiguous => {
                CallSelection::Ambiguous { candidates: ids.clone().into_boxed_slice() }
            }
            TestResolution::TypeRejected => CallSelection::Rejected(CallRejection::Type),
            TestResolution::ArityRejected => {
                let mismatch = ArityMismatch::TooMany { actual: 6, maximum: 5 };
                CallSelection::Rejected(CallRejection::Arity {
                    fallback: ArityFallback { candidate: ids[0], mismatch, distance: 1 },
                })
            }
        };
        let resolution = CallResolution { candidates: facts, selection, return_ty: unknown };
        CallArgBinding {
            owner: hir_def::DefWithBodyId::ModuleCode,
            call_expr: ExprId::from_raw(0u32.into()),
            args: (0..5).map(|index| ExprId::from_raw((index as u32).into())).collect(),
            candidate: crate::infer::CandidateCallBinding { candidates, resolution },
            in_dead_branch: false,
        }
    }
}
