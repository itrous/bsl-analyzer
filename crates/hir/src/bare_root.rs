//! Who owns a bare receiver name at a position.
//!
//! The decision itself is [`bare_global_name_claim`](crate::bare_global_name_claim) —
//! this module only binds a position to the enclosing body so the predicate can
//! be asked positionally. Every consumer that types a receiver from its
//! identifier text must go through here; a consumer with its own guard set
//! drifts from the diagnostic's verdict, which is how the shadowed-global
//! suggestions kept coming back.

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
    let item_tree = db.item_tree(file_id);
    let owner = method_item_at(&item_tree, offset)
        .map(|method| DefWithBodyId::Method(method.key()))
        .unwrap_or(DefWithBodyId::ModuleCode);
    with_scope(db, file_id, owner, read_offset, |resolver, scope| {
        let body_env = hir_ty::owner_body_env(db, file_id, owner);
        (bare_global_name_claim(db, resolver, scope, name, body_env), owner)
    })
}

/// Whether the bare name at this position belongs to a common module rather than
/// a nearer local, parameter, module declaration or implicit member.
pub fn common_module_at(
    db: &dyn HirDatabase,
    file_id: FileId,
    owner: DefWithBodyId,
    offset: TextSize,
    name: &Name,
) -> Option<crate::CommonModuleRead> {
    with_scope(db, file_id, owner, offset, |resolver, scope| {
        let scope = scope?;
        let body_env = hir_ty::owner_body_env(db, file_id, owner);
        let has_form_context = hir_ty::owner_has_form_context(db, file_id, owner);
        Some(hir_ty::platform_global_lookup::bare_name_common_module_read(
            db,
            resolver,
            scope,
            name,
            body_env,
            has_form_context,
        ))
    })
}

fn with_scope<R>(
    db: &dyn HirDatabase,
    file_id: FileId,
    owner: DefWithBodyId,
    read_offset: TextSize,
    f: impl FnOnce(&Resolver, Option<&BodyShadowScope<'_>>) -> R,
) -> R {
    let module_id = ModuleId::new(file_id);
    let module_bodies = db.module_bodies_ref(module_id);
    let lower_result = match owner {
        DefWithBodyId::Method(key) => module_bodies.lower_result(key),
        DefWithBodyId::ModuleCode => module_bodies.module_code_result(),
    };
    let scope = lower_result.map(|r| BodyShadowScope {
        body: r.body(),
        source_map: r.source_map(),
        read_offset,
    });
    let resolver = Resolver::with_builtins_and_workspace(module_id);
    f(&resolver, scope.as_ref())
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
