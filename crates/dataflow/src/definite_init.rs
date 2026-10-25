//! Definite initialization: a must-analysis over the CFG that answers whether a
//! tracked name has been assigned on every executable path reaching a point.
//!
//! The analysis knows nothing about names or resolution: the caller numbers the
//! tracked names and describes, per statement and header root, which bits each
//! one sets.

use std::collections::VecDeque;
use std::sync::Arc;

use cfg::{CfgEdgeType, CfgVertex, ControlFlowGraph};
use fixedbitset::FixedBitSet;
use hir_def::{body::Body, BindingId, ExprId, StmtId};
use la_arena::RawIdx;
use petgraph::graph::NodeIndex;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::{DataflowResult, DataflowSolver, Lattice, Transfer};

/// Where a read is evaluated: before a statement of a basic block, or inside a
/// header root (`Если`/`ИначеЕсли`/`Пока` condition, `Для` bounds, `Для Каждого`
/// collection).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InitPoint {
    Stmt(StmtId),
    Header(ExprId),
}

/// Bits set by each place of the body; bits are positions of the tracked names.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InitEvents {
    pub width: usize,
    pub stmt: FxHashMap<StmtId, Vec<u32>>,
    pub header: FxHashMap<ExprId, Vec<u32>>,
    pub loop_var: FxHashMap<BindingId, u32>,
    pub entering_headers: FxHashMap<StmtId, EnteringHeader>,
}

/// A loop header whose literal condition decides its false branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EnteringHeader {
    /// `Пока Истина`: the false branch is never taken.
    NeverExits,
    /// `Для` over literal bounds `from ≤ to`: the first arrival always enters the body.
    EntersOnFirstArrival,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DefiniteInit {
    /// No executable path reaches the point; neutral for the meet.
    Unreached,
    /// Bits assigned on every executable path reaching the point.
    Reached(FixedBitSet),
}

impl DefiniteInit {
    /// `None` when the point is unreachable.
    pub fn is_initialized(&self, bit: u32) -> Option<bool> {
        match self {
            DefiniteInit::Unreached => None,
            DefiniteInit::Reached(set) => Some(set.contains(bit as usize)),
        }
    }

    fn lacks(&self, bit: u32) -> bool {
        self.is_initialized(bit) == Some(false)
    }

    fn add(&mut self, bits: &[u32]) {
        if let DefiniteInit::Reached(set) = self {
            for &bit in bits {
                set.insert(bit as usize);
            }
        }
    }
}

impl Lattice for DefiniteInit {
    fn join(&self, other: &Self) -> Self {
        match (self, other) {
            (DefiniteInit::Unreached, x) | (x, DefiniteInit::Unreached) => x.clone(),
            (DefiniteInit::Reached(a), DefiniteInit::Reached(b)) => {
                let mut met = a.clone();
                met.intersect_with(b);
                DefiniteInit::Reached(met)
            }
        }
    }
}

/// One edge of a path without initialization: control leaves `vertex` along `edge`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WitnessStep {
    pub vertex: NodeIndex,
    pub edge: CfgEdgeType,
}

struct DefiniteInitTransfer {
    events: Arc<InitEvents>,
}

impl Transfer<DefiniteInit> for DefiniteInitTransfer {
    fn transfer_stmt(&self, stmt_id: RawIdx, state: &DefiniteInit, _body: &Body) -> DefiniteInit {
        let mut state = state.clone();
        if let Some(bits) = self.events.stmt.get(&StmtId::from_raw(stmt_id)) {
            state.add(bits);
        }
        state
    }

    fn transfer_expr(&self, expr_id: ExprId, state: &DefiniteInit, _body: &Body) -> DefiniteInit {
        let mut state = state.clone();
        if let Some(bits) = self.events.header.get(&expr_id) {
            state.add(bits);
        }
        state
    }

    fn transfer_edge(&self, edge_kind: CfgEdgeType, state: &DefiniteInit) -> DefiniteInit {
        match edge_kind {
            CfgEdgeType::Unexecutable => DefiniteInit::Unreached,
            _ => state.clone(),
        }
    }

    fn transfer_loop_var_bind(&self, loop_var: BindingId, state: &mut DefiniteInit, _body: &Body) {
        if let Some(&bit) = self.events.loop_var.get(&loop_var) {
            state.add(&[bit]);
        }
    }
}

pub struct DefiniteInitResult {
    inner: DataflowResult<DefiniteInit>,
    events: Arc<InitEvents>,
    states: FxHashMap<InitPoint, DefiniteInit>,
    vertices: FxHashMap<InitPoint, NodeIndex>,
    rpo: FxHashMap<NodeIndex, usize>,
}

/// Solves definite initialization; `None` when the solver ran out of its
/// iteration budget, because a partial solution proves nothing.
pub fn analyze_definite_init(
    cfg: Arc<ControlFlowGraph>,
    body: Body,
    events: Arc<InitEvents>,
    max_iterations: usize,
) -> Option<DefiniteInitResult> {
    let transfer = DefiniteInitTransfer { events: events.clone() };
    let mut solver = DataflowSolver::new(cfg, body, transfer);
    solver.set_max_iterations(max_iterations);
    solver.set_bottom_factory(|| DefiniteInit::Unreached);
    solver.set_initial_state(DefiniteInit::Reached(FixedBitSet::with_capacity(events.width)));
    let inner = solver.solve()?;
    if !inner.converged() {
        return None;
    }

    let mut states = FxHashMap::default();
    let mut vertices = FxHashMap::default();
    let mut record = |point: InitPoint, vertex: NodeIndex, state: &DefiniteInit| {
        states.insert(point, state.clone());
        vertices.insert(point, vertex);
    };
    let header_events = |state: &mut DefiniteInit, root: ExprId| {
        if let Some(bits) = events.header.get(&root) {
            state.add(bits);
        }
    };
    let cfg = inner.cfg();
    for (vertex, kind) in cfg.vertices() {
        let Some(entry) = inner.block_in(vertex) else { continue };
        match kind {
            CfgVertex::BasicBlock(block) => {
                let mut state = entry.clone();
                for &stmt in block.statements() {
                    record(InitPoint::Stmt(stmt), vertex, &state);
                    if let Some(bits) = events.stmt.get(&stmt) {
                        state.add(bits);
                    }
                }
            }
            CfgVertex::Conditional(header) => {
                record(InitPoint::Header(header.condition), vertex, entry);
            }
            CfgVertex::WhileHeader(header) => {
                record(InitPoint::Header(header.condition), vertex, entry);
            }
            CfgVertex::ForHeader(header) => {
                record(InitPoint::Header(header.from), vertex, entry);
                let mut state = entry.clone();
                header_events(&mut state, header.from);
                record(InitPoint::Header(header.to), vertex, &state);
            }
            CfgVertex::ForEachHeader(header) => {
                record(InitPoint::Header(header.collection), vertex, entry);
            }
            _ => {}
        }
    }
    let rpo = cfg
        .reverse_postorder()
        .into_iter()
        .enumerate()
        .map(|(pos, vertex)| (vertex, pos))
        .collect();

    Some(DefiniteInitResult { inner, events, states, vertices, rpo })
}

impl DefiniteInitResult {
    pub fn cfg(&self) -> &ControlFlowGraph {
        self.inner.cfg()
    }

    /// State on entry to `point`; `None` when the point is outside the CFG.
    pub fn state_before(&self, point: InitPoint) -> Option<&DefiniteInit> {
        self.states.get(&point)
    }

    pub fn vertex_of(&self, point: InitPoint) -> Option<NodeIndex> {
        self.vertices.get(&point).copied()
    }

    /// An edge `from → to` returns to an earlier vertex of the reverse postorder.
    /// A vertex outside the order has no position and closes no back edge.
    fn is_back_edge(&self, from: NodeIndex, to: NodeIndex) -> bool {
        match (self.rpo.get(&from), self.rpo.get(&to)) {
            (Some(from), Some(to)) => from >= to,
            _ => false,
        }
    }

    fn back_edges(&self, header: NodeIndex) -> impl Iterator<Item = NodeIndex> + '_ {
        self.cfg()
            .incoming_edges(header)
            .filter(move |(from, edge)| edge.is_executable() && self.is_back_edge(*from, header))
            .map(|(from, _)| from)
    }

    /// Every back edge into `header` that some path takes carries `bit`, and at
    /// least one such edge exists.
    pub fn back_edges_initialize(&self, header: NodeIndex, bit: u32) -> bool {
        let mut any = false;
        for from in self.back_edges(header) {
            match self.inner.block_out(from).and_then(|state| state.is_initialized(bit)) {
                Some(true) => any = true,
                Some(false) => return false,
                None => {}
            }
        }
        any
    }

    /// A path from the start to `point` on which no vertex sets `bit`, as the
    /// edges it takes; empty when the point sits in the start vertex.
    pub fn witness(&self, point: InitPoint, bit: u32) -> Option<Vec<WitnessStep>> {
        let target = self.vertex_of(point)?;
        let cfg = self.cfg();
        let start = cfg.start()?;
        if target == start {
            return Some(Vec::new());
        }

        let origin: Pair = (target, false);
        let mut queue: VecDeque<Pair> = VecDeque::from([origin]);
        let mut visited: FxHashSet<Pair> = FxHashSet::from_iter([origin]);
        let mut parent: FxHashMap<Pair, (Pair, CfgEdgeType)> = FxHashMap::default();

        while let Some(pair) = queue.pop_front() {
            let (vertex, back_only) = pair;
            for (from, &edge) in cfg.incoming_edges(vertex) {
                if !edge.is_executable() || (back_only && !self.is_back_edge(from, vertex)) {
                    continue;
                }
                if !self.inner.block_out(from).is_some_and(|state| state.lacks(bit)) {
                    continue;
                }
                let entering = cfg
                    .source_stmt_id(from)
                    .and_then(|stmt| self.events.entering_headers.get(&stmt))
                    .filter(|_| edge == CfgEdgeType::FalseBranch);
                let next: Pair = match entering {
                    Some(EnteringHeader::NeverExits) => continue,
                    Some(EnteringHeader::EntersOnFirstArrival) => (from, true),
                    None => (from, false),
                };
                if !visited.insert(next) {
                    continue;
                }
                parent.insert(next, (pair, edge));
                if next == (start, false) {
                    return Some(steps_to(&parent, next, origin));
                }
                queue.push_back(next);
            }
        }
        None
    }
}

/// A vertex of the witness search plus "it may be entered only along a back edge".
type Pair = (NodeIndex, bool);

fn steps_to(
    parent: &FxHashMap<Pair, (Pair, CfgEdgeType)>,
    from: Pair,
    origin: Pair,
) -> Vec<WitnessStep> {
    let mut steps = Vec::new();
    let mut pair = from;
    while pair != origin {
        let (next, edge) = parent[&pair];
        steps.push(WitnessStep { vertex: pair.0, edge });
        pair = next;
    }
    steps
}

#[cfg(test)]
mod tests {
    use cfg::{
        BasicBlockVertex, ConditionalVertex, ForEachHeaderVertex, ForHeaderVertex,
        WhileHeaderVertex,
    };

    use super::*;
    use crate::DEFAULT_MAX_ITERATIONS;

    fn stmt(n: u32) -> StmtId {
        StmtId::from_raw(RawIdx::from(n))
    }

    fn expr(n: u32) -> ExprId {
        ExprId::from_raw(RawIdx::from(n))
    }

    fn block(stmts: &[u32]) -> CfgVertex {
        let mut block = BasicBlockVertex::new();
        for &n in stmts {
            block.add_statement(stmt(n));
        }
        CfgVertex::BasicBlock(block)
    }

    fn events(width: usize, stmt_bits: &[(u32, u32)]) -> InitEvents {
        let mut events = InitEvents { width, ..InitEvents::default() };
        for &(n, bit) in stmt_bits {
            events.stmt.entry(stmt(n)).or_default().push(bit);
        }
        events
    }

    fn solve(cfg: ControlFlowGraph, events: InitEvents) -> DefiniteInitResult {
        analyze_definite_init(
            Arc::new(cfg),
            Body::default(),
            Arc::new(events),
            DEFAULT_MAX_ITERATIONS,
        )
        .expect("converges")
    }

    struct LoopCfg {
        cfg: ControlFlowGraph,
        start: NodeIndex,
        header: NodeIndex,
    }

    /// `[s0]` → `Пока e0` (statement 10) → body `[s1, s2]` → back to the header;
    /// the false branch leads to `[s3]` and the exit.
    fn while_loop() -> LoopCfg {
        let mut cfg = ControlFlowGraph::new();
        let start = cfg.add_vertex(block(&[0]));
        let header = cfg.add_vertex_with_origin(
            CfgVertex::WhileHeader(WhileHeaderVertex::new(expr(0))),
            stmt(10),
        );
        let body = cfg.add_vertex(block(&[1, 2]));
        let after = cfg.add_vertex(block(&[3]));
        let exit = cfg.exit();
        cfg.set_start(start);
        cfg.add_edge(start, header, CfgEdgeType::Unconditional);
        cfg.add_edge(header, body, CfgEdgeType::TrueBranch);
        cfg.add_edge(body, header, CfgEdgeType::Unconditional);
        cfg.add_edge(header, after, CfgEdgeType::FalseBranch);
        cfg.add_edge(after, exit, CfgEdgeType::Unconditional);
        LoopCfg { cfg, start, header }
    }

    #[test]
    fn first_iteration_read_is_not_initialized() {
        let LoopCfg { cfg, start, header } = while_loop();
        let result = solve(cfg, events(1, &[(2, 0)]));

        let in_body = result.state_before(InitPoint::Stmt(stmt(1))).unwrap();
        assert_eq!(in_body.is_initialized(0), Some(false));
        let after = result.state_before(InitPoint::Stmt(stmt(3))).unwrap();
        assert_eq!(after.is_initialized(0), Some(false));
        assert_eq!(
            result.witness(InitPoint::Stmt(stmt(1)), 0),
            Some(vec![
                WitnessStep { vertex: start, edge: CfgEdgeType::Unconditional },
                WitnessStep { vertex: header, edge: CfgEdgeType::TrueBranch },
            ])
        );
        assert!(result.back_edges_initialize(header, 0));

        let LoopCfg { cfg, .. } = while_loop();
        let result = solve(cfg, events(1, &[(0, 0), (2, 0)]));
        let in_body = result.state_before(InitPoint::Stmt(stmt(1))).unwrap();
        assert_eq!(in_body.is_initialized(0), Some(true));
    }

    #[test]
    fn unexecutable_edge_carries_unreached() {
        let mut cfg = ControlFlowGraph::new();
        let start = cfg.add_vertex(block(&[0]));
        let dead = cfg.add_vertex(block(&[1]));
        let exit = cfg.exit();
        cfg.set_start(start);
        cfg.add_edge(start, dead, CfgEdgeType::Unexecutable);
        cfg.add_edge(dead, exit, CfgEdgeType::Unconditional);
        let result = solve(cfg, events(1, &[]));
        assert_eq!(result.state_before(InitPoint::Stmt(stmt(1))).unwrap().is_initialized(0), None);

        let mut cfg = ControlFlowGraph::new();
        let start = cfg.add_vertex(block(&[0]));
        let cond = cfg.add_vertex(CfgVertex::Conditional(ConditionalVertex::new(expr(0))));
        let live = cfg.add_vertex(block(&[1]));
        let exiting = cfg.add_vertex(block(&[2]));
        let merge = cfg.add_vertex(block(&[3]));
        let exit = cfg.exit();
        cfg.set_start(start);
        cfg.add_edge(start, cond, CfgEdgeType::Unconditional);
        cfg.add_edge(cond, live, CfgEdgeType::TrueBranch);
        cfg.add_edge(cond, exiting, CfgEdgeType::FalseBranch);
        cfg.add_edge(live, merge, CfgEdgeType::Unconditional);
        cfg.add_edge(exiting, exit, CfgEdgeType::Unconditional);
        cfg.add_edge(exiting, merge, CfgEdgeType::Unexecutable);
        cfg.add_edge(merge, exit, CfgEdgeType::Unconditional);
        let result = solve(cfg, events(1, &[(1, 0)]));
        assert_eq!(
            result.state_before(InitPoint::Stmt(stmt(3))).unwrap().is_initialized(0),
            Some(true)
        );
    }

    #[test]
    fn loop_variable_bind_sets_its_bit() {
        let for_loop = || {
            let mut cfg = ControlFlowGraph::new();
            let start = cfg.add_vertex(block(&[0]));
            let header = cfg.add_vertex_with_origin(
                CfgVertex::ForHeader(ForHeaderVertex::new(
                    BindingId::from_raw(RawIdx::from(0)),
                    expr(0),
                    expr(1),
                )),
                stmt(10),
            );
            let body = cfg.add_vertex(block(&[1]));
            let after = cfg.add_vertex(block(&[2]));
            let exit = cfg.exit();
            cfg.set_start(start);
            cfg.add_edge(start, header, CfgEdgeType::Unconditional);
            cfg.add_edge(header, body, CfgEdgeType::TrueBranch);
            cfg.add_edge(body, header, CfgEdgeType::Unconditional);
            cfg.add_edge(header, after, CfgEdgeType::FalseBranch);
            cfg.add_edge(after, exit, CfgEdgeType::Unconditional);
            cfg
        };
        let initialized = |result: &DefiniteInitResult, n: u32| {
            result.state_before(InitPoint::Stmt(stmt(n))).unwrap().is_initialized(0)
        };

        let mut bound = events(1, &[]);
        bound.loop_var.insert(BindingId::from_raw(RawIdx::from(0)), 0);
        let result = solve(for_loop(), bound);
        assert_eq!(initialized(&result, 1), Some(true));
        assert_eq!(initialized(&result, 2), Some(true));

        let result = solve(for_loop(), events(1, &[]));
        assert_eq!(initialized(&result, 1), Some(false));
        assert_eq!(initialized(&result, 2), Some(false));
    }

    #[test]
    fn literal_headers_shape_the_witness() {
        let after = InitPoint::Stmt(stmt(3));
        let with_header = |stmt_bits: &[(u32, u32)], header: Option<EnteringHeader>| {
            let mut events = events(1, stmt_bits);
            if let Some(header) = header {
                events.entering_headers.insert(stmt(10), header);
            }
            solve(while_loop().cfg, events)
        };

        assert!(with_header(&[], None).witness(after, 0).is_some());
        assert_eq!(with_header(&[], Some(EnteringHeader::NeverExits)).witness(after, 0), None);
        assert_eq!(
            with_header(&[(2, 0)], Some(EnteringHeader::EntersOnFirstArrival)).witness(after, 0),
            None
        );
        assert!(with_header(&[], Some(EnteringHeader::EntersOnFirstArrival))
            .witness(after, 0)
            .is_some());
    }

    #[test]
    fn budget_exhaustion_yields_none() {
        let run = |max_iterations| {
            analyze_definite_init(
                Arc::new(while_loop().cfg),
                Body::default(),
                Arc::new(events(1, &[(2, 0)])),
                max_iterations,
            )
        };
        assert!(run(1).is_none());
        assert!(run(DEFAULT_MAX_ITERATIONS).is_some());
    }
    fn reached(bits: &[usize]) -> DefiniteInit {
        let mut set = FixedBitSet::with_capacity(4);
        set.extend(bits.iter().copied());
        DefiniteInit::Reached(set)
    }

    #[test]
    fn join_meets_reached_states_and_ignores_unreached() {
        assert_eq!(reached(&[0, 1]).join(&reached(&[1, 2])), reached(&[1]));
        assert_eq!(DefiniteInit::Unreached.join(&reached(&[0])), reached(&[0]));
        assert_eq!(reached(&[0]).join(&DefiniteInit::Unreached), reached(&[0]));
        assert_eq!(DefiniteInit::Unreached.join(&DefiniteInit::Unreached), DefiniteInit::Unreached);
        assert_eq!(reached(&[1]).is_initialized(1), Some(true));
        assert_eq!(reached(&[1]).is_initialized(0), Some(false));
        assert_eq!(DefiniteInit::Unreached.is_initialized(1), None);
    }

    /// `[s0]` → `Для` (bounds e0, e1; statement 10) → body `[s1]`, back; false →
    /// `Для Каждого` (collection e2; statement 11) → body `[s2]`, back; false →
    /// `Если` (e3; statement 12) → `[s3]` / `[s4]` → merge `[s5]`.
    struct HeaderCfg {
        cfg: ControlFlowGraph,
        for_header: NodeIndex,
        for_each_header: NodeIndex,
        conditional: NodeIndex,
    }

    fn header_cfg() -> HeaderCfg {
        let mut cfg = ControlFlowGraph::new();
        let start = cfg.add_vertex(block(&[0]));
        let for_header = cfg.add_vertex_with_origin(
            CfgVertex::ForHeader(ForHeaderVertex::new(
                BindingId::from_raw(RawIdx::from(0)),
                expr(0),
                expr(1),
            )),
            stmt(10),
        );
        let for_body = cfg.add_vertex(block(&[1]));
        let for_each_header = cfg.add_vertex_with_origin(
            CfgVertex::ForEachHeader(ForEachHeaderVertex::new(
                BindingId::from_raw(RawIdx::from(1)),
                expr(2),
            )),
            stmt(11),
        );
        let for_each_body = cfg.add_vertex(block(&[2]));
        let conditional = cfg.add_vertex_with_origin(
            CfgVertex::Conditional(ConditionalVertex::new(expr(3))),
            stmt(12),
        );
        let then_block = cfg.add_vertex(block(&[3]));
        let else_block = cfg.add_vertex(block(&[4]));
        let merge = cfg.add_vertex(block(&[5]));
        let exit = cfg.exit();
        cfg.set_start(start);
        cfg.add_edge(start, for_header, CfgEdgeType::Unconditional);
        cfg.add_edge(for_header, for_body, CfgEdgeType::TrueBranch);
        cfg.add_edge(for_body, for_header, CfgEdgeType::Unconditional);
        cfg.add_edge(for_header, for_each_header, CfgEdgeType::FalseBranch);
        cfg.add_edge(for_each_header, for_each_body, CfgEdgeType::TrueBranch);
        cfg.add_edge(for_each_body, for_each_header, CfgEdgeType::Unconditional);
        cfg.add_edge(for_each_header, conditional, CfgEdgeType::FalseBranch);
        cfg.add_edge(conditional, then_block, CfgEdgeType::TrueBranch);
        cfg.add_edge(conditional, else_block, CfgEdgeType::FalseBranch);
        cfg.add_edge(then_block, merge, CfgEdgeType::Unconditional);
        cfg.add_edge(else_block, merge, CfgEdgeType::Unconditional);
        cfg.add_edge(merge, exit, CfgEdgeType::Unconditional);
        HeaderCfg { cfg, for_header, for_each_header, conditional }
    }

    #[test]
    fn header_points_are_recorded_with_their_events() {
        let HeaderCfg { cfg, for_header, for_each_header, conditional } = header_cfg();
        let mut events = events(1, &[]);
        events.header.insert(expr(0), vec![0]);
        let result = solve(cfg, events);
        let initialized = |point| result.state_before(point).unwrap().is_initialized(0);

        assert_eq!(initialized(InitPoint::Header(expr(0))), Some(false));
        assert_eq!(initialized(InitPoint::Header(expr(1))), Some(true));
        assert_eq!(initialized(InitPoint::Header(expr(2))), Some(true));
        assert_eq!(initialized(InitPoint::Header(expr(3))), Some(true));
        assert_eq!(result.vertex_of(InitPoint::Header(expr(0))), Some(for_header));
        assert_eq!(result.vertex_of(InitPoint::Header(expr(1))), Some(for_header));
        assert_eq!(result.vertex_of(InitPoint::Header(expr(2))), Some(for_each_header));
        assert_eq!(result.vertex_of(InitPoint::Header(expr(3))), Some(conditional));
        assert_eq!(result.state_before(InitPoint::Header(expr(99))), None);
        assert_eq!(result.vertex_of(InitPoint::Stmt(stmt(99))), None);
    }

    #[test]
    fn witness_is_empty_in_the_start_vertex_and_absent_when_initialized() {
        let result = solve(header_cfg().cfg, events(1, &[(0, 0)]));
        assert_eq!(result.witness(InitPoint::Stmt(stmt(5)), 0), None);

        let result = solve(header_cfg().cfg, events(1, &[]));
        assert_eq!(result.witness(InitPoint::Stmt(stmt(0)), 0), Some(Vec::new()));
        assert!(result.witness(InitPoint::Stmt(stmt(5)), 0).is_some());
    }

    #[test]
    fn back_edges_initialize_needs_a_back_edge_and_ignores_unexecutable_ones() {
        let HeaderCfg { cfg, for_header, conditional, .. } = header_cfg();
        let result = solve(cfg, events(1, &[(1, 0)]));
        assert!(result.back_edges_initialize(for_header, 0));
        assert!(!result.back_edges_initialize(conditional, 0));

        let LoopCfg { cfg, header, .. } = while_loop();
        let result = solve(cfg, events(1, &[]));
        assert!(!result.back_edges_initialize(header, 0));
    }
    #[test]
    fn witness_avoids_unexecutable_edges() {
        // The shortest way into `merge` leaves the jump block `[s1]` along its dead
        // fall-through edge; the witness must take the longer executable way.
        let mut cfg = ControlFlowGraph::new();
        let start = cfg.add_vertex(block(&[0]));
        let cond = cfg.add_vertex(CfgVertex::Conditional(ConditionalVertex::new(expr(0))));
        let jump = cfg.add_vertex(block(&[1]));
        let first = cfg.add_vertex(block(&[2]));
        let second = cfg.add_vertex(block(&[3]));
        let merge = cfg.add_vertex(block(&[4]));
        let exit = cfg.exit();
        cfg.set_start(start);
        cfg.add_edge(start, cond, CfgEdgeType::Unconditional);
        cfg.add_edge(cond, jump, CfgEdgeType::TrueBranch);
        cfg.add_edge(jump, merge, CfgEdgeType::Unexecutable);
        cfg.add_edge(cond, first, CfgEdgeType::FalseBranch);
        cfg.add_edge(first, second, CfgEdgeType::Unconditional);
        cfg.add_edge(second, merge, CfgEdgeType::Unconditional);
        cfg.add_edge(merge, exit, CfgEdgeType::Unconditional);
        let result = solve(cfg, events(1, &[]));
        assert_eq!(
            result.witness(InitPoint::Stmt(stmt(4)), 0),
            Some(vec![
                WitnessStep { vertex: start, edge: CfgEdgeType::Unconditional },
                WitnessStep { vertex: cond, edge: CfgEdgeType::FalseBranch },
                WitnessStep { vertex: first, edge: CfgEdgeType::Unconditional },
                WitnessStep { vertex: second, edge: CfgEdgeType::Unconditional },
            ])
        );
    }
}
