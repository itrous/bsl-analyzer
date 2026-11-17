use crate::{CfgEdgeType, CfgVertex, ControlFlowGraph};
use hir_def::{Body, Stmt};
use petgraph::algo::dominators::simple_fast;
use petgraph::graph::NodeIndex;
use petgraph::visit::EdgeRef;
use rustc_hash::FxHashMap;

pub fn format_cfg(cfg: &ControlFlowGraph, body: &Body) -> String {
    let depths = dominator_depths(cfg);
    let mut base_fingerprints = FxHashMap::default();
    let mut base_counts = FxHashMap::default();

    for (idx, vertex) in cfg.vertices() {
        let base = base_fingerprint(cfg, body, idx, vertex, *depths.get(&idx).unwrap_or(&0));
        *base_counts.entry(base.clone()).or_insert(0usize) += 1;
        base_fingerprints.insert(idx, base);
    }

    let mut fingerprints = FxHashMap::default();
    for (idx, _) in cfg.vertices() {
        let base =
            base_fingerprints.get(&idx).expect("every CFG vertex must have a base fingerprint");
        if base_counts.get(base).copied().unwrap_or(0) > 1 {
            fingerprints
                .insert(idx, format!("{base}:#{}", predecessor_hash(cfg, idx, &base_fingerprints)));
        } else {
            fingerprints.insert(idx, base.clone());
        }
    }

    let mut blocks: Vec<_> = fingerprints.values().cloned().collect();
    blocks.sort();

    let mut edges: Vec<_> = cfg
        .graph()
        .edge_references()
        .map(|edge| {
            let from = fingerprints
                .get(&edge.source())
                .expect("edge source must have a fingerprint")
                .clone();
            let to = fingerprints
                .get(&edge.target())
                .expect("edge target must have a fingerprint")
                .clone();
            (from, to, edge_kind_name(*edge.weight()).to_owned())
        })
        .collect();
    edges.sort();

    let mut out = String::new();
    out.push_str("blocks:\n");
    for block in blocks {
        out.push_str("  ");
        out.push_str(&block);
        out.push('\n');
    }
    out.push_str("edges:\n");
    for (from, to, kind) in edges {
        out.push_str("  ");
        out.push_str(&from);
        out.push_str(" -> ");
        out.push_str(&to);
        out.push_str(" [");
        out.push_str(&kind);
        out.push_str("]\n");
    }
    out
}

fn dominator_depths(cfg: &ControlFlowGraph) -> FxHashMap<NodeIndex, usize> {
    let mut depths = FxHashMap::default();
    let Some(start) = cfg.start() else {
        return depths;
    };
    if !cfg.contains_vertex(start) {
        return depths;
    }

    let dominators = simple_fast(cfg.graph(), start);
    for (idx, _) in cfg.vertices() {
        let depth = dominators.strict_dominators(idx).map_or(0, Iterator::count);
        depths.insert(idx, depth);
    }
    depths
}

fn base_fingerprint(
    cfg: &ControlFlowGraph,
    body: &Body,
    idx: NodeIndex,
    vertex: &CfgVertex,
    dom_depth: usize,
) -> String {
    format!("{}:{}:{dom_depth}", role(cfg, idx), first_stmt_kind(body, vertex))
}

fn role(cfg: &ControlFlowGraph, idx: NodeIndex) -> &'static str {
    if cfg.start() == Some(idx) {
        "ENTRY"
    } else if cfg.exit() == idx {
        "EXIT"
    } else {
        "NORMAL"
    }
}

/// A block is named by the jump that ends it, else by a label that heads it.
fn first_stmt_kind(body: &Body, vertex: &CfgVertex) -> &'static str {
    match vertex {
        CfgVertex::BasicBlock(block) => {
            let (Some(first), Some(last)) = (block.first_statement(), block.last_statement())
            else {
                return "EMPTY";
            };
            match body.stmt(last) {
                Stmt::Break => "BREAK_STMT",
                Stmt::Continue => "CONTINUE_STMT",
                Stmt::Goto(_) => "GOTO_STMT",
                Stmt::Return { .. } => "RETURN_STMT",
                Stmt::Raise { .. } => "RAISE_STMT",
                _ if matches!(body.stmt(first), Stmt::Label(_)) => "LABEL_STMT",
                _ => "CALL_STMT",
            }
        }
        CfgVertex::Conditional(_) => "IF_STMT",
        CfgVertex::LoopEntry(_) => "LOOP_ENTRY",
        CfgVertex::WhileHeader(_) => "WHILE_STMT",
        CfgVertex::ForHeader(_) => "FOR_STMT",
        CfgVertex::ForEachHeader(_) => "FOR_EACH_STMT",
        CfgVertex::Try => "TRY_STMT",
        CfgVertex::PreprocCondition(_) => "PRE_IF_DIR",
        CfgVertex::Exit => "EMPTY",
    }
}

fn predecessor_hash(
    cfg: &ControlFlowGraph,
    idx: NodeIndex,
    base_fingerprints: &FxHashMap<NodeIndex, String>,
) -> String {
    let mut parts: Vec<_> = cfg
        .incoming_edges(idx)
        .map(|(pred, edge)| {
            let pred_fingerprint =
                base_fingerprints.get(&pred).map(String::as_str).unwrap_or("UNKNOWN");
            format!("{pred_fingerprint}:{}", edge_kind_name(*edge))
        })
        .collect();
    parts.sort();

    let mut hash = 0xcbf29ce484222325u64;
    for part in parts {
        for byte in part.bytes().chain([0]) {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
    }
    format!("{:08x}", hash as u32)
}

fn edge_kind_name(kind: CfgEdgeType) -> &'static str {
    match kind {
        CfgEdgeType::Unconditional => "Unconditional",
        CfgEdgeType::TrueBranch => "TrueBranch",
        CfgEdgeType::FalseBranch => "FalseBranch",
        CfgEdgeType::Exception => "Exception",
        CfgEdgeType::Unexecutable => "Unexecutable",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BasicBlockVertex;

    #[test]
    fn format_cfg_stable_across_block_renumber() {
        fn graph_a() -> ControlFlowGraph {
            let mut cfg = ControlFlowGraph::new();
            let entry = cfg.add_vertex(CfgVertex::BasicBlock(BasicBlockVertex::new()));
            let left = cfg.add_vertex(CfgVertex::BasicBlock(BasicBlockVertex::new()));
            let right = cfg.add_vertex(CfgVertex::BasicBlock(BasicBlockVertex::new()));
            let merge = cfg.add_vertex(CfgVertex::BasicBlock(BasicBlockVertex::new()));
            cfg.set_start(entry);
            cfg.add_edge(entry, left, CfgEdgeType::TrueBranch);
            cfg.add_edge(entry, right, CfgEdgeType::FalseBranch);
            cfg.add_edge(left, merge, CfgEdgeType::Unconditional);
            cfg.add_edge(right, merge, CfgEdgeType::Unconditional);
            cfg.add_edge(merge, cfg.exit(), CfgEdgeType::Unconditional);
            cfg
        }

        fn graph_b() -> ControlFlowGraph {
            let mut cfg = ControlFlowGraph::new();
            let merge = cfg.add_vertex(CfgVertex::BasicBlock(BasicBlockVertex::new()));
            let right = cfg.add_vertex(CfgVertex::BasicBlock(BasicBlockVertex::new()));
            let left = cfg.add_vertex(CfgVertex::BasicBlock(BasicBlockVertex::new()));
            let entry = cfg.add_vertex(CfgVertex::BasicBlock(BasicBlockVertex::new()));
            cfg.set_start(entry);
            cfg.add_edge(entry, right, CfgEdgeType::FalseBranch);
            cfg.add_edge(entry, left, CfgEdgeType::TrueBranch);
            cfg.add_edge(right, merge, CfgEdgeType::Unconditional);
            cfg.add_edge(left, merge, CfgEdgeType::Unconditional);
            cfg.add_edge(merge, cfg.exit(), CfgEdgeType::Unconditional);
            cfg
        }

        let body = Body::default();
        assert_eq!(format_cfg(&graph_a(), &body), format_cfg(&graph_b(), &body));
    }
}
