fn identifiers(source: &str) -> impl Iterator<Item = &str> {
    source.split(|c: char| !c.is_alphanumeric() && c != '_').filter(|word| !word.is_empty())
}

#[test]
fn removed_cfg_surface_is_absent_from_definitions_and_consumers() {
    let sources = [
        include_str!("../../../cfg/src/builder.rs"),
        include_str!("../../../cfg/src/edge.rs"),
        include_str!("../../../cfg/src/graph.rs"),
        include_str!("../../../cfg/src/vertex.rs"),
        include_str!("../../../cfg/src/lib.rs"),
        include_str!("../../../cfg/src/cyclomatic.rs"),
        include_str!("../../../cfg/src/test_utils.rs"),
        include_str!("../../../dataflow/src/lib.rs"),
        include_str!("../../../dataflow/src/guard_predicates.rs"),
        include_str!("../../../dataflow/src/path_terminates.rs"),
        include_str!("../../../dataflow/src/security_state.rs"),
        include_str!("../../../dataflow/src/temp_resource.rs"),
        include_str!("../../../hir-ty/src/narrow.rs"),
        include_str!("../../src/lib.rs"),
        include_str!("../../../ide-diagnostics/src/handlers/unreachable_code.rs"),
        include_str!("../../../ide-diagnostics/src/handlers/pairing_broken_transaction.rs"),
        include_str!("../../../ide-diagnostics/src/handlers/all_function_path_must_have_return.rs"),
    ];
    let removed = [
        "produce_loop_iterations",
        "is_branching",
        "is_loop",
        "edge_presentation",
        "AdjacentCode",
        "Direct",
        "LoopIteration",
        "LoopBreak",
        "LoopContinue",
        "WhileLoopVertex",
        "ForLoopVertex",
        "ForEachLoopVertex",
        "TryExceptVertex",
        "LabelVertex",
        "is_conditional_branch",
        "is_loop_back_edge",
        "is_user_loop_jump",
        "is_dead_code_edge",
        "entry_point",
        "set_entry_point",
        "exit_point",
    ];
    assert!(identifiers(sources[1]).any(|word| word == "CfgEdgeType"));
    assert!(identifiers(sources[2]).any(|word| word == "ControlFlowGraph"));
    for (index, source) in sources.iter().enumerate() {
        assert!(!source.is_empty());
        for word in identifiers(source) {
            assert!(!removed.contains(&word), "removed CFG identifier {word} in source {index}");
        }
    }
}

#[test]
fn removed_edge_default_and_unused_vertex_helpers_are_not_restored() {
    let edge = include_str!("../../../cfg/src/edge.rs");
    assert!(!identifiers(edge).any(|word| word == "Default"));
    let vertex = include_str!("../../../cfg/src/vertex.rs");
    for removed in ["first_stmt_id", "type_name"] {
        assert!(!identifiers(vertex).any(|word| word == removed));
    }
}
