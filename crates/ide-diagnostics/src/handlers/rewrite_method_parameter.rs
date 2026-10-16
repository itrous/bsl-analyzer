use crate::define_metadata;
use crate::metadata::*;
use crate::BodyContext;
use crate::{Diagnostic, DiagnosticCode};
use hir::LocalRange;
use hir::{BindingId, ExprId, IdConversion, StmtId};
use std::collections::{HashSet, VecDeque};
use stdx::case::{eq_ignore_case, CaseExt};

pub const METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: DiagnosticType::CodeSmell,
    severity: DiagnosticSeverityLevel::Major,
    scope: DiagnosticScope::All,
    modules: &[],
    minutes_to_fix: 2,
    activated_by_default: true,
    compatibility_mode: DiagnosticCompatibilityMode::Undefined,
    tags: &[MetadataTag::Suspicious],
    can_locate_on_project: false,
    extra_min_for_complexity: 0.0,
    lsp_severity_override: "",
};

pub fn from_hir(
    param_id: BindingId,
    _stmt_id: StmtId,
    stmt_range: LocalRange,
    ident_range: LocalRange,
    ctx: &BodyContext,
) -> Option<Diagnostic<LocalRange>> {
    let code = DiagnosticCode::RewriteMethodParameter;

    if ctx.is_disabled_with_metadata(code) {
        return None;
    }

    let body = ctx.body();
    let stmt_id = ctx.source_map().stmt_at_range(stmt_range)?;

    let param = body.binding(param_id);
    let param_name = param.name.as_str();

    let reaching_defs_result = ctx.reaching_definitions()?;
    let reaching_defs_set = reaching_defs_result.defs_before_stmt(stmt_id)?;
    let param_name_lower = param_name.fold_lower();
    let param_definitions: Vec<_> =
        reaching_defs_set.iter().filter(|def| def.var_name.as_str() == param_name_lower).collect();

    let eligible_entry = if target_is_inside_while(body, stmt_id) {
        has_unwritten_entry_path(&ctx.cfg(), body, stmt_id, param_id)
    } else if param_definitions.len() == 1 {
        matches!(
            param_definitions[0].def_site,
            hir::dataflow::reaching_defs::DefSite::Parameter(def_binding_id)
                if def_binding_id == param_id
        )
    } else {
        false
    };

    if parameter_used_in_assignment_rhs(body, stmt_id, param_id)
        || parameter_used_before_stmt(body, stmt_id, param_id)
        || !eligible_entry
    {
        return None;
    }

    Some(Diagnostic {
        code,
        message: format!("Переприсваивание параметра метода '{}'", param_name),
        severity: ctx.severity(code),
        range: ident_range,
        tags: ctx.tags(code),
        fixes: vec![],
    })
}

fn target_is_inside_while(body: &hir::Body, target_stmt_id: StmtId) -> bool {
    fn contains_target_in_loop(
        body: &hir::Body,
        stmts: impl IntoIterator<Item = StmtId>,
        target_stmt_id: StmtId,
    ) -> bool {
        use hir::Stmt;

        for stmt_id in stmts {
            if stmt_id == target_stmt_id {
                continue;
            }
            match body.stmt(stmt_id) {
                Stmt::While { body: loop_body, .. } => {
                    if stmts_contain_target(body, loop_body, target_stmt_id) {
                        return true;
                    }
                }
                Stmt::If(if_stmt) => {
                    if contains_target_in_loop(body, ids(&if_stmt.then_branch), target_stmt_id)
                        || if_stmt.elsif_branches.iter().any(|(_, branch)| {
                            contains_target_in_loop(body, ids(branch), target_stmt_id)
                        })
                        || if_stmt.else_branch.as_ref().is_some_and(|branch| {
                            contains_target_in_loop(body, ids(branch), target_stmt_id)
                        })
                    {
                        return true;
                    }
                }
                Stmt::PreprocIf(preproc) => {
                    if preproc.branches().any(|branch| {
                        contains_target_in_loop(body, ids(branch.stmts), target_stmt_id)
                    }) {
                        return true;
                    }
                }
                Stmt::For { body: loop_body, .. } | Stmt::ForEach { body: loop_body, .. } => {
                    if contains_target_in_loop(body, ids(loop_body), target_stmt_id) {
                        return true;
                    }
                }
                Stmt::Try { body: try_body, except }
                    if contains_target_in_loop(body, ids(try_body), target_stmt_id)
                        || contains_target_in_loop(body, ids(except), target_stmt_id) =>
                {
                    return true;
                }
                _ => {}
            }
        }
        false
    }

    fn ids(stmts: &[hir::StmtIdx]) -> impl Iterator<Item = StmtId> + '_ {
        stmts.iter().copied().map(StmtId::from_idx)
    }

    contains_target_in_loop(body, body.body_stmts(), target_stmt_id)
}

/// Keep loop-carried assignments separate from paths that still carry the
/// parameter's entry value. The two states reach a loop header independently.
fn has_unwritten_entry_path(
    cfg: &hir::cfg::ControlFlowGraph,
    body: &hir::Body,
    target_stmt_id: StmtId,
    param_id: BindingId,
) -> bool {
    let Some(start) = cfg.start() else { return false };
    let mut pending = VecDeque::from([(start, true)]);
    let mut visited = HashSet::new();

    while let Some((vertex_id, entry_value_intact)) = pending.pop_front() {
        if !visited.insert((vertex_id, entry_value_intact)) {
            continue;
        }

        let entry_value_intact =
            entry_value_intact && !loop_header_writes_binding(cfg, body, vertex_id, param_id);

        if let Some(hir::cfg::CfgVertex::BasicBlock(block)) = cfg.vertex(vertex_id) {
            let mut entry_value_intact = entry_value_intact;
            for &stmt_id in block.statements() {
                if stmt_id == target_stmt_id {
                    if entry_value_intact {
                        return true;
                    }
                    break;
                }
                if is_real_assignment_to_binding(body, stmt_id, param_id) {
                    entry_value_intact = false;
                }
            }
            // A target with no entry value cannot regain it on a later iteration.
            if block.statements().contains(&target_stmt_id) {
                continue;
            }
            for (next, edge_type) in cfg.outgoing_edges(vertex_id) {
                if edge_type.is_executable() {
                    pending.push_back((next, entry_value_intact));
                }
            }
        } else {
            for (next, edge_type) in cfg.outgoing_edges(vertex_id) {
                if edge_type.is_executable() {
                    pending.push_back((next, entry_value_intact));
                }
            }
        }
    }

    false
}

fn loop_header_writes_binding(
    cfg: &hir::cfg::ControlFlowGraph,
    body: &hir::Body,
    vertex_id: hir::cfg::NodeIndex,
    binding_id: BindingId,
) -> bool {
    let Some(vertex) = cfg.vertex(vertex_id) else { return false };
    let loop_var = match vertex {
        hir::cfg::CfgVertex::ForHeader(header) => header.loop_var,
        hir::cfg::CfgVertex::ForEachHeader(header) => header.loop_var,
        _ => return false,
    };

    eq_ignore_case(body.binding(loop_var).name.as_str(), body.binding(binding_id).name.as_str())
}

fn is_real_assignment_to_binding(body: &hir::Body, stmt_id: StmtId, binding_id: BindingId) -> bool {
    use hir::{Expr, Stmt};

    let Stmt::Assign { target, .. } = body.stmt(stmt_id) else { return false };
    let Expr::Path(target_name) = body.expr(ExprId::from_idx(*target)) else { return false };
    let binding_name = body.binding(binding_id).name.as_str();
    eq_ignore_case(target_name.as_str(), binding_name)
        && !is_self_assign_to_binding(body, stmt_id, binding_id)
}

/// Outcome of scanning statements in execution order looking for a read of the
/// parameter that happens before the rewrite.
enum Scan {
    /// No read encountered yet; keep scanning following statements.
    NotFound,
    /// The parameter is read before the rewrite — the rewrite is not blind.
    ReadFound,
    /// Reached the rewrite statement; nothing executes after it counts as "before".
    ReachedTarget,
}

/// A by-value parameter is only suspicious when it is overwritten *before* being
/// read. A flat statement-arena scan misses reads that live in the header of an
/// enclosing block (`Если Параметр = Неопределено Тогда Параметр = …`), which is
/// the idiomatic way to compute a non-literal default. Walk the statement tree in
/// execution order instead and stop at the rewrite itself.
fn parameter_used_before_stmt(
    body: &hir::Body,
    target_stmt_id: StmtId,
    param_id: BindingId,
) -> bool {
    let top_level: Vec<StmtId> = body.body_stmts().collect();
    matches!(scan_stmts_before_target(body, &top_level, target_stmt_id, param_id), Scan::ReadFound)
}

fn scan_stmts_before_target(
    body: &hir::Body,
    stmts: &[StmtId],
    target_stmt_id: StmtId,
    param_id: BindingId,
) -> Scan {
    for &stmt_id in stmts {
        match scan_stmt_before_target(body, stmt_id, target_stmt_id, param_id) {
            Scan::NotFound => {}
            other => return other,
        }
    }
    Scan::NotFound
}

fn scan_idx_slice(
    body: &hir::Body,
    stmts: &[hir::StmtIdx],
    target_stmt_id: StmtId,
    param_id: BindingId,
) -> Scan {
    let ids: Vec<StmtId> = stmts.iter().map(|&idx| StmtId::from_idx(idx)).collect();
    scan_stmts_before_target(body, &ids, target_stmt_id, param_id)
}

fn scan_stmt_before_target(
    body: &hir::Body,
    stmt_id: StmtId,
    target_stmt_id: StmtId,
    param_id: BindingId,
) -> Scan {
    use hir::{Expr, Stmt};

    if stmt_id == target_stmt_id {
        return Scan::ReachedTarget;
    }

    let reads =
        |expr_idx: hir::ExprIdx| expr_uses_binding(body, ExprId::from_idx(expr_idx), param_id);

    match body.stmt(stmt_id) {
        Stmt::If(if_stmt) => scan_if_before_target(body, if_stmt, target_stmt_id, param_id),
        Stmt::PreprocIf(preproc) => {
            for branch in preproc.branches() {
                match scan_idx_slice(body, branch.stmts, target_stmt_id, param_id) {
                    Scan::NotFound => {}
                    other => return other,
                }
            }
            Scan::NotFound
        }
        Stmt::While { condition, body: loop_body } => {
            if reads(*condition) {
                return Scan::ReadFound;
            }
            scan_idx_slice(body, loop_body, target_stmt_id, param_id)
        }
        Stmt::For { from, to, body: loop_body, .. } => {
            if reads(*from) || reads(*to) {
                return Scan::ReadFound;
            }
            scan_idx_slice(body, loop_body, target_stmt_id, param_id)
        }
        Stmt::ForEach { collection, body: loop_body, .. } => {
            if reads(*collection) {
                return Scan::ReadFound;
            }
            scan_idx_slice(body, loop_body, target_stmt_id, param_id)
        }
        Stmt::Try { body: try_body, except } => {
            match scan_idx_slice(body, try_body, target_stmt_id, param_id) {
                Scan::NotFound => {}
                other => return other,
            }
            scan_idx_slice(body, except, target_stmt_id, param_id)
        }
        Stmt::Assign { target, value } => {
            // Self-assignments (`П = П`) are not meaningful use.
            if is_self_assign_to_binding(body, stmt_id, param_id) {
                return Scan::NotFound;
            }
            if reads(*value) {
                return Scan::ReadFound;
            }
            // A bare write target (`П = …`) is not a read, but a member/index target
            // (`П.Поле = …`, `П[i] = …`) reads the parameter.
            let target_expr = body.expr(ExprId::from_idx(*target));
            let target_is_bare_param = matches!(
                target_expr,
                Expr::Path(name)
                    if eq_ignore_case(name.as_str(), body.binding(param_id).name.as_str())
            );
            if !target_is_bare_param && reads(*target) {
                return Scan::ReadFound;
            }
            Scan::NotFound
        }
        _ => {
            if stmt_uses_binding(body, stmt_id, param_id) {
                Scan::ReadFound
            } else {
                Scan::NotFound
            }
        }
    }
}

/// Path-sensitive scan of an `If` for a read that precedes the rewrite.
///
/// Reads in a guarding condition always count: every condition up to the
/// branch that owns the rewrite is evaluated before it runs. Reads in a branch
/// *body* only count when that body either contains the rewrite (the read
/// precedes it on the same path) or the whole `If` precedes the rewrite (the
/// rewrite lives in a following sibling, so the `If` fully executes first).
/// A read in a sibling branch that is mutually exclusive with the rewrite's
/// branch must NOT suppress the diagnostic.
fn scan_if_before_target(
    body: &hir::Body,
    if_stmt: &hir::IfStmt,
    target_stmt_id: StmtId,
    param_id: BindingId,
) -> Scan {
    let reads =
        |expr_idx: hir::ExprIdx| expr_uses_binding(body, ExprId::from_idx(expr_idx), param_id);

    let target_in_if = if_contains_target(body, if_stmt, target_stmt_id);

    if reads(if_stmt.condition) {
        return Scan::ReadFound;
    }
    match scan_if_branch(body, &if_stmt.then_branch, target_stmt_id, param_id, target_in_if) {
        Scan::NotFound => {}
        other => return other,
    }
    for (cond, branch) in if_stmt.elsif_branches.iter() {
        if reads(*cond) {
            return Scan::ReadFound;
        }
        match scan_if_branch(body, branch, target_stmt_id, param_id, target_in_if) {
            Scan::NotFound => {}
            other => return other,
        }
    }
    if let Some(branch) = &if_stmt.else_branch {
        match scan_if_branch(body, branch, target_stmt_id, param_id, target_in_if) {
            Scan::NotFound => {}
            other => return other,
        }
    }
    Scan::NotFound
}

fn scan_if_branch(
    body: &hir::Body,
    branch: &[hir::StmtIdx],
    target_stmt_id: StmtId,
    param_id: BindingId,
    target_in_if: bool,
) -> Scan {
    if stmts_contain_target(body, branch, target_stmt_id) {
        // The rewrite lives in this branch — its result is authoritative.
        return scan_idx_slice(body, branch, target_stmt_id, param_id);
    }
    // The rewrite is not in this branch. A read here only counts when the whole
    // `If` precedes the rewrite; otherwise this branch is a mutually exclusive
    // sibling of the rewrite's branch and must be ignored.
    match scan_idx_slice(body, branch, target_stmt_id, param_id) {
        Scan::ReadFound if !target_in_if => Scan::ReadFound,
        _ => Scan::NotFound,
    }
}

fn if_contains_target(body: &hir::Body, if_stmt: &hir::IfStmt, target_stmt_id: StmtId) -> bool {
    stmts_contain_target(body, &if_stmt.then_branch, target_stmt_id)
        || if_stmt
            .elsif_branches
            .iter()
            .any(|(_, branch)| stmts_contain_target(body, branch, target_stmt_id))
        || if_stmt
            .else_branch
            .as_ref()
            .is_some_and(|branch| stmts_contain_target(body, branch, target_stmt_id))
}

fn stmts_contain_target(body: &hir::Body, stmts: &[hir::StmtIdx], target_stmt_id: StmtId) -> bool {
    stmts.iter().any(|&idx| stmt_contains_target(body, StmtId::from_idx(idx), target_stmt_id))
}

fn stmt_contains_target(body: &hir::Body, stmt_id: StmtId, target_stmt_id: StmtId) -> bool {
    use hir::Stmt;

    if stmt_id == target_stmt_id {
        return true;
    }
    match body.stmt(stmt_id) {
        Stmt::If(if_stmt) => if_contains_target(body, if_stmt, target_stmt_id),
        Stmt::PreprocIf(preproc) => preproc
            .branches()
            .any(|branch| stmts_contain_target(body, branch.stmts, target_stmt_id)),
        Stmt::While { body: loop_body, .. }
        | Stmt::For { body: loop_body, .. }
        | Stmt::ForEach { body: loop_body, .. } => {
            stmts_contain_target(body, loop_body, target_stmt_id)
        }
        Stmt::Try { body: try_body, except } => {
            stmts_contain_target(body, try_body, target_stmt_id)
                || stmts_contain_target(body, except, target_stmt_id)
        }
        _ => false,
    }
}

fn is_self_assign_to_binding(body: &hir::Body, stmt_id: StmtId, binding_id: BindingId) -> bool {
    use hir::{Expr, Stmt};

    let stmt = body.stmt(stmt_id);
    match stmt {
        Stmt::Assign { target, value } => {
            if let Expr::Path(target_name) = body.expr(ExprId::from_idx(*target)) {
                let binding = body.binding(binding_id);
                if !eq_ignore_case(target_name.as_str(), binding.name.as_str()) {
                    return false;
                }

                if let Expr::Path(value_name) = body.expr(ExprId::from_idx(*value)) {
                    return eq_ignore_case(value_name.as_str(), binding.name.as_str());
                }
            }
            false
        }
        _ => false,
    }
}

fn stmt_uses_binding(body: &hir::Body, stmt_id: StmtId, binding_id: BindingId) -> bool {
    use hir::Stmt;

    let stmt = body.stmt(stmt_id);
    match stmt {
        Stmt::Expr(expr_id) => expr_uses_binding(body, ExprId::from_idx(*expr_id), binding_id),
        Stmt::Assign { target, value } => {
            expr_uses_binding(body, ExprId::from_idx(*target), binding_id)
                || expr_uses_binding(body, ExprId::from_idx(*value), binding_id)
        }
        Stmt::If(if_stmt) => {
            if expr_uses_binding(body, ExprId::from_idx(if_stmt.condition), binding_id) {
                return true;
            }
            for &stmt_idx in if_stmt.then_branch.iter() {
                if stmt_uses_binding(body, StmtId::from_idx(stmt_idx), binding_id) {
                    return true;
                }
            }
            for (cond, branch) in if_stmt.elsif_branches.iter() {
                if expr_uses_binding(body, ExprId::from_idx(*cond), binding_id) {
                    return true;
                }
                for &stmt_idx in branch.iter() {
                    if stmt_uses_binding(body, StmtId::from_idx(stmt_idx), binding_id) {
                        return true;
                    }
                }
            }
            if let Some(ref branch) = if_stmt.else_branch {
                for &stmt_idx in branch.iter() {
                    if stmt_uses_binding(body, StmtId::from_idx(stmt_idx), binding_id) {
                        return true;
                    }
                }
            }
            false
        }
        Stmt::While { condition, body: loop_body } => {
            if expr_uses_binding(body, ExprId::from_idx(*condition), binding_id) {
                return true;
            }
            for &stmt_idx in loop_body.iter() {
                if stmt_uses_binding(body, StmtId::from_idx(stmt_idx), binding_id) {
                    return true;
                }
            }
            false
        }
        Stmt::For { from, to, body: loop_body, .. } => {
            if expr_uses_binding(body, ExprId::from_idx(*from), binding_id)
                || expr_uses_binding(body, ExprId::from_idx(*to), binding_id)
            {
                return true;
            }
            for &stmt_idx in loop_body.iter() {
                if stmt_uses_binding(body, StmtId::from_idx(stmt_idx), binding_id) {
                    return true;
                }
            }
            false
        }
        Stmt::ForEach { collection, body: loop_body, .. } => {
            if expr_uses_binding(body, ExprId::from_idx(*collection), binding_id) {
                return true;
            }
            for &stmt_idx in loop_body.iter() {
                if stmt_uses_binding(body, StmtId::from_idx(stmt_idx), binding_id) {
                    return true;
                }
            }
            false
        }
        Stmt::Try { body: try_body, except } => {
            for &stmt_idx in try_body.iter() {
                if stmt_uses_binding(body, StmtId::from_idx(stmt_idx), binding_id) {
                    return true;
                }
            }
            for &stmt_idx in except.iter() {
                if stmt_uses_binding(body, StmtId::from_idx(stmt_idx), binding_id) {
                    return true;
                }
            }
            false
        }
        Stmt::Return { value: Some(expr_id) } => {
            expr_uses_binding(body, ExprId::from_idx(*expr_id), binding_id)
        }
        Stmt::Return { value: None } => false,
        Stmt::Raise { value: Some(expr_id) } => {
            expr_uses_binding(body, ExprId::from_idx(*expr_id), binding_id)
        }
        Stmt::Raise { value: None } => false,
        Stmt::Execute { expr } => expr_uses_binding(body, ExprId::from_idx(*expr), binding_id),
        Stmt::AddHandler { event, handler } => {
            expr_uses_binding(body, ExprId::from_idx(*event), binding_id)
                || expr_uses_binding(body, ExprId::from_idx(*handler), binding_id)
        }
        Stmt::RemoveHandler { event, handler } => {
            expr_uses_binding(body, ExprId::from_idx(*event), binding_id)
                || expr_uses_binding(body, ExprId::from_idx(*handler), binding_id)
        }
        _ => false,
    }
}

fn parameter_used_in_assignment_rhs(
    body: &hir::Body,
    stmt_id: StmtId,
    param_id: BindingId,
) -> bool {
    use hir::Stmt;

    let stmt = body.stmt(stmt_id);
    let value_expr_id = match stmt {
        Stmt::Assign { value, .. } => ExprId::from_idx(*value),
        _ => return false,
    };

    expr_uses_binding(body, value_expr_id, param_id)
}

fn expr_uses_binding(body: &hir::Body, expr_id: ExprId, binding_id: BindingId) -> bool {
    use hir::Expr;

    let uses = |idx: hir::ExprIdx| expr_uses_binding(body, ExprId::from_idx(idx), binding_id);

    match body.expr(expr_id) {
        Expr::Path(name) => {
            let binding = body.binding(binding_id);
            eq_ignore_case(name.as_str(), binding.name.as_str())
        }
        Expr::BinaryOp { lhs, rhs, .. } => uses(*lhs) || uses(*rhs),
        Expr::UnaryOp { expr, .. } => uses(*expr),
        Expr::Call { callee, args } => uses(*callee) || args.iter().any(|&arg| uses(arg)),
        Expr::MethodCall { receiver, args, .. } => {
            uses(*receiver) || args.iter().any(|&arg| uses(arg))
        }
        Expr::Index { base, index } => uses(*base) || uses(*index),
        Expr::Field { base, .. } => uses(*base),
        Expr::New { args, .. } => args.iter().any(|&arg| uses(arg)),
        Expr::Array(elements) => elements.iter().any(|&arg| uses(arg)),
        Expr::Ternary { condition, then_expr, else_expr } => {
            uses(*condition) || uses(*then_expr) || uses(*else_expr)
        }
        Expr::Await { expr } => uses(*expr),
        Expr::Literal(_) | Expr::Missing => false,
    }
}

#[cfg(test)]
mod tests {
    use crate::test_utils::{
        assert_diagnostic_range, check_diagnostics_for, check_diagnostics_snapshot_for,
    };
    use crate::DiagnosticCode;
    use expect_test::expect;

    fn check(code: &str, expected: expect_test::Expect) {
        check_diagnostics_snapshot_for(code, DiagnosticCode::RewriteMethodParameter, expected);
    }

    fn assert_targets(code: &str, expected_lines: &[u32]) {
        let diagnostics = check_diagnostics_for(code, DiagnosticCode::RewriteMethodParameter);
        assert_eq!(diagnostics.len(), expected_lines.len());

        for (diagnostic, &line) in diagnostics.iter().zip(expected_lines) {
            let assignment = code
                .lines()
                .nth(line as usize)
                .expect("expected target line")
                .find("Уставка =")
                .expect("expected target assignment");
            let start_col = code.lines().nth(line as usize).expect("expected target line")
                [..assignment]
                .chars()
                .count() as u32;
            assert_diagnostic_range(
                code,
                diagnostic,
                line,
                start_col,
                start_col + "Уставка".chars().count() as u32,
            );
            let start: u32 = diagnostic.range.start().into();
            let end: u32 = diagnostic.range.end().into();
            assert_eq!(&code[start as usize..end as usize], "Уставка");
        }
    }

    #[test]
    fn test_s7_first_blind_write_in_while_reports_exact_target() {
        let code = r#"Процедура Установить(Знач Уставка, Знач Повторять)
	Пока Повторять Цикл
		Уставка = 21;
	КонецЦикла;
КонецПроцедуры
"#;

        assert_targets(code, &[2]);
    }

    #[test]
    fn test_s8_nested_branches_report_first_writes_once() {
        let code = r#"Процедура Установить(Знач Уставка, Знач Повторять, Знач Ветка, Знач ВложенныйЦикл, Знач ЕщёВетка)
	Пока Повторять Цикл
		Если Ветка Тогда
			Уставка = 1;
			Уставка = 2;
		Иначе
			Пока ВложенныйЦикл Цикл
				Если ЕщёВетка Тогда
					Уставка = 3;
					Уставка = 4;
				Иначе
					Уставка = 5;
					Уставка = 6;
				КонецЕсли;
			КонецЦикла;
		КонецЕсли;
	КонецЦикла;
КонецПроцедуры
"#;

        assert_targets(code, &[3, 8, 11]);
    }

    #[test]
    fn test_s9_read_before_condition_and_rhs_suppress_but_later_and_sibling_reads_do_not() {
        let code = r#"Процедура ДоЗаписи(Знач Уставка, Знач Повторять)
	Журнал.Добавить(Уставка);
	Пока Повторять Цикл
		Уставка = 1;
	КонецЦикла;
КонецПроцедуры

Процедура Условие(Знач Уставка)
	Пока Уставка > 0 Цикл
		Уставка = 1;
	КонецЦикла;
КонецПроцедуры

Процедура ПраваяЧасть(Знач Уставка, Знач Повторять)
	Пока Повторять Цикл
		Уставка = Уставка + 1;
	КонецЦикла;
КонецПроцедуры

Процедура ПослеЗаписи(Знач Уставка, Знач Повторять)
	Пока Повторять Цикл
		Уставка = 1;
		Журнал.Добавить(Уставка);
	КонецЦикла;
КонецПроцедуры

Процедура СоседняяВетвь(Знач Уставка, Знач Повторять, Знач Ветка)
	Пока Повторять Цикл
		Если Ветка Тогда
			Журнал.Добавить(Уставка);
		Иначе
			Уставка = 1;
		КонецЕсли;
	КонецЦикла;
КонецПроцедуры
"#;

        assert_targets(code, &[21, 31]);
    }

    #[test]
    fn test_s10_prior_write_suppresses_next_write_but_prior_self_assign_does_not() {
        let code = r#"Процедура РеальнаяЗапись(Знач Уставка, Знач Повторять)
	Уставка = 5;
	Пока Повторять Цикл
		Уставка = 21;
	КонецЦикла;
КонецПроцедуры

Процедура СамоприсваиваниеДоЦикла(Знач Уставка, Знач Повторять)
	Уставка = Уставка;
	Пока Повторять Цикл
		Уставка = 21;
	КонецЦикла;
КонецПроцедуры
"#;

        assert_targets(code, &[1, 10]);
    }

    #[test]
    fn test_s10_self_assign_inside_while_does_not_hide_first_real_write() {
        let code = r#"Процедура Установить(Знач Уставка, Знач Повторять)
	Пока Повторять Цикл
		Уставка = Уставка;
		Уставка = 21;
	КонецЦикла;
КонецПроцедуры
"#;

        assert_targets(code, &[3]);
    }

    #[test]
    fn test_s10_by_ref_parameter_is_silent_inside_while() {
        let code = r#"Процедура Установить(Уставка, Знач Повторять)
	Пока Повторять Цикл
		Уставка = 21;
	КонецЦикла;
КонецПроцедуры
"#;

        assert_targets(code, &[]);
    }

    #[test]
    fn test_s11_break_and_continue_keep_unreachable_targets_silent() {
        let code = r#"Процедура ПослеПрерывания(Знач Уставка, Знач Повторять)
	Пока Повторять Цикл
		Прервать;
		Уставка = 1;
	КонецЦикла;
КонецПроцедуры

Процедура ПослеПродолжения(Знач Уставка, Знач Повторять)
	Пока Повторять Цикл
		Продолжить;
		Уставка = 1;
	КонецЦикла;
КонецПроцедуры

Процедура ДоступныйПослеВетвиПродолжения(Знач Уставка, Знач Повторять, Знач Пропустить)
	Пока Повторять Цикл
		Если Пропустить Тогда
			Продолжить;
		КонецЕсли;
		Уставка = 1;
	КонецЦикла;
КонецПроцедуры
"#;

        assert_targets(code, &[19]);
    }

    #[test]
    fn test_s11_non_while_reaching_definition_guards_remain_unchanged() {
        let code = r#"Процедура ПослеВетви(Знач Уставка, Знач Флаг)
	Если Флаг Тогда
		Уставка = 1;
	КонецЕсли;
	Уставка = 2;
КонецПроцедуры

Процедура ВЦиклеДля(Знач Уставка, Знач Количество)
	Для Номер = 1 По Количество Цикл
		Уставка = 1;
	КонецЦикла;
КонецПроцедуры

Процедура ВЦиклеДляКаждого(Знач Уставка, Знач Элементы)
	Для Каждого Элемент Из Элементы Цикл
		Уставка = 1;
	КонецЦикла;
КонецПроцедуры
"#;

        assert_targets(code, &[2]);
    }

    #[test]
    fn test_s11_for_headers_replace_parameter_before_nested_while() {
        let code = r#"Процедура ЦиклДля(Знач Уставка, Знач Количество, Знач Повторять)
	Для Уставка = 1 По Количество Цикл
		Пока Повторять Цикл
			Уставка = 21;
		КонецЦикла;
	КонецЦикла;
КонецПроцедуры

Процедура ЦиклДляКаждого(Знач Уставка, Знач Значения, Знач Повторять)
	Для Каждого Уставка Из Значения Цикл
		Пока Повторять Цикл
			Уставка = 21;
		КонецЦикла;
	КонецЦикла;
КонецПроцедуры
"#;

        assert_targets(code, &[]);
    }

    #[test]
    fn test_read_before_write_is_silent() {
        let code = r#"Процедура Установить(Знач Уставка)
	Журнал.Добавить(Уставка);
	Уставка = 21;
КонецПроцедуры
"#;
        check(code, expect![[r#""#]]);
    }

    #[test]
    fn test_simple_overwrite() {
        let code = r#"Процедура Установить(Знач Уставка)
	Уставка = 21;
КонецПроцедуры
"#;
        check(
            code,
            expect![[r#"
            RewriteMethodParameter @ 2:2..2:9
              message: Переприсваивание параметра метода 'Уставка'
              severity: Warning"#]],
        );
    }

    #[test]
    fn test_no_diagnostic_for_by_ref() {
        let code = r#"Процедура Установить(Уставка, Знач Режим)
	Уставка = 21;
КонецПроцедуры
"#;
        check(code, expect![[r#""#]]);
    }

    #[test]
    fn test_default_parameter_overwrite() {
        let code = r#"Процедура Установить(Знач Уставка = 20, Знач Гистерезис = 1)
	Уставка = 22;
КонецПроцедуры
"#;
        check(
            code,
            expect![[r#"
            RewriteMethodParameter @ 2:2..2:9
              message: Переприсваивание параметра метода 'Уставка'
              severity: Warning"#]],
        );
    }

    #[test]
    fn test_self_assign_no_diagnostic() {
        let code = r#"Процедура Установить(Знач Уставка)
	Уставка = Уставка;
КонецПроцедуры
"#;
        check(code, expect![[r#""#]]);
    }

    #[test]
    fn test_self_assign_does_not_count_as_use() {
        // Self-assignments are skipped; the overwrite after them is the first real use.
        let code = r#"Процедура Установить(Знач Уставка, Знач Режим)
	Уставка = Уставка;
	Уставка = 21;
	Режим = Режим;
	Режим = Режим;
	Режим = "Авто";
КонецПроцедуры
"#;
        check(
            code,
            expect![[r#"
            RewriteMethodParameter @ 3:2..3:9
              message: Переприсваивание параметра метода 'Уставка'
              severity: Warning
            RewriteMethodParameter @ 6:2..6:7
              message: Переприсваивание параметра метода 'Режим'
              severity: Warning"#]],
        );
    }

    #[test]
    fn test_used_in_expression_no_diagnostic() {
        let code = r#"Процедура Установить(Знач Уставка, Знач Поправка, Знач Датчик)
	Уставка = Округлить(Уставка);
	Поправка = 0.5 * Поправка;
	Датчик = Датчик.Резервный;
КонецПроцедуры
"#;
        check(code, expect![[r#""#]]);
    }

    #[test]
    fn test_member_writes_and_reads_are_uses() {
        // Writing or reading a member of the parameter uses its value, so the later overwrite is silent.
        let code = r#"Процедура Настроить(Знач Котел, Знач Насос)
	Котел.Мощность = 10;
	Насос.Обороты = Насос.Минимум;
	Насос = Неопределено;
КонецПроцедуры
"#;
        check(code, expect![[r#""#]]);
    }

    #[test]
    fn test_overwrite_then_read_is_flagged() {
        let code = r#"Процедура Установить(Знач Уставка)
	Уставка = 21;
	Термостат.Применить(Уставка);
КонецПроцедуры
"#;
        check(
            code,
            expect![[r#"
            RewriteMethodParameter @ 2:2..2:9
              message: Переприсваивание параметра метода 'Уставка'
              severity: Warning"#]],
        );
    }

    #[test]
    fn test_parameter_never_written() {
        let code = r#"Функция Нагреть(Знач Уставка)
	Текущая = Термостат.Температура;
	Возврат Уставка - Текущая;
КонецФункции
"#;
        check(code, expect![[r#""#]]);
    }

    #[test]
    fn test_default_guard_idiom_no_diagnostic() {
        let code = r#"Процедура Запланировать(Знач Момент = Неопределено)
	Если Момент = Неопределено Тогда
		Момент = ТекущаяДатаСеанса();
	КонецЕсли;
КонецПроцедуры
"#;
        check(code, expect![[r#""#]]);
    }

    #[test]
    fn test_read_inside_call_in_condition_no_diagnostic() {
        let code = r#"Функция Расписание(Знач График)
	Если Не ЗначениеЗаполнено(График) Тогда
		График = Новый Массив;
	КонецЕсли;
	Возврат График;
КонецФункции
"#;
        check(code, expect![[r#""#]]);
    }

    #[test]
    fn test_read_with_cyrillic_case_mismatch_no_diagnostic() {
        // Declaration and uses differ by Cyrillic case; BSL names are case-insensitive.
        let code = r#"Функция Зона(Знач КодЗоны = Неопределено)
	Если КОДЗОНЫ = Неопределено ИЛИ ПустаяСтрока(кодзоны) Тогда
		КОДЗОНЫ = "Холл";
	КонецЕсли;
	Возврат КодЗоны;
КонецФункции
"#;
        check(code, expect![[r#""#]]);
    }

    #[test]
    fn test_read_in_elsif_condition_no_diagnostic() {
        let code = r#"Процедура Переключить(Знач Режим)
	Если Термостат.Выключен Тогда
		Возврат;
	ИначеЕсли Режим = "Эко" Тогда
		Режим = "Комфорт";
	КонецЕсли;
КонецПроцедуры
"#;
        check(code, expect![[r#""#]]);
    }

    #[test]
    fn test_read_in_sibling_branch_does_not_suppress() {
        // The read in Тогда is mutually exclusive with the overwrite in ИначеЕсли.
        let code = r#"Процедура Переключить(Знач Режим)
	Если Термостат.Ручной Тогда
		Термостат.Режим = Режим;
	ИначеЕсли Термостат.Ночь Тогда
		Режим = "Ночь";
	КонецЕсли;
КонецПроцедуры
"#;
        check(
            code,
            expect![[r#"
            RewriteMethodParameter @ 5:3..5:8
              message: Переприсваивание параметра метода 'Режим'
              severity: Warning"#]],
        );
    }

    #[test]
    fn test_read_in_preceding_sibling_if_suppresses() {
        // A read anywhere in a whole Если that precedes the overwrite counts as a use.
        let code = r#"Процедура Переключить(Знач Режим)
	Если Термостат.Ручной Тогда
		Термостат.Режим = Режим;
	КонецЕсли;
	Режим = "Авто";
КонецПроцедуры
"#;
        check(code, expect![[r#""#]]);
    }

    #[test]
    fn test_rewrite_after_unrelated_guard_is_flagged() {
        let code = r#"Процедура Переключить(Знач Режим)
	Если Термостат.Ночь Тогда
		Режим = "Ночь";
	КонецЕсли;
КонецПроцедуры
"#;
        check(
            code,
            expect![[r#"
            RewriteMethodParameter @ 3:3..3:8
              message: Переприсваивание параметра метода 'Режим'
              severity: Warning"#]],
        );
    }

    #[test]
    fn test_several_methods_in_one_module() {
        // Each method is analysed on its own: overwrites in the first and the last are
        // reported, the read-first method between them is not.
        let code = r#"Процедура Первая(Знач Уставка)
	Уставка = 21;
КонецПроцедуры

Функция Вторая(Знач Уставка)
	Возврат Уставка + 1;
КонецФункции

Процедура Третья(Знач Режим, Знач Уставка)
	Режим = "Авто";
	Уставка = Уставка;
КонецПроцедуры
"#;
        check(
            code,
            expect![[r#"
            RewriteMethodParameter @ 2:2..2:9
              message: Переприсваивание параметра метода 'Уставка'
              severity: Warning
            RewriteMethodParameter @ 10:2..10:7
              message: Переприсваивание параметра метода 'Режим'
              severity: Warning"#]],
        );
    }
}
