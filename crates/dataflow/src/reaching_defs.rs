use cfg_types::IdConversion;
use fixedbitset::FixedBitSet;
use hir_def::{
    body::Body,
    hir::{Expr, Stmt},
    BindingId, Name,
};
use la_arena::RawIdx;
use rustc_hash::FxHashMap;
use smallvec::SmallVec;
use smol_str::SmolStr;
use std::sync::Arc;
use stdx::case::CaseExt;

use crate::{Lattice, Transfer};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DefSite {
    Parameter(BindingId),

    VarDecl(BindingId),

    Assignment(RawIdx),

    ForLoop(BindingId),

    ForEachLoop(BindingId),

    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Definition {
    pub var_name: SmolStr,

    pub def_site: DefSite,
}

impl Definition {
    pub fn new(var_name: SmolStr, def_site: DefSite) -> Self {
        let var_name = SmolStr::new(var_name.fold_lower());
        Self { var_name, def_site }
    }

    pub fn parameter(name: &Name, binding_id: BindingId) -> Self {
        Self::new(SmolStr::new(name.as_str()), DefSite::Parameter(binding_id))
    }

    pub fn var_decl(name: &Name, binding_id: BindingId) -> Self {
        Self::new(SmolStr::new(name.as_str()), DefSite::VarDecl(binding_id))
    }

    pub fn assignment(var_name: SmolStr, stmt_id: RawIdx) -> Self {
        Self::new(var_name, DefSite::Assignment(stmt_id))
    }

    pub fn for_loop(name: &Name, binding_id: BindingId) -> Self {
        Self::new(SmolStr::new(name.as_str()), DefSite::ForLoop(binding_id))
    }

    pub fn for_each_loop(name: &Name, binding_id: BindingId) -> Self {
        Self::new(SmolStr::new(name.as_str()), DefSite::ForEachLoop(binding_id))
    }

    pub fn unknown(var_name: SmolStr) -> Self {
        Self::new(var_name, DefSite::Unknown)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefinitionIndex {
    definitions: Vec<Definition>,

    var_to_defs: FxHashMap<SmolStr, SmallVec<[u32; 4]>>,

    def_to_idx: FxHashMap<Definition, u32>,
}

impl DefinitionIndex {
    pub fn from_body(body: &Body) -> Arc<Self> {
        let mut definitions = Vec::new();
        let mut var_to_defs: FxHashMap<SmolStr, SmallVec<[u32; 4]>> = FxHashMap::default();
        let mut def_to_idx: FxHashMap<Definition, u32> = FxHashMap::default();

        let mut add_def = |def: Definition| {
            if def_to_idx.contains_key(&def) {
                return;
            }
            let idx = definitions.len() as u32;
            var_to_defs.entry(def.var_name.clone()).or_default().push(idx);
            def_to_idx.insert(def.clone(), idx);
            definitions.push(def);
        };

        fn collect_stmt_defs<F: FnMut(Definition)>(
            stmts: &[hir_def::hir::StmtIdx],
            body: &Body,
            add_def: &mut F,
        ) {
            for &stmt_id in stmts {
                let opaque_id = hir_def::StmtId::from_idx(stmt_id);
                match body.stmt(opaque_id) {
                    Stmt::Assign { target, .. } => {
                        if let Some(var_name) =
                            extract_var_name_from_expr(hir_def::ExprId::from_idx(*target), body)
                        {
                            let def = Definition::assignment(var_name, stmt_id.into_raw());
                            add_def(def);
                        }
                    }
                    Stmt::VarDecl { bindings } => {
                        for &binding_id in bindings.iter() {
                            let binding = body.binding_idx(binding_id);
                            let def = Definition::var_decl(
                                &binding.name,
                                BindingId::from_idx(binding_id),
                            );
                            add_def(def);
                        }
                    }
                    Stmt::For { var, body: loop_body, .. } => {
                        let binding = body.binding_idx(*var);
                        let def = Definition::for_loop(&binding.name, BindingId::from_idx(*var));
                        add_def(def);
                        collect_stmt_defs(loop_body, body, add_def);
                    }
                    Stmt::ForEach { var, body: loop_body, .. } => {
                        let binding = body.binding_idx(*var);
                        let def =
                            Definition::for_each_loop(&binding.name, BindingId::from_idx(*var));
                        add_def(def);
                        collect_stmt_defs(loop_body, body, add_def);
                    }
                    Stmt::If(if_stmt) => {
                        collect_stmt_defs(&if_stmt.then_branch, body, add_def);
                        for (_cond, stmts) in if_stmt.elsif_branches.iter() {
                            collect_stmt_defs(stmts, body, add_def);
                        }
                        if let Some(ref else_stmts) = if_stmt.else_branch {
                            collect_stmt_defs(else_stmts, body, add_def);
                        }
                    }
                    Stmt::While { body: loop_body, .. } => {
                        collect_stmt_defs(loop_body, body, add_def);
                    }
                    Stmt::Try { body: try_body, except, .. } => {
                        collect_stmt_defs(try_body, body, add_def);
                        collect_stmt_defs(except, body, add_def);
                    }
                    _ => {}
                }
            }
        }

        collect_stmt_defs(body.body_stmts_typed(), body, &mut add_def);

        Arc::new(Self { definitions, var_to_defs, def_to_idx })
    }

    pub fn from_body_with_params(
        body: &Body,
        params: impl IntoIterator<Item = (Name, BindingId)>,
    ) -> Arc<Self> {
        let mut definitions = Vec::new();
        let mut var_to_defs: FxHashMap<SmolStr, SmallVec<[u32; 4]>> = FxHashMap::default();
        let mut def_to_idx: FxHashMap<Definition, u32> = FxHashMap::default();

        let mut add_def = |def: Definition| {
            if def_to_idx.contains_key(&def) {
                return;
            }
            let idx = definitions.len() as u32;
            var_to_defs.entry(def.var_name.clone()).or_default().push(idx);
            def_to_idx.insert(def.clone(), idx);
            definitions.push(def);
        };

        for (name, binding_id) in params {
            let def = Definition::parameter(&name, binding_id);
            add_def(def);
        }

        fn collect_stmt_defs<F: FnMut(Definition)>(
            stmts: &[hir_def::hir::StmtIdx],
            body: &Body,
            add_def: &mut F,
        ) {
            for &stmt_id in stmts {
                let opaque_id = hir_def::StmtId::from_idx(stmt_id);
                match body.stmt(opaque_id) {
                    Stmt::Assign { target, .. } => {
                        if let Some(var_name) =
                            extract_var_name_from_expr(hir_def::ExprId::from_idx(*target), body)
                        {
                            let def = Definition::assignment(var_name, stmt_id.into_raw());
                            add_def(def);
                        }
                    }
                    Stmt::VarDecl { bindings } => {
                        for &binding_id in bindings.iter() {
                            let binding = body.binding_idx(binding_id);
                            let def = Definition::var_decl(
                                &binding.name,
                                BindingId::from_idx(binding_id),
                            );
                            add_def(def);
                        }
                    }
                    Stmt::For { var, body: loop_body, .. } => {
                        let binding = body.binding_idx(*var);
                        let def = Definition::for_loop(&binding.name, BindingId::from_idx(*var));
                        add_def(def);
                        collect_stmt_defs(loop_body, body, add_def);
                    }
                    Stmt::ForEach { var, body: loop_body, .. } => {
                        let binding = body.binding_idx(*var);
                        let def =
                            Definition::for_each_loop(&binding.name, BindingId::from_idx(*var));
                        add_def(def);
                        collect_stmt_defs(loop_body, body, add_def);
                    }
                    Stmt::If(if_stmt) => {
                        collect_stmt_defs(&if_stmt.then_branch, body, add_def);
                        for (_cond, stmts) in if_stmt.elsif_branches.iter() {
                            collect_stmt_defs(stmts, body, add_def);
                        }
                        if let Some(ref else_stmts) = if_stmt.else_branch {
                            collect_stmt_defs(else_stmts, body, add_def);
                        }
                    }
                    Stmt::While { body: loop_body, .. } => {
                        collect_stmt_defs(loop_body, body, add_def);
                    }
                    Stmt::Try { body: try_body, except, .. } => {
                        collect_stmt_defs(try_body, body, add_def);
                        collect_stmt_defs(except, body, add_def);
                    }
                    _ => {}
                }
            }
        }

        collect_stmt_defs(body.body_stmts_typed(), body, &mut add_def);

        Arc::new(Self { definitions, var_to_defs, def_to_idx })
    }

    #[inline]
    pub fn get_index(&self, def: &Definition) -> Option<u32> {
        self.def_to_idx.get(def).copied()
    }

    #[inline]
    pub fn defs_for_var(&self, var_name: &str) -> &[u32] {
        let normalized = var_name.fold_lower();
        self.var_to_defs.get(normalized.as_str()).map(|v| v.as_slice()).unwrap_or(&[])
    }

    #[inline]
    pub fn get_definition(&self, idx: u32) -> &Definition {
        &self.definitions[idx as usize]
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.definitions.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.definitions.is_empty()
    }
}

fn extract_var_name_from_expr(expr_id: hir_def::ExprId, body: &Body) -> Option<SmolStr> {
    match body.expr(expr_id) {
        Expr::Path(name) => Some(SmolStr::new(name.as_str().fold_lower())),
        Expr::Field { base, field } => {
            let base_name = extract_var_name_from_expr(hir_def::ExprId::from_idx(*base), body)?;
            Some(SmolStr::new(format!("{}.{}", base_name, field.as_str().fold_lower())))
        }
        Expr::Index { base, .. } => {
            extract_var_name_from_expr(hir_def::ExprId::from_idx(*base), body)
        }
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReachingDefs {
    bits: FixedBitSet,

    def_index: Arc<DefinitionIndex>,
}

impl ReachingDefs {
    pub fn new(def_index: Arc<DefinitionIndex>) -> Self {
        Self { bits: FixedBitSet::with_capacity(def_index.len()), def_index }
    }

    pub fn singleton(def_index: Arc<DefinitionIndex>, def: &Definition) -> Self {
        let mut result = Self::new(def_index);
        result.insert(def);
        result
    }

    pub fn from_definitions(
        def_index: Arc<DefinitionIndex>,
        defs: impl IntoIterator<Item = Definition>,
    ) -> Self {
        let mut result = Self::new(def_index);
        for def in defs {
            result.insert(&def);
        }
        result
    }

    pub fn def_index(&self) -> &Arc<DefinitionIndex> {
        &self.def_index
    }

    pub fn iter(&self) -> impl Iterator<Item = &Definition> + '_ {
        self.bits.ones().map(|idx| self.def_index.get_definition(idx as u32))
    }

    pub fn defs_for_var(&self, var_name: &str) -> impl Iterator<Item = &Definition> + '_ {
        let indices = self.def_index.defs_for_var(var_name);
        indices
            .iter()
            .filter(|&&idx| self.bits.contains(idx as usize))
            .map(|&idx| self.def_index.get_definition(idx))
    }

    pub fn has_def_for_var(&self, var_name: &str) -> bool {
        self.defs_for_var(var_name).next().is_some()
    }

    pub fn insert(&mut self, def: &Definition) {
        if let Some(idx) = self.def_index.get_index(def) {
            self.bits.insert(idx as usize);
        }
    }

    pub fn kill(&mut self, var_name: &str) {
        for &idx in self.def_index.defs_for_var(var_name) {
            self.bits.set(idx as usize, false);
        }
    }

    pub fn gen_kill(&mut self, var_name: &str, new_def: &Definition) {
        self.kill(var_name);
        self.insert(new_def);
    }

    pub fn len(&self) -> usize {
        self.bits.count_ones(..)
    }

    pub fn is_empty(&self) -> bool {
        self.bits.is_clear()
    }

    /// Approximate live heap bytes for Salsa's `memory_usage` report: the `bits`
    /// bitset words (`ceil(bits / 8)`). The shared `Arc<DefinitionIndex>` is counted
    /// as the pointer only (shared across the method's program points), so omitted.
    pub fn estimated_heap(&self) -> usize {
        self.bits.len().div_ceil(8)
    }

    #[inline]
    pub fn bits(&self) -> &FixedBitSet {
        &self.bits
    }

    #[inline]
    pub fn bits_mut(&mut self) -> &mut FixedBitSet {
        &mut self.bits
    }
}

impl Lattice for ReachingDefs {
    fn join(&self, other: &Self) -> Self {
        debug_assert!(
            Arc::ptr_eq(&self.def_index, &other.def_index),
            "Cannot join ReachingDefs from different methods"
        );

        let mut bits = self.bits.clone();
        bits.union_with(&other.bits);

        Self { bits, def_index: self.def_index.clone() }
    }

    fn join_in_place(&mut self, other: &Self) {
        debug_assert!(
            Arc::ptr_eq(&self.def_index, &other.def_index),
            "Cannot join ReachingDefs from different methods"
        );

        self.bits.union_with(&other.bits);
    }

    fn is_more_informative_than(&self, other: &Self) -> bool {
        self.bits.is_subset(&other.bits)
    }
}

pub struct ReachingDefsTransfer;

impl ReachingDefsTransfer {
    fn extract_var_name(expr_id: hir_def::ExprId, body: &Body) -> Option<SmolStr> {
        match body.expr(expr_id) {
            Expr::Path(name) => Some(SmolStr::new(name.as_str())),

            Expr::Field { base, field } => {
                let base_name = Self::extract_var_name(hir_def::ExprId::from_idx(*base), body)?;
                Some(SmolStr::new(format!("{}.{}", base_name, field.as_str())))
            }

            Expr::Index { base, .. } => {
                Self::extract_var_name(hir_def::ExprId::from_idx(*base), body)
            }

            _ => None,
        }
    }
}

/// Reaching definitions over one body. Parameters seed the entry state only
/// for methods; module-level code has none.
pub fn solve_reaching_definitions(
    cfg: Arc<cfg::ControlFlowGraph>,
    body_arc: &Arc<Body>,
    seed_params: bool,
) -> Option<Arc<ReachingDefsResult>> {
    solve_with(ReachingDefsTransfer, cfg, body_arc, seed_params)
}

/// [`solve_reaching_definitions`] where nothing crosses an unexecutable edge: a write followed by
/// `Возврат`, `Прервать` or `ВызватьИсключение` does not reach the text after the jump, and a
/// write in dead code reaches nothing.
pub fn solve_executable_reaching_definitions(
    cfg: Arc<cfg::ControlFlowGraph>,
    body_arc: &Arc<Body>,
    seed_params: bool,
) -> Option<Arc<ReachingDefsResult>> {
    solve_with(ExecutableReachingDefsTransfer, cfg, body_arc, seed_params)
}

fn solve_with<T: Transfer<ReachingDefs>>(
    transfer: T,
    cfg: Arc<cfg::ControlFlowGraph>,
    body_arc: &Arc<Body>,
    seed_params: bool,
) -> Option<Arc<ReachingDefsResult>> {
    let body: &Body = body_arc;
    let params: Vec<_> = if seed_params {
        body.params().map(|param_id| (body.binding(param_id).name.clone(), param_id)).collect()
    } else {
        Vec::new()
    };
    let def_index = DefinitionIndex::from_body_with_params(body, params);

    let mut initial_defs = ReachingDefs::new(def_index.clone());
    if seed_params {
        for param_id in body.params() {
            let binding = body.binding(param_id);
            initial_defs.insert(&Definition::parameter(&binding.name, param_id));
        }
    }

    let mut solver = crate::DataflowSolver::new(cfg, body.clone(), transfer);
    solver.set_max_iterations(crate::DEFAULT_MAX_ITERATIONS);
    solver.set_bottom_factory(|| ReachingDefs::new(def_index.clone()));
    solver.set_initial_state(initial_defs);

    solver.solve().map(|dataflow_result| {
        Arc::new(ReachingDefsResult::new(dataflow_result, Arc::clone(body_arc)))
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReachingDefsResult {
    block_in: rustc_hash::FxHashMap<petgraph::graph::NodeIndex, ReachingDefs>,

    block_out: rustc_hash::FxHashMap<petgraph::graph::NodeIndex, ReachingDefs>,

    stmt_to_block: rustc_hash::FxHashMap<hir_def::StmtId, petgraph::graph::NodeIndex>,

    block_stmts: rustc_hash::FxHashMap<petgraph::graph::NodeIndex, Vec<la_arena::RawIdx>>,

    /// The vertex of each condition a pass evaluates anew, by that condition: such a statement
    /// belongs to no basic block, and what enters the vertex is what the condition reads.
    condition_blocks: rustc_hash::FxHashMap<hir_def::ExprId, petgraph::graph::NodeIndex>,

    /// The block entering each `Для` and `Для Каждого`, by the first expression of the header.
    /// Those run once, on entry: what the loop's own back-edge brings to the header never
    /// reaches them, so they read what leaves this block.
    loop_entry_blocks: rustc_hash::FxHashMap<hir_def::ExprId, petgraph::graph::NodeIndex>,

    /// Shared with the lowering memo: a retained result must not carry its
    /// own copy of the body.
    body: Arc<Body>,
}

/// The expression naming the CFG header a read in `stmt` happens at, when `stmt` is a header
/// (`defs_for_var_at_header` takes it). Nothing writes between an `Если` condition and an
/// `ИначеЕсли` one, so the first condition answers for all of them, and likewise a `Для` lower
/// bound for the upper. `None` for a statement that is not a header: ask `defs_for_var_at_stmt`.
pub fn header_read_expr(body: &Body, stmt: hir_def::StmtId) -> Option<hir_def::ExprId> {
    let header = match body.stmt(stmt) {
        Stmt::If(if_stmt) => if_stmt.condition,
        Stmt::While { condition, .. } => *condition,
        Stmt::For { from, .. } => *from,
        Stmt::ForEach { collection, .. } => *collection,
        _ => return None,
    };
    Some(hir_def::ExprId::from_idx(header))
}

impl ReachingDefsResult {
    pub fn new(dataflow: crate::DataflowResult<ReachingDefs>, body: Arc<Body>) -> Self {
        use cfg::CfgVertex;

        let mut stmt_to_block = rustc_hash::FxHashMap::default();
        let mut block_stmts = rustc_hash::FxHashMap::default();
        let mut condition_blocks = rustc_hash::FxHashMap::default();
        let mut loop_entry_blocks = rustc_hash::FxHashMap::default();
        // The builder creates a loop header before its body, so among the header's predecessors
        // only the block entering the loop precedes it.
        let entry_of = |header: petgraph::graph::NodeIndex| {
            dataflow.cfg().incoming_edges(header).map(|(source, _)| source).find(|s| *s < header)
        };

        for (block_idx, vertex) in dataflow.cfg().vertices() {
            match vertex {
                CfgVertex::BasicBlock(basic_block) => {
                    let stmts: Vec<la_arena::RawIdx> =
                        basic_block.statements().iter().map(|stmt_id| stmt_id.into_raw()).collect();

                    block_stmts.insert(block_idx, stmts);

                    for &stmt_id in basic_block.statements() {
                        stmt_to_block.insert(stmt_id, block_idx);
                    }
                }
                CfgVertex::Conditional(header) => {
                    condition_blocks.insert(header.condition, block_idx);
                }
                CfgVertex::WhileHeader(header) => {
                    condition_blocks.insert(header.condition, block_idx);
                }
                CfgVertex::ForHeader(header) => {
                    loop_entry_blocks.extend(entry_of(block_idx).map(|entry| (header.from, entry)));
                }
                CfgVertex::ForEachHeader(header) => {
                    loop_entry_blocks
                        .extend(entry_of(block_idx).map(|entry| (header.collection, entry)));
                }
                _ => {}
            }
        }

        let mut block_in = rustc_hash::FxHashMap::default();
        let mut block_out = rustc_hash::FxHashMap::default();

        for (block_idx, in_state, out_state) in dataflow.blocks() {
            block_in.insert(block_idx, in_state.clone());
            block_out.insert(block_idx, out_state.clone());
        }

        Self {
            block_in,
            block_out,
            stmt_to_block,
            block_stmts,
            condition_blocks,
            loop_entry_blocks,
            body,
        }
    }

    pub fn defs_before_stmt(&self, stmt_id: hir_def::StmtId) -> Option<&ReachingDefs> {
        let block_idx = self.stmt_to_block.get(&stmt_id)?;
        self.block_in.get(block_idx)
    }

    pub fn defs_after_stmt(&self, stmt_id: hir_def::StmtId) -> Option<&ReachingDefs> {
        let block_idx = self.stmt_to_block.get(&stmt_id)?;
        self.block_out.get(block_idx)
    }

    pub fn defs_up_to_stmt(&self, stmt_id: hir_def::StmtId) -> Option<ReachingDefs> {
        let block_idx = self.stmt_to_block.get(&stmt_id)?;
        let stmt_list = self.block_stmts.get(block_idx)?;

        let mut state = self.block_in.get(block_idx)?.clone();

        for &hir_stmt_raw in stmt_list {
            let hir_stmt_id = hir_def::StmtId::from_raw(hir_stmt_raw);

            if hir_stmt_id == stmt_id {
                break;
            }

            let transfer = ReachingDefsTransfer;
            state = transfer.transfer_stmt(hir_stmt_raw, &state, &self.body);
        }

        Some(state)
    }

    pub fn defs_for_var_at_stmt(
        &self,
        var_name: &str,
        stmt_id: hir_def::StmtId,
    ) -> Option<Vec<Definition>> {
        let reaching = self.defs_up_to_stmt(stmt_id)?;
        Some(reaching.defs_for_var(var_name).cloned().collect())
    }

    /// The definitions of `var_name` a header reads, by the expression it evaluates first: an
    /// `Если`, `ИначеЕсли` or `Пока` condition, a `Для` lower bound, a `Для Каждого` collection.
    pub fn defs_for_var_at_header(
        &self,
        var_name: &str,
        header_expr: hir_def::ExprId,
    ) -> Option<Vec<Definition>> {
        let state = match self.condition_blocks.get(&header_expr) {
            Some(block) => self.block_in.get(block)?,
            None => self.block_out.get(self.loop_entry_blocks.get(&header_expr)?)?,
        };
        Some(state.defs_for_var(var_name).cloned().collect())
    }

    pub fn var_is_defined_at_stmt(&self, var_name: &str, stmt_id: hir_def::StmtId) -> bool {
        self.defs_before_stmt(stmt_id)
            .map(|reaching| reaching.has_def_for_var(var_name))
            .unwrap_or(false)
    }

    pub fn body(&self) -> &Body {
        &self.body
    }

    pub fn defs_at_block(&self, block_idx: petgraph::graph::NodeIndex) -> Option<&ReachingDefs> {
        self.block_in.get(&block_idx)
    }

    /// Approximate live heap bytes for Salsa's `memory_usage` report: the six
    /// per-block hashbrown tables (including each `ReachingDefs` bitset and the
    /// `block_stmts` index vectors). The shared [`Body`] and the shared
    /// `Arc<DefinitionIndex>` payloads behind each `ReachingDefs` are counted
    /// as the pointer only — their owners count the rest.
    pub fn estimated_heap(&self) -> usize {
        use petgraph::graph::NodeIndex;

        let mut bytes = crate::map_table_bytes::<NodeIndex, ReachingDefs>(self.block_in.len());
        for v in self.block_in.values() {
            bytes += v.estimated_heap();
        }
        bytes += crate::map_table_bytes::<NodeIndex, ReachingDefs>(self.block_out.len());
        for v in self.block_out.values() {
            bytes += v.estimated_heap();
        }
        bytes += crate::map_table_bytes::<hir_def::StmtId, NodeIndex>(self.stmt_to_block.len());
        bytes += crate::map_table_bytes::<hir_def::ExprId, NodeIndex>(self.condition_blocks.len());
        bytes += crate::map_table_bytes::<hir_def::ExprId, NodeIndex>(self.loop_entry_blocks.len());
        bytes += crate::map_table_bytes::<NodeIndex, Vec<la_arena::RawIdx>>(self.block_stmts.len());
        for stmts in self.block_stmts.values() {
            bytes += crate::vec_bytes::<la_arena::RawIdx>(stmts.len());
        }
        // The body is the lowering memo's `Arc`, counted there; only the pointer
        // is this result's own.
        bytes
    }
}

struct ExecutableReachingDefsTransfer;

impl Transfer<ReachingDefs> for ExecutableReachingDefsTransfer {
    fn transfer_stmt(&self, stmt_id: RawIdx, state: &ReachingDefs, body: &Body) -> ReachingDefs {
        ReachingDefsTransfer.transfer_stmt(stmt_id, state, body)
    }

    fn transfer_edge(&self, edge_kind: cfg::CfgEdgeType, state: &ReachingDefs) -> ReachingDefs {
        match edge_kind {
            cfg::CfgEdgeType::Unexecutable => ReachingDefs::new(state.def_index.clone()),
            _ => state.clone(),
        }
    }
}

impl Transfer<ReachingDefs> for ReachingDefsTransfer {
    fn transfer_stmt(&self, stmt_id: RawIdx, state: &ReachingDefs, body: &Body) -> ReachingDefs {
        use cfg_types::StmtId;

        let stmt_id = StmtId::from_raw(stmt_id);

        let mut new_state = state.clone();

        match body.stmt(stmt_id) {
            Stmt::Assign { target, .. } => {
                if let Some(var_name) =
                    Self::extract_var_name(hir_def::ExprId::from_idx(*target), body)
                {
                    let def = Definition::assignment(var_name.clone(), stmt_id.into_raw());
                    new_state.gen_kill(&var_name, &def);
                }
            }

            Stmt::VarDecl { bindings } => {
                for &binding_id in bindings.iter() {
                    let binding = body.binding_idx(binding_id);
                    let def = Definition::var_decl(&binding.name, BindingId::from_idx(binding_id));
                    new_state.insert(&def);
                }
            }

            Stmt::For { var, .. } => {
                let binding = body.binding_idx(*var);
                let def = Definition::for_loop(&binding.name, BindingId::from_idx(*var));
                new_state.gen_kill(binding.name.as_str(), &def);
            }

            Stmt::ForEach { var, .. } => {
                let binding = body.binding_idx(*var);
                let def = Definition::for_each_loop(&binding.name, BindingId::from_idx(*var));
                new_state.gen_kill(binding.name.as_str(), &def);
            }

            _ => {}
        }

        new_state
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_def_index(defs: &[Definition]) -> Arc<DefinitionIndex> {
        let mut definitions = Vec::new();
        let mut var_to_defs: FxHashMap<SmolStr, SmallVec<[u32; 4]>> = FxHashMap::default();
        let mut def_to_idx: FxHashMap<Definition, u32> = FxHashMap::default();

        for def in defs {
            if def_to_idx.contains_key(def) {
                continue;
            }
            let idx = definitions.len() as u32;
            var_to_defs.entry(def.var_name.clone()).or_default().push(idx);
            def_to_idx.insert(def.clone(), idx);
            definitions.push(def.clone());
        }

        Arc::new(DefinitionIndex { definitions, var_to_defs, def_to_idx })
    }

    #[test]
    fn test_definition_creation() {
        let name = Name::new("Переменная");
        let binding_id = BindingId::from_raw(la_arena::RawIdx::from_u32(0));

        let param_def = Definition::parameter(&name, binding_id);
        assert_eq!(param_def.var_name, "переменная");
        assert!(matches!(param_def.def_site, DefSite::Parameter(_)));

        let var_def = Definition::var_decl(&name, binding_id);
        assert!(matches!(var_def.def_site, DefSite::VarDecl(_)));
    }

    #[test]
    fn test_reaching_defs_empty() {
        let def_index = create_def_index(&[]);
        let empty = ReachingDefs::new(def_index);
        assert!(empty.is_empty());
    }

    #[test]
    fn test_reaching_defs_lattice_join() {
        let name = Name::new("x");
        let binding_id = BindingId::from_raw(la_arena::RawIdx::from_u32(0));

        let def1 = Definition::parameter(&name, binding_id);
        let def2 = Definition::var_decl(&name, binding_id);

        let def_index = create_def_index(&[def1.clone(), def2.clone()]);

        let set1 = ReachingDefs::singleton(def_index.clone(), &def1);
        let set2 = ReachingDefs::singleton(def_index.clone(), &def2);

        let joined = set1.join(&set2);
        assert_eq!(joined.len(), 2);
        assert!(joined.iter().any(|d| d == &def1));
        assert!(joined.iter().any(|d| d == &def2));
    }

    #[test]
    fn test_reaching_defs_lattice_join_idempotent() {
        let name = Name::new("x");
        let binding_id = BindingId::from_raw(la_arena::RawIdx::from_u32(0));
        let def = Definition::parameter(&name, binding_id);

        let def_index = create_def_index(std::slice::from_ref(&def));
        let set = ReachingDefs::singleton(def_index, &def);
        let joined = set.join(&set);
        assert_eq!(joined, set);
    }

    #[test]
    fn test_reaching_defs_lattice_join_commutative() {
        let name = Name::new("x");
        let binding_id = BindingId::from_raw(la_arena::RawIdx::from_u32(0));

        let def1 = Definition::parameter(&name, binding_id);
        let def2 = Definition::var_decl(&name, binding_id);

        let def_index = create_def_index(&[def1.clone(), def2.clone()]);

        let set1 = ReachingDefs::singleton(def_index.clone(), &def1);
        let set2 = ReachingDefs::singleton(def_index.clone(), &def2);

        assert_eq!(set1.join(&set2), set2.join(&set1));
    }

    #[test]
    fn test_reaching_defs_lattice_bottom_identity() {
        let name = Name::new("x");
        let binding_id = BindingId::from_raw(la_arena::RawIdx::from_u32(0));
        let def = Definition::parameter(&name, binding_id);

        let def_index = create_def_index(std::slice::from_ref(&def));
        let set = ReachingDefs::singleton(def_index.clone(), &def);
        let bottom = ReachingDefs::new(def_index);

        assert_eq!(set.join(&bottom), set);
        assert_eq!(bottom.join(&set), set);
    }

    #[test]
    fn test_gen_kill() {
        let name = Name::new("x");
        let binding_id1 = BindingId::from_raw(la_arena::RawIdx::from_u32(0));
        let binding_id2 = BindingId::from_raw(la_arena::RawIdx::from_u32(1));

        let def1 = Definition::parameter(&name, binding_id1);
        let def2 = Definition::var_decl(&name, binding_id2);

        let def_index = create_def_index(&[def1.clone(), def2.clone()]);
        let mut state = ReachingDefs::singleton(def_index, &def1);
        assert_eq!(state.len(), 1);

        state.gen_kill("x", &def2);
        assert_eq!(state.len(), 1);
        assert!(!state.iter().any(|d| d == &def1));
        assert!(state.iter().any(|d| d == &def2));
    }

    #[test]
    fn test_case_insensitive() {
        let name_upper = Name::new("ПЕРЕМЕННАЯ");
        let name_lower = Name::new("переменная");
        let binding_id = BindingId::from_raw(la_arena::RawIdx::from_u32(0));

        let def1 = Definition::parameter(&name_upper, binding_id);
        let def2 = Definition::var_decl(&name_lower, binding_id);

        assert_eq!(def1.var_name, def2.var_name);

        let def_index = create_def_index(&[def1.clone(), def2.clone()]);
        let mut state = ReachingDefs::singleton(def_index, &def1);
        state.gen_kill("ПЕРЕМЕННАЯ", &def2);

        assert_eq!(state.len(), 1);
        assert!(state.iter().any(|d| d == &def2));
    }

    #[test]
    fn test_defs_for_var() {
        let name_x = Name::new("x");
        let name_y = Name::new("y");
        let binding_id = BindingId::from_raw(la_arena::RawIdx::from_u32(0));

        let def_x = Definition::parameter(&name_x, binding_id);
        let def_y = Definition::parameter(&name_y, binding_id);

        let def_index = create_def_index(&[def_x.clone(), def_y.clone()]);
        let state = ReachingDefs::from_definitions(def_index, [def_x.clone(), def_y.clone()]);

        let x_defs: Vec<_> = state.defs_for_var("x").collect();
        assert_eq!(x_defs.len(), 1);
        assert_eq!(x_defs[0], &def_x);

        assert!(state.has_def_for_var("x"));
        assert!(state.has_def_for_var("y"));
        assert!(!state.has_def_for_var("z"));
    }
}
