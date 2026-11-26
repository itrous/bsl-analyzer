use cfg_types::IdConversion;
use fixedbitset::FixedBitSet;
use hir_def::{
    body::{
        events::{hir_suppressed, BodyEvents, EvalPoint},
        Body,
    },
    hir::{Expr, Stmt, StmtIdx},
    BindingId, ExprId, Name, StmtId,
};
use la_arena::RawIdx;
use rustc_hash::{FxHashMap, FxHashSet};
use smallvec::SmallVec;
use smol_str::SmolStr;
use std::sync::Arc;
use stdx::case::CaseExt;

use crate::{DataflowResult, DataflowSolver, Lattice, Transfer, DEFAULT_MAX_ITERATIONS};

#[cfg(test)]
mod tests;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DefSite {
    Parameter(BindingId),

    VarDecl(BindingId),

    Assignment(RawIdx),

    ForLoop(BindingId),

    ForEachLoop(BindingId),

    /// No value the body gave: whatever the name held on entry.
    Unknown,

    /// An event that may have rebound the name, or may have left it as it was.
    MayWrite(WriteId),
}

/// The identity of one possible write: its row in the table of
/// [`DefinitionIndex::write`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WriteId(u32);

/// An event at `point` that may rebind `target`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteEvent {
    pub point: EvalPoint,
    /// The name the event may rebind, folded to lower case.
    pub target: SmolStr,
    pub kind: WriteEventKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteEventKind {
    /// The name is passed bare to `call` as its argument `arg` at `position`.
    /// Whether the callee writes the argument back is not known here.
    Argument { call: ExprId, arg: ExprId, position: u32 },
    /// `Выполнить` runs code that may assign any name of the body.
    Execute,
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

    writes: Vec<WriteEvent>,

    /// The possible writes of each point, as definition indices.
    point_writes: FxHashMap<EvalPoint, SmallVec<[u32; 4]>>,

    /// The `Для` or `Для Каждого` definition of each loop variable.
    loop_defs: FxHashMap<BindingId, u32>,
}

#[derive(Default)]
struct IndexBuilder {
    definitions: Vec<Definition>,
    var_to_defs: FxHashMap<SmolStr, SmallVec<[u32; 4]>>,
    def_to_idx: FxHashMap<Definition, u32>,
    writes: Vec<WriteEvent>,
    point_writes: FxHashMap<EvalPoint, SmallVec<[u32; 4]>>,
    loop_defs: FxHashMap<BindingId, u32>,
}

impl IndexBuilder {
    fn add(&mut self, def: Definition) -> u32 {
        if let Some(&idx) = self.def_to_idx.get(&def) {
            return idx;
        }
        let idx = self.definitions.len() as u32;
        self.var_to_defs.entry(def.var_name.clone()).or_default().push(idx);
        self.def_to_idx.insert(def.clone(), idx);
        self.definitions.push(def);
        idx
    }

    fn add_write(&mut self, point: EvalPoint, target: &str, kind: WriteEventKind) {
        let id = WriteId(self.writes.len() as u32);
        let def = Definition::new(SmolStr::new(target), DefSite::MayWrite(id));
        self.writes.push(WriteEvent { point, target: def.var_name.clone(), kind });
        let idx = self.add(def);
        self.point_writes.entry(point).or_default().push(idx);
    }

    fn finish(self) -> Arc<DefinitionIndex> {
        Arc::new(DefinitionIndex {
            definitions: self.definitions,
            var_to_defs: self.var_to_defs,
            def_to_idx: self.def_to_idx,
            writes: self.writes,
            point_writes: self.point_writes,
            loop_defs: self.loop_defs,
        })
    }
}

impl DefinitionIndex {
    pub fn from_body(body: &Body, events: &BodyEvents) -> Arc<Self> {
        Self::from_body_with_params(body, events, [])
    }

    /// Every definition the body can make, from its syntactic events: the
    /// parameters given, declarations, writes and loop variables of every
    /// statement, an entry `Unknown` for every name of the body, and a
    /// possible write for every bare-name argument and for every name at each
    /// `Выполнить`, names mentioned only after it included.
    pub fn from_body_with_params(
        body: &Body,
        events: &BodyEvents,
        params: impl IntoIterator<Item = (Name, BindingId)>,
    ) -> Arc<Self> {
        let mut index = IndexBuilder::default();
        for (name, binding_id) in params {
            index.add(Definition::parameter(&name, binding_id));
        }

        for stmt in events.stmts() {
            if let Some(var_name) =
                stmt.write.and_then(|write| extract_var_name_from_expr(write.target, body))
            {
                index.add(Definition::assignment(var_name, stmt.stmt.into_raw()));
            }
            match body.stmt(stmt.stmt) {
                Stmt::VarDecl { bindings } => {
                    for &binding_id in bindings.iter() {
                        let binding = body.binding_idx(binding_id);
                        index.add(Definition::var_decl(
                            &binding.name,
                            BindingId::from_idx(binding_id),
                        ));
                    }
                }
                Stmt::For { var, .. } => {
                    let var = BindingId::from_idx(*var);
                    let idx = index.add(Definition::for_loop(&body.binding(var).name, var));
                    index.loop_defs.insert(var, idx);
                }
                Stmt::ForEach { var, .. } => {
                    let var = BindingId::from_idx(*var);
                    let idx = index.add(Definition::for_each_loop(&body.binding(var).name, var));
                    index.loop_defs.insert(var, idx);
                }
                _ => {}
            }
        }

        for name in events.names() {
            index.add(Definition::unknown(SmolStr::new(name)));
        }

        for stmt in events.stmts() {
            for root in stmt.roots.iter() {
                for call in root.calls.iter() {
                    for arg in call.path_args.iter() {
                        let Expr::Path(name) = body.expr(arg.expr) else { continue };
                        index.add_write(
                            root.point,
                            &name.as_str().fold_lower(),
                            WriteEventKind::Argument {
                                call: call.call,
                                arg: arg.expr,
                                position: arg.position,
                            },
                        );
                    }
                }
            }
            if stmt.execute {
                for name in events.names() {
                    index.add_write(EvalPoint::Stmt(stmt.stmt), name, WriteEventKind::Execute);
                }
            }
        }

        index.finish()
    }

    /// The possible-write event `id` stands for.
    pub fn write(&self, id: WriteId) -> &WriteEvent {
        &self.writes[id.0 as usize]
    }

    fn point_writes(&self, point: EvalPoint) -> &[u32] {
        self.point_writes.get(&point).map_or(&[], |writes| writes.as_slice())
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
/// for methods; module-level code has none. `events` are the body's own
/// syntactic events, kept with the result for its readers.
pub fn solve_reaching_definitions(
    cfg: Arc<cfg::ControlFlowGraph>,
    body_arc: &Arc<Body>,
    events: Arc<BodyEvents>,
    seed_params: bool,
) -> Option<Arc<ReachingDefsResult>> {
    let body: &Body = body_arc;
    let params = params_of(body, seed_params);
    let def_index = DefinitionIndex::from_body_with_params(body, &events, params.iter().cloned());

    let mut initial_defs = ReachingDefs::new(def_index.clone());
    for (name, binding_id) in &params {
        initial_defs.insert(&Definition::parameter(name, *binding_id));
    }

    let mut solver = DataflowSolver::new(cfg, body.clone(), ReachingDefsTransfer);
    solver.set_max_iterations(DEFAULT_MAX_ITERATIONS);
    solver.set_bottom_factory(|| ReachingDefs::new(def_index.clone()));
    solver.set_initial_state(initial_defs);

    solver.solve().map(|dataflow_result| {
        Arc::new(ReachingDefsResult::new(dataflow_result, Arc::clone(body_arc), events))
    })
}

/// Reaching definitions along executable paths only, with the body's possible
/// writes: a bare-name argument of a call and every name at `Выполнить` add a
/// [`DefSite::MayWrite`] beside the definitions already reaching, and an
/// ordinary write replaces them all. A point no executable path reaches —
/// text after `Возврат`, `Прервать` or `ВызватьИсключение`, a block entered
/// only from there — makes no definition, and the result holds no state for
/// it. On entry a parameter holds its `Parameter` definition and every other
/// name of the body `Unknown`.
///
/// `None` when no complete solution was published: the solver ran out of visits
/// before converging, or `recovered` refused a body whose lowering may have dropped
/// statements or a jump target ([`hir_suppressed`]). `None` never stands for an empty
/// body or a graph no executable path enters: those solve to a complete result whose
/// states are simply unreached. A reader must therefore read `None` as "the write set
/// is unknown", never as "there are no writes".
pub fn solve_executable_reaching_definitions(
    cfg: Arc<cfg::ControlFlowGraph>,
    body_arc: &Arc<Body>,
    events: Arc<BodyEvents>,
    seed_params: bool,
    recovered: RecoveredBody,
) -> Option<Arc<ReachingDefsResult>> {
    let body: &Body = body_arc;
    if recovered == RecoveredBody::Refuse && hir_suppressed(body) {
        return None;
    }
    let params = params_of(body, seed_params);
    let def_index = DefinitionIndex::from_body_with_params(body, &events, params.iter().cloned());
    let entry = entry_definitions(&def_index, &events, &params);
    let budget = visit_budget(&cfg);
    let solved =
        solve_executable(ExecutableReachingDefsTransfer::new(body), cfg, body, entry, budget)?;
    publish_executable(&solved, body_arc, events)
}

/// Block visits a body's solution may take before it is given up as not converging.
///
/// Every state only grows, so the visits a body needs grow with its blocks: a long method
/// with many nested loops needs tens of thousands of them and does converge, while one bound
/// for every body would cut it off and leave it without any proof. The bound keeps a guard
/// against a transfer that never settles.
fn visit_budget(cfg: &cfg::ControlFlowGraph) -> usize {
    #[cfg(feature = "test-seams")]
    if let Some(visits) = visit_budget_seam::overridden() {
        return visits;
    }
    const VISITS_PER_BLOCK: usize = 64;
    DEFAULT_MAX_ITERATIONS.max(cfg.vertex_count().saturating_mul(VISITS_PER_BLOCK))
}

/// A bound a test can install for [`visit_budget`] without changing the graph.
///
/// Only the bound shrinks; the solve itself is the production one. Available with the
/// `test-seams` feature, which a dependent crate enables from its dev-dependencies.
#[cfg(feature = "test-seams")]
pub fn with_visit_budget<R>(visits: usize, run: impl FnOnce() -> R) -> R {
    let _restore = visit_budget_seam::install(visits);
    run()
}

#[cfg(feature = "test-seams")]
mod visit_budget_seam {
    use std::cell::Cell;

    thread_local! {
        static OVERRIDE: Cell<Option<usize>> = const { Cell::new(None) };
    }

    pub(super) fn overridden() -> Option<usize> {
        OVERRIDE.with(Cell::get)
    }

    /// Puts the thread's bound back when the test ends, so a panic cannot leak it into
    /// the next test of the same thread.
    pub(super) struct Restore(Option<usize>);

    impl Drop for Restore {
        fn drop(&mut self) {
            let previous = self.0;
            OVERRIDE.with(|bound| bound.set(previous));
        }
    }

    pub(super) fn install(visits: usize) -> Restore {
        Restore(OVERRIDE.with(|bound| bound.replace(Some(visits))))
    }
}

/// What [`solve_executable_reaching_definitions`] does with a body whose
/// lowering may have dropped statements or a jump target ([`hir_suppressed`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoveredBody {
    /// No result: a write the lowering lost may reach any read, so what the
    /// kept statements give is no proof of a value.
    Refuse,
    /// Solve the statements the lowering kept, for a reader that accepts an
    /// answer blind to what a syntax error swallowed.
    Solve,
}

fn params_of(body: &Body, seed_params: bool) -> Vec<(Name, BindingId)> {
    if !seed_params {
        return Vec::new();
    }
    body.params().map(|param_id| (body.binding(param_id).name.clone(), param_id)).collect()
}

/// A parameter holds its own definition on entry; any other name of the body
/// holds nothing the body gave it.
fn entry_definitions(
    def_index: &Arc<DefinitionIndex>,
    events: &BodyEvents,
    params: &[(Name, BindingId)],
) -> ReachingDefs {
    let mut entry = ReachingDefs::new(def_index.clone());
    let mut param_names = FxHashSet::default();
    for (name, binding_id) in params {
        entry.insert(&Definition::parameter(name, *binding_id));
        param_names.insert(name.as_str().fold_lower());
    }
    for name in events.names() {
        if !param_names.contains(name) {
            entry.insert(&Definition::unknown(SmolStr::new(name)));
        }
    }
    entry
}

fn solve_executable<T: Transfer<ExecutableDefs>>(
    transfer: T,
    cfg: Arc<cfg::ControlFlowGraph>,
    body: &Body,
    entry: ReachingDefs,
    max_iterations: usize,
) -> Option<DataflowResult<ExecutableDefs>> {
    let mut solver = DataflowSolver::new(cfg, body.clone(), transfer);
    solver.set_max_iterations(max_iterations);
    solver.set_bottom_factory(ExecutableDefs::unreached);
    solver.set_initial_state(ExecutableDefs::entering(Reach::Reached(entry)));
    solver.solve()
}

/// A partial solution proves nothing, so none is published.
fn publish_executable(
    solved: &DataflowResult<ExecutableDefs>,
    body_arc: &Arc<Body>,
    events: Arc<BodyEvents>,
) -> Option<Arc<ReachingDefsResult>> {
    solved.converged().then(|| {
        Arc::new(ReachingDefsResult::from_executable(solved, Arc::clone(body_arc), events))
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReachingDefsResult {
    block_in: rustc_hash::FxHashMap<petgraph::graph::NodeIndex, ReachingDefs>,

    block_out: rustc_hash::FxHashMap<petgraph::graph::NodeIndex, ReachingDefs>,

    stmt_to_block: rustc_hash::FxHashMap<hir_def::StmtId, petgraph::graph::NodeIndex>,

    block_stmts: rustc_hash::FxHashMap<petgraph::graph::NodeIndex, Vec<la_arena::RawIdx>>,

    /// The vertex evaluating each root no statement holds, by that root: an `Если`,
    /// `ИначеЕсли` or `Пока` condition, a `Для` bound, a `Для Каждого` collection. What
    /// enters the vertex is what the root reads; a loop's entry is not on its back-edge.
    root_blocks: rustc_hash::FxHashMap<hir_def::ExprId, petgraph::graph::NodeIndex>,

    /// The roots a vertex evaluates before each of its later roots, in order: a `Для` upper
    /// bound is read after the lower bound has run.
    earlier_roots: rustc_hash::FxHashMap<hir_def::ExprId, Box<[hir_def::ExprId]>>,

    /// Shared with the lowering memo: a retained result must not carry its
    /// own copy of the body.
    body: Arc<Body>,

    /// Shared with the events memo of the same body, like `body`.
    events: Arc<BodyEvents>,

    /// Whether the states come from the executable solver, whose transfer
    /// replays the statements of a block for a statement query.
    executable: bool,
}

impl ReachingDefsResult {
    pub fn new(
        dataflow: crate::DataflowResult<ReachingDefs>,
        body: Arc<Body>,
        events: Arc<BodyEvents>,
    ) -> Self {
        let mut block_in = rustc_hash::FxHashMap::default();
        let mut block_out = rustc_hash::FxHashMap::default();
        for (block_idx, in_state, out_state) in dataflow.blocks() {
            block_in.insert(block_idx, in_state.clone());
            block_out.insert(block_idx, out_state.clone());
        }
        Self::with_states(dataflow.cfg(), block_in, block_out, body, events, false)
    }

    /// The normal exit states of an executable solution; a block no executable
    /// path enters has no state at all.
    fn from_executable(
        dataflow: &crate::DataflowResult<ExecutableDefs>,
        body: Arc<Body>,
        events: Arc<BodyEvents>,
    ) -> Self {
        let mut block_in = rustc_hash::FxHashMap::default();
        let mut block_out = rustc_hash::FxHashMap::default();
        for (block_idx, in_state, out_state) in dataflow.blocks() {
            if let Reach::Reached(defs) = &in_state.normal {
                block_in.insert(block_idx, defs.clone());
            }
            if let Reach::Reached(defs) = &out_state.normal {
                block_out.insert(block_idx, defs.clone());
            }
        }
        Self::with_states(dataflow.cfg(), block_in, block_out, body, events, true)
    }

    fn with_states(
        cfg: &cfg::ControlFlowGraph,
        block_in: rustc_hash::FxHashMap<petgraph::graph::NodeIndex, ReachingDefs>,
        block_out: rustc_hash::FxHashMap<petgraph::graph::NodeIndex, ReachingDefs>,
        body: Arc<Body>,
        events: Arc<BodyEvents>,
        executable: bool,
    ) -> Self {
        use cfg::CfgVertex;

        let mut stmt_to_block = rustc_hash::FxHashMap::default();
        let mut block_stmts = rustc_hash::FxHashMap::default();
        let mut root_blocks = rustc_hash::FxHashMap::default();
        let mut earlier_roots = rustc_hash::FxHashMap::default();

        for (block_idx, vertex) in cfg.vertices() {
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
                    root_blocks.insert(header.condition, block_idx);
                }
                CfgVertex::WhileHeader(header) => {
                    root_blocks.insert(header.condition, block_idx);
                }
                CfgVertex::LoopEntry(entry) => {
                    let roots = entry.roots();
                    root_blocks.extend(roots.iter().map(|&root| (root, block_idx)));
                    earlier_roots.extend(
                        roots
                            .iter()
                            .enumerate()
                            .skip(1)
                            .map(|(at, &root)| (root, roots[..at].into())),
                    );
                }
                _ => {}
            }
        }

        Self {
            block_in,
            block_out,
            stmt_to_block,
            block_stmts,
            root_blocks,
            earlier_roots,
            body,
            events,
            executable,
        }
    }

    /// The syntactic events of the body the result was solved over.
    pub fn events(&self) -> &Arc<BodyEvents> {
        &self.events
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

            state = if self.executable {
                match ExecutableReachingDefsTransfer::replay()
                    .transfer_stmt(
                        hir_stmt_raw,
                        &ExecutableDefs::entering(Reach::Reached(state)),
                        &self.body,
                    )
                    .normal
                {
                    Reach::Reached(defs) => defs,
                    Reach::Unreached => unreachable!("a reached statement exits reached"),
                }
            } else {
                ReachingDefsTransfer.transfer_stmt(hir_stmt_raw, &state, &self.body)
            };
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

    /// The definitions of `var_name` a header root reads: an `Если`, `ИначеЕсли` or `Пока`
    /// condition, a `Для` bound, a `Для Каждого` collection. A root evaluated after others of
    /// its vertex reads what they left.
    pub fn defs_for_var_at_header(
        &self,
        var_name: &str,
        header_expr: hir_def::ExprId,
    ) -> Option<Vec<Definition>> {
        let mut state = self.block_in.get(self.root_blocks.get(&header_expr)?)?.clone();
        if self.executable {
            for &root in self.earlier_roots.get(&header_expr).into_iter().flatten() {
                state = match ExecutableReachingDefsTransfer::replay()
                    .transfer_expr(
                        root,
                        &ExecutableDefs::entering(Reach::Reached(state)),
                        &self.body,
                    )
                    .normal
                {
                    Reach::Reached(defs) => defs,
                    Reach::Unreached => unreachable!("a reached root exits reached"),
                };
            }
        }
        Some(state.defs_for_var(var_name).cloned().collect())
    }

    /// The possible-write event `id` stands for.
    pub fn write(&self, id: WriteId) -> Option<&WriteEvent> {
        Some(self.def_index()?.write(id))
    }

    /// The possible writes of `var_name` that the evaluation at `point` makes itself.
    pub fn possible_writes_at(&self, point: EvalPoint, var_name: &str) -> Vec<Definition> {
        let var_name = var_name.fold_lower();
        self.writes_at_point(point).filter(|def| def.var_name == var_name).cloned().collect()
    }

    /// Every possible write the evaluation at `point` makes itself, over all names, in index
    /// order. [`Definition::var_name`] already holds the folded key [`Self::possible_writes_at`]
    /// matches by. A caller that checks several names of one point iterates this once instead
    /// of repeating the point lookup per name; no reachable state answers nothing, like the
    /// per-name query.
    pub fn writes_at_point(&self, point: EvalPoint) -> impl Iterator<Item = &Definition> + '_ {
        const NO_WRITES: &[u32] = &[];
        let index = self.def_index();
        let rows = index.map_or(NO_WRITES, |index| index.point_writes(point));
        rows.iter().filter_map(move |&idx| index.map(|index| index.get_definition(idx)))
    }

    /// The index every state numbers its definitions by; absent when no point was reached.
    fn def_index(&self) -> Option<&Arc<DefinitionIndex>> {
        self.block_in.values().next().map(ReachingDefs::def_index)
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

    /// Approximate live heap bytes for Salsa's `memory_usage` report: the five
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
        bytes += crate::map_table_bytes::<hir_def::ExprId, NodeIndex>(self.root_blocks.len());
        bytes += crate::map_table_bytes::<hir_def::ExprId, Box<[hir_def::ExprId]>>(
            self.earlier_roots.len(),
        );
        for roots in self.earlier_roots.values() {
            bytes += crate::vec_bytes::<hir_def::ExprId>(roots.len());
        }
        bytes += crate::map_table_bytes::<NodeIndex, Vec<la_arena::RawIdx>>(self.block_stmts.len());
        for stmts in self.block_stmts.values() {
            bytes += crate::vec_bytes::<la_arena::RawIdx>(stmts.len());
        }
        // The body is the lowering memo's `Arc`, counted there; only the pointer
        // is this result's own.
        bytes
    }
}

/// Definitions reaching a point along executable paths, or `Unreached` when no
/// executable path gets there. `Unreached` is the neutral element of the join
/// and makes no definitions; an entry reached with nothing is `Reached` with an
/// empty set.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Reach {
    Unreached,
    Reached(ReachingDefs),
}

impl Reach {
    fn join_in_place(&mut self, other: &Reach) {
        let Reach::Reached(other) = other else { return };
        match self {
            Reach::Unreached => *self = Reach::Reached(other.clone()),
            Reach::Reached(defs) => defs.join_in_place(other),
        }
    }
}

/// The state of the executable solver: what leaves a point when it completes,
/// and what leaves it when it fails part way, to a handler. A state entering a
/// point has both parts equal: each point builds its exceptional part from its
/// own input again, so a failure carries the effects of the failing point, not
/// those of every point before it.
///
/// `skipped` is what leaves a `Для Каждого` header when the collection has no
/// further element: the loop then binds nothing, so the variable keeps what it
/// held. Every other point leaves it `Unreached`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ExecutableDefs {
    normal: Reach,
    exceptional: Reach,
    skipped: Reach,
}

impl ExecutableDefs {
    fn unreached() -> Self {
        Self::entering(Reach::Unreached)
    }

    fn entering(reach: Reach) -> Self {
        Self { normal: reach.clone(), exceptional: reach, skipped: Reach::Unreached }
    }
}

impl Lattice for ExecutableDefs {
    fn join(&self, other: &Self) -> Self {
        let mut joined = self.clone();
        joined.join_in_place(other);
        joined
    }

    fn join_in_place(&mut self, other: &Self) {
        self.normal.join_in_place(&other.normal);
        self.exceptional.join_in_place(&other.exceptional);
        self.skipped.join_in_place(&other.skipped);
    }
}

struct ExecutableReachingDefsTransfer {
    /// See [`writes_hidden_on_exception`].
    hidden_on_exception: FxHashSet<StmtId>,
}

/// A raising statement whose ordinary writes must not ride its exception edge:
/// every statement after it in the same region is an assignment that cannot raise,
/// and at least one such statement follows. Once an ordinary write completes,
/// nothing left can enter the handler. A possible write of the statement itself
/// is not covered: the call may write its argument and then raise, and the
/// assignments after it then do not run. A statement with nothing after it is not
/// here — its own failure may come after the write, which the exceptional exit
/// already records.
fn writes_hidden_on_exception(body: &Body) -> FxHashSet<StmtId> {
    let mut hidden = FxHashSet::default();
    hide_writes_after_safe_tail(body.body_stmts_typed(), body, false, &mut hidden);
    hidden
}

/// `region_ends_safely` means that once `stmts` completes, nothing that can raise remains.
/// A protected body's own list does not: its last statement may fail after its write.
/// A branch does, when the statement that contains it is followed only by assignments
/// that cannot raise — the last statement of that branch then has the same tail.
fn hide_writes_after_safe_tail(
    stmts: &[StmtIdx],
    body: &Body,
    region_ends_safely: bool,
    hidden: &mut FxHashSet<StmtId>,
) {
    for (position, &stmt_id) in stmts.iter().enumerate() {
        let later = &stmts[position + 1..];
        let later_cannot_raise =
            later.iter().all(|&later_id| crate::is_non_raising_assignment(body, later_id));
        let ends_safely_after = later_cannot_raise && (!later.is_empty() || region_ends_safely);
        if ends_safely_after && !crate::is_non_raising_assignment(body, stmt_id) {
            hidden.insert(StmtId::from_idx(stmt_id));
        }
        match body.stmt_idx(stmt_id) {
            Stmt::For { body: nested, .. }
            | Stmt::ForEach { body: nested, .. }
            | Stmt::While { body: nested, .. } => {
                hide_writes_after_safe_tail(nested, body, false, hidden);
            }
            Stmt::If(if_stmt) => {
                hide_writes_after_safe_tail(&if_stmt.then_branch, body, ends_safely_after, hidden);
                for (_, branch) in if_stmt.elsif_branches.iter() {
                    hide_writes_after_safe_tail(branch, body, ends_safely_after, hidden);
                }
                if let Some(else_stmts) = &if_stmt.else_branch {
                    hide_writes_after_safe_tail(else_stmts, body, ends_safely_after, hidden);
                }
            }
            Stmt::Try { body: protected, except, .. } => {
                hide_writes_after_safe_tail(protected, body, false, hidden);
                hide_writes_after_safe_tail(except, body, ends_safely_after, hidden);
            }
            _ => {}
        }
    }
}

impl ExecutableReachingDefsTransfer {
    fn new(body: &Body) -> Self {
        Self { hidden_on_exception: writes_hidden_on_exception(body) }
    }

    /// Replaying a block's normal exits does not consult [`Self::hidden_on_exception`]:
    /// that set changes only the exceptional exit.
    fn replay() -> Self {
        Self { hidden_on_exception: FxHashSet::default() }
    }

    /// A point evaluated over the definitions `input`: its possible writes add
    /// to them, and its ordinary writes then replace the definitions of their
    /// names. A failure may come after any possible write and after the
    /// ordinary write itself, so the exceptional exit keeps the input together
    /// with every effect of the point.
    fn evaluate(
        input: &ReachingDefs,
        may_writes: &[u32],
        writes: &[(SmolStr, u32)],
    ) -> ExecutableDefs {
        let mut normal = input.clone();
        for &idx in may_writes {
            normal.bits.insert(idx as usize);
        }
        let mut exceptional = normal.clone();
        for (var_name, idx) in writes {
            normal.kill(var_name);
            normal.bits.insert(*idx as usize);
            exceptional.bits.insert(*idx as usize);
        }
        ExecutableDefs {
            normal: Reach::Reached(normal),
            exceptional: Reach::Reached(exceptional),
            skipped: Reach::Unreached,
        }
    }

    /// The ordinary writes of a statement: an assignment rebinds its target,
    /// and a declaration gives each declared name its declaration in place of
    /// the entry `Unknown`.
    fn stmt_writes(
        stmt_id: StmtId,
        body: &Body,
        def_index: &DefinitionIndex,
    ) -> SmallVec<[(SmolStr, u32); 1]> {
        let mut writes = SmallVec::new();
        match body.stmt(stmt_id) {
            Stmt::Assign { target, .. } => {
                if let Some(var_name) = extract_var_name_from_expr(ExprId::from_idx(*target), body)
                {
                    let def = Definition::assignment(var_name, stmt_id.into_raw());
                    if let Some(idx) = def_index.get_index(&def) {
                        writes.push((def.var_name, idx));
                    }
                }
            }
            Stmt::VarDecl { bindings } => {
                for &binding_id in bindings.iter() {
                    let binding = body.binding_idx(binding_id);
                    let def = Definition::var_decl(&binding.name, BindingId::from_idx(binding_id));
                    if let Some(idx) = def_index.get_index(&def) {
                        writes.push((def.var_name, idx));
                    }
                }
            }
            _ => {}
        }
        writes
    }
}

impl Transfer<ExecutableDefs> for ExecutableReachingDefsTransfer {
    fn transfer_stmt(
        &self,
        stmt_id: RawIdx,
        state: &ExecutableDefs,
        body: &Body,
    ) -> ExecutableDefs {
        let Reach::Reached(input) = &state.normal else { return ExecutableDefs::unreached() };
        let stmt_id = StmtId::from_raw(stmt_id);
        let def_index = input.def_index();
        let writes = Self::stmt_writes(stmt_id, body, def_index);
        let may = def_index.point_writes(EvalPoint::Stmt(stmt_id));
        // A plain assignment cannot raise, so its exception edge carries nothing.
        // A raising statement followed only by such assignments has completed its
        // ordinary write only when the region can no longer raise, so that write is
        // not on a path into the handler. A possible write of the statement itself
        // may have happened before the statement raised, and the assignments after
        // it then do not run, so that write still reaches the handler.
        if crate::is_non_raising_assignment(body, stmt_id.to_idx()) {
            let mut out = Self::evaluate(input, &[], &writes);
            out.exceptional = Reach::Unreached;
            return out;
        }
        if self.hidden_on_exception.contains(&stmt_id) {
            let mut out = Self::evaluate(input, may, &writes);
            let mut exceptional = input.clone();
            for &idx in may {
                exceptional.bits.insert(idx as usize);
            }
            out.exceptional = Reach::Reached(exceptional);
            return out;
        }
        Self::evaluate(input, may, &writes)
    }

    fn transfer_expr(
        &self,
        expr_id: ExprId,
        state: &ExecutableDefs,
        _body: &Body,
    ) -> ExecutableDefs {
        let Reach::Reached(input) = &state.normal else { return ExecutableDefs::unreached() };
        Self::evaluate(input, input.def_index().point_writes(EvalPoint::Header(expr_id)), &[])
    }

    fn transfer_loop_var_bind(&self, loop_var: BindingId, state: &mut ExecutableDefs, body: &Body) {
        let Reach::Reached(input) = &state.normal else {
            *state = ExecutableDefs::unreached();
            return;
        };
        let def_index = input.def_index();
        let loop_def = def_index.loop_defs.get(&loop_var).copied();
        let writes: SmallVec<[(SmolStr, u32); 1]> = loop_def
            .map(|idx| (SmolStr::new(body.binding(loop_var).name.as_str().fold_lower()), idx))
            .into_iter()
            .collect();
        let mut out = Self::evaluate(input, &[], &writes);
        let for_each = loop_def.is_some_and(|idx| {
            matches!(def_index.get_definition(idx).def_site, DefSite::ForEachLoop(_))
        });
        if for_each {
            out.skipped = Reach::Reached(input.clone());
        }
        *state = out;
    }

    /// An exception edge carries the exceptional exit of its source, the exit
    /// of a `Для Каждого` header without a further element its state before
    /// the binding, any other executable edge the normal exit; an unexecutable
    /// edge carries nothing.
    fn transfer_edge(&self, edge_kind: cfg::CfgEdgeType, state: &ExecutableDefs) -> ExecutableDefs {
        match edge_kind {
            cfg::CfgEdgeType::Unexecutable => ExecutableDefs::unreached(),
            cfg::CfgEdgeType::Exception => ExecutableDefs::entering(state.exceptional.clone()),
            cfg::CfgEdgeType::FalseBranch if state.skipped != Reach::Unreached => {
                ExecutableDefs::entering(state.skipped.clone())
            }
            _ => ExecutableDefs::entering(state.normal.clone()),
        }
    }

    fn follows_possible_failures(&self) -> bool {
        true
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
