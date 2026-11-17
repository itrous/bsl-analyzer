use std::sync::Arc;
use stdx::case::CaseExt;

pub use bsl_platform::security::Category;

use bsl_platform::security::{registry, Role};
use cfg::{CfgVertex, ControlFlowGraph};
use hir_def::{
    body::Body,
    hir::{Expr, Literal, Stmt},
    BindingId, ExprId, IdConversion, StmtId,
};
use la_arena::RawIdx;

use crate::value_state::{KnownValue, ValueOverlay, ValueStateProvider};
use crate::{DataflowResult, DataflowSolver, Direction, Lattice, Transfer};

pub const K_MAX: u8 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaturatingCount {
    Exact(u8),
    AtLeast(u8),
}

impl SaturatingCount {
    pub fn inc(self) -> Self {
        match self {
            Self::Exact(n) if n < K_MAX => Self::Exact(n + 1),
            Self::Exact(_) => Self::AtLeast(K_MAX),
            Self::AtLeast(k) if k < K_MAX => Self::AtLeast(k + 1),
            Self::AtLeast(_) => Self::AtLeast(K_MAX),
        }
    }

    pub fn dec(self) -> Self {
        match self {
            Self::Exact(0) => Self::Exact(0),
            Self::Exact(n) => Self::Exact(n - 1),
            Self::AtLeast(0) => Self::AtLeast(0),
            Self::AtLeast(k) => Self::AtLeast(k - 1),
        }
    }

    pub fn join_max(a: Self, b: Self) -> Self {
        match (a, b) {
            (Self::Exact(x), Self::Exact(y)) => Self::Exact(x.max(y)),
            (Self::Exact(x), Self::AtLeast(y)) | (Self::AtLeast(y), Self::Exact(x)) => {
                Self::AtLeast(x.max(y))
            }
            (Self::AtLeast(x), Self::AtLeast(y)) => Self::AtLeast(x.max(y)),
        }
    }

    pub fn join_min(a: Self, b: Self) -> Self {
        match (a, b) {
            (Self::Exact(x), Self::Exact(y)) => Self::Exact(x.min(y)),
            (Self::Exact(x), Self::AtLeast(y)) | (Self::AtLeast(y), Self::Exact(x)) => {
                if x <= y {
                    Self::Exact(x)
                } else {
                    Self::AtLeast(y)
                }
            }
            (Self::AtLeast(x), Self::AtLeast(y)) => Self::AtLeast(x.min(y)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrivilegeCounter {
    pub may: SaturatingCount,
    pub must: SaturatingCount,
}

impl PrivilegeCounter {
    pub const ENTRY: Self =
        Self { may: SaturatingCount::Exact(0), must: SaturatingCount::Exact(0) };

    pub fn open(self) -> Self {
        Self { may: self.may.inc(), must: self.must.inc() }
    }

    pub fn close(self) -> Self {
        Self { may: self.may.dec(), must: self.must.dec() }
    }

    pub fn unknown(self) -> Self {
        Self { may: self.may.inc(), must: self.must.dec() }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SecurityCounters {
    pub privilege: PrivilegeCounter,
    pub unsafe_frame: PrivilegeCounter,
}

impl SecurityCounters {
    pub const ENTRY: Self =
        Self { privilege: PrivilegeCounter::ENTRY, unsafe_frame: PrivilegeCounter::ENTRY };
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SecurityModeState(Inner);

#[derive(Debug, Clone, Default, PartialEq, Eq)]
enum Inner {
    #[default]
    Unreachable,
    Reachable {
        counters: SecurityCounters,
        overlay: ValueOverlay,
    },
}

impl SecurityModeState {
    pub fn unreachable() -> Self {
        Self(Inner::Unreachable)
    }

    pub fn entry() -> Self {
        Self(Inner::Reachable { counters: SecurityCounters::ENTRY, overlay: ValueOverlay::empty() })
    }

    /// Approximate live heap bytes for Salsa's `memory_usage` report: only a
    /// `Reachable` state's `ValueOverlay` owns heap; the `SecurityCounters` are
    /// `Copy`.
    pub fn estimated_heap(&self) -> usize {
        match &self.0 {
            Inner::Unreachable => 0,
            Inner::Reachable { overlay, .. } => overlay.estimated_heap(),
        }
    }

    pub fn counters(&self) -> Option<&SecurityCounters> {
        match &self.0 {
            Inner::Unreachable => None,
            Inner::Reachable { counters, .. } => Some(counters),
        }
    }

    pub fn is_reachable(&self) -> bool {
        matches!(self.0, Inner::Reachable { .. })
    }
}

impl Lattice for SecurityModeState {
    fn join(&self, other: &Self) -> Self {
        let inner = match (&self.0, &other.0) {
            (Inner::Unreachable, x) | (x, Inner::Unreachable) => x.clone(),
            (
                Inner::Reachable { counters: a, overlay: oa },
                Inner::Reachable { counters: b, overlay: ob },
            ) => {
                let counters = SecurityCounters {
                    privilege: PrivilegeCounter {
                        may: SaturatingCount::join_max(a.privilege.may, b.privilege.may),
                        must: SaturatingCount::join_min(a.privilege.must, b.privilege.must),
                    },
                    unsafe_frame: PrivilegeCounter {
                        may: SaturatingCount::join_max(a.unsafe_frame.may, b.unsafe_frame.may),
                        must: SaturatingCount::join_min(a.unsafe_frame.must, b.unsafe_frame.must),
                    },
                };
                Inner::Reachable { counters, overlay: oa.join(ob) }
            }
        };
        Self(inner)
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct SecurityStateProvider;

impl Transfer<SecurityModeState> for SecurityStateProvider {
    fn transfer_stmt(
        &self,
        stmt_id: RawIdx,
        state: &SecurityModeState,
        body: &Body,
    ) -> SecurityModeState {
        let mut next = state.clone();
        let Inner::Reachable { counters, overlay } = &mut next.0 else {
            return next;
        };
        let typed_stmt = StmtId::from_raw(stmt_id);

        let mut calls = Vec::new();
        collect_call_exprs_in_stmt(body, typed_stmt, &mut calls);
        for call_expr in calls {
            if let Some(info) = recognize_security_call(body, call_expr, overlay) {
                apply_call_to_counters(counters, info);
            }
        }

        crate::value_state::step(overlay, body, typed_stmt);

        next
    }

    fn transfer_expr_in_place(
        &self,
        expr_id: hir_def::ExprId,
        state: &mut SecurityModeState,
        body: &Body,
    ) {
        let Inner::Reachable { counters, overlay } = &mut state.0 else {
            return;
        };
        let mut calls = Vec::new();
        walk_expr_for_calls(body, expr_id.to_idx(), &mut calls);
        for call_expr in calls {
            if let Some(info) = recognize_security_call(body, call_expr, overlay) {
                apply_call_to_counters(counters, info);
            }
        }
    }

    fn transfer_loop_var_bind(
        &self,
        loop_var: BindingId,
        state: &mut SecurityModeState,
        body: &Body,
    ) {
        if let Inner::Reachable { overlay, .. } = &mut state.0 {
            ValueStateProvider.transfer_loop_var_bind(loop_var, overlay, body);
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum ArgValue {
    KnownTrue,
    KnownFalse,
    Unknown,
}

#[derive(Debug, Clone, Copy)]
struct SecurityCallInfo {
    category: Category,
    opens_unsafe_when: bool,
    arg: ArgValue,
}

fn recognize_security_call(
    body: &Body,
    call_expr: ExprId,
    overlay: &ValueOverlay,
) -> Option<SecurityCallInfo> {
    let Expr::Call { callee, args } = body.expr(call_expr) else {
        return None;
    };
    let Expr::Path(callee_name) = body.expr(ExprId::from_idx(*callee)) else {
        return None;
    };
    let lc_name = callee_name.as_str().fold_lower();
    let entry = registry().lookup_global_lc(&lc_name)?;
    if !matches!(entry.category, Category::PrivilegedMode | Category::SafeMode) {
        return None;
    }
    let opens_unsafe_when = entry.params.iter().find_map(|p| match p.role {
        Role::ModeBool { opens_unsafe_when } if p.index == 0 => Some(opens_unsafe_when),
        _ => None,
    })?;
    let arg = args
        .first()
        .map(|&arg_idx| classify_arg(body, ExprId::from_idx(arg_idx), overlay))
        .unwrap_or(ArgValue::Unknown);
    Some(SecurityCallInfo { category: entry.category, opens_unsafe_when, arg })
}

fn classify_arg(body: &Body, arg_expr: ExprId, overlay: &ValueOverlay) -> ArgValue {
    match body.expr(arg_expr) {
        Expr::Literal(Literal::Bool(true)) => ArgValue::KnownTrue,
        Expr::Literal(Literal::Bool(false)) => ArgValue::KnownFalse,
        Expr::Path(name) => match overlay.get(name.as_str()) {
            Some(KnownValue::Bool(true)) => ArgValue::KnownTrue,
            Some(KnownValue::Bool(false)) => ArgValue::KnownFalse,
            None => ArgValue::Unknown,
        },
        _ => ArgValue::Unknown,
    }
}

pub fn analyze(
    cfg: Arc<ControlFlowGraph>,
    body: Body,
) -> Option<DataflowResult<SecurityModeState>> {
    let mut solver = DataflowSolver::new(cfg, body, SecurityStateProvider);
    solver.set_direction(Direction::Forward);
    solver.set_bottom_factory(SecurityModeState::unreachable);
    solver.set_initial_state(SecurityModeState::entry());
    solver.solve()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenEvent {
    pub category: Category,
    pub callee: ExprId,
    pub stmt: Option<StmtId>,
}

pub fn open_events(result: &DataflowResult<SecurityModeState>, body: &Body) -> Vec<OpenEvent> {
    let mut events = Vec::new();
    let mut calls_buf: Vec<ExprId> = Vec::new();
    for (vertex_idx, vertex) in result.cfg().vertices() {
        let Some(in_state) = result.block_in(vertex_idx) else { continue };
        let Inner::Reachable { overlay: vertex_overlay, .. } = &in_state.0 else { continue };
        match vertex {
            CfgVertex::BasicBlock(bb) => {
                let mut overlay = vertex_overlay.clone();
                for &stmt_id in bb.statements() {
                    calls_buf.clear();
                    collect_call_exprs_in_stmt(body, stmt_id, &mut calls_buf);
                    emit_for_calls(body, &calls_buf, &overlay, Some(stmt_id), &mut events);
                    crate::value_state::step(&mut overlay, body, stmt_id);
                }
            }
            CfgVertex::Conditional(v) => {
                calls_buf.clear();
                walk_expr_for_calls(body, v.condition.to_idx(), &mut calls_buf);
                emit_for_calls(body, &calls_buf, vertex_overlay, None, &mut events);
            }
            CfgVertex::WhileHeader(v) => {
                calls_buf.clear();
                walk_expr_for_calls(body, v.condition.to_idx(), &mut calls_buf);
                emit_for_calls(body, &calls_buf, vertex_overlay, None, &mut events);
            }
            CfgVertex::LoopEntry(v) => {
                calls_buf.clear();
                for root in v.roots() {
                    walk_expr_for_calls(body, root.to_idx(), &mut calls_buf);
                }
                let origin = result.cfg().source_stmt_id(vertex_idx);
                emit_for_calls(body, &calls_buf, vertex_overlay, origin, &mut events);
            }
            _ => {}
        }
    }
    events
}

fn emit_for_calls(
    body: &Body,
    calls: &[ExprId],
    overlay: &ValueOverlay,
    stmt: Option<StmtId>,
    events: &mut Vec<OpenEvent>,
) {
    for &call_expr in calls {
        let Some(info) = recognize_security_call(body, call_expr, overlay) else {
            continue;
        };
        if !call_opens_frame(&info) {
            continue;
        }
        if let Expr::Call { callee, .. } = body.expr(call_expr) {
            events.push(OpenEvent {
                category: info.category,
                callee: ExprId::from_idx(*callee),
                stmt,
            });
        }
    }
}

fn collect_call_exprs_in_stmt(body: &Body, stmt: StmtId, out: &mut Vec<ExprId>) {
    let roots: &[hir_def::hir::ExprIdx] = match body.stmt(stmt) {
        Stmt::Expr(e) => std::slice::from_ref(e),
        Stmt::Assign { target, value } => {
            walk_expr_for_calls(body, *target, out);
            walk_expr_for_calls(body, *value, out);
            return;
        }
        Stmt::Return { value: Some(v) } => std::slice::from_ref(v),
        Stmt::Raise { value: Some(v) } => std::slice::from_ref(v),
        Stmt::Execute { expr } => std::slice::from_ref(expr),
        Stmt::AddHandler { event, handler } => {
            walk_expr_for_calls(body, *event, out);
            walk_expr_for_calls(body, *handler, out);
            return;
        }
        Stmt::RemoveHandler { event, handler } => {
            walk_expr_for_calls(body, *event, out);
            walk_expr_for_calls(body, *handler, out);
            return;
        }
        _ => return,
    };
    for &root in roots {
        walk_expr_for_calls(body, root, out);
    }
}

fn walk_expr_for_calls(body: &Body, expr_idx: hir_def::hir::ExprIdx, out: &mut Vec<ExprId>) {
    let typed = ExprId::from_idx(expr_idx);
    match body.expr(typed) {
        Expr::Call { callee, args } => {
            walk_expr_for_calls(body, *callee, out);
            for &a in args.iter() {
                walk_expr_for_calls(body, a, out);
            }
            out.push(typed);
        }
        Expr::MethodCall { receiver, args, .. } => {
            walk_expr_for_calls(body, *receiver, out);
            for &a in args.iter() {
                walk_expr_for_calls(body, a, out);
            }
            out.push(typed);
        }
        Expr::BinaryOp { lhs, rhs, .. } => {
            walk_expr_for_calls(body, *lhs, out);
            walk_expr_for_calls(body, *rhs, out);
        }
        Expr::UnaryOp { expr, .. } => walk_expr_for_calls(body, *expr, out),
        Expr::Ternary { condition, then_expr, else_expr } => {
            walk_expr_for_calls(body, *condition, out);
            walk_expr_for_calls(body, *then_expr, out);
            walk_expr_for_calls(body, *else_expr, out);
        }
        Expr::Index { base, index } => {
            walk_expr_for_calls(body, *base, out);
            walk_expr_for_calls(body, *index, out);
        }
        Expr::Field { base, .. } => walk_expr_for_calls(body, *base, out),
        Expr::New { args, .. } => {
            for &a in args.iter() {
                walk_expr_for_calls(body, a, out);
            }
            out.push(typed);
        }
        Expr::Array(items) => {
            for &i in items.iter() {
                walk_expr_for_calls(body, i, out);
            }
        }
        Expr::Await { expr } => walk_expr_for_calls(body, *expr, out),
        Expr::Missing | Expr::Literal(_) | Expr::Path(_) => {}
    }
}

fn call_opens_frame(info: &SecurityCallInfo) -> bool {
    match info.arg {
        ArgValue::KnownTrue if info.opens_unsafe_when => true,
        ArgValue::KnownFalse if !info.opens_unsafe_when => true,
        ArgValue::KnownTrue | ArgValue::KnownFalse => false,
        ArgValue::Unknown => true,
    }
}

fn apply_call_to_counters(counters: &mut SecurityCounters, info: SecurityCallInfo) {
    let target = match info.category {
        Category::PrivilegedMode => &mut counters.privilege,
        Category::SafeMode => &mut counters.unsafe_frame,
        _ => return,
    };
    *target = match info.arg {
        ArgValue::KnownTrue if info.opens_unsafe_when => target.open(),
        ArgValue::KnownFalse if !info.opens_unsafe_when => target.open(),
        ArgValue::KnownTrue | ArgValue::KnownFalse => target.close(),
        ArgValue::Unknown => target.unknown(),
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reachable(p: PrivilegeCounter, u: PrivilegeCounter) -> SecurityModeState {
        SecurityModeState(Inner::Reachable {
            counters: SecurityCounters { privilege: p, unsafe_frame: u },
            overlay: ValueOverlay::empty(),
        })
    }

    fn pc(may: u8, must: u8) -> PrivilegeCounter {
        PrivilegeCounter { may: SaturatingCount::Exact(may), must: SaturatingCount::Exact(must) }
    }

    #[test]
    fn saturating_join_max_idempotent() {
        for k in 0..=K_MAX {
            assert_eq!(
                SaturatingCount::join_max(SaturatingCount::Exact(k), SaturatingCount::Exact(k)),
                SaturatingCount::Exact(k),
            );
            assert_eq!(
                SaturatingCount::join_max(SaturatingCount::AtLeast(k), SaturatingCount::AtLeast(k)),
                SaturatingCount::AtLeast(k),
            );
        }
    }

    #[test]
    fn saturating_join_min_idempotent() {
        for k in 0..=K_MAX {
            assert_eq!(
                SaturatingCount::join_min(SaturatingCount::Exact(k), SaturatingCount::Exact(k)),
                SaturatingCount::Exact(k),
            );
            assert_eq!(
                SaturatingCount::join_min(SaturatingCount::AtLeast(k), SaturatingCount::AtLeast(k)),
                SaturatingCount::AtLeast(k),
            );
        }
    }

    #[test]
    fn saturating_inc_saturates_at_k_max() {
        let mut s = SaturatingCount::Exact(0);
        for _ in 0..(K_MAX as usize + 5) {
            s = s.inc();
        }
        assert!(matches!(s, SaturatingCount::AtLeast(K_MAX)));
    }

    #[test]
    fn saturating_dec_saturates_at_zero() {
        let s = SaturatingCount::Exact(0).dec();
        assert_eq!(s, SaturatingCount::Exact(0));
        let s = SaturatingCount::AtLeast(0).dec();
        assert_eq!(s, SaturatingCount::AtLeast(0));
    }

    /// Каждый отдельный шаг `AtLeast` наблюдается здесь, а не через доведение до
    /// насыщения: упёршийся в `AtLeast(K_MAX)` счётчик даёт один и тот же итог при
    /// любом искажении самого шага, поэтому итог о шаге ничего не говорит.
    #[test]
    fn saturating_inc_steps_through_at_least() {
        assert_eq!(SaturatingCount::AtLeast(0).inc(), SaturatingCount::AtLeast(1));
        assert_eq!(SaturatingCount::AtLeast(K_MAX - 1).inc(), SaturatingCount::AtLeast(K_MAX));
        assert_eq!(SaturatingCount::AtLeast(K_MAX).inc(), SaturatingCount::AtLeast(K_MAX));
    }

    /// Точка перехода `Exact` -> `AtLeast` лежит ровно на `K_MAX`: до неё счётчик
    /// остаётся точным, на ней теряет точность.
    #[test]
    fn saturating_inc_leaves_exact_exactly_at_k_max() {
        assert_eq!(SaturatingCount::Exact(K_MAX - 1).inc(), SaturatingCount::Exact(K_MAX));
        assert_eq!(SaturatingCount::Exact(K_MAX).inc(), SaturatingCount::AtLeast(K_MAX));
    }

    /// Шаг вниз с положительного значения — вторая половина `dec`, которую проверка
    /// одного лишь пола на нуле не затрагивает.
    #[test]
    fn saturating_dec_steps_down_above_zero() {
        assert_eq!(SaturatingCount::Exact(1).dec(), SaturatingCount::Exact(0));
        assert_eq!(SaturatingCount::AtLeast(1).dec(), SaturatingCount::AtLeast(0));
        assert_eq!(SaturatingCount::AtLeast(K_MAX).dec(), SaturatingCount::AtLeast(K_MAX - 1));
    }

    #[test]
    fn unreachable_is_join_identity() {
        let bot = SecurityModeState::unreachable();
        let r = reachable(pc(2, 1), pc(1, 0));
        assert_eq!(bot.join(&r), r);
        assert_eq!(r.join(&bot), r);
    }

    #[test]
    fn join_idempotent_on_reachable() {
        let r = reachable(pc(2, 1), pc(0, 0));
        assert_eq!(r.join(&r), r);
    }

    #[test]
    fn join_commutative_on_reachable() {
        let a = reachable(pc(1, 1), pc(0, 0));
        let b = reachable(pc(2, 0), pc(1, 1));
        assert_eq!(a.join(&b), b.join(&a));
    }

    #[test]
    fn counter_lub_is_pointwise_max_min() {
        let a = reachable(pc(1, 1), pc(0, 0));
        let b = reachable(pc(0, 0), pc(0, 0));
        let merged = a.join(&b);
        let merged_p = merged.counters().unwrap().privilege;
        assert_eq!(merged_p.may, SaturatingCount::Exact(1));
        assert_eq!(merged_p.must, SaturatingCount::Exact(0));
    }

    #[test]
    fn three_calls_two_open_one_close_leave_must_one() {
        let p = PrivilegeCounter::ENTRY.open().open().close();
        assert_eq!(p.must, SaturatingCount::Exact(1));
        assert_eq!(p.may, SaturatingCount::Exact(1));
    }

    #[test]
    fn unknown_arg_widens_may_and_shrinks_must() {
        let p = PrivilegeCounter::ENTRY.open().unknown();
        assert_eq!(p.may, SaturatingCount::Exact(2));
        assert_eq!(p.must, SaturatingCount::Exact(0));
    }

    #[test]
    fn unknown_at_entry_creates_at_least_state() {
        let p = PrivilegeCounter::ENTRY.unknown();
        assert_eq!(p.may, SaturatingCount::Exact(1));
        assert_eq!(p.must, SaturatingCount::Exact(0));
    }

    fn reachable_with_overlay(p: PrivilegeCounter, overlay: ValueOverlay) -> SecurityModeState {
        SecurityModeState(Inner::Reachable {
            counters: SecurityCounters { privilege: p, unsafe_frame: PrivilegeCounter::ENTRY },
            overlay,
        })
    }

    #[test]
    fn idempotent_with_reachable_arm_engaged() {
        let r = reachable_with_overlay(pc(2, 1), ValueOverlay::empty());
        assert_eq!(r.join(&r), r);
        let r2 = r.clone();
        assert_eq!(r.join(&r2), r);
    }

    #[test]
    fn unreachable_identity_with_non_empty_overlay() {
        let bot = SecurityModeState::unreachable();
        let r = reachable_with_overlay(pc(1, 1), ValueOverlay::empty());
        assert_eq!(bot.join(&r), r);
        assert_eq!(r.join(&bot), r);
    }

    fn call_info(category: Category, opens_unsafe_when: bool, arg: ArgValue) -> SecurityCallInfo {
        SecurityCallInfo { category, opens_unsafe_when, arg }
    }

    /// `УстановитьПривилегированныйРежим(Истина)` снимает проверку прав, `(Ложь)` её
    /// возвращает: аргумент `Истина` открывает кадр.
    #[test]
    fn call_opening_on_true_arg_opens_and_closes_mirrored() {
        let mut counters =
            SecurityCounters { privilege: pc(1, 1), unsafe_frame: PrivilegeCounter::ENTRY };
        apply_call_to_counters(
            &mut counters,
            call_info(Category::PrivilegedMode, true, ArgValue::KnownTrue),
        );
        assert_eq!(counters.privilege, pc(2, 2));

        let mut counters =
            SecurityCounters { privilege: pc(1, 1), unsafe_frame: PrivilegeCounter::ENTRY };
        apply_call_to_counters(
            &mut counters,
            call_info(Category::PrivilegedMode, false, ArgValue::KnownTrue),
        );
        assert_eq!(counters.privilege, pc(0, 0));
    }

    /// Зеркальная форма: `УстановитьБезопасныйРежим(Ложь)` снимает защиту, `(Истина)`
    /// её включает — то есть кадр открывает аргумент `Ложь`. Обе формы обязаны
    /// различаться, иначе вызовы двух категорий трактуются одинаково.
    #[test]
    fn call_opening_on_false_arg_opens_and_closes_mirrored() {
        let mut counters =
            SecurityCounters { privilege: PrivilegeCounter::ENTRY, unsafe_frame: pc(1, 1) };
        apply_call_to_counters(
            &mut counters,
            call_info(Category::SafeMode, false, ArgValue::KnownFalse),
        );
        assert_eq!(counters.unsafe_frame, pc(2, 2));

        let mut counters =
            SecurityCounters { privilege: PrivilegeCounter::ENTRY, unsafe_frame: pc(1, 1) };
        apply_call_to_counters(
            &mut counters,
            call_info(Category::SafeMode, true, ArgValue::KnownFalse),
        );
        assert_eq!(counters.unsafe_frame, pc(0, 0));
    }
}
