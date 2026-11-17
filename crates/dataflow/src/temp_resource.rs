use cfg::{CfgEdgeType, ControlFlowGraph};
use hir_def::body::Body;
use hir_def::hir::{Expr, ExprIdx, Stmt};
use hir_def::{ExprId, IdConversion, StmtId};
use la_arena::RawIdx;
use rustc_hash::{FxHashMap, FxHashSet};
use std::hash::Hash;
use std::marker::PhantomData;
use std::sync::Arc;

use crate::{DataflowResult, DataflowSolver, Direction, Lattice, Transfer, DEFAULT_MAX_ITERATIONS};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResourceEvent<R>
where
    R: Clone + Eq + Hash,
{
    Open(R),
    Close(R),
}

pub trait ResourceProvider<R>
where
    R: Clone + Eq + Hash,
{
    fn classify(&self, body: &Body, expr_idx: ExprIdx) -> Option<ResourceEvent<R>>;

    fn classify_many(&self, body: &Body, expr_idx: ExprIdx) -> Vec<ResourceEvent<R>> {
        self.classify(body, expr_idx).into_iter().collect()
    }

    /// Resources released already at the entry of `Попытка`: the release then
    /// holds on the exception edge into the handler as well as on the
    /// protected path.
    fn closed_at_try_entry(&self, _body: &Body, _try_stmt: StmtId) -> Vec<R> {
        Vec::new()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenSet<R>
where
    R: Clone + Eq + Hash,
{
    open: FxHashMap<R, FxHashSet<ExprIdx>>,
}

impl<R> Default for OpenSet<R>
where
    R: Clone + Eq + Hash,
{
    fn default() -> Self {
        Self { open: FxHashMap::default() }
    }
}

impl<R> OpenSet<R>
where
    R: Clone + Eq + Hash,
{
    pub fn bottom() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.open.is_empty()
    }

    pub fn as_map(&self) -> &FxHashMap<R, FxHashSet<ExprIdx>> {
        &self.open
    }

    fn open_at(&mut self, r: R, site: ExprIdx) {
        self.open.entry(r).or_default().insert(site);
    }

    fn close(&mut self, r: &R) {
        self.open.remove(r);
    }
}

impl<R> Lattice for OpenSet<R>
where
    R: Clone + Eq + Hash,
{
    fn join(&self, other: &Self) -> Self {
        let mut result = self.clone();
        result.join_in_place(other);
        result
    }

    fn join_in_place(&mut self, other: &Self) {
        for (key, sites) in &other.open {
            let entry = self.open.entry(key.clone()).or_default();
            for site in sites {
                entry.insert(*site);
            }
        }
    }
}

struct ResourceTransfer<P, R>
where
    R: Clone + Eq + Hash,
    P: ResourceProvider<R>,
{
    provider: P,
    _r: PhantomData<R>,
}

impl<P, R> ResourceTransfer<P, R>
where
    R: Clone + Eq + Hash,
    P: ResourceProvider<R>,
{
    fn apply(&self, body: &Body, expr_idx: ExprIdx, state: &mut OpenSet<R>) {
        for event in self.provider.classify_many(body, expr_idx) {
            match event {
                ResourceEvent::Open(r) => state.open_at(r, expr_idx),
                ResourceEvent::Close(r) => state.close(&r),
            }
        }
    }
}

impl<P, R> Transfer<OpenSet<R>> for ResourceTransfer<P, R>
where
    R: Clone + Eq + Hash,
    P: ResourceProvider<R>,
{
    fn transfer_stmt(&self, stmt_id: RawIdx, state: &OpenSet<R>, body: &Body) -> OpenSet<R> {
        let mut next = state.clone();
        let stmt = StmtId::from_raw(stmt_id);
        walk_stmt_subexprs(body, stmt, &mut |expr_idx| {
            self.apply(body, expr_idx, &mut next);
        });
        next
    }

    fn transfer_expr(&self, expr_id: ExprId, state: &OpenSet<R>, body: &Body) -> OpenSet<R> {
        let mut next = state.clone();
        walk_expr(body, expr_id.to_idx(), &mut |expr_idx| {
            self.apply(body, expr_idx, &mut next);
        });
        next
    }

    fn transfer_try_entry(&self, try_stmt: StmtId, state: &mut OpenSet<R>, body: &Body) {
        for r in self.provider.closed_at_try_entry(body, try_stmt) {
            state.close(&r);
        }
    }

    fn transfer_edge(&self, edge_kind: CfgEdgeType, state: &OpenSet<R>) -> OpenSet<R> {
        if !edge_kind.is_executable() {
            OpenSet::bottom()
        } else {
            state.clone()
        }
    }
}

fn walk_stmt_subexprs<F: FnMut(ExprIdx)>(body: &Body, stmt_id: StmtId, f: &mut F) {
    match body.stmt(stmt_id) {
        Stmt::Expr(e) => walk_expr(body, *e, f),
        Stmt::Assign { target, value } => {
            walk_expr(body, *target, f);
            walk_expr(body, *value, f);
        }
        Stmt::Return { value: Some(e) } => walk_expr(body, *e, f),
        Stmt::Raise { value: Some(e) } => walk_expr(body, *e, f),
        Stmt::Execute { expr } => walk_expr(body, *expr, f),
        Stmt::AddHandler { event, handler } | Stmt::RemoveHandler { event, handler } => {
            walk_expr(body, *event, f);
            walk_expr(body, *handler, f);
        }
        Stmt::VarDecl { .. }
        | Stmt::If(_)
        | Stmt::PreprocIf(_)
        | Stmt::While { .. }
        | Stmt::For { .. }
        | Stmt::ForEach { .. }
        | Stmt::Try { .. }
        | Stmt::Return { value: None }
        | Stmt::Raise { value: None }
        | Stmt::Break
        | Stmt::Continue
        | Stmt::Goto(_)
        | Stmt::Label(_) => {}
    }
}

fn walk_expr<F: FnMut(ExprIdx)>(body: &Body, expr_idx: ExprIdx, f: &mut F) {
    f(expr_idx);
    match body.expr_idx(expr_idx) {
        Expr::Call { callee, args } => {
            walk_expr(body, *callee, f);
            for &arg in args.iter() {
                walk_expr(body, arg, f);
            }
        }
        Expr::MethodCall { receiver, args, .. } => {
            walk_expr(body, *receiver, f);
            for &arg in args.iter() {
                walk_expr(body, arg, f);
            }
        }
        Expr::Field { base, .. } => walk_expr(body, *base, f),
        Expr::Index { base, index } => {
            walk_expr(body, *base, f);
            walk_expr(body, *index, f);
        }
        Expr::BinaryOp { lhs, rhs, .. } => {
            walk_expr(body, *lhs, f);
            walk_expr(body, *rhs, f);
        }
        Expr::UnaryOp { expr, .. } => walk_expr(body, *expr, f),
        Expr::Ternary { condition, then_expr, else_expr } => {
            walk_expr(body, *condition, f);
            walk_expr(body, *then_expr, f);
            walk_expr(body, *else_expr, f);
        }
        Expr::New { args, .. } => {
            for &arg in args.iter() {
                walk_expr(body, arg, f);
            }
        }
        Expr::Array(items) => {
            for &item in items.iter() {
                walk_expr(body, item, f);
            }
        }
        Expr::Await { expr } => walk_expr(body, *expr, f),
        Expr::Path(_) | Expr::Literal(_) | Expr::Missing => {}
    }
}

pub struct OpenResourcesResult<R>
where
    R: Clone + Eq + Hash,
{
    inner: DataflowResult<OpenSet<R>>,
    exit_block: petgraph::graph::NodeIndex,
    empty_map: FxHashMap<R, FxHashSet<ExprIdx>>,
}

impl<R> OpenResourcesResult<R>
where
    R: Clone + Eq + Hash,
{
    pub fn open_at_exit(&self) -> &FxHashMap<R, FxHashSet<ExprIdx>> {
        self.inner.block_out(self.exit_block).map(|set| set.as_map()).unwrap_or(&self.empty_map)
    }
}

pub fn analyze_open_resources<P, R>(
    body: &Body,
    cfg: &ControlFlowGraph,
    provider: P,
) -> Option<OpenResourcesResult<R>>
where
    R: Clone + Eq + Hash,
    P: ResourceProvider<R>,
{
    let exit_block = cfg.exit();
    let transfer = ResourceTransfer { provider, _r: PhantomData };
    let mut solver = DataflowSolver::new(Arc::new(cfg.clone()), body.clone(), transfer);
    solver.set_direction(Direction::Forward);
    solver.set_bottom_factory(OpenSet::<R>::bottom);
    solver.set_max_iterations(DEFAULT_MAX_ITERATIONS);
    let inner = solver.solve()?;
    Some(OpenResourcesResult { inner, exit_block, empty_map: FxHashMap::default() })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw_expr(n: u32) -> ExprIdx {
        ExprIdx::from_raw(RawIdx::from_u32(n))
    }

    #[test]
    fn open_set_join_unions_inner_site_sets() {
        let s1 = raw_expr(1);
        let s2 = raw_expr(2);
        let s3 = raw_expr(3);

        let mut a: OpenSet<&str> = OpenSet::bottom();
        a.open_at("X", s1);

        let mut b: OpenSet<&str> = OpenSet::bottom();
        b.open_at("X", s2);
        b.open_at("Y", s3);

        let joined = a.join(&b);
        let map = joined.as_map();
        assert_eq!(map.len(), 2);
        let x = map.get("X").expect("X must survive join");
        assert!(x.contains(&s1));
        assert!(x.contains(&s2));
        let y = map.get("Y").expect("Y must survive join (only in b)");
        assert!(y.contains(&s3));
    }

    #[test]
    fn open_set_close_clears_all_sites() {
        let s1 = raw_expr(1);
        let s2 = raw_expr(2);

        let mut state: OpenSet<&str> = OpenSet::bottom();
        state.open_at("X", s1);
        state.open_at("X", s2);
        assert_eq!(state.as_map().get("X").unwrap().len(), 2);

        state.close(&"X");
        assert!(state.is_empty(), "close must drop the entry, not just shrink it");
    }

    #[test]
    fn open_set_lattice_idempotence_and_commutativity() {
        let s1 = raw_expr(1);
        let s2 = raw_expr(2);

        let mut a: OpenSet<&str> = OpenSet::bottom();
        a.open_at("X", s1);
        let mut b: OpenSet<&str> = OpenSet::bottom();
        b.open_at("X", s2);

        assert_eq!(a.join(&a), a);
        assert_eq!(a.join(&b), b.join(&a));
        assert_eq!(OpenSet::<&str>::bottom().join(&a), a);
    }

    #[test]
    fn open_set_join_in_place_is_consistent_with_join() {
        let s1 = raw_expr(1);
        let s2 = raw_expr(2);
        let s3 = raw_expr(3);

        let mut a: OpenSet<&str> = OpenSet::bottom();
        a.open_at("X", s1);
        a.open_at("Y", s2);

        let mut b: OpenSet<&str> = OpenSet::bottom();
        b.open_at("Y", s3);
        b.open_at("Z", s1);

        let merged = a.join(&b);
        let mut in_place = a.clone();
        in_place.join_in_place(&b);
        assert_eq!(in_place, merged);
    }
}
