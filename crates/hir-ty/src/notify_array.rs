//! Proof-oriented extraction of notification handlers from a locally-built argument array.
//!
//! Dynamic `Новый("Тип", МассивПараметров)` receives one array, not positional callback arguments.
//! This module only exposes handlers when a body-local array has a unique declaration, a known
//! `Массив` construction, and an unbroken straight-line sequence of platform `Добавить` calls up
//! to the exact constructor read. Any other use that could alias or mutate the array opens the
//! state. Type and constructor facts come from the platform snapshot, not syntax lowering.

use bsl_platform::PlatformDataInner;
use cfg::{CfgEdgeType, CfgVertex, ControlFlowGraph};
use cfg_types::{ExprId, IdConversion};
use hir_def::body::events::{for_each_expr_child, for_each_stmt_expr_root, EvalPoint};
use hir_def::body::Body;
use hir_def::call_graph::NotifyReceiverSyntax;
use hir_def::execution_env::EnvFlags;
use hir_def::hir::{Expr, ExprIdx, Literal, Stmt, StmtIdx};
use hir_def::Name;
use intern::NormName;
use rustc_hash::{FxHashMap, FxHashSet};
use stdx::case::eq_ignore_case;

use crate::call_resolution::{
    CallParamMode, CallRejection, CallSelection, CandidateApplicability, CandidateDisposition,
    CandidateEvaluation, CandidateRejection,
};
use crate::infer::{CallArgBinding, InferOwnerResult};
use bsl_types::builders::Builders;
use bsl_types::intern::TypeKernelDb;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NotifyArrayHandler {
    pub constructor: ExprId,
    pub callback_name: Name,
    pub callback_expr: ExprId,
    pub receiver: Option<ExprId>,
    pub receiver_syntax: NotifyReceiverSyntax,
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
pub(crate) fn collect_notify_array_registrations(
    db: &dyn TypeKernelDb,
    body: &Body,
    cfg: &ControlFlowGraph,
    reaching_defs: Option<&dataflow::reaching_defs::ReachingDefsResult>,
    inference: &InferOwnerResult,
    platform: &PlatformDataInner,
) -> Vec<NotifyArrayRegistration> {
    if body.is_lowering_incomplete() {
        return Vec::new();
    }
    let Some(reaching_defs) = reaching_defs else {
        return Vec::new();
    };

    let reachable_stmts = reachable_statements(cfg);
    let name_spellings = local_name_spellings(body);
    let local_declarations = unique_local_declarations(body);
    let mut states: FxHashMap<NormName, ArrayState> =
        local_declarations.keys().map(|&name| (name, ArrayState::Uninitialized)).collect();
    let mut used_before_initialization = FxHashSet::default();
    let mut registrations = Vec::new();

    for (position, &stmt_idx) in body.body_stmts_typed().iter().enumerate() {
        let stmt_id = cfg_types::StmtId::from_idx(stmt_idx);
        if !reachable_stmts.contains(&stmt_id) {
            continue;
        }
        let stmt = body.stmt_idx(stmt_idx);
        if is_control_flow(stmt) {
            let mentioned = stmt_path_names(body, stmt_idx);
            for name in mentioned {
                if matches!(states.get(&name), Some(ArrayState::Uninitialized)) {
                    used_before_initialization.insert(name);
                }
            }
            for state in states.values_mut() {
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
                    if is_empty_array_constructor(body, *value, platform) {
                        let state = if *declaration_position < position
                            && !used_before_initialization.contains(&key)
                            && matches!(states.get(&key), Some(ArrayState::Uninitialized))
                        {
                            ArrayState::Known(Vec::new())
                        } else {
                            ArrayState::Unknown
                        };
                        states.insert(key, state);
                        invalidate_possible_writes(
                            &mut states,
                            &name_spellings,
                            reaching_defs,
                            stmt_id,
                        );
                        continue;
                    }
                }

                if let Some((constructor, array_name)) =
                    dynamic_notify_array_call(body, *value, platform)
                {
                    let array_key = NormName::intern(array_name.as_str());
                    if let Some(ArrayState::Known(elements)) = states.get(&array_key) {
                        if let Some(binding) = dynamic_callback_binding(
                            db,
                            inference,
                            body,
                            constructor,
                            elements,
                            platform,
                        ) {
                            let handlers = handlers_from_binding(body, constructor, &binding);
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

                let mut mentioned = stmt_path_names(body, stmt_idx);
                mentioned.remove(&key);
                if let Some((_, array_name)) = dynamic_notify_array_call(body, *value, platform) {
                    mentioned.remove(&NormName::intern(array_name.as_str()));
                }
                invalidate_or_record_uses(&mut states, &mut used_before_initialization, mentioned);
                invalidate_possible_writes(&mut states, &name_spellings, reaching_defs, stmt_id);
                continue;
            }

            if let Some((constructor, array_name)) =
                dynamic_notify_array_call(body, *value, platform)
            {
                let array_key = NormName::intern(array_name.as_str());
                if let Some(ArrayState::Known(elements)) = states.get(&array_key) {
                    if let Some(binding) = dynamic_callback_binding(
                        db,
                        inference,
                        body,
                        constructor,
                        elements,
                        platform,
                    ) {
                        let handlers = handlers_from_binding(body, constructor, &binding);
                        if !handlers.is_empty() {
                            registrations.push(NotifyArrayRegistration { constructor, handlers });
                        }
                    }
                }

                let mut mentioned = stmt_path_names(body, stmt_idx);
                mentioned.remove(&array_key);
                invalidate_or_record_uses(&mut states, &mut used_before_initialization, mentioned);
                invalidate_possible_writes(&mut states, &name_spellings, reaching_defs, stmt_id);
                continue;
            }
        }

        if let Stmt::Expr(expr) = stmt {
            if let Some((receiver_name, argument)) = array_add_statement(body, *expr, platform) {
                let key = NormName::intern(receiver_name.as_str());
                let argument_names = expression_path_names(body, argument);
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

                invalidate_or_record_uses(
                    &mut states,
                    &mut used_before_initialization,
                    argument_names,
                );
                let mut other_names = stmt_path_names(body, stmt_idx);
                other_names.remove(&key);
                for name in expression_path_names(body, argument) {
                    other_names.remove(&name);
                }
                invalidate_or_record_uses(
                    &mut states,
                    &mut used_before_initialization,
                    other_names,
                );
                invalidate_possible_writes(&mut states, &name_spellings, reaching_defs, stmt_id);
                continue;
            }
        }

        invalidate_or_record_uses(
            &mut states,
            &mut used_before_initialization,
            stmt_path_names(body, stmt_idx),
        );
        invalidate_possible_writes(&mut states, &name_spellings, reaching_defs, stmt_id);
    }

    registrations
}

fn invalidate_possible_writes(
    states: &mut FxHashMap<NormName, ArrayState>,
    name_spellings: &FxHashMap<NormName, Vec<Name>>,
    reaching_defs: &dataflow::reaching_defs::ReachingDefsResult,
    stmt: cfg_types::StmtId,
) {
    for (name, state) in states {
        if matches!(state, ArrayState::Known(_))
            && name_spellings.get(name).is_some_and(|spellings| {
                spellings.iter().any(|spelling| {
                    !reaching_defs
                        .possible_writes_at(EvalPoint::Stmt(stmt), spelling.as_str())
                        .is_empty()
                })
            })
        {
            *state = ArrayState::Unknown;
        }
    }
}

fn local_name_spellings(body: &Body) -> FxHashMap<NormName, Vec<Name>> {
    let mut spellings: FxHashMap<NormName, Vec<Name>> = FxHashMap::default();
    let mut add_spelling = |name: &Name| {
        let names = spellings.entry(NormName::intern(name.as_str())).or_default();
        if !names.iter().any(|existing| existing == name) {
            names.push(name.clone());
        }
    };
    for (_, binding) in body.bindings_iter() {
        add_spelling(&binding.name);
    }
    for (_, expr) in body.exprs_iter() {
        if let Expr::Path(name) = expr {
            add_spelling(name);
        }
    }
    spellings
}

fn unique_local_declarations(body: &Body) -> FxHashMap<NormName, (usize, Name)> {
    let mut binding_counts: FxHashMap<NormName, usize> = FxHashMap::default();
    for (_, binding) in body.bindings_iter() {
        *binding_counts.entry(NormName::intern(binding.name.as_str())).or_default() += 1;
    }

    let parameters: FxHashSet<NormName> =
        body.params().map(|id| NormName::intern(body.binding(id).name.as_str())).collect();
    let mut declarations: FxHashMap<NormName, Vec<(usize, Name)>> = FxHashMap::default();
    for (position, &stmt_idx) in body.body_stmts_typed().iter().enumerate() {
        let Stmt::VarDecl { bindings } = body.stmt_idx(stmt_idx) else { continue };
        for &binding in bindings.iter() {
            let binding = body.binding_idx(binding);
            let name = NormName::intern(binding.name.as_str());
            declarations.entry(name).or_default().push((position, binding.name.clone()));
        }
    }

    declarations
        .into_iter()
        .filter_map(|(name, declarations)| {
            (declarations.len() == 1
                && binding_counts.get(&name) == Some(&1)
                && !parameters.contains(&name))
            .then(|| (name, declarations.into_iter().next().expect("one declaration")))
        })
        .collect()
}

fn is_empty_array_constructor(body: &Body, expr: ExprIdx, platform: &PlatformDataInner) -> bool {
    let Expr::New { type_name: Some(type_name), args } = body.expr_idx(expr) else {
        return false;
    };
    if !args.is_empty() || !resolves_to_type(platform, type_name.as_str(), "Array") {
        return false;
    }
    platform.get_constructors(type_name.as_str()).iter().any(|constructor| {
        constructor.parameters.iter().all(|param| param.is_optional || param.is_variadic)
    })
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
) -> Option<CallArgBinding> {
    let Expr::New { type_name: None, args } = body.expr(constructor) else { return None };
    let Expr::Literal(Literal::String(type_name)) = body.expr_idx(*args.first()?) else {
        return None;
    };
    let constructor_refs = platform.get_constructors(type_name).to_vec();
    let (definite, possible) = inference
        .notify_constructor_registration_envs(constructor)
        .unwrap_or((EnvFlags::EMPTY, EnvFlags::ALL));
    let environment = definite | possible;
    let environment = if environment.is_empty() { EnvFlags::ALL } else { environment };
    let candidates =
        crate::builtin::candidates::constructor_candidates(db, &constructor_refs, environment)
            .ok()?;
    let argument_types = elements
        .iter()
        .map(|&expr| inference.type_id_of_expr(expr).unwrap_or_else(|| db.unknown()))
        .collect::<Vec<_>>();
    let candidate = crate::call_binding::resolve_binding(db, candidates, &argument_types).semantic;
    Some(CallArgBinding {
        owner: inference.owner(),
        call_expr: constructor,
        args: elements.to_vec(),
        candidate,
        in_dead_branch: false,
    })
}

pub(crate) fn callback_arguments_for_binding(
    binding: &CallArgBinding,
) -> Option<CallbackArguments> {
    let resolution = &binding.candidate.resolution;
    let candidate_ids = match &resolution.selection {
        CallSelection::Unique { candidate } => vec![*candidate],
        CallSelection::Ambiguous { candidates } => candidates.to_vec(),
        CallSelection::Rejected(CallRejection::Type) => resolution
            .candidates
            .iter()
            .filter_map(|fact| {
                matches!(
                    &fact.disposition,
                    CandidateDisposition::Rejected(CandidateRejection::TypeIncompatible { .. })
                )
                .then_some(fact.id)
            })
            .collect(),
        CallSelection::Rejected(CallRejection::Arity { .. } | CallRejection::NoCandidates) => {
            return None;
        }
    };

    let candidates = binding.candidate.candidates.as_slice();
    let mut projection = None;
    for candidate_id in candidate_ids {
        let fact = resolution.candidates.iter().find(|fact| fact.id == candidate_id)?;
        let signature = candidates.iter().find(|signature| signature.id == candidate_id)?;
        let current = callback_argument_projection(fact, signature)?;
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
) -> Option<CallbackArguments> {
    let CandidateEvaluation::ArityCompatible { applicability, .. } = &fact.evaluation else {
        return None;
    };
    let evaluations = match applicability {
        CandidateApplicability::Applicable { arguments }
        | CandidateApplicability::Indeterminate { arguments }
        | CandidateApplicability::Incompatible { arguments } => arguments.as_ref(),
    };
    let parameters = signature
        .params
        .iter()
        .map(|parameter| bsl_platform::MethodParam {
            name: parameter.name.clone(),
            param_type: None,
            is_optional: parameter.has_default,
            is_variadic: parameter.mode == CallParamMode::Variadic,
            passing: parameter.passing,
            result_follows_argument_text: parameter.result_follows_argument_text,
        })
        .collect::<Vec<_>>();
    let slots = bsl_platform::callback_description_slots_for_parameters(&parameters)?;
    let argument_for_parameter = |parameter_index| {
        let mut matches = evaluations.iter().filter_map(|argument| match argument.parameter {
            crate::call_resolution::ArgumentParameter::Declared { index, .. }
                if index == parameter_index =>
            {
                Some(argument.index)
            }
            _ => None,
        });
        let argument = matches.next();
        argument.filter(|_| matches.next().is_none())
    };
    Some(CallbackArguments {
        handler: argument_for_parameter(slots.handler),
        receiver: argument_for_parameter(slots.receiver),
        error_handler: argument_for_parameter(slots.error_handler),
        error_receiver: argument_for_parameter(slots.error_receiver),
    })
}

fn handlers_from_binding(
    body: &Body,
    constructor: ExprId,
    binding: &CallArgBinding,
) -> Vec<NotifyArrayHandler> {
    let Some(arguments) = callback_arguments_for_binding(binding) else { return Vec::new() };
    let mut handlers = Vec::with_capacity(2);
    for (handler, receiver) in [
        (arguments.handler, arguments.receiver),
        (arguments.error_handler, arguments.error_receiver),
    ] {
        if let Some(handler) =
            handler_from_bound_arguments(body, constructor, binding, handler, receiver)
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
    })
}

fn array_add_statement(
    body: &Body,
    expr: ExprIdx,
    platform: &PlatformDataInner,
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
) {
    for name in mentioned {
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
}

fn stmt_path_names(body: &Body, stmt: StmtIdx) -> FxHashSet<NormName> {
    let mut names = FxHashSet::default();
    match body.stmt_idx(stmt) {
        Stmt::If(if_stmt) => {
            collect_expression_paths(body, if_stmt.condition, &mut names);
            for (_, branch) in if_stmt.elsif_branches.iter() {
                collect_statements_paths(body, branch, &mut names);
            }
            collect_statements_paths(body, &if_stmt.then_branch, &mut names);
            if let Some(branch) = &if_stmt.else_branch {
                collect_statements_paths(body, branch, &mut names);
            }
        }
        Stmt::PreprocIf(if_stmt) => {
            for branch in if_stmt.branches() {
                collect_statements_paths(body, branch.stmts, &mut names);
            }
        }
        Stmt::While { condition, body: branch } => {
            collect_expression_paths(body, *condition, &mut names);
            collect_statements_paths(body, branch, &mut names);
        }
        Stmt::For { from, to, body: branch, .. } => {
            collect_expression_paths(body, *from, &mut names);
            collect_expression_paths(body, *to, &mut names);
            collect_statements_paths(body, branch, &mut names);
        }
        Stmt::ForEach { collection, body: branch, .. } => {
            collect_expression_paths(body, *collection, &mut names);
            collect_statements_paths(body, branch, &mut names);
        }
        Stmt::Try { body: attempt, except } => {
            collect_statements_paths(body, attempt, &mut names);
            collect_statements_paths(body, except, &mut names);
        }
        _ => {
            for_each_stmt_expr_root(body, stmt, &mut |root| {
                collect_expression_paths(body, root, &mut names);
            });
        }
    }
    names
}

fn collect_statements_paths(body: &Body, statements: &[StmtIdx], names: &mut FxHashSet<NormName>) {
    for &stmt in statements {
        names.extend(stmt_path_names(body, stmt));
    }
}

fn expression_path_names(body: &Body, expr: ExprIdx) -> FxHashSet<NormName> {
    let mut names = FxHashSet::default();
    collect_expression_paths(body, expr, &mut names);
    names
}

fn collect_expression_paths(body: &Body, expr: ExprIdx, names: &mut FxHashSet<NormName>) {
    if let Expr::Path(name) = body.expr_idx(expr) {
        names.insert(NormName::intern(name.as_str()));
        return;
    }
    for_each_expr_child(body, expr, &mut |child| collect_expression_paths(body, child, names));
}

#[cfg(test)]
mod tests {
    use super::*;

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
            &cfg,
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
}
