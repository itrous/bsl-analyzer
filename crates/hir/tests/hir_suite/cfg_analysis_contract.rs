use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::sync::Arc;

use cfg::{CfgBuilder, CfgEdgeType, CfgVertex, ControlFlowGraph, NodeIndex};
use dataflow::path_terminates::analyze_path_terminates_default;
use dataflow::reaching_defs::{
    DefinitionIndex, ReachingDefs, ReachingDefsResult, ReachingDefsTransfer,
};
use dataflow::{DataflowSolver, Transfer, DEFAULT_MAX_ITERATIONS};
use hir_def::body::lower_method;
use hir_def::StmtId;
use syntax::SyntaxKind;

fn points(graph: &ControlFlowGraph, node: NodeIndex) -> Vec<StmtId> {
    match graph.vertex(node).unwrap() {
        CfgVertex::BasicBlock(block) => block.statements().to_vec(),
        _ => graph.source_stmt_id(node).into_iter().collect(),
    }
}

fn reachable(graph: &ControlFlowGraph, executable_only: bool) -> BTreeSet<u32> {
    let mut visited = BTreeSet::new();
    let mut stack: Vec<_> = graph.start().into_iter().collect();
    let mut result = BTreeSet::new();
    while let Some(node) = stack.pop() {
        if !visited.insert(node) {
            continue;
        }
        result.extend(points(graph, node).iter().map(|id| id.into_raw().into_u32()));
        stack.extend(graph.outgoing_edges(node).filter_map(|(target, kind)| {
            (!executable_only || kind.is_executable()).then_some(target)
        }));
    }
    result
}

// Empty routing blocks are not source operations. Collapse only those blocks,
// preserving cycles through source operations and each condition's polarity.
fn next_points(graph: &ControlFlowGraph, start: NodeIndex) -> BTreeSet<String> {
    let mut result = BTreeSet::new();
    let mut visited = BTreeSet::new();
    let mut stack = vec![start];
    while let Some(node) = stack.pop() {
        if !visited.insert(node) {
            continue;
        }
        if node == graph.exit() {
            result.insert("exit".into());
        } else if let Some(stmt) = points(graph, node).first() {
            result.insert(format!("s{}", stmt.into_raw().into_u32()));
        } else {
            stack.extend(
                graph
                    .outgoing_edges(node)
                    .filter_map(|(target, kind)| kind.is_executable().then_some(target)),
            );
        }
    }
    result
}

fn observation(source: &str) -> String {
    let parse = parser::parse(source);
    assert!(parse.errors().is_empty(), "{:?}", parse.errors());
    let method = parse
        .syntax_node()
        .descendants()
        .find(|node| matches!(node.kind(), SyntaxKind::PROCEDURE_DEF | SyntaxKind::FUNCTION_DEF))
        .unwrap();
    let lowered = lower_method(&method, method.kind() == SyntaxKind::FUNCTION_DEF);
    let body = lowered.body.as_ref();
    let graph = Arc::new(CfgBuilder::new().build_graph_from_hir(body.body_stmts_typed(), body));
    let live = reachable(&graph, true);
    let all = reachable(&graph, false);
    let mut out = String::new();
    writeln!(out, "complexity: {}", cfg::cyclomatic_complexity(&graph)).unwrap();
    writeln!(out, "live: {:?}", live).unwrap();
    writeln!(out, "dead: {:?}", all.difference(&live).collect::<Vec<_>>()).unwrap();

    let mut transitions = BTreeSet::new();
    for (node, vertex) in graph.vertices() {
        let stmts = points(&graph, node);
        for pair in stmts.windows(2) {
            transitions.insert(format!(
                "s{} -> s{}",
                pair[0].into_raw().into_u32(),
                pair[1].into_raw().into_u32()
            ));
        }
        let Some(last) = stmts.last() else { continue };
        for (target, kind) in graph.outgoing_edges(node) {
            if !kind.is_executable() {
                continue;
            }
            let polarity = if matches!(vertex, CfgVertex::Try) {
                ""
            } else {
                match kind {
                    CfgEdgeType::TrueBranch => " T",
                    CfgEdgeType::FalseBranch => " F",
                    _ => "",
                }
            };
            for successor in next_points(&graph, target) {
                transitions
                    .insert(format!("s{}{polarity} -> {successor}", last.into_raw().into_u32()));
            }
        }
    }
    let mut evaluations = BTreeMap::new();
    for (node, vertex) in graph.vertices() {
        let payload = match vertex {
            CfgVertex::Conditional(v) => format!("condition={:?}", v.condition),
            CfgVertex::WhileHeader(v) => format!("condition={:?}", v.condition),
            CfgVertex::ForHeader(v) => {
                format!("from={:?}; to={:?}; binding={:?}", v.from, v.to, v.loop_var)
            }
            CfgVertex::ForEachHeader(v) => {
                format!("collection={:?}; binding={:?}", v.collection, v.loop_var)
            }
            CfgVertex::PreprocCondition(v) => format!(
                "condition={:?}; directive={:?}; full={:?}",
                v.condition_range, v.directive_range, v.full_range
            ),
            _ => continue,
        };
        let origin = match vertex {
            CfgVertex::PreprocCondition(v) => format!("directive:{:?}", v.directive_range),
            _ => format!(
                "s{}",
                graph
                    .source_stmt_id(node)
                    .expect("runtime evaluations have a source statement")
                    .into_raw()
                    .into_u32()
            ),
        };
        evaluations.entry(origin).or_insert_with(BTreeSet::new).insert(payload);
    }
    writeln!(out, "evaluations: {evaluations:?}").unwrap();
    for (id, _) in body.exprs_iter() {
        writeln!(out, "expr {id:?}: {:?}", lowered.source_map.expr_range(id)).unwrap();
    }
    for (id, _) in body.bindings_iter() {
        writeln!(out, "binding {id:?}: {:?}", lowered.source_map.binding_range(id)).unwrap();
    }
    writeln!(out, "transitions:").unwrap();
    for transition in transitions {
        writeln!(out, "  {transition}").unwrap();
    }

    let index = DefinitionIndex::from_body(body);
    for limit in [3, DEFAULT_MAX_ITERATIONS] {
        let mut solver = DataflowSolver::new(graph.clone(), body.clone(), ReachingDefsTransfer);
        solver.set_max_iterations(limit);
        solver.set_bottom_factory(|| ReachingDefs::new(index.clone()));
        solver.set_initial_state(ReachingDefs::new(index.clone()));
        let result = ReachingDefsResult::new(solver.solve().unwrap(), Arc::new(body.clone()));
        writeln!(out, "definitions (limit {limit}):").unwrap();
        for (id, stmt) in body.stmts_iter() {
            // Labels route control but perform no transfer. Their standalone
            // vertices do not participate in the per-statement definitions API.
            if matches!(stmt, hir_def::Stmt::Label(_)) {
                continue;
            }
            let defs = result.defs_up_to_stmt(id).map(|state| {
                let mut defs = state.iter().map(|def| format!("{def:?}")).collect::<Vec<_>>();
                defs.sort();
                defs
            });
            writeln!(out, "  s{}: {defs:?}", id.into_raw().into_u32()).unwrap();
        }
    }

    let paths = analyze_path_terminates_default(body, &graph).unwrap();
    let values = dataflow::value_state::analyze(graph.clone(), body.clone()).unwrap();
    let security = dataflow::security_state::analyze(graph.clone(), body.clone()).unwrap();
    let mut states = BTreeMap::new();
    for (node, _) in graph.vertices() {
        let mut value = values.block_in(node).unwrap().clone();
        let mut security = security.block_in(node).unwrap().clone();
        for stmt in points(&graph, node) {
            states.insert(
                stmt.into_raw().into_u32(),
                format!(
                    "range={:?}; fallthrough={}; value=({},{:?}); security={:?}",
                    lowered.source_map.stmt_range(stmt),
                    paths.may_fallthrough_at_block(node),
                    value.is_reachable(),
                    value.get("Flag"),
                    security.counters()
                ),
            );
            dataflow::value_state::step(&mut value, body, stmt);
            security = dataflow::security_state::SecurityStateProvider.transfer_stmt(
                stmt.into_raw(),
                &security,
                body,
            );
        }
    }
    writeln!(out, "source states:").unwrap();
    for (stmt, state) in states {
        writeln!(out, "  s{stmt}: {state}").unwrap();
    }
    writeln!(out, "exit security: {:?}", security.block_out(graph.exit()).unwrap().counters())
        .unwrap();
    out
}

macro_rules! contract {
    ($name:ident) => {
        #[test]
        fn $name() {
            let actual = observation(include_str!(concat!(
                "../fixtures/cfg_contract/",
                stringify!($name),
                ".bsl"
            )));
            expect_test::expect_file![concat!(
                "../fixtures/cfg_contract/",
                stringify!($name),
                ".txt"
            )]
            .assert_eq(&actual);
        }
    };
}

contract!(empty);
contract!(linear);
contract!(return_dead_branch);
contract!(live_branch);
contract!(while_jumps);
contract!(counted_and_collection_loops);
contract!(nested_handlers);
contract!(unhandled_raise);
contract!(goto_labels);
contract!(unresolved_goto);
contract!(preproc_alternatives);
contract!(dead_preproc);
contract!(security_balanced);
contract!(security_open);
contract!(limited_labels);

#[test]
fn observations_distinguish_security_open_and_closed() {
    let open = observation(include_str!("../fixtures/cfg_contract/security_open.bsl"));
    let closed = observation(include_str!("../fixtures/cfg_contract/security_balanced.bsl"));
    assert!(open
        .lines()
        .last()
        .unwrap()
        .contains("privilege: PrivilegeCounter { may: Exact(1), must: Exact(1) }"));
    assert!(closed
        .lines()
        .last()
        .unwrap()
        .contains("privilege: PrivilegeCounter { may: Exact(0), must: Exact(0) }"));
    assert_ne!(open, closed);
}

#[test]
fn observations_distinguish_live_dead_and_branching_inputs() {
    let linear = observation(include_str!("../fixtures/cfg_contract/linear.bsl"));
    let dead = observation(include_str!("../fixtures/cfg_contract/return_dead_branch.bsl"));
    let branch = observation(include_str!("../fixtures/cfg_contract/live_branch.bsl"));
    assert!(linear.starts_with("complexity: 1\n"));
    assert!(linear.contains("dead: []\n"));
    assert!(dead.starts_with("complexity: 1\n"));
    assert!(!dead.contains("dead: []\n"));
    assert!(branch.starts_with("complexity: 2\n"));
    assert!(branch.contains(" T -> ") && branch.contains(" F -> "));
    assert_ne!(linear, dead);
    assert_ne!(linear, branch);
}
