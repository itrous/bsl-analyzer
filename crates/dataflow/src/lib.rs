pub mod definite_init;
pub mod effect_summary;
pub mod guard_predicates;
pub mod path_terminates;
pub mod reaching_defs;
pub mod security_state;
pub mod temp_resource;
pub mod value_state;

use cfg::{CfgEdgeType, ControlFlowGraph};
use hir_def::body::Body;
use la_arena::RawIdx;
use petgraph::graph::NodeIndex;
use rustc_hash::{FxHashMap, FxHashSet};
use std::sync::Arc;

pub const DEFAULT_MAX_ITERATIONS: usize = 10000;

/// Heap bytes of a `Vec`/`Box<[T]>` backing store holding `len` elements, counted
/// at element granularity (spare capacity ignored). Shared by the crate's
/// `estimated_heap` Salsa `memory_usage` estimators.
pub(crate) fn vec_bytes<T>(len: usize) -> usize {
    len * std::mem::size_of::<T>()
}

/// Approximate live bytes of an `FxHashMap`/hashbrown table with `len` entries of
/// `(K, V)`: one control byte plus the `(K, V)` slot per bucket, bucket count grown
/// to the next power of two above `len / (7/8)`.
pub(crate) fn map_table_bytes<K, V>(len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    let cap = (len * 8 / 7 + 1).checked_next_power_of_two().unwrap_or(len);
    cap.saturating_mul(std::mem::size_of::<K>() + std::mem::size_of::<V>() + 1)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Forward,
    Backward,
}

pub trait Lattice: Clone + PartialEq + Eq {
    fn join(&self, other: &Self) -> Self;

    fn join_in_place(&mut self, other: &Self) {
        *self = self.join(other);
    }

    fn is_more_informative_than(&self, other: &Self) -> bool {
        self == other
    }
}

pub trait Transfer<L: Lattice> {
    fn transfer_stmt(&self, stmt_id: RawIdx, state: &L, body: &Body) -> L;

    fn transfer_stmt_in_place(&self, stmt_id: RawIdx, state: &mut L, body: &Body) {
        *state = self.transfer_stmt(stmt_id, state, body);
    }

    fn transfer_expr(&self, _expr_id: hir_def::ExprId, state: &L, _body: &Body) -> L {
        state.clone()
    }

    fn transfer_expr_in_place(&self, expr_id: hir_def::ExprId, state: &mut L, body: &Body) {
        *state = self.transfer_expr(expr_id, state, body);
    }

    fn transfer_edge(&self, _edge_kind: CfgEdgeType, state: &L) -> L {
        state.clone()
    }

    fn transfer_loop_var_bind(&self, _loop_var: hir_def::BindingId, _state: &mut L, _body: &Body) {}

    /// Transfer through the `Попытка` vertex. In a forward analysis the state
    /// computed here flows both into the protected statements and along the
    /// exception edge into the handler; a backward analysis receives the join
    /// of both successors instead and flows the result to the predecessor.
    fn transfer_try_entry(&self, _try_stmt: hir_def::StmtId, _state: &mut L, _body: &Body) {}
}

pub struct DataflowSolver<L: Lattice, T: Transfer<L>> {
    cfg: Arc<ControlFlowGraph>,

    body: Body,

    transfer: T,

    block_in: FxHashMap<NodeIndex, L>,

    block_out: FxHashMap<NodeIndex, L>,

    max_iterations: usize,

    direction: Direction,
}

impl<L: Lattice, T: Transfer<L>> DataflowSolver<L, T> {
    pub fn new(cfg: Arc<ControlFlowGraph>, body: Body, transfer: T) -> Self {
        Self {
            cfg,
            body,
            transfer,
            block_in: FxHashMap::default(),
            block_out: FxHashMap::default(),
            max_iterations: DEFAULT_MAX_ITERATIONS,
            direction: Direction::Forward,
        }
    }

    pub fn set_direction(&mut self, direction: Direction) {
        self.direction = direction;
    }

    pub fn set_max_iterations(&mut self, max_iterations: usize) {
        self.max_iterations = max_iterations;
    }

    pub fn set_initial_state(&mut self, initial: L) {
        if let Some(entry) = self.cfg.start() {
            self.block_in.insert(entry, initial);
        }
    }

    pub fn set_initial_state_at_exit(&mut self, initial: L) {
        let exit = self.cfg.exit();
        debug_assert!(
            self.cfg
                .vertices()
                .all(|(idx, _)| self.block_in.contains_key(&idx)
                    && self.block_out.contains_key(&idx)),
            "set_initial_state_at_exit must be called *after* set_bottom_factory \
             (or another initialiser that seeds block_in/block_out for every vertex) — \
             otherwise the factory called later will overwrite the seeded exit OUT state",
        );
        self.block_out.insert(exit, initial);
    }

    pub fn set_bottom_factory<F>(&mut self, factory: F)
    where
        F: Fn() -> L,
    {
        for (block_idx, _vertex) in self.cfg.vertices() {
            self.block_in.insert(block_idx, factory());
            self.block_out.insert(block_idx, factory());
        }
    }

    pub fn solve(self) -> Option<DataflowResult<L>> {
        match self.direction {
            Direction::Forward => self.solve_forward(),
            Direction::Backward => self.solve_backward(),
        }
    }

    fn solve_forward(mut self) -> Option<DataflowResult<L>> {
        use std::collections::VecDeque;

        let entry = self.cfg.start();
        assert!(
            self.cfg
                .vertices()
                .all(|(idx, _)| self.block_in.contains_key(&idx)
                    && self.block_out.contains_key(&idx)),
            "All blocks must be initialized before solve(). Call set_bottom_factory() first."
        );

        let rpo = self.cfg.reverse_postorder();
        let mut worklist: VecDeque<NodeIndex> = rpo.into_iter().collect();

        let mut worklist_set: FxHashSet<NodeIndex> = worklist.iter().copied().collect();

        let mut iterations = 0;
        let mut converged = true;

        while let Some(block_idx) = worklist.pop_front() {
            worklist_set.remove(&block_idx);
            iterations += 1;

            if iterations > self.max_iterations {
                tracing::warn!(
                    "Dataflow analysis exceeded max iterations ({}), returning partial solution",
                    self.max_iterations
                );
                converged = false;
                break;
            }

            let has_predecessors = self.cfg.incoming_edges(block_idx).next().is_some();
            let is_entry = Some(block_idx) == entry;

            let in_state = if is_entry && !has_predecessors {
                self.block_in.get(&block_idx).cloned().expect("block_in should be initialized")
            } else {
                let mut state: Option<L> = None;
                for (pred_idx, edge_kind) in self.cfg.incoming_edges(block_idx) {
                    if let Some(pred_out) = self.block_out.get(&pred_idx) {
                        let edge_state = self.transfer.transfer_edge(*edge_kind, pred_out);
                        match &mut state {
                            None => {
                                state = Some(edge_state);
                            }
                            Some(s) => {
                                s.join_in_place(&edge_state);
                            }
                        }
                    }
                }
                state.unwrap_or_else(|| {
                    self.block_in.get(&block_idx).cloned().expect("block_in should be initialized")
                })
            };

            self.block_in.insert(block_idx, in_state.clone());

            let out_state = self.transfer_block(block_idx, &in_state);

            let changed = if let Some(old_out) = self.block_out.get(&block_idx) {
                &out_state != old_out
            } else {
                true
            };

            if changed {
                self.block_out.insert(block_idx, out_state);

                for (succ_idx, _edge) in self.cfg.outgoing_edges(block_idx) {
                    if worklist_set.insert(succ_idx) {
                        worklist.push_back(succ_idx);
                    }
                }
            }
        }

        tracing::debug!("Forward dataflow analysis converged in {} iterations", iterations);

        Some(DataflowResult {
            block_in: self.block_in,
            block_out: self.block_out,
            cfg: self.cfg,
            converged,
        })
    }

    fn solve_backward(mut self) -> Option<DataflowResult<L>> {
        use std::collections::VecDeque;

        let exit = self.cfg.exit();
        let num_blocks = self.cfg.vertices().count();

        assert!(
            self.cfg
                .vertices()
                .all(|(idx, _)| self.block_in.contains_key(&idx)
                    && self.block_out.contains_key(&idx)),
            "All blocks must be initialized before solve(). Call set_bottom_factory() first."
        );

        let postorder = self.cfg.postorder_from_exit();
        let mut worklist: VecDeque<NodeIndex> = postorder.into_iter().collect();

        let mut worklist_set: FxHashSet<NodeIndex> = worklist.iter().copied().collect();

        let mut iterations = 0;
        let mut converged = true;
        let mut block_visit_count: rustc_hash::FxHashMap<NodeIndex, usize> =
            rustc_hash::FxHashMap::default();

        while let Some(block_idx) = worklist.pop_front() {
            worklist_set.remove(&block_idx);
            iterations += 1;
            *block_visit_count.entry(block_idx).or_insert(0) += 1;

            if iterations > self.max_iterations {
                let max_visits = block_visit_count.values().max().copied().unwrap_or(0);
                let avg_visits = iterations as f64 / num_blocks as f64;

                tracing::debug!(
                    "Backward dataflow analysis exceeded max iterations: {} iterations, {} blocks, max visits per block: {}, avg visits: {:.1}",
                    iterations,
                    num_blocks,
                    max_visits,
                    avg_visits
                );

                let mut frequent_blocks: Vec<_> = block_visit_count.iter().collect();
                frequent_blocks.sort_by_key(|(_, &count)| std::cmp::Reverse(count));

                if !frequent_blocks.is_empty() {
                    tracing::debug!("Top 5 most visited blocks:");
                    for (idx, count) in frequent_blocks.iter().take(5) {
                        tracing::debug!("  Block {:?}: {} visits", idx, count);
                    }
                }

                converged = false;
                break;
            }

            let has_successors = self.cfg.outgoing_edges(block_idx).next().is_some();
            let is_exit = block_idx == exit;

            let out_state = if is_exit && !has_successors {
                self.block_out.get(&block_idx).cloned().expect("block_out should be initialized")
            } else {
                let mut state: Option<L> = None;
                for (succ_idx, edge_kind) in self.cfg.outgoing_edges(block_idx) {
                    if let Some(succ_in) = self.block_in.get(&succ_idx) {
                        let edge_state = self.transfer.transfer_edge(*edge_kind, succ_in);
                        match &mut state {
                            None => {
                                state = Some(edge_state);
                            }
                            Some(s) => {
                                s.join_in_place(&edge_state);
                            }
                        }
                    }
                }
                state.unwrap_or_else(|| {
                    self.block_out
                        .get(&block_idx)
                        .cloned()
                        .expect("block_out should be initialized")
                })
            };

            self.block_out.insert(block_idx, out_state.clone());

            let in_state = self.transfer_block(block_idx, &out_state);

            let changed = if let Some(old_in) = self.block_in.get(&block_idx) {
                &in_state != old_in
            } else {
                true
            };

            if changed {
                self.block_in.insert(block_idx, in_state);

                for (pred_idx, _edge) in self.cfg.incoming_edges(block_idx) {
                    if worklist_set.insert(pred_idx) {
                        worklist.push_back(pred_idx);
                    }
                }
            }
        }

        let max_visits = block_visit_count.values().max().copied().unwrap_or(0);
        let avg_visits = if num_blocks > 0 { iterations as f64 / num_blocks as f64 } else { 0.0 };

        if iterations > 100 {
            tracing::info!(
                "Backward dataflow analysis converged: {} iterations, {} blocks, max visits: {}, avg visits: {:.1}",
                iterations,
                num_blocks,
                max_visits,
                avg_visits
            );
        } else {
            tracing::debug!(
                "Backward dataflow analysis converged: {} iterations, {} blocks",
                iterations,
                num_blocks
            );
        }

        Some(DataflowResult {
            block_in: self.block_in,
            block_out: self.block_out,
            cfg: self.cfg,
            converged,
        })
    }

    fn transfer_block(&self, block_idx: NodeIndex, in_state: &L) -> L {
        use cfg::CfgVertex;

        let Some(vertex) = self.cfg.vertex(block_idx) else {
            return in_state.clone();
        };

        match vertex {
            CfgVertex::BasicBlock(block) => {
                let mut state = in_state.clone();
                for &stmt_id in block.statements() {
                    self.transfer.transfer_stmt_in_place(
                        stmt_id.into_raw(),
                        &mut state,
                        &self.body,
                    );
                }
                state
            }

            CfgVertex::WhileHeader(while_vertex) => {
                let mut state = in_state.clone();
                self.transfer.transfer_expr_in_place(
                    while_vertex.condition,
                    &mut state,
                    &self.body,
                );
                state
            }

            CfgVertex::Conditional(conditional_vertex) => {
                let mut state = in_state.clone();
                self.transfer.transfer_expr_in_place(
                    conditional_vertex.condition,
                    &mut state,
                    &self.body,
                );
                state
            }

            CfgVertex::ForHeader(for_vertex) => {
                let mut state = in_state.clone();
                self.transfer.transfer_expr_in_place(for_vertex.from, &mut state, &self.body);
                self.transfer.transfer_expr_in_place(for_vertex.to, &mut state, &self.body);
                self.transfer.transfer_loop_var_bind(for_vertex.loop_var, &mut state, &self.body);
                state
            }

            CfgVertex::ForEachHeader(foreach_vertex) => {
                let mut state = in_state.clone();
                self.transfer.transfer_expr_in_place(
                    foreach_vertex.collection,
                    &mut state,
                    &self.body,
                );
                self.transfer.transfer_loop_var_bind(
                    foreach_vertex.loop_var,
                    &mut state,
                    &self.body,
                );
                state
            }

            CfgVertex::Try => {
                let mut state = in_state.clone();
                if let Some(try_stmt) = self.cfg.source_stmt_id(block_idx) {
                    self.transfer.transfer_try_entry(try_stmt, &mut state, &self.body);
                }
                state
            }

            _ => in_state.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataflowResult<L: Lattice> {
    block_in: FxHashMap<NodeIndex, L>,

    block_out: FxHashMap<NodeIndex, L>,

    cfg: Arc<ControlFlowGraph>,

    converged: bool,
}

impl<L: Lattice> DataflowResult<L> {
    pub fn block_in(&self, block: NodeIndex) -> Option<&L> {
        self.block_in.get(&block)
    }

    pub fn block_out(&self, block: NodeIndex) -> Option<&L> {
        self.block_out.get(&block)
    }

    pub fn cfg(&self) -> &ControlFlowGraph {
        &self.cfg
    }

    /// `false` when the iteration budget ran out and the states are a partial solution.
    pub fn converged(&self) -> bool {
        self.converged
    }

    pub fn blocks(&self) -> impl Iterator<Item = (NodeIndex, &L, &L)> {
        self.block_in.iter().filter_map(move |(idx, in_state)| {
            self.block_out.get(idx).map(|out_state| (*idx, in_state, out_state))
        })
    }

    /// Approximate live heap bytes for Salsa's `memory_usage` report: the two
    /// per-block lattice maps (`block_in`/`block_out`). `value_heap` supplies the
    /// heap owned by each lattice value (e.g. a bitset's words) — the generic `L`
    /// cannot report it here. The shared `Arc<ControlFlowGraph>` is counted as the
    /// pointer only (its payload is owned by the CFG memo), so it is omitted; the
    /// HIR `Body` is not retained here (callers re-source it from the body memo).
    pub fn estimated_heap_with(&self, value_heap: impl Fn(&L) -> usize) -> usize {
        let mut bytes = map_table_bytes::<NodeIndex, L>(self.block_in.len());
        for v in self.block_in.values() {
            bytes += value_heap(v);
        }
        bytes += map_table_bytes::<NodeIndex, L>(self.block_out.len());
        for v in self.block_out.values() {
            bytes += value_heap(v);
        }
        bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct IntSetLattice {
        values: Vec<i32>,
    }

    impl Lattice for IntSetLattice {
        fn join(&self, other: &Self) -> Self {
            let mut values = self.values.clone();
            for &v in &other.values {
                if !values.contains(&v) {
                    values.push(v);
                }
            }
            values.sort_unstable();
            Self { values }
        }
    }

    #[test]
    fn test_lattice_bottom() {
        let bottom = IntSetLattice { values: vec![] };
        assert!(bottom.values.is_empty());
    }

    #[test]
    fn test_lattice_join() {
        let a = IntSetLattice { values: vec![1, 2, 3] };
        let b = IntSetLattice { values: vec![2, 3, 4] };
        let joined = a.join(&b);
        assert_eq!(joined.values, vec![1, 2, 3, 4]);
    }

    #[test]
    fn test_lattice_join_idempotent() {
        let a = IntSetLattice { values: vec![1, 2, 3] };
        let joined = a.join(&a);
        assert_eq!(joined, a);
    }

    #[test]
    fn test_lattice_join_commutative() {
        let a = IntSetLattice { values: vec![1, 2] };
        let b = IntSetLattice { values: vec![3, 4] };
        assert_eq!(a.join(&b), b.join(&a));
    }

    #[test]
    fn test_lattice_bottom_identity() {
        let a = IntSetLattice { values: vec![1, 2, 3] };
        let bottom = IntSetLattice { values: vec![] };
        assert_eq!(bottom.join(&a), a);
        assert_eq!(a.join(&bottom), a);
    }

    struct NoopTransfer;

    impl Transfer<IntSetLattice> for NoopTransfer {
        fn transfer_stmt(&self, _: RawIdx, state: &IntSetLattice, _: &Body) -> IntSetLattice {
            state.clone()
        }
    }

    #[test]
    fn transfer_edge_default_is_identity_across_all_edge_kinds() {
        let s = IntSetLattice { values: vec![1, 2, 3] };
        let t = NoopTransfer;
        for edge in [
            CfgEdgeType::Unconditional,
            CfgEdgeType::TrueBranch,
            CfgEdgeType::FalseBranch,
            CfgEdgeType::Exception,
            CfgEdgeType::Unexecutable,
        ] {
            assert_eq!(t.transfer_edge(edge, &s), s, "edge {edge:?}");
        }
    }

    struct SignSplitTransfer;

    impl Transfer<IntSetLattice> for SignSplitTransfer {
        fn transfer_stmt(&self, _: RawIdx, state: &IntSetLattice, _: &Body) -> IntSetLattice {
            state.clone()
        }
        fn transfer_edge(&self, edge_kind: CfgEdgeType, state: &IntSetLattice) -> IntSetLattice {
            match edge_kind {
                CfgEdgeType::TrueBranch => IntSetLattice {
                    values: state.values.iter().copied().filter(|v| *v > 0).collect(),
                },
                CfgEdgeType::FalseBranch => IntSetLattice {
                    values: state.values.iter().copied().filter(|v| *v <= 0).collect(),
                },
                _ => state.clone(),
            }
        }
    }

    #[test]
    fn transfer_edge_override_refines_per_branch() {
        let s = IntSetLattice { values: vec![-2, -1, 0, 1, 2] };
        let t = SignSplitTransfer;
        assert_eq!(t.transfer_edge(CfgEdgeType::TrueBranch, &s).values, vec![1, 2]);
        assert_eq!(t.transfer_edge(CfgEdgeType::FalseBranch, &s).values, vec![-2, -1, 0]);
        assert_eq!(t.transfer_edge(CfgEdgeType::Unconditional, &s), s);
        assert_eq!(t.transfer_edge(CfgEdgeType::Exception, &s), s);
        assert_eq!(t.transfer_edge(CfgEdgeType::Unexecutable, &s), s);
    }

    #[test]
    fn solve_forward_applies_transfer_edge_at_branch_successors() {
        use cfg::{BasicBlockVertex, CfgVertex, ControlFlowGraph};

        let mut cfg = ControlFlowGraph::new();
        let entry = cfg.add_vertex(CfgVertex::BasicBlock(BasicBlockVertex::new()));
        let cond = cfg.add_vertex(CfgVertex::BasicBlock(BasicBlockVertex::new()));
        let tside = cfg.add_vertex(CfgVertex::BasicBlock(BasicBlockVertex::new()));
        let fside = cfg.add_vertex(CfgVertex::BasicBlock(BasicBlockVertex::new()));
        let merge = cfg.add_vertex(CfgVertex::BasicBlock(BasicBlockVertex::new()));
        let exit = cfg.exit();
        cfg.set_start(entry);
        cfg.add_edge(entry, cond, CfgEdgeType::Unconditional);
        cfg.add_edge(cond, tside, CfgEdgeType::TrueBranch);
        cfg.add_edge(cond, fside, CfgEdgeType::FalseBranch);
        cfg.add_edge(tside, merge, CfgEdgeType::Unconditional);
        cfg.add_edge(fside, merge, CfgEdgeType::Unconditional);
        cfg.add_edge(merge, exit, CfgEdgeType::Unconditional);

        let body = Body::default();
        let initial = IntSetLattice { values: vec![-2, -1, 0, 1, 2] };

        let mut solver = DataflowSolver::new(Arc::new(cfg), body, SignSplitTransfer);
        let bottom = IntSetLattice { values: vec![] };
        solver.set_bottom_factory(move || bottom.clone());
        solver.set_initial_state(initial.clone());
        let result = solver.solve().expect("forward solve converges");

        let tside_in = result.block_in(tside).expect("tside IN exists");
        assert_eq!(tside_in.values, vec![1, 2], "TrueBranch edge did not refine state");

        let fside_in = result.block_in(fside).expect("fside IN exists");
        assert_eq!(fside_in.values, vec![-2, -1, 0], "FalseBranch edge did not refine state");

        let merge_in = result.block_in(merge).expect("merge IN exists");
        let mut merged: Vec<i32> = merge_in.values.clone();
        merged.sort_unstable();
        assert_eq!(merged, vec![-2, -1, 0, 1, 2], "diamond merge must recover full set");
    }
}
