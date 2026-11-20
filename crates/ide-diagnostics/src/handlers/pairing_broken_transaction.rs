use crate::define_metadata;
use crate::metadata::*;
use crate::{BodyContext, Diagnostic, DiagnosticCode};
use hir::cfg::{CfgEdgeType, CfgVertex, ControlFlowGraph, NodeIndex};
use hir::LocalRange;
use hir::{Body, BodySourceMap, Expr, ExprId, IdConversion, Literal, Stmt, StmtId, UnaryOp};
use rustc_hash::{FxHashMap, FxHashSet};
use stdx::case::CaseExt;

pub const METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: DiagnosticType::Error,
    severity: DiagnosticSeverityLevel::Major,
    scope: DiagnosticScope::All,
    modules: &[],
    minutes_to_fix: 15,
    activated_by_default: true,
    compatibility_mode: DiagnosticCompatibilityMode::Undefined,
    tags: &[MetadataTag::Standard],
    can_locate_on_project: false,
    extra_min_for_complexity: 0.0,
    lsp_severity_override: "",
    clean_code_attribute: CleanCodeAttribute::Intentional,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TransactionType {
    Begin,
    Commit,
    Rollback,
}

#[derive(Debug, Clone)]
struct TransactionCall {
    tx_type: TransactionType,
    method_name: String,
    range: LocalRange,
}

#[derive(Debug, Clone)]
struct TransactionIssue {
    range: LocalRange,
    method_name: String,
    pair_method: &'static str,
}

pub fn check_body(ctx: &BodyContext, acc: &mut Vec<Diagnostic<LocalRange>>) {
    let code = DiagnosticCode::PairingBrokenTransaction;

    if ctx.is_disabled_with_metadata(code) || ctx.is_module_code() {
        return;
    }

    let max_level = ctx
        .config
        .get_int(DiagnosticCode::PairingBrokenTransaction, "maxTransactionLevel")
        .unwrap_or(32) as i32;

    let cfg = ctx.cfg();
    let body = ctx.body();
    let no_platform_calls = FxHashSet::default();
    let platform_calls =
        if body.exprs_iter().any(|(id, _)| active_condition_call(body, id).is_some()) {
            ctx.unshadowed_platform_global_calls()
        } else {
            &no_platform_calls
        };
    let issues =
        check_transaction_pairing_cfg(body, ctx.source_map(), &cfg, max_level, platform_calls);
    for issue in issues {
        acc.push(create_diagnostic(issue, code, ctx));
    }
}

impl TransactionType {
    fn pair_method(&self) -> &'static str {
        match self {
            TransactionType::Begin => "ЗафиксироватьТранзакцию/ОтменитьТранзакцию",
            TransactionType::Commit | TransactionType::Rollback => "НачатьТранзакцию",
        }
    }
}

#[derive(Clone)]
struct PathState {
    level: i32,
    begin_stack: Vec<TransactionCall>,
    /// Concrete value pinned to a stable guard variable along the current path.
    /// Lets the DFS prune infeasible paths where the same flag would have to be
    /// both true and false (e.g. `Если Флаг Тогда НачатьТранзакцию()` paired
    /// with `Если Флаг Тогда ЗафиксироватьТранзакцию()`).
    pinned: FxHashMap<String, bool>,
}

impl PathState {
    fn new() -> Self {
        Self { level: 0, begin_stack: Vec::new(), pinned: FxHashMap::default() }
    }
}

/// Guard variable referenced by a conditional, paired with the variable value
/// that selects the `TrueBranch`. Only simple `Если Флаг` / `Если Не Флаг`
/// conditions are recognized; anything else returns `None` (no correlation).
fn condition_guard_var(body: &Body, condition: ExprId) -> Option<(String, bool)> {
    match body.expr(condition) {
        Expr::Path(name) => Some((name.as_str().fold_lower(), true)),
        Expr::UnaryOp { expr, op: UnaryOp::Not } => match body.expr(ExprId::from_idx(*expr)) {
            Expr::Path(name) => Some((name.as_str().fold_lower(), false)),
            _ => None,
        },
        _ => None,
    }
}

fn active_condition_call(body: &Body, condition: ExprId) -> Option<(ExprId, bool)> {
    match body.expr(condition) {
        Expr::Call { callee, args } if args.is_empty() => {
            let callee = ExprId::from_idx(*callee);
            let Expr::Path(name) = body.expr(callee) else { return None };
            matches!(name.as_str().fold_lower().as_str(), "транзакцияактивна" | "transactionactive")
                .then_some((callee, true))
        }
        Expr::UnaryOp { expr, op: UnaryOp::Not } => {
            active_condition_call(body, ExprId::from_idx(*expr))
                .map(|(callee, value)| (callee, !value))
        }
        _ => None,
    }
}

/// Counts simple `Имя = …` assignments per variable. A variable assigned at
/// most once is treated as a stable flag whose value is constant wherever it is
/// read, so two guards on it can be correlated without risking false negatives.
fn compute_assign_counts(body: &Body) -> FxHashMap<String, u32> {
    let mut counts: FxHashMap<String, u32> = FxHashMap::default();
    for (_, stmt) in body.stmts_iter() {
        if let Stmt::Assign { target, .. } = stmt {
            if let Expr::Path(name) = body.expr(ExprId::from_idx(*target)) {
                *counts.entry(name.as_str().fold_lower()).or_default() += 1;
            }
        }
    }
    counts
}

fn expr_is_begin(body: &Body, expr_id: ExprId) -> bool {
    if let Expr::Call { callee, .. } = body.expr(expr_id) {
        if let Expr::Path(name) = body.expr(ExprId::from_idx(*callee)) {
            return matches!(get_transaction_type(name.as_str()), Some(TransactionType::Begin));
        }
    }
    false
}

fn block_has_begin(body: &Body, stmts: &[hir::StmtIdx]) -> bool {
    stmts.iter().any(|&s| match body.stmt(StmtId::from_idx(s)) {
        Stmt::Expr(e) => expr_is_begin(body, ExprId::from_idx(*e)),
        Stmt::Assign { value, .. } => expr_is_begin(body, ExprId::from_idx(*value)),
        Stmt::If(i) => {
            block_has_begin(body, &i.then_branch)
                || i.elsif_branches.iter().any(|(_, b)| block_has_begin(body, b))
                || i.else_branch.as_ref().is_some_and(|b| block_has_begin(body, b))
        }
        Stmt::While { body: b, .. } | Stmt::For { body: b, .. } | Stmt::ForEach { body: b, .. } => {
            block_has_begin(body, b)
        }
        Stmt::Try { body: b, except } => block_has_begin(body, b) || block_has_begin(body, except),
        _ => false,
    })
}

/// Flags that gate a `НачатьТранзакцию()` call. Only these are correlated by the
/// DFS: pinning is the fix for the `Если Флаг Тогда НачатьТранзакцию()` idiom, so
/// flags that never guard a begin add no precision and would only enlarge the
/// memo state space. Restricting to begin-guarding flags keeps that space small.
fn compute_begin_guard_flags(body: &Body) -> FxHashSet<String> {
    fn walk(body: &Body, stmts: &[hir::StmtIdx], flags: &mut FxHashSet<String>) {
        for &s in stmts {
            if let Stmt::If(i) = body.stmt(StmtId::from_idx(s)) {
                if let Some((var, _)) = condition_guard_var(body, ExprId::from_idx(i.condition)) {
                    let in_then = block_has_begin(body, &i.then_branch);
                    let in_else = i.else_branch.as_ref().is_some_and(|b| block_has_begin(body, b));
                    if in_then || in_else {
                        flags.insert(var);
                    }
                }
                for (cond, branch) in i.elsif_branches.iter() {
                    if let Some((var, _)) = condition_guard_var(body, ExprId::from_idx(*cond)) {
                        if block_has_begin(body, branch) {
                            flags.insert(var);
                        }
                    }
                }
                walk(body, &i.then_branch, flags);
                for (_, b) in i.elsif_branches.iter() {
                    walk(body, b, flags);
                }
                if let Some(b) = &i.else_branch {
                    walk(body, b, flags);
                }
            } else if let Some(block) = stmt_child_blocks(body, StmtId::from_idx(s)) {
                for b in block {
                    walk(body, b, flags);
                }
            }
        }
    }

    let mut flags = FxHashSet::default();
    walk(body, body.body_stmts_typed(), &mut flags);
    flags
}

fn stmt_child_blocks(body: &Body, stmt_id: StmtId) -> Option<Vec<&[hir::StmtIdx]>> {
    match body.stmt(stmt_id) {
        Stmt::While { body: b, .. } | Stmt::For { body: b, .. } | Stmt::ForEach { body: b, .. } => {
            Some(vec![b])
        }
        Stmt::Try { body: b, except } => Some(vec![b, except]),
        _ => None,
    }
}

fn check_transaction_pairing_cfg(
    body: &Body,
    source_map: &BodySourceMap,
    cfg: &ControlFlowGraph,
    max_level: i32,
    platform_calls: &FxHashSet<ExprId>,
) -> Vec<TransactionIssue> {
    let entry = match cfg.start() {
        Some(e) => e,
        None => return vec![],
    };

    let node_tx_calls = precompute_transaction_calls(body, source_map, cfg);
    let node_assigned_vars = precompute_assigned_vars(body, cfg);
    let assign_counts = compute_assign_counts(body);
    let begin_guard_flags = compute_begin_guard_flags(body);

    let dfs_ctx = DfsContext {
        cfg,
        body,
        node_tx_calls: &node_tx_calls,
        node_assigned_vars: &node_assigned_vars,
        assign_counts: &assign_counts,
        begin_guard_flags: &begin_guard_flags,
        platform_calls,
        max_level,
    };

    let mut issues = Vec::new();
    let mut visited_states: FxHashMap<NodeIndex, FxHashSet<VisitKey>> = FxHashMap::default();

    dfs_check_paths(entry, PathState::new(), &mut visited_states, &mut issues, &dfs_ctx);

    let mut seen_ranges: FxHashSet<LocalRange> = FxHashSet::default();
    issues.retain(|issue| seen_ranges.insert(issue.range));

    issues
}

fn precompute_transaction_calls(
    body: &Body,
    source_map: &BodySourceMap,
    cfg: &ControlFlowGraph,
) -> FxHashMap<NodeIndex, Vec<TransactionCall>> {
    let mut result: FxHashMap<NodeIndex, Vec<TransactionCall>> = FxHashMap::default();

    for (node_idx, vertex) in cfg.vertices() {
        let mut calls = Vec::new();

        if let CfgVertex::BasicBlock(block) = vertex {
            for &stmt_id in block.statements() {
                if let Some(call) = check_transaction_call(body, stmt_id, source_map) {
                    calls.push(call);
                }
            }
        }

        if !calls.is_empty() {
            result.insert(node_idx, calls);
        }
    }

    result
}

/// Per-node simple-variable assignments in that basic block, paired with the
/// assigned boolean literal when the right-hand side is one. Crossing such an
/// assignment updates the variable's pin: a literal `Истина`/`Ложь` re-pins the
/// concrete value (constant propagation), any other right-hand side drops the
/// pin (value now unknown). This keeps correlation sound across reassignment —
/// a flag rewritten between its begin-guard and commit-guard, or one reassigned
/// each loop iteration — without losing precision on literal rewrites.
fn precompute_assigned_vars(
    body: &Body,
    cfg: &ControlFlowGraph,
) -> FxHashMap<NodeIndex, Vec<(String, Option<bool>)>> {
    let mut result: FxHashMap<NodeIndex, Vec<(String, Option<bool>)>> = FxHashMap::default();

    for (node_idx, vertex) in cfg.vertices() {
        if let CfgVertex::BasicBlock(block) = vertex {
            let mut vars = Vec::new();
            for &stmt_id in block.statements() {
                if let Stmt::Assign { target, value } = body.stmt(stmt_id) {
                    if let Expr::Path(name) = body.expr(ExprId::from_idx(*target)) {
                        let literal = match body.expr(ExprId::from_idx(*value)) {
                            Expr::Literal(Literal::Bool(b)) => Some(*b),
                            _ => None,
                        };
                        vars.push((name.as_str().fold_lower(), literal));
                    }
                }
            }
            if !vars.is_empty() {
                result.insert(node_idx, vars);
            }
        }
    }

    result
}

struct DfsContext<'a> {
    cfg: &'a ControlFlowGraph,
    body: &'a Body,
    node_tx_calls: &'a FxHashMap<NodeIndex, Vec<TransactionCall>>,
    node_assigned_vars: &'a FxHashMap<NodeIndex, Vec<(String, Option<bool>)>>,
    assign_counts: &'a FxHashMap<String, u32>,
    begin_guard_flags: &'a FxHashSet<String>,
    platform_calls: &'a FxHashSet<ExprId>,
    max_level: i32,
}

/// Memoization key for a node visit. The pinned guard values are part of the
/// key: the same `(node, level)` reached under different flag assumptions must
/// be explored separately, otherwise the first arrival would shadow a feasible
/// path (e.g. the `flag = false` branch that uncovers a double rollback).
type VisitKey = (i32, Vec<(String, bool)>);

/// Upper bound on simultaneously pinned guard variables on one path. Bounds the
/// pin-aware memo state at `2^MAX_PINNED_GUARDS` per `(node, level)`.
const MAX_PINNED_GUARDS: usize = 6;

fn visit_key(state: &PathState) -> VisitKey {
    let mut pins: Vec<(String, bool)> =
        state.pinned.iter().map(|(name, value)| (name.clone(), *value)).collect();
    pins.sort();
    (state.level, pins)
}

fn dfs_check_paths(
    node: NodeIndex,
    mut state: PathState,
    visited_states: &mut FxHashMap<NodeIndex, FxHashSet<VisitKey>>,
    issues: &mut Vec<TransactionIssue>,
    ctx: &DfsContext,
) {
    if state.level > ctx.max_level || state.level < -ctx.max_level {
        return;
    }

    let visits_at_node = visited_states.entry(node).or_default();
    if !visits_at_node.insert(visit_key(&state)) {
        return;
    }

    if let Some(calls) = ctx.node_tx_calls.get(&node) {
        for call in calls {
            match call.tx_type {
                TransactionType::Begin => {
                    state.level += 1;
                    state.begin_stack.push(call.clone());
                }
                TransactionType::Commit | TransactionType::Rollback => {
                    state.level -= 1;
                    if state.level < 0 {
                        issues.push(TransactionIssue {
                            range: call.range,
                            method_name: call.method_name.clone(),
                            pair_method: call.tx_type.pair_method(),
                        });
                        state.level = 0;
                    } else {
                        state.begin_stack.pop();
                    }
                }
            }
        }
    }

    if let Some(vars) = ctx.node_assigned_vars.get(&node) {
        for (var, literal) in vars {
            match literal {
                // Constant propagation: a literal rewrite re-pins the concrete
                // value (sound for any variable, and bounded by the cap).
                Some(value)
                    if ctx.begin_guard_flags.contains(var)
                        && (state.pinned.len() < MAX_PINNED_GUARDS
                            || state.pinned.contains_key(var)) =>
                {
                    state.pinned.insert(var.clone(), *value);
                }
                // Unknown right-hand side (or an untracked flag): the prior pin
                // no longer holds.
                _ => {
                    state.pinned.remove(var);
                }
            }
        }
    }

    if node == ctx.cfg.exit() {
        for begin_call in &state.begin_stack {
            issues.push(TransactionIssue {
                range: begin_call.range,
                method_name: begin_call.method_name.clone(),
                pair_method: begin_call.tx_type.pair_method(),
            });
        }
        return;
    }

    if matches!(ctx.cfg.vertex(node), Some(CfgVertex::Try)) {
        let mut try_node = None;
        let mut except_node = None;

        for (idx, edge_type) in ctx.cfg.outgoing_edges(node) {
            match edge_type {
                CfgEdgeType::Unconditional => try_node = Some(idx),
                CfgEdgeType::Exception => except_node = Some(idx),
                _ => {}
            }
        }

        if let Some(try_n) = try_node {
            dfs_check_paths(try_n, state.clone(), visited_states, issues, ctx);
        }

        if let Some(except_n) = except_node {
            let has_raise_edges = ctx.cfg.incoming_edges(except_n).any(|(source, kind)| {
                source != node && !ctx.cfg.is_possible_failure(source, *kind)
            });

            if !has_raise_edges {
                dfs_check_paths(except_n, state.clone(), visited_states, issues, ctx);
            }
        }
    } else if let Some(CfgVertex::Conditional(cond)) = ctx.cfg.vertex(node) {
        // An exception does not cancel a transaction begun by this method.
        // At level zero the caller may own a transaction, so absence proves nothing.
        let active_condition = (state.level > 0)
            .then(|| active_condition_call(ctx.body, cond.condition))
            .flatten()
            .filter(|(callee, _)| ctx.platform_calls.contains(callee))
            .map(|(_, value)| value);
        let guard = condition_guard_var(ctx.body, cond.condition)
            .filter(|(name, _)| ctx.begin_guard_flags.contains(name))
            .filter(|(name, _)| ctx.assign_counts.get(name).copied().unwrap_or(0) <= 1);

        let edges: Vec<_> = ctx
            .cfg
            .outgoing_edges(node)
            .filter(|(_, edge_type)| is_followed(ctx.cfg, node, **edge_type))
            .collect();

        for (succ, edge_type) in edges {
            if matches!(
                (edge_type, active_condition),
                (CfgEdgeType::TrueBranch, Some(false)) | (CfgEdgeType::FalseBranch, Some(true))
            ) {
                continue;
            }
            let mut next = state.clone();

            if let Some((var, true_val)) = &guard {
                let branch_val = match edge_type {
                    CfgEdgeType::TrueBranch => Some(*true_val),
                    CfgEdgeType::FalseBranch => Some(!*true_val),
                    _ => None,
                };
                if let Some(val) = branch_val {
                    if next.pinned.get(var).is_some_and(|&pinned| pinned != val) {
                        continue;
                    }
                    // Cap the pin set so the memo state (keyed on pinned values)
                    // cannot blow up on pathological procedures; begin-guarding
                    // flags are few in practice, so this is never hit by real code.
                    if next.pinned.len() < MAX_PINNED_GUARDS || next.pinned.contains_key(var) {
                        next.pinned.insert(var.clone(), val);
                    }
                }
            }

            dfs_check_paths(succ, next, visited_states, issues, ctx);
        }
    } else {
        let successors: Vec<_> = ctx
            .cfg
            .outgoing_edges(node)
            .filter(|(_, edge_type)| is_followed(ctx.cfg, node, **edge_type))
            .map(|(idx, _)| idx)
            .collect();
        for succ in successors {
            dfs_check_paths(succ, state.clone(), visited_states, issues, ctx);
        }
    }
}

/// The walk enters a handler from the entry of `Попытка` or from
/// `ВызватьИсключение`, not from every protected point that may fail.
fn is_followed(cfg: &ControlFlowGraph, node: NodeIndex, kind: CfgEdgeType) -> bool {
    kind != CfgEdgeType::Unexecutable && !cfg.is_possible_failure(node, kind)
}

fn check_transaction_call(
    body: &Body,
    stmt_id: StmtId,
    source_map: &BodySourceMap,
) -> Option<TransactionCall> {
    let stmt = body.stmt(stmt_id);

    let expr_id = match stmt {
        Stmt::Expr(expr_idx) => ExprId::from_idx(*expr_idx),
        Stmt::Assign { value, .. } => ExprId::from_idx(*value),
        _ => return None,
    };

    check_expr_transaction_call(body, expr_id, source_map)
}

fn check_expr_transaction_call(
    body: &Body,
    expr_id: ExprId,
    source_map: &BodySourceMap,
) -> Option<TransactionCall> {
    let expr = body.expr(expr_id);

    if let Expr::Call { callee, .. } = expr {
        let callee_id = ExprId::from_idx(*callee);
        let callee_expr = body.expr(callee_id);

        if let Expr::Path(name) = callee_expr {
            let method_name = name.as_str();
            if let Some(tx_type) = get_transaction_type(method_name) {
                let range = source_map.expr_range(expr_id)?;
                return Some(TransactionCall {
                    tx_type,
                    method_name: method_name.to_string(),
                    range,
                });
            }
        }
    }

    None
}

fn get_transaction_type(name: &str) -> Option<TransactionType> {
    let lower = name.fold_lower();
    match lower.as_str() {
        "начатьтранзакцию" | "begintransaction" => Some(TransactionType::Begin),
        "зафиксироватьтранзакцию" | "committransaction" => {
            Some(TransactionType::Commit)
        }
        "отменитьтранзакцию" | "rollbacktransaction" => {
            Some(TransactionType::Rollback)
        }
        _ => None,
    }
}

fn create_diagnostic(
    issue: TransactionIssue,
    code: DiagnosticCode,
    ctx: &BodyContext,
) -> Diagnostic<LocalRange> {
    Diagnostic {
        code,
        message: format!(
            "Нарушена парность использования метода '{}' и '{}'",
            issue.pair_method, issue.method_name
        ),
        severity: ctx.severity(code),
        range: issue.range,
        tags: ctx.tags(code),
        fixes: vec![],
    }
}

#[cfg(test)]
mod tests {
    use crate::test_utils::check_diagnostics_snapshot_for;
    use crate::DiagnosticCode;
    use expect_test::expect;

    fn configured_diagnostics(code: &str) -> Vec<crate::Diagnostic> {
        crate::test_utils::check_with_cfe(code, test_fixture::CfeFixtureBuilder::new("").build())
            .into_iter()
            .filter(|diagnostic| diagnostic.code == DiagnosticCode::PairingBrokenTransaction)
            .collect()
    }

    #[test]
    fn platform_active_guards_preserve_method_namespace_and_loop_exits() {
        let code = include_str!("fixtures/transaction_active_guards.bsl");
        let diagnostics = configured_diagnostics(code);
        assert!(diagnostics.is_empty(), "{diagnostics:#?}");
    }

    #[test]
    fn shadowed_or_qualified_active_checks_do_not_prove_a_rollback() {
        let code = include_str!("fixtures/shadowed_transaction_active.bsl");
        let diagnostics = configured_diagnostics(code);
        assert_eq!(diagnostics.len(), 2, "{diagnostics:#?}");
    }

    #[test]
    fn full_reported_transaction_methods_do_not_emit() {
        let code = include_str!("fixtures/reported_transaction_methods.bsl");
        let diagnostics = configured_diagnostics(code);
        assert!(diagnostics.is_empty(), "{diagnostics:#?}");
    }

    #[test]
    fn unindexed_active_checks_do_not_prove_a_rollback() {
        let code = include_str!("fixtures/reported_transaction_methods.bsl");
        let diagnostics = crate::test_utils::check_diagnostics_for(
            code,
            DiagnosticCode::PairingBrokenTransaction,
        );
        assert_eq!(diagnostics.len(), 2, "{diagnostics:#?}");
    }

    #[test]
    fn real_unpaired_paths_still_emit() {
        let code = include_str!("fixtures/unpaired_transaction_paths.bsl");
        assert_eq!(configured_diagnostics(code).len(), 4);
    }
    #[test]
    fn test_valid_pairing_with_commit() {
        let code = r#"
Процедура Тест()
    НачатьТранзакцию();
    Действие();
    ЗафиксироватьТранзакцию();
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::PairingBrokenTransaction,
            expect![[r#""#]],
        );
    }

    #[test]
    fn test_valid_pairing_with_rollback() {
        let code = r#"
Процедура Тест()
    НачатьТранзакцию();
    Действие();
    ОтменитьТранзакцию();
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::PairingBrokenTransaction,
            expect![[r#""#]],
        );
    }

    #[test]
    fn test_rollback_then_commit_is_invalid() {
        let code = r#"
Процедура Тест()
    НачатьТранзакцию();
    Действие();
    ОтменитьТранзакцию();
    ЗафиксироватьТранзакцию();
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::PairingBrokenTransaction,
            expect![[r#"
                PairingBrokenTransaction @ 6:5..6:30
                  message: Нарушена парность использования метода 'НачатьТранзакцию' и 'ЗафиксироватьТранзакцию'
                  severity: Major"#]],
        );
    }

    #[test]
    fn test_orphaned_commit() {
        let code = r#"
Процедура Тест()
    Действие();
    ЗафиксироватьТранзакцию();
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::PairingBrokenTransaction,
            expect![[r#"
                PairingBrokenTransaction @ 4:5..4:30
                  message: Нарушена парность использования метода 'НачатьТранзакцию' и 'ЗафиксироватьТранзакцию'
                  severity: Major"#]],
        );
    }

    #[test]
    fn test_orphaned_begin() {
        let code = r#"
Процедура Тест()
    BeginTransaction();
    Действие();
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::PairingBrokenTransaction,
            expect![[r#"
                PairingBrokenTransaction @ 3:5..3:23
                  message: Нарушена парность использования метода 'ЗафиксироватьТранзакцию/ОтменитьТранзакцию' и 'BeginTransaction'
                  severity: Major"#]],
        );
    }

    #[test]
    fn test_nested_transactions() {
        let code = r#"
Процедура Тест()
    НачатьТранзакцию();
    НачатьТранзакцию();
    Действие();
    ЗафиксироватьТранзакцию();
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::PairingBrokenTransaction,
            expect![[r#"
                PairingBrokenTransaction @ 3:5..3:23
                  message: Нарушена парность использования метода 'ЗафиксироватьТранзакцию/ОтменитьТранзакцию' и 'НачатьТранзакцию'
                  severity: Major"#]],
        );
    }

    #[test]
    fn test_branch_imbalance() {
        let code = r#"
Процедура Тест(Условие)
    Если Условие Тогда
        НачатьТранзакцию();
    Иначе
        ЗафиксироватьТранзакцию();
    КонецЕсли;
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::PairingBrokenTransaction,
            expect![[r#"
                PairingBrokenTransaction @ 4:9..4:27
                  message: Нарушена парность использования метода 'ЗафиксироватьТранзакцию/ОтменитьТранзакцию' и 'НачатьТранзакцию'
                  severity: Major
                PairingBrokenTransaction @ 6:9..6:34
                  message: Нарушена парность использования метода 'НачатьТранзакцию' и 'ЗафиксироватьТранзакцию'
                  severity: Major"#]],
        );
    }

    #[test]
    fn test_standard_try_except_transaction_pattern() {
        let code = r#"
Процедура ОбновитьПоЗадаче(Задача)

    НачатьТранзакцию();
    Попытка

        Блокировка = Новый БлокировкаДанных;
        Блокировка.Заблокировать();

        НаборЗаписей = СоздатьНаборЗаписей();
        НаборЗаписей.Записать();

        ЗафиксироватьТранзакцию();

    Исключение
        ОтменитьТранзакцию();
        ВызватьИсключение;
    КонецПопытки;

КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::PairingBrokenTransaction,
            expect![[r#""#]],
        );
    }

    #[test]
    fn test_try_except_commit_rollback_no_raise() {
        let code = r#"
Процедура Тест()
    НачатьТранзакцию();
    Попытка
        Действие();
        ЗафиксироватьТранзакцию();
    Исключение
        ОтменитьТранзакцию();
    КонецПопытки;
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::PairingBrokenTransaction,
            expect![[r#""#]],
        );
    }

    #[test]
    fn test_try_except_only_rollback_invalid() {
        let code = r#"
Процедура Тест()
    НачатьТранзакцию();
    Попытка
        Действие();
        // Missing ЗафиксироватьТранзакцию here!
    Исключение
        ОтменитьТранзакцию();
    КонецПопытки;
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::PairingBrokenTransaction,
            expect![[r#"
                PairingBrokenTransaction @ 3:5..3:23
                  message: Нарушена парность использования метода 'ЗафиксироватьТранзакцию/ОтменитьТранзакцию' и 'НачатьТранзакцию'
                  severity: Major"#]],
        );
    }

    #[test]
    fn test_try_except_only_commit_invalid() {
        let code = r#"
Процедура Тест()
    НачатьТранзакцию();
    Попытка
        Действие();
        ЗафиксироватьТранзакцию();
    Исключение
        // Missing ОтменитьТранзакцию here!
        ЗаписатьОшибку();
    КонецПопытки;
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::PairingBrokenTransaction,
            expect![[r#"
                PairingBrokenTransaction @ 3:5..3:23
                  message: Нарушена парность использования метода 'ЗафиксироватьТранзакцию/ОтменитьТранзакцию' и 'НачатьТранзакцию'
                  severity: Major"#]],
        );
    }

    #[test]
    fn test_nested_try_except_valid() {
        let code = r#"
Процедура Тест()
    НачатьТранзакцию();
    Попытка
        Попытка
            Действие();
        Исключение
            ОбработатьОшибку();
        КонецПопытки;
        ЗафиксироватьТранзакцию();
    Исключение
        ОтменитьТранзакцию();
    КонецПопытки;
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::PairingBrokenTransaction,
            expect![[r#""#]],
        );
    }

    #[test]
    fn test_multiple_sequential_transactions_valid() {
        let code = r#"
Процедура Тест()
    // First transaction
    НачатьТранзакцию();
    Попытка
        Действие1();
        ЗафиксироватьТранзакцию();
    Исключение
        ОтменитьТранзакцию();
    КонецПопытки;

    // Second transaction
    НачатьТранзакцию();
    Попытка
        Действие2();
        ЗафиксироватьТранзакцию();
    Исключение
        ОтменитьТранзакцию();
    КонецПопытки;
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::PairingBrokenTransaction,
            expect![[r#""#]],
        );
    }

    #[test]
    fn test_conditional_inside_try_valid() {
        let code = r#"
Процедура Тест(Условие)
    НачатьТранзакцию();
    Попытка
        Если Условие Тогда
            Действие1();
        Иначе
            Действие2();
        КонецЕсли;
        ЗафиксироватьТранзакцию();
    Исключение
        ОтменитьТранзакцию();
    КонецПопытки;
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::PairingBrokenTransaction,
            expect![[r#""#]],
        );
    }

    #[test]
    fn test_early_return_before_commit_invalid() {
        let code = r#"
Процедура Тест(Условие)
    НачатьТранзакцию();
    Попытка
        Если Условие Тогда
            Возврат;  // Early return without commit!
        КонецЕсли;
        ЗафиксироватьТранзакцию();
    Исключение
        ОтменитьТранзакцию();
    КонецПопытки;
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::PairingBrokenTransaction,
            expect![[r#"
                PairingBrokenTransaction @ 3:5..3:23
                  message: Нарушена парность использования метода 'ЗафиксироватьТранзакцию/ОтменитьТранзакцию' и 'НачатьТранзакцию'
                  severity: Major"#]],
        );
    }

    #[test]
    fn test_raise_inside_try_transfers_to_except() {
        let code = r#"
Процедура Тест(Условие)
    НачатьТранзакцию();
    Попытка
        Если Условие Тогда
            ВызватьИсключение "Ошибка";
        КонецЕсли;
        ЗафиксироватьТранзакцию();
    Исключение
        ОтменитьТранзакцию();
        ВызватьИсключение;
    КонецПопытки;
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::PairingBrokenTransaction,
            expect![[r#""#]],
        );
    }

    #[test]
    fn test_raise_inside_nested_try() {
        let code = r#"
Процедура Тест(Условие)
    НачатьТранзакцию();
    Попытка
        Попытка
            Если Условие Тогда
                ВызватьИсключение "Внутренняя ошибка";
            КонецЕсли;
        Исключение
            // Inner except - does NOT rollback, just logs
            ЗаписатьОшибку();
        КонецПопытки;
        ЗафиксироватьТранзакцию();
    Исключение
        ОтменитьТранзакцию();
    КонецПопытки;
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::PairingBrokenTransaction,
            expect![[r#""#]],
        );
    }

    #[test]
    fn test_raise_outside_try_goes_to_exit() {
        let code = r#"
Процедура Тест(Условие)
    НачатьТранзакцию();
    Если Условие Тогда
        ВызватьИсключение "Ошибка без try";
    КонецЕсли;
    ЗафиксироватьТранзакцию();
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::PairingBrokenTransaction,
            expect![[r#"
                PairingBrokenTransaction @ 3:5..3:23
                  message: Нарушена парность использования метода 'ЗафиксироватьТранзакцию/ОтменитьТранзакцию' и 'НачатьТранзакцию'
                  severity: Major"#]],
        );
    }

    #[test]
    fn test_begin_inside_try_with_nested_raise() {
        let code = r#"
Процедура Тест()
    Попытка
        НачатьТранзакцию();
        Попытка
            Действие();
        Исключение
            ВызватьИсключение;
        КонецПопытки;
        ЗафиксироватьТранзакцию();
    Исключение
        ОтменитьТранзакцию();
        ВызватьИсключение;
    КонецПопытки;
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::PairingBrokenTransaction,
            expect![[r#""#]],
        );
    }

    #[test]
    fn test_break_after_commit_in_loop_valid() {
        let code = r#"
Процедура Тест()
    Пока Истина Цикл
        НачатьТранзакцию();
        Действие();
        ЗафиксироватьТранзакцию();
        Прервать;
    КонецЦикла;
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::PairingBrokenTransaction,
            expect![[r#""#]],
        );
    }

    #[test]
    fn test_break_with_open_transaction_invalid() {
        let code = r#"
Процедура Тест()
    Пока Истина Цикл
        НачатьТранзакцию();
        Действие();
        Прервать;
    КонецЦикла;
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::PairingBrokenTransaction,
            expect![[r#"
                PairingBrokenTransaction @ 4:9..4:27
                  message: Нарушена парность использования метода 'ЗафиксироватьТранзакцию/ОтменитьТранзакцию' и 'НачатьТранзакцию'
                  severity: Major"#]],
        );
    }

    #[test]
    fn test_multiple_commit_continue_in_loop() {
        let code = r#"
Процедура Тест()
    Пока Истина Цикл
        НачатьТранзакцию();
        Попытка
            Если Условие1 Тогда
                ЗафиксироватьТранзакцию();
                Продолжить;
            КонецЕсли;
            Если Условие2 Тогда
                ЗафиксироватьТранзакцию();
                Продолжить;
            КонецЕсли;
            ЗафиксироватьТранзакцию();
        Исключение
            ОтменитьТранзакцию();
        КонецПопытки;
    КонецЦикла;
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::PairingBrokenTransaction,
            expect![[r#""#]],
        );
    }

    #[test]
    fn test_fixture() {
        let code = r#"Процедура Проц1()
    // Парность соблюдается
    НачатьТранзакцию();
    Действие();
    ОтменитьТранзакцию();
    ЗафиксироватьТранзакцию();
КонецПроцедуры

Процедура Проц2()
    // Парность соблюдается
    BeginTransaction();
    Действие();
    RollbackTransaction();
    CommitTransaction();
КонецПроцедуры

Функция Функ1()
    // Парность соблюдается
    BeginTransaction();
    Действие();
    RollbackTransaction();
    CommitTransaction();
    Возврат Истина;
КонецФункции

Процедура Проц3()
    Действие();
    ЗафиксироватьТранзакцию(); // Парность не соблюдается здесь
КонецПроцедуры

Процедура Проц4()
    BeginTransaction(); // Парность не соблюдается здесь
    Действие();
КонецПроцедуры

Процедура Проц5()
    НачатьТранзакцию();
    Действие();
    ЗафиксироватьТранзакцию();
    ОтменитьТранзакцию();
    ЗафиксироватьТранзакцию(); // Парность не соблюдается здесь
КонецПроцедуры

Процедура Проц6()
    НачатьТранзакцию(); // Парность не соблюдается здесь для Зафаксировать И Отменить
    НачатьТранзакцию(); // Парность не соблюдается здесь для Отменить
    Действие();
    ЗафиксироватьТранзакцию();
КонецПроцедуры

Процедура Проц7()
    // Парность соблюдается
    НачатьТранзакцию(); // Парность не соблюдается здесь для Отменить
    НачатьТранзакцию(); // Парность не соблюдается здесь для Отменить
    Действие();
    ЗафиксироватьТранзакцию();
    НачатьТранзакцию(); // Парность не соблюдается здесь для Отменить
    ЗафиксироватьТранзакцию();
    ЗафиксироватьТранзакцию();
КонецПроцедуры

Процедура Проц8()
    // Парность соблюдается
    НачатьТранзакцию();
    Если Истина Тогда
        НачатьТранзакцию();
    КонецЕсли;
    Действие();
    Если Условие1() Тогда
        ЗафиксироватьТранзакцию();
    КонецЕсли;
    ОтменитьТранзакцию();
    НачатьТранзакцию();
    ЗафиксироватьТранзакцию();
    ОтменитьТранзакцию();
    НачатьТранзакцию();
    ЗафиксироватьТранзакцию();
    ОтменитьТранзакцию();
КонецПроцедуры

Процедура Проц8()
    НачатьТранзакцию();
    ЗафиксироватьТранзакцию();
    ОтменитьТранзакцию();
    ЗафиксироватьТранзакцию(); // Парность не соблюдается здесь
    НачатьТранзакцию();
    ЗафиксироватьТранзакцию();
    ОтменитьТранзакцию();
    ЗафиксироватьТранзакцию(); // Парность не соблюдается здесь
    ОтменитьТранзакцию(); // Парность не соблюдается здесь
КонецПроцедуры

Процедура Проц9()
    НачатьТранзакцию(); // Парность не соблюдается здесь
        НачатьТранзакцию(); // Парность не соблюдается здесь для отменить
        ЗафиксироватьТранзакцию();
        НачатьТранзакцию(); // Парность не соблюдается здесь для отменить
        ЗафиксироватьТранзакцию();
        НачатьТранзакцию();
        ЗафиксироватьТранзакцию();
    ЗафиксироватьТранзакцию();
    ОтменитьТранзакцию();
    ЗафиксироватьТранзакцию(); // Парность не соблюдается здесь
КонецПроцедуры

Процедура Проц9()
    НачатьТранзакцию(); // Парность не соблюдается здесь для отменить
        НачатьТранзакцию(); // Парность не соблюдается здесь для отменить
        ЗафиксироватьТранзакцию();
        НачатьТранзакцию(); // Парность не соблюдается здесь для отменить
        ЗафиксироватьТранзакцию();
        НачатьТранзакцию();
        ЗафиксироватьТранзакцию();
    ЗафиксироватьТранзакцию();
    зафиксироватьТРАНЗакциЮ(); // Парность не соблюдается здесь
    ОтменитьТранзакцию();
КонецПроцедуры"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::PairingBrokenTransaction,
            expect![[r#"
                PairingBrokenTransaction @ 6:5..6:30
                  message: Нарушена парность использования метода 'НачатьТранзакцию' и 'ЗафиксироватьТранзакцию'
                  severity: Major
                PairingBrokenTransaction @ 14:5..14:24
                  message: Нарушена парность использования метода 'НачатьТранзакцию' и 'CommitTransaction'
                  severity: Major
                PairingBrokenTransaction @ 22:5..22:24
                  message: Нарушена парность использования метода 'НачатьТранзакцию' и 'CommitTransaction'
                  severity: Major
                PairingBrokenTransaction @ 28:5..28:30
                  message: Нарушена парность использования метода 'НачатьТранзакцию' и 'ЗафиксироватьТранзакцию'
                  severity: Major
                PairingBrokenTransaction @ 32:5..32:23
                  message: Нарушена парность использования метода 'ЗафиксироватьТранзакцию/ОтменитьТранзакцию' и 'BeginTransaction'
                  severity: Major
                PairingBrokenTransaction @ 40:5..40:25
                  message: Нарушена парность использования метода 'НачатьТранзакцию' и 'ОтменитьТранзакцию'
                  severity: Major
                PairingBrokenTransaction @ 41:5..41:30
                  message: Нарушена парность использования метода 'НачатьТранзакцию' и 'ЗафиксироватьТранзакцию'
                  severity: Major
                PairingBrokenTransaction @ 45:5..45:23
                  message: Нарушена парность использования метода 'ЗафиксироватьТранзакцию/ОтменитьТранзакцию' и 'НачатьТранзакцию'
                  severity: Major
                PairingBrokenTransaction @ 72:5..72:25
                  message: Нарушена парность использования метода 'НачатьТранзакцию' и 'ОтменитьТранзакцию'
                  severity: Major
                PairingBrokenTransaction @ 75:5..75:25
                  message: Нарушена парность использования метода 'НачатьТранзакцию' и 'ОтменитьТранзакцию'
                  severity: Major
                PairingBrokenTransaction @ 78:5..78:25
                  message: Нарушена парность использования метода 'НачатьТранзакцию' и 'ОтменитьТранзакцию'
                  severity: Major
                PairingBrokenTransaction @ 84:5..84:25
                  message: Нарушена парность использования метода 'НачатьТранзакцию' и 'ОтменитьТранзакцию'
                  severity: Major
                PairingBrokenTransaction @ 85:5..85:30
                  message: Нарушена парность использования метода 'НачатьТранзакцию' и 'ЗафиксироватьТранзакцию'
                  severity: Major
                PairingBrokenTransaction @ 88:5..88:25
                  message: Нарушена парность использования метода 'НачатьТранзакцию' и 'ОтменитьТранзакцию'
                  severity: Major
                PairingBrokenTransaction @ 89:5..89:30
                  message: Нарушена парность использования метода 'НачатьТранзакцию' и 'ЗафиксироватьТранзакцию'
                  severity: Major
                PairingBrokenTransaction @ 90:5..90:25
                  message: Нарушена парность использования метода 'НачатьТранзакцию' и 'ОтменитьТранзакцию'
                  severity: Major
                PairingBrokenTransaction @ 102:5..102:25
                  message: Нарушена парность использования метода 'НачатьТранзакцию' и 'ОтменитьТранзакцию'
                  severity: Major
                PairingBrokenTransaction @ 103:5..103:30
                  message: Нарушена парность использования метода 'НачатьТранзакцию' и 'ЗафиксироватьТранзакцию'
                  severity: Major
                PairingBrokenTransaction @ 115:5..115:30
                  message: Нарушена парность использования метода 'НачатьТранзакцию' и 'зафиксироватьТРАНЗакциЮ'
                  severity: Major
                PairingBrokenTransaction @ 116:5..116:25
                  message: Нарушена парность использования метода 'НачатьТранзакцию' и 'ОтменитьТранзакцию'
                  severity: Major"#]],
        );
    }

    #[test]
    fn test_conditional_guard_begin_commit_rollback_no_fp() {
        let code = r#"
Процедура Тест()
    ЛокальнаяТранзакция = Не ТранзакцияАктивна();
    Если ЛокальнаяТранзакция Тогда
        НачатьТранзакцию();
    КонецЕсли;
    Попытка
        Действие();
        Если ЛокальнаяТранзакция Тогда
            ЗафиксироватьТранзакцию();
        КонецЕсли;
    Исключение
        Если ЛокальнаяТранзакция Тогда
            ОтменитьТранзакцию();
        КонецЕсли;
    КонецПопытки;
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::PairingBrokenTransaction,
            expect![[r#""#]],
        );
    }

    #[test]
    fn test_conditional_guard_commit_or_rollback_branches_no_fp() {
        let code = r#"
Процедура Тест(ЕстьИзменения)
    ЛокальнаяТранзакция = Не ТранзакцияАктивна();
    Если ЛокальнаяТранзакция Тогда
        НачатьТранзакцию();
    КонецЕсли;
    Попытка
        Если ЕстьИзменения Тогда
            Если ЛокальнаяТранзакция Тогда
                ЗафиксироватьТранзакцию();
            КонецЕсли;
        Иначе
            Если ЛокальнаяТранзакция Тогда
                ОтменитьТранзакцию();
            КонецЕсли;
        КонецЕсли;
    Исключение
        Если ЛокальнаяТранзакция Тогда
            ОтменитьТранзакцию();
        КонецЕсли;
    КонецПопытки;
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::PairingBrokenTransaction,
            expect![[r#""#]],
        );
    }

    #[test]
    fn test_distinct_guard_flags_still_flags() {
        let code = r#"
Процедура Тест(Флаг1, Флаг2)
    Если Флаг1 Тогда
        НачатьТранзакцию();
    КонецЕсли;
    Если Флаг2 Тогда
        ЗафиксироватьТранзакцию();
    КонецЕсли;
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::PairingBrokenTransaction,
            expect![[r#"
                PairingBrokenTransaction @ 4:9..4:27
                  message: Нарушена парность использования метода 'ЗафиксироватьТранзакцию/ОтменитьТранзакцию' и 'НачатьТранзакцию'
                  severity: Major
                PairingBrokenTransaction @ 7:9..7:34
                  message: Нарушена парность использования метода 'НачатьТранзакцию' и 'ЗафиксироватьТранзакцию'
                  severity: Major"#]],
        );
    }

    #[test]
    fn test_guarded_early_exit_rollbacks_no_fp() {
        let code = r#"
Процедура Тест(Условие1, Условие2)
    НачатьТранзакцию();
    Попытка
        Если Условие1 Тогда
            ОтменитьТранзакцию();
            Возврат;
        КонецЕсли;
        Если Условие2 Тогда
            ОтменитьТранзакцию();
            Возврат;
        КонецЕсли;
        ЗафиксироватьТранзакцию();
    Исключение
        ОтменитьТранзакцию();
    КонецПопытки;
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::PairingBrokenTransaction,
            expect![[r#""#]],
        );
    }

    #[test]
    fn test_interprocedural_begin_commit_are_not_paired_snapshot() {
        check_diagnostics_snapshot_for(
            r#"Процедура Открыть()
    НачатьТранзакцию();
КонецПроцедуры

Процедура Закрыть()
    ЗафиксироватьТранзакцию();
КонецПроцедуры"#,
            DiagnosticCode::PairingBrokenTransaction,
            expect![[r#"
                PairingBrokenTransaction @ 2:5..2:23
                  message: Нарушена парность использования метода 'ЗафиксироватьТранзакцию/ОтменитьТранзакцию' и 'НачатьТранзакцию'
                  severity: Major
                PairingBrokenTransaction @ 6:5..6:30
                  message: Нарушена парность использования метода 'НачатьТранзакцию' и 'ЗафиксироватьТранзакцию'
                  severity: Major"#]],
        );
    }
    #[test]
    fn test_flag_reassigned_between_guards_still_flags() {
        let code = r#"
Процедура Тест(Флаг)
    Если Флаг Тогда
        НачатьТранзакцию();
    КонецЕсли;
    Флаг = Ложь;
    Если Флаг Тогда
        ЗафиксироватьТранзакцию();
    КонецЕсли;
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::PairingBrokenTransaction,
            expect![[r#"
                PairingBrokenTransaction @ 4:9..4:27
                  message: Нарушена парность использования метода 'ЗафиксироватьТранзакцию/ОтменитьТранзакцию' и 'НачатьТранзакцию'
                  severity: Major"#]],
        );
    }

    #[test]
    fn test_double_rollback_via_guarded_raise_flags() {
        let code = r#"
Процедура Тест(ПравоДоступа, СертификатСсылка)
    Если Не ПравоДоступа Тогда
        Подготовить();
    КонецЕсли;
    НачатьТранзакцию();
    Попытка
        Если Не ПравоДоступа Тогда
            ОтменитьТранзакцию();
            ВызватьИсключение "Нет прав";
        КонецЕсли;
        Действие();
        ЗафиксироватьТранзакцию();
    Исключение
        ОтменитьТранзакцию();
        ВызватьИсключение;
    КонецПопытки;
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::PairingBrokenTransaction,
            expect![[r#"
            PairingBrokenTransaction @ 15:9..15:29
              message: Нарушена парность использования метода 'НачатьТранзакцию' и 'ОтменитьТранзакцию'
              severity: Major"#]],
        );
    }
}
