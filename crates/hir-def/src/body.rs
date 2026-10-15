pub mod lower;

use intern::NormName;
use la_arena::Arena;
use rustc_hash::{FxHashMap, FxHashSet};
use std::sync::Arc;
use stdx::case::CaseExt;
use syntax::SyntaxNode;
use text_size::TextRange;

use crate::hir::{Binding, BindingIdx, Expr, ExprIdx, Stmt, StmtIdx};
use crate::Name;

use cfg_types::{BindingId, ExprId, IdConversion, LocalRange, MethodOffset, StmtId};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Body {
    pub(crate) exprs: Arena<Expr>,
    pub(crate) stmts: Arena<Stmt>,
    pub(crate) bindings: Arena<Binding>,
    pub(crate) params: Box<[BindingIdx]>,
    pub(crate) body_stmts: Box<[StmtIdx]>,

    pub(crate) sdbl_exprs: Vec<(ExprIdx, LocalRange, syntax::SdblQuery)>,

    pub(crate) recovered_exprs: FxHashSet<ExprIdx>,
}

impl Default for Body {
    fn default() -> Self {
        Self::new()
    }
}

impl Body {
    pub fn new() -> Self {
        Self {
            exprs: Arena::new(),
            stmts: Arena::new(),
            bindings: Arena::new(),
            params: Box::new([]),
            body_stmts: Box::new([]),
            sdbl_exprs: Vec::new(),
            recovered_exprs: FxHashSet::default(),
        }
    }

    #[doc(hidden)]
    pub fn alloc_expr(&mut self, expr: Expr) -> ExprId {
        ExprId::from_idx(self.exprs.alloc(expr))
    }

    #[doc(hidden)]
    pub fn alloc_stmt(&mut self, stmt: Stmt) -> StmtId {
        StmtId::from_idx(self.stmts.alloc(stmt))
    }

    pub fn expr(&self, id: ExprId) -> &Expr {
        let typed_id: ExprIdx = id.to_idx();
        &self.exprs[typed_id]
    }

    pub fn stmt(&self, id: StmtId) -> &Stmt {
        let typed_id: StmtIdx = id.to_idx();
        &self.stmts[typed_id]
    }

    pub fn binding(&self, id: BindingId) -> &Binding {
        let typed_id: BindingIdx = id.to_idx();
        &self.bindings[typed_id]
    }

    pub fn expr_idx(&self, id: ExprIdx) -> &Expr {
        &self.exprs[id]
    }

    pub fn stmt_idx(&self, id: StmtIdx) -> &Stmt {
        &self.stmts[id]
    }

    pub fn binding_idx(&self, id: BindingIdx) -> &Binding {
        &self.bindings[id]
    }

    /// Approximate live heap bytes of this body for Salsa's `memory_usage`
    /// report — see [`body_heap`]. Exposed for downstream crates (dataflow) whose
    /// memoised results own a cloned [`Body`].
    pub fn estimated_heap(&self) -> usize {
        body_heap(self)
    }

    pub fn expr_count(&self) -> usize {
        self.exprs.len()
    }

    pub fn stmt_count(&self) -> usize {
        self.stmts.len()
    }

    pub fn binding_count(&self) -> usize {
        self.bindings.len()
    }

    pub fn body_stmts_typed(&self) -> &[StmtIdx] {
        &self.body_stmts
    }

    pub fn exprs_iter(
        &self,
    ) -> impl ExactSizeIterator<Item = (ExprId, &Expr)> + DoubleEndedIterator + Clone {
        self.exprs.iter().map(|(idx, expr)| (ExprId::from_idx(idx), expr))
    }

    pub fn stmts_iter(
        &self,
    ) -> impl ExactSizeIterator<Item = (StmtId, &Stmt)> + DoubleEndedIterator + Clone {
        self.stmts.iter().map(|(idx, stmt)| (StmtId::from_idx(idx), stmt))
    }

    pub fn bindings_iter(
        &self,
    ) -> impl ExactSizeIterator<Item = (BindingId, &Binding)> + DoubleEndedIterator + Clone {
        self.bindings.iter().map(|(idx, binding)| (BindingId::from_idx(idx), binding))
    }

    /// Query literals of the body with the literal's range, method-relative.
    pub fn sdbl_exprs(&self) -> impl Iterator<Item = (ExprId, LocalRange, &syntax::SdblQuery)> {
        self.sdbl_exprs.iter().map(|(idx, range, query)| (ExprId::from_idx(*idx), *range, query))
    }

    pub fn enclosing_stmt(&self, target: ExprId) -> Option<StmtId> {
        let target_idx: ExprIdx = target.to_idx();
        for (stmt_id, stmt) in self.stmts_iter() {
            if stmt_owns_expr(self, stmt, target_idx) {
                return Some(stmt_id);
            }
        }
        None
    }

    pub fn is_recovered(&self, id: ExprId) -> bool {
        let typed_id: ExprIdx = id.to_idx();
        self.recovered_exprs.contains(&typed_id)
    }

    pub fn params(&self) -> impl Iterator<Item = BindingId> + '_ {
        self.params.iter().map(|&idx| BindingId::from_idx(idx))
    }

    pub fn body_stmts(&self) -> impl Iterator<Item = StmtId> + '_ {
        self.body_stmts.iter().map(|&idx| StmtId::from_idx(idx))
    }

    #[doc(hidden)]
    pub fn exprs_mut(&mut self) -> &mut Arena<Expr> {
        &mut self.exprs
    }

    #[doc(hidden)]
    pub fn stmts_mut(&mut self) -> &mut Arena<Stmt> {
        &mut self.stmts
    }

    #[doc(hidden)]
    pub fn bindings_mut(&mut self) -> &mut Arena<Binding> {
        &mut self.bindings
    }

    #[doc(hidden)]
    pub fn set_body_stmts(&mut self, stmts: Box<[StmtIdx]>) {
        self.body_stmts = stmts;
    }

    #[doc(hidden)]
    pub fn set_params(&mut self, params: Box<[BindingIdx]>) {
        self.params = params;
    }
}

fn stmt_owns_expr(body: &Body, stmt: &Stmt, target: ExprIdx) -> bool {
    match stmt {
        Stmt::Expr(e) => expr_contains(body, *e, target),
        Stmt::Assign { target: lhs, value } => {
            expr_contains(body, *lhs, target) || expr_contains(body, *value, target)
        }
        Stmt::Return { value } | Stmt::Raise { value } => {
            value.as_ref().is_some_and(|v| expr_contains(body, *v, target))
        }
        Stmt::Execute { expr } => expr_contains(body, *expr, target),
        Stmt::AddHandler { event, handler } | Stmt::RemoveHandler { event, handler } => {
            expr_contains(body, *event, target) || expr_contains(body, *handler, target)
        }
        Stmt::If(if_stmt) => {
            expr_contains(body, if_stmt.condition, target)
                || if_stmt.elsif_branches.iter().any(|(cond, _)| expr_contains(body, *cond, target))
        }
        Stmt::While { condition, body: _ } => expr_contains(body, *condition, target),
        Stmt::For { from, to, body: _, var: _ } => {
            expr_contains(body, *from, target) || expr_contains(body, *to, target)
        }
        Stmt::ForEach { collection, body: _, var: _ } => expr_contains(body, *collection, target),
        Stmt::PreprocIf(_)
        | Stmt::Try { .. }
        | Stmt::VarDecl { .. }
        | Stmt::Break
        | Stmt::Continue
        | Stmt::Goto(_)
        | Stmt::Label(_) => false,
    }
}

fn expr_contains(body: &Body, root: ExprIdx, target: ExprIdx) -> bool {
    if root == target {
        return true;
    }
    match body.expr_idx(root) {
        Expr::Missing | Expr::Path(_) | Expr::Literal(_) => false,
        Expr::BinaryOp { lhs, rhs, .. } => {
            expr_contains(body, *lhs, target) || expr_contains(body, *rhs, target)
        }
        Expr::UnaryOp { expr, .. } => expr_contains(body, *expr, target),
        Expr::Ternary { condition, then_expr, else_expr } => {
            expr_contains(body, *condition, target)
                || expr_contains(body, *then_expr, target)
                || expr_contains(body, *else_expr, target)
        }
        Expr::Call { callee, args } => {
            expr_contains(body, *callee, target)
                || args.iter().any(|a| expr_contains(body, *a, target))
        }
        Expr::MethodCall { receiver, args, .. } => {
            expr_contains(body, *receiver, target)
                || args.iter().any(|a| expr_contains(body, *a, target))
        }
        Expr::Index { base, index } => {
            expr_contains(body, *base, target) || expr_contains(body, *index, target)
        }
        Expr::Field { base, .. } => expr_contains(body, *base, target),
        Expr::New { args, .. } => args.iter().any(|a| expr_contains(body, *a, target)),
        Expr::Array(elems) => elems.iter().any(|e| expr_contains(body, *e, target)),
        Expr::Await { expr } => expr_contains(body, *expr, target),
    }
}

/// Ranges of the lowered nodes, relative to the method's own root. A file
/// position enters or leaves only through [`SourceMapAt`].
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct BodySourceMap {
    expr_ranges: Vec<Option<LocalRange>>,
    stmt_ranges: Vec<Option<LocalRange>>,
    binding_ranges: Vec<Option<LocalRange>>,

    range_to_expr: FxHashMap<LocalRange, ExprId>,
    range_to_stmt: FxHashMap<LocalRange, StmtId>,
    range_to_binding: FxHashMap<LocalRange, BindingId>,
}

impl BodySourceMap {
    pub fn new() -> Self {
        Self::default()
    }

    pub(crate) fn record_expr(&mut self, id: ExprIdx, range: LocalRange) {
        let opaque_id = ExprId::from_idx(id);
        let idx = id.into_raw().into_u32() as usize;
        if idx >= self.expr_ranges.len() {
            self.expr_ranges.resize(idx + 1, None);
        }
        self.expr_ranges[idx] = Some(range);
        self.range_to_expr.insert(range, opaque_id);
    }

    pub(crate) fn record_stmt(&mut self, id: StmtIdx, range: LocalRange) {
        let opaque_id = StmtId::from_idx(id);
        let idx = id.into_raw().into_u32() as usize;
        if idx >= self.stmt_ranges.len() {
            self.stmt_ranges.resize(idx + 1, None);
        }
        self.stmt_ranges[idx] = Some(range);
        self.range_to_stmt.insert(range, opaque_id);
    }

    pub(crate) fn record_binding(&mut self, id: BindingIdx, range: LocalRange) {
        let opaque_id = BindingId::from_idx(id);
        let idx = id.into_raw().into_u32() as usize;
        if idx >= self.binding_ranges.len() {
            self.binding_ranges.resize(idx + 1, None);
        }
        self.binding_ranges[idx] = Some(range);
        self.range_to_binding.insert(range, opaque_id);
    }

    pub fn expr_range(&self, id: ExprId) -> Option<LocalRange> {
        let idx = id.into_raw().into_u32() as usize;
        self.expr_ranges.get(idx).copied().flatten()
    }

    pub fn stmt_range(&self, id: StmtId) -> Option<LocalRange> {
        let idx = id.into_raw().into_u32() as usize;
        self.stmt_ranges.get(idx).copied().flatten()
    }

    pub fn binding_range(&self, id: BindingId) -> Option<LocalRange> {
        let idx = id.into_raw().into_u32() as usize;
        self.binding_ranges.get(idx).copied().flatten()
    }

    pub fn expr_at_range(&self, range: LocalRange) -> Option<ExprId> {
        self.range_to_expr.get(&range).copied()
    }

    pub fn stmt_at_range(&self, range: LocalRange) -> Option<StmtId> {
        self.range_to_stmt.get(&range).copied()
    }

    pub fn binding_at_range(&self, range: LocalRange) -> Option<BindingId> {
        self.range_to_binding.get(&range).copied()
    }
}

/// A body's source map placed in its file: the lift and the lookup are the
/// two directions of the same [`MethodOffset`].
#[derive(Debug, Clone, Copy)]
pub struct SourceMapAt<'a> {
    map: &'a BodySourceMap,
    base: MethodOffset,
}

impl<'a> SourceMapAt<'a> {
    pub fn new(map: &'a BodySourceMap, base: MethodOffset) -> Self {
        Self { map, base }
    }

    pub fn local(&self) -> &'a BodySourceMap {
        self.map
    }

    pub fn base(&self) -> MethodOffset {
        self.base
    }

    pub fn expr_range(&self, id: ExprId) -> Option<TextRange> {
        self.map.expr_range(id).map(|r| r.lift(self.base))
    }

    pub fn stmt_range(&self, id: StmtId) -> Option<TextRange> {
        self.map.stmt_range(id).map(|r| r.lift(self.base))
    }

    pub fn binding_range(&self, id: BindingId) -> Option<TextRange> {
        self.map.binding_range(id).map(|r| r.lift(self.base))
    }

    pub fn expr_at_range(&self, range: TextRange) -> Option<ExprId> {
        self.map.expr_at_range(self.base.lower(range)?)
    }

    pub fn stmt_at_range(&self, range: TextRange) -> Option<StmtId> {
        self.map.stmt_at_range(self.base.lower(range)?)
    }

    pub fn binding_at_range(&self, range: TextRange) -> Option<BindingId> {
        self.map.binding_at_range(self.base.lower(range)?)
    }
}

/// Rough live bytes of a lowered [`Body`] for Salsa's `memory_usage`
/// introspection: the expr/stmt/binding arenas plus every owned payload they
/// point at — names, string/date literals, boxed argument and branch index
/// lists — and the body-level index boxes, SDBL side table, and recovered set.
/// Element-count granularity (spare arena/box capacity is ignored), so the
/// figure tracks live content within a small factor.
pub(crate) fn body_heap(body: &Body) -> usize {
    use std::mem::size_of;

    use crate::heap_estimate::{map_table_bytes, name_bytes, vec_bytes};
    use crate::hir::{Expr, IfStmt, Literal, PreprocIfStmt};

    let mut bytes = size_of::<Body>();

    // Arena backing stores: one element slot per allocated node.
    bytes += vec_bytes::<Expr>(body.exprs.len());
    bytes += vec_bytes::<Stmt>(body.stmts.len());
    bytes += vec_bytes::<Binding>(body.bindings.len());

    // Body-level boxed index lists and side tables.
    bytes += vec_bytes::<BindingIdx>(body.params.len());
    bytes += vec_bytes::<StmtIdx>(body.body_stmts.len());
    bytes += vec_bytes::<(ExprIdx, LocalRange, syntax::SdblQuery)>(body.sdbl_exprs.len());
    bytes += map_table_bytes::<ExprIdx, ()>(body.recovered_exprs.len());

    for binding in body.bindings.values() {
        bytes += name_bytes(&binding.name);
    }

    // Per-expression owned heap: names, string/date literals, boxed argument
    // lists, and the qualified-path box. Index-only variants own nothing extra.
    for expr in body.exprs.values() {
        bytes += match expr {
            Expr::Literal(Literal::String(s)) | Expr::Literal(Literal::Date(s)) => s.capacity(),
            Expr::Path(name) => name_bytes(name),
            Expr::Field { field, .. } => name_bytes(field),
            Expr::MethodCall { method, args, .. } => {
                name_bytes(method) + vec_bytes::<ExprIdx>(args.len())
            }
            Expr::New { type_name, args } => {
                type_name.as_ref().map_or(0, name_bytes) + vec_bytes::<ExprIdx>(args.len())
            }
            Expr::Call { args, .. } | Expr::Array(args) => vec_bytes::<ExprIdx>(args.len()),
            _ => 0,
        };
    }

    // Per-statement owned heap: the boxed branch/loop index lists and the
    // separately-allocated `If`/`PreprocIf` nodes (the arena element holds only
    // a `Box` pointer to them).
    for stmt in body.stmts.values() {
        bytes += match stmt {
            Stmt::VarDecl { bindings } => vec_bytes::<BindingIdx>(bindings.len()),
            Stmt::If(if_stmt) => {
                size_of::<IfStmt>()
                    + vec_bytes::<StmtIdx>(if_stmt.then_branch.len())
                    + vec_bytes::<(ExprIdx, Box<[StmtIdx]>)>(if_stmt.elsif_branches.len())
                    + if_stmt
                        .elsif_branches
                        .iter()
                        .map(|(_, branch)| vec_bytes::<StmtIdx>(branch.len()))
                        .sum::<usize>()
                    + if_stmt.else_branch.as_ref().map_or(0, |b| vec_bytes::<StmtIdx>(b.len()))
            }
            Stmt::PreprocIf(pre) => {
                size_of::<PreprocIfStmt>()
                    + vec_bytes::<StmtIdx>(pre.then_branch.len())
                    + vec_bytes::<(TextRange, TextRange, Box<[StmtIdx]>)>(pre.elsif_branches.len())
                    + pre
                        .elsif_branches
                        .iter()
                        .map(|branch| vec_bytes::<StmtIdx>(branch.2.len()))
                        .sum::<usize>()
                    + pre.else_branch.as_ref().map_or(0, |b| vec_bytes::<StmtIdx>(b.len()))
                    + pre.condition.memory_usage()
                    + vec_bytes::<crate::preproc_condition::PreprocCondition>(
                        pre.elsif_conditions.len(),
                    )
                    + pre.elsif_conditions.iter().map(|c| c.memory_usage()).sum::<usize>()
            }
            Stmt::While { body, .. } | Stmt::For { body, .. } | Stmt::ForEach { body, .. } => {
                vec_bytes::<StmtIdx>(body.len())
            }
            Stmt::Try { body, except } => {
                vec_bytes::<StmtIdx>(body.len()) + vec_bytes::<StmtIdx>(except.len())
            }
            Stmt::Goto(name) | Stmt::Label(name) => name_bytes(name),
            _ => 0,
        };
    }

    bytes
}

/// Rough live bytes of a [`BodySourceMap`]: the three id-keyed range vectors and
/// the three range-keyed lookup maps. Used together with [`body_heap`] to size
/// the `method_lower_query` memo.
pub(crate) fn source_map_heap(map: &BodySourceMap) -> usize {
    use std::mem::size_of;

    use crate::heap_estimate::{map_table_bytes, vec_bytes};

    size_of::<BodySourceMap>()
        + vec_bytes::<Option<LocalRange>>(map.expr_ranges.len())
        + vec_bytes::<Option<LocalRange>>(map.stmt_ranges.len())
        + vec_bytes::<Option<LocalRange>>(map.binding_ranges.len())
        + map_table_bytes::<LocalRange, ExprId>(map.range_to_expr.len())
        + map_table_bytes::<LocalRange, StmtId>(map.range_to_stmt.len())
        + map_table_bytes::<LocalRange, BindingId>(map.range_to_binding.len())
}

/// `heap_size` hook for `method_body_query`: its `Arc<Body>` is the one
/// `method_lower_query` already counts, so the projection reports nothing
/// beyond the (already-counted) pointer. The two memos have independent LRU
/// queues, so when the lowering memo is evicted first the body goes
/// unreported; counting it here instead would report it twice while both
/// live, which is the common state. An estimate, deliberately on the low side.
pub(crate) fn body_arc_heap(_v: &Arc<Body>) -> usize {
    0
}

/// `heap_size` hook for `method_lower_query`: the body and its source map,
/// the diagnostics and the external-ref side tables.
pub(crate) fn lower_result_heap(v: &Option<Arc<LowerResult>>) -> usize {
    use crate::heap_estimate::{map_table_bytes, vec_bytes};
    let Some(r) = v else { return 0 };
    body_heap(&r.body)
        + source_map_heap(&r.source_map)
        + vec_bytes::<BodyDiagnostic<LocalRange>>(r.diagnostics.len())
        + map_table_bytes::<NormName, ()>(r.referenced_externals.len())
        + vec_bytes::<ExternalRef<LocalRange>>(r.external_refs.len())
}

/// Everything lowering produces for one method (or for the module code).
/// Positions in it are relative to the lowered root; see [`SourceMapAt`] and
/// [`BodyDiagnostic::lift`] for placing them in the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LowerResult {
    pub body: Arc<Body>,
    pub source_map: BodySourceMap,
    pub diagnostics: Vec<BodyDiagnostic<LocalRange>>,
    pub referenced_externals: rustc_hash::FxHashSet<NormName>,
    pub external_refs: Vec<ExternalRef<LocalRange>>,
    pub size_lines: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExistingBindingKind {
    Local,
    Param,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BodyDiagnostic<R = TextRange> {
    MissingReturn {
        range: R,
    },

    EmptyCodeBlock {
        range: R,
    },

    DeprecatedMethod {
        name: String,
        range: R,
    },

    DeprecatedCurrentDate {
        name: String,
        range: R,
    },

    DeprecatedFind {
        name: String,
        range: R,
    },

    DeprecatedMessage {
        name: String,
        range: R,
    },

    DeprecatedTypeManagedForm {
        type_name: String,
        range: R,
    },

    MagicNumber {
        value: String,
        range: R,
        context: MagicNumberContext,
    },

    SelfAssign {
        range: R,
    },

    FunctionShouldHaveReturn {
        range: R,
    },

    BeginTransactionBeforeTryCatch {
        range: R,
    },

    MisplacedLoopControl {
        range: R,
        is_continue: bool,
    },

    MissedRequiredParameter {
        callee: String,
        module: Option<String>,
        mdo_type: Option<String>,
        mdo_name: Option<String>,
        args: Vec<bool>,
        range: R,
    },

    IfElseDuplicatedCodeBlock {
        range: R,
    },

    CodeAfterAsyncCall {
        method_name: String,
        range: R,
    },

    CommitTransactionOutsideTryCatch {
        range: R,
    },

    CommonModuleAssign {
        variable_name: String,
        range: R,
        existing_binding_kind: Option<ExistingBindingKind>,
    },

    MissingCommonModuleMethod {
        module: String,
        method: String,
        range: R,
    },

    RewriteMethodParameter {
        param_id: BindingId,
        stmt_id: StmtId,
        stmt_range: R,
        ident_range: R,
    },

    CreateQueryInCycle {
        range: R,
    },

    DeletingCollectionItem {
        collection_text: String,
        range: R,
    },

    SelfInsertion {
        range: R,
    },

    DeprecatedAttribute8312 {
        name: String,
        kind: DeprecatedKind8312,
        /// The object before the dot; `None` for a global method.
        owner: Option<String>,
        range: R,
    },

    ExecuteExternalCode {
        range: R,
    },

    ExtraCommas {
        range: R,
    },

    FileSystemAccess {
        range: R,
    },

    FormDataToValue {
        range: R,
    },

    FunctionNameStartsWithGet {
        name: String,
        range: R,
    },

    FunctionOutParameter {
        name: String,
        range: R,
    },

    FunctionReturnsSamePrimitive {
        range: R,
    },

    GetFormMethod {
        method_name: String,
        range: R,
    },

    GlobalContextMethodCollision8312 {
        method_name: String,
        range: R,
    },

    EmptyStatement {
        range: R,
    },

    MissingSemicolon {
        range: R,
    },

    IfElseDuplicatedCondition {
        first_occurrence_index: usize,
        range: R,
    },

    IfElseIfEndsWithElse {
        range: R,
    },

    IncorrectUseOfStrTemplate {
        range: R,
    },

    OneStatementPerLine {
        range: R,
    },

    OSUsersMethod {
        range: R,
    },

    ProcedureReturnsValue {
        range: R,
    },

    ReservedWordAsMethodName {
        name: String,
        range: R,
    },

    RedundantAccessToObject {
        kind: RedundantAccessKind,
        range: R,
    },

    StyleElementConstructors {
        type_name: String,
        range: R,
    },

    TempFilesDir {
        name: String,
        range: R,
    },

    TernaryOperatorUsage {
        range: R,
    },

    TooManyReturns {
        method_name: String,
        method_name_range: R,
        returns: Vec<R>,
    },

    UnaryPlusInConcatenation {
        range: R,
    },

    UseSystemInformation {
        range: R,
    },

    UsingCancelParameter {
        range: R,
    },

    UsingExternalCodeTools {
        range: R,
    },

    UsingFindElementByString {
        range: R,
    },

    UsingGoto {
        range: R,
    },

    UsingModalWindows {
        method_name: String,
        replacement: String,
        range: R,
    },

    UsingSynchronousCalls {
        method_name: String,
        replacement: String,
        range: R,
    },

    UsingThisForm {
        range: R,
    },

    WrongUseFunctionProceedWithCall {
        range: R,
    },

    WrongUseOfRollbackTransactionMethod {
        range: R,
    },

    DeprecatedMethodCall {
        callee: String,
        module: Option<String>,
        range: R,
    },

    ThisObjectAssign {
        range: R,
    },

    TryNumber {
        range: R,
    },

    UsingObjectNotAvailableUnix {
        type_name: String,
        range: R,
    },

    UnsafeSafeModeMethodCall {
        range: R,
    },

    UselessForEach {
        iterator_name: String,
        range: R,
    },

    UnsafeFindByCode {
        manager_name: String,
        object_name: String,
        range: R,
    },

    UsageWriteLogEvent {
        in_except_block: bool,
        arg_count: usize,
        log_level_empty: bool,
        comment_empty: bool,
        has_error_log_level: bool,
        has_detail_error_description: bool,
        except_has_raise: bool,
        range: R,
    },
}

impl<R> BodyDiagnostic<R> {
    /// The same diagnostic with every range passed through `f`. Exhaustive by
    /// construction: a new variant that carries a range fails to compile here
    /// until it is lifted too.
    pub fn map_range<T>(self, f: impl Fn(R) -> T) -> BodyDiagnostic<T> {
        match self {
            BodyDiagnostic::MissingReturn { range } => {
                BodyDiagnostic::MissingReturn { range: f(range) }
            }
            BodyDiagnostic::EmptyCodeBlock { range } => {
                BodyDiagnostic::EmptyCodeBlock { range: f(range) }
            }
            BodyDiagnostic::DeprecatedMethod { name, range } => {
                BodyDiagnostic::DeprecatedMethod { name, range: f(range) }
            }
            BodyDiagnostic::DeprecatedCurrentDate { name, range } => {
                BodyDiagnostic::DeprecatedCurrentDate { name, range: f(range) }
            }
            BodyDiagnostic::DeprecatedFind { name, range } => {
                BodyDiagnostic::DeprecatedFind { name, range: f(range) }
            }
            BodyDiagnostic::DeprecatedMessage { name, range } => {
                BodyDiagnostic::DeprecatedMessage { name, range: f(range) }
            }
            BodyDiagnostic::DeprecatedTypeManagedForm { type_name, range } => {
                BodyDiagnostic::DeprecatedTypeManagedForm { type_name, range: f(range) }
            }
            BodyDiagnostic::MagicNumber { value, range, context } => {
                BodyDiagnostic::MagicNumber { value, range: f(range), context }
            }
            BodyDiagnostic::SelfAssign { range } => BodyDiagnostic::SelfAssign { range: f(range) },
            BodyDiagnostic::FunctionShouldHaveReturn { range } => {
                BodyDiagnostic::FunctionShouldHaveReturn { range: f(range) }
            }
            BodyDiagnostic::BeginTransactionBeforeTryCatch { range } => {
                BodyDiagnostic::BeginTransactionBeforeTryCatch { range: f(range) }
            }
            BodyDiagnostic::MisplacedLoopControl { range, is_continue } => {
                BodyDiagnostic::MisplacedLoopControl { range: f(range), is_continue }
            }
            BodyDiagnostic::MissedRequiredParameter {
                callee,
                module,
                mdo_type,
                mdo_name,
                args,
                range,
            } => BodyDiagnostic::MissedRequiredParameter {
                callee,
                module,
                mdo_type,
                mdo_name,
                args,
                range: f(range),
            },
            BodyDiagnostic::IfElseDuplicatedCodeBlock { range } => {
                BodyDiagnostic::IfElseDuplicatedCodeBlock { range: f(range) }
            }
            BodyDiagnostic::CodeAfterAsyncCall { method_name, range } => {
                BodyDiagnostic::CodeAfterAsyncCall { method_name, range: f(range) }
            }
            BodyDiagnostic::CommitTransactionOutsideTryCatch { range } => {
                BodyDiagnostic::CommitTransactionOutsideTryCatch { range: f(range) }
            }
            BodyDiagnostic::CommonModuleAssign { variable_name, range, existing_binding_kind } => {
                BodyDiagnostic::CommonModuleAssign {
                    variable_name,
                    range: f(range),
                    existing_binding_kind,
                }
            }
            BodyDiagnostic::MissingCommonModuleMethod { module, method, range } => {
                BodyDiagnostic::MissingCommonModuleMethod { module, method, range: f(range) }
            }
            BodyDiagnostic::RewriteMethodParameter {
                param_id,
                stmt_id,
                stmt_range,
                ident_range,
            } => BodyDiagnostic::RewriteMethodParameter {
                param_id,
                stmt_id,
                stmt_range: f(stmt_range),
                ident_range: f(ident_range),
            },
            BodyDiagnostic::CreateQueryInCycle { range } => {
                BodyDiagnostic::CreateQueryInCycle { range: f(range) }
            }
            BodyDiagnostic::DeletingCollectionItem { collection_text, range } => {
                BodyDiagnostic::DeletingCollectionItem { collection_text, range: f(range) }
            }
            BodyDiagnostic::SelfInsertion { range } => {
                BodyDiagnostic::SelfInsertion { range: f(range) }
            }
            BodyDiagnostic::DeprecatedAttribute8312 { name, kind, owner, range } => {
                BodyDiagnostic::DeprecatedAttribute8312 { name, kind, owner, range: f(range) }
            }
            BodyDiagnostic::ExecuteExternalCode { range } => {
                BodyDiagnostic::ExecuteExternalCode { range: f(range) }
            }
            BodyDiagnostic::ExtraCommas { range } => {
                BodyDiagnostic::ExtraCommas { range: f(range) }
            }
            BodyDiagnostic::FileSystemAccess { range } => {
                BodyDiagnostic::FileSystemAccess { range: f(range) }
            }
            BodyDiagnostic::FormDataToValue { range } => {
                BodyDiagnostic::FormDataToValue { range: f(range) }
            }
            BodyDiagnostic::FunctionNameStartsWithGet { name, range } => {
                BodyDiagnostic::FunctionNameStartsWithGet { name, range: f(range) }
            }
            BodyDiagnostic::FunctionOutParameter { name, range } => {
                BodyDiagnostic::FunctionOutParameter { name, range: f(range) }
            }
            BodyDiagnostic::FunctionReturnsSamePrimitive { range } => {
                BodyDiagnostic::FunctionReturnsSamePrimitive { range: f(range) }
            }
            BodyDiagnostic::GetFormMethod { method_name, range } => {
                BodyDiagnostic::GetFormMethod { method_name, range: f(range) }
            }
            BodyDiagnostic::GlobalContextMethodCollision8312 { method_name, range } => {
                BodyDiagnostic::GlobalContextMethodCollision8312 { method_name, range: f(range) }
            }
            BodyDiagnostic::EmptyStatement { range } => {
                BodyDiagnostic::EmptyStatement { range: f(range) }
            }
            BodyDiagnostic::MissingSemicolon { range } => {
                BodyDiagnostic::MissingSemicolon { range: f(range) }
            }
            BodyDiagnostic::IfElseDuplicatedCondition { first_occurrence_index, range } => {
                BodyDiagnostic::IfElseDuplicatedCondition {
                    first_occurrence_index,
                    range: f(range),
                }
            }
            BodyDiagnostic::IfElseIfEndsWithElse { range } => {
                BodyDiagnostic::IfElseIfEndsWithElse { range: f(range) }
            }
            BodyDiagnostic::IncorrectUseOfStrTemplate { range } => {
                BodyDiagnostic::IncorrectUseOfStrTemplate { range: f(range) }
            }
            BodyDiagnostic::OneStatementPerLine { range } => {
                BodyDiagnostic::OneStatementPerLine { range: f(range) }
            }
            BodyDiagnostic::OSUsersMethod { range } => {
                BodyDiagnostic::OSUsersMethod { range: f(range) }
            }
            BodyDiagnostic::ProcedureReturnsValue { range } => {
                BodyDiagnostic::ProcedureReturnsValue { range: f(range) }
            }
            BodyDiagnostic::ReservedWordAsMethodName { name, range } => {
                BodyDiagnostic::ReservedWordAsMethodName { name, range: f(range) }
            }
            BodyDiagnostic::RedundantAccessToObject { kind, range } => {
                BodyDiagnostic::RedundantAccessToObject { kind, range: f(range) }
            }
            BodyDiagnostic::StyleElementConstructors { type_name, range } => {
                BodyDiagnostic::StyleElementConstructors { type_name, range: f(range) }
            }
            BodyDiagnostic::TempFilesDir { name, range } => {
                BodyDiagnostic::TempFilesDir { name, range: f(range) }
            }
            BodyDiagnostic::TernaryOperatorUsage { range } => {
                BodyDiagnostic::TernaryOperatorUsage { range: f(range) }
            }
            BodyDiagnostic::TooManyReturns { method_name, method_name_range, returns } => {
                BodyDiagnostic::TooManyReturns {
                    method_name,
                    method_name_range: f(method_name_range),
                    returns: returns.into_iter().map(&f).collect(),
                }
            }
            BodyDiagnostic::UnaryPlusInConcatenation { range } => {
                BodyDiagnostic::UnaryPlusInConcatenation { range: f(range) }
            }
            BodyDiagnostic::UseSystemInformation { range } => {
                BodyDiagnostic::UseSystemInformation { range: f(range) }
            }
            BodyDiagnostic::UsingCancelParameter { range } => {
                BodyDiagnostic::UsingCancelParameter { range: f(range) }
            }
            BodyDiagnostic::UsingExternalCodeTools { range } => {
                BodyDiagnostic::UsingExternalCodeTools { range: f(range) }
            }
            BodyDiagnostic::UsingFindElementByString { range } => {
                BodyDiagnostic::UsingFindElementByString { range: f(range) }
            }
            BodyDiagnostic::UsingGoto { range } => BodyDiagnostic::UsingGoto { range: f(range) },
            BodyDiagnostic::UsingModalWindows { method_name, replacement, range } => {
                BodyDiagnostic::UsingModalWindows { method_name, replacement, range: f(range) }
            }
            BodyDiagnostic::UsingSynchronousCalls { method_name, replacement, range } => {
                BodyDiagnostic::UsingSynchronousCalls { method_name, replacement, range: f(range) }
            }
            BodyDiagnostic::UsingThisForm { range } => {
                BodyDiagnostic::UsingThisForm { range: f(range) }
            }
            BodyDiagnostic::WrongUseFunctionProceedWithCall { range } => {
                BodyDiagnostic::WrongUseFunctionProceedWithCall { range: f(range) }
            }
            BodyDiagnostic::WrongUseOfRollbackTransactionMethod { range } => {
                BodyDiagnostic::WrongUseOfRollbackTransactionMethod { range: f(range) }
            }
            BodyDiagnostic::DeprecatedMethodCall { callee, module, range } => {
                BodyDiagnostic::DeprecatedMethodCall { callee, module, range: f(range) }
            }
            BodyDiagnostic::ThisObjectAssign { range } => {
                BodyDiagnostic::ThisObjectAssign { range: f(range) }
            }
            BodyDiagnostic::TryNumber { range } => BodyDiagnostic::TryNumber { range: f(range) },
            BodyDiagnostic::UsingObjectNotAvailableUnix { type_name, range } => {
                BodyDiagnostic::UsingObjectNotAvailableUnix { type_name, range: f(range) }
            }
            BodyDiagnostic::UnsafeSafeModeMethodCall { range } => {
                BodyDiagnostic::UnsafeSafeModeMethodCall { range: f(range) }
            }
            BodyDiagnostic::UselessForEach { iterator_name, range } => {
                BodyDiagnostic::UselessForEach { iterator_name, range: f(range) }
            }
            BodyDiagnostic::UnsafeFindByCode { manager_name, object_name, range } => {
                BodyDiagnostic::UnsafeFindByCode { manager_name, object_name, range: f(range) }
            }
            BodyDiagnostic::UsageWriteLogEvent {
                in_except_block,
                arg_count,
                log_level_empty,
                comment_empty,
                has_error_log_level,
                has_detail_error_description,
                except_has_raise,
                range,
            } => BodyDiagnostic::UsageWriteLogEvent {
                in_except_block,
                arg_count,
                log_level_empty,
                comment_empty,
                has_error_log_level,
                has_detail_error_description,
                except_has_raise,
                range: f(range),
            },
        }
    }
}

impl BodyDiagnostic<LocalRange> {
    /// The diagnostic placed in its file.
    pub fn lift(self, base: MethodOffset) -> BodyDiagnostic {
        self.map_range(|range| range.lift(base))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MagicNumberContext {
    InConstructor { type_name: String },
    InStructureInsert,
    InStructureConstructor,
    InDefaultParam,
    InArrayIndex,
    InPropertyAssignment,
    InSimpleAssignment,
    InExpression,
    InReturn,
    InMethodCall,
    InTernaryBranch,
    InRoundPrecision,
    InForLoopBoundary,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeprecatedKind8312 {
    Attribute,
    Method,
    GlobalMethod,
    EnumName,
    EnumValue,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RedundantAccessKind {
    ThisObject { prefix: String },
    TwoLevel { module: String },
    ThreeLevel { mdo_type: String, mdo_name: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ExternalRef<R = TextRange> {
    QualifiedCall { receiver: Name, method: Name, range: R },

    ManagerAccess { manager_type: ManagerType, object_name: Name, method: Option<Name>, range: R },
}

impl<R> ExternalRef<R> {
    pub fn map_range<T>(self, f: impl Fn(R) -> T) -> ExternalRef<T> {
        match self {
            ExternalRef::QualifiedCall { receiver, method, range } => {
                ExternalRef::QualifiedCall { receiver, method, range: f(range) }
            }
            ExternalRef::ManagerAccess { manager_type, object_name, method, range } => {
                ExternalRef::ManagerAccess { manager_type, object_name, method, range: f(range) }
            }
        }
    }
}

impl ExternalRef<LocalRange> {
    pub fn lift(self, base: MethodOffset) -> ExternalRef {
        self.map_range(|range| range.lift(base))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ManagerType {
    Documents,
    Catalogs,
    DataProcessors,
    Reports,
    InformationRegisters,
    AccumulationRegisters,
    AccountingRegisters,
    CalculationRegisters,
    ChartsOfCharacteristicTypes,
    ChartsOfAccounts,
    ChartsOfCalculationTypes,
    BusinessProcesses,
    Tasks,
    Enums,
    ExchangePlans,
    ExternalDataSources,
    Constants,
}

impl ManagerType {
    pub fn from_name(name: &str) -> Option<Self> {
        let lower = name.fold_lower();
        match lower.as_str() {
            "документы" | "documents" => Some(Self::Documents),
            "справочники" | "catalogs" => Some(Self::Catalogs),
            "обработки" | "dataprocessors" => Some(Self::DataProcessors),
            "отчёты" | "отчеты" | "reports" => Some(Self::Reports),
            "регистрысведений" | "informationregisters" => {
                Some(Self::InformationRegisters)
            }
            "регистрынакопления" | "accumulationregisters" => {
                Some(Self::AccumulationRegisters)
            }
            "регистрыбухгалтерии" | "accountingregisters" => {
                Some(Self::AccountingRegisters)
            }
            "регистрырасчёта" | "регистрырасчета" | "calculationregisters" => {
                Some(Self::CalculationRegisters)
            }
            "планывидовхарактеристик" | "chartsofcharacteristictypes" => {
                Some(Self::ChartsOfCharacteristicTypes)
            }
            "планысчетов" | "chartsofaccounts" => Some(Self::ChartsOfAccounts),
            "планывидоврасчёта" | "планывидоврасчета" | "chartsofcalculationtypes" => {
                Some(Self::ChartsOfCalculationTypes)
            }
            "бизнеспроцессы" | "businessprocesses" => Some(Self::BusinessProcesses),
            "задачи" | "tasks" => Some(Self::Tasks),
            "перечисления" | "enums" => Some(Self::Enums),
            "планыобмена" | "exchangeplans" => Some(Self::ExchangePlans),
            "внешниеисточникиданных" | "externaldatasources" => {
                Some(Self::ExternalDataSources)
            }
            "константы" | "constants" => Some(Self::Constants),
            _ => None,
        }
    }

    /// Whether this manager addresses a register (accumulation / information /
    /// accounting / calculation). Registers expose the record-set engine
    /// (`СоздатьНаборЗаписей` / `СоздатьМенеджерЗаписи`), so a literal-manager
    /// record-set creator on one of these is a register write-capable access.
    pub fn is_register(self) -> bool {
        matches!(
            self,
            Self::InformationRegisters
                | Self::AccumulationRegisters
                | Self::AccountingRegisters
                | Self::CalculationRegisters
        )
    }

    pub fn from_mdo_type(mdo_type: bsl_metadata::MdoType) -> Option<Self> {
        use bsl_metadata::MdoType;
        match mdo_type {
            MdoType::Document => Some(Self::Documents),
            MdoType::Catalog => Some(Self::Catalogs),
            MdoType::DataProcessor => Some(Self::DataProcessors),
            MdoType::Report => Some(Self::Reports),
            MdoType::InformationRegister => Some(Self::InformationRegisters),
            MdoType::AccumulationRegister => Some(Self::AccumulationRegisters),
            MdoType::AccountingRegister => Some(Self::AccountingRegisters),
            MdoType::CalculationRegister => Some(Self::CalculationRegisters),
            MdoType::ChartOfCharacteristicTypes => Some(Self::ChartsOfCharacteristicTypes),
            MdoType::ChartOfAccounts => Some(Self::ChartsOfAccounts),
            MdoType::ChartOfCalculationTypes => Some(Self::ChartsOfCalculationTypes),
            MdoType::BusinessProcess => Some(Self::BusinessProcesses),
            MdoType::Task => Some(Self::Tasks),
            MdoType::Enum => Some(Self::Enums),
            MdoType::ExchangePlan => Some(Self::ExchangePlans),
            MdoType::ExternalDataSource => Some(Self::ExternalDataSources),
            MdoType::Constant => Some(Self::Constants),
            // External objects have no collection: `ВнешниеОбработки` is the
            // platform's manager, not a directory of the dump.
            MdoType::ExternalDataProcessor
            | MdoType::ExternalReport
            | MdoType::CommonModule
            | MdoType::EventSubscription
            | MdoType::Subsystem
            | MdoType::Role => None,
        }
    }

    pub fn to_mdo_type(self) -> bsl_metadata::MdoType {
        use bsl_metadata::MdoType;
        match self {
            Self::Documents => MdoType::Document,
            Self::Catalogs => MdoType::Catalog,
            Self::DataProcessors => MdoType::DataProcessor,
            Self::Reports => MdoType::Report,
            Self::InformationRegisters => MdoType::InformationRegister,
            Self::AccumulationRegisters => MdoType::AccumulationRegister,
            Self::AccountingRegisters => MdoType::AccountingRegister,
            Self::CalculationRegisters => MdoType::CalculationRegister,
            Self::ChartsOfCharacteristicTypes => MdoType::ChartOfCharacteristicTypes,
            Self::ChartsOfAccounts => MdoType::ChartOfAccounts,
            Self::ChartsOfCalculationTypes => MdoType::ChartOfCalculationTypes,
            Self::BusinessProcesses => MdoType::BusinessProcess,
            Self::Tasks => MdoType::Task,
            Self::Enums => MdoType::Enum,
            Self::ExchangePlans => MdoType::ExchangePlan,
            Self::ExternalDataSources => MdoType::ExternalDataSource,
            Self::Constants => MdoType::Constant,
        }
    }
}

impl<R: Copy> BodyDiagnostic<R> {
    pub fn range(&self) -> R {
        match self {
            BodyDiagnostic::MissingReturn { range } => *range,
            BodyDiagnostic::EmptyCodeBlock { range } => *range,
            BodyDiagnostic::DeprecatedMethod { range, .. } => *range,
            BodyDiagnostic::DeprecatedCurrentDate { range, .. } => *range,
            BodyDiagnostic::DeprecatedFind { range, .. } => *range,
            BodyDiagnostic::DeprecatedMessage { range, .. } => *range,
            BodyDiagnostic::DeprecatedTypeManagedForm { range, .. } => *range,
            BodyDiagnostic::MagicNumber { range, .. } => *range,
            BodyDiagnostic::SelfAssign { range } => *range,
            BodyDiagnostic::FunctionShouldHaveReturn { range } => *range,
            BodyDiagnostic::BeginTransactionBeforeTryCatch { range } => *range,
            BodyDiagnostic::MisplacedLoopControl { range, .. } => *range,
            BodyDiagnostic::MissedRequiredParameter { range, .. } => *range,
            BodyDiagnostic::IfElseDuplicatedCodeBlock { range } => *range,
            BodyDiagnostic::CodeAfterAsyncCall { range, .. } => *range,
            BodyDiagnostic::CommitTransactionOutsideTryCatch { range } => *range,
            BodyDiagnostic::CommonModuleAssign { range, .. } => *range,
            BodyDiagnostic::MissingCommonModuleMethod { range, .. } => *range,
            BodyDiagnostic::RewriteMethodParameter { ident_range, .. } => *ident_range,
            BodyDiagnostic::CreateQueryInCycle { range } => *range,
            BodyDiagnostic::DeletingCollectionItem { range, .. } => *range,
            BodyDiagnostic::SelfInsertion { range } => *range,
            BodyDiagnostic::DeprecatedAttribute8312 { range, .. } => *range,
            BodyDiagnostic::ExecuteExternalCode { range } => *range,
            BodyDiagnostic::ExtraCommas { range } => *range,
            BodyDiagnostic::FileSystemAccess { range } => *range,
            BodyDiagnostic::FormDataToValue { range } => *range,
            BodyDiagnostic::FunctionNameStartsWithGet { range, .. } => *range,
            BodyDiagnostic::FunctionOutParameter { range, .. } => *range,
            BodyDiagnostic::FunctionReturnsSamePrimitive { range } => *range,
            BodyDiagnostic::GetFormMethod { range, .. } => *range,
            BodyDiagnostic::GlobalContextMethodCollision8312 { range, .. } => *range,
            BodyDiagnostic::EmptyStatement { range } => *range,
            BodyDiagnostic::MissingSemicolon { range } => *range,
            BodyDiagnostic::IfElseDuplicatedCondition { range, .. } => *range,
            BodyDiagnostic::IfElseIfEndsWithElse { range } => *range,
            BodyDiagnostic::IncorrectUseOfStrTemplate { range } => *range,
            BodyDiagnostic::OneStatementPerLine { range } => *range,
            BodyDiagnostic::OSUsersMethod { range } => *range,
            BodyDiagnostic::ProcedureReturnsValue { range } => *range,
            BodyDiagnostic::ReservedWordAsMethodName { range, .. } => *range,
            BodyDiagnostic::RedundantAccessToObject { range, .. } => *range,
            BodyDiagnostic::StyleElementConstructors { range, .. } => *range,
            BodyDiagnostic::TempFilesDir { range, .. } => *range,
            BodyDiagnostic::TernaryOperatorUsage { range } => *range,
            BodyDiagnostic::TooManyReturns { method_name_range, .. } => *method_name_range,
            BodyDiagnostic::UnaryPlusInConcatenation { range } => *range,
            BodyDiagnostic::UseSystemInformation { range } => *range,
            BodyDiagnostic::UsingCancelParameter { range } => *range,
            BodyDiagnostic::UsingExternalCodeTools { range } => *range,
            BodyDiagnostic::UsingFindElementByString { range } => *range,
            BodyDiagnostic::UsingGoto { range } => *range,
            BodyDiagnostic::UsingModalWindows { range, .. } => *range,
            BodyDiagnostic::UsingSynchronousCalls { range, .. } => *range,
            BodyDiagnostic::UsingThisForm { range } => *range,
            BodyDiagnostic::WrongUseFunctionProceedWithCall { range } => *range,
            BodyDiagnostic::WrongUseOfRollbackTransactionMethod { range } => *range,
            BodyDiagnostic::DeprecatedMethodCall { range, .. } => *range,
            BodyDiagnostic::ThisObjectAssign { range } => *range,
            BodyDiagnostic::TryNumber { range } => *range,
            BodyDiagnostic::UsingObjectNotAvailableUnix { range, .. } => *range,
            BodyDiagnostic::UnsafeSafeModeMethodCall { range } => *range,
            BodyDiagnostic::UselessForEach { range, .. } => *range,
            BodyDiagnostic::UnsafeFindByCode { range, .. } => *range,
            BodyDiagnostic::UsageWriteLogEvent { range, .. } => *range,
        }
    }
}

/// Lower one method taken from any tree; see [`lower::lower_method`] — the
/// node is re-rooted, so the result's positions are method-relative.
pub fn lower_method(method_node: &SyntaxNode, is_function: bool) -> LowerResult {
    lower::lower_method(method_node, is_function)
}

pub fn lower_method_with_externals(
    method_node: &SyntaxNode,
    is_function: bool,
    line_index: Option<std::sync::Arc<line_index::LineIndex>>,
) -> LowerResult {
    lower::lower_method_with_externals(method_node, is_function, line_index)
}

pub fn lower_module_code(
    root: &SyntaxNode,
    line_index: Option<std::sync::Arc<line_index::LineIndex>>,
) -> LowerResult {
    lower::lower_module_code(root, line_index)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hir::{Binding, Expr, Literal, Stmt};
    use crate::Name;
    use ordered_float::NotNan;

    #[test]
    fn every_manager_type_yields_a_manager_access_ref() {
        // Dependency discovery must over-approximate: a missing `ManagerAccess`
        // silently drops a file dependency, which no diagnostic or edge comparison
        // would surface.
        for &mdo in bsl_metadata::MdoType::all() {
            let Some(manager_type) = ManagerType::from_mdo_type(mdo) else {
                continue;
            };
            let plural = mdo.russian_plural().expect("manager-backed kinds name a collection");
            let code = format!("Процедура Тест()\n    {plural}.Объект1.Метод();\nКонецПроцедуры");
            let parse = parser::parse(&code);
            let method = parse
                .syntax_node()
                .descendants()
                .find(|node| node.kind() == syntax::SyntaxKind::PROCEDURE_DEF)
                .expect("fixture declares a procedure");
            let lowered = lower_method_with_externals(&method, false, None);
            assert!(
                lowered.external_refs.iter().any(|r| matches!(
                    r,
                    ExternalRef::ManagerAccess { manager_type: found, object_name, .. }
                        if *found == manager_type && object_name.as_str() == "Объект1"
                )),
                "{manager_type:?} ({plural}) must produce a ManagerAccess ref; got {:?}",
                lowered.external_refs
            );
        }
    }

    /// Обнаружение зависимостей над-приближает СОЗНАТЕЛЬНО: даже когда имя корня
    /// занято локалью, ребро строится. Лишнее ребро стоит одной лишней инвалидации,
    /// пропущенное — неверного ответа, поэтому пропуск здесь дефект, а лишнее нет.
    /// Вердикт о владении принимает инференс, у которого есть резолвер.
    #[test]
    fn a_shadowed_root_still_yields_a_dependency_edge() {
        let code = "Процедура Тест()\n    Перем Справочники;\n    \
                    Справочники.Объект1.Метод();\nКонецПроцедуры";
        let parse = parser::parse(code);
        let method = parse
            .syntax_node()
            .descendants()
            .find(|node| node.kind() == syntax::SyntaxKind::PROCEDURE_DEF)
            .expect("fixture declares a procedure");
        let lowered = lower_method_with_externals(&method, false, None);
        assert!(
            lowered.external_refs.iter().any(|r| matches!(
                r,
                ExternalRef::ManagerAccess { object_name, .. } if object_name.as_str() == "Объект1"
            )),
            "dependency discovery must not lose the edge; got {:?}",
            lowered.external_refs
        );
    }

    #[test]
    fn manager_type_to_mdo_type_round_trips() {
        let all = [
            ManagerType::Documents,
            ManagerType::Catalogs,
            ManagerType::DataProcessors,
            ManagerType::Reports,
            ManagerType::InformationRegisters,
            ManagerType::AccumulationRegisters,
            ManagerType::AccountingRegisters,
            ManagerType::CalculationRegisters,
            ManagerType::ChartsOfCharacteristicTypes,
            ManagerType::ChartsOfAccounts,
            ManagerType::ChartsOfCalculationTypes,
            ManagerType::BusinessProcesses,
            ManagerType::Tasks,
            ManagerType::Enums,
            ManagerType::ExchangePlans,
            ManagerType::ExternalDataSources,
            ManagerType::Constants,
        ];
        for mt in all {
            assert_eq!(
                ManagerType::from_mdo_type(mt.to_mdo_type()),
                Some(mt),
                "to_mdo_type/from_mdo_type must round-trip for {mt:?}"
            );
        }
    }

    #[test]
    fn test_body_creation() {
        let body = Body::new();
        assert_eq!(body.expr_count(), 0);
        assert_eq!(body.stmt_count(), 0);
        assert_eq!(body.binding_count(), 0);
        assert_eq!(body.params.len(), 0);
        assert_eq!(body.body_stmts.len(), 0);
    }

    #[test]
    fn test_body_with_expressions() {
        let mut body = Body::new();

        let expr_id = body.exprs.alloc(Expr::Literal(Literal::Number(NotNan::new(42.0).unwrap())));
        assert_eq!(body.expr_count(), 1);

        let expr = body.expr(ExprId::from_idx(expr_id));
        assert!(
            matches!(expr, Expr::Literal(Literal::Number(n)) if *n == NotNan::new(42.0).unwrap())
        );
    }

    #[test]
    fn test_body_with_statements() {
        let mut body = Body::new();

        let expr_id = body.exprs.alloc(Expr::Literal(Literal::Number(NotNan::new(1.0).unwrap())));
        let stmt_id = body.stmts.alloc(Stmt::Expr(expr_id));

        assert_eq!(body.stmt_count(), 1);
        let stmt = body.stmt(StmtId::from_idx(stmt_id));
        assert!(matches!(stmt, Stmt::Expr(id) if *id == expr_id));
    }

    #[test]
    fn test_body_with_bindings() {
        let mut body = Body::new();

        let binding_id = body.bindings.alloc(Binding::var(Name::new("Переменная")));
        assert_eq!(body.binding_count(), 1);

        let binding = body.binding(BindingId::from_idx(binding_id));
        assert_eq!(binding.name.as_str(), "Переменная");
        assert!(!binding.is_val);
    }

    #[test]
    fn test_source_map() {
        let mut body = Body::new();
        let mut source_map = BodySourceMap::new();

        let expr_id = body.exprs.alloc(Expr::Literal(Literal::Number(NotNan::new(42.0).unwrap())));
        let range = LocalRange::of_detached_node(TextRange::new(0.into(), 2.into()));

        source_map.record_expr(expr_id, range);

        assert_eq!(source_map.expr_range(ExprId::from_idx(expr_id)), Some(range));
        assert_eq!(source_map.expr_at_range(range), Some(ExprId::from_idx(expr_id)));
    }

    #[test]
    fn test_source_map_binding_at_range() {
        let mut body = Body::new();
        let mut source_map = BodySourceMap::new();

        let binding_id = body.bindings.alloc(Binding::var(Name::new("Запись")));
        let range = LocalRange::of_detached_node(TextRange::new(10.into(), 16.into()));
        source_map.record_binding(binding_id, range);

        assert_eq!(source_map.binding_range(BindingId::from_idx(binding_id)), Some(range));
        assert_eq!(source_map.binding_at_range(range), Some(BindingId::from_idx(binding_id)));
        let other = LocalRange::of_detached_node(TextRange::new(20.into(), 24.into()));
        assert_eq!(source_map.binding_at_range(other), None);
    }

    #[test]
    fn test_body_diagnostic_range() {
        let range = TextRange::new(10.into(), 20.into());

        let diagnostics = vec![
            BodyDiagnostic::MissingReturn { range },
            BodyDiagnostic::EmptyCodeBlock { range },
            BodyDiagnostic::DeprecatedMethod { name: "Test".to_string(), range },
            BodyDiagnostic::MagicNumber {
                value: "42".to_string(),
                range,
                context: MagicNumberContext::Other,
            },
            BodyDiagnostic::SelfAssign { range },
            BodyDiagnostic::FunctionShouldHaveReturn { range },
            BodyDiagnostic::IfElseDuplicatedCodeBlock { range },
            BodyDiagnostic::CommitTransactionOutsideTryCatch { range },
            BodyDiagnostic::CommonModuleAssign {
                variable_name: "Test".to_string(),
                range,
                existing_binding_kind: None,
            },
        ];

        for diag in diagnostics {
            assert_eq!(diag.range(), range);
        }
    }
}
