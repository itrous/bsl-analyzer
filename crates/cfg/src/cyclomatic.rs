use crate::graph::ControlFlowGraph;
use petgraph::visit::EdgeRef;

/// Counts the decisions of the text: an edge saying only that a protected
/// point may fail adds no path of its own, and a loop's entry adds a vertex
/// and its single edge, which cancel.
pub fn cyclomatic_complexity(cfg: &ControlFlowGraph) -> u32 {
    let Some(start) = cfg.start() else {
        return 1;
    };

    let graph = cfg.graph();
    let mut reachable: rustc_hash::FxHashSet<_> = rustc_hash::FxHashSet::default();
    reachable.insert(start);
    let mut stack = vec![start];
    while let Some(node) = stack.pop() {
        for edge in graph.edges(node) {
            if !edge.weight().is_executable() {
                continue;
            }
            let target = edge.target();
            if reachable.insert(target) {
                stack.push(target);
            }
        }
    }
    let node_count = reachable.len() as i64;

    let mut edge_count: i64 = 0;
    for &node in &reachable {
        for edge in graph.edges(node) {
            if !edge.weight().is_executable() {
                continue;
            }
            if !reachable.contains(&edge.target()) {
                continue;
            }
            if cfg.is_possible_failure(node, *edge.weight()) {
                continue;
            }
            edge_count += 1;
        }
    }

    let v = edge_count - node_count + 2;
    v.max(1) as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edge::CfgEdgeType;
    use crate::vertex::{BasicBlockVertex, CfgVertex, ConditionalVertex};
    use cfg_types::ExprId;
    use la_arena::RawIdx;

    fn fresh_block() -> CfgVertex {
        CfgVertex::BasicBlock(BasicBlockVertex::new())
    }

    fn fresh_conditional() -> CfgVertex {
        CfgVertex::Conditional(ConditionalVertex::new(ExprId::from_raw(RawIdx::from(0u32))))
    }

    fn build_cfg() -> ControlFlowGraph {
        ControlFlowGraph::new()
    }

    #[test]
    fn empty_cfg_returns_one() {
        let cfg = ControlFlowGraph::new();
        assert_eq!(cyclomatic_complexity(&cfg), 1);
    }

    #[test]
    fn straight_line_returns_one() {
        let mut cfg = build_cfg();
        let entry = cfg.add_vertex(fresh_block());
        let mid = cfg.add_vertex(fresh_block());
        let exit = cfg.exit();
        cfg.add_edge(entry, mid, CfgEdgeType::Unconditional);
        cfg.add_edge(mid, exit, CfgEdgeType::Unconditional);
        cfg.set_start(entry);
        assert_eq!(cyclomatic_complexity(&cfg), 1);
    }

    #[test]
    fn single_if_else_returns_two() {
        let mut cfg = build_cfg();
        let entry = cfg.add_vertex(fresh_block());
        let cond = cfg.add_vertex(fresh_conditional());
        let then_block = cfg.add_vertex(fresh_block());
        let else_block = cfg.add_vertex(fresh_block());
        let exit = cfg.exit();
        cfg.add_edge(entry, cond, CfgEdgeType::Unconditional);
        cfg.add_edge(cond, then_block, CfgEdgeType::TrueBranch);
        cfg.add_edge(cond, else_block, CfgEdgeType::FalseBranch);
        cfg.add_edge(then_block, exit, CfgEdgeType::Unconditional);
        cfg.add_edge(else_block, exit, CfgEdgeType::Unconditional);
        cfg.set_start(entry);
        assert_eq!(cyclomatic_complexity(&cfg), 2);
    }

    #[test]
    fn unexecutable_edge_is_excluded() {
        let mut cfg = build_cfg();
        let entry = cfg.add_vertex(fresh_block());
        let mid = cfg.add_vertex(fresh_block());
        let exit = cfg.exit();
        let unreachable = cfg.add_vertex(fresh_block());
        cfg.add_edge(entry, mid, CfgEdgeType::Unconditional);
        cfg.add_edge(mid, exit, CfgEdgeType::Unconditional);
        cfg.add_edge(mid, unreachable, CfgEdgeType::Unexecutable);
        cfg.set_start(entry);
        assert_eq!(cyclomatic_complexity(&cfg), 1);
    }

    #[test]
    fn while_loop_returns_two() {
        let mut cfg = build_cfg();
        let entry = cfg.add_vertex(fresh_block());
        let cond = cfg.add_vertex(fresh_conditional());
        let body = cfg.add_vertex(fresh_block());
        let exit = cfg.exit();
        cfg.add_edge(entry, cond, CfgEdgeType::Unconditional);
        cfg.add_edge(cond, body, CfgEdgeType::TrueBranch);
        cfg.add_edge(body, cond, CfgEdgeType::Unconditional);
        cfg.add_edge(cond, exit, CfgEdgeType::FalseBranch);
        cfg.set_start(entry);
        assert_eq!(cyclomatic_complexity(&cfg), 2);
    }

    #[test]
    fn unexecutable_edge_reachability_excludes_orphan() {
        let mut cfg = build_cfg();
        let entry = cfg.add_vertex(fresh_block());
        let live = cfg.add_vertex(fresh_block());
        let orphan = cfg.add_vertex(fresh_block());
        let exit = cfg.exit();
        cfg.add_edge(entry, live, CfgEdgeType::Unconditional);
        cfg.add_edge(live, exit, CfgEdgeType::Unconditional);
        cfg.add_edge(live, orphan, CfgEdgeType::Unexecutable);
        cfg.add_edge(orphan, exit, CfgEdgeType::Unconditional);
        cfg.set_start(entry);
        assert_eq!(cyclomatic_complexity(&cfg), 1);
    }

    #[test]
    fn nested_if_in_while_returns_three() {
        let mut cfg = build_cfg();
        let entry = cfg.add_vertex(fresh_block());
        let cond_w = cfg.add_vertex(fresh_conditional());
        let cond_i = cfg.add_vertex(fresh_conditional());
        let body = cfg.add_vertex(fresh_block());
        let skip = cfg.add_vertex(fresh_block());
        let exit = cfg.exit();
        cfg.add_edge(entry, cond_w, CfgEdgeType::Unconditional);
        cfg.add_edge(cond_w, cond_i, CfgEdgeType::TrueBranch);
        cfg.add_edge(cond_i, body, CfgEdgeType::TrueBranch);
        cfg.add_edge(cond_i, skip, CfgEdgeType::FalseBranch);
        cfg.add_edge(body, cond_w, CfgEdgeType::Unconditional);
        cfg.add_edge(skip, cond_w, CfgEdgeType::Unconditional);
        cfg.add_edge(cond_w, exit, CfgEdgeType::FalseBranch);
        cfg.set_start(entry);
        assert_eq!(cyclomatic_complexity(&cfg), 3);
    }
}
