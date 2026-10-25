mod control_flow;
mod diagnostics;
mod expr;

mod platform_helpers;
mod preproc;
mod query_fragment;
mod stmt;
mod utils;

#[cfg(test)]
mod tests;

use cfg_types::LocalRange;
use intern::NormName;
use line_index::LineIndex;
use rustc_hash::{FxHashMap, FxHashSet};
use std::sync::Arc;
use stdx::case::CaseExt;
use syntax::{SyntaxKind, SyntaxNode};
use text_size::TextRange;

use crate::body::{Body, BodyDiagnostic, BodySourceMap, LowerResult};
use crate::hir::{Binding, BindingIdx, Expr, ExprIdx, Stmt, StmtIdx};
use crate::Name;

pub(crate) struct LoweringCtx {
    pub(crate) body: Body,
    pub(crate) source_map: BodySourceMap,
    pub(crate) diagnostics: Vec<BodyDiagnostic<LocalRange>>,
    pub(crate) is_function: bool,

    pub(crate) local_vars: FxHashMap<NormName, (Name, TextRange)>,

    pub(crate) param_names: FxHashSet<NormName>,

    pub(crate) by_ref_param_names: FxHashSet<NormName>,

    pub(crate) by_value_params: FxHashMap<String, BindingIdx>,

    pub(crate) cancel_params: FxHashSet<String>,

    pub(crate) pending_sdbl: Vec<(TextRange, syntax::SdblQuery)>,

    pub(crate) loop_depth: usize,

    pub(crate) query_vars: FxHashMap<NormName, QueryVarType>,

    pub(crate) foreach_collections: Vec<(ExprIdx, String)>,

    pub(crate) is_client_only: bool,

    pub(crate) has_no_context_annotation: bool,

    pub(crate) external_refs: Vec<crate::body::ExternalRef>,

    pub(crate) line_index: Option<Arc<LineIndex>>,

    pub(crate) statements_by_line: FxHashMap<u32, Vec<TextRange>>,

    pub(crate) current_method_name: Option<String>,

    pub(crate) return_statements: Vec<TextRange>,

    pub(crate) is_instead_method: bool,

    pub(crate) in_platform_guard: bool,

    pub(crate) in_except_block: bool,

    pub(crate) except_has_raise: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum QueryVarType {
    Query,
    QueryBuilder,
    ReportBuilder,
    Undefined,
}

impl LoweringCtx {
    pub(crate) fn new(is_function: bool) -> Self {
        Self {
            body: Body::new(),
            source_map: BodySourceMap::new(),
            diagnostics: Vec::new(),
            is_function,
            local_vars: FxHashMap::default(),
            param_names: FxHashSet::default(),
            by_ref_param_names: FxHashSet::default(),
            by_value_params: FxHashMap::default(),
            cancel_params: FxHashSet::default(),
            pending_sdbl: Vec::new(),
            loop_depth: 0,
            query_vars: FxHashMap::default(),
            foreach_collections: Vec::new(),
            is_client_only: false,
            has_no_context_annotation: false,
            external_refs: Vec::new(),
            line_index: None,
            statements_by_line: FxHashMap::default(),
            current_method_name: None,
            return_statements: Vec::new(),
            is_instead_method: false,
            in_platform_guard: false,
            in_except_block: false,
            except_has_raise: false,
        }
    }

    pub(crate) fn set_line_index(&mut self, line_index: Arc<LineIndex>) {
        self.line_index = Some(line_index);
    }

    pub(crate) fn track_statement_line(&mut self, range: TextRange) -> bool {
        let Some(ref line_index) = self.line_index else {
            return false;
        };

        let line = line_index.line_col(range.start()).line;
        let statements = self.statements_by_line.entry(line).or_default();
        statements.push(range);

        statements.len() > 1
    }

    pub(crate) fn emit_one_statement_per_line_diagnostics(&mut self) {
        if self.line_index.is_none() {
            return;
        }

        for (_line, ranges) in std::mem::take(&mut self.statements_by_line) {
            if ranges.len() > 1 {
                for range in ranges.into_iter().skip(1) {
                    self.emit(BodyDiagnostic::OneStatementPerLine { range });
                }
            }
        }
    }

    pub(crate) fn emit_too_many_returns_diagnostic(
        &mut self,
        method_name: String,
        method_name_range: TextRange,
    ) {
        const MIN_THRESHOLD: usize = 2;

        let returns = std::mem::take(&mut self.return_statements);
        if returns.len() > MIN_THRESHOLD {
            self.emit(BodyDiagnostic::TooManyReturns { method_name, method_name_range, returns });
        }
    }

    pub(crate) fn register_param(&mut self, name: &str) {
        self.param_names.insert(NormName::intern(name));
    }

    pub(crate) fn register_local_var(&mut self, name: Name, range: TextRange) {
        let key = NormName::intern(name.as_str());
        self.local_vars.insert(key, (name, range));
    }

    // Lowering runs on a tree rooted at the lowered node itself, so every
    // range read off it is already relative to that root; these three and
    // `emit` are where that fact is stamped into the type.
    pub(crate) fn alloc_expr(&mut self, expr: Expr, range: TextRange) -> ExprIdx {
        let id = self.body.exprs.alloc(expr);
        self.source_map.record_expr(id, LocalRange::of_detached_node(range));
        id
    }

    pub(crate) fn alloc_stmt(&mut self, stmt: Stmt, range: TextRange) -> StmtIdx {
        let id = self.body.stmts.alloc(stmt);
        self.source_map.record_stmt(id, LocalRange::of_detached_node(range));
        id
    }

    pub(crate) fn alloc_binding(&mut self, binding: Binding, range: TextRange) -> BindingIdx {
        let id = self.body.bindings.alloc(binding);
        self.source_map.record_binding(id, LocalRange::of_detached_node(range));
        id
    }

    pub(crate) fn missing_expr(&mut self) -> ExprIdx {
        self.body.exprs.alloc(Expr::Missing)
    }

    pub(crate) fn mark_recovered_rec(&mut self, root: ExprIdx) {
        if !self.body.recovered_exprs.insert(root) {
            return;
        }
        let children: Vec<ExprIdx> = collect_child_exprs(&self.body.exprs[root]);
        for child in children {
            self.mark_recovered_rec(child);
        }
    }

    pub(crate) fn enter_loop(&mut self) {
        self.loop_depth += 1;
    }

    pub(crate) fn leave_loop(&mut self) {
        if self.loop_depth > 0 {
            self.loop_depth -= 1;
        }
    }

    pub(crate) fn in_loop(&self) -> bool {
        self.loop_depth > 0
    }

    pub(crate) fn register_query_var(&mut self, name: String, var_type: QueryVarType) {
        self.query_vars.insert(NormName::intern(&name), var_type);
    }

    pub(crate) fn get_query_var_type(&self, name: &str) -> Option<QueryVarType> {
        self.query_vars.get(&NormName::intern(name)).copied()
    }

    pub(crate) fn is_query_var(&self, name: &str) -> bool {
        matches!(
            self.get_query_var_type(name),
            Some(QueryVarType::Query | QueryVarType::QueryBuilder | QueryVarType::ReportBuilder)
        )
    }

    pub(crate) fn enter_foreach(&mut self, collection_expr: ExprIdx, collection_text: String) {
        self.foreach_collections.push((collection_expr, collection_text));
    }

    pub(crate) fn leave_foreach(&mut self) {
        self.foreach_collections.pop();
    }

    pub(crate) fn matches_foreach_collection(&self, expr: ExprIdx) -> Option<&str> {
        use crate::body::lower::expr::exprs_are_equal;

        for (collection_expr, collection_text) in self.foreach_collections.iter().rev() {
            if exprs_are_equal(&self.body, *collection_expr, expr) {
                return Some(collection_text.as_str());
            }
        }
        None
    }

    pub(crate) fn emit(&mut self, diagnostic: BodyDiagnostic) {
        self.diagnostics.push(diagnostic.map_range(LocalRange::of_detached_node));
    }
}

/// Lower one method taken from any tree: the node is re-rooted first, so the
/// result's positions are relative to the method wherever it sat.
pub fn lower_method(method_node: &SyntaxNode, is_function: bool) -> LowerResult {
    let root = crate::method_syntax::detach(method_node);
    lower_method_with_externals(&root, is_function, None)
}

fn collect_child_exprs(expr: &Expr) -> Vec<ExprIdx> {
    match expr {
        Expr::Missing | Expr::Literal(_) | Expr::Path(_) => Vec::new(),
        Expr::BinaryOp { lhs, rhs, .. } => vec![*lhs, *rhs],
        Expr::UnaryOp { expr, .. } | Expr::Await { expr } => vec![*expr],
        Expr::Ternary { condition, then_expr, else_expr } => {
            vec![*condition, *then_expr, *else_expr]
        }
        Expr::Call { callee, args } => {
            let mut out = Vec::with_capacity(1 + args.len());
            out.push(*callee);
            out.extend(args.iter().copied());
            out
        }
        Expr::MethodCall { receiver, args, .. } => {
            let mut out = Vec::with_capacity(1 + args.len());
            out.push(*receiver);
            out.extend(args.iter().copied());
            out
        }
        Expr::Index { base, index } => vec![*base, *index],
        Expr::Field { base, .. } => vec![*base],
        Expr::New { args, .. } => args.to_vec(),
        Expr::Array(elems) => elems.to_vec(),
    }
}

fn is_client_only_method(method_node: &SyntaxNode) -> bool {
    let annotations: Vec<_> = method_node
        .children()
        .filter(|child| {
            matches!(child.kind(), SyntaxKind::ANNOTATION | SyntaxKind::COMPILER_DIRECTIVE)
        })
        .collect();

    if annotations.len() != 1 {
        return false;
    }

    annotations[0]
        .descendants_with_tokens()
        .filter_map(|el| el.into_token())
        .any(|token| token.kind() == SyntaxKind::ANN_AT_CLIENT)
}

fn is_global_context_collision_8312(name: &str) -> bool {
    const COLLISION_METHODS: &[&str] = &[
        "проверитьбит",
        "проверитьпобитовоймаске",
        "установитьбит",
        "побитовоеи",
        "побитовоеили",
        "побитовоене",
        "побитовоеине",
        "побитовоеисключительноеили",
        "побитовыйсдвигвлево",
        "побитовыйсдвигвправо",
        "checkbit",
        "checkbybitmask",
        "setbit",
        "bitwiseand",
        "bitwiseor",
        "bitwisenot",
        "bitwiseandnot",
        "bitwisexor",
        "bitwiseshiftleft",
        "bitwiseshiftright",
    ];

    COLLISION_METHODS.contains(&name.fold_lower().as_str())
}

fn has_no_context_annotation_method(method_node: &SyntaxNode) -> bool {
    let annotations: Vec<_> = method_node
        .children()
        .filter(|child| {
            matches!(child.kind(), SyntaxKind::ANNOTATION | SyntaxKind::COMPILER_DIRECTIVE)
        })
        .collect();

    annotations.iter().any(|ann| {
        ann.descendants_with_tokens().filter_map(|el| el.into_token()).any(|token| {
            matches!(
                token.kind(),
                SyntaxKind::ANN_AT_SERVER_NO_CONTEXT
                    | SyntaxKind::ANN_AT_CLIENT_AT_SERVER_NO_CONTEXT
            )
        })
    })
}

fn is_around_annotation_method(method_node: &SyntaxNode) -> bool {
    let annotations: Vec<_> = method_node
        .children()
        .filter(|child| {
            matches!(child.kind(), SyntaxKind::ANNOTATION | SyntaxKind::COMPILER_DIRECTIVE)
        })
        .collect();

    annotations.iter().any(|ann| {
        ann.descendants_with_tokens()
            .filter_map(|el| el.into_token())
            .any(|token| token.kind() == SyntaxKind::ANN_AROUND)
    })
}

fn check_function_returns_same_primitive(ctx: &mut LoweringCtx, method_node: &SyntaxNode) {
    use crate::hir::{Expr, Literal, Stmt};

    if let Some(name_token) = method_node
        .children_with_tokens()
        .filter_map(|el| el.into_token())
        .find(|tok| tok.kind() == SyntaxKind::IDENT)
    {
        let name = name_token.text().fold_lower();
        if name.starts_with("подключаемый_") || name.starts_with("attachable_") {
            return;
        }
    }

    let mut return_literals: Vec<&Literal> = Vec::new();

    for (stmt_id, _) in ctx.body.stmts.iter() {
        if let Stmt::Return { value: Some(expr_id) } = &ctx.body.stmts[stmt_id] {
            if let Expr::Literal(lit) = &ctx.body.exprs[*expr_id] {
                return_literals.push(lit);
            } else {
                return;
            }
        }
    }

    if return_literals.len() < 2 {
        return;
    }

    let first = return_literals[0];
    let all_same = return_literals[1..].iter().all(|lit| literals_equal(first, lit));

    if all_same {
        if let Some(name_token) = method_node
            .children_with_tokens()
            .filter_map(|el| el.into_token())
            .find(|tok| tok.kind() == SyntaxKind::IDENT)
        {
            ctx.emit(BodyDiagnostic::FunctionReturnsSamePrimitive {
                range: name_token.text_range(),
            });
        }
    }
}

fn literals_equal(a: &crate::hir::Literal, b: &crate::hir::Literal) -> bool {
    use crate::hir::Literal;

    match (a, b) {
        (Literal::Number(a), Literal::Number(b)) => (a - b).abs() < f64::EPSILON,
        (Literal::String(a), Literal::String(b)) => a.to_uppercase() == b.to_uppercase(),
        (Literal::Date(a), Literal::Date(b)) => a == b,
        (Literal::Bool(a), Literal::Bool(b)) => a == b,
        (Literal::Undefined, Literal::Undefined) => true,
        (Literal::Null, Literal::Null) => true,
        _ => false,
    }
}

/// `method_node` must be the root of its tree (see `method_syntax::detach`):
/// every range recorded below is taken as relative to it.
pub fn lower_method_with_externals(
    method_node: &SyntaxNode,
    is_function: bool,
    line_index: Option<Arc<LineIndex>>,
) -> LowerResult {
    let mut ctx = LoweringCtx::new(is_function);

    if let Some(li) = line_index {
        ctx.set_line_index(li);
    }

    if has_error_node(method_node) {
        ctx.body.lowering_incomplete = true;
    }

    ctx.is_client_only = is_client_only_method(method_node);
    ctx.has_no_context_annotation = has_no_context_annotation_method(method_node);
    ctx.is_instead_method = is_around_annotation_method(method_node);

    let name_token =
        method_node.children_with_tokens().filter_map(|el| el.into_token()).find(|tok| {
            let k = tok.kind();
            if k == SyntaxKind::IDENT {
                return true;
            }
            k.is_keyword()
                && !matches!(
                    k,
                    SyntaxKind::KW_PROCEDURE
                        | SyntaxKind::KW_END_PROCEDURE
                        | SyntaxKind::KW_FUNCTION
                        | SyntaxKind::KW_END_FUNCTION
                        | SyntaxKind::KW_ASYNC
                        | SyntaxKind::KW_EXPORT
                )
        });

    if let Some(ref token) = name_token {
        ctx.current_method_name = Some(token.text().to_string());
    }

    if let Some(ref token) = name_token {
        if token.kind().is_keyword() {
            ctx.emit(BodyDiagnostic::ReservedWordAsMethodName {
                name: token.text().to_string(),
                range: token.text_range(),
            });
        }
    }

    if let Some(ref token) = name_token {
        let name_text = token.text();

        if is_function && name_text.fold_lower().starts_with("получить") {
            ctx.emit(BodyDiagnostic::FunctionNameStartsWithGet {
                name: name_text.to_string(),
                range: token.text_range(),
            });
        }

        if is_global_context_collision_8312(name_text) {
            ctx.emit(BodyDiagnostic::GlobalContextMethodCollision8312 {
                method_name: name_text.to_string(),
                range: token.text_range(),
            });
        }
    }

    if let Some(param_list) = method_node.children().find(|n| n.kind() == SyntaxKind::PARAM_LIST) {
        let params = stmt::lower_params(&mut ctx, &param_list);
        ctx.body.params = params.into_boxed_slice();
    }

    if let Some(stmt_list) = method_node.children().find(|n| n.kind() == SyntaxKind::STMT_LIST) {
        let stmts = stmt::lower_stmt_list(&mut ctx, &stmt_list);
        ctx.body.body_stmts = stmts.into_boxed_slice();

        let cf_analysis = control_flow::analyze_control_flow(&stmt_list);

        if is_function {
            let name_range = name_token
                .as_ref()
                .map(|t| t.text_range())
                .unwrap_or_else(|| method_node.text_range());

            if !cf_analysis.has_return {
                ctx.emit(BodyDiagnostic::FunctionShouldHaveReturn { range: name_range });
            }

            if cf_analysis.has_return {
                ctx.emit(BodyDiagnostic::MissingReturn { range: name_range });
            }
        }

        diagnostics::check_code_after_async_call(&mut ctx, &cf_analysis.call_stmts[..]);
    }

    if is_function {
        check_function_returns_same_primitive(&mut ctx, method_node);
    }

    ctx.emit_one_statement_per_line_diagnostics();

    if let Some(ref token) = name_token {
        ctx.emit_too_many_returns_diagnostic(token.text().to_string(), token.text_range());
    }

    let referenced_externals = collect_referenced_externals(&ctx.body);

    let size_lines = compute_method_size_lines(method_node, ctx.line_index.as_deref());

    LowerResult {
        body: Arc::new(ctx.body),
        source_map: ctx.source_map,
        diagnostics: ctx.diagnostics,
        referenced_externals,
        external_refs: ctx
            .external_refs
            .into_iter()
            .map(|r| r.map_range(LocalRange::of_detached_node))
            .collect(),
        size_lines,
    }
}

fn compute_method_size_lines(method_node: &SyntaxNode, line_index: Option<&LineIndex>) -> u32 {
    let Some(line_index) = line_index else { return 0 };
    let method_range = method_node.text_range();
    let start_line = line_index.line_col(method_range.start()).line as usize;
    let end_line = line_index.line_col(method_range.end()).line as usize;
    end_line.saturating_sub(start_line) as u32
}

/// The parser recovered somewhere inside `node` (itself included), so some of its
/// statements may not be lowered.
fn has_error_node(node: &SyntaxNode) -> bool {
    node.descendants().any(|n| n.kind() == SyntaxKind::ERROR)
}

fn is_compound_block_stmt(node: &SyntaxNode) -> bool {
    matches!(
        node.kind(),
        SyntaxKind::IF_STMT
            | SyntaxKind::WHILE_STMT
            | SyntaxKind::FOR_STMT
            | SyntaxKind::FOR_EACH_STMT
            | SyntaxKind::TRY_STMT
    )
}

pub fn lower_module_code(root: &SyntaxNode, line_index: Option<Arc<LineIndex>>) -> LowerResult {
    let mut ctx = LoweringCtx::new(false);

    if let Some(li) = line_index {
        ctx.set_line_index(li);
    }

    let mut stmts = Vec::new();

    for node in root.children() {
        if node.kind() == SyntaxKind::ERROR {
            ctx.body.lowering_incomplete = true;
        }
        if node.kind() == SyntaxKind::PRE_IF_DIR {
            if has_error_node(&node) {
                ctx.body.lowering_incomplete = true;
            }
            if let Some(stmt) = preproc::lower_preproc_if(&mut ctx, &node) {
                let stmt_id = ctx.alloc_stmt(stmt, node.text_range());
                stmts.push(stmt_id);
            } else {
                ctx.body.lowering_incomplete = true;
            }
            continue;
        }
        // Flat region markers are skipped; module-level code that used to live
        // inside a region container is now a direct sibling here.
        if !control_flow::is_statement_node(&node) {
            continue;
        }

        if node.kind() == SyntaxKind::VAR_DEF {
            continue;
        }

        if has_error_node(&node) {
            ctx.body.lowering_incomplete = true;
        }
        if let Some(stmt_id) = stmt::lower_stmt(&mut ctx, &node) {
            stmts.push(stmt_id);

            // A compound block statement shares its header line with its first
            // body statement, which is tracked during lowering; tracking the
            // header too would double-count it as a second statement on the line.
            if !stmt::should_skip_one_statement_per_line(&node) && !is_compound_block_stmt(&node) {
                ctx.track_statement_line(node.text_range());
            }

            if !stmt::should_skip_semicolon_check(&node) && !stmt::has_trailing_semicolon(&node) {
                let range = stmt::last_token_range(&node);
                ctx.emit(BodyDiagnostic::MissingSemicolon { range });
            }
        }
    }

    ctx.body.body_stmts = stmts.into_boxed_slice();

    ctx.emit_one_statement_per_line_diagnostics();

    let referenced_externals = collect_referenced_externals(&ctx.body);

    LowerResult {
        body: Arc::new(ctx.body),
        source_map: ctx.source_map,
        diagnostics: ctx.diagnostics,
        referenced_externals,
        external_refs: ctx
            .external_refs
            .into_iter()
            .map(|r| r.map_range(LocalRange::of_detached_node))
            .collect(),
        size_lines: 0,
    }
}

fn collect_referenced_externals(body: &Body) -> FxHashSet<NormName> {
    let mut referenced = FxHashSet::default();
    let mut declared = FxHashSet::default();

    for &param_id in body.params.iter() {
        let binding = &body.bindings[param_id];
        declared.insert(NormName::intern(binding.name.as_str()));
    }

    fn collect_declared(body: &Body, stmt_id: StmtIdx, declared: &mut FxHashSet<NormName>) {
        match body.stmt_idx(stmt_id) {
            Stmt::VarDecl { bindings } => {
                for &binding_id in bindings.iter() {
                    let binding = &body.bindings[binding_id];
                    declared.insert(NormName::intern(binding.name.as_str()));
                }
            }
            Stmt::For { var, body: loop_body, .. } => {
                let binding = &body.bindings[*var];
                declared.insert(NormName::intern(binding.name.as_str()));
                for &s in loop_body.iter() {
                    collect_declared(body, s, declared);
                }
            }
            Stmt::ForEach { var, body: loop_body, .. } => {
                let binding = &body.bindings[*var];
                declared.insert(NormName::intern(binding.name.as_str()));
                for &s in loop_body.iter() {
                    collect_declared(body, s, declared);
                }
            }
            Stmt::If(if_stmt) => {
                for &s in if_stmt.then_branch.iter() {
                    collect_declared(body, s, declared);
                }
                for (_, branch) in if_stmt.elsif_branches.iter() {
                    for &s in branch.iter() {
                        collect_declared(body, s, declared);
                    }
                }
                if let Some(ref else_stmts) = if_stmt.else_branch {
                    for &s in else_stmts.iter() {
                        collect_declared(body, s, declared);
                    }
                }
            }
            Stmt::While { body: loop_body, .. } => {
                for &s in loop_body.iter() {
                    collect_declared(body, s, declared);
                }
            }
            Stmt::Try { body: try_body, except } => {
                for &s in try_body.iter() {
                    collect_declared(body, s, declared);
                }
                for &s in except.iter() {
                    collect_declared(body, s, declared);
                }
            }
            _ => {}
        }
    }

    for &stmt_id in body.body_stmts.iter() {
        collect_declared(body, stmt_id, &mut declared);
    }

    for (_, expr) in body.exprs.iter() {
        if let Expr::Path(name) = expr {
            referenced.insert(NormName::intern(name.as_str()));
        }
    }

    referenced.retain(|name| !declared.contains(name));

    referenced
}
