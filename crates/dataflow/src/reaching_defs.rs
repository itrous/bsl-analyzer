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

/// A writer of a local that the body does not spell out as an assignment: the analysis cannot
/// read the value, only the fact that one may have been written here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ForeignWrite {
    /// An argument the callee is PROVEN to take by reference.
    ByRefArgument,
    /// An argument whose passing direction is not attested: the callee may take it either way.
    /// Kept apart from [`ForeignWrite::ByRefArgument`] because a reader is allowed to look past
    /// an unattested direction and not past a proven one.
    UnattestedArgument,
    /// `Выполнить`, which may write any name of the body.
    Execute,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DefSite {
    Parameter(BindingId),

    VarDecl(BindingId),

    Assignment(RawIdx),

    ForLoop(BindingId),

    ForEachLoop(BindingId),

    /// A write the body does not spell out, attributed to the statement that may perform it.
    /// `site` is the enclosing statement for a write inside a header expression too: identity
    /// of the place is all a consumer needs, and a header expression is not a statement.
    ForeignWrite {
        site: RawIdx,
        kind: ForeignWrite,
    },

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

    pub fn foreign_write(var_name: SmolStr, site: RawIdx, kind: ForeignWrite) -> Self {
        Self::new(var_name, DefSite::ForeignWrite { site, kind })
    }
}

/// One write a [`ForeignWriteEvents`] collector attributes to one place of the body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignWriteEvent {
    /// The enclosing statement, which becomes `DefSite::ForeignWrite::site`.
    pub site: hir_def::StmtId,
    pub var_name: SmolStr,
    pub kind: ForeignWrite,
}

/// Writes of the body that reaching definitions cannot read, described by a collector outside
/// this crate: the engine knows nothing about calls, callees or passing directions.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ForeignWriteEvents {
    /// Writes performed while a statement runs.
    pub stmt: FxHashMap<hir_def::StmtId, Vec<ForeignWriteEvent>>,
    /// Writes performed while a header expression is evaluated - an `Если` or `Пока` condition,
    /// a `Для` bound, a `Для Каждого` collection. Such an expression belongs to no basic block,
    /// so its writes cannot hang off a statement.
    pub header: FxHashMap<hir_def::ExprId, Vec<ForeignWriteEvent>>,
    /// Statements that may write EVERY name. The engine expands these over its own definition
    /// index: only the engine knows which names the body has.
    pub every_name: Vec<hir_def::StmtId>,
}

impl ForeignWriteEvents {
    pub fn is_empty(&self) -> bool {
        self.stmt.is_empty() && self.header.is_empty() && self.every_name.is_empty()
    }

    pub fn push_stmt(&mut self, site: hir_def::StmtId, var_name: SmolStr, kind: ForeignWrite) {
        self.stmt.entry(site).or_default().push(ForeignWriteEvent { site, var_name, kind });
    }

    pub fn push_header(
        &mut self,
        header: hir_def::ExprId,
        site: hir_def::StmtId,
        var_name: SmolStr,
        kind: ForeignWrite,
    ) {
        self.header.entry(header).or_default().push(ForeignWriteEvent { site, var_name, kind });
    }

    pub fn push_every_name(&mut self, site: hir_def::StmtId) {
        self.every_name.push(site);
    }
}

/// The foreign definitions of one body, ready to be generated: the events as the collector
/// described them, plus the name universe the engine expanded `every_name` over.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ForeignWriteGen {
    writes: Arc<ForeignWriteEvents>,
    names: Arc<Vec<SmolStr>>,
}

impl ForeignWriteGen {
    fn new(writes: Arc<ForeignWriteEvents>, index: &DefinitionIndex) -> Self {
        let names: Vec<SmolStr> =
            if writes.every_name.is_empty() { Vec::new() } else { index.var_names().collect() };
        Self { writes, names: Arc::new(names) }
    }

    /// Every foreign definition this body can hold, to be added to the definition index: a
    /// definition absent from the index cannot be set in any state.
    fn definitions(&self) -> Vec<Definition> {
        let from_event = |event: &ForeignWriteEvent| {
            Definition::foreign_write(event.var_name.clone(), event.site.into_raw(), event.kind)
        };
        let mut defs: Vec<Definition> = self
            .writes
            .stmt
            .values()
            .chain(self.writes.header.values())
            .flatten()
            .map(from_event)
            .collect();
        for &site in &self.writes.every_name {
            defs.extend(self.names.iter().map(|name| {
                Definition::foreign_write(name.clone(), site.into_raw(), ForeignWrite::Execute)
            }));
        }
        defs
    }

    /// Adds, without killing, the definitions a statement writes foreign. The earlier
    /// definition still reaches the use: a by-reference argument or `Выполнить` MAY write, so a
    /// consumer that sees a foreign definition beside an ordinary one knows only that the value
    /// is not proven.
    fn apply_stmt(&self, stmt_id: hir_def::StmtId, state: &mut ReachingDefs) {
        if let Some(events) = self.writes.stmt.get(&stmt_id) {
            for event in events {
                state.insert(&Definition::foreign_write(
                    event.var_name.clone(),
                    event.site.into_raw(),
                    event.kind,
                ));
            }
        }
        if self.writes.every_name.contains(&stmt_id) {
            for name in self.names.iter() {
                state.insert(&Definition::foreign_write(
                    name.clone(),
                    stmt_id.into_raw(),
                    ForeignWrite::Execute,
                ));
            }
        }
    }

    /// Approximate live heap bytes of the shared events and name universe, as
    /// [`ReachingDefsResult::estimated_heap`] counts its own tables. Both are behind an `Arc`
    /// shared with the transfer, which is dropped with the solve, so this result owns them.
    fn estimated_heap(&self) -> usize {
        let event_bytes =
            |events: &Vec<ForeignWriteEvent>| crate::vec_bytes::<ForeignWriteEvent>(events.len());
        let mut bytes = crate::map_table_bytes::<hir_def::StmtId, Vec<ForeignWriteEvent>>(
            self.writes.stmt.len(),
        ) + crate::map_table_bytes::<hir_def::ExprId, Vec<ForeignWriteEvent>>(
            self.writes.header.len(),
        ) + crate::vec_bytes::<hir_def::StmtId>(self.writes.every_name.len())
            + crate::vec_bytes::<SmolStr>(self.names.len());
        bytes += self.writes.stmt.values().map(event_bytes).sum::<usize>();
        bytes += self.writes.header.values().map(event_bytes).sum::<usize>();
        bytes
    }

    fn apply_header(&self, header: hir_def::ExprId, state: &mut ReachingDefs) {
        if let Some(events) = self.writes.header.get(&header) {
            for event in events {
                state.insert(&Definition::foreign_write(
                    event.var_name.clone(),
                    event.site.into_raw(),
                    event.kind,
                ));
            }
        }
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

    /// Every name the index holds a definition for, `fold_lower`ed as [`Definition::new`] folds
    /// them. The name universe a `Выполнить` write expands over.
    fn var_names(&self) -> impl Iterator<Item = SmolStr> + '_ {
        self.var_to_defs.keys().cloned()
    }

    /// This index with `extra` appended. The definitions already present keep their positions,
    /// so a state built against either index means the same bits.
    fn extended(&self, extra: impl IntoIterator<Item = Definition>) -> Arc<Self> {
        let mut definitions = self.definitions.clone();
        let mut var_to_defs = self.var_to_defs.clone();
        let mut def_to_idx = self.def_to_idx.clone();
        for def in extra {
            if def_to_idx.contains_key(&def) {
                continue;
            }
            let idx = definitions.len() as u32;
            var_to_defs.entry(def.var_name.clone()).or_default().push(idx);
            def_to_idx.insert(def.clone(), idx);
            definitions.push(def);
        }
        Arc::new(Self { definitions, var_to_defs, def_to_idx })
    }
}

/// Every definition the statements of one region generate, nested statements included: the
/// gen set of a whole subtree of the body, which both the definition index and the
/// `Exception` edge of a `Попытка` need.
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
                    let def = Definition::var_decl(&binding.name, BindingId::from_idx(binding_id));
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
                let def = Definition::for_each_loop(&binding.name, BindingId::from_idx(*var));
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

/// The definitions the protected statements of a `Попытка` may have produced when its handler
/// starts: the union, over those statements, of what holds immediately BEFORE each of them.
/// [`walk_interruptible`] decides which statements contribute and which are in tail position.
fn collect_interruptible_defs<F: FnMut(Definition)>(
    stmts: &[hir_def::hir::StmtIdx],
    body: &Body,
    live: &crate::LiveStatements,
    add_def: &mut F,
) {
    walk_interruptible(stmts, body, live, true, &mut |stmt_id, in_tail| {
        match body.stmt_idx(stmt_id) {
            Stmt::Assign { target, .. } if !in_tail => {
                if let Some(var_name) =
                    extract_var_name_from_expr(hir_def::ExprId::from_idx(*target), body)
                {
                    add_def(Definition::assignment(var_name, stmt_id.into_raw()));
                }
            }
            Stmt::VarDecl { bindings } if !in_tail => {
                for &binding_id in bindings.iter() {
                    let binding = body.binding_idx(binding_id);
                    add_def(Definition::var_decl(&binding.name, BindingId::from_idx(binding_id)));
                }
            }
            // The loop variable is written before the body and the next condition run, both of
            // which can raise, so it contributes even in tail position.
            Stmt::For { var, .. } => {
                let binding = body.binding_idx(*var);
                add_def(Definition::for_loop(&binding.name, BindingId::from_idx(*var)));
            }
            Stmt::ForEach { var, .. } => {
                let binding = body.binding_idx(*var);
                add_def(Definition::for_each_loop(&binding.name, BindingId::from_idx(*var)));
            }
            _ => {}
        }
    });
}

/// Visits every live protected statement of a `Попытка`, nested ones included, with whether it
/// stands in TAIL position: once it completes, nothing that can raise runs before the region
/// ends.
///
/// The handler is entered only if something inside the region raised, so what it sees is the
/// state at a point where an exception can STILL be raised. A statement therefore contributes
/// its own writes only when something that can raise runs after it while the region is still
/// open; a statement in tail position contributes nothing, because once it completes the region
/// ends normally and the handler is not entered at all. The statements after it may still run,
/// but when each of them is an assignment that cannot raise
/// ([`crate::is_non_raising_assignment`]) none of them can send control to the handler either.
///
/// `tail` says that the region ends, with nothing left that could raise, once `stmts` completes.
/// It carries into the places where that remains true - a trailing `Если` branch, the body and
/// the handler of a trailing nested `Попытка`, whose own handler ends the inner region and
/// returns control to the outer one - and is dropped where it does not: after the last statement
/// of a LOOP body the loop's own condition is evaluated, which can raise, and the loop may run
/// again.
///
/// `live` drops a statement no execution reaches. The tail rule does not subsume it: a dead
/// statement standing LAST in the protected body of a nested `Попытка` that is itself followed
/// by more region statements that can raise is not in tail position, and only liveness keeps its
/// write out of the outer handler.
fn walk_interruptible(
    stmts: &[hir_def::hir::StmtIdx],
    body: &Body,
    live: &crate::LiveStatements,
    tail: bool,
    visit: &mut impl FnMut(hir_def::hir::StmtIdx, bool),
) {
    let tail_from = stmts
        .iter()
        .rposition(|&stmt_id| !crate::is_non_raising_assignment(body, stmt_id))
        .unwrap_or(0);
    for (position, &stmt_id) in stmts.iter().enumerate() {
        if !live.contains(hir_def::StmtId::from_idx(stmt_id)) {
            continue;
        }
        let in_tail = tail && position >= tail_from;
        visit(stmt_id, in_tail);
        match body.stmt_idx(stmt_id) {
            Stmt::For { body: loop_body, .. }
            | Stmt::ForEach { body: loop_body, .. }
            | Stmt::While { body: loop_body, .. } => {
                walk_interruptible(loop_body, body, live, false, visit);
            }
            Stmt::If(if_stmt) => {
                walk_interruptible(&if_stmt.then_branch, body, live, in_tail, visit);
                for (_cond, branch) in if_stmt.elsif_branches.iter() {
                    walk_interruptible(branch, body, live, in_tail, visit);
                }
                if let Some(ref else_stmts) = if_stmt.else_branch {
                    walk_interruptible(else_stmts, body, live, in_tail, visit);
                }
            }
            Stmt::Try { body: try_body, except, .. } => {
                walk_interruptible(try_body, body, live, in_tail, visit);
                walk_interruptible(except, body, live, in_tail, visit);
            }
            _ => {}
        }
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

/// [`solve_executable_reaching_definitions`] told about the writes it cannot read: a call that
/// may write the local it is given, `Выполнить`. Such a write GENERATES its definition beside
/// whatever reached the place already, and kills nothing, so a consumer reading the definitions
/// of a use sees both the value it could follow and the fact that something else may have
/// written since.
///
/// Empty events give the plain executable solve, down to the bits.
pub fn solve_executable_reaching_definitions_with_writes(
    cfg: Arc<cfg::ControlFlowGraph>,
    body_arc: &Arc<Body>,
    seed_params: bool,
    writes: Arc<ForeignWriteEvents>,
) -> Option<Arc<ReachingDefsResult>> {
    if writes.is_empty() {
        return solve_executable_reaching_definitions(cfg, body_arc, seed_params);
    }
    let body: &Body = body_arc;
    let base = definition_index_of(body, seed_params);
    let foreign = ForeignWriteGen::new(writes, &base);
    let def_index = base.extended(foreign.definitions());
    solve_prepared(
        WritesAwareTransfer { foreign: foreign.clone() },
        def_index,
        Some(foreign),
        cfg,
        body_arc,
        seed_params,
    )
}

fn definition_index_of(body: &Body, seed_params: bool) -> Arc<DefinitionIndex> {
    let params: Vec<_> = if seed_params {
        body.params().map(|param_id| (body.binding(param_id).name.clone(), param_id)).collect()
    } else {
        Vec::new()
    };
    DefinitionIndex::from_body_with_params(body, params)
}

fn solve_with<T: Transfer<ReachingDefs>>(
    transfer: T,
    cfg: Arc<cfg::ControlFlowGraph>,
    body_arc: &Arc<Body>,
    seed_params: bool,
) -> Option<Arc<ReachingDefsResult>> {
    let def_index = definition_index_of(body_arc, seed_params);
    solve_prepared(transfer, def_index, None, cfg, body_arc, seed_params)
}

fn solve_prepared<T: Transfer<ReachingDefs>>(
    transfer: T,
    def_index: Arc<DefinitionIndex>,
    foreign: Option<ForeignWriteGen>,
    cfg: Arc<cfg::ControlFlowGraph>,
    body_arc: &Arc<Body>,
    seed_params: bool,
) -> Option<Arc<ReachingDefsResult>> {
    let body: &Body = body_arc;

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
        let mut result = ReachingDefsResult::new(dataflow_result, Arc::clone(body_arc));
        result.foreign = foreign;
        Arc::new(result)
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

    /// Present when the solve was told about writes it cannot read. The replay inside a basic
    /// block ([`ReachingDefsResult::defs_up_to_stmt`]) must generate the same definitions as
    /// the solve did, or a read would see the block's entry state only.
    foreign: Option<ForeignWriteGen>,
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
            foreign: None,
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
            if let Some(foreign) = &self.foreign {
                foreign.apply_stmt(hir_stmt_id, &mut state);
            }
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
        if let Some(foreign) = &self.foreign {
            bytes += foreign.estimated_heap();
        }
        bytes
    }
}

/// [`ExecutableReachingDefsTransfer`] that also generates the foreign definitions of a place
/// AFTER its ordinary transfer: a write performed while the statement or header expression runs
/// is seen by the next read, not by the reads of the place itself.
struct WritesAwareTransfer {
    foreign: ForeignWriteGen,
}

impl Transfer<ReachingDefs> for WritesAwareTransfer {
    fn transfer_stmt(&self, stmt_id: RawIdx, state: &ReachingDefs, body: &Body) -> ReachingDefs {
        let mut new_state = ReachingDefsTransfer.transfer_stmt(stmt_id, state, body);
        self.foreign.apply_stmt(hir_def::StmtId::from_raw(stmt_id), &mut new_state);
        new_state
    }

    fn transfer_expr(
        &self,
        expr_id: hir_def::ExprId,
        state: &ReachingDefs,
        _body: &Body,
    ) -> ReachingDefs {
        let mut new_state = state.clone();
        self.foreign.apply_header(expr_id, &mut new_state);
        new_state
    }

    fn transfer_edge(&self, edge_kind: cfg::CfgEdgeType, state: &ReachingDefs) -> ReachingDefs {
        ExecutableReachingDefsTransfer.transfer_edge(edge_kind, state)
    }

    fn transfer_exception_edge(
        &self,
        try_stmt: hir_def::StmtId,
        state: &mut ReachingDefs,
        body: &Body,
        live: &crate::LiveStatements,
    ) {
        ExecutableReachingDefsTransfer.transfer_exception_edge(try_stmt, state, body, live);
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

    fn transfer_exception_edge(
        &self,
        try_stmt: hir_def::StmtId,
        state: &mut ReachingDefs,
        body: &Body,
        live: &crate::LiveStatements,
    ) {
        ReachingDefsTransfer.transfer_exception_edge(try_stmt, state, body, live);
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

    /// Adds, without killing, the definitions the protected statements may have produced when
    /// the handler starts - [`collect_interruptible_defs`] decides which. Nothing is killed
    /// because the exception may equally have come BEFORE such a write, so what reached the
    /// `Попытка` still reaches the handler beside it.
    ///
    /// Both restrictions the collector applies are facts about the edge, not policies this
    /// transfer is free to relax: propagating state ACROSS an unexecutable edge is what the
    /// plain transfer does, inventing a write that no execution performs, or one after which
    /// the region can no longer raise, is not. So they hold here as well as in
    /// [`ExecutableReachingDefsTransfer`].
    fn transfer_exception_edge(
        &self,
        try_stmt: hir_def::StmtId,
        state: &mut ReachingDefs,
        body: &Body,
        live: &crate::LiveStatements,
    ) {
        if let Stmt::Try { body: protected, .. } = body.stmt(try_stmt) {
            collect_interruptible_defs(protected, body, live, &mut |def| state.insert(&def));
        }
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

    /// Builds one body by hand. The verdicts under test are about ORDER, so the fixture needs
    /// real statements - the ordinary transfer reads the body - and a real CFG built from them,
    /// not a hand-drawn graph whose edges could be drawn to suit the expectation.
    struct Fixture {
        body: Body,
    }

    impl Fixture {
        fn new() -> Self {
            Self { body: Body::new() }
        }

        fn path(&mut self, text: &str) -> hir_def::hir::ExprIdx {
            self.body.alloc_expr(Expr::Path(Name::new(text))).to_idx()
        }

        /// `<target> = Истина`: these tests read which writes arrive, never a value.
        fn assign(&mut self, target: &str) -> hir_def::StmtId {
            let target = self.path(target);
            let value =
                self.body.alloc_expr(Expr::Literal(hir_def::hir::Literal::Bool(true))).to_idx();
            self.body.alloc_stmt(Stmt::Assign { target, value })
        }

        /// `<callee>(<arg>)` as a statement: the place a foreign write is attributed to.
        fn call(&mut self, callee: &str, arg: &str) -> hir_def::StmtId {
            let callee = self.path(callee);
            let arg = self.path(arg);
            let call = self.body.alloc_expr(Expr::Call { callee, args: Box::new([arg]) }).to_idx();
            self.body.alloc_stmt(Stmt::Expr(call))
        }

        fn execute(&mut self, text: &str) -> hir_def::StmtId {
            let expr = self
                .body
                .alloc_expr(Expr::Literal(hir_def::hir::Literal::String(text.into())))
                .to_idx();
            self.body.alloc_stmt(Stmt::Execute { expr })
        }

        fn ret(&mut self) -> hir_def::StmtId {
            self.body.alloc_stmt(Stmt::Return { value: None })
        }

        /// `Пока <cond> Цикл <body> КонецЦикла`, returning the statement and its condition.
        fn while_loop(
            &mut self,
            cond: &str,
            stmts: &[hir_def::StmtId],
        ) -> (hir_def::StmtId, hir_def::ExprId) {
            let condition = self.path(cond);
            let body: Vec<_> = stmts.iter().map(|s| s.to_idx()).collect();
            let stmt =
                self.body.alloc_stmt(Stmt::While { condition, body: body.into_boxed_slice() });
            (stmt, hir_def::ExprId::from_idx(condition))
        }

        fn if_then(&mut self, cond: &str, stmts: &[hir_def::StmtId]) -> hir_def::StmtId {
            let condition = self.path(cond);
            let then_branch: Vec<_> = stmts.iter().map(|s| s.to_idx()).collect();
            self.body.alloc_stmt(Stmt::If(Box::new(hir_def::hir::IfStmt {
                condition,
                then_branch: then_branch.into_boxed_slice(),
                elsif_branches: Box::new([]),
                else_branch: None,
            })))
        }

        fn try_except(
            &mut self,
            protected: &[hir_def::StmtId],
            handler: &[hir_def::StmtId],
        ) -> hir_def::StmtId {
            let body: Vec<_> = protected.iter().map(|s| s.to_idx()).collect();
            let except: Vec<_> = handler.iter().map(|s| s.to_idx()).collect();
            self.body.alloc_stmt(Stmt::Try {
                body: body.into_boxed_slice(),
                except: except.into_boxed_slice(),
            })
        }

        fn finish(mut self, top_level: &[hir_def::StmtId]) -> Arc<Body> {
            let stmts: Vec<_> = top_level.iter().map(|s| s.to_idx()).collect();
            self.body.set_body_stmts(stmts.into_boxed_slice());
            Arc::new(self.body)
        }
    }

    fn solve_foreign(body: &Arc<Body>, writes: ForeignWriteEvents) -> Arc<ReachingDefsResult> {
        let cfg = cfg::CfgBuilder::new().build_graph_from_hir(body.body_stmts_typed(), body);
        solve_executable_reaching_definitions_with_writes(
            Arc::new(cfg),
            body,
            false,
            Arc::new(writes),
        )
        .expect("the solve converges")
    }

    fn foreign_kinds(defs: &[Definition]) -> Vec<ForeignWrite> {
        defs.iter()
            .filter_map(|def| match def.def_site {
                DefSite::ForeignWrite { kind, .. } => Some(kind),
                _ => None,
            })
            .collect()
    }

    fn assignment_sites(defs: &[Definition]) -> Vec<RawIdx> {
        defs.iter()
            .filter_map(|def| match def.def_site {
                DefSite::Assignment(site) => Some(site),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn foreign_write_at_a_statement_is_seen_after_it_and_not_before() {
        // х = 1; Ф(х); у = х;
        let mut fixture = Fixture::new();
        let assign = fixture.assign("х");
        let site = fixture.call("ф", "х");
        let read = fixture.assign("у");
        let body = fixture.finish(&[assign, site, read]);

        let mut writes = ForeignWriteEvents::default();
        writes.push_stmt(site, SmolStr::new("х"), ForeignWrite::ByRefArgument);
        let result = solve_foreign(&body, writes);

        let before = result.defs_for_var_at_stmt("х", site).expect("state before the call");
        assert_eq!(foreign_kinds(&before), Vec::new(), "the call's own write is not read by it");

        let after = result.defs_for_var_at_stmt("х", read).expect("state before the read");
        assert_eq!(foreign_kinds(&after), vec![ForeignWrite::ByRefArgument]);
        assert_eq!(
            assignment_sites(&after),
            vec![assign.into_raw()],
            "a write that MAY happen kills nothing"
        );
    }

    #[test]
    fn foreign_write_round_a_loop_is_seen_inside_it_and_not_before() {
        // х = 1; в = х; Пока у Цикл г = х; Ф(х); КонецЦикла;
        let mut fixture = Fixture::new();
        let assign = fixture.assign("х");
        let before_loop = fixture.assign("в");
        let in_loop = fixture.assign("г");
        let site = fixture.call("ф", "х");
        let (loop_stmt, _) = fixture.while_loop("у", &[in_loop, site]);
        let body = fixture.finish(&[assign, before_loop, loop_stmt]);

        let mut writes = ForeignWriteEvents::default();
        writes.push_stmt(site, SmolStr::new("х"), ForeignWrite::ByRefArgument);
        let result = solve_foreign(&body, writes);

        let outside = result.defs_for_var_at_stmt("х", before_loop).expect("state before the loop");
        assert_eq!(foreign_kinds(&outside), Vec::new());

        let inside = result.defs_for_var_at_stmt("х", in_loop).expect("state inside the loop");
        assert_eq!(
            foreign_kinds(&inside),
            vec![ForeignWrite::ByRefArgument],
            "the back edge carries the write to a read that precedes it in the text"
        );
    }

    /// The control for [`foreign_write_in_dead_code_reaches_nothing`]: one statement apart, the
    /// same write does reach the read.
    #[test]
    fn foreign_write_in_a_live_branch_reaches_the_read_after_it() {
        let mut fixture = Fixture::new();
        let assign = fixture.assign("х");
        let site = fixture.call("ф", "х");
        let branch = fixture.if_then("у", &[site]);
        let read = fixture.assign("з");
        let body = fixture.finish(&[assign, branch, read]);

        let mut writes = ForeignWriteEvents::default();
        writes.push_stmt(site, SmolStr::new("х"), ForeignWrite::ByRefArgument);
        let result = solve_foreign(&body, writes);

        let defs = result.defs_for_var_at_stmt("х", read).expect("state before the read");
        assert_eq!(foreign_kinds(&defs), vec![ForeignWrite::ByRefArgument]);
    }

    #[test]
    fn foreign_write_in_dead_code_reaches_nothing() {
        // х = 1; Если у Тогда Возврат; Ф(х); КонецЕсли; з = 2;
        let mut fixture = Fixture::new();
        let assign = fixture.assign("х");
        let ret = fixture.ret();
        let site = fixture.call("ф", "х");
        let branch = fixture.if_then("у", &[ret, site]);
        let read = fixture.assign("з");
        let body = fixture.finish(&[assign, branch, read]);

        let mut writes = ForeignWriteEvents::default();
        writes.push_stmt(site, SmolStr::new("х"), ForeignWrite::ByRefArgument);
        let result = solve_foreign(&body, writes);

        let defs = result.defs_for_var_at_stmt("х", read).expect("state before the read");
        assert_eq!(
            foreign_kinds(&defs),
            Vec::new(),
            "nothing executes the call, so nothing can be written there"
        );
        assert_eq!(assignment_sites(&defs), vec![assign.into_raw()]);
    }

    #[test]
    fn execute_adds_a_foreign_definition_beside_the_assignment() {
        // х = 1; Выполнить("…"); у = 2;
        let mut fixture = Fixture::new();
        let assign = fixture.assign("х");
        let site = fixture.execute("х = 2");
        let read = fixture.assign("у");
        let body = fixture.finish(&[assign, site, read]);

        let mut writes = ForeignWriteEvents::default();
        writes.push_every_name(site);
        let result = solve_foreign(&body, writes);

        let defs = result.defs_for_var_at_stmt("х", read).expect("state before the read");
        assert_eq!(foreign_kinds(&defs), vec![ForeignWrite::Execute]);
        assert_eq!(assignment_sites(&defs), vec![assign.into_raw()]);
    }

    /// The handler is reached from the `Попытка` vertex, BEFORE the protected statements, so the
    /// definition that reached the statement still reaches the handler - the earlier value is
    /// what the handler may see - and the write among the protected statements does not.
    #[test]
    fn a_foreign_write_among_protected_statements_leaves_the_handlers_read_its_earlier_definition()
    {
        // х = 1; Попытка Ф(х); Исключение у = 2; КонецПопытки;
        let mut fixture = Fixture::new();
        let assign = fixture.assign("х");
        let site = fixture.call("ф", "х");
        let handler_read = fixture.assign("у");
        let try_stmt = fixture.try_except(&[site], &[handler_read]);
        let body = fixture.finish(&[assign, try_stmt]);

        let mut writes = ForeignWriteEvents::default();
        writes.push_stmt(site, SmolStr::new("х"), ForeignWrite::ByRefArgument);
        let result = solve_foreign(&body, writes);

        let defs = result.defs_for_var_at_stmt("х", handler_read).expect("state in the handler");
        assert_eq!(
            assignment_sites(&defs),
            vec![assign.into_raw()],
            "the earlier definition is not lost"
        );
        assert_eq!(
            foreign_kinds(&defs),
            Vec::new(),
            "an error can interrupt before the call, so its write is not on the handler's path"
        );
    }

    #[test]
    fn a_write_in_an_if_condition_is_seen_after_the_branch_and_not_by_the_condition() {
        // х = Истина; Если Ф(х) Тогда г = Истина; КонецЕсли; з = Истина;
        let mut fixture = Fixture::new();
        let assign = fixture.assign("х");
        let in_branch = fixture.assign("г");
        let callee = fixture.path("ф");
        let arg = fixture.path("х");
        let call = fixture.body.alloc_expr(Expr::Call { callee, args: Box::new([arg]) });
        let branch = fixture.body.alloc_stmt(Stmt::If(Box::new(hir_def::hir::IfStmt {
            condition: call.to_idx(),
            then_branch: Box::new([in_branch.to_idx()]),
            elsif_branches: Box::new([]),
            else_branch: None,
        })));
        let read = fixture.assign("з");
        let body = fixture.finish(&[assign, branch, read]);

        let mut writes = ForeignWriteEvents::default();
        writes.push_header(call, branch, SmolStr::new("х"), ForeignWrite::UnattestedArgument);
        let result = solve_foreign(&body, writes);

        let at_header = result.defs_for_var_at_header("х", call).expect("state at the header");
        assert_eq!(foreign_kinds(&at_header), Vec::new(), "the condition reads before it writes");

        let after = result.defs_for_var_at_stmt("х", read).expect("state after the branch");
        assert_eq!(foreign_kinds(&after), vec![ForeignWrite::UnattestedArgument]);
    }

    /// The control for the test above, one statement apart: a LOOP condition is evaluated again
    /// after its own write, so the write of the previous pass is among the definitions the
    /// condition reads. That is the answer the body-wide scan this replaced could not give at
    /// all - it had no notion of a second pass.
    #[test]
    fn a_write_in_a_loop_condition_is_seen_by_the_condition_on_a_later_pass() {
        // х = Истина; Пока Ф(х) Цикл г = Истина; КонецЦикла;
        let mut fixture = Fixture::new();
        let assign = fixture.assign("х");
        let in_loop = fixture.assign("г");
        let callee = fixture.path("ф");
        let arg = fixture.path("х");
        let call = fixture.body.alloc_expr(Expr::Call { callee, args: Box::new([arg]) });
        let loop_stmt = fixture.body.alloc_stmt(Stmt::While {
            condition: call.to_idx(),
            body: Box::new([in_loop.to_idx()]),
        });
        let body = fixture.finish(&[assign, loop_stmt]);

        let mut writes = ForeignWriteEvents::default();
        writes.push_header(call, loop_stmt, SmolStr::new("х"), ForeignWrite::UnattestedArgument);
        let result = solve_foreign(&body, writes);

        let at_header = result.defs_for_var_at_header("х", call).expect("state at the header");
        assert_eq!(foreign_kinds(&at_header), vec![ForeignWrite::UnattestedArgument]);

        let in_body = result.defs_for_var_at_stmt("х", in_loop).expect("state inside the loop");
        assert_eq!(foreign_kinds(&in_body), vec![ForeignWrite::UnattestedArgument]);
    }

    #[test]
    fn empty_events_give_the_plain_executable_solve() {
        let mut fixture = Fixture::new();
        let assign = fixture.assign("х");
        let read = fixture.assign("у");
        let body = fixture.finish(&[assign, read]);

        let cfg =
            Arc::new(cfg::CfgBuilder::new().build_graph_from_hir(body.body_stmts_typed(), &body));
        let plain = solve_executable_reaching_definitions(cfg.clone(), &body, false)
            .expect("the solve converges");
        let with_none = solve_executable_reaching_definitions_with_writes(
            cfg,
            &body,
            false,
            Arc::new(ForeignWriteEvents::default()),
        )
        .expect("the solve converges");

        assert_eq!(plain, with_none);
    }

    /// The case the liveness filter still catches once the tail rule is in place: a dead write
    /// standing LAST in the protected body of a nested `Попытка` that is itself followed by more
    /// region statements. It is not in tail position of the outer region - the call after
    /// `КонецПопытки` can raise - so only liveness keeps it out of the outer handler. The live
    /// write of the same inner body does reach it.
    #[test]
    fn a_dead_write_last_in_an_inner_protected_body_does_not_reach_the_outer_handler() {
        // Попытка Попытка х = Истина; Возврат; д = Истина; Исключение г = Истина; КонецПопытки;
        // Ф(е); Исключение у = Истина; КонецПопытки;
        let mut fixture = Fixture::new();
        let live_write = fixture.assign("х");
        let ret = fixture.ret();
        let dead_write = fixture.assign("д");
        let inner_handler = fixture.assign("г");
        let inner_try = fixture.try_except(&[live_write, ret, dead_write], &[inner_handler]);
        let after_inner = fixture.call("ф", "е");
        let outer_handler_read = fixture.assign("у");
        let outer_try = fixture.try_except(&[inner_try, after_inner], &[outer_handler_read]);
        let body = fixture.finish(&[outer_try]);

        let result = solve_plain(&body);

        let live = result
            .defs_for_var_at_stmt("х", outer_handler_read)
            .expect("state in the outer handler");
        assert_eq!(
            assignment_sites(&live),
            vec![live_write.into_raw()],
            "the live write of the inner body may have happened before the outer handler"
        );

        let dead = result
            .defs_for_var_at_stmt("д", outer_handler_read)
            .expect("state in the outer handler");
        assert_eq!(
            assignment_sites(&dead),
            Vec::new(),
            "nothing executes a write after Возврат, whatever its position in the region"
        );
    }

    /// The handler is entered only when the region raised, so a write after which the region
    /// can no longer raise is not on any path into it: once the LAST protected statement
    /// completes, the region ends normally and the handler is skipped. The write before it is
    /// on such a path, because the call between them can raise.
    #[test]
    fn a_write_in_tail_position_does_not_reach_the_handler_but_one_before_it_does() {
        // Попытка х = Истина; Ф(з); г = Истина; Исключение у = Истина; КонецПопытки;
        let mut fixture = Fixture::new();
        let before_tail = fixture.assign("х");
        let raising = fixture.call("ф", "з");
        let tail_write = fixture.assign("г");
        let handler_read = fixture.assign("у");
        let try_stmt = fixture.try_except(&[before_tail, raising, tail_write], &[handler_read]);
        let body = fixture.finish(&[try_stmt]);

        let result = solve_plain(&body);

        let before = result.defs_for_var_at_stmt("х", handler_read).expect("state in the handler");
        assert_eq!(
            assignment_sites(&before),
            vec![before_tail.into_raw()],
            "the call after it can raise, so the write before it may have happened"
        );

        let tail = result.defs_for_var_at_stmt("г", handler_read).expect("state in the handler");
        assert_eq!(
            assignment_sites(&tail),
            Vec::new(),
            "once the last protected statement completes, the region ends and the handler is              not entered"
        );
    }

    /// An assignment of a literal cannot raise, so a region that ends in such assignments ends
    /// normally once the write before them completes: neither write is on a path into the
    /// handler. The test above is the control - a call in between can raise, and the first write
    /// then does reach.
    #[test]
    fn writes_followed_only_by_assignments_that_cannot_raise_do_not_reach_the_handler() {
        // Попытка х = Истина; г = Истина; Исключение у = Истина; КонецПопытки;
        let mut fixture = Fixture::new();
        let first = fixture.assign("х");
        let second = fixture.assign("г");
        let handler_read = fixture.assign("у");
        let try_stmt = fixture.try_except(&[first, second], &[handler_read]);
        let body = fixture.finish(&[try_stmt]);

        let result = solve_plain(&body);

        for name in ["х", "г"] {
            let defs =
                result.defs_for_var_at_stmt(name, handler_read).expect("state in the handler");
            assert_eq!(assignment_sites(&defs), Vec::new(), "{name}: nothing after it can raise");
        }
    }

    /// A trailing LOOP is not a tail: after the last statement of its body the loop's own
    /// condition is evaluated, which can raise, and the loop may run again.
    #[test]
    fn a_write_in_the_body_of_a_trailing_loop_reaches_the_handler() {
        // Попытка Пока у Цикл х = Истина; КонецЦикла; Исключение з = Истина; КонецПопытки;
        let mut fixture = Fixture::new();
        let in_loop = fixture.assign("х");
        let (loop_stmt, _) = fixture.while_loop("у", &[in_loop]);
        let handler_read = fixture.assign("з");
        let try_stmt = fixture.try_except(&[loop_stmt], &[handler_read]);
        let body = fixture.finish(&[try_stmt]);

        let result = solve_plain(&body);
        let defs = result.defs_for_var_at_stmt("х", handler_read).expect("state in the handler");

        assert_eq!(
            assignment_sites(&defs),
            vec![in_loop.into_raw()],
            "the condition after the loop body can raise, so the write may have happened"
        );
    }

    /// The PLAIN solve, which propagates state across an unexecutable edge instead of dropping
    /// it: the strictest setting for the liveness filter of the `Exception` edge, since nothing
    /// else here removes a dead statement's contribution.
    fn solve_plain(body: &Arc<Body>) -> Arc<ReachingDefsResult> {
        let cfg = cfg::CfgBuilder::new().build_graph_from_hir(body.body_stmts_typed(), body);
        solve_reaching_definitions(Arc::new(cfg), body, false).expect("the solve converges")
    }

    /// Raising an exception after a write requires reaching that write. The live write of the
    /// protected body is therefore on the handler's path and the write after `Возврат` is not,
    /// though both stand in the same protected body and write the same local.
    #[test]
    fn a_dead_write_in_the_protected_body_does_not_reach_the_handler_but_a_live_one_does() {
        // Попытка х = Истина; Возврат; х = Истина; Исключение у = Истина; КонецПопытки;
        let mut fixture = Fixture::new();
        let live_write = fixture.assign("х");
        let ret = fixture.ret();
        let dead_write = fixture.assign("х");
        let handler_read = fixture.assign("у");
        let try_stmt = fixture.try_except(&[live_write, ret, dead_write], &[handler_read]);
        let body = fixture.finish(&[try_stmt]);

        let result = solve_plain(&body);
        let defs = result.defs_for_var_at_stmt("х", handler_read).expect("state in the handler");
        let sites = assignment_sites(&defs);

        assert!(
            sites.contains(&live_write.into_raw()),
            "an exception can interrupt the protected body after the live write: {sites:?}"
        );
        assert!(
            !sites.contains(&dead_write.into_raw()),
            "nothing executes the write after Возврат, so no handler can see it: {sites:?}"
        );
    }

    fn solve_executable(body: &Arc<Body>) -> Arc<ReachingDefsResult> {
        let cfg = cfg::CfgBuilder::new().build_graph_from_hir(body.body_stmts_typed(), body);
        solve_executable_reaching_definitions(Arc::new(cfg), body, false)
            .expect("the solve converges")
    }

    /// The handler is entered from the `Попытка` vertex, before any protected statement runs, so
    /// the graph alone would say the handler sees only what reached the `Попытка`. An exception
    /// can interrupt the protected statements anywhere, so a write among them may have happened
    /// by then - and may equally not have happened, which is why BOTH definitions reach.
    #[test]
    fn a_write_among_protected_statements_reaches_the_handler_beside_the_earlier_definition() {
        // х = Истина; Попытка х = Истина; Ф(г); Исключение у = Истина; КонецПопытки;
        let mut fixture = Fixture::new();
        let before_try = fixture.assign("х");
        let inside_try = fixture.assign("х");
        let after_write = fixture.call("ф", "г");
        let handler_read = fixture.assign("у");
        let try_stmt = fixture.try_except(&[inside_try, after_write], &[handler_read]);
        let body = fixture.finish(&[before_try, try_stmt]);

        let result = solve_executable(&body);
        let defs = result.defs_for_var_at_stmt("х", handler_read).expect("state in the handler");
        let sites = assignment_sites(&defs);

        assert_eq!(
            sites.len(),
            2,
            "both the interrupted write and the earlier one reach: {sites:?}"
        );
        assert!(
            sites.contains(&inside_try.into_raw()),
            "the write among the protected statements may have happened"
        );
        assert!(
            sites.contains(&before_try.into_raw()),
            "the exception may have come before that write"
        );
    }

    /// The code after `КонецПопытки` is fed by the end of the protected body and by the end of
    /// the handler. A write the protected body OVERWRITES later leaves the first path, so it can
    /// only arrive through the handler - which is the regression this carries: without the write
    /// on the exception edge, a value written and overwritten inside a `Попытка` reaches nothing.
    #[test]
    fn a_write_overwritten_later_in_the_protected_body_reaches_the_code_after_it() {
        // х = Истина; Попытка х = Истина; Ф(з); х = Истина; Исключение г = Истина; КонецПопытки;
        // у = Истина;
        let mut fixture = Fixture::new();
        let before_try = fixture.assign("х");
        let interrupted = fixture.assign("х");
        let raising = fixture.call("ф", "з");
        let overwrite = fixture.assign("х");
        let handler_stmt = fixture.assign("г");
        let try_stmt = fixture.try_except(&[interrupted, raising, overwrite], &[handler_stmt]);
        let read_after = fixture.assign("у");
        let body = fixture.finish(&[before_try, try_stmt, read_after]);

        let result = solve_executable(&body);
        let defs = result.defs_for_var_at_stmt("х", read_after).expect("state after the Попытка");
        let sites = assignment_sites(&defs);

        assert!(
            sites.contains(&interrupted.into_raw()),
            "the overwritten write survives through the handler: {sites:?}"
        );
        assert!(sites.contains(&overwrite.into_raw()), "the uninterrupted path reaches too");
    }

    /// The control for the two above: the union is of the PROTECTED statements, not of the body.
    /// A write placed after `КонецПопытки` cannot have run when the handler starts.
    #[test]
    fn a_write_after_the_popytka_is_not_seen_inside_the_handler() {
        // Попытка г = Истина; Исключение у = Истина; КонецПопытки; х = Истина;
        let mut fixture = Fixture::new();
        let inside_try = fixture.assign("г");
        let handler_read = fixture.assign("у");
        let try_stmt = fixture.try_except(&[inside_try], &[handler_read]);
        let after_try = fixture.assign("х");
        let body = fixture.finish(&[try_stmt, after_try]);

        let result = solve_executable(&body);
        let defs = result.defs_for_var_at_stmt("х", handler_read).expect("state in the handler");

        assert_eq!(
            assignment_sites(&defs),
            Vec::new(),
            "the handler runs before the text that follows the Попытка"
        );
    }

    /// The statements of a nested `Попытка` are protected by the outer handler as well: an
    /// exception raised there and not caught inside propagates outwards. The inner write needs
    /// something after it that can still raise - here the call following the inner
    /// `КонецПопытки` - or it would stand in tail position of the outer region and
    /// contribute nothing, which is what the single-statement region of `security_balanced`
    /// records.
    #[test]
    fn a_write_in_an_inner_protected_body_reaches_the_outer_handler() {
        // х = Истина; Попытка Попытка х = Истина; Исключение г = Истина; КонецПопытки;
        // Ф(д); Исключение у = Истина; КонецПопытки;
        let mut fixture = Fixture::new();
        let before_try = fixture.assign("х");
        let inner_write = fixture.assign("х");
        let inner_handler = fixture.assign("г");
        let inner_try = fixture.try_except(&[inner_write], &[inner_handler]);
        let after_inner = fixture.call("ф", "д");
        let outer_handler_read = fixture.assign("у");
        let outer_try = fixture.try_except(&[inner_try, after_inner], &[outer_handler_read]);
        let body = fixture.finish(&[before_try, outer_try]);

        let result = solve_executable(&body);
        let defs =
            result.defs_for_var_at_stmt("х", outer_handler_read).expect("state in the handler");
        let sites = assignment_sites(&defs);

        assert!(
            sites.contains(&inner_write.into_raw()),
            "the inner write may have happened before the outer handler: {sites:?}"
        );
        assert!(sites.contains(&before_try.into_raw()), "the earlier definition is not lost");
    }
}
