use crate::edge::CfgEdgeType;
use crate::vertex::CfgVertex;
use cfg_types::StmtId;
use petgraph::graph::{DiGraph, NodeIndex};
use petgraph::visit::{DfsPostOrder, EdgeRef, Reversed};
use petgraph::Direction;
use rustc_hash::{FxHashMap, FxHashSet};

#[derive(Debug, Clone)]
pub struct ControlFlowGraph {
    graph: DiGraph<CfgVertex, CfgEdgeType>,
    statement_origins: FxHashMap<NodeIndex, StmtId>,
    start: Option<NodeIndex>,
    exit: NodeIndex,
}

impl PartialEq for ControlFlowGraph {
    fn eq(&self, other: &Self) -> bool {
        if self.start != other.start || self.exit != other.exit {
            return false;
        }

        if self.graph.node_count() != other.graph.node_count()
            || self.graph.edge_count() != other.graph.edge_count()
        {
            return false;
        }

        true
    }
}

impl Eq for ControlFlowGraph {}

impl ControlFlowGraph {
    pub fn new() -> Self {
        let mut graph = DiGraph::new();

        let exit = graph.add_node(CfgVertex::Exit);

        Self { graph, statement_origins: FxHashMap::default(), start: None, exit }
    }

    pub fn add_vertex(&mut self, vertex: CfgVertex) -> NodeIndex {
        self.graph.add_node(vertex)
    }

    pub fn add_vertex_with_origin(&mut self, vertex: CfgVertex, stmt_id: StmtId) -> NodeIndex {
        let index = self.add_vertex(vertex);
        self.statement_origins.insert(index, stmt_id);
        index
    }

    pub fn source_stmt_id(&self, index: NodeIndex) -> Option<StmtId> {
        self.statement_origins.get(&index).copied()
    }

    pub fn add_edge(&mut self, source: NodeIndex, target: NodeIndex, edge_type: CfgEdgeType) {
        self.graph.add_edge(source, target, edge_type);
    }

    pub fn set_start(&mut self, start: NodeIndex) {
        self.start = Some(start);
    }

    /// Where forward analyses begin. A graph built without a body has none.
    pub fn start(&self) -> Option<NodeIndex> {
        self.start
    }

    /// Exists in every graph, so backward analyses always have a seed.
    pub fn exit(&self) -> NodeIndex {
        self.exit
    }

    /// Approximate live heap bytes of this graph for Salsa's `memory_usage`
    /// report. Counts petgraph's node/edge backbone (a `Vec<Node>` and a
    /// `Vec<Edge>`, each element a weight plus four `u32` index links) at element
    /// granularity, plus statement-origin metadata and the only vertex-owned heap: a basic
    /// block's `Vec<StmtId>`.
    /// The other vertices own no extra heap. Spare capacity is not counted,
    /// so the figure tracks live content within a small factor.
    pub fn estimated_heap(&self) -> usize {
        use std::mem::size_of;

        // petgraph `Node<N, u32>` = weight + `[EdgeIndex; 2]` (8 bytes);
        // `Edge<E, u32>` = weight + `[EdgeIndex; 2]` + `[NodeIndex; 2]` (16 bytes).
        let mut bytes = self.graph.node_count() * (size_of::<CfgVertex>() + 8);
        bytes += self.graph.edge_count() * (size_of::<CfgEdgeType>() + 16);
        bytes += self.statement_origins.len() * size_of::<(NodeIndex, StmtId)>();

        for vertex in self.graph.node_weights() {
            if let CfgVertex::BasicBlock(block) = vertex {
                bytes += block.len() * size_of::<cfg_types::StmtId>();
            }
        }

        bytes
    }

    /// An `Exception` edge only saying the point may fail: the point also has
    /// its own continuation, so the edge is no decision of the text. The edge
    /// from the entry of `Попытка` and the edge of `ВызватьИсключение` are not
    /// such edges.
    pub fn is_possible_failure(&self, from: NodeIndex, kind: CfgEdgeType) -> bool {
        kind == CfgEdgeType::Exception
            && !matches!(self.vertex(from), Some(CfgVertex::Try))
            && self
                .outgoing_edges(from)
                .any(|(_, kind)| kind.is_executable() && *kind != CfgEdgeType::Exception)
    }

    pub fn vertex(&self, index: NodeIndex) -> Option<&CfgVertex> {
        self.graph.node_weight(index)
    }

    pub fn vertex_exists(&self, index: NodeIndex) -> bool {
        self.graph.node_weight(index).is_some()
    }

    pub(crate) fn vertex_mut(&mut self, index: NodeIndex) -> Option<&mut CfgVertex> {
        self.graph.node_weight_mut(index)
    }

    pub fn outgoing_edges(
        &self,
        vertex: NodeIndex,
    ) -> impl Iterator<Item = (NodeIndex, &CfgEdgeType)> {
        self.graph
            .edges_directed(vertex, Direction::Outgoing)
            .map(|edge| (edge.target(), edge.weight()))
    }

    pub fn incoming_edges(
        &self,
        vertex: NodeIndex,
    ) -> impl Iterator<Item = (NodeIndex, &CfgEdgeType)> {
        self.graph
            .edges_directed(vertex, Direction::Incoming)
            .map(|edge| (edge.source(), edge.weight()))
    }

    pub fn contains_vertex(&self, vertex: NodeIndex) -> bool {
        self.graph.node_weight(vertex).is_some()
    }

    pub fn remove_vertex(&mut self, vertex: NodeIndex) -> Option<CfgVertex> {
        if !self.vertex_exists(vertex) {
            return None;
        }

        let last_vertex = NodeIndex::new(self.graph.node_count() - 1);
        self.statement_origins.remove(&vertex);
        if vertex != last_vertex {
            if let Some(stmt_id) = self.statement_origins.remove(&last_vertex) {
                self.statement_origins.insert(vertex, stmt_id);
            }
        }

        self.graph.remove_node(vertex)
    }

    pub fn vertex_count(&self) -> usize {
        self.graph.node_count()
    }

    pub fn edge_count(&self) -> usize {
        self.graph.edge_count()
    }

    pub fn graph(&self) -> &DiGraph<CfgVertex, CfgEdgeType> {
        &self.graph
    }

    pub fn vertices(&self) -> impl Iterator<Item = (NodeIndex, &CfgVertex)> {
        self.graph.node_indices().map(|idx| (idx, self.graph.node_weight(idx).unwrap()))
    }

    pub fn in_degree(&self, vertex: NodeIndex) -> usize {
        self.graph.edges_directed(vertex, Direction::Incoming).count()
    }

    /// The vertices `entry` reaches over the edges `can_traverse` accepts, `entry` itself
    /// included. Reach is the GRAPH's own knowledge, so every analysis that needs it asks
    /// here instead of walking the edges again - one walk cannot forget to exclude the dead
    /// links ([`CfgEdgeType::is_executable`]), nor exclude them where it must not.
    pub fn reachable_vertices(
        &self,
        entry: NodeIndex,
        can_traverse: impl Fn(&CfgEdgeType) -> bool,
    ) -> FxHashSet<NodeIndex> {
        let mut reached = FxHashSet::default();
        let mut stack = vec![entry];
        while let Some(vertex) = stack.pop() {
            if !reached.insert(vertex) {
                continue;
            }
            stack.extend(
                self.outgoing_edges(vertex)
                    .filter(|(_, kind)| can_traverse(kind))
                    .map(|(target, _)| target),
            );
        }
        reached
    }

    pub fn reverse_postorder(&self) -> Vec<NodeIndex> {
        let Some(start) = self.start else {
            return vec![];
        };

        let mut postorder = Vec::with_capacity(self.vertex_count());
        let mut dfs = DfsPostOrder::new(&self.graph, start);

        while let Some(node) = dfs.next(&self.graph) {
            postorder.push(node);
        }

        postorder.reverse();
        postorder
    }

    pub fn postorder_from_exit(&self) -> Vec<NodeIndex> {
        let exit = self.exit;

        let reversed = Reversed(&self.graph);
        let mut postorder = Vec::with_capacity(self.vertex_count());
        let mut dfs = DfsPostOrder::new(reversed, exit);

        while let Some(node) = dfs.next(reversed) {
            postorder.push(node);
        }

        postorder
    }
}

impl Default for ControlFlowGraph {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vertex::BasicBlockVertex;
    use cfg_types::StmtId;
    use la_arena::RawIdx;

    #[test]
    fn reachable_vertices_skips_what_only_an_unexecutable_edge_leads_to() {
        let mut cfg = ControlFlowGraph::new();
        let entry = cfg.add_vertex(CfgVertex::BasicBlock(BasicBlockVertex::new()));
        let live = cfg.add_vertex(CfgVertex::BasicBlock(BasicBlockVertex::new()));
        let dead = cfg.add_vertex(CfgVertex::BasicBlock(BasicBlockVertex::new()));
        cfg.add_edge(entry, live, CfgEdgeType::Unconditional);
        cfg.add_edge(entry, dead, CfgEdgeType::Unexecutable);

        let all = cfg.reachable_vertices(entry, |_| true);
        let executable = cfg.reachable_vertices(entry, |kind| kind.is_executable());

        assert!(all.contains(&dead));
        assert!(!executable.contains(&dead));
        assert!(executable.contains(&live) && executable.contains(&entry));
    }

    #[test]
    fn reachable_vertices_of_a_sink_is_just_the_entry() {
        let mut cfg = ControlFlowGraph::new();
        let entry = cfg.add_vertex(CfgVertex::BasicBlock(BasicBlockVertex::new()));

        let reached = cfg.reachable_vertices(entry, |_| true);

        assert_eq!(reached.len(), 1);
        assert!(reached.contains(&entry));
    }

    #[test]
    fn reachable_vertices_terminates_on_a_cycle() {
        let mut cfg = ControlFlowGraph::new();
        let first = cfg.add_vertex(CfgVertex::BasicBlock(BasicBlockVertex::new()));
        let second = cfg.add_vertex(CfgVertex::BasicBlock(BasicBlockVertex::new()));
        cfg.add_edge(first, second, CfgEdgeType::Unconditional);
        cfg.add_edge(second, first, CfgEdgeType::Unconditional);

        let reached = cfg.reachable_vertices(first, |_| true);

        assert_eq!(reached.len(), 2);
    }

    #[test]
    fn test_graph_creation() {
        let cfg = ControlFlowGraph::new();
        assert_eq!(cfg.vertex_count(), 1);
        assert!(cfg.start().is_none());
        assert!(cfg.contains_vertex(cfg.exit()));
    }

    #[test]
    fn test_add_vertex() {
        let mut cfg = ControlFlowGraph::new();
        let block = CfgVertex::BasicBlock(BasicBlockVertex::new());
        let idx = cfg.add_vertex(block);

        assert_eq!(cfg.vertex_count(), 2);
        assert!(cfg.vertex(idx).is_some());
    }

    #[test]
    fn source_stmt_id_returns_only_the_explicit_vertex_origin() {
        let mut cfg = ControlFlowGraph::new();
        let stmt_id = StmtId::from_raw(RawIdx::from(7));
        let with_origin =
            cfg.add_vertex_with_origin(CfgVertex::BasicBlock(BasicBlockVertex::new()), stmt_id);
        let without_origin = cfg.add_vertex(CfgVertex::BasicBlock(BasicBlockVertex::new()));

        assert_eq!(cfg.source_stmt_id(with_origin), Some(stmt_id));
        assert_eq!(cfg.source_stmt_id(without_origin), None);
    }

    #[test]
    fn removing_a_vertex_keeps_the_moved_vertex_origin() {
        let mut cfg = ControlFlowGraph::new();
        let removed_stmt_id = StmtId::from_raw(RawIdx::from(7));
        let moved_stmt_id = StmtId::from_raw(RawIdx::from(8));
        let removed = cfg.add_vertex_with_origin(
            CfgVertex::BasicBlock(BasicBlockVertex::new()),
            removed_stmt_id,
        );
        let moved = cfg
            .add_vertex_with_origin(CfgVertex::BasicBlock(BasicBlockVertex::new()), moved_stmt_id);

        let _ = cfg.remove_vertex(removed);

        assert_eq!(cfg.source_stmt_id(removed), Some(moved_stmt_id));
        assert_eq!(cfg.source_stmt_id(moved), None);
    }

    #[test]
    fn test_add_edge() {
        let mut cfg = ControlFlowGraph::new();
        let block1 = cfg.add_vertex(CfgVertex::BasicBlock(BasicBlockVertex::new()));
        let block2 = cfg.add_vertex(CfgVertex::BasicBlock(BasicBlockVertex::new()));

        cfg.add_edge(block1, block2, CfgEdgeType::Unconditional);
        assert_eq!(cfg.edge_count(), 1);
    }

    #[test]
    fn test_set_start() {
        let mut cfg = ControlFlowGraph::new();
        let entry = cfg.add_vertex(CfgVertex::BasicBlock(BasicBlockVertex::new()));
        cfg.set_start(entry);

        assert_eq!(cfg.start(), Some(entry));
    }

    #[test]
    fn test_reverse_postorder() {
        let mut cfg = ControlFlowGraph::new();
        let b1 = cfg.add_vertex(CfgVertex::BasicBlock(BasicBlockVertex::new()));
        let b2 = cfg.add_vertex(CfgVertex::BasicBlock(BasicBlockVertex::new()));
        let b3 = cfg.add_vertex(CfgVertex::BasicBlock(BasicBlockVertex::new()));

        cfg.set_start(b1);
        cfg.add_edge(b1, b2, CfgEdgeType::Unconditional);
        cfg.add_edge(b2, b3, CfgEdgeType::Unconditional);
        cfg.add_edge(b3, cfg.exit(), CfgEdgeType::Unconditional);

        let rpo = cfg.reverse_postorder();

        assert!(rpo.len() >= 4);
        assert_eq!(rpo[0], b1);

        let b1_pos = rpo.iter().position(|&n| n == b1).unwrap();
        let b2_pos = rpo.iter().position(|&n| n == b2).unwrap();
        let b3_pos = rpo.iter().position(|&n| n == b3).unwrap();
        assert!(b1_pos < b2_pos);
        assert!(b2_pos < b3_pos);
    }

    #[test]
    fn test_postorder_from_exit() {
        let mut cfg = ControlFlowGraph::new();
        let b1 = cfg.add_vertex(CfgVertex::BasicBlock(BasicBlockVertex::new()));
        let b2 = cfg.add_vertex(CfgVertex::BasicBlock(BasicBlockVertex::new()));

        cfg.set_start(b1);
        cfg.add_edge(b1, b2, CfgEdgeType::Unconditional);
        cfg.add_edge(b2, cfg.exit(), CfgEdgeType::Unconditional);

        let postorder = cfg.postorder_from_exit();

        assert!(postorder.len() >= 3);
        assert!(postorder.contains(&cfg.exit()));
    }

    #[test]
    fn test_rpo_with_loop() {
        let mut cfg = ControlFlowGraph::new();
        let entry = cfg.add_vertex(CfgVertex::BasicBlock(BasicBlockVertex::new()));
        let loop_header = cfg.add_vertex(CfgVertex::BasicBlock(BasicBlockVertex::new()));
        let loop_body = cfg.add_vertex(CfgVertex::BasicBlock(BasicBlockVertex::new()));
        let after_loop = cfg.add_vertex(CfgVertex::BasicBlock(BasicBlockVertex::new()));

        cfg.set_start(entry);
        cfg.add_edge(entry, loop_header, CfgEdgeType::Unconditional);
        cfg.add_edge(loop_header, loop_body, CfgEdgeType::TrueBranch);
        cfg.add_edge(loop_header, after_loop, CfgEdgeType::FalseBranch);
        cfg.add_edge(loop_body, loop_header, CfgEdgeType::Unconditional);
        cfg.add_edge(after_loop, cfg.exit(), CfgEdgeType::Unconditional);

        let rpo = cfg.reverse_postorder();

        assert_eq!(rpo[0], entry);

        let header_pos = rpo.iter().position(|&n| n == loop_header).unwrap();
        let body_pos = rpo.iter().position(|&n| n == loop_body).unwrap();
        assert!(header_pos < body_pos);
    }
}
