//! Who owns a bare receiver name at a position or outside the body.
//!
//! The decision itself is [`bare_global_name_claim`](crate::bare_global_name_claim) —
//! this module binds body-sensitive lookups to the enclosing body and exposes
//! nonlocal ownership without requiring a body scan.

use hir_def::item_tree::{ItemTree, MethodItem};
use hir_ty::db::HirDatabase;
use syntax::{SyntaxNode, TextSize};
use vfs::FileId;

use crate::{
    bare_global_name_claim, infer_owner, BareGlobalClaim, BodyShadowScope, DefWithBodyId, ExprId,
    ModuleId, Name, Resolver, TypeId,
};

/// The top-level method whose source range contains `offset`. Inclusive end:
/// while an unfinished method is being typed at the end of the file, the
/// cursor commonly sits exactly at the method's current end — it still
/// belongs to that method, not to module code.
pub fn method_item_at(item_tree: &ItemTree, offset: TextSize) -> Option<MethodItem<'_>> {
    item_tree.methods().find(|method| {
        let range = method.source_range();
        range.contains(offset) || range.end() == offset
    })
}

/// The user symbol claiming `name` at this read, plus the body owner needed to
/// type its reaching assignment. The read position is the receiver's own start,
/// so only assignments sequential inference has already completed claim the name.
pub fn claim_at<DB: HirDatabase>(
    db: &DB,
    file_id: FileId,
    offset: TextSize,
    name: &Name,
    read_offset: TextSize,
) -> (Option<BareGlobalClaim>, DefWithBodyId) {
    let module_id = ModuleId::new(file_id);
    let item_tree = db.item_tree(file_id);
    let module_bodies = db.module_bodies_ref(module_id);
    let (owner, lower_result) = match method_item_at(&item_tree, offset) {
        Some(method) => {
            (DefWithBodyId::Method(method.key()), module_bodies.lower_result(method.key()))
        }
        None => (DefWithBodyId::ModuleCode, module_bodies.module_code_result()),
    };
    let scope = lower_result.map(|r| BodyShadowScope {
        body: r.body(),
        source_map: r.source_map(),
        read_offset,
    });
    let resolver = Resolver::with_builtins_and_workspace(module_id);
    let body_env = hir_ty::owner_body_env(db, file_id, owner);
    (bare_global_name_claim(db, &resolver, scope.as_ref(), name, body_env), owner)
}

/// Whether the nonlocal owner of `name` is a visible common module.
///
/// The caller handles names declared in the body. This lookup intentionally
/// does not load the owner's body or source map.
pub fn nonlocal_common_module_receiver(
    db: &dyn HirDatabase,
    file_id: FileId,
    owner: DefWithBodyId,
    name: &Name,
) -> bool {
    let module_id = ModuleId::new(file_id);
    let resolver = Resolver::with_builtins_and_workspace(module_id);
    let has_form_context = hir_ty::owner_has_form_context(db, file_id, owner);
    hir_ty::nonlocal_common_module_receiver(db, &resolver, name, has_form_context)
}

/// Same, keyed off a receiver node: the read position is the node's own start.
pub fn claim_at_node<DB: HirDatabase>(
    db: &DB,
    file_id: FileId,
    offset: TextSize,
    name: &Name,
    root: &SyntaxNode,
) -> (Option<BareGlobalClaim>, DefWithBodyId) {
    claim_at(db, file_id, offset, name, root.text_range().start())
}

/// The inferred type of a reaching assignment's value — the claiming local's
/// type at the read. `None` when inference has nothing for the expression.
pub fn reaching_value_ty<DB: HirDatabase>(
    db: &DB,
    file_id: FileId,
    owner: DefWithBodyId,
    value_id: ExprId,
) -> Option<TypeId> {
    infer_owner(db, file_id, owner).type_id_of_expr(value_id)
}
