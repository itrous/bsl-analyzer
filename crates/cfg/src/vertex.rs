use cfg_types::LocalRange;
use cfg_types::{BindingId, ExprId, StmtId};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CfgVertex {
    BasicBlock(BasicBlockVertex),

    /// Evaluates the condition of `Если` or of one `ИначеЕсли`.
    Conditional(ConditionalVertex),

    /// Entry of `Для` or `Для Каждого`: evaluates the bounds or the collection
    /// once, before the header is first reached. The loop's own back-edge and
    /// `Продолжить` return to the header, not here.
    LoopEntry(LoopEntryVertex),

    /// Loop headers, reached before every pass of the body: `Пока` evaluates
    /// its condition here, `Для` and `Для Каждого` bind the loop variable and
    /// test whether another pass follows.
    WhileHeader(WhileHeaderVertex),

    ForHeader(ForHeaderVertex),

    ForEachHeader(ForEachHeaderVertex),

    /// Entry of `Попытка`: the protected statements start here, and the handler
    /// is assumed reachable from here.
    Try,

    /// One `#Если` or `#ИначеЕсли` directive.
    PreprocCondition(PreprocConditionVertex),

    /// The single completion point of the method.
    Exit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BasicBlockVertex {
    statements: Vec<StmtId>,
}

impl BasicBlockVertex {
    pub fn new() -> Self {
        Self { statements: Vec::new() }
    }

    pub fn add_statement(&mut self, stmt: StmtId) {
        self.statements.push(stmt);
    }

    pub fn statements(&self) -> &[StmtId] {
        &self.statements
    }

    pub fn first_statement(&self) -> Option<StmtId> {
        self.statements.first().copied()
    }

    pub fn last_statement(&self) -> Option<StmtId> {
        self.statements.last().copied()
    }

    pub fn is_empty(&self) -> bool {
        self.statements.is_empty()
    }

    pub fn len(&self) -> usize {
        self.statements.len()
    }
}

impl Default for BasicBlockVertex {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConditionalVertex {
    pub condition: ExprId,
}

impl ConditionalVertex {
    pub fn new(condition: ExprId) -> Self {
        Self { condition }
    }
}

/// Ranges are those of the lowered body: relative to the method root, so the
/// graph of a method is the same value wherever the method sits in its file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreprocConditionVertex {
    pub condition_range: LocalRange,
    pub directive_range: Option<LocalRange>,
    pub full_range: Option<LocalRange>,
}

impl PreprocConditionVertex {
    pub fn with_directive_range(condition_range: LocalRange, directive_range: LocalRange) -> Self {
        Self { condition_range, directive_range: Some(directive_range), full_range: None }
    }

    pub fn with_ranges(
        condition_range: LocalRange,
        directive_range: LocalRange,
        full_range: LocalRange,
    ) -> Self {
        Self {
            condition_range,
            directive_range: Some(directive_range),
            full_range: Some(full_range),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WhileHeaderVertex {
    pub condition: ExprId,
}

impl WhileHeaderVertex {
    pub fn new(condition: ExprId) -> Self {
        Self { condition }
    }
}

/// The expressions a counted or collection loop evaluates once, on entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoopEntryVertex {
    /// `Для`: the lower bound, then the upper bound.
    For([ExprId; 2]),
    /// `Для Каждого`: the collection.
    ForEach(ExprId),
}

impl LoopEntryVertex {
    /// The roots in evaluation order.
    pub fn roots(&self) -> &[ExprId] {
        match self {
            LoopEntryVertex::For(bounds) => bounds,
            LoopEntryVertex::ForEach(collection) => std::slice::from_ref(collection),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForHeaderVertex {
    pub loop_var: BindingId,
}

impl ForHeaderVertex {
    pub fn new(loop_var: BindingId) -> Self {
        Self { loop_var }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForEachHeaderVertex {
    pub loop_var: BindingId,
}

impl ForEachHeaderVertex {
    pub fn new(loop_var: BindingId) -> Self {
        Self { loop_var }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_block_empty() {
        let block = BasicBlockVertex::new();
        assert!(block.is_empty());
        assert_eq!(block.len(), 0);
    }
}
