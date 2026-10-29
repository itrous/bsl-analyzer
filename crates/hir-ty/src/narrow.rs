use cfg::CfgEdgeType;
use dataflow::{Lattice, Transfer};
use hir_def::body::Body;
use hir_def::hir::{BinaryOp, Expr, Literal, Stmt};
use hir_def::{DefWithBodyId, ExprId, IdConversion, MethodIdInput, ModuleId, Name};
use stdx::case::CaseExt;

use bsl_types::builders::Builders;
use bsl_types::facet::DateComponent;
use bsl_types::intern::TypeKernelDb;
use bsl_types::kind::{TypeId, TypeKind};

use crate::lower::builtin_names::bare_name_to_typeid;
use la_arena::{Idx, RawIdx};
use rustc_hash::FxHashMap;
use std::sync::Arc;
use std::time::Instant;
use vfs::FileId;

use crate::db::HirDatabase;

type ExprIdx = Idx<Expr>;
type StmtIdx = Idx<Stmt>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Guard {
    TypeCheck { var: Name, type_name: String },

    IsUndefined { var: Name },

    IsNotUndefined { var: Name },

    ValueFilled { var: Name },
}

pub fn recognize_guard(expr: ExprIdx, body: &Body) -> Option<Guard> {
    match body.expr_idx(expr) {
        Expr::BinaryOp { lhs, rhs, op } => match op {
            BinaryOp::Eq => recognize_eq_guard(*lhs, *rhs, body, false),
            BinaryOp::Neq => recognize_eq_guard(*lhs, *rhs, body, true),
            _ => None,
        },
        Expr::Call { callee, args } => {
            let callee_name = path_name(*callee, body)?;
            if callee_name.eq_ignore_case(&Name::new("ЗначениеЗаполнено"))
                || callee_name.eq_ignore_case(&Name::new("ValueIsFilled"))
                || callee_name.eq_ignore_case(&Name::new("ValueFilled"))
            {
                let var = single_path_arg(args, body)?;
                return Some(Guard::ValueFilled { var });
            }
            None
        }
        _ => None,
    }
}

fn recognize_eq_guard(lhs: ExprIdx, rhs: ExprIdx, body: &Body, negated: bool) -> Option<Guard> {
    if let Some(g) =
        try_type_check(lhs, rhs, body, negated).or_else(|| try_type_check(rhs, lhs, body, negated))
    {
        return Some(g);
    }

    if let Some(var) =
        try_undefined_compare(lhs, rhs, body).or_else(|| try_undefined_compare(rhs, lhs, body))
    {
        return Some(if negated {
            Guard::IsNotUndefined { var }
        } else {
            Guard::IsUndefined { var }
        });
    }

    None
}

fn try_type_check(lhs: ExprIdx, rhs: ExprIdx, body: &Body, negated: bool) -> Option<Guard> {
    if negated {
        return None;
    }
    let var = type_of_arg(lhs, body)?;
    let type_name = type_literal_arg(rhs, body)?;
    Some(Guard::TypeCheck { var, type_name })
}

fn try_undefined_compare(lhs: ExprIdx, rhs: ExprIdx, body: &Body) -> Option<Name> {
    let var = path_name(lhs, body)?;
    match body.expr_idx(rhs) {
        Expr::Literal(Literal::Undefined) => Some(var),
        _ => None,
    }
}

fn type_of_arg(expr: ExprIdx, body: &Body) -> Option<Name> {
    let (callee_name, args) = call_parts(expr, body)?;
    if !callee_name.eq_ignore_case(&Name::new("ТипЗнч"))
        && !callee_name.eq_ignore_case(&Name::new("TypeOf"))
    {
        return None;
    }
    single_path_arg(args, body)
}

fn type_literal_arg(expr: ExprIdx, body: &Body) -> Option<String> {
    crate::type_literal::type_ctor_literal(body, expr).map(|(_, text)| text.to_owned())
}

fn call_parts(expr: ExprIdx, body: &Body) -> Option<(Name, &[ExprIdx])> {
    match body.expr_idx(expr) {
        Expr::Call { callee, args } => {
            let name = path_name(*callee, body)?;
            Some((name, args.as_ref()))
        }
        _ => None,
    }
}

fn path_name(expr: ExprIdx, body: &Body) -> Option<Name> {
    match body.expr_idx(expr) {
        Expr::Path(name) => Some(name.clone()),
        _ => None,
    }
}

fn single_path_arg(args: &[ExprIdx], body: &Body) -> Option<Name> {
    if args.len() != 1 {
        return None;
    }
    path_name(args[0], body)
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NarrowState {
    narrowed: FxHashMap<Name, Box<[TypeId]>>,
    pending_guard: Option<Guard>,
    /// Bottom of the lattice: the state arriving over an unexecutable edge (the
    /// fall-through from a branch every path of which returned or raised).
    /// Joining with it is identity, so `Если Х = Неопределено Тогда Возврат
    /// КонецЕсли` keeps the inverted guard after the block instead of being
    /// diluted by the terminated then-branch.
    unreachable: bool,
}

impl NarrowState {
    pub fn new() -> Self {
        Self::default()
    }

    fn unreachable_bottom() -> Self {
        Self { unreachable: true, ..Self::default() }
    }

    pub fn get(&self, name: &Name) -> Option<&[TypeId]> {
        self.narrowed.get(&fold_name(name)).map(|arms| &arms[..])
    }

    pub fn len(&self) -> usize {
        self.narrowed.len()
    }

    pub fn is_empty(&self) -> bool {
        self.narrowed.is_empty()
    }
}

impl Lattice for NarrowState {
    fn join(&self, other: &Self) -> Self {
        if self.unreachable {
            return NarrowState {
                narrowed: other.narrowed.clone(),
                pending_guard: None,
                unreachable: other.unreachable,
            };
        }
        if other.unreachable {
            return NarrowState {
                narrowed: self.narrowed.clone(),
                pending_guard: None,
                unreachable: false,
            };
        }
        let mut narrowed = FxHashMap::default();
        for (k, arms_self) in &self.narrowed {
            if let Some(arms_other) = other.narrowed.get(k) {
                narrowed.insert(k.clone(), merge_arm_sets(arms_self, arms_other));
            }
        }
        NarrowState { narrowed, pending_guard: None, unreachable: false }
    }
}

pub(crate) struct NarrowingTransfer<'db> {
    db: &'db dyn TypeKernelDb,
    base_types: FxHashMap<Name, TypeId>,
}

impl<'db> NarrowingTransfer<'db> {
    pub(crate) fn new(db: &'db dyn TypeKernelDb, base_types: FxHashMap<Name, TypeId>) -> Self {
        Self { db, base_types }
    }

    fn apply_guard(&self, state: &mut NarrowState, guard: &Guard, on_true: bool) {
        match guard {
            Guard::TypeCheck { var, type_name } => {
                let matched = bare_name_to_typeid(self.db, type_name);
                let arms = if on_true {
                    self.refine_matched_with_base(matched, var)
                } else {
                    self.complement_of(state, var, matched)
                };
                insert_if_informative(state, var, arms);
            }
            Guard::IsUndefined { var } => {
                let arms = if on_true {
                    arm_set_from_type_id(self.db, self.db.undefined())
                } else {
                    self.complement_of(state, var, self.db.undefined())
                };
                insert_if_informative(state, var, arms);
            }
            Guard::IsNotUndefined { var } => {
                let arms = if on_true {
                    self.complement_of(state, var, self.db.undefined())
                } else {
                    arm_set_from_type_id(self.db, self.db.undefined())
                };
                insert_if_informative(state, var, arms);
            }
            Guard::ValueFilled { var } => {
                if on_true {
                    let Some(&base) = self.base_types.get(&fold_name(var)) else {
                        return;
                    };
                    let residual = ty_difference_unfilled_witnesses(self.db, base);
                    if *residual != *arm_set_from_type_id(self.db, base) {
                        insert_if_informative(state, var, residual);
                    }
                }
            }
        }
    }

    fn refine_matched_with_base(&self, matched: TypeId, var: &Name) -> Box<[TypeId]> {
        if !is_array_kind(self.db, matched) {
            return arm_set_from_type_id(self.db, matched);
        }
        let Some(&base) = self.base_types.get(&fold_name(var)) else {
            return arm_set_from_type_id(self.db, matched);
        };
        match self.db.lookup_type(base) {
            TypeKind::Array(_) => arm_set_from_type_id(self.db, base),
            TypeKind::Union(members) => {
                let array_arms: Vec<TypeId> =
                    members.iter().copied().filter(|id| is_array_kind(self.db, *id)).collect();
                if array_arms.is_empty() {
                    arm_set_from_type_id(self.db, matched)
                } else {
                    normalize_arms(self.db, array_arms)
                }
            }
            _ => arm_set_from_type_id(self.db, matched),
        }
    }

    fn complement_of(&self, state: &NarrowState, var: &Name, matched: TypeId) -> Box<[TypeId]> {
        // The tracked arm set is the flow-sensitive type at this point —
        // subtract from it when it actually carries the matched arm (the
        // first-occurrence base below may be a single assignment's type and
        // miss arms that joined in later).
        if !is_array_kind(self.db, matched) {
            if let Some(arms) = state.narrowed.get(&fold_name(var)) {
                if arms.contains(&matched) {
                    let remaining: Vec<TypeId> =
                        arms.iter().copied().filter(|m| *m != matched).collect();
                    return normalize_arms(self.db, remaining);
                }
            }
        }
        let Some(&base) = self.base_types.get(&fold_name(var)) else {
            return Box::new([]);
        };
        if is_array_kind(self.db, matched) {
            return ty_difference_array_aware(self.db, base);
        }
        ty_difference(self.db, base, matched)
    }

    fn infer_rhs_type(&self, value: ExprIdx, state: &NarrowState, body: &Body) -> Box<[TypeId]> {
        match body.expr_idx(value) {
            Expr::Literal(lit) => {
                let id = match lit {
                    Literal::Number(_) => self.db.number(None, None),
                    Literal::String(_) => self.db.string(None, false),
                    Literal::Date(_) => self.db.date(DateComponent::DateTime),
                    Literal::Bool(_) => self.db.boolean(),
                    Literal::Undefined => self.db.undefined(),
                    Literal::Null => self.db.null(),
                };
                arm_set_from_type_id(self.db, id)
            }
            Expr::Path(name) => {
                let folded = fold_name(name);
                if let Some(arms) = state.narrowed.get(&folded) {
                    arms.clone()
                } else if let Some(&base) = self.base_types.get(&folded) {
                    arm_set_from_type_id(self.db, base)
                } else {
                    Box::new([])
                }
            }
            _ => Box::new([]),
        }
    }
}

impl Transfer<NarrowState> for NarrowingTransfer<'_> {
    fn transfer_stmt(&self, stmt_id: RawIdx, state: &NarrowState, body: &Body) -> NarrowState {
        let mut new_state = state.clone();
        let stmt_idx: StmtIdx = Idx::from_raw(stmt_id);
        if let Stmt::Assign { target, value } = body.stmt_idx(stmt_idx) {
            if let Expr::Path(name) = body.expr_idx(*target) {
                let new_arms = self.infer_rhs_type(*value, &new_state, body);
                let folded = fold_name(name);
                if new_arms.is_empty() {
                    new_state.narrowed.remove(&folded);
                } else {
                    new_state.narrowed.insert(folded, new_arms);
                }
            }
        }
        new_state
    }

    fn transfer_expr(&self, expr_id: ExprId, state: &NarrowState, body: &Body) -> NarrowState {
        let mut new_state = state.clone();
        new_state.pending_guard = recognize_guard(expr_id.to_idx(), body);
        new_state
    }

    fn transfer_edge(&self, edge_kind: CfgEdgeType, state: &NarrowState) -> NarrowState {
        if !edge_kind.is_executable() {
            return NarrowState::unreachable_bottom();
        }
        let mut new_state = state.clone();
        new_state.unreachable = false;
        let pending = new_state.pending_guard.take();
        match (edge_kind, pending) {
            (CfgEdgeType::TrueBranch, Some(g)) => self.apply_guard(&mut new_state, &g, true),
            (CfgEdgeType::FalseBranch, Some(g)) => self.apply_guard(&mut new_state, &g, false),
            _ => {}
        }
        new_state
    }
}

fn insert_if_informative(state: &mut NarrowState, var: &Name, arms: Box<[TypeId]>) {
    if !arms.is_empty() {
        state.narrowed.insert(fold_name(var), arms);
    }
}

fn arm_set_from_type_id(db: &dyn TypeKernelDb, id: TypeId) -> Box<[TypeId]> {
    let arms: Vec<TypeId> = match db.lookup_type(id) {
        TypeKind::Union(members) => members.iter().copied().collect(),
        TypeKind::Unknown | TypeKind::Never | TypeKind::Any => Vec::new(),
        _ => vec![id],
    };
    normalize_arms(db, arms)
}

fn normalize_arms(db: &dyn TypeKernelDb, arms: Vec<TypeId>) -> Box<[TypeId]> {
    let mut arms: Vec<TypeId> = arms
        .into_iter()
        .filter(|id| {
            !matches!(db.lookup_type(*id), TypeKind::Unknown | TypeKind::Never | TypeKind::Any)
        })
        .collect();
    arms.sort_by_key(|id| id.raw());
    arms.dedup();
    arms.into_boxed_slice()
}

fn merge_arm_sets(a: &[TypeId], b: &[TypeId]) -> Box<[TypeId]> {
    let mut merged: Vec<TypeId> = Vec::with_capacity(a.len() + b.len());
    merged.extend_from_slice(a);
    merged.extend_from_slice(b);
    merged.sort_by_key(|id| id.raw());
    merged.dedup();
    merged.into_boxed_slice()
}

fn is_array_kind(db: &dyn TypeKernelDb, id: TypeId) -> bool {
    matches!(db.lookup_type(id), TypeKind::Array(_))
}

fn fold_name(n: &Name) -> Name {
    Name::new(&n.as_str().fold_lower())
}

fn ty_difference(db: &dyn TypeKernelDb, base: TypeId, matched: TypeId) -> Box<[TypeId]> {
    match db.lookup_type(base) {
        TypeKind::Union(members) => {
            let remaining: Vec<TypeId> =
                members.iter().copied().filter(|m| *m != matched).collect();
            normalize_arms(db, remaining)
        }
        _ => Box::new([]),
    }
}

fn ty_difference_unfilled_witnesses(db: &dyn TypeKernelDb, base: TypeId) -> Box<[TypeId]> {
    match db.lookup_type(base) {
        TypeKind::Union(members) => {
            let remaining: Vec<TypeId> =
                members.iter().copied().filter(|m| !is_unfilled_witness(db, *m)).collect();
            normalize_arms(db, remaining)
        }
        _ => Box::new([]),
    }
}

fn is_unfilled_witness(db: &dyn TypeKernelDb, id: TypeId) -> bool {
    matches!(db.lookup_type(id), TypeKind::Undefined | TypeKind::Null)
}

fn ty_difference_array_aware(db: &dyn TypeKernelDb, base: TypeId) -> Box<[TypeId]> {
    match db.lookup_type(base) {
        TypeKind::Union(members) => {
            let remaining: Vec<TypeId> =
                members.iter().copied().filter(|m| !is_array_kind(db, *m)).collect();
            normalize_arms(db, remaining)
        }
        _ => Box::new([]),
    }
}

pub fn narrow_body(
    db: &dyn TypeKernelDb,
    body: Body,
    base_types: FxHashMap<Name, TypeId>,
) -> Option<dataflow::DataflowResult<NarrowState>> {
    let cfg = Arc::new(cfg::CfgBuilder::new().build_graph_from_hir(body.body_stmts_typed(), &body));
    let mut solver =
        dataflow::DataflowSolver::new(cfg, body, NarrowingTransfer::new(db, base_types));
    solver.set_bottom_factory(NarrowState::new);
    solver.solve()
}

pub fn narrow_query(
    db: &dyn HirDatabase,
    file_id: FileId,
    owner: DefWithBodyId,
) -> Option<Arc<dataflow::DataflowResult<NarrowState>>> {
    let _span = tracing::info_span!("narrow_query", ?file_id, ?owner).entered();
    let total_start = Instant::now();

    let resolve_start = Instant::now();
    let module_id = ModuleId { file_id };
    // A method's body comes from its own memo: reading the file fold here
    // would tie every caller keyed by the method to the whole file.
    let method_body;
    let module_bodies;
    let body: &Body = match owner {
        DefWithBodyId::ModuleCode => {
            module_bodies = db.module_bodies(module_id);
            module_bodies.module_code()?
        }
        DefWithBodyId::Method(local_id) => {
            let method = hir_def::MethodId { module: module_id, local_id };
            db.interface_method(method)?;
            method_body = db.method_body(MethodIdInput::new(db, method));
            &method_body
        }
    };
    let resolve_ns = resolve_start.elapsed().as_nanos();

    let infer_start = Instant::now();
    let infer_routed = crate::infer::infer_owner(db, file_id, owner);
    let per_body_types = Some(infer_routed.expr_types());
    let infer_ns = infer_start.elapsed().as_nanos();

    let base_types_start = Instant::now();
    let base_types = build_base_types_for_body(body, per_body_types);
    let base_types_ns = base_types_start.elapsed().as_nanos();

    let body_clone_start = Instant::now();
    let body_owned = body.clone();
    let body_clone_ns = body_clone_start.elapsed().as_nanos();

    let cfg_build_start = Instant::now();
    let cfg = Arc::new(
        cfg::CfgBuilder::new().build_graph_from_hir(body_owned.body_stmts_typed(), &body_owned),
    );
    let cfg_build_ns = cfg_build_start.elapsed().as_nanos();

    let solve_start = Instant::now();
    let mut solver =
        dataflow::DataflowSolver::new(cfg, body_owned, NarrowingTransfer::new(db, base_types));
    solver.set_bottom_factory(NarrowState::new);
    let solved = solver.solve();
    let solve_ns = solve_start.elapsed().as_nanos();

    let stages = NarrowQueryStages {
        total_ns: total_start.elapsed().as_nanos(),
        resolve_ns,
        infer_ns,
        base_types_ns,
        body_clone_ns,
        cfg_build_ns,
        solve_ns,
    };
    log_narrow_query_stages(owner, &stages);

    Some(Arc::new(solved?))
}

struct NarrowQueryStages {
    total_ns: u128,
    resolve_ns: u128,
    infer_ns: u128,
    base_types_ns: u128,
    body_clone_ns: u128,
    cfg_build_ns: u128,
    solve_ns: u128,
}

fn log_narrow_query_stages(owner: DefWithBodyId, stages: &NarrowQueryStages) {
    const SLOW_NS: u128 = 20_000_000;
    if stages.total_ns < SLOW_NS {
        return;
    }
    let to_us = |ns: u128| (ns / 1_000) as u64;
    tracing::info!(
        owner = ?owner,
        total_us = to_us(stages.total_ns),
        resolve_us = to_us(stages.resolve_ns),
        infer_us = to_us(stages.infer_ns),
        base_types_us = to_us(stages.base_types_ns),
        body_clone_us = to_us(stages.body_clone_ns),
        cfg_build_us = to_us(stages.cfg_build_ns),
        solve_us = to_us(stages.solve_ns),
        "narrow_query stages",
    );
}

fn build_base_types_for_body(
    body: &Body,
    per_body_types: Option<&FxHashMap<hir_def::ExprId, TypeId>>,
) -> FxHashMap<Name, TypeId> {
    let mut base_types: FxHashMap<Name, TypeId> = FxHashMap::default();
    let Some(per_body) = per_body_types else {
        return base_types;
    };
    for (expr_id, expr) in body.exprs_iter() {
        if let Expr::Path(name) = expr {
            if let Some(tid) = per_body.get(&expr_id).copied() {
                base_types.entry(fold_name(name)).or_insert(tid);
            }
        }
    }
    base_types
}

pub fn narrowed_type_at<DB: TypeKernelDb + ?Sized>(
    db: &DB,
    result: &dataflow::DataflowResult<NarrowState>,
    body: &Body,
    expr_idx: ExprIdx,
    name: &Name,
) -> Option<TypeId> {
    let cfg = result.cfg();

    let node = containing_vertex(body, cfg, expr_idx)?;
    let arms = result.block_in(node)?.get(name)?;
    if arms.is_empty() {
        return None;
    }
    Some(db.union(arms.to_vec()))
}

/// Applies a guard from an enclosing `?(Усл, А, Б)` to an expression sitting in one of its arms.
///
/// Narrowing is a dataflow over the CFG, and the CFG does not branch inside an expression: both
/// arms live in the vertex that holds the condition, so the dataflow state is identical on either
/// side and a guard written in `Усл` never reaches them. The enclosing ternaries are therefore
/// walked lexically here.
///
/// Runs per call argument on hot bodies, so a type no guard could change is returned before any
/// subtree walk: only a union can lose an arm, and only a body that actually spells a ternary is
/// scanned.
pub fn refine_by_ternary_guard(
    db: &dyn TypeKernelDb,
    body: &Body,
    expr_idx: ExprIdx,
    name: &Name,
    base: TypeId,
) -> TypeId {
    if !matches!(db.lookup_type(base), TypeKind::Union(_)) {
        return base;
    }

    let mut refined = base;
    for (_, expr) in body.exprs_iter() {
        let Expr::Ternary { condition, then_expr, else_expr } = expr else {
            continue;
        };
        let on_true = if expr_covers_expr(body, *then_expr, expr_idx) {
            true
        } else if expr_covers_expr(body, *else_expr, expr_idx) {
            false
        } else {
            continue;
        };
        let Some(guard) = recognize_guard(*condition, body) else {
            continue;
        };
        if !guard_var(&guard).eq_ignore_case(name) {
            continue;
        }

        let mut base_types = FxHashMap::default();
        base_types.insert(fold_name(name), refined);
        let transfer = NarrowingTransfer::new(db, base_types);
        let mut state = NarrowState::new();
        transfer.apply_guard(&mut state, &guard, on_true);
        if let Some(arms) = state.get(name) {
            if !arms.is_empty() {
                refined = db.union(arms.to_vec());
            }
        }
    }
    refined
}

/// The variable a guard talks about.
pub fn guard_var(guard: &Guard) -> &Name {
    match guard {
        Guard::TypeCheck { var, .. }
        | Guard::IsUndefined { var }
        | Guard::IsNotUndefined { var }
        | Guard::ValueFilled { var } => var,
    }
}

/// A guard an enclosing `Если` proves for the statements of one of its branches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchGuard {
    pub guard: Guard,
    /// Polarity this branch gives the condition.
    pub on_true: bool,
    /// The `Если` that proves it. A definition made inside this statement is
    /// not the value the condition tested, so the guard says nothing about it.
    pub if_stmt: StmtIdx,
}

/// What a branch guard proves about a candidate type of the variable it guards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardVerdict {
    /// The guard says nothing about this type.
    NoInformation,
    /// Every arm contradicts the guard: the value cannot carry this type here.
    Contradicted,
    /// Part of the type survives the guard.
    Narrowed(TypeId),
}

/// Guards the `Если` statements lexically enclosing `stmt` prove for it.
///
/// Statement-level narrowing is a dataflow ([`narrow_query`]) that inference cannot
/// consult, because that query is built on top of inference. What is wanted here is
/// purely syntactic anyway — a branch dominates its own statements — so the facts are
/// read off the `Если` tree directly, the way [`refine_by_ternary_guard`] reads the
/// arms of a ternary.
pub fn branch_guards_for_stmt(body: &Body, stmt: hir_def::StmtId) -> Vec<BranchGuard> {
    let mut acc = Vec::new();
    collect_branch_guards(body, body.body_stmts_typed(), stmt.to_idx(), &mut acc);
    acc
}

/// Walks the statement tree towards `target`, recording the guard every `Если`
/// branch on the way proves. Reports whether `target` lives below `stmts`.
fn collect_branch_guards(
    body: &Body,
    stmts: &[StmtIdx],
    target: StmtIdx,
    acc: &mut Vec<BranchGuard>,
) -> bool {
    for &stmt_idx in stmts {
        if stmt_idx == target {
            return true;
        }
        let found = match body.stmt_idx(stmt_idx) {
            Stmt::If(if_stmt) => match find_if_branch(body, if_stmt, target, acc) {
                Some(branch) => {
                    push_branch_guards(body, if_stmt, stmt_idx, branch, acc);
                    true
                }
                None => false,
            },
            Stmt::PreprocIf(pre) => {
                collect_branch_guards(body, &pre.then_branch, target, acc)
                    || pre
                        .elsif_branches
                        .iter()
                        .any(|(_, _, branch)| collect_branch_guards(body, branch, target, acc))
                    || pre
                        .else_branch
                        .as_ref()
                        .is_some_and(|branch| collect_branch_guards(body, branch, target, acc))
            }
            Stmt::While { body: inner, .. }
            | Stmt::For { body: inner, .. }
            | Stmt::ForEach { body: inner, .. } => collect_branch_guards(body, inner, target, acc),
            Stmt::Try { body: inner, except } => {
                collect_branch_guards(body, inner, target, acc)
                    || collect_branch_guards(body, except, target, acc)
            }
            _ => false,
        };
        if found {
            return true;
        }
    }
    false
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IfBranch {
    Then,
    ElsIf(usize),
    Else,
}

/// Which branch of `if_stmt` holds `target`. The descent into that branch has
/// already pushed the guards of any nested `Если` onto `acc`.
fn find_if_branch(
    body: &Body,
    if_stmt: &hir_def::hir::IfStmt,
    target: StmtIdx,
    acc: &mut Vec<BranchGuard>,
) -> Option<IfBranch> {
    if collect_branch_guards(body, &if_stmt.then_branch, target, acc) {
        return Some(IfBranch::Then);
    }
    for (index, (_, branch)) in if_stmt.elsif_branches.iter().enumerate() {
        if collect_branch_guards(body, branch, target, acc) {
            return Some(IfBranch::ElsIf(index));
        }
    }
    let else_branch = if_stmt.else_branch.as_ref()?;
    collect_branch_guards(body, else_branch, target, acc).then_some(IfBranch::Else)
}

/// Whether control can enter these statements without passing the condition that
/// guards them.
///
/// A branch dominates its own statements only while `Перейти` cannot land inside
/// it: the CFG draws an edge straight from the jump to the label, so a statement
/// after a label in the branch may be reached on a path where the condition was
/// never evaluated. The tree says nothing about that path, so a branch holding a
/// label proves nothing.
fn contains_label(body: &Body, stmts: &[StmtIdx]) -> bool {
    stmts.iter().any(|&stmt_idx| match body.stmt_idx(stmt_idx) {
        Stmt::Label(_) => true,
        Stmt::If(if_stmt) => {
            contains_label(body, &if_stmt.then_branch)
                || if_stmt.elsif_branches.iter().any(|(_, branch)| contains_label(body, branch))
                || if_stmt.else_branch.as_ref().is_some_and(|branch| contains_label(body, branch))
        }
        Stmt::PreprocIf(pre) => {
            contains_label(body, &pre.then_branch)
                || pre.elsif_branches.iter().any(|(_, _, branch)| contains_label(body, branch))
                || pre.else_branch.as_ref().is_some_and(|branch| contains_label(body, branch))
        }
        Stmt::While { body: inner, .. }
        | Stmt::For { body: inner, .. }
        | Stmt::ForEach { body: inner, .. } => contains_label(body, inner),
        Stmt::Try { body: inner, except } => {
            contains_label(body, inner) || contains_label(body, except)
        }
        _ => false,
    })
}

/// The statements of one branch of an `Если`.
fn branch_stmts(if_stmt: &hir_def::hir::IfStmt, branch: IfBranch) -> &[StmtIdx] {
    match branch {
        IfBranch::Then => &if_stmt.then_branch,
        IfBranch::ElsIf(index) => &if_stmt.elsif_branches[index].1,
        IfBranch::Else => if_stmt.else_branch.as_deref().unwrap_or(&[]),
    }
}

/// The conditions a branch settles, with the polarity that reaches it: every
/// condition written before the branch is false there, the branch's own is true.
fn push_branch_guards(
    body: &Body,
    if_stmt: &hir_def::hir::IfStmt,
    if_stmt_idx: StmtIdx,
    branch: IfBranch,
    acc: &mut Vec<BranchGuard>,
) {
    if contains_label(body, branch_stmts(if_stmt, branch)) {
        return;
    }
    let push = |condition: ExprIdx, on_true: bool, acc: &mut Vec<BranchGuard>| {
        if let Some(guard) = recognize_guard(condition, body) {
            acc.push(BranchGuard { guard, on_true, if_stmt: if_stmt_idx });
        }
    };
    match branch {
        IfBranch::Then => push(if_stmt.condition, true, acc),
        IfBranch::ElsIf(index) => {
            push(if_stmt.condition, false, acc);
            for (earlier, (condition, _)) in if_stmt.elsif_branches.iter().enumerate() {
                push(*condition, earlier == index, acc);
                if earlier == index {
                    break;
                }
            }
        }
        IfBranch::Else => {
            push(if_stmt.condition, false, acc);
            for (condition, _) in if_stmt.elsif_branches.iter() {
                push(*condition, false, acc);
            }
        }
    }
}

/// Whether the local named `key` may take a value reaching definitions do not see at
/// `use_stmt`.
///
/// An argument is passed by reference unless the callee declares it `Знач`, so a call may
/// write the local it is given. `Выполнить` may write any local. The handler of `Попытка` is reached from
/// the entry of the protected statements only, so a write among them that an error
/// interrupts is not seen by the handler or the code after it.
///
/// `own_call` is the call the use is an argument of, as its binding names it (the call, or
/// its callee): its write happens after the argument is read, so it reaches the use only
/// round a loop around it.
pub(crate) fn local_may_change_unseen(
    body: &Body,
    key: &str,
    use_stmt: StmtIdx,
    own_call: Option<ExprId>,
) -> bool {
    let names_local = |expr: &hir_def::hir::ExprIdx| matches!(body.expr_idx(*expr), Expr::Path(arg) if arg.as_str().fold_lower() == key);
    let covers = |outer: StmtIdx, inner: StmtIdx| stmt_covers_stmt(body, outer, inner);
    let in_loop = || {
        body.stmts_iter().any(|(id, stmt)| {
            matches!(stmt, Stmt::While { .. } | Stmt::For { .. } | Stmt::ForEach { .. })
                && covers(id.to_idx(), use_stmt)
        })
    };
    let is_own_call = |id: ExprId, callee: Option<&hir_def::hir::ExprIdx>| {
        own_call.is_some_and(|own| {
            own == id || callee.is_some_and(|callee| own == ExprId::from_idx(*callee))
        })
    };
    let passed_to_call = body.exprs_iter().any(|(id, expr)| {
        let (args, callee) = match expr {
            Expr::Call { callee, args } => (args, Some(callee)),
            Expr::MethodCall { args, .. } => (args, None),
            _ => return false,
        };
        args.iter().any(names_local) && (!is_own_call(id, callee) || in_loop())
    });
    if passed_to_call {
        return true;
    }
    let writes: Vec<StmtIdx> = body
        .stmts_iter()
        .filter_map(|(id, stmt)| match stmt {
            Stmt::Assign { target, .. } if names_local(target) => Some(id.to_idx()),
            Stmt::For { var, .. } | Stmt::ForEach { var, .. }
                if body.binding_idx(*var).name.as_str().fold_lower() == key =>
            {
                Some(id.to_idx())
            }
            _ => None,
        })
        .collect();
    body.stmts_iter().any(|(_, stmt)| match stmt {
        Stmt::Execute { .. } => true,
        Stmt::Try { body: protected, .. } => {
            let in_protected = |target: StmtIdx| protected.iter().any(|s| covers(*s, target));
            !in_protected(use_stmt) && writes.iter().any(|write| in_protected(*write))
        }
        _ => false,
    })
}

/// Whether `target` is `root` itself or one of the statements nested in it.
pub fn stmt_covers_stmt(body: &Body, root: StmtIdx, target: StmtIdx) -> bool {
    if root == target {
        return true;
    }
    let covers = |stmts: &[StmtIdx]| stmts.iter().any(|s| stmt_covers_stmt(body, *s, target));
    match body.stmt_idx(root) {
        Stmt::If(if_stmt) => {
            covers(&if_stmt.then_branch)
                || if_stmt.elsif_branches.iter().any(|(_, branch)| covers(branch))
                || if_stmt.else_branch.as_ref().is_some_and(|branch| covers(branch))
        }
        Stmt::PreprocIf(pre) => {
            covers(&pre.then_branch)
                || pre.elsif_branches.iter().any(|(_, _, branch)| covers(branch))
                || pre.else_branch.as_ref().is_some_and(|branch| covers(branch))
        }
        Stmt::While { body: inner, .. }
        | Stmt::For { body: inner, .. }
        | Stmt::ForEach { body: inner, .. } => covers(inner),
        Stmt::Try { body: inner, except } => covers(inner) || covers(except),
        _ => false,
    }
}

/// What `guard` on the given branch proves about `ty` held by the guarded variable.
///
/// `ТипЗнч(Х) = Тип(…)` deliberately proves nothing here: equality of interned type
/// ids is not a subtyping test across facets, so a mistaken exclusion would hide a
/// real diagnostic rather than a false one. The `Неопределено` family needs no such
/// comparison — an arm either is that type or is not.
pub fn apply_branch_guard(
    db: &dyn TypeKernelDb,
    guard: &Guard,
    on_true: bool,
    ty: TypeId,
) -> GuardVerdict {
    let arms = arm_set_from_type_id(db, ty);
    if arms.is_empty() {
        // `Unknown`/`Any`/`Never` carry no arms: there is nothing to contradict.
        return GuardVerdict::NoInformation;
    }
    let is_undefined = |id: &TypeId| matches!(db.lookup_type(*id), TypeKind::Undefined);
    let survivors: Vec<TypeId> = match (guard, on_true) {
        (Guard::IsUndefined { .. }, true) | (Guard::IsNotUndefined { .. }, false) => {
            arms.iter().copied().filter(is_undefined).collect()
        }
        (Guard::IsUndefined { .. }, false) | (Guard::IsNotUndefined { .. }, true) => {
            arms.iter().copied().filter(|id| !is_undefined(id)).collect()
        }
        (Guard::ValueFilled { .. }, true) => {
            arms.iter().copied().filter(|id| !is_unfilled_witness(db, *id)).collect()
        }
        (Guard::ValueFilled { .. }, false) | (Guard::TypeCheck { .. }, _) => {
            return GuardVerdict::NoInformation
        }
    };
    if survivors.is_empty() {
        GuardVerdict::Contradicted
    } else if survivors.len() == arms.len() {
        GuardVerdict::NoInformation
    } else {
        GuardVerdict::Narrowed(db.union(survivors))
    }
}

pub fn narrow_or_base<DB: HirDatabase + ?Sized>(
    db: &DB,
    file_id: FileId,
    owner: DefWithBodyId,
    body: &Body,
    expr_id: ExprId,
    base: TypeId,
) -> TypeId {
    if !db.type_narrowing_enabled() {
        return base;
    }
    let Expr::Path(name) = body.expr(expr_id) else {
        return base;
    };
    let Some(result) = db.narrow(file_id, owner) else {
        return base;
    };
    narrowed_type_at(db, &result, body, expr_id.to_idx(), name).unwrap_or(base)
}

/// [`narrow_or_base`] for callers that hold the body's dataflow result and
/// its [`NarrowExprIndex`], paying the vertex lookup instead of a CFG scan.
pub fn narrow_or_base_indexed<DB: HirDatabase + ?Sized>(
    db: &DB,
    body: &Body,
    result: &dataflow::DataflowResult<NarrowState>,
    index: &NarrowExprIndex,
    expr_id: ExprId,
    base: TypeId,
) -> TypeId {
    let Expr::Path(name) = body.expr(expr_id) else {
        return base;
    };
    let narrowed = || -> Option<TypeId> {
        let node = index.vertex_of(expr_id.to_idx())?;
        let arms = result.block_in(node)?.get(name)?;
        if arms.is_empty() {
            return None;
        }
        Some(db.union(arms.to_vec()))
    };
    narrowed().unwrap_or(base)
}

fn containing_vertex(
    body: &Body,
    cfg: &cfg::ControlFlowGraph,
    expr_idx: ExprIdx,
) -> Option<cfg::NodeIndex> {
    use cfg::CfgVertex;

    for (node_idx, vertex) in cfg.vertices() {
        let covers = match vertex {
            CfgVertex::BasicBlock(bb) => bb
                .statements()
                .iter()
                .any(|stmt_id| stmt_covers_expr(body, stmt_id.to_idx(), expr_idx)),
            CfgVertex::Conditional(v) => expr_covers_expr(body, v.condition.to_idx(), expr_idx),
            CfgVertex::WhileHeader(v) => expr_covers_expr(body, v.condition.to_idx(), expr_idx),
            CfgVertex::ForHeader(v) => {
                expr_covers_expr(body, v.from.to_idx(), expr_idx)
                    || expr_covers_expr(body, v.to.to_idx(), expr_idx)
            }
            CfgVertex::ForEachHeader(v) => expr_covers_expr(body, v.collection.to_idx(), expr_idx),
            CfgVertex::Try | CfgVertex::PreprocCondition(_) | CfgVertex::Exit => false,
        };
        if covers {
            return Some(node_idx);
        }
    }
    None
}

/// Expression roots a statement contributes to its basic block, i.e. the
/// trees [`stmt_covers_expr`] searches. Control statements contribute none:
/// their conditions live on dedicated CFG vertices.
pub(crate) fn for_each_stmt_expr_root(body: &Body, stmt_idx: StmtIdx, f: &mut impl FnMut(ExprIdx)) {
    match body.stmt_idx(stmt_idx) {
        Stmt::Expr(e) => f(*e),
        Stmt::Assign { target: lhs, value } => {
            f(*lhs);
            f(*value);
        }
        Stmt::Return { value } | Stmt::Raise { value } => {
            if let Some(v) = value {
                f(*v);
            }
        }
        Stmt::Execute { expr } => f(*expr),
        Stmt::AddHandler { event, handler } | Stmt::RemoveHandler { event, handler } => {
            f(*event);
            f(*handler);
        }
        Stmt::VarDecl { .. } | Stmt::Break | Stmt::Continue | Stmt::Goto(_) | Stmt::Label(_) => {}
        Stmt::If(_)
        | Stmt::PreprocIf(_)
        | Stmt::While { .. }
        | Stmt::For { .. }
        | Stmt::ForEach { .. }
        | Stmt::Try { .. } => {}
    }
}

pub(crate) fn for_each_expr_child(body: &Body, root: ExprIdx, f: &mut impl FnMut(ExprIdx)) {
    match body.expr_idx(root) {
        Expr::Missing | Expr::Path(_) | Expr::Literal(_) => {}
        Expr::BinaryOp { lhs, rhs, .. } => {
            f(*lhs);
            f(*rhs);
        }
        Expr::UnaryOp { expr, .. } => f(*expr),
        Expr::Ternary { condition, then_expr, else_expr } => {
            f(*condition);
            f(*then_expr);
            f(*else_expr);
        }
        Expr::Call { callee, args } => {
            f(*callee);
            args.iter().copied().for_each(f);
        }
        Expr::MethodCall { receiver, args, .. } => {
            f(*receiver);
            args.iter().copied().for_each(f);
        }
        Expr::Index { base, index } => {
            f(*base);
            f(*index);
        }
        Expr::Field { base, .. } => f(*base),
        Expr::New { args, .. } => args.iter().copied().for_each(f),
        Expr::Array(elems) => elems.iter().copied().for_each(f),
        Expr::Await { expr } => f(*expr),
    }
}

fn stmt_covers_expr(body: &Body, stmt_idx: StmtIdx, target: ExprIdx) -> bool {
    let mut found = false;
    for_each_stmt_expr_root(body, stmt_idx, &mut |root| {
        if !found {
            found = expr_covers_expr(body, root, target);
        }
    });
    found
}

fn expr_covers_expr(body: &Body, root: ExprIdx, target: ExprIdx) -> bool {
    if root == target {
        return true;
    }
    let mut found = false;
    for_each_expr_child(body, root, &mut |child| {
        if !found {
            found = expr_covers_expr(body, child, target);
        }
    });
    found
}

/// Inverted [`containing_vertex`]: every expression of a body mapped to its
/// CFG vertex in one pass. `containing_vertex` walks all vertices per lookup,
/// which is fine for a one-off query but quadratic when a caller resolves
/// narrowed types for every path expression of a body (semantic
/// highlighting).
pub struct NarrowExprIndex {
    expr_to_vertex: FxHashMap<ExprIdx, cfg::NodeIndex>,
}

impl NarrowExprIndex {
    /// First vertex in `cfg.vertices()` order whose roots cover the
    /// expression — the same winner `containing_vertex` picks.
    pub fn build(body: &Body, cfg: &cfg::ControlFlowGraph) -> Self {
        use cfg::CfgVertex;

        let mut expr_to_vertex = FxHashMap::default();
        for (node_idx, vertex) in cfg.vertices() {
            let mut add_tree = |root: ExprIdx| {
                collect_expr_tree(body, root, node_idx, &mut expr_to_vertex);
            };
            match vertex {
                CfgVertex::BasicBlock(bb) => {
                    for stmt_id in bb.statements() {
                        for_each_stmt_expr_root(body, stmt_id.to_idx(), &mut add_tree);
                    }
                }
                CfgVertex::Conditional(v) => add_tree(v.condition.to_idx()),
                CfgVertex::WhileHeader(v) => add_tree(v.condition.to_idx()),
                CfgVertex::ForHeader(v) => {
                    add_tree(v.from.to_idx());
                    add_tree(v.to.to_idx());
                }
                CfgVertex::ForEachHeader(v) => add_tree(v.collection.to_idx()),
                CfgVertex::Try | CfgVertex::PreprocCondition(_) | CfgVertex::Exit => {}
            }
        }
        Self { expr_to_vertex }
    }

    pub fn vertex_of(&self, expr: ExprIdx) -> Option<cfg::NodeIndex> {
        self.expr_to_vertex.get(&expr).copied()
    }
}

fn collect_expr_tree(
    body: &Body,
    root: ExprIdx,
    node: cfg::NodeIndex,
    out: &mut FxHashMap<ExprIdx, cfg::NodeIndex>,
) {
    out.entry(root).or_insert(node);
    for_each_expr_child(body, root, &mut |child| collect_expr_tree(body, child, node, out));
}

#[cfg(test)]
mod tests {
    use super::*;
    use bsl_types::testing::InMemoryDb;
    use hir_def::hir::UnaryOp;

    fn kdb() -> InMemoryDb {
        InMemoryDb::new()
    }

    fn arm_set(db: &dyn TypeKernelDb, ids: Vec<TypeId>) -> Box<[TypeId]> {
        normalize_arms(db, ids)
    }

    fn overlay_type_id(db: &dyn TypeKernelDb, s: &NarrowState, name: &str) -> Option<TypeId> {
        let arms = s.get(&Name::new(name))?;
        Some(db.union(arms.to_vec()))
    }

    fn type_id_of_arms(db: &dyn TypeKernelDb, arms: &[TypeId]) -> TypeId {
        if arms.is_empty() {
            return db.unknown();
        }
        db.union(arms.to_vec())
    }

    struct ExprBuilder {
        body: Body,
    }

    impl ExprBuilder {
        fn new() -> Self {
            Self { body: Body::new() }
        }

        fn alloc(&mut self, expr: Expr) -> ExprIdx {
            self.body.exprs_mut().alloc(expr)
        }

        fn path(&mut self, name: &str) -> ExprIdx {
            self.alloc(Expr::Path(Name::new(name)))
        }

        fn string_lit(&mut self, s: &str) -> ExprIdx {
            self.alloc(Expr::Literal(Literal::String(s.to_string())))
        }

        fn undefined(&mut self) -> ExprIdx {
            self.alloc(Expr::Literal(Literal::Undefined))
        }

        fn call(&mut self, callee: ExprIdx, args: Vec<ExprIdx>) -> ExprIdx {
            self.alloc(Expr::Call { callee, args: args.into_boxed_slice() })
        }

        fn bin(&mut self, lhs: ExprIdx, rhs: ExprIdx, op: BinaryOp) -> ExprIdx {
            self.alloc(Expr::BinaryOp { lhs, rhs, op })
        }

        fn assign(&mut self, target: ExprIdx, value: ExprIdx) -> StmtIdx {
            self.body.stmts_mut().alloc(Stmt::Assign { target, value })
        }

        fn if_then(&mut self, condition: ExprIdx, then_stmt: StmtIdx) -> StmtIdx {
            let if_stmt = hir_def::hir::IfStmt {
                condition,
                then_branch: Box::from([then_stmt]),
                elsif_branches: Box::from([]),
                else_branch: None,
            };
            self.body.stmts_mut().alloc(Stmt::If(Box::new(if_stmt)))
        }

        fn if_then_else(
            &mut self,
            condition: ExprIdx,
            then_stmt: StmtIdx,
            else_stmt: StmtIdx,
        ) -> StmtIdx {
            let if_stmt = hir_def::hir::IfStmt {
                condition,
                then_branch: Box::from([then_stmt]),
                elsif_branches: Box::from([]),
                else_branch: Some(Box::from([else_stmt])),
            };
            self.body.stmts_mut().alloc(Stmt::If(Box::new(if_stmt)))
        }

        fn if_then_elsif(
            &mut self,
            condition: ExprIdx,
            then_stmt: StmtIdx,
            elsif_cond: ExprIdx,
            elsif_stmt: StmtIdx,
        ) -> StmtIdx {
            let if_stmt = hir_def::hir::IfStmt {
                condition,
                then_branch: Box::from([then_stmt]),
                elsif_branches: Box::from([(elsif_cond, Box::from([elsif_stmt]))]),
                else_branch: None,
            };
            self.body.stmts_mut().alloc(Stmt::If(Box::new(if_stmt)))
        }

        fn while_stmt(&mut self, condition: ExprIdx, body_stmt: StmtIdx) -> StmtIdx {
            self.body.stmts_mut().alloc(Stmt::While { condition, body: Box::from([body_stmt]) })
        }

        fn set_top_level(&mut self, stmts: Vec<StmtIdx>) {
            self.body.set_body_stmts(stmts.into_boxed_slice());
        }
    }

    #[test]
    fn recognizes_type_check_direct() {
        let mut b = ExprBuilder::new();
        let x = b.path("Х");
        let typznc_callee = b.path("ТипЗнч");
        let lhs = b.call(typznc_callee, vec![x]);
        let tip_callee = b.path("Тип");
        let s = b.string_lit("Строка");
        let rhs = b.call(tip_callee, vec![s]);
        let guard = b.bin(lhs, rhs, BinaryOp::Eq);

        assert_eq!(
            recognize_guard(guard, &b.body),
            Some(Guard::TypeCheck { var: Name::new("Х"), type_name: "Строка".to_string() })
        );
    }

    #[test]
    fn recognizes_type_check_reversed() {
        let mut b = ExprBuilder::new();
        let tip_callee = b.path("Тип");
        let s = b.string_lit("Массив");
        let lhs = b.call(tip_callee, vec![s]);
        let x = b.path("Данные");
        let typznc_callee = b.path("ТипЗнч");
        let rhs = b.call(typznc_callee, vec![x]);
        let guard = b.bin(lhs, rhs, BinaryOp::Eq);

        assert_eq!(
            recognize_guard(guard, &b.body),
            Some(Guard::TypeCheck {
                var: Name::new("Данные"), type_name: "Массив".to_string()
            })
        );
    }

    #[test]
    fn recognizes_type_check_english_spelling() {
        let mut b = ExprBuilder::new();
        let x = b.path("X");
        let typeof_callee = b.path("TypeOf");
        let lhs = b.call(typeof_callee, vec![x]);
        let type_callee = b.path("Type");
        let s = b.string_lit("String");
        let rhs = b.call(type_callee, vec![s]);
        let guard = b.bin(lhs, rhs, BinaryOp::Eq);

        assert_eq!(
            recognize_guard(guard, &b.body),
            Some(Guard::TypeCheck { var: Name::new("X"), type_name: "String".to_string() })
        );
    }

    #[test]
    fn recognizes_type_check_case_insensitive() {
        let mut b = ExprBuilder::new();
        let x = b.path("а");
        let typznc_callee = b.path("тИпЗнЧ");
        let lhs = b.call(typznc_callee, vec![x]);
        let tip_callee = b.path("ТИП");
        let s = b.string_lit("Число");
        let rhs = b.call(tip_callee, vec![s]);
        let guard = b.bin(lhs, rhs, BinaryOp::Eq);

        assert!(matches!(recognize_guard(guard, &b.body), Some(Guard::TypeCheck { .. })));
    }

    #[test]
    fn recognizes_is_undefined() {
        let mut b = ExprBuilder::new();
        let lhs = b.path("Х");
        let rhs = b.undefined();
        let guard = b.bin(lhs, rhs, BinaryOp::Eq);

        assert_eq!(
            recognize_guard(guard, &b.body),
            Some(Guard::IsUndefined { var: Name::new("Х") })
        );
    }

    #[test]
    fn recognizes_is_undefined_reversed() {
        let mut b = ExprBuilder::new();
        let lhs = b.undefined();
        let rhs = b.path("Х");
        let guard = b.bin(lhs, rhs, BinaryOp::Eq);

        assert_eq!(
            recognize_guard(guard, &b.body),
            Some(Guard::IsUndefined { var: Name::new("Х") })
        );
    }

    #[test]
    fn recognizes_is_not_undefined() {
        let mut b = ExprBuilder::new();
        let lhs = b.path("Х");
        let rhs = b.undefined();
        let guard = b.bin(lhs, rhs, BinaryOp::Neq);

        assert_eq!(
            recognize_guard(guard, &b.body),
            Some(Guard::IsNotUndefined { var: Name::new("Х") })
        );
    }

    #[test]
    fn recognizes_is_not_undefined_reversed() {
        let mut b = ExprBuilder::new();
        let lhs = b.undefined();
        let rhs = b.path("Х");
        let guard = b.bin(lhs, rhs, BinaryOp::Neq);

        assert_eq!(
            recognize_guard(guard, &b.body),
            Some(Guard::IsNotUndefined { var: Name::new("Х") })
        );
    }

    #[test]
    fn recognizes_value_filled() {
        let mut b = ExprBuilder::new();
        let x = b.path("Х");
        let callee = b.path("ЗначениеЗаполнено");
        let call = b.call(callee, vec![x]);

        assert_eq!(
            recognize_guard(call, &b.body),
            Some(Guard::ValueFilled { var: Name::new("Х") })
        );
    }

    #[test]
    fn recognizes_value_filled_english() {
        let mut b = ExprBuilder::new();
        let x = b.path("X");
        let callee = b.path("ValueIsFilled");
        let call = b.call(callee, vec![x]);

        assert_eq!(
            recognize_guard(call, &b.body),
            Some(Guard::ValueFilled { var: Name::new("X") })
        );
    }

    #[test]
    fn does_not_recognize_random_binary_op() {
        let mut b = ExprBuilder::new();
        let lhs = b.path("Х");
        let rhs = b.alloc(Expr::Literal(Literal::Number(1.0.try_into().unwrap())));
        let expr = b.bin(lhs, rhs, BinaryOp::Add);

        assert_eq!(recognize_guard(expr, &b.body), None);
    }

    #[test]
    fn does_not_recognize_negated_type_check() {
        let mut b = ExprBuilder::new();
        let x = b.path("Х");
        let typznc_callee = b.path("ТипЗнч");
        let lhs = b.call(typznc_callee, vec![x]);
        let tip_callee = b.path("Тип");
        let s = b.string_lit("Строка");
        let rhs = b.call(tip_callee, vec![s]);
        let guard = b.bin(lhs, rhs, BinaryOp::Neq);

        assert_eq!(recognize_guard(guard, &b.body), None);
    }

    #[test]
    fn does_not_recognize_unary_not() {
        let mut b = ExprBuilder::new();
        let x = b.path("Х");
        let callee = b.path("ЗначениеЗаполнено");
        let call = b.call(callee, vec![x]);
        let negated = b.alloc(Expr::UnaryOp { expr: call, op: UnaryOp::Not });

        assert_eq!(recognize_guard(negated, &b.body), None);
    }

    #[test]
    fn does_not_recognize_or_composition() {
        let mut b = ExprBuilder::new();

        let build_tc = |b: &mut ExprBuilder, type_lit: &str| {
            let x = b.path("Х");
            let tz = b.path("ТипЗнч");
            let lhs = b.call(tz, vec![x]);
            let tp = b.path("Тип");
            let s = b.string_lit(type_lit);
            let rhs = b.call(tp, vec![s]);
            b.bin(lhs, rhs, BinaryOp::Eq)
        };

        let left = build_tc(&mut b, "Строка");
        let right = build_tc(&mut b, "Число");
        let or_expr = b.bin(left, right, BinaryOp::Or);

        assert_eq!(recognize_guard(or_expr, &b.body), None);
    }

    #[test]
    fn does_not_recognize_type_check_with_non_string_arg() {
        let mut b = ExprBuilder::new();
        let x = b.path("Х");
        let tz = b.path("ТипЗнч");
        let lhs = b.call(tz, vec![x]);
        let tp = b.path("Тип");
        let dynamic_arg = b.path("СомеПеременная");
        let rhs = b.call(tp, vec![dynamic_arg]);
        let guard = b.bin(lhs, rhs, BinaryOp::Eq);

        assert_eq!(recognize_guard(guard, &b.body), None);
    }

    #[test]
    fn does_not_recognize_multi_arg_type_of() {
        let mut b = ExprBuilder::new();
        let x = b.path("Х");
        let y = b.path("Y");
        let tz = b.path("ТипЗнч");
        let lhs = b.call(tz, vec![x, y]);
        let tp = b.path("Тип");
        let s = b.string_lit("Строка");
        let rhs = b.call(tp, vec![s]);
        let guard = b.bin(lhs, rhs, BinaryOp::Eq);

        assert_eq!(recognize_guard(guard, &b.body), None);
    }

    #[test]
    fn does_not_recognize_value_filled_with_no_args() {
        let mut b = ExprBuilder::new();
        let callee = b.path("ЗначениеЗаполнено");
        let call = b.call(callee, vec![]);

        assert_eq!(recognize_guard(call, &b.body), None);
    }

    #[test]
    fn does_not_recognize_value_filled_with_literal_arg() {
        let mut b = ExprBuilder::new();
        let lit = b.string_lit("hi");
        let callee = b.path("ЗначениеЗаполнено");
        let call = b.call(callee, vec![lit]);

        assert_eq!(recognize_guard(call, &b.body), None);
    }

    #[test]
    fn does_not_recognize_field_receiver() {
        let mut b = ExprBuilder::new();
        let obj = b.path("Объект");
        let field = b.alloc(Expr::Field { base: obj, field: Name::new("Поле") });
        let rhs = b.undefined();
        let guard = b.bin(field, rhs, BinaryOp::Eq);

        assert_eq!(recognize_guard(guard, &b.body), None);
    }

    #[test]
    fn does_not_recognize_missing_literal() {
        let mut b = ExprBuilder::new();
        let lhs = b.path("Х");
        let rhs = b.alloc(Expr::Literal(Literal::Number(1.0.try_into().unwrap())));
        let guard = b.bin(lhs, rhs, BinaryOp::Eq);

        assert_eq!(recognize_guard(guard, &b.body), None);
    }

    #[test]
    fn does_not_recognize_path_eq_path() {
        let mut b = ExprBuilder::new();
        let lhs = b.path("Х");
        let rhs = b.path("Y");
        let guard = b.bin(lhs, rhs, BinaryOp::Eq);

        assert_eq!(recognize_guard(guard, &b.body), None);
    }

    #[test]
    fn does_not_recognize_type_check_on_type_check() {
        let mut b = ExprBuilder::new();
        let x = b.path("Х");
        let tz1 = b.path("ТипЗнч");
        let lhs = b.call(tz1, vec![x]);
        let y = b.path("Y");
        let tz2 = b.path("ТипЗнч");
        let rhs = b.call(tz2, vec![y]);
        let guard = b.bin(lhs, rhs, BinaryOp::Eq);

        assert_eq!(recognize_guard(guard, &b.body), None);
    }

    #[test]
    fn does_not_recognize_ternary() {
        let mut b = ExprBuilder::new();
        let x = b.path("Х");
        let und = b.undefined();
        let condition = b.bin(x, und, BinaryOp::Eq);
        let then_expr = b.path("А");
        let else_expr = b.path("Б");
        let ternary = b.alloc(Expr::Ternary { condition, then_expr, else_expr });

        assert_eq!(recognize_guard(ternary, &b.body), None);
    }

    #[test]
    fn does_not_recognize_value_filled_on_field() {
        let mut b = ExprBuilder::new();
        let obj = b.path("Объект");
        let field = b.alloc(Expr::Field { base: obj, field: Name::new("Поле") });
        let callee = b.path("ЗначениеЗаполнено");
        let call = b.call(callee, vec![field]);

        assert_eq!(recognize_guard(call, &b.body), None);
    }

    fn state_with(db: &dyn TypeKernelDb, entries: &[(&str, TypeId)]) -> NarrowState {
        let mut s = NarrowState::new();
        for (n, id) in entries {
            let arms = arm_set(db, vec![*id]);
            if !arms.is_empty() {
                s.narrowed.insert(fold_name(&Name::new(n)), arms);
            }
        }
        s
    }

    #[test]
    fn lattice_join_empty_with_empty_is_empty() {
        let a = NarrowState::new();
        let b = NarrowState::new();
        assert!(a.join(&b).is_empty());
    }

    #[test]
    fn lattice_join_drops_one_sided_entry() {
        let db = kdb();
        let a = state_with(&db, &[("Х", db.string(None, false))]);
        let b = NarrowState::new();
        let joined = a.join(&b);
        assert_eq!(joined.get(&Name::new("Х")), None);

        let joined_rev = b.join(&a);
        assert_eq!(joined_rev.get(&Name::new("Х")), None);
    }

    #[test]
    fn lattice_join_equal_entries_stay_equal() {
        let db = kdb();
        let a = state_with(&db, &[("Х", db.string(None, false))]);
        let b = state_with(&db, &[("Х", db.string(None, false))]);
        let joined = a.join(&b);
        assert_eq!(overlay_type_id(&db, &joined, "Х"), Some(db.string(None, false)));
    }

    #[test]
    fn lattice_join_different_entries_go_to_union() {
        let db = kdb();
        let a = state_with(&db, &[("Х", db.string(None, false))]);
        let b = state_with(&db, &[("Х", db.number(None, None))]);
        let joined = a.join(&b);
        let expected = db.union(vec![db.string(None, false), db.number(None, None)]);
        assert_eq!(overlay_type_id(&db, &joined, "Х"), Some(expected));
    }

    #[test]
    fn lattice_join_clears_pending_guard() {
        let mut a = NarrowState::new();
        a.pending_guard = Some(Guard::IsUndefined { var: Name::new("Х") });
        let b = NarrowState::new();
        assert!(a.join(&b).pending_guard.is_none());
        assert!(b.join(&a).pending_guard.is_none());
    }

    fn transfer_no_bases(db: &dyn TypeKernelDb) -> NarrowingTransfer<'_> {
        NarrowingTransfer::new(db, FxHashMap::default())
    }

    fn transfer_with_bases<'a>(
        db: &'a dyn TypeKernelDb,
        entries: &[(&str, TypeId)],
    ) -> NarrowingTransfer<'a> {
        let mut bases = FxHashMap::default();
        for (name, id) in entries {
            bases.insert(fold_name(&Name::new(name)), *id);
        }
        NarrowingTransfer::new(db, bases)
    }

    #[test]
    fn apply_guard_type_check_true_maps_to_named_ty() {
        let db = kdb();
        let tr = transfer_no_bases(&db);
        let mut s = NarrowState::new();
        tr.apply_guard(
            &mut s,
            &Guard::TypeCheck { var: Name::new("Х"), type_name: "Строка".to_string() },
            true,
        );
        assert_eq!(overlay_type_id(&db, &s, "Х"), Some(db.string(None, false)));
    }

    #[test]
    fn apply_guard_type_check_true_promotes_array_to_typed_array_base() {
        let db = kdb();
        let tr = transfer_with_bases(&db, &[("М", db.array(Some(db.string(None, false))))]);
        let mut s = NarrowState::new();
        tr.apply_guard(
            &mut s,
            &Guard::TypeCheck { var: Name::new("М"), type_name: "Массив".to_string() },
            true,
        );
        assert_eq!(overlay_type_id(&db, &s, "М"), Some(db.array(Some(db.string(None, false)))));
    }

    #[test]
    fn apply_guard_type_check_true_promotes_array_through_union_base() {
        let db = kdb();
        let typed = db.array(Some(db.number(None, None)));
        let tr = transfer_with_bases(&db, &[("М", db.union(vec![typed, db.undefined()]))]);
        let mut s = NarrowState::new();
        tr.apply_guard(
            &mut s,
            &Guard::TypeCheck { var: Name::new("М"), type_name: "Массив".to_string() },
            true,
        );
        assert_eq!(overlay_type_id(&db, &s, "М"), Some(typed));
    }

    #[test]
    fn apply_guard_type_check_true_preserves_both_array_and_typed_array_arms() {
        let db = kdb();
        let typed = db.array(Some(db.string(None, false)));
        let tr = transfer_with_bases(
            &db,
            &[("М", db.union(vec![typed, db.array(None), db.number(None, None)]))],
        );
        let mut s = NarrowState::new();
        tr.apply_guard(
            &mut s,
            &Guard::TypeCheck { var: Name::new("М"), type_name: "Массив".to_string() },
            true,
        );
        let expected = db.union(vec![typed, db.array(None)]);
        assert_eq!(overlay_type_id(&db, &s, "М"), Some(expected));
    }

    #[test]
    fn apply_guard_type_check_false_removes_typed_array_arm() {
        let db = kdb();
        let typed = db.array(Some(db.string(None, false)));
        let tr = transfer_with_bases(&db, &[("М", db.union(vec![typed, db.number(None, None)]))]);
        let mut s = NarrowState::new();
        tr.apply_guard(
            &mut s,
            &Guard::TypeCheck { var: Name::new("М"), type_name: "Массив".to_string() },
            false,
        );
        assert_eq!(overlay_type_id(&db, &s, "М"), Some(db.number(None, None)));
    }

    #[test]
    fn apply_guard_type_check_false_drops_typed_array_only_base_to_dead() {
        let db = kdb();
        let typed = db.array(Some(db.string(None, false)));
        let tr = transfer_with_bases(&db, &[("М", typed)]);
        let mut s = NarrowState::new();
        tr.apply_guard(
            &mut s,
            &Guard::TypeCheck { var: Name::new("М"), type_name: "Массив".to_string() },
            false,
        );
        assert_eq!(s.get(&Name::new("М")), None);
    }

    #[test]
    fn apply_guard_type_check_true_keeps_array_when_base_has_no_typed_array() {
        let db = kdb();
        let tr = transfer_with_bases(
            &db,
            &[("М", db.union(vec![db.number(None, None), db.string(None, false)]))],
        );
        let mut s = NarrowState::new();
        tr.apply_guard(
            &mut s,
            &Guard::TypeCheck { var: Name::new("М"), type_name: "Массив".to_string() },
            true,
        );
        assert_eq!(overlay_type_id(&db, &s, "М"), Some(db.array(None)));
    }

    #[test]
    fn apply_guard_type_check_false_without_base_is_overlay_noop() {
        let db = kdb();
        let tr = transfer_no_bases(&db);
        let mut s = NarrowState::new();
        tr.apply_guard(
            &mut s,
            &Guard::TypeCheck { var: Name::new("Х"), type_name: "Строка".to_string() },
            false,
        );
        assert_eq!(s.get(&Name::new("Х")), None);
    }

    #[test]
    fn apply_guard_type_check_false_narrows_binary_union_to_singleton() {
        let db = kdb();
        let tr = transfer_with_bases(
            &db,
            &[("Х", db.union(vec![db.number(None, None), db.string(None, false)]))],
        );
        let mut s = NarrowState::new();
        tr.apply_guard(
            &mut s,
            &Guard::TypeCheck { var: Name::new("Х"), type_name: "Строка".to_string() },
            false,
        );
        assert_eq!(overlay_type_id(&db, &s, "Х"), Some(db.number(None, None)));
    }

    #[test]
    fn apply_guard_type_check_false_narrows_ternary_union_to_union() {
        let db = kdb();
        let tr = transfer_with_bases(
            &db,
            &[(
                "Х",
                db.union(vec![
                    db.number(None, None),
                    db.string(None, false),
                    db.date(DateComponent::DateTime),
                ]),
            )],
        );
        let mut s = NarrowState::new();
        tr.apply_guard(
            &mut s,
            &Guard::TypeCheck { var: Name::new("Х"), type_name: "Строка".to_string() },
            false,
        );
        let expected = db.union(vec![db.number(None, None), db.date(DateComponent::DateTime)]);
        assert_eq!(overlay_type_id(&db, &s, "Х"), Some(expected));
    }

    #[test]
    fn apply_guard_type_check_false_on_exhausted_union_is_overlay_noop() {
        let db = kdb();
        let tr = transfer_with_bases(&db, &[("Х", db.string(None, false))]);
        let mut s = NarrowState::new();
        tr.apply_guard(
            &mut s,
            &Guard::TypeCheck { var: Name::new("Х"), type_name: "Строка".to_string() },
            false,
        );
        assert_eq!(s.get(&Name::new("Х")), None);
    }

    #[test]
    fn apply_guard_imprecise_refinement_preserves_prior_narrowing() {
        let db = kdb();
        let tr = transfer_no_bases(&db);
        let mut s = state_with(&db, &[("Х", db.string(None, false))]);
        tr.apply_guard(
            &mut s,
            &Guard::TypeCheck { var: Name::new("Х"), type_name: "Число".to_string() },
            false,
        );
        assert_eq!(
            overlay_type_id(&db, &s, "Х"),
            Some(db.string(None, false)),
            "imprecise refinement must leave prior narrowing intact"
        );
    }

    #[test]
    fn apply_guard_is_undefined_true_maps_to_undefined() {
        let db = kdb();
        let tr = transfer_no_bases(&db);
        let mut s = NarrowState::new();
        tr.apply_guard(&mut s, &Guard::IsUndefined { var: Name::new("Х") }, true);
        assert_eq!(overlay_type_id(&db, &s, "Х"), Some(db.undefined()));
    }

    #[test]
    fn apply_guard_is_undefined_false_narrows_union_minus_undefined() {
        let db = kdb();
        let tr = transfer_with_bases(
            &db,
            &[("Х", db.union(vec![db.string(None, false), db.undefined()]))],
        );
        let mut s = NarrowState::new();
        tr.apply_guard(&mut s, &Guard::IsUndefined { var: Name::new("Х") }, false);
        assert_eq!(overlay_type_id(&db, &s, "Х"), Some(db.string(None, false)));
    }

    #[test]
    fn apply_guard_is_not_undefined_false_maps_to_undefined() {
        let db = kdb();
        let tr = transfer_no_bases(&db);
        let mut s = NarrowState::new();
        tr.apply_guard(&mut s, &Guard::IsNotUndefined { var: Name::new("Х") }, false);
        assert_eq!(overlay_type_id(&db, &s, "Х"), Some(db.undefined()));
    }

    #[test]
    fn apply_guard_is_not_undefined_true_narrows_union_minus_undefined() {
        let db = kdb();
        let tr = transfer_with_bases(
            &db,
            &[("Х", db.union(vec![db.string(None, false), db.undefined()]))],
        );
        let mut s = NarrowState::new();
        tr.apply_guard(&mut s, &Guard::IsNotUndefined { var: Name::new("Х") }, true);
        assert_eq!(overlay_type_id(&db, &s, "Х"), Some(db.string(None, false)));
    }

    #[test]
    fn apply_guard_value_filled_true_strips_undefined_and_null() {
        let db = kdb();
        let tr = transfer_with_bases(
            &db,
            &[("Х", db.union(vec![db.string(None, false), db.undefined(), db.null()]))],
        );
        let mut s = NarrowState::new();
        tr.apply_guard(&mut s, &Guard::ValueFilled { var: Name::new("Х") }, true);
        assert_eq!(overlay_type_id(&db, &s, "Х"), Some(db.string(None, false)));
    }

    #[test]
    fn apply_guard_value_filled_true_strips_only_null() {
        let db = kdb();
        let tr =
            transfer_with_bases(&db, &[("Х", db.union(vec![db.number(None, None), db.null()]))]);
        let mut s = NarrowState::new();
        tr.apply_guard(&mut s, &Guard::ValueFilled { var: Name::new("Х") }, true);
        assert_eq!(overlay_type_id(&db, &s, "Х"), Some(db.number(None, None)));
    }

    #[test]
    fn apply_guard_value_filled_false_leaves_overlay_untouched() {
        let db = kdb();
        let tr = transfer_with_bases(
            &db,
            &[("Х", db.union(vec![db.string(None, false), db.undefined(), db.null()]))],
        );
        let mut s = NarrowState::new();
        tr.apply_guard(&mut s, &Guard::ValueFilled { var: Name::new("Х") }, false);
        assert_eq!(s.get(&Name::new("Х")), None);
    }

    #[test]
    fn apply_guard_value_filled_true_no_witness_in_base_is_noop() {
        let db = kdb();
        let base = db.union(vec![db.number(None, None), db.string(None, false)]);
        let tr = transfer_with_bases(&db, &[("Х", base)]);
        let mut s = NarrowState::new();
        tr.apply_guard(&mut s, &Guard::ValueFilled { var: Name::new("Х") }, true);
        assert_eq!(s.get(&Name::new("Х")), None);
    }

    #[test]
    fn apply_guard_value_filled_true_preserves_prior_overlay_when_no_witness() {
        let db = kdb();
        let base = db.union(vec![db.number(None, None), db.string(None, false)]);
        let tr = transfer_with_bases(&db, &[("Х", base)]);
        let mut s = NarrowState::new();
        s.narrowed.insert(fold_name(&Name::new("Х")), arm_set(&db, vec![db.string(None, false)]));
        tr.apply_guard(&mut s, &Guard::ValueFilled { var: Name::new("Х") }, true);
        assert_eq!(overlay_type_id(&db, &s, "Х"), Some(db.string(None, false)));
    }

    #[test]
    fn ty_difference_union_minus_member_collapses_to_singleton() {
        let db = kdb();
        let base = db.union(vec![db.number(None, None), db.string(None, false)]);
        let arms = ty_difference(&db, base, db.string(None, false));
        assert_eq!(type_id_of_arms(&db, &arms), db.number(None, None));
    }

    #[test]
    fn ty_difference_union_minus_missing_member_returns_whole_union() {
        let db = kdb();
        let base = db.union(vec![db.number(None, None), db.string(None, false)]);
        let arms = ty_difference(&db, base, db.date(DateComponent::DateTime));
        assert_eq!(type_id_of_arms(&db, &arms), base);
    }

    #[test]
    fn ty_difference_multi_member_union_keeps_residue_as_union() {
        let db = kdb();
        let base = db.union(vec![
            db.number(None, None),
            db.string(None, false),
            db.date(DateComponent::DateTime),
        ]);
        let arms = ty_difference(&db, base, db.string(None, false));
        let expected = db.union(vec![db.number(None, None), db.date(DateComponent::DateTime)]);
        assert_eq!(type_id_of_arms(&db, &arms), expected);
    }

    #[test]
    fn ty_difference_non_union_base_returns_unknown() {
        let db = kdb();
        let s = db.string(None, false);
        let n = db.number(None, None);
        assert_eq!(type_id_of_arms(&db, &ty_difference(&db, s, s)), db.unknown());
        assert_eq!(type_id_of_arms(&db, &ty_difference(&db, s, n)), db.unknown());
    }

    #[test]
    fn ty_difference_unfilled_witnesses_strips_both_undefined_and_null() {
        let db = kdb();
        let base = db.union(vec![db.string(None, false), db.undefined(), db.null()]);
        assert_eq!(
            type_id_of_arms(&db, &ty_difference_unfilled_witnesses(&db, base)),
            db.string(None, false)
        );
    }

    #[test]
    fn ty_difference_unfilled_witnesses_keeps_other_arms_untouched() {
        let db = kdb();
        let base = db.union(vec![
            db.number(None, None),
            db.string(None, false),
            db.undefined(),
            db.null(),
        ]);
        let expected = db.union(vec![db.number(None, None), db.string(None, false)]);
        assert_eq!(type_id_of_arms(&db, &ty_difference_unfilled_witnesses(&db, base)), expected);
    }

    #[test]
    fn ty_difference_unfilled_witnesses_non_union_collapses_to_unknown() {
        let db = kdb();
        let s = db.string(None, false);
        let u = db.undefined();
        assert_eq!(type_id_of_arms(&db, &ty_difference_unfilled_witnesses(&db, s)), db.unknown());
        assert_eq!(type_id_of_arms(&db, &ty_difference_unfilled_witnesses(&db, u)), db.unknown());
    }

    #[test]
    fn is_unfilled_witness_recognizes_only_undefined_and_null() {
        let db = kdb();
        assert!(is_unfilled_witness(&db, db.undefined()));
        assert!(is_unfilled_witness(&db, db.null()));
        assert!(!is_unfilled_witness(&db, db.string(None, false)));
        assert!(!is_unfilled_witness(&db, db.number(None, None)));
        assert!(!is_unfilled_witness(&db, db.date(DateComponent::DateTime)));
    }

    #[test]
    fn ty_difference_chain_to_exhaustion_stays_sound() {
        let db = kdb();
        let base = db.union(vec![db.number(None, None), db.string(None, false)]);
        let step1 = ty_difference(&db, base, db.number(None, None));
        assert_eq!(type_id_of_arms(&db, &step1), db.string(None, false));
        let step1_id = db.union(step1.to_vec());
        let step2 = ty_difference(&db, step1_id, db.string(None, false));
        assert_eq!(type_id_of_arms(&db, &step2), db.unknown());
    }

    #[test]
    fn transfer_expr_stashes_recognized_guard() {
        let mut b = ExprBuilder::new();
        let x = b.path("Х");
        let und = b.undefined();
        let condition = b.bin(x, und, BinaryOp::Eq);

        let db = kdb();
        let tr = transfer_no_bases(&db);
        let state = tr.transfer_expr(ExprId::from_idx(condition), &NarrowState::new(), &b.body);
        assert_eq!(state.pending_guard, Some(Guard::IsUndefined { var: Name::new("Х") }));
    }

    #[test]
    fn transfer_expr_non_guard_condition_clears_pending() {
        let mut b = ExprBuilder::new();
        let x = b.path("Х");
        let one = b.alloc(Expr::Literal(Literal::Number(1.0.try_into().unwrap())));
        let condition = b.bin(x, one, BinaryOp::Gt);

        let mut initial = NarrowState::new();
        initial.pending_guard = Some(Guard::IsUndefined { var: Name::new("Stale") });
        let db = kdb();
        let tr = transfer_no_bases(&db);
        let state = tr.transfer_expr(ExprId::from_idx(condition), &initial, &b.body);
        assert!(state.pending_guard.is_none());
    }

    #[test]
    fn transfer_edge_true_branch_applies_pending_guard() {
        let db = kdb();
        let tr = transfer_no_bases(&db);
        let mut state = NarrowState::new();
        state.pending_guard =
            Some(Guard::TypeCheck { var: Name::new("Х"), type_name: "Число".to_string() });
        let out = tr.transfer_edge(CfgEdgeType::TrueBranch, &state);
        assert_eq!(overlay_type_id(&db, &out, "Х"), Some(db.number(None, None)));
        assert!(out.pending_guard.is_none(), "guard must be consumed");
    }

    #[test]
    fn transfer_edge_false_branch_applies_pending_guard() {
        let db = kdb();
        let tr = transfer_no_bases(&db);
        let mut state = NarrowState::new();
        state.pending_guard = Some(Guard::IsNotUndefined { var: Name::new("Х") });
        let out = tr.transfer_edge(CfgEdgeType::FalseBranch, &state);
        assert_eq!(overlay_type_id(&db, &out, "Х"), Some(db.undefined()));
        assert!(out.pending_guard.is_none());
    }

    #[test]
    fn transfer_edge_direct_clears_pending_without_applying() {
        let db = kdb();
        let tr = transfer_no_bases(&db);
        let mut state = state_with(&db, &[("Х", db.string(None, false))]);
        state.pending_guard = Some(Guard::IsUndefined { var: Name::new("Х") });
        let out = tr.transfer_edge(CfgEdgeType::Unconditional, &state);
        assert_eq!(
            overlay_type_id(&db, &out, "Х"),
            Some(db.string(None, false)),
            "narrowing must be untouched"
        );
        assert!(out.pending_guard.is_none(), "guard must be cleared on an unconditional edge");
    }

    #[test]
    fn transfer_stmt_assign_from_untyped_rhs_drops_narrowed_entry() {
        let mut b = ExprBuilder::new();
        let x_tgt = b.path("Х");
        let y_val = b.path("Y");
        let assign = b.assign(x_tgt, y_val);

        let db = kdb();
        let tr = transfer_no_bases(&db);
        let state_in = state_with(&db, &[("Х", db.string(None, false))]);
        let state_out = tr.transfer_stmt(assign.into_raw(), &state_in, &b.body);
        assert_eq!(state_out.get(&Name::new("Х")), None);
    }

    #[test]
    fn transfer_stmt_assign_to_non_path_preserves_narrowed() {
        let mut b = ExprBuilder::new();
        let obj = b.path("Объект");
        let target = b.alloc(Expr::Field { base: obj, field: Name::new("Поле") });
        let one = b.alloc(Expr::Literal(Literal::Number(1.0.try_into().unwrap())));
        let assign = b.assign(target, one);

        let db = kdb();
        let tr = transfer_no_bases(&db);
        let state_in = state_with(&db, &[("Х", db.string(None, false))]);
        let state_out = tr.transfer_stmt(assign.into_raw(), &state_in, &b.body);
        assert_eq!(overlay_type_id(&db, &state_out, "Х"), Some(db.string(None, false)));
    }

    #[test]
    fn transfer_stmt_assign_number_literal_records_number() {
        let mut b = ExprBuilder::new();
        let x_tgt = b.path("Х");
        let num = b.alloc(Expr::Literal(Literal::Number(42.0.try_into().unwrap())));
        let assign = b.assign(x_tgt, num);

        let db = kdb();
        let tr = transfer_no_bases(&db);
        let state_in = state_with(&db, &[("Х", db.string(None, false))]);
        let state_out = tr.transfer_stmt(assign.into_raw(), &state_in, &b.body);
        assert_eq!(overlay_type_id(&db, &state_out, "Х"), Some(db.number(None, None)));
    }

    #[test]
    fn transfer_stmt_assign_string_literal_records_string() {
        let mut b = ExprBuilder::new();
        let x_tgt = b.path("Х");
        let s = b.string_lit("hello");
        let assign = b.assign(x_tgt, s);

        let db = kdb();
        let tr = transfer_no_bases(&db);
        let state_out = tr.transfer_stmt(assign.into_raw(), &NarrowState::new(), &b.body);
        assert_eq!(overlay_type_id(&db, &state_out, "Х"), Some(db.string(None, false)));
    }

    #[test]
    fn transfer_stmt_assign_undefined_literal_records_undefined() {
        let mut b = ExprBuilder::new();
        let x_tgt = b.path("Х");
        let und = b.undefined();
        let assign = b.assign(x_tgt, und);

        let db = kdb();
        let tr = transfer_no_bases(&db);
        let state_out = tr.transfer_stmt(assign.into_raw(), &NarrowState::new(), &b.body);
        assert_eq!(overlay_type_id(&db, &state_out, "Х"), Some(db.undefined()));
    }

    #[test]
    fn transfer_stmt_assign_bool_literal_records_boolean() {
        let mut b = ExprBuilder::new();
        let x_tgt = b.path("Х");
        let v = b.alloc(Expr::Literal(Literal::Bool(true)));
        let assign = b.assign(x_tgt, v);
        let db = kdb();
        let tr = transfer_no_bases(&db);
        let out = tr.transfer_stmt(assign.into_raw(), &NarrowState::new(), &b.body);
        assert_eq!(overlay_type_id(&db, &out, "Х"), Some(db.boolean()));
    }

    #[test]
    fn transfer_stmt_assign_date_literal_records_date() {
        let mut b = ExprBuilder::new();
        let x_tgt = b.path("Х");
        let v = b.alloc(Expr::Literal(Literal::Date("20260101".into())));
        let assign = b.assign(x_tgt, v);
        let db = kdb();
        let tr = transfer_no_bases(&db);
        let out = tr.transfer_stmt(assign.into_raw(), &NarrowState::new(), &b.body);
        assert_eq!(overlay_type_id(&db, &out, "Х"), Some(db.date(DateComponent::DateTime)));
    }

    #[test]
    fn transfer_stmt_assign_null_literal_records_null() {
        let mut b = ExprBuilder::new();
        let x_tgt = b.path("Х");
        let v = b.alloc(Expr::Literal(Literal::Null));
        let assign = b.assign(x_tgt, v);
        let db = kdb();
        let tr = transfer_no_bases(&db);
        let out = tr.transfer_stmt(assign.into_raw(), &NarrowState::new(), &b.body);
        assert_eq!(overlay_type_id(&db, &out, "Х"), Some(db.null()));
    }

    #[test]
    fn transfer_stmt_assign_from_base_typed_rhs_records_base_type() {
        let mut b = ExprBuilder::new();
        let x_tgt = b.path("Х");
        let y_val = b.path("Y");
        let assign = b.assign(x_tgt, y_val);

        let db = kdb();
        let tr = transfer_with_bases(&db, &[("Y", db.number(None, None))]);
        let state_out = tr.transfer_stmt(assign.into_raw(), &NarrowState::new(), &b.body);
        assert_eq!(overlay_type_id(&db, &state_out, "Х"), Some(db.number(None, None)));
    }

    #[test]
    fn transfer_stmt_assign_from_narrowed_rhs_prefers_overlay_over_base() {
        let mut b = ExprBuilder::new();
        let x_tgt = b.path("Х");
        let y_val = b.path("Y");
        let assign = b.assign(x_tgt, y_val);

        let db = kdb();
        let tr = transfer_with_bases(
            &db,
            &[("Y", db.union(vec![db.number(None, None), db.string(None, false)]))],
        );
        let state_in = state_with(&db, &[("Y", db.string(None, false))]);
        let state_out = tr.transfer_stmt(assign.into_raw(), &state_in, &b.body);
        assert_eq!(overlay_type_id(&db, &state_out, "Х"), Some(db.string(None, false)));
    }

    #[test]
    fn transfer_stmt_assign_from_complex_rhs_drops_entry() {
        let mut b = ExprBuilder::new();
        let x_tgt = b.path("Х");
        let y = b.path("Y");
        let one = b.alloc(Expr::Literal(Literal::Number(1.0.try_into().unwrap())));
        let sum = b.bin(y, one, BinaryOp::Add);
        let assign = b.assign(x_tgt, sum);

        let db = kdb();
        let tr = transfer_no_bases(&db);
        let state_in = state_with(&db, &[("Х", db.string(None, false))]);
        let state_out = tr.transfer_stmt(assign.into_raw(), &state_in, &b.body);
        assert_eq!(state_out.get(&Name::new("Х")), None);
    }

    #[test]
    fn transfer_stmt_assign_literal_does_not_touch_unrelated_entries() {
        let mut b = ExprBuilder::new();
        let x_tgt = b.path("Х");
        let num = b.alloc(Expr::Literal(Literal::Number(7.0.try_into().unwrap())));
        let assign = b.assign(x_tgt, num);

        let db = kdb();
        let tr = transfer_no_bases(&db);
        let state_in = state_with(&db, &[("Х", db.string(None, false)), ("Y", db.boolean())]);
        let state_out = tr.transfer_stmt(assign.into_raw(), &state_in, &b.body);
        assert_eq!(overlay_type_id(&db, &state_out, "Y"), Some(db.boolean()));
        assert_eq!(overlay_type_id(&db, &state_out, "Х"), Some(db.number(None, None)));
    }

    #[test]
    fn e2e_if_type_check_narrows_then_block() {
        let mut b = ExprBuilder::new();

        let x_arg = b.path("Х");
        let typznc = b.path("ТипЗнч");
        let lhs = b.call(typznc, vec![x_arg]);
        let tip = b.path("Тип");
        let s = b.string_lit("Строка");
        let rhs = b.call(tip, vec![s]);
        let condition = b.bin(lhs, rhs, BinaryOp::Eq);

        let x_tgt = b.path("Х");
        let x_val = b.path("Х");
        let assign = b.assign(x_tgt, x_val);

        let if_stmt = b.if_then(condition, assign);
        b.set_top_level(vec![if_stmt]);

        let db = kdb();
        let body = b.body.clone();
        let result =
            narrow_body(&db, body, FxHashMap::default()).expect("narrowing analysis must converge");
        let cfg = result.cfg();

        use cfg::CfgVertex;
        let cond_idx = cfg
            .vertices()
            .find(|(_, v)| matches!(v, CfgVertex::Conditional(_)))
            .map(|(idx, _)| idx)
            .expect("CFG must contain a Conditional vertex for the Если");

        let then_block_idx = cfg
            .outgoing_edges(cond_idx)
            .find(|(_, kind)| **kind == CfgEdgeType::TrueBranch)
            .map(|(idx, _)| idx)
            .expect("Conditional vertex must have a TrueBranch successor");

        let then_in = result
            .block_in(then_block_idx)
            .expect("then-block must have an IN state after solving");
        assert_eq!(
            overlay_type_id(&db, then_in, "Х"),
            Some(db.string(None, false)),
            "IN[then-block] must carry Х → String after TrueBranch narrowing, got {then_in:?}"
        );
    }

    #[test]
    fn e2e_if_type_check_else_branch_narrows_union_complement() {
        let mut b = ExprBuilder::new();

        let x_arg = b.path("Х");
        let typznc = b.path("ТипЗнч");
        let lhs = b.call(typznc, vec![x_arg]);
        let tip = b.path("Тип");
        let s = b.string_lit("Строка");
        let rhs = b.call(tip, vec![s]);
        let condition = b.bin(lhs, rhs, BinaryOp::Eq);

        let x_tgt_then = b.path("Х");
        let x_val_then = b.path("Х");
        let assign_then = b.assign(x_tgt_then, x_val_then);

        let x_tgt_else = b.path("Х");
        let x_val_else = b.path("Х");
        let assign_else = b.assign(x_tgt_else, x_val_else);

        let if_stmt = b.if_then_else(condition, assign_then, assign_else);
        b.set_top_level(vec![if_stmt]);

        let db = kdb();
        let body = b.body.clone();
        let mut bases = FxHashMap::default();
        bases.insert(
            fold_name(&Name::new("Х")),
            db.union(vec![db.number(None, None), db.string(None, false)]),
        );
        let result = narrow_body(&db, body, bases).expect("narrowing analysis must converge");
        let cfg = result.cfg();

        use cfg::CfgVertex;
        let cond_idx = cfg
            .vertices()
            .find(|(_, v)| matches!(v, CfgVertex::Conditional(_)))
            .map(|(idx, _)| idx)
            .expect("CFG must contain a Conditional vertex for the Если");

        let else_block_idx = cfg
            .outgoing_edges(cond_idx)
            .find(|(_, kind)| **kind == CfgEdgeType::FalseBranch)
            .map(|(idx, _)| idx)
            .expect("Conditional vertex must have a FalseBranch successor");

        let else_in = result
            .block_in(else_block_idx)
            .expect("else-block must have an IN state after solving");
        assert_eq!(
            overlay_type_id(&db, else_in, "Х"),
            Some(db.number(None, None)),
            "IN[else-block] must carry Х → Number (= Union(Number, String) \\ String), got {else_in:?}"
        );
    }

    #[test]
    fn e2e_reassignment_in_then_block_records_new_type_in_out_state() {
        let mut b = ExprBuilder::new();

        let x_arg = b.path("Х");
        let typznc = b.path("ТипЗнч");
        let lhs = b.call(typznc, vec![x_arg]);
        let tip = b.path("Тип");
        let s = b.string_lit("Строка");
        let rhs = b.call(tip, vec![s]);
        let condition = b.bin(lhs, rhs, BinaryOp::Eq);

        let x_tgt = b.path("Х");
        let num = b.alloc(Expr::Literal(Literal::Number(42.0.try_into().unwrap())));
        let assign = b.assign(x_tgt, num);

        let if_stmt = b.if_then(condition, assign);
        b.set_top_level(vec![if_stmt]);

        let db = kdb();
        let body = b.body.clone();
        let result =
            narrow_body(&db, body, FxHashMap::default()).expect("narrowing analysis must converge");
        let cfg = result.cfg();

        use cfg::CfgVertex;
        let cond_idx = cfg
            .vertices()
            .find(|(_, v)| matches!(v, CfgVertex::Conditional(_)))
            .map(|(idx, _)| idx)
            .expect("CFG must contain a Conditional vertex for the Если");

        let then_block_idx = cfg
            .outgoing_edges(cond_idx)
            .find(|(_, kind)| **kind == CfgEdgeType::TrueBranch)
            .map(|(idx, _)| idx)
            .expect("Conditional vertex must have a TrueBranch successor");

        let then_in = result
            .block_in(then_block_idx)
            .expect("then-block must have an IN state after solving");
        assert_eq!(
            overlay_type_id(&db, then_in, "Х"),
            Some(db.string(None, false)),
            "IN[then-block] carries guard narrowing, got {then_in:?}"
        );

        let then_out = result
            .block_out(then_block_idx)
            .expect("then-block must have an OUT state after solving");
        assert_eq!(
            overlay_type_id(&db, then_out, "Х"),
            Some(db.number(None, None)),
            "OUT[then-block] must reflect the Х = 42 reassignment, got {then_out:?}"
        );
    }

    #[test]
    fn e2e_one_sided_reassignment_does_not_leak_past_merge() {
        let mut b = ExprBuilder::new();

        let x_arg = b.path("Х");
        let typznc = b.path("ТипЗнч");
        let lhs = b.call(typznc, vec![x_arg]);
        let tip = b.path("Тип");
        let s = b.string_lit("Строка");
        let rhs = b.call(tip, vec![s]);
        let condition = b.bin(lhs, rhs, BinaryOp::Eq);

        let x_tgt = b.path("Х");
        let num = b.alloc(Expr::Literal(Literal::Number(42.0.try_into().unwrap())));
        let assign = b.assign(x_tgt, num);

        let if_stmt = b.if_then(condition, assign);
        b.set_top_level(vec![if_stmt]);

        let db = kdb();
        let body = b.body.clone();
        let result =
            narrow_body(&db, body, FxHashMap::default()).expect("narrowing analysis must converge");
        let cfg = result.cfg();

        use cfg::CfgVertex;
        let exit_idx = cfg
            .vertices()
            .find(|(_, v)| matches!(v, CfgVertex::Exit))
            .map(|(idx, _)| idx)
            .expect("CFG must contain an Exit vertex");

        let exit_in =
            result.block_in(exit_idx).expect("Exit vertex must have an IN state after solving");
        assert_eq!(
            exit_in.get(&Name::new("Х")),
            None,
            "one-sided `Х → Number` must NOT survive the post-КонецЕсли merge, got {exit_in:?}"
        );
    }

    struct NarrowProbe {
        db: InMemoryDb,
        result: dataflow::DataflowResult<NarrowState>,
        body: Body,
        then_body_path: ExprIdx,
        else_body_path: Option<ExprIdx>,
    }

    fn build_probe_if_then_else(
        bases: impl FnOnce(&dyn TypeKernelDb) -> FxHashMap<Name, TypeId>,
    ) -> NarrowProbe {
        let db = kdb();
        let bases = bases(&db);
        let mut b = ExprBuilder::new();

        let receiver = b.path("Х");
        let typznc = b.path("ТипЗнч");
        let lhs = b.call(typznc, vec![receiver]);
        let tip = b.path("Тип");
        let s = b.string_lit("Строка");
        let rhs = b.call(tip, vec![s]);
        let condition = b.bin(lhs, rhs, BinaryOp::Eq);

        let then_lhs = b.path("Х");
        let then_body_path = b.path("Х");
        let then_assign = b.assign(then_lhs, then_body_path);

        let else_lhs = b.path("Х");
        let else_body_path = b.path("Х");
        let else_assign = b.assign(else_lhs, else_body_path);

        let if_stmt = b.if_then_else(condition, then_assign, else_assign);
        b.set_top_level(vec![if_stmt]);

        let body = b.body.clone();
        let result =
            narrow_body(&db, body.clone(), bases).expect("narrowing analysis must converge");
        NarrowProbe { db, result, body, then_body_path, else_body_path: Some(else_body_path) }
    }

    fn build_probe_if_then_only() -> NarrowProbe {
        let db = kdb();
        let mut b = ExprBuilder::new();

        let receiver = b.path("Х");
        let typznc = b.path("ТипЗнч");
        let lhs = b.call(typznc, vec![receiver]);
        let tip = b.path("Тип");
        let s = b.string_lit("Строка");
        let rhs = b.call(tip, vec![s]);
        let condition = b.bin(lhs, rhs, BinaryOp::Eq);

        let then_lhs = b.path("Х");
        let then_body_path = b.path("Х");
        let then_assign = b.assign(then_lhs, then_body_path);

        let if_stmt = b.if_then(condition, then_assign);
        b.set_top_level(vec![if_stmt]);

        let body = b.body.clone();
        let result = narrow_body(&db, body.clone(), FxHashMap::default())
            .expect("narrowing analysis must converge");
        NarrowProbe { db, result, body, then_body_path, else_body_path: None }
    }

    #[test]
    fn narrowed_type_at_guard_receiver_returns_pre_narrow() {
        let mut b = ExprBuilder::new();

        let pre_target = b.path("Х");
        let num42 = b.alloc(Expr::Literal(Literal::Number(42.0.try_into().unwrap())));
        let pre_assign = b.assign(pre_target, num42);

        let receiver = b.path("Х");
        let typznc = b.path("ТипЗнч");
        let lhs = b.call(typznc, vec![receiver]);
        let tip = b.path("Тип");
        let s = b.string_lit("Строка");
        let rhs = b.call(tip, vec![s]);
        let condition = b.bin(lhs, rhs, BinaryOp::Eq);

        let then_lhs = b.path("Х");
        let then_rhs = b.path("Х");
        let then_assign = b.assign(then_lhs, then_rhs);
        let if_stmt = b.if_then(condition, then_assign);

        b.set_top_level(vec![pre_assign, if_stmt]);

        let db = kdb();
        let body = b.body.clone();
        let result =
            narrow_body(&db, body, FxHashMap::default()).expect("narrowing analysis must converge");

        assert_eq!(
            narrowed_type_at(&db, &result, &b.body,receiver, &Name::new("Х")),
            Some(db.number(None, None)),
            "receiver must see pre-narrow overlay (Х → Number from prior assign), NOT the post-narrow String from the guard"
        );

        assert_eq!(
            narrowed_type_at(&db, &result, &b.body, then_rhs, &Name::new("Х")),
            Some(db.string(None, false)),
            "then-body Х must see the guard's narrowing to Строка"
        );
    }

    #[test]
    fn narrowed_type_at_then_body_sees_narrowed() {
        let probe = build_probe_if_then_else(|_| FxHashMap::default());
        let expected = probe.db.string(None, false);
        assert_eq!(
            narrowed_type_at(
                &probe.db,
                &probe.result,
                &probe.body,
                probe.then_body_path,
                &Name::new("Х")
            ),
            Some(expected),
            "then-body Х must carry the TrueBranch narrowing Х → Строка"
        );
    }

    #[test]
    fn narrowed_type_at_else_body_sees_complement() {
        let probe = build_probe_if_then_else(|db| {
            let mut bases = FxHashMap::default();
            bases.insert(
                fold_name(&Name::new("Х")),
                db.union(vec![db.number(None, None), db.string(None, false)]),
            );
            bases
        });
        let else_expr = probe.else_body_path.expect("else branch is present");
        assert_eq!(
            narrowed_type_at(&probe.db, &probe.result, &probe.body,else_expr, &Name::new("Х")),
            Some(probe.db.number(None, None)),
            "else-body Х must carry the FalseBranch complement Union(Number,String) \\ String = Number"
        );
    }

    #[test]
    fn narrowed_type_at_untouched_var_returns_none() {
        let probe = build_probe_if_then_else(|_| FxHashMap::default());
        assert_eq!(
            narrowed_type_at(
                &probe.db,
                &probe.result,
                &probe.body,
                probe.then_body_path,
                &Name::new("Y")
            ),
            None,
            "unrelated variable Y must not pick up any narrowing"
        );
    }

    #[test]
    fn narrowed_type_at_after_konec_esli_drops_one_sided_narrowing() {
        use cfg::CfgVertex;

        let probe = build_probe_if_then_only();
        let cfg = probe.result.cfg();
        let exit_idx = cfg
            .vertices()
            .find(|(_, v)| matches!(v, CfgVertex::Exit))
            .map(|(idx, _)| idx)
            .expect("CFG must contain an Exit vertex");
        let exit_in = probe.result.block_in(exit_idx).expect("Exit IN must be populated");
        assert_eq!(
            exit_in.get(&Name::new("Х")),
            None,
            "one-sided narrowing Х → Строка must drop at post-КонецЕсли merge (intersection join)"
        );
    }

    #[test]
    fn narrowed_type_at_missing_expr_returns_none() {
        let probe = build_probe_if_then_else(|_| FxHashMap::default());
        let stray_expr = Idx::<Expr>::from_raw(RawIdx::from(u32::MAX - 1));
        assert_eq!(
            narrowed_type_at(&probe.db, &probe.result, &probe.body, stray_expr, &Name::new("Х")),
            None,
            "expression not reachable from any CFG vertex must return None"
        );
    }

    #[test]
    fn narrowed_type_at_elsif_condition_receiver_sees_pre_narrow() {
        let mut b = ExprBuilder::new();

        let x1 = b.path("Х");
        let typznc1 = b.path("ТипЗнч");
        let lhs1 = b.call(typznc1, vec![x1]);
        let tip1 = b.path("Тип");
        let s1 = b.string_lit("Строка");
        let rhs1 = b.call(tip1, vec![s1]);
        let cond1 = b.bin(lhs1, rhs1, BinaryOp::Eq);

        let then_lhs = b.path("Х");
        let then_rhs = b.path("Х");
        let then_assign = b.assign(then_lhs, then_rhs);

        let elsif_receiver = b.path("Х");
        let typznc2 = b.path("ТипЗнч");
        let lhs2 = b.call(typznc2, vec![elsif_receiver]);
        let tip2 = b.path("Тип");
        let s2 = b.string_lit("Дата");
        let rhs2 = b.call(tip2, vec![s2]);
        let cond2 = b.bin(lhs2, rhs2, BinaryOp::Eq);

        let elsif_lhs = b.path("Х");
        let elsif_rhs = b.path("Х");
        let elsif_assign = b.assign(elsif_lhs, elsif_rhs);

        let if_stmt = b.if_then_elsif(cond1, then_assign, cond2, elsif_assign);
        b.set_top_level(vec![if_stmt]);

        let db = kdb();
        let body = b.body.clone();
        let mut bases = FxHashMap::default();
        bases.insert(
            fold_name(&Name::new("Х")),
            db.union(vec![db.number(None, None), db.string(None, false)]),
        );
        let result = narrow_body(&db, body, bases).expect("narrowing analysis must converge");

        assert_eq!(
            narrowed_type_at(&db, &result, &b.body,elsif_receiver, &Name::new("Х")),
            Some(db.number(None, None)),
            "elsif-condition receiver must see the FalseBranch-complement from the first Conditional (Number), not its own elsif narrowing target (Дата)"
        );

        assert_eq!(
            narrowed_type_at(&db, &result, &b.body, elsif_rhs, &Name::new("Х")),
            Some(db.date(DateComponent::DateTime)),
            "elsif then-body Х must see the TrueBranch narrowing to Дата"
        );
    }

    #[test]
    fn narrowed_type_at_while_condition_receiver_sees_pre_narrow() {
        let mut b = ExprBuilder::new();

        let pre_target = b.path("Х");
        let num42 = b.alloc(Expr::Literal(Literal::Number(42.0.try_into().unwrap())));
        let pre_assign = b.assign(pre_target, num42);

        let receiver = b.path("Х");
        let typznc = b.path("ТипЗнч");
        let lhs = b.call(typznc, vec![receiver]);
        let tip = b.path("Тип");
        let s = b.string_lit("Строка");
        let rhs = b.call(tip, vec![s]);
        let condition = b.bin(lhs, rhs, BinaryOp::Eq);

        let body_lhs = b.path("Х");
        let body_rhs = b.path("Х");
        let body_assign = b.assign(body_lhs, body_rhs);

        let while_stmt = b.while_stmt(condition, body_assign);
        b.set_top_level(vec![pre_assign, while_stmt]);

        let db = kdb();
        let body = b.body.clone();
        let result =
            narrow_body(&db, body, FxHashMap::default()).expect("narrowing analysis must converge");

        assert_eq!(
            narrowed_type_at(&db, &result, &b.body,receiver, &Name::new("Х")),
            Some(db.union(vec![db.number(None, None), db.string(None, false)])),
            "while-condition receiver must see the merged pre-narrow overlay Union(Number, String), not either side in isolation"
        );

        assert_eq!(
            narrowed_type_at(&db, &result, &b.body, body_rhs, &Name::new("Х")),
            Some(db.string(None, false)),
            "while-body Х must see the TrueBranch narrowing to Строка"
        );
    }
}

#[cfg(test)]
mod branch_guard_tests {
    use super::*;
    use bsl_types::testing::InMemoryDb;
    use hir_def::hir::IfStmt;

    /// `Если Х = Неопределено Тогда <then> ИначеЕсли Х = Неопределено Тогда <elsif>
    /// Иначе <else> КонецЕсли`, with a `Продолжить` standing in for each branch body.
    struct IfFixture {
        body: Body,
        if_stmt: StmtIdx,
        then_stmt: StmtIdx,
        elsif_stmt: StmtIdx,
        else_stmt: StmtIdx,
    }

    fn undefined_compare(body: &mut Body, var: &str) -> ExprIdx {
        let lhs = body.alloc_expr(Expr::Path(Name::new(var))).to_idx();
        let rhs = body.alloc_expr(Expr::Literal(Literal::Undefined)).to_idx();
        body.alloc_expr(Expr::BinaryOp { lhs, rhs, op: BinaryOp::Eq }).to_idx()
    }

    fn if_fixture() -> IfFixture {
        let mut body = Body::default();
        let condition = undefined_compare(&mut body, "Х");
        let elsif_condition = undefined_compare(&mut body, "Х");
        let then_stmt = body.alloc_stmt(Stmt::Continue).to_idx();
        let elsif_stmt = body.alloc_stmt(Stmt::Continue).to_idx();
        let else_stmt = body.alloc_stmt(Stmt::Continue).to_idx();
        let if_stmt = body
            .alloc_stmt(Stmt::If(Box::new(IfStmt {
                condition,
                then_branch: Box::new([then_stmt]),
                elsif_branches: Box::new([(elsif_condition, Box::new([elsif_stmt]) as Box<[_]>)]),
                else_branch: Some(Box::new([else_stmt])),
            })))
            .to_idx();
        body.set_body_stmts(Box::new([if_stmt]));
        IfFixture { body, if_stmt, then_stmt, elsif_stmt, else_stmt }
    }

    fn guards_at(fixture: &IfFixture, stmt: StmtIdx) -> Vec<BranchGuard> {
        branch_guards_for_stmt(&fixture.body, hir_def::StmtId::from_idx(stmt))
    }

    #[test]
    fn then_branch_takes_the_condition_as_proven() {
        let fixture = if_fixture();
        let guards = guards_at(&fixture, fixture.then_stmt);
        assert_eq!(guards.len(), 1);
        assert!(guards[0].on_true);
        assert_eq!(guards[0].if_stmt, fixture.if_stmt);
        assert!(matches!(guards[0].guard, Guard::IsUndefined { .. }));
    }

    #[test]
    fn else_branch_takes_every_earlier_condition_as_refuted() {
        let fixture = if_fixture();
        let guards = guards_at(&fixture, fixture.else_stmt);
        assert_eq!(guards.len(), 2);
        assert!(guards.iter().all(|g| !g.on_true));
    }

    #[test]
    fn elsif_branch_refutes_the_earlier_condition_and_proves_its_own() {
        let fixture = if_fixture();
        let guards = guards_at(&fixture, fixture.elsif_stmt);
        assert_eq!(guards.iter().map(|g| g.on_true).collect::<Vec<_>>(), vec![false, true]);
    }

    #[test]
    fn a_statement_outside_the_if_is_guarded_by_nothing() {
        let mut fixture = if_fixture();
        let after = fixture.body.alloc_stmt(Stmt::Continue).to_idx();
        fixture.body.set_body_stmts(Box::new([fixture.if_stmt, after]));
        assert!(guards_at(&fixture, after).is_empty());
    }

    /// The `Если` of the reported ERP shape sits inside `Для Каждого … Цикл`.
    #[test]
    fn guards_are_found_through_an_enclosing_loop() {
        let mut fixture = if_fixture();
        let collection = fixture.body.alloc_expr(Expr::Path(Name::new("Массив"))).to_idx();
        let var = fixture.body.bindings_mut().alloc(hir_def::hir::Binding::var(Name::new("Э")));
        let loop_stmt = fixture
            .body
            .alloc_stmt(Stmt::ForEach { var, collection, body: Box::new([fixture.if_stmt]) })
            .to_idx();
        fixture.body.set_body_stmts(Box::new([loop_stmt]));
        assert_eq!(guards_at(&fixture, fixture.else_stmt).len(), 2);
    }

    #[test]
    fn stmt_covers_stmt_sees_a_branch_statement_but_not_a_sibling() {
        let mut fixture = if_fixture();
        let after = fixture.body.alloc_stmt(Stmt::Continue).to_idx();
        fixture.body.set_body_stmts(Box::new([fixture.if_stmt, after]));
        assert!(stmt_covers_stmt(&fixture.body, fixture.if_stmt, fixture.else_stmt));
        assert!(!stmt_covers_stmt(&fixture.body, fixture.if_stmt, after));
    }

    fn is_undefined_guard() -> Guard {
        Guard::IsUndefined { var: Name::new("Х") }
    }

    #[test]
    fn refuted_undefined_check_rules_out_a_confident_undefined() {
        let db = InMemoryDb::new();
        let undefined = db.undefined();
        assert_eq!(
            apply_branch_guard(&db, &is_undefined_guard(), false, undefined),
            GuardVerdict::Contradicted
        );
        assert_eq!(
            apply_branch_guard(
                &db,
                &Guard::IsNotUndefined { var: Name::new("Х") },
                true,
                undefined
            ),
            GuardVerdict::Contradicted
        );
    }

    #[test]
    fn refuted_undefined_check_leaves_a_type_that_never_was_undefined() {
        let db = InMemoryDb::new();
        assert_eq!(
            apply_branch_guard(&db, &is_undefined_guard(), false, db.boolean()),
            GuardVerdict::NoInformation
        );
    }

    #[test]
    fn refuted_undefined_check_drops_only_the_undefined_arm_of_a_union() {
        let db = InMemoryDb::new();
        let union = db.union(vec![db.undefined(), db.boolean()]);
        assert_eq!(
            apply_branch_guard(&db, &is_undefined_guard(), false, union),
            GuardVerdict::Narrowed(db.boolean())
        );
    }

    #[test]
    fn an_untyped_receiver_is_never_contradicted() {
        let db = InMemoryDb::new();
        for ty in [db.unknown(), db.any(), db.never()] {
            assert_eq!(
                apply_branch_guard(&db, &is_undefined_guard(), false, ty),
                GuardVerdict::NoInformation
            );
        }
    }

    #[test]
    fn value_filled_rules_out_undefined_only_where_it_holds() {
        let db = InMemoryDb::new();
        let guard = Guard::ValueFilled { var: Name::new("Х") };
        assert_eq!(
            apply_branch_guard(&db, &guard, true, db.undefined()),
            GuardVerdict::Contradicted
        );
        assert_eq!(
            apply_branch_guard(&db, &guard, false, db.undefined()),
            GuardVerdict::NoInformation
        );
    }

    #[test]
    fn a_type_check_rules_out_nothing() {
        let db = InMemoryDb::new();
        let guard = Guard::TypeCheck { var: Name::new("Х"), type_name: "Массив".to_owned() };
        for on_true in [true, false] {
            assert_eq!(
                apply_branch_guard(&db, &guard, on_true, db.undefined()),
                GuardVerdict::NoInformation
            );
        }
    }
}
