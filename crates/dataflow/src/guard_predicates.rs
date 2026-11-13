use cfg::{CfgEdgeType, CfgVertex, ControlFlowGraph};
use cfg_types::{ExprId, StmtId};
use hir_def::{
    body::Body,
    hir::{BinaryOp, Expr},
    IdConversion, Name,
};
use petgraph::graph::NodeIndex;
use rustc_hash::FxHashSet;
use stdx::case::CaseExt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GuardSemantics {
    RoleCheck,
    UserCheck,
    PrivilegedQuery,
}

#[derive(Debug, Clone)]
pub struct GuardPredicate {
    pub ru: &'static str,
    pub en: &'static str,
    pub semantics: GuardSemantics,
}

#[derive(Debug, Clone, Default)]
pub struct GuardRegistry {
    lower_aliases: Vec<String>,
}

impl GuardRegistry {
    pub fn new(entries: Vec<GuardPredicate>) -> Self {
        let mut lower_aliases = Vec::with_capacity(entries.len() * 2);
        for p in &entries {
            if !is_supported(p.semantics) {
                tracing::warn!(
                    semantics = ?p.semantics,
                    name = %p.ru,
                    "GuardRegistry::new dropped a guard predicate whose semantics is not yet \
                     supported by the call-shape recogniser; see GuardSemantics doc."
                );
                continue;
            }
            let ru_lower = p.ru.fold_lower();
            let en_lower = if p.en.is_empty() { String::new() } else { p.en.fold_lower() };
            if is_blocklisted_name(&ru_lower) || is_blocklisted_name(&en_lower) {
                tracing::warn!(
                    name = %p.ru,
                    en = %p.en,
                    "GuardRegistry::new dropped a guard predicate whose name is blocklisted \
                     (tautological / multi-state under bare-call recognition); \
                     see default_registry doc."
                );
                continue;
            }
            lower_aliases.push(ru_lower);
            if !en_lower.is_empty() {
                lower_aliases.push(en_lower);
            }
        }
        Self { lower_aliases }
    }

    pub fn alias_count(&self) -> usize {
        self.lower_aliases.len()
    }

    pub fn matches(&self, name: &Name) -> bool {
        let n = name.as_str().fold_lower();
        self.lower_aliases.iter().any(|alias| alias == &n)
    }
}

const BLOCKLISTED_GUARD_NAMES: &[&str] = &[
    "привилегированныйрежим",
    "privilegedmode",
    "безопасныйрежим",
    "safemode",
    "текущийпользователь",
    "currentuser",
];

fn is_blocklisted_name(lower_name: &str) -> bool {
    !lower_name.is_empty() && BLOCKLISTED_GUARD_NAMES.contains(&lower_name)
}

fn is_supported(semantics: GuardSemantics) -> bool {
    match semantics {
        GuardSemantics::RoleCheck => true,
        GuardSemantics::PrivilegedQuery => false,
        GuardSemantics::UserCheck => false,
    }
}

pub fn default_registry() -> GuardRegistry {
    GuardRegistry::new(vec![
        GuardPredicate {
            ru: "РольДоступна", en: "IsInRole", semantics: GuardSemantics::RoleCheck
        },
        GuardPredicate {
            ru: "РольДоступнаПользователю",
            en: "IsInRoleByUser",
            semantics: GuardSemantics::RoleCheck,
        },
    ])
}

pub fn is_stmt_guarded(
    cfg: &ControlFlowGraph,
    body: &Body,
    stmt: StmtId,
    registry: &GuardRegistry,
) -> bool {
    let Some(start) = find_block_containing(cfg, stmt) else {
        return false;
    };
    if cfg.start().is_none() {
        return false;
    }
    let mut visited = FxHashSet::default();
    is_guarded_dfs(cfg, body, registry, start, &mut visited)
}

fn is_guarded_dfs(
    cfg: &ControlFlowGraph,
    body: &Body,
    registry: &GuardRegistry,
    current: NodeIndex,
    visited: &mut FxHashSet<NodeIndex>,
) -> bool {
    if cfg.start() == Some(current) {
        return false;
    }
    if !visited.insert(current) {
        return true;
    }

    let mut any_predecessor = false;
    for (pred, edge_type) in cfg.incoming_edges(current) {
        if !edge_type.is_executable() {
            continue;
        }
        any_predecessor = true;

        if matches!(edge_type, CfgEdgeType::TrueBranch) {
            if let Some(CfgVertex::Conditional(cv)) = cfg.vertex(pred) {
                if condition_matches_guard(body, cv.condition, registry) {
                    continue;
                }
            }
        }

        if !is_guarded_dfs(cfg, body, registry, pred, visited) {
            return false;
        }
    }

    any_predecessor
}

fn condition_matches_guard(body: &Body, condition: ExprId, registry: &GuardRegistry) -> bool {
    match body.expr(condition) {
        Expr::Call { callee, .. } => {
            let callee_id = hir_def::ExprId::from_idx(*callee);
            matches!(body.expr(callee_id), Expr::Path(name) if registry.matches(name))
        }
        Expr::BinaryOp { op: BinaryOp::And, lhs, rhs } => {
            let lhs_id = hir_def::ExprId::from_idx(*lhs);
            let rhs_id = hir_def::ExprId::from_idx(*rhs);
            condition_matches_guard(body, lhs_id, registry)
                || condition_matches_guard(body, rhs_id, registry)
        }
        _ => false,
    }
}

fn find_block_containing(cfg: &ControlFlowGraph, stmt: StmtId) -> Option<NodeIndex> {
    cfg.vertices().find_map(|(idx, vertex)| {
        if let CfgVertex::BasicBlock(block) = vertex {
            if block.statements().contains(&stmt) {
                return Some(idx);
            }
        }
        None
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use cfg::{BasicBlockVertex, ConditionalVertex};
    use hir_def::{
        body::Body,
        hir::{Expr, Literal},
    };
    use la_arena::RawIdx;

    fn name(s: &str) -> Name {
        Name::new(s)
    }

    fn body_with_guard_call(callee_name: &str) -> (Body, ExprId) {
        let mut body = Body::default();
        let callee_expr = body.alloc_expr(Expr::Path(name(callee_name)));
        let call = body.alloc_expr(Expr::Call { callee: callee_expr.to_idx(), args: Box::new([]) });
        (body, call)
    }

    fn body_with_literal_true() -> (Body, ExprId) {
        let mut body = Body::default();
        let lit = body.alloc_expr(Expr::Literal(Literal::Bool(true)));
        (body, lit)
    }

    fn make_block_with_stmt(stmt: StmtId) -> BasicBlockVertex {
        let mut b = BasicBlockVertex::new();
        b.add_statement(stmt);
        b
    }

    fn synthetic_stmt(raw: u32) -> StmtId {
        StmtId::from_raw(RawIdx::from(raw))
    }

    #[test]
    fn missing_block_returns_false() {
        let cfg = ControlFlowGraph::new();
        let (body, _) = body_with_literal_true();
        let registry = default_registry();
        assert!(!is_stmt_guarded(&cfg, &body, synthetic_stmt(0), &registry));
    }

    #[test]
    fn missing_start_returns_false() {
        let mut cfg = ControlFlowGraph::new();
        let stmt = synthetic_stmt(1);
        let _idx = cfg.add_vertex(CfgVertex::BasicBlock(make_block_with_stmt(stmt)));
        let (body, _) = body_with_literal_true();
        let registry = default_registry();
        assert!(!is_stmt_guarded(&cfg, &body, stmt, &registry));
    }

    #[test]
    fn unguarded_linear_path_returns_false() {
        let mut cfg = ControlFlowGraph::new();
        let entry = cfg.add_vertex(CfgVertex::BasicBlock(BasicBlockVertex::new()));
        cfg.set_start(entry);
        let stmt = synthetic_stmt(1);
        let call_block = cfg.add_vertex(CfgVertex::BasicBlock(make_block_with_stmt(stmt)));
        cfg.add_edge(entry, call_block, CfgEdgeType::Unconditional);
        cfg.add_edge(call_block, cfg.exit(), CfgEdgeType::Unconditional);

        let (body, _) = body_with_literal_true();
        let registry = default_registry();
        assert!(!is_stmt_guarded(&cfg, &body, stmt, &registry));
    }

    #[test]
    fn guarded_then_branch_returns_true() {
        let (body, guard_expr) = body_with_guard_call("РольДоступна");
        let stmt = synthetic_stmt(1);

        let mut cfg = ControlFlowGraph::new();
        let entry = cfg.add_vertex(CfgVertex::BasicBlock(BasicBlockVertex::new()));
        cfg.set_start(entry);
        let cond = cfg.add_vertex(CfgVertex::Conditional(ConditionalVertex::new(guard_expr)));
        let call_block = cfg.add_vertex(CfgVertex::BasicBlock(make_block_with_stmt(stmt)));
        let other_block = cfg.add_vertex(CfgVertex::BasicBlock(BasicBlockVertex::new()));
        cfg.add_edge(entry, cond, CfgEdgeType::Unconditional);
        cfg.add_edge(cond, call_block, CfgEdgeType::TrueBranch);
        cfg.add_edge(cond, other_block, CfgEdgeType::FalseBranch);
        cfg.add_edge(call_block, cfg.exit(), CfgEdgeType::Unconditional);
        cfg.add_edge(other_block, cfg.exit(), CfgEdgeType::Unconditional);

        let registry = default_registry();
        assert!(is_stmt_guarded(&cfg, &body, stmt, &registry));
    }

    #[test]
    fn guarded_else_branch_returns_false() {
        let (body, guard_expr) = body_with_guard_call("РольДоступна");
        let stmt = synthetic_stmt(1);

        let mut cfg = ControlFlowGraph::new();
        let entry = cfg.add_vertex(CfgVertex::BasicBlock(BasicBlockVertex::new()));
        cfg.set_start(entry);
        let cond = cfg.add_vertex(CfgVertex::Conditional(ConditionalVertex::new(guard_expr)));
        let then_block = cfg.add_vertex(CfgVertex::BasicBlock(BasicBlockVertex::new()));
        let call_block = cfg.add_vertex(CfgVertex::BasicBlock(make_block_with_stmt(stmt)));
        cfg.add_edge(entry, cond, CfgEdgeType::Unconditional);
        cfg.add_edge(cond, then_block, CfgEdgeType::TrueBranch);
        cfg.add_edge(cond, call_block, CfgEdgeType::FalseBranch);
        cfg.add_edge(then_block, cfg.exit(), CfgEdgeType::Unconditional);
        cfg.add_edge(call_block, cfg.exit(), CfgEdgeType::Unconditional);

        let registry = default_registry();
        assert!(!is_stmt_guarded(&cfg, &body, stmt, &registry));
    }

    #[test]
    fn english_alias_is_recognised() {
        let (body, guard_expr) = body_with_guard_call("IsInRole");
        let stmt = synthetic_stmt(1);

        let mut cfg = ControlFlowGraph::new();
        let entry = cfg.add_vertex(CfgVertex::BasicBlock(BasicBlockVertex::new()));
        cfg.set_start(entry);
        let cond = cfg.add_vertex(CfgVertex::Conditional(ConditionalVertex::new(guard_expr)));
        let call_block = cfg.add_vertex(CfgVertex::BasicBlock(make_block_with_stmt(stmt)));
        let other_block = cfg.add_vertex(CfgVertex::BasicBlock(BasicBlockVertex::new()));
        cfg.add_edge(entry, cond, CfgEdgeType::Unconditional);
        cfg.add_edge(cond, call_block, CfgEdgeType::TrueBranch);
        cfg.add_edge(cond, other_block, CfgEdgeType::FalseBranch);
        cfg.add_edge(call_block, cfg.exit(), CfgEdgeType::Unconditional);
        cfg.add_edge(other_block, cfg.exit(), CfgEdgeType::Unconditional);

        let registry = default_registry();
        assert!(is_stmt_guarded(&cfg, &body, stmt, &registry));
    }

    #[test]
    fn unrelated_predicate_does_not_match() {
        let (body, cond_expr) = body_with_guard_call("СовершенноДругаяФункция");
        let stmt = synthetic_stmt(1);

        let mut cfg = ControlFlowGraph::new();
        let entry = cfg.add_vertex(CfgVertex::BasicBlock(BasicBlockVertex::new()));
        cfg.set_start(entry);
        let cond = cfg.add_vertex(CfgVertex::Conditional(ConditionalVertex::new(cond_expr)));
        let call_block = cfg.add_vertex(CfgVertex::BasicBlock(make_block_with_stmt(stmt)));
        let other_block = cfg.add_vertex(CfgVertex::BasicBlock(BasicBlockVertex::new()));
        cfg.add_edge(entry, cond, CfgEdgeType::Unconditional);
        cfg.add_edge(cond, call_block, CfgEdgeType::TrueBranch);
        cfg.add_edge(cond, other_block, CfgEdgeType::FalseBranch);
        cfg.add_edge(call_block, cfg.exit(), CfgEdgeType::Unconditional);
        cfg.add_edge(other_block, cfg.exit(), CfgEdgeType::Unconditional);

        let registry = default_registry();
        assert!(!is_stmt_guarded(&cfg, &body, stmt, &registry));
    }

    #[test]
    fn and_chain_recognises_either_operand() {
        let mut body = Body::default();
        let role_callee = body.alloc_expr(Expr::Path(name("РольДоступна")));
        let role_call =
            body.alloc_expr(Expr::Call { callee: role_callee.to_idx(), args: Box::new([]) });
        let other_callee = body.alloc_expr(Expr::Path(name("ДругаяПроверка")));
        let other_call =
            body.alloc_expr(Expr::Call { callee: other_callee.to_idx(), args: Box::new([]) });
        let and_expr = body.alloc_expr(Expr::BinaryOp {
            op: BinaryOp::And,
            lhs: role_call.to_idx(),
            rhs: other_call.to_idx(),
        });

        let stmt = synthetic_stmt(1);
        let mut cfg = ControlFlowGraph::new();
        let entry = cfg.add_vertex(CfgVertex::BasicBlock(BasicBlockVertex::new()));
        cfg.set_start(entry);
        let cond = cfg.add_vertex(CfgVertex::Conditional(ConditionalVertex::new(and_expr)));
        let call_block = cfg.add_vertex(CfgVertex::BasicBlock(make_block_with_stmt(stmt)));
        let other_block = cfg.add_vertex(CfgVertex::BasicBlock(BasicBlockVertex::new()));
        cfg.add_edge(entry, cond, CfgEdgeType::Unconditional);
        cfg.add_edge(cond, call_block, CfgEdgeType::TrueBranch);
        cfg.add_edge(cond, other_block, CfgEdgeType::FalseBranch);
        cfg.add_edge(call_block, cfg.exit(), CfgEdgeType::Unconditional);
        cfg.add_edge(other_block, cfg.exit(), CfgEdgeType::Unconditional);

        let registry = default_registry();
        assert!(is_stmt_guarded(&cfg, &body, stmt, &registry));
    }

    #[test]
    fn merged_paths_one_guarded_one_not_returns_false() {
        let (body, guard_expr) = body_with_guard_call("РольДоступна");
        let stmt = synthetic_stmt(1);

        let mut cfg = ControlFlowGraph::new();
        let entry = cfg.add_vertex(CfgVertex::BasicBlock(BasicBlockVertex::new()));
        cfg.set_start(entry);
        let cond = cfg.add_vertex(CfgVertex::Conditional(ConditionalVertex::new(guard_expr)));
        let call_block = cfg.add_vertex(CfgVertex::BasicBlock(make_block_with_stmt(stmt)));
        cfg.add_edge(entry, cond, CfgEdgeType::Unconditional);
        cfg.add_edge(cond, call_block, CfgEdgeType::TrueBranch);
        cfg.add_edge(cond, call_block, CfgEdgeType::FalseBranch);
        cfg.add_edge(call_block, cfg.exit(), CfgEdgeType::Unconditional);

        let registry = default_registry();
        assert!(!is_stmt_guarded(&cfg, &body, stmt, &registry));
    }

    #[test]
    fn dead_code_edges_are_ignored() {
        let (body, guard_expr) = body_with_guard_call("РольДоступна");
        let stmt = synthetic_stmt(1);

        let mut cfg = ControlFlowGraph::new();
        let entry = cfg.add_vertex(CfgVertex::BasicBlock(BasicBlockVertex::new()));
        cfg.set_start(entry);
        let cond = cfg.add_vertex(CfgVertex::Conditional(ConditionalVertex::new(guard_expr)));
        let call_block = cfg.add_vertex(CfgVertex::BasicBlock(make_block_with_stmt(stmt)));
        let dead_block = cfg.add_vertex(CfgVertex::BasicBlock(BasicBlockVertex::new()));
        cfg.add_edge(entry, cond, CfgEdgeType::Unconditional);
        cfg.add_edge(cond, call_block, CfgEdgeType::TrueBranch);
        cfg.add_edge(cond, cfg.exit(), CfgEdgeType::FalseBranch);
        cfg.add_edge(call_block, cfg.exit(), CfgEdgeType::Unconditional);
        cfg.add_edge(dead_block, call_block, CfgEdgeType::Unexecutable);

        let registry = default_registry();
        assert!(is_stmt_guarded(&cfg, &body, stmt, &registry));
    }

    #[test]
    fn loop_back_edge_does_not_create_false_negative() {
        let (body, guard_expr) = body_with_guard_call("РольДоступна");
        let stmt = synthetic_stmt(1);

        let mut cfg = ControlFlowGraph::new();
        let entry = cfg.add_vertex(CfgVertex::BasicBlock(BasicBlockVertex::new()));
        cfg.set_start(entry);
        let cond = cfg.add_vertex(CfgVertex::Conditional(ConditionalVertex::new(guard_expr)));
        let loop_header = cfg.add_vertex(CfgVertex::BasicBlock(BasicBlockVertex::new()));
        let call_block = cfg.add_vertex(CfgVertex::BasicBlock(make_block_with_stmt(stmt)));
        cfg.add_edge(entry, cond, CfgEdgeType::Unconditional);
        cfg.add_edge(cond, loop_header, CfgEdgeType::TrueBranch);
        cfg.add_edge(cond, cfg.exit(), CfgEdgeType::FalseBranch);
        cfg.add_edge(loop_header, call_block, CfgEdgeType::Unconditional);
        cfg.add_edge(call_block, loop_header, CfgEdgeType::Unconditional);
        cfg.add_edge(loop_header, cfg.exit(), CfgEdgeType::Unconditional);

        let registry = default_registry();
        assert!(is_stmt_guarded(&cfg, &body, stmt, &registry));
    }

    #[test]
    fn registry_match_is_case_insensitive() {
        let registry = default_registry();
        assert!(registry.matches(&name("рольдоступна")));
        assert!(registry.matches(&name("РОЛЬДОСТУПНА")));
        assert!(registry.matches(&name("isinrole")));
        assert!(!registry.matches(&name("NotARealGuard")));
    }

    #[test]
    fn safe_mode_is_not_a_guard() {
        let registry = default_registry();
        assert!(!registry.matches(&name("БезопасныйРежим")));
        assert!(!registry.matches(&name("SafeMode")));
    }

    #[test]
    fn privileged_mode_query_is_not_a_guard() {
        let registry = default_registry();
        assert!(!registry.matches(&name("ПривилегированныйРежим")));
        assert!(!registry.matches(&name("PrivilegedMode")));
    }

    #[test]
    fn current_user_is_not_a_guard_today() {
        let registry = default_registry();
        assert!(!registry.matches(&name("ТекущийПользователь")));
        assert!(!registry.matches(&name("CurrentUser")));
    }

    #[test]
    fn name_blocklist_rejects_miscategorised_privileged_mode() {
        let registry = GuardRegistry::new(vec![GuardPredicate {
            ru: "ПривилегированныйРежим",
            en: "PrivilegedMode",
            semantics: GuardSemantics::RoleCheck,
        }]);
        assert_eq!(
            registry.alias_count(),
            0,
            "miscategorised PrivilegedMode entry must be dropped by name blocklist"
        );
    }

    #[test]
    fn name_blocklist_rejects_miscategorised_safe_mode_and_current_user() {
        let safe_mode = GuardRegistry::new(vec![GuardPredicate {
            ru: "БезопасныйРежим",
            en: "SafeMode",
            semantics: GuardSemantics::RoleCheck,
        }]);
        assert_eq!(safe_mode.alias_count(), 0);
        let current_user = GuardRegistry::new(vec![GuardPredicate {
            ru: "ТекущийПользователь",
            en: "CurrentUser",
            semantics: GuardSemantics::RoleCheck,
        }]);
        assert_eq!(current_user.alias_count(), 0);
    }

    #[test]
    fn custom_registry_drops_privileged_query_entries() {
        let registry = GuardRegistry::new(vec![GuardPredicate {
            ru: "ПривилегированныйРежим",
            en: "PrivilegedMode",
            semantics: GuardSemantics::PrivilegedQuery,
        }]);
        assert_eq!(
            registry.alias_count(),
            0,
            "PrivilegedQuery entries must be dropped at construction"
        );
        assert!(!registry.matches(&name("ПривилегированныйРежим")));
        assert!(!registry.matches(&name("PrivilegedMode")));
    }

    #[test]
    fn custom_registry_drops_user_check_entries() {
        let registry = GuardRegistry::new(vec![GuardPredicate {
            ru: "ТекущийПользователь",
            en: "CurrentUser",
            semantics: GuardSemantics::UserCheck,
        }]);
        assert_eq!(registry.alias_count(), 0, "UserCheck entries must be dropped at construction");
        assert!(!registry.matches(&name("ТекущийПользователь")));
        assert!(!registry.matches(&name("CurrentUser")));
    }

    #[test]
    fn custom_registry_keeps_supported_semantics_alongside_dropped_ones() {
        let registry = GuardRegistry::new(vec![
            GuardPredicate {
                ru: "МояПроверкаРоли",
                en: "MyRoleCheck",
                semantics: GuardSemantics::RoleCheck,
            },
            GuardPredicate {
                ru: "ТекущийПользователь",
                en: "CurrentUser",
                semantics: GuardSemantics::UserCheck,
            },
        ]);
        assert_eq!(registry.alias_count(), 2, "only the RoleCheck entry's two aliases survive");
        assert!(registry.matches(&name("МояПроверкаРоли")));
        assert!(registry.matches(&name("MyRoleCheck")));
        assert!(!registry.matches(&name("ТекущийПользователь")));
    }

    #[test]
    fn role_by_user_is_recognised() {
        let (body, guard_expr) = body_with_guard_call("РольДоступнаПользователю");
        let stmt = synthetic_stmt(1);

        let mut cfg = ControlFlowGraph::new();
        let entry = cfg.add_vertex(CfgVertex::BasicBlock(BasicBlockVertex::new()));
        cfg.set_start(entry);
        let cond = cfg.add_vertex(CfgVertex::Conditional(ConditionalVertex::new(guard_expr)));
        let call_block = cfg.add_vertex(CfgVertex::BasicBlock(make_block_with_stmt(stmt)));
        let other_block = cfg.add_vertex(CfgVertex::BasicBlock(BasicBlockVertex::new()));
        cfg.add_edge(entry, cond, CfgEdgeType::Unconditional);
        cfg.add_edge(cond, call_block, CfgEdgeType::TrueBranch);
        cfg.add_edge(cond, other_block, CfgEdgeType::FalseBranch);
        cfg.add_edge(call_block, cfg.exit(), CfgEdgeType::Unconditional);
        cfg.add_edge(other_block, cfg.exit(), CfgEdgeType::Unconditional);

        let registry = default_registry();
        assert!(is_stmt_guarded(&cfg, &body, stmt, &registry));
    }

    #[test]
    fn or_chain_with_guard_is_not_recognised() {
        let mut body = Body::default();
        let role_callee = body.alloc_expr(Expr::Path(name("РольДоступна")));
        let role_call =
            body.alloc_expr(Expr::Call { callee: role_callee.to_idx(), args: Box::new([]) });
        let other_callee = body.alloc_expr(Expr::Path(name("ДругаяПроверка")));
        let other_call =
            body.alloc_expr(Expr::Call { callee: other_callee.to_idx(), args: Box::new([]) });
        let or_expr = body.alloc_expr(Expr::BinaryOp {
            op: BinaryOp::Or,
            lhs: role_call.to_idx(),
            rhs: other_call.to_idx(),
        });

        let stmt = synthetic_stmt(1);
        let mut cfg = ControlFlowGraph::new();
        let entry = cfg.add_vertex(CfgVertex::BasicBlock(BasicBlockVertex::new()));
        cfg.set_start(entry);
        let cond = cfg.add_vertex(CfgVertex::Conditional(ConditionalVertex::new(or_expr)));
        let call_block = cfg.add_vertex(CfgVertex::BasicBlock(make_block_with_stmt(stmt)));
        let other_block = cfg.add_vertex(CfgVertex::BasicBlock(BasicBlockVertex::new()));
        cfg.add_edge(entry, cond, CfgEdgeType::Unconditional);
        cfg.add_edge(cond, call_block, CfgEdgeType::TrueBranch);
        cfg.add_edge(cond, other_block, CfgEdgeType::FalseBranch);
        cfg.add_edge(call_block, cfg.exit(), CfgEdgeType::Unconditional);
        cfg.add_edge(other_block, cfg.exit(), CfgEdgeType::Unconditional);

        let registry = default_registry();
        assert!(!is_stmt_guarded(&cfg, &body, stmt, &registry));
    }

    #[test]
    fn negated_guard_is_not_recognised() {
        let mut body = Body::default();
        let role_callee = body.alloc_expr(Expr::Path(name("РольДоступна")));
        let role_call =
            body.alloc_expr(Expr::Call { callee: role_callee.to_idx(), args: Box::new([]) });
        let not_expr = body
            .alloc_expr(Expr::UnaryOp { op: hir_def::hir::UnaryOp::Not, expr: role_call.to_idx() });

        let stmt = synthetic_stmt(1);
        let mut cfg = ControlFlowGraph::new();
        let entry = cfg.add_vertex(CfgVertex::BasicBlock(BasicBlockVertex::new()));
        cfg.set_start(entry);
        let cond = cfg.add_vertex(CfgVertex::Conditional(ConditionalVertex::new(not_expr)));
        let call_block = cfg.add_vertex(CfgVertex::BasicBlock(make_block_with_stmt(stmt)));
        let other_block = cfg.add_vertex(CfgVertex::BasicBlock(BasicBlockVertex::new()));
        cfg.add_edge(entry, cond, CfgEdgeType::Unconditional);
        cfg.add_edge(cond, call_block, CfgEdgeType::TrueBranch);
        cfg.add_edge(cond, other_block, CfgEdgeType::FalseBranch);
        cfg.add_edge(call_block, cfg.exit(), CfgEdgeType::Unconditional);
        cfg.add_edge(other_block, cfg.exit(), CfgEdgeType::Unconditional);

        let registry = default_registry();
        assert!(!is_stmt_guarded(&cfg, &body, stmt, &registry));
    }

    #[test]
    fn post_loop_call_is_unguarded_when_loop_skipped() {
        let stmt = synthetic_stmt(1);
        let cond_expr = {
            let mut b = Body::default();
            let lit = b.alloc_expr(Expr::Literal(Literal::Bool(true)));
            let _ = (b, lit);
            let mut body2 = Body::default();
            let p = body2.alloc_expr(Expr::Path(name("Что-тоНеГуард")));
            let c = body2.alloc_expr(Expr::Call { callee: p.to_idx(), args: Box::new([]) });
            (body2, c)
        };
        let (body, header_cond) = cond_expr;

        let mut cfg = ControlFlowGraph::new();
        let entry = cfg.add_vertex(CfgVertex::BasicBlock(BasicBlockVertex::new()));
        cfg.set_start(entry);
        let loop_header =
            cfg.add_vertex(CfgVertex::Conditional(ConditionalVertex::new(header_cond)));
        let loop_body = cfg.add_vertex(CfgVertex::BasicBlock(BasicBlockVertex::new()));
        let call_block = cfg.add_vertex(CfgVertex::BasicBlock(make_block_with_stmt(stmt)));
        cfg.add_edge(entry, loop_header, CfgEdgeType::Unconditional);
        cfg.add_edge(loop_header, loop_body, CfgEdgeType::TrueBranch);
        cfg.add_edge(loop_header, call_block, CfgEdgeType::FalseBranch);
        cfg.add_edge(loop_body, loop_header, CfgEdgeType::Unconditional);
        cfg.add_edge(call_block, cfg.exit(), CfgEdgeType::Unconditional);

        let registry = default_registry();
        assert!(!is_stmt_guarded(&cfg, &body, stmt, &registry));
    }

    #[test]
    fn except_handler_path_is_not_guarded_by_try_block_role_check() {
        let (body, guard_expr) = body_with_guard_call("РольДоступна");
        let stmt = synthetic_stmt(1);

        let mut cfg = ControlFlowGraph::new();
        let entry = cfg.add_vertex(CfgVertex::BasicBlock(BasicBlockVertex::new()));
        cfg.set_start(entry);
        let try_vertex = cfg.add_vertex(CfgVertex::Try);
        let try_body_guard =
            cfg.add_vertex(CfgVertex::Conditional(ConditionalVertex::new(guard_expr)));
        let try_else = cfg.add_vertex(CfgVertex::BasicBlock(BasicBlockVertex::new()));
        let except_handler = cfg.add_vertex(CfgVertex::BasicBlock(BasicBlockVertex::new()));
        let call_block = cfg.add_vertex(CfgVertex::BasicBlock(make_block_with_stmt(stmt)));

        cfg.add_edge(entry, try_vertex, CfgEdgeType::Unconditional);
        cfg.add_edge(try_vertex, try_body_guard, CfgEdgeType::Unconditional);
        cfg.add_edge(try_body_guard, call_block, CfgEdgeType::TrueBranch);
        cfg.add_edge(try_body_guard, try_else, CfgEdgeType::FalseBranch);
        cfg.add_edge(try_else, cfg.exit(), CfgEdgeType::Unconditional);
        cfg.add_edge(try_vertex, except_handler, CfgEdgeType::Exception);
        cfg.add_edge(except_handler, call_block, CfgEdgeType::Unconditional);
        cfg.add_edge(call_block, cfg.exit(), CfgEdgeType::Unconditional);

        let registry = default_registry();
        assert!(!is_stmt_guarded(&cfg, &body, stmt, &registry));
    }

    #[test]
    fn guard_call_with_arguments_is_recognised() {
        let mut body = Body::default();
        let callee = body.alloc_expr(Expr::Path(name("РольДоступна")));
        let arg = body.alloc_expr(Expr::Literal(Literal::String("Администратор".to_string())));
        let call =
            body.alloc_expr(Expr::Call { callee: callee.to_idx(), args: Box::new([arg.to_idx()]) });

        let stmt = synthetic_stmt(1);
        let mut cfg = ControlFlowGraph::new();
        let entry = cfg.add_vertex(CfgVertex::BasicBlock(BasicBlockVertex::new()));
        cfg.set_start(entry);
        let cond = cfg.add_vertex(CfgVertex::Conditional(ConditionalVertex::new(call)));
        let call_block = cfg.add_vertex(CfgVertex::BasicBlock(make_block_with_stmt(stmt)));
        let other_block = cfg.add_vertex(CfgVertex::BasicBlock(BasicBlockVertex::new()));
        cfg.add_edge(entry, cond, CfgEdgeType::Unconditional);
        cfg.add_edge(cond, call_block, CfgEdgeType::TrueBranch);
        cfg.add_edge(cond, other_block, CfgEdgeType::FalseBranch);
        cfg.add_edge(call_block, cfg.exit(), CfgEdgeType::Unconditional);
        cfg.add_edge(other_block, cfg.exit(), CfgEdgeType::Unconditional);

        let registry = default_registry();
        assert!(is_stmt_guarded(&cfg, &body, stmt, &registry));
    }
}
