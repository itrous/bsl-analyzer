//! Where the value of an expression came from, proven on EVERY path that reaches it.
//!
//! The question a rule asks is "is every value this expression can hold the text of a type
//! name" (`Строка(ТипЗнч(О))`, kept through `ВРег`, a local, a branch). The answer has three
//! parts, and they are not symmetric:
//!
//! * [`ValueOrigin::TypeNameText`] - every path was seen and every one ends in such a text;
//! * [`ValueOrigin::Other`] - some value is definitely something else. One such value makes the
//!   statement "every value here is the text of a type name" FALSE, and that is a proven
//!   statement, so `Other` dominates everything it is met with;
//! * [`ValueOrigin::Unproven`] - the set of values could not be seen whole.
//!
//! The anchor is the INFERRED TYPE of the argument of a call whose parameter is flagged
//! `result_follows_argument_text` - the platform type `Тип` - never the callee's name.

use std::sync::Arc;

use dataflow::reaching_defs::{header_read_expr, DefSite, ReachingDefsResult};
use hir_def::body::Body;
use hir_def::hir::{Expr, Stmt};
use hir_def::{
    BindingId, DefWithBodyId, ExprId, IdConversion, MethodId, MethodIdInput, ModuleId, StmtId,
};
use rustc_hash::FxHashMap;
use stdx::case::CaseExt;
use vfs::FileId;

use bsl_types::kind::{TypeId, TypeKind};

use crate::call_resolution::CallSelection;
use crate::db::HirDatabase;
use crate::definite_init::ArgPassing;
use crate::infer::{argument_passing_of, CallArgBinding};
use crate::narrow::local_may_be_rewritten;

/// What the walk proved about the values of an expression.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueOrigin {
    /// Every value on every path is the text of a type name.
    TypeNameText,
    /// Some value is definitely not: a proven negative.
    Other,
    /// The set of values could not be seen whole, for the stated reason.
    Unproven(Unproven),
}

/// Why the walk could not decide. The order of the variants is the order of the meet below.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unproven {
    /// The only obstacle is a call whose passing direction is not attested: the value may have
    /// been overwritten, or may not. A reader is allowed to look past this one - which is the
    /// policy of issue #854 item 5 - and no other.
    RewriteDirectionUnknown,
    /// A writer the walk cannot see through can certainly write: `Выполнить`, a call PROVEN to
    /// take the argument by reference, or a write an error can skip past.
    Rewritten,
    /// A definition the walk cannot read: `DefSite::Unknown`, no definition at all, or a value
    /// that comes from a call the walk cannot resolve to one callee.
    OpaqueDefinition,
    /// The walk stopped itself: the depth cap, or a value defined through itself.
    WalkLimit,
}

impl ValueOrigin {
    /// The meet of two definitions reaching one use.
    ///
    /// `Other` dominates `Unproven`, which dominates `TypeNameText`. Among `Unproven` reasons
    /// `RewriteDirectionUnknown` is the WEAKEST - a reader may choose to look past it - so any
    /// other reason survives the meet and replaces it. The remaining three are
    /// indistinguishable to any reader; their order is their declaration order and the walk
    /// keeps the first one it meets (`self`, the one met earlier), not whichever came last.
    fn meet(self, other: ValueOrigin) -> ValueOrigin {
        match (self, other) {
            (ValueOrigin::Other, _) | (_, ValueOrigin::Other) => ValueOrigin::Other,
            (ValueOrigin::TypeNameText, ValueOrigin::TypeNameText) => ValueOrigin::TypeNameText,
            (ValueOrigin::Unproven(reason), ValueOrigin::TypeNameText)
            | (ValueOrigin::TypeNameText, ValueOrigin::Unproven(reason)) => {
                ValueOrigin::Unproven(reason)
            }
            (ValueOrigin::Unproven(first), ValueOrigin::Unproven(second)) => {
                ValueOrigin::Unproven(match (first, second) {
                    (Unproven::RewriteDirectionUnknown, stronger) => stronger,
                    (kept, _) => kept,
                })
            }
        }
    }
}

const DEPTH_CAP: u32 = 16;

/// Walks the definitions of an expression of one body. Lazy: the reaching definitions are
/// requested only when a local is met.
pub struct ValueOriginWalk<'a> {
    db: &'a dyn HirDatabase,
    file_id: FileId,
    owner: DefWithBodyId,
    body: &'a Body,
    bindings: &'a [CallArgBinding],
    expr_types: &'a FxHashMap<ExprId, TypeId>,
    // Kept for parity with the other walks over one inference result; a parameter has no
    // assigned value to follow, so the walk reads no binding type.
    #[allow(dead_code)]
    binding_types: &'a FxHashMap<BindingId, TypeId>,
    reaching: Option<Option<Arc<ReachingDefsResult>>>,
    memo: FxHashMap<ExprId, ValueOrigin>,
}

impl<'a> ValueOriginWalk<'a> {
    pub fn new(
        db: &'a dyn HirDatabase,
        file_id: FileId,
        owner: DefWithBodyId,
        body: &'a Body,
        bindings: &'a [CallArgBinding],
        expr_types: &'a FxHashMap<ExprId, TypeId>,
        binding_types: &'a FxHashMap<BindingId, TypeId>,
    ) -> Self {
        Self {
            db,
            file_id,
            owner,
            body,
            bindings,
            expr_types,
            binding_types,
            reaching: None,
            memo: FxHashMap::default(),
        }
    }

    /// Whether the value of `expr` is the text of a type name on every path reaching it.
    pub fn value_origin_on_every_path(&mut self, expr: ExprId) -> ValueOrigin {
        self.walk(expr, 0)
    }

    fn walk(&mut self, expr: ExprId, depth: u32) -> ValueOrigin {
        if depth > DEPTH_CAP {
            return ValueOrigin::Unproven(Unproven::WalkLimit);
        }
        if let Some(known) = self.memo.get(&expr) {
            return *known;
        }
        // A value defined through itself (`Х = Х.Родитель`) proves nothing.
        self.memo.insert(expr, ValueOrigin::Unproven(Unproven::WalkLimit));
        let origin = match self.body.expr(expr) {
            // Inference binds a call under its callee expression, not the call itself.
            Expr::Call { callee, .. } => self.walk_call(ExprId::from_idx(*callee), depth),
            Expr::Path(name) => self.walk_name(expr, name.as_str(), depth),
            _ => ValueOrigin::Other,
        };
        self.memo.insert(expr, origin);
        origin
    }

    fn walk_call(&mut self, callee: ExprId, depth: u32) -> ValueOrigin {
        let opaque = ValueOrigin::Unproven(Unproven::OpaqueDefinition);
        let Some(binding) = self.bindings.iter().find(|binding| binding.call_expr == callee) else {
            return opaque;
        };
        // The same test `argument_passing_of` applies: one candidate, or the call proves nothing.
        let candidate = match &binding.candidate.resolution.selection {
            CallSelection::Unique { candidate } => *candidate,
            CallSelection::Ambiguous { candidates } if candidates.len() == 1 => candidates[0],
            _ => return opaque,
        };
        let Some(signature) = binding
            .candidate
            .candidates
            .as_slice()
            .iter()
            .find(|signature| signature.id == candidate)
        else {
            return opaque;
        };
        let followed: Vec<ExprId> = signature
            .params
            .iter()
            .enumerate()
            .filter(|(_, param)| param.result_follows_argument_text)
            .filter_map(|(index, _)| binding.args.get(index).copied())
            .collect();
        match followed.as_slice() {
            [] => ValueOrigin::Other,
            [argument] => {
                let argument = *argument;
                if self.is_type_value(argument) {
                    ValueOrigin::TypeNameText
                } else {
                    self.walk(argument, depth + 1)
                }
            }
            // Not a shape the walk claims to understand.
            _ => opaque,
        }
    }

    /// The inferred type of the argument is the platform type `Тип`.
    ///
    /// In this tree `TypeKind::TypeDescriptor` denotes `Тип`, whatever its Rust name suggests:
    /// `bsl-types/src/display.rs` prints it as `Тип` / `Type`, and `method_lookup.rs` maps it to
    /// the key `"Type"`. `ОписаниеТипов` is a different notion (`typedescription`, a constructor
    /// returning `Unknown`). The anchor matches the interned variant and nothing else - no name
    /// comparison: a variant is a stable identity, a facet name is not.
    fn is_type_value(&self, expr: ExprId) -> bool {
        self.expr_types
            .get(&expr)
            .is_some_and(|ty| matches!(self.db.lookup_type(*ty), TypeKind::TypeDescriptor))
    }

    fn is_local(&self, key: &str) -> bool {
        let names_key = |expr: &hir_def::hir::ExprIdx| matches!(self.body.expr_idx(*expr), Expr::Path(name) if name.as_str().fold_lower() == key);
        self.body.bindings_iter().any(|(_, binding)| binding.name.as_str().fold_lower() == key)
            || self.body.stmts_iter().any(|(_, stmt)| match stmt {
                Stmt::Assign { target, .. } => names_key(target),
                _ => false,
            })
    }

    fn walk_name(&mut self, expr: ExprId, name: &str, depth: u32) -> ValueOrigin {
        let key = name.fold_lower();
        // A module variable or a global is not a value this body wrote: stating "every value
        // here is a type name's text" about it is false for want of any such write, which is
        // a proven negative, not an unseen set.
        if !self.is_local(&key) {
            return ValueOrigin::Other;
        }
        let Some(stmt) = self.body.enclosing_stmt(expr) else {
            return ValueOrigin::Unproven(Unproven::OpaqueDefinition);
        };

        // Two questions to the same scan, differing in the oracle only. The conservative one
        // asks "may be rewritten unless every call is PROVEN to take it by value". The second
        // counts only calls PROVEN to take it by reference, plus the writers the oracle does
        // not govern at all - `Выполнить`, a write the handler of `Попытка` does not see.
        // Those are not a matter of an unknown direction, which is why `Выполнить` lands in
        // `Rewritten` and not under the policy knob: it can certainly write.
        //
        // `own_call` is `None` on purpose: the operand need not be a call argument, and counting
        // its own call as a possible rewrite loses findings but invents none.
        let passing = |arg| argument_passing_of(self.db, self.bindings, arg);
        let may_rewrite = local_may_be_rewritten(self.body, &key, stmt.to_idx(), None, &|arg| {
            passing(arg) == ArgPassing::ByVal
        });
        if may_rewrite {
            let proven_writer =
                local_may_be_rewritten(self.body, &key, stmt.to_idx(), None, &|arg| {
                    passing(arg) != ArgPassing::ByRef
                });
            return ValueOrigin::Unproven(if proven_writer {
                Unproven::Rewritten
            } else {
                Unproven::RewriteDirectionUnknown
            });
        }

        // A read inside an `Если` / `Пока` condition or a `Для` bound happens at the header, which
        // is not a statement of any block: its definitions are asked by the expression the header
        // evaluates first, as inference does.
        let header = header_read_expr(self.body, stmt);
        let Some(defs) = self.reaching().and_then(|reaching| match header {
            Some(header) => reaching.defs_for_var_at_header(name, header),
            None => reaching.defs_for_var_at_stmt(name, stmt),
        }) else {
            return ValueOrigin::Unproven(Unproven::OpaqueDefinition);
        };
        if defs.is_empty() {
            return ValueOrigin::Unproven(Unproven::OpaqueDefinition);
        }
        let mut met: Option<ValueOrigin> = None;
        for def in &defs {
            let origin = match def.def_site {
                DefSite::Assignment(stmt_raw) => match self.body.stmt(StmtId::from_raw(stmt_raw)) {
                    Stmt::Assign { value, .. } => {
                        let value = ExprId::from_idx(*value);
                        self.walk(value, depth + 1)
                    }
                    _ => ValueOrigin::Unproven(Unproven::OpaqueDefinition),
                },
                DefSite::Parameter(_)
                | DefSite::VarDecl(_)
                | DefSite::ForLoop(_)
                | DefSite::ForEachLoop(_) => ValueOrigin::Other,
                DefSite::Unknown => ValueOrigin::Unproven(Unproven::OpaqueDefinition),
            };
            let merged = met.map_or(origin, |earlier| earlier.meet(origin));
            if merged == ValueOrigin::Other {
                return ValueOrigin::Other;
            }
            met = Some(merged);
        }
        met.unwrap_or(ValueOrigin::Unproven(Unproven::OpaqueDefinition))
    }

    fn reaching(&mut self) -> Option<Arc<ReachingDefsResult>> {
        self.reaching
            .get_or_insert_with(|| match self.owner {
                DefWithBodyId::Method(local_id) => {
                    let module = ModuleId::new(self.file_id);
                    self.db.method_reaching_definitions(MethodIdInput::new(
                        self.db,
                        MethodId { module, local_id },
                    ))
                }
                DefWithBodyId::ModuleCode => self.db.module_code_reaching_definitions(self.file_id),
            })
            .clone()
    }
}

/// The value origin of one expression of one body, with the walk assembled from the database:
/// the body and the inference result are fetched here, so a caller outside this crate needs
/// neither. A body that cannot be found proves nothing.
pub fn value_origin_of(
    db: &dyn HirDatabase,
    file_id: FileId,
    owner: DefWithBodyId,
    expr: ExprId,
) -> ValueOrigin {
    match owner {
        DefWithBodyId::Method(local_id) => {
            let method = MethodId { module: ModuleId::new(file_id), local_id };
            let input = MethodIdInput::new(db, method);
            let body = db.method_body_ref(input);
            let infer = db.infer_method_ref(input);
            ValueOriginWalk::new(
                db,
                file_id,
                owner,
                body,
                &infer.call_arg_bindings,
                &infer.expr_types,
                &infer.binding_types,
            )
            .value_origin_on_every_path(expr)
        }
        DefWithBodyId::ModuleCode => {
            let module_bodies = db.module_bodies_ref(ModuleId::new(file_id));
            let Some(body) = module_bodies.module_code_arc() else {
                return ValueOrigin::Unproven(Unproven::OpaqueDefinition);
            };
            let infer = db.infer_module_code_ref(file_id);
            ValueOriginWalk::new(
                db,
                file_id,
                owner,
                body,
                &infer.call_arg_bindings,
                &infer.expr_types,
                &infer.binding_types,
            )
            .value_origin_on_every_path(expr)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIR: ValueOrigin = ValueOrigin::Unproven(Unproven::RewriteDirectionUnknown);
    const LIMIT: ValueOrigin = ValueOrigin::Unproven(Unproven::WalkLimit);
    const OPAQUE: ValueOrigin = ValueOrigin::Unproven(Unproven::OpaqueDefinition);
    const REWRITTEN: ValueOrigin = ValueOrigin::Unproven(Unproven::Rewritten);

    #[test]
    fn other_dominates_every_unproven_reason() {
        for unproven in [DIR, LIMIT, OPAQUE, REWRITTEN, ValueOrigin::TypeNameText] {
            assert_eq!(unproven.meet(ValueOrigin::Other), ValueOrigin::Other);
            assert_eq!(ValueOrigin::Other.meet(unproven), ValueOrigin::Other);
        }
    }

    #[test]
    fn the_weak_reason_yields_to_any_other_in_either_order() {
        assert_eq!(DIR.meet(LIMIT), LIMIT);
        assert_eq!(LIMIT.meet(DIR), LIMIT);
        assert_eq!(DIR.meet(REWRITTEN), REWRITTEN);
        assert_eq!(DIR.meet(DIR), DIR);
    }

    #[test]
    fn the_three_equal_reasons_keep_the_one_met_first() {
        assert_eq!(OPAQUE.meet(LIMIT), OPAQUE);
        assert_eq!(LIMIT.meet(REWRITTEN), LIMIT);
        assert_eq!(REWRITTEN.meet(OPAQUE), REWRITTEN);
    }

    #[test]
    fn a_text_of_a_type_name_yields_to_an_unproven_one() {
        assert_eq!(ValueOrigin::TypeNameText.meet(DIR), DIR);
        assert_eq!(
            ValueOrigin::TypeNameText.meet(ValueOrigin::TypeNameText),
            ValueOrigin::TypeNameText
        );
    }
}
