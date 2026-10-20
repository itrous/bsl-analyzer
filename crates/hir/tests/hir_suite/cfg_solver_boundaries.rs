use std::sync::Arc;

use cfg::{BasicBlockVertex, CfgEdgeType, CfgVertex, ControlFlowGraph};
use dataflow::path_terminates::{MayFallthrough, PathTerminatesConfig, PathTerminatesTransfer};
use dataflow::{DataflowSolver, Direction};
use hir_def::Body;

#[test]
fn forward_and_backward_seeds_reach_opposite_boundaries() {
    let mut graph = ControlFlowGraph::new();
    let start = graph.add_vertex(CfgVertex::BasicBlock(BasicBlockVertex::new()));
    let middle = graph.add_vertex(CfgVertex::BasicBlock(BasicBlockVertex::new()));
    let exit = graph.exit();
    graph.set_start(start);
    graph.add_edge(start, middle, CfgEdgeType::Unconditional);
    graph.add_edge(middle, exit, CfgEdgeType::Unconditional);
    let graph = Arc::new(graph);

    let mut forward = DataflowSolver::new(
        graph.clone(),
        Body::default(),
        PathTerminatesTransfer::new(PathTerminatesConfig::default()),
    );
    forward.set_bottom_factory(|| MayFallthrough::BOTTOM);
    forward.set_initial_state(MayFallthrough::TOP);
    let forward = forward.solve().unwrap();
    assert_eq!(forward.block_in(start), Some(&MayFallthrough::TOP));
    assert_eq!(forward.block_out(exit), Some(&MayFallthrough::TOP));

    let mut backward = DataflowSolver::new(
        graph,
        Body::default(),
        PathTerminatesTransfer::new(PathTerminatesConfig::default()),
    );
    backward.set_direction(Direction::Backward);
    backward.set_bottom_factory(|| MayFallthrough::BOTTOM);
    backward.set_initial_state_at_exit(MayFallthrough::TOP);
    let backward = backward.solve().unwrap();
    assert_eq!(backward.block_out(exit), Some(&MayFallthrough::TOP));
    assert_eq!(backward.block_in(start), Some(&MayFallthrough::TOP));
}

#[test]
fn graph_without_a_body_has_no_forward_seed_but_keeps_its_backward_exit() {
    let graph = Arc::new(ControlFlowGraph::new());
    assert!(graph.start().is_none());
    let exit = graph.exit();
    let mut forward = DataflowSolver::new(
        graph.clone(),
        Body::default(),
        PathTerminatesTransfer::new(PathTerminatesConfig::default()),
    );
    forward.set_bottom_factory(|| MayFallthrough::BOTTOM);
    forward.set_initial_state(MayFallthrough::TOP);
    assert_eq!(forward.solve().unwrap().block_out(exit), Some(&MayFallthrough::BOTTOM));

    let mut backward = DataflowSolver::new(
        graph,
        Body::default(),
        PathTerminatesTransfer::new(PathTerminatesConfig::default()),
    );
    backward.set_direction(Direction::Backward);
    backward.set_bottom_factory(|| MayFallthrough::BOTTOM);
    backward.set_initial_state_at_exit(MayFallthrough::TOP);
    assert_eq!(backward.solve().unwrap().block_in(exit), Some(&MayFallthrough::TOP));
}
