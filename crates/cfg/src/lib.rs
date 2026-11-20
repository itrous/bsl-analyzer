pub mod builder;
pub mod cyclomatic;
pub mod edge;
pub mod graph;
pub mod test_utils;
pub mod vertex;

pub use builder::CfgBuilder;
pub use cyclomatic::cyclomatic_complexity;
pub use edge::CfgEdgeType;
pub use graph::ControlFlowGraph;
pub use vertex::{
    BasicBlockVertex, CfgVertex, ConditionalVertex, ForEachHeaderVertex, ForHeaderVertex,
    LoopEntryVertex, PreprocConditionVertex, WhileHeaderVertex,
};

pub use petgraph::graph::NodeIndex;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_version() {
        assert!(VERSION.contains('.'));
    }
}
