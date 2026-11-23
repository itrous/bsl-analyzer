//! Tracks values produced by exception handlers until a method exposes them.

use std::sync::Arc;

use cfg::{CfgEdgeType, ControlFlowGraph};
use hir_def::{
    body::Body,
    hir::{BinaryOp, Expr, ExprIdx, Literal, Stmt, StmtIdx, UnaryOp},
    BindingId, ExprId, IdConversion, StmtId,
};
use la_arena::RawIdx;
use rustc_hash::{FxHashMap, FxHashSet};
use smol_str::SmolStr;
use stdx::case::CaseExt;

use crate::{DataflowSolver, Lattice, Transfer};

/// Proven transfers for one call in the body being analysed. Positions refer to its arguments.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct CallEffect {
    pub arguments_to_result: Vec<usize>,
    /// An empty list can still store a default element when an argument was omitted.
    pub arguments_to_receiver: Option<Vec<usize>>,
    pub receiver_to_result: bool,
    pub clear_receiver: bool,
    /// Argument places the call cannot write back to.
    pub preserved_arguments: Vec<usize>,
}

type Origins = FxHashSet<StmtId>;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct OutcomeState {
    reachable: bool,
    condition: Option<bool>,
    values: FxHashMap<SmolStr, Origins>,
    observed: Origins,
    aliases: Arc<FxHashMap<SmolStr, FxHashSet<SmolStr>>>,
    call_effects: Arc<FxHashMap<ExprId, CallEffect>>,
}

fn within(candidate: &str, parent: &str) -> bool {
    candidate == parent || candidate.strip_prefix(parent).is_some_and(|rest| rest.starts_with('.'))
}

impl OutcomeState {
    fn read(&self, place: &str) -> Origins {
        self.values
            .iter()
            .filter(|(stored, _)| within(stored, place) || within(place, stored))
            .flat_map(|(_, origins)| origins.iter().copied())
            .collect()
    }

    fn kill(&mut self, place: &str) {
        let root = place.split('.').next().unwrap_or(place);
        if !self.aliases.contains_key(root) {
            self.values.retain(|stored, _| !within(stored, place) && !within(place, stored));
            return;
        }
        // shortcut: aliases only invalidate proof; heap identity is needed to propagate through them.
        let mut pending = vec![SmolStr::new(root)];
        let mut roots = FxHashSet::default();
        while let Some(root) = pending.pop() {
            if roots.insert(root.clone()) {
                pending.extend(self.aliases.get(&root).into_iter().flatten().cloned());
            }
        }
        self.values.retain(|stored, _| !roots.contains(stored.split('.').next().unwrap_or(stored)));
    }

    fn write(&mut self, place: SmolStr, origins: Origins) {
        self.kill(&place);
        if !origins.is_empty() {
            self.values.insert(place, origins);
        }
    }
}

impl Lattice for OutcomeState {
    fn join(&self, other: &Self) -> Self {
        if !self.reachable {
            return other.clone();
        }
        if !other.reachable {
            return self.clone();
        }
        let mut result = self.clone();
        for (place, origins) in &other.values {
            result.values.entry(place.clone()).or_default().extend(origins);
        }
        result.observed.extend(&other.observed);
        result.condition = (self.condition == other.condition).then_some(self.condition).flatten();
        result
    }
}

#[derive(Default)]
struct Contexts {
    stmts: FxHashMap<StmtId, Origins>,
    headers: FxHashMap<ExprId, Origins>,
    escaping_raises: FxHashSet<StmtId>,
    aliases: FxHashMap<SmolStr, FxHashSet<SmolStr>>,
}

impl Contexts {
    fn collect(&mut self, body: &Body, stmts: &[StmtIdx], active: &Origins, caught: bool) {
        for &stmt in stmts {
            let id = StmtId::from_idx(stmt);
            self.stmts.insert(id, active.clone());
            match body.stmt_idx(stmt) {
                Stmt::Assign { target, value } => {
                    if let Some(target) = place(body, *target) {
                        let target = SmolStr::new(target.split('.').next().unwrap_or(&target));
                        for source in alias_sources(body, *value) {
                            let source = SmolStr::new(source.split('.').next().unwrap_or(&source));
                            if source != target {
                                self.aliases
                                    .entry(target.clone())
                                    .or_default()
                                    .insert(source.clone());
                                self.aliases.entry(source).or_default().insert(target.clone());
                            }
                        }
                    }
                }
                Stmt::Try { body: protected, except } => {
                    self.collect(body, protected, active, true);
                    let mut handling = active.clone();
                    handling.insert(id);
                    self.collect(body, except, &handling, caught);
                }
                Stmt::If(branch) => {
                    self.headers.insert(ExprId::from_idx(branch.condition), active.clone());
                    self.collect(body, &branch.then_branch, active, caught);
                    for (condition, stmts) in branch.elsif_branches.iter() {
                        self.headers.insert(ExprId::from_idx(*condition), active.clone());
                        self.collect(body, stmts, active, caught);
                    }
                    if let Some(stmts) = &branch.else_branch {
                        self.collect(body, stmts, active, caught);
                    }
                }
                Stmt::While { condition, body: stmts } => {
                    self.headers.insert(ExprId::from_idx(*condition), active.clone());
                    self.collect(body, stmts, active, caught);
                }
                Stmt::For { from, to, body: stmts, .. } => {
                    self.headers.insert(ExprId::from_idx(*from), active.clone());
                    self.headers.insert(ExprId::from_idx(*to), active.clone());
                    self.collect(body, stmts, active, caught);
                }
                Stmt::ForEach { collection, body: stmts, .. } => {
                    self.headers.insert(ExprId::from_idx(*collection), active.clone());
                    self.collect(body, stmts, active, caught);
                }
                Stmt::PreprocIf(branch) => {
                    self.collect(body, &branch.then_branch, active, caught);
                    for (_, _, stmts) in branch.elsif_branches.iter() {
                        self.collect(body, stmts, active, caught);
                    }
                    if let Some(stmts) = &branch.else_branch {
                        self.collect(body, stmts, active, caught);
                    }
                }
                Stmt::Raise { .. } if !caught => {
                    self.escaping_raises.insert(id);
                }
                _ => {}
            }
        }
    }
}

fn alias_sources(body: &Body, expr: ExprIdx) -> Vec<SmolStr> {
    if let Some(place) = place(body, expr) {
        return vec![place];
    }
    match body.expr_idx(expr) {
        Expr::New { args, .. } | Expr::Array(args) => {
            args.iter().flat_map(|&arg| alias_sources(body, arg)).collect()
        }
        _ => Vec::new(),
    }
}

fn place(body: &Body, expr: ExprIdx) -> Option<SmolStr> {
    match body.expr_idx(expr) {
        Expr::Path(name) => Some(name.as_str().fold_lower().into()),
        Expr::Field { base, field } => {
            Some(format!("{}.{}", place(body, *base)?, field.as_str().fold_lower()).into())
        }
        _ => None,
    }
}

fn mutation_place(body: &Body, expr: ExprIdx) -> Option<SmolStr> {
    match body.expr_idx(expr) {
        Expr::Index { base, .. } => mutation_place(body, *base),
        Expr::Field { base, .. } => place(body, expr).or_else(|| mutation_place(body, *base)),
        _ => place(body, expr),
    }
}

fn literal_bool(body: &Body, expr: ExprIdx) -> Option<bool> {
    match body.expr_idx(expr) {
        Expr::Literal(Literal::Bool(value)) => Some(*value),
        Expr::UnaryOp { expr, op: UnaryOp::Not } => literal_bool(body, *expr).map(|value| !value),
        _ => None,
    }
}

fn evaluate_call(
    body: &Body,
    call: ExprIdx,
    receiver: Option<ExprIdx>,
    args: &[ExprIdx],
    state: &mut OutcomeState,
    active: &Origins,
) -> Origins {
    let effects = Arc::clone(&state.call_effects);
    let effect = effects.get(&ExprId::from_idx(call));
    let receiver_origins =
        receiver.map(|receiver| evaluate(body, receiver, state, active)).unwrap_or_default();
    let mut result = Origins::default();
    let mut stored = Origins::default();
    for (position, &arg) in args.iter().enumerate() {
        let origins = evaluate(body, arg, state, active);
        if let Some(effect) = effect {
            if effect.arguments_to_result.contains(&position) {
                result.extend(&origins);
            }
            if effect.arguments_to_receiver.as_ref().is_some_and(|args| args.contains(&position)) {
                stored.extend(origins);
            }
        }
    }

    if let Some(effect) = effect {
        let receiver_place = receiver.and_then(|receiver| place(body, receiver));
        if effect.receiver_to_result {
            result.extend(
                receiver_place.as_ref().map_or(receiver_origins, |place| state.read(place)),
            );
        }
        if effect.clear_receiver {
            if let Some(place) = receiver.and_then(|receiver| mutation_place(body, receiver)) {
                state.kill(&place);
            }
        } else if effect.arguments_to_receiver.is_some() {
            stored.extend(active);
            if let Some(place) = receiver_place.filter(|_| !stored.is_empty()) {
                state.values.entry(place).or_default().extend(stored);
            }
        }
    } else if let Some(place) = receiver.and_then(|receiver| mutation_place(body, receiver)) {
        state.kill(&place);
    }

    // Unknown callees can replace arguments passed by reference.
    for (position, &arg) in args.iter().enumerate() {
        if !effect.is_some_and(|effect| effect.preserved_arguments.contains(&position)) {
            if let Some(place) = mutation_place(body, arg) {
                state.kill(&place);
            }
        }
    }
    result
}

fn evaluate(body: &Body, expr: ExprIdx, state: &mut OutcomeState, active: &Origins) -> Origins {
    match body.expr_idx(expr) {
        Expr::Path(_) | Expr::Field { .. } => {
            if let Some(place) = place(body, expr) {
                state.read(&place)
            } else {
                if let Expr::Field { base, .. } = body.expr_idx(expr) {
                    evaluate(body, *base, state, active);
                }
                Origins::default()
            }
        }
        Expr::BinaryOp { lhs, rhs, op } => {
            let mut origins = evaluate(body, *lhs, state, active);
            if matches!(
                (op, literal_bool(body, *lhs)),
                (BinaryOp::And, Some(false)) | (BinaryOp::Or, Some(true))
            ) {
                return origins;
            }
            if matches!(op, BinaryOp::And | BinaryOp::Or) && literal_bool(body, *lhs).is_none() {
                let mut alternative = state.clone();
                origins.extend(evaluate(body, *rhs, &mut alternative, active));
                *state = state.join(&alternative);
            } else {
                origins.extend(evaluate(body, *rhs, state, active));
            }
            origins
        }
        Expr::UnaryOp { expr, .. } | Expr::Await { expr } => evaluate(body, *expr, state, active),
        Expr::Ternary { condition, then_expr, else_expr } => {
            let mut origins = evaluate(body, *condition, state, active);
            if let Some(value) = literal_bool(body, *condition) {
                origins.extend(evaluate(
                    body,
                    if value { *then_expr } else { *else_expr },
                    state,
                    active,
                ));
                return origins;
            }
            let mut alternative = state.clone();
            origins.extend(evaluate(body, *then_expr, state, active));
            origins.extend(evaluate(body, *else_expr, &mut alternative, active));
            *state = state.join(&alternative);
            origins
        }
        Expr::New { args, .. } => evaluate_call(body, expr, None, args, state, active),
        Expr::Array(args) => {
            args.iter().flat_map(|&arg| evaluate(body, arg, state, active)).collect()
        }
        Expr::Call { callee, args } => {
            let receiver = match body.expr_idx(*callee) {
                Expr::Field { base, .. } => Some(*base),
                _ => None,
            };
            evaluate_call(body, expr, receiver, args, state, active)
        }
        Expr::MethodCall { receiver, args, .. } => {
            evaluate_call(body, expr, Some(*receiver), args, state, active)
        }
        Expr::Index { base, index } => {
            evaluate(body, *base, state, active);
            evaluate(body, *index, state, active);
            Origins::default()
        }
        Expr::Missing | Expr::Literal(_) => Origins::default(),
    }
}

struct OutcomeTransfer(Contexts);

impl Transfer<OutcomeState> for OutcomeTransfer {
    fn transfer_stmt(&self, stmt: RawIdx, state: &OutcomeState, body: &Body) -> OutcomeState {
        let mut state = state.clone();
        if !state.reachable {
            return state;
        }
        state.condition = None;
        let id = StmtId::from_raw(stmt);
        let active = self.0.stmts.get(&id).cloned().unwrap_or_default();
        match body.stmt(id) {
            Stmt::Assign { target, value } => {
                let mut origins = evaluate(body, *value, &mut state, &active);
                origins.extend(active);
                if let Some(place) = place(body, *target) {
                    state.write(place, origins);
                } else if let Some(place) = mutation_place(body, *target) {
                    state.kill(&place);
                }
            }
            Stmt::Expr(expr) | Stmt::Execute { expr } => {
                evaluate(body, *expr, &mut state, &active);
            }
            Stmt::Return { value: Some(value) } => {
                let origins = evaluate(body, *value, &mut state, &active);
                state.observed.extend(origins.into_iter().chain(active));
            }
            Stmt::Raise { value: Some(value) } => {
                let origins = evaluate(body, *value, &mut state, &active);
                if self.0.escaping_raises.contains(&id) {
                    state.observed.extend(origins);
                }
            }
            _ => {}
        }
        state
    }

    fn transfer_expr(&self, expr: ExprId, state: &OutcomeState, body: &Body) -> OutcomeState {
        let mut state = state.clone();
        if state.reachable {
            let active = self.0.headers.get(&expr).cloned().unwrap_or_default();
            evaluate(body, expr.to_idx(), &mut state, &active);
            state.condition = literal_bool(body, expr.to_idx());
        }
        state
    }

    fn transfer_edge(&self, edge: CfgEdgeType, state: &OutcomeState) -> OutcomeState {
        if edge == CfgEdgeType::Unexecutable
            || matches!(
                (edge, state.condition),
                (CfgEdgeType::TrueBranch, Some(false)) | (CfgEdgeType::FalseBranch, Some(true))
            )
        {
            OutcomeState::default()
        } else {
            let mut result = state.clone();
            result.condition = None;
            result
        }
    }

    fn transfer_loop_var_bind(&self, var: BindingId, state: &mut OutcomeState, body: &Body) {
        state.kill(&body.binding(var).name.as_str().fold_lower());
    }
}

/// Handlers whose produced value reaches a return, escaping raise, or output parameter.
/// Local assignments alone do not qualify; incomplete analysis proves nothing.
/// `call_effects` uses expression IDs from this body, including the whole call or constructor.
pub fn observable_catch_results(
    body: &Body,
    cfg: Arc<ControlFlowGraph>,
    call_effects: FxHashMap<ExprId, CallEffect>,
) -> FxHashSet<StmtId> {
    if body.is_lowering_incomplete() {
        return FxHashSet::default();
    }
    let mut contexts = Contexts::default();
    let statements: Vec<_> = body.body_stmts().map(|id| id.to_idx()).collect();
    contexts.collect(body, &statements, &Origins::default(), false);
    let exit = cfg.exit();
    let aliases = Arc::new(std::mem::take(&mut contexts.aliases));
    let mut solver = DataflowSolver::new(cfg, body.clone(), OutcomeTransfer(contexts));
    solver.set_bottom_factory(OutcomeState::default);
    solver.set_initial_state(OutcomeState {
        reachable: true,
        aliases,
        call_effects: Arc::new(call_effects),
        ..OutcomeState::default()
    });
    let Some(result) = solver.solve().filter(|result| result.converged()) else {
        return FxHashSet::default();
    };
    let Some(state) = result.block_in(exit) else { return FxHashSet::default() };
    let mut observed = state.observed.clone();
    for param in body.params() {
        let binding = body.binding(param);
        let root = binding.name.as_str().fold_lower();
        for (place, origins) in &state.values {
            if (!binding.is_val && place.as_str() == root) || place.starts_with(&format!("{root}."))
            {
                observed.extend(origins);
            }
        }
    }
    observed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn origin() -> StmtId {
        StmtId::from_raw(RawIdx::from_u32(0))
    }

    #[test]
    fn field_overwrite_invalidates_an_aggregate_origin() {
        let mut state = OutcomeState::default();
        state.values.insert("result".into(), [origin()].into_iter().collect());
        state.kill("result.error");
        assert!(state.read("result").is_empty());
    }

    #[test]
    fn field_overwrite_preserves_an_independent_field() {
        let mut state = OutcomeState::default();
        state.values.insert("result.error".into(), [origin()].into_iter().collect());
        state.kill("result.count");
        assert_eq!(state.read("result.error"), [origin()].into_iter().collect());
    }

    #[test]
    fn alias_mutation_invalidates_the_connected_places() {
        let mut state = OutcomeState {
            aliases: Arc::new(
                [
                    ("errors".into(), ["copy".into()].into_iter().collect()),
                    ("copy".into(), ["errors".into()].into_iter().collect()),
                ]
                .into_iter()
                .collect(),
            ),
            ..OutcomeState::default()
        };
        state.values.insert("errors".into(), [origin()].into_iter().collect());
        state.values.insert("independent".into(), [origin()].into_iter().collect());
        state.kill("copy");
        assert!(state.read("errors").is_empty());
        assert_eq!(state.read("independent"), [origin()].into_iter().collect());
    }
}
