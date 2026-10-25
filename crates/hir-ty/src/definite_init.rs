//! Inputs and verdict parts of the definite-initialization check: which names
//! are tracked, which reads are judged, which places initialize, when a body
//! proves nothing, and how a path without initialization is described.
//!
//! The functions see only the body, its CFG and the analysis result; whatever
//! needs the resolver arrives from `InferenceContext` as data or a closure.

use cfg_types::IdConversion;
use dataflow::definite_init::{
    DefiniteInitResult, EnteringHeader, InitEvents, InitPoint, WitnessStep,
};
use hir_def::body::Body;
use hir_def::hir::{Expr, ExprIdx, Literal, Stmt, StmtIdx};
use hir_def::{ExprId, Name, StmtId};
use rustc_hash::{FxHashMap, FxHashSet};
use stdx::case::CaseExt;

use crate::infer::{assignment_target_base, UninitializedPath};
use crate::narrow::{for_each_expr_child, for_each_stmt_expr_root, stmt_covers_stmt};

/// How a call passes one argument, as far as the resolved callee proves it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ArgPassing {
    ByVal,
    ByRef,
    Unknown,
}

/// The body may be missing statements or edges, so no path through it is proven.
pub(crate) fn hir_suppressed(body: &Body) -> bool {
    if body.is_lowering_incomplete() {
        return true;
    }
    let mut labels = FxHashSet::default();
    let mut gotos = Vec::new();
    for (_, stmt) in body.stmts_iter() {
        match stmt {
            Stmt::Expr(expr) if body.is_recovered(ExprId::from_idx(*expr)) => return true,
            Stmt::Label(label) => {
                labels.insert(label);
            }
            Stmt::Goto(label) => gotos.push(label),
            _ => {}
        }
    }
    gotos.into_iter().any(|label| !labels.contains(label))
}

/// Names the body may initialize itself: implicit locals, bases of indexed
/// assignment targets and loop variables, one per `fold_lower` key.
pub(crate) fn tracked_name_sources<'a>(
    body: &Body,
    implicit_local_names: impl Iterator<Item = &'a Name>,
) -> Vec<Name> {
    let mut seen = FxHashSet::default();
    let mut names = Vec::new();
    let mut push = |name: &Name| {
        if seen.insert(name.as_str().fold_lower()) {
            names.push(name.clone());
        }
    };
    implicit_local_names.for_each(&mut push);
    for (_, stmt) in body.stmts_iter() {
        match stmt {
            Stmt::Assign { target, .. } => {
                let target = ExprId::from_idx(*target);
                if matches!(body.expr(target), Expr::Index { .. }) {
                    if let Some((_, name)) = assignment_target_base(body, target) {
                        push(name);
                    }
                }
            }
            Stmt::For { var, .. } | Stmt::ForEach { var, .. } => {
                push(&body.binding(hir_def::BindingId::from_idx(*var)).name);
            }
            _ => {}
        }
    }
    names
}

/// Tracked names as `fold_lower` keys; a name's bit is its position in the
/// ascending key order.
pub(crate) struct Tracked {
    keys: Vec<String>,
}

impl Tracked {
    /// Drops the names the body declares itself (parameters and `Перем`): their
    /// initial value is not this check's question. Names owned outside the body
    /// are expected to be dropped by the caller.
    pub(crate) fn new(body: &Body, names: Vec<Name>) -> Self {
        let mut declared: FxHashSet<String> =
            body.params().map(|param| body.binding(param).name.as_str().fold_lower()).collect();
        for (_, stmt) in body.stmts_iter() {
            if let Stmt::VarDecl { bindings } = stmt {
                declared.extend(bindings.iter().map(|binding| {
                    body.binding(hir_def::BindingId::from_idx(*binding)).name.as_str().fold_lower()
                }));
            }
        }
        let mut keys: Vec<String> = names
            .iter()
            .map(|name| name.as_str().fold_lower())
            .filter(|key| !declared.contains(key))
            .collect();
        keys.sort_unstable();
        keys.dedup();
        Self { keys }
    }

    pub(crate) fn bit(&self, key: &str) -> Option<u32> {
        self.keys.binary_search_by(|probe| probe.as_str().cmp(key)).ok().map(|pos| pos as u32)
    }

    pub(crate) fn width(&self) -> usize {
        self.keys.len()
    }

    fn name_bit(&self, name: &Name) -> Option<u32> {
        self.bit(&name.as_str().fold_lower())
    }
}

/// A read judged by the check: `expr` reads `name` at `point` of statement `owner`.
pub(crate) struct Candidate {
    pub(crate) expr: ExprId,
    pub(crate) name: Name,
    pub(crate) bit: u32,
    pub(crate) point: InitPoint,
    pub(crate) owner: StmtId,
}

pub(crate) struct Collected {
    pub(crate) events: InitEvents,
    /// Bits a point initializes while it is evaluated (arguments not known as
    /// `Знач`): a read in the same point is not ordered against them.
    pub(crate) during: FxHashMap<InitPoint, FxHashSet<u32>>,
    pub(crate) candidates: Vec<Candidate>,
}

/// One pass over the statements: the reads to judge and every initialization event.
pub(crate) fn collect(
    body: &Body,
    tracked: &Tracked,
    local_reads: &FxHashSet<ExprId>,
    passing: &impl Fn(ExprId) -> ArgPassing,
) -> Collected {
    let mut events = InitEvents { width: tracked.width(), ..InitEvents::default() };
    let mut during: FxHashMap<InitPoint, FxHashSet<u32>> = FxHashMap::default();
    let mut candidates = Vec::new();

    for (stmt_id, stmt) in body.stmts_iter() {
        let index_base = match stmt {
            Stmt::Assign { target, .. } => {
                let target = ExprId::from_idx(*target);
                match body.expr(target) {
                    Expr::Index { .. } => assignment_target_base(body, target).map(|(id, _)| id),
                    _ => None,
                }
            }
            _ => None,
        };

        let mut roots: Vec<(InitPoint, ExprIdx)> = Vec::new();
        let header = |root: &ExprIdx| (InitPoint::Header(ExprId::from_idx(*root)), *root);
        match stmt {
            Stmt::If(if_stmt) => {
                roots.push(header(&if_stmt.condition));
                roots.extend(if_stmt.elsif_branches.iter().map(|(condition, _)| header(condition)));
            }
            Stmt::While { condition, .. } => roots.push(header(condition)),
            Stmt::For { from, to, .. } => roots.extend([header(from), header(to)]),
            Stmt::ForEach { collection, .. } => roots.push(header(collection)),
            _ => {
                let stmt_idx: StmtIdx = stmt_id.to_idx();
                for_each_stmt_expr_root(body, stmt_idx, &mut |root| {
                    roots.push((InitPoint::Stmt(stmt_id), root));
                });
            }
        }

        for (point, root) in roots {
            let mut nodes = vec![root];
            let mut pos = 0;
            while let Some(&node) = nodes.get(pos) {
                for_each_expr_child(body, node, &mut |child| nodes.push(child));
                pos += 1;
            }

            let mut callees = FxHashSet::default();
            let mut arguments: FxHashSet<ExprIdx> = FxHashSet::default();
            for &node in &nodes {
                let args = match body.expr_idx(node) {
                    Expr::Call { callee, args } => {
                        callees.insert(*callee);
                        args
                    }
                    Expr::MethodCall { args, .. } | Expr::New { args, .. } => args,
                    _ => continue,
                };
                arguments.extend(
                    args.iter().filter(|arg| matches!(body.expr_idx(**arg), Expr::Path(_))),
                );
            }

            for node in nodes {
                let Expr::Path(name) = body.expr_idx(node) else { continue };
                let Some(bit) = tracked.name_bit(name) else { continue };
                let expr = ExprId::from_idx(node);
                let judged = if arguments.contains(&node) {
                    if passing(expr) == ArgPassing::ByVal {
                        true
                    } else {
                        during.entry(point).or_default().insert(bit);
                        false
                    }
                } else {
                    !callees.contains(&node) && index_base != Some(expr)
                };
                if judged && local_reads.contains(&expr) {
                    candidates.push(Candidate {
                        expr,
                        name: name.clone(),
                        bit,
                        point,
                        owner: stmt_id,
                    });
                }
            }
        }

        let mut after: Vec<u32> = match stmt {
            Stmt::Assign { target, .. } => assignment_target_base(body, ExprId::from_idx(*target))
                .and_then(|(_, name)| tracked.name_bit(name))
                .into_iter()
                .collect(),
            Stmt::Execute { .. } => (0..tracked.width() as u32).collect(),
            _ => Vec::new(),
        };
        if let Some(bits) = during.get(&InitPoint::Stmt(stmt_id)) {
            after.extend(bits);
        }
        if !after.is_empty() {
            after.sort_unstable();
            after.dedup();
            events.stmt.insert(stmt_id, after);
        }

        if let Stmt::For { var, .. } | Stmt::ForEach { var, .. } = stmt {
            let var = hir_def::BindingId::from_idx(*var);
            if let Some(bit) = tracked.name_bit(&body.binding(var).name) {
                events.loop_var.insert(var, bit);
            }
        }
        if let Some(entering) = entering_header(body, stmt) {
            events.entering_headers.insert(stmt_id, entering);
        }
    }

    for (point, bits) in &during {
        if let InitPoint::Header(root) = point {
            let mut bits: Vec<u32> = bits.iter().copied().collect();
            bits.sort_unstable();
            events.header.insert(*root, bits);
        }
    }

    Collected { events, during, candidates }
}

/// The two loop headers whose literal condition is evaluated: `Пока Истина`
/// never leaves by its false branch, `Для` over literal bounds `from ≤ to`
/// enters the body on the first arrival.
fn entering_header(body: &Body, stmt: &Stmt) -> Option<EnteringHeader> {
    match stmt {
        Stmt::While { condition, .. } => {
            matches!(body.expr_idx(*condition), Expr::Literal(Literal::Bool(true)))
                .then_some(EnteringHeader::NeverExits)
        }
        Stmt::For { from, to, .. } => match (body.expr_idx(*from), body.expr_idx(*to)) {
            (Expr::Literal(Literal::Number(from)), Expr::Literal(Literal::Number(to))) => {
                (from <= to).then_some(EnteringHeader::EntersOnFirstArrival)
            }
            _ => None,
        },
        _ => None,
    }
}

/// What the witness says about the path without initialization, checked from
/// the step nearest the read back to the start; the first rule that matches wins.
pub(crate) fn path_kind(
    body: &Body,
    result: &DefiniteInitResult,
    witness: &[WitnessStep],
    owner: StmtId,
    bit: u32,
) -> UninitializedPath {
    use cfg::{CfgEdgeType, CfgVertex};

    let covers =
        |root: StmtId, inner: StmtId| stmt_covers_stmt(body, root.to_idx(), inner.to_idx());
    let cfg = result.cfg();
    for step in witness.iter().rev() {
        let Some(stmt) = cfg.source_stmt_id(step.vertex) else { continue };
        match cfg.vertex(step.vertex) {
            Some(
                CfgVertex::WhileHeader(_) | CfgVertex::ForHeader(_) | CfgVertex::ForEachHeader(_),
            ) => {
                let inside = owner != stmt && covers(stmt, owner);
                let edge_matches = match step.edge {
                    CfgEdgeType::TrueBranch => inside,
                    CfgEdgeType::FalseBranch => !inside,
                    _ => false,
                };
                if edge_matches && result.back_edges_initialize(step.vertex, bit) {
                    return if inside {
                        UninitializedPath::FirstIteration
                    } else {
                        UninitializedPath::LoopNotRun
                    };
                }
            }
            Some(CfgVertex::Conditional(_))
                if matches!(step.edge, CfgEdgeType::TrueBranch | CfgEdgeType::FalseBranch)
                    && !covers(stmt, owner) =>
            {
                return UninitializedPath::Branch;
            }
            _ => {}
        }
    }
    UninitializedPath::OtherPath
}
