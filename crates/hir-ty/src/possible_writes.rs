//! Which possible writes recorded by reaching definitions may rebind a local.
//!
//! Reaching definitions record a bare-name argument of a call and every name at `Выполнить` as a
//! [`DefSite::MayWrite`] without knowing the callee. Whether the callee can write the argument
//! back needs the resolver, so it is answered here, from the call bindings inference made: every
//! reader of a local's value - inference, the argument diagnostics, the value origin walk - asks
//! the same question and gets the same answer.

use bsl_platform::ParamPassing;
use dataflow::reaching_defs::{DefSite, Definition, ReachingDefsResult, WriteEventKind};
use hir_def::body::events::{for_each_expr_child, EvalPoint};
use hir_def::body::Body;
use hir_def::hir::Expr;
use hir_def::{ExprId, IdConversion, StmtId};

use crate::db::HirDatabase;
use crate::infer::{argument_passing_of, CallArgBinding};

/// How surely a possible write rebinds the local it names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Rebinding {
    /// It cannot: an argument of a constructor, which only builds a new object from its
    /// arguments, or of a parameter proven to take its argument by value.
    Cannot,
    /// The callee's passing direction is not attested: it may write the local back, or not.
    Unattested,
    /// It certainly can: `Выполнить`, or a parameter proven to take its argument by reference.
    Certain,
}

/// How surely `def` rebinds its local; `None` when it is no possible write.
pub(crate) fn rebinding(
    db: &dyn HirDatabase,
    body: &Body,
    bindings: &[CallArgBinding],
    defs: &ReachingDefsResult,
    def: &Definition,
) -> Option<Rebinding> {
    let DefSite::MayWrite(id) = def.def_site else {
        return None;
    };
    Some(match defs.write(id).map(|write| write.kind) {
        Some(WriteEventKind::Argument { call, arg, .. }) => {
            if matches!(body.expr(call), Expr::New { .. }) {
                Rebinding::Cannot
            } else {
                match argument_passing_of(db, bindings, arg) {
                    Some(ParamPassing::ByVal) => Rebinding::Cannot,
                    Some(ParamPassing::ByRef) => Rebinding::Certain,
                    // `ParamPassing` is non-exhaustive: a direction that cannot be named is
                    // no proof of a copy.
                    _ => Rebinding::Unattested,
                }
            }
        }
        Some(WriteEventKind::Execute) => Rebinding::Certain,
        None => Rebinding::Unattested,
    })
}

/// Whether a possible write made at the point that evaluates `read` may happen before it.
/// The calls of one point are not ordered, so any call may write first, except one holding
/// the read: it writes only after its arguments are read. `Выполнить` runs after its own
/// expression.
pub(crate) fn may_write_before(
    body: &Body,
    defs: &ReachingDefsResult,
    def: &Definition,
    read: ExprId,
) -> bool {
    let DefSite::MayWrite(id) = def.def_site else {
        return false;
    };
    match defs.write(id).map(|write| write.kind) {
        Some(WriteEventKind::Argument { call, .. }) => !expr_contains(body, call, read),
        Some(WriteEventKind::Execute) | None => false,
    }
}

fn expr_contains(body: &Body, root: ExprId, target: ExprId) -> bool {
    if root == target {
        return true;
    }
    let mut found = false;
    for_each_expr_child(body, root.to_idx(), &mut |child| {
        found = found || expr_contains(body, ExprId::from_idx(child), target);
    });
    found
}

/// The definitions of `key` that reach `read` in statement `stmt`, with the possible writes its
/// own point may make before it. `None` when the solution holds no state for the read.
pub(crate) fn defs_reaching_read(
    body: &Body,
    defs: &ReachingDefsResult,
    key: &str,
    read: ExprId,
    stmt: StmtId,
) -> Option<Vec<Definition>> {
    let point = defs.events().point_of(read).unwrap_or(EvalPoint::Stmt(stmt));
    let mut reaching = match point {
        EvalPoint::Header(header) => defs.defs_for_var_at_header(key, header)?,
        EvalPoint::Stmt(_) => defs.defs_for_var_at_stmt(key, stmt)?,
    };
    reaching.extend(
        defs.possible_writes_at(point, key)
            .into_iter()
            .filter(|def| may_write_before(body, defs, def, read)),
    );
    Some(reaching)
}

/// Whether a write reaching definitions cannot read - a call that may write back the local it
/// is given, `Выполнить` - may have rebound `key` before `read` in statement `stmt`. `defs` is
/// an executable solution, so a write only dead code performs reaches nothing. No state for the
/// read answers `true`: the absence of such a write is then unproven, and a lost finding is
/// cheaper than an invented one.
pub(crate) fn rebinding_write_reaches(
    db: &dyn HirDatabase,
    body: &Body,
    bindings: &[CallArgBinding],
    defs: &ReachingDefsResult,
    key: &str,
    read: ExprId,
    stmt: StmtId,
) -> bool {
    let Some(reaching) = defs_reaching_read(body, defs, key, read, stmt) else {
        return true;
    };
    reaching.iter().any(|def| {
        rebinding(db, body, bindings, defs, def)
            .is_some_and(|rebinding| rebinding != Rebinding::Cannot)
    })
}
