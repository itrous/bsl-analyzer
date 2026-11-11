//! Types a slot documented as `см. Модуль.Метод` by reading the documentation it points at.
//!
//! The 1C convention writes a structure once and refers to it everywhere else:
//!
//! ```text
//! // Параметры:
//! //   Параметры - см. ОбщегоНазначенияКлиентСервер.ПараметрыЗаписи
//! ```
//!
//! Answering what such a slot holds needs the target module, so it cannot happen where the
//! signature is lowered — that path is pure, cheap and shared by every call site. It happens here
//! instead, memoised per referring method, and only IDE consumers read it.
//!
//! Two properties this module exists to keep:
//!
//! **Acyclic in salsa.** The query never calls itself. A chain `см. → см. → …` is walked by
//! [`Driver`] with its own visited set, exactly as [`crate::structure_param_keys`] does, so a
//! caller that is itself a fixpoint (inference) reads a settled value.
//!
//! **Documentation only.** The recursion runs on `&dyn SeeTargets` and `&dyn TypeKernelDb`.
//! Neither offers inference, so a target's body cannot be read from here — not by convention, but
//! because the call does not compile. Keys a body proves are out of scope by that same boundary.

use std::cell::RefCell;
use std::sync::Arc;

use rustc_hash::{FxHashMap, FxHashSet};

use bsl_types::builders::Builders;
use bsl_types::intern::TypeKernelDb;
use bsl_types::kind::TypeId;
use hir_def::docs::{parse_type_expr, DocTypeExpr, TypeDoc};
use hir_def::module_interface::MethodDecl;
use hir_def::resolver::Resolver;
use hir_def::{MethodId, MethodIdInput, ModuleId, Name, QualifiedName};
use stdx::case::CaseExt;

use crate::db::HirDatabase;
use crate::lower::doc_structure::{self, SeePolicy};

/// Bounds a chain of references. The visited set is what makes the walk terminate; this only keeps
/// a pathological configuration from spending the whole budget in one slot.
const MAX_SEE_DEPTH: usize = 32;

/// Bounds the total work of one query. Unlike the depth this counts references followed across all
/// slots of the method, so a signature with a thousand referring parameters cannot walk a thousand
/// deep chains.
const MAX_SEE_EXPANSIONS: usize = 1024;

/// Which slot of a method a reference names: two segments mean its returned value, three name one
/// of its parameters. Both are separate nodes of the walk — `А.М` and `А.М.П` are different
/// answers and must not share a visited entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Slot {
    Ret,
    Param(u32),
}

/// The types resolving references gave a method's slots. An absent entry means the slot keeps the
/// type the signature already has.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DocSeeSignature {
    pub ret: Option<TypeId>,

    pub params: FxHashMap<u32, TypeId>,
}

impl DocSeeSignature {
    pub fn is_empty(&self) -> bool {
        self.ret.is_none() && self.params.is_empty()
    }

    pub fn param(&self, index: usize) -> Option<TypeId> {
        u32::try_from(index).ok().and_then(|index| self.params.get(&index).copied())
    }
}

/// Approximate live heap of a memoised result, for salsa's `heap_size` hook: the boxed struct and
/// the parameter table. The types themselves are interned and shared, so they are not counted.
pub(crate) fn doc_see_signature_heap(value: &Arc<DocSeeSignature>) -> usize {
    use std::mem::size_of;

    size_of::<DocSeeSignature>()
        + crate::infer::heap_estimate::map_table_bytes::<u32, TypeId>(value.params.len())
}

/// Finding the method a reference names. The whole capability the walk is given over the
/// workspace: enough to follow a reference, not enough to read a body.
trait SeeTargets {
    fn method(
        &self,
        from: ModuleId,
        module: &Name,
        method: &Name,
    ) -> Option<(MethodId, Arc<MethodDecl>)>;
}

struct WorkspaceTargets<'db> {
    db: &'db dyn HirDatabase,
}

impl SeeTargets for WorkspaceTargets<'_> {
    fn method(
        &self,
        from: ModuleId,
        module: &Name,
        method: &Name,
    ) -> Option<(MethodId, Arc<MethodDecl>)> {
        let resolver = Resolver::with_workspace_scope(from);
        let resolution = resolver.resolve_qualified_method(self.db, module, method).ok()?;
        // Documentation on a method its own module cannot see is not documentation this slot may
        // rely on: the reference names something the referring module has no access to.
        if !resolution.is_export {
            return None;
        }
        let symbol = self.db.interface_method(resolution.method_id)?;
        Some((resolution.method_id, symbol))
    }
}

#[derive(Default)]
struct State {
    /// Nodes on the current path. Re-entering one is a cycle, and the walk stops there.
    visited: FxHashSet<(MethodId, Slot)>,

    /// Nodes whose type is settled. Only results computed without any truncation land here — a
    /// value degraded because someone else's descent ran out of budget must not be handed to the
    /// next reference as if it were the answer.
    completed: FxHashMap<(MethodId, Slot), TypeId>,

    /// References followed so far, against [`MAX_SEE_EXPANSIONS`].
    expansions: usize,

    /// How often the walk stopped short. Bracketing a subtree's computation with this counter
    /// tells whether that subtree saw the whole chain or a cut one.
    truncations: usize,

    /// How often a resolved type was accepted. A slot whose lowering accepted nothing keeps the
    /// signature's own type, so re-lowering can never disturb a slot the references did not reach.
    accepted: usize,
}

struct Driver<'a> {
    targets: &'a dyn SeeTargets,
    state: RefCell<State>,
    /// The `mapKeyTypes` profile, so a slot reached through a reference reads its key items the
    /// same way the signature does.
    map_keys: bool,
}

impl Driver<'_> {
    /// The type a reference standing in `from` resolves to, or `None` to leave it permissive.
    fn resolve(
        &self,
        db: &dyn TypeKernelDb,
        from: ModuleId,
        name: &QualifiedName,
    ) -> Option<TypeId> {
        let (method_id, symbol, slot) = self.target(from, name)?;
        let key = (method_id, slot);

        // Bound before the branch on purpose: an `if let` would hold the borrow across the body,
        // and everything below it takes the state mutably.
        let settled = self.state.borrow().completed.get(&key).copied();
        if let Some(settled) = settled {
            return self.accept(db, settled);
        }

        {
            let mut state = self.state.borrow_mut();
            let out_of_budget =
                state.visited.len() >= MAX_SEE_DEPTH || state.expansions >= MAX_SEE_EXPANSIONS;
            if out_of_budget || !state.visited.insert(key) {
                state.truncations += 1;
                return None;
            }
            state.expansions += 1;
        }

        let truncations_before = self.state.borrow().truncations;
        let lowered = self.lower_slot(db, method_id.module, &symbol, slot);
        let complete = self.state.borrow().truncations == truncations_before;

        {
            let mut state = self.state.borrow_mut();
            state.visited.remove(&key);
            match lowered {
                Some(ty) if complete => {
                    state.completed.insert(key, ty);
                }
                _ => {}
            }
        }

        lowered.and_then(|ty| self.accept(db, ty))
    }

    /// A resolved type replaces the permissive one only when it documents fields — that is the
    /// entire benefit, and anything else is a narrowing bought for nothing.
    ///
    /// The decision belongs to the reference, not to the slot that contains it. A slot such as
    /// `Структура: * Ключ - см. Цель` is a documented structure whatever its field holds, so a
    /// slot-level check would accept a target resolving to `Строка` and narrow the field.
    fn accept(&self, db: &dyn TypeKernelDb, ty: TypeId) -> Option<TypeId> {
        if !doc_structure::is_doc_structure(db, ty) {
            return None;
        }
        self.state.borrow_mut().accepted += 1;
        Some(ty)
    }

    fn target(
        &self,
        from: ModuleId,
        name: &QualifiedName,
    ) -> Option<(MethodId, Arc<MethodDecl>, Slot)> {
        let segments = name.segments();
        let (module, method) = match segments {
            [module, method] | [module, method, _] => (module, method),
            _ => return None,
        };
        let (method_id, symbol) = self.targets.method(from, module, method)?;
        let slot = match segments {
            [_, _] => Slot::Ret,
            [_, _, param] => Slot::Param(param_index(&symbol, param)?),
            _ => return None,
        };
        Some((method_id, symbol, slot))
    }

    fn lower_slot(
        &self,
        db: &dyn TypeKernelDb,
        module: ModuleId,
        symbol: &MethodDecl,
        slot: Slot,
    ) -> Option<TypeId> {
        let alternatives = slot_alternatives(symbol, slot)?;
        // A fresh closure per module: a reference inside the target's documentation resolves from
        // the target's module, not from wherever the walk started.
        let resolve = |name: &QualifiedName| self.resolve(db, module, name);
        lower_alternatives(db, alternatives, &SeePolicy::Resolve(&resolve), self.map_keys)
    }
}

/// The alternatives documented for one slot, in declaration order.
fn slot_alternatives(symbol: &MethodDecl, slot: Slot) -> Option<&[TypeDoc]> {
    let docs = symbol.docs.as_deref()?;
    match slot {
        Slot::Ret => Some(docs.returned_value.as_slice()),
        Slot::Param(index) => {
            let name = symbol.params.get(index as usize)?.name.as_str().fold_lower();
            docs.parameters
                .iter()
                .find(|param| param.name.fold_lower() == name)
                .map(|param| param.types.as_slice())
        }
    }
}

/// Matched without regard to case, the same way parameter documentation is matched everywhere.
fn param_index(symbol: &MethodDecl, name: &Name) -> Option<u32> {
    let needle = name.as_str().fold_lower();
    symbol
        .params
        .iter()
        .position(|param| param.name.as_str().fold_lower() == needle)
        .and_then(|index| u32::try_from(index).ok())
}

/// Builds the slot's type from its alternatives, so an arm documented beside a reference survives.
/// Collapsing the alternatives first and patching the result afterwards cannot work: a slot
/// `Неопределено, см. X.Y` has already become the single permissive type by then, and its
/// `Неопределено` arm is gone.
fn lower_alternatives(
    db: &dyn TypeKernelDb,
    docs: &[TypeDoc],
    policy: &SeePolicy<'_>,
    map_keys: bool,
) -> Option<TypeId> {
    // An alternative this parser cannot read becomes the top type, never nothing. Dropping it
    // would narrow the slot to whatever else happened to parse — the kernel discards `Unknown`
    // from a union, so only `Any` keeps the declaration as permissive as it was written. The
    // other doc-type parser makes the same choice for the same reason (`ty/doc_types.rs`).
    let exprs: Vec<DocTypeExpr> = docs
        .iter()
        .map(|doc| parse_type_expr(doc).unwrap_or(DocTypeExpr::TypeRef(hir_def::TypeRef::Any)))
        .map(|expr| if map_keys { expr } else { expr.without_map_keys() })
        .collect();
    (!exprs.is_empty()).then(|| doc_structure::field_ty(db, &exprs, policy))
}

/// Memoised per referring method. Keyed by the method that holds the reference rather than by the
/// target: a query keyed by target would have to call itself, and a pair of methods referring to
/// each other would become a salsa cycle instead of a walk that stops.
#[salsa::tracked(lru = 262144, heap_size = doc_see_signature_heap, returns(ref))]
pub fn doc_see_signature_query<'db>(
    db: &'db dyn HirDatabase,
    method: MethodIdInput<'db>,
) -> Arc<DocSeeSignature> {
    let mid = method.method_id(db);
    let _span = tracing::info_span!(
        "doc_see_signature",
        file_id = mid.module.file_id.0,
        local_id = ?mid.local_id,
    )
    .entered();

    let Some(symbol) = db.interface_method(mid) else {
        return Arc::new(DocSeeSignature::default());
    };

    let map_keys = db.map_key_types_enabled();
    let signature = crate::method_resolution::materialise_signature(db, &symbol, map_keys);
    // Nothing in this method's documentation names a target, so there is nothing to resolve. This
    // is the answer for the overwhelming majority of methods, and it costs one bit to reach.
    if signature.doc_see.is_empty() {
        return Arc::new(DocSeeSignature::default());
    }

    let targets = WorkspaceTargets { db };
    let driver = Driver { targets: &targets, state: RefCell::new(State::default()), map_keys };

    let mut resolved = DocSeeSignature::default();
    if signature.doc_see.ret {
        resolved.ret = resolve_own_slot(db, &driver, mid.module, &symbol, Slot::Ret, signature.ret);
    }
    for (index, &lowered) in signature.params.iter().enumerate() {
        if !signature.doc_see.param(index) {
            continue;
        }
        let slot = Slot::Param(index as u32);
        if let Some(ty) = resolve_own_slot(db, &driver, mid.module, &symbol, slot, lowered) {
            resolved.params.insert(index as u32, ty);
        }
    }

    Arc::new(resolved)
}

/// Lowers one slot of the referring method itself. Unlike a target's slot this one is not a node
/// of the walk — nothing refers to it — so it neither enters the visited set nor is memoised.
///
/// Two conditions guard the substitution, and neither implies the other.
///
/// **The rebuild has to agree with the signature.** Re-lowering reads only one of the two
/// doc-parsers the signature is built from, and the other reads forms this one does not:
/// `Соответствие из X` has a space in its name, becomes the top type here, and then dominates
/// everything documented beside it. Lowering the slot once more with references left permissive
/// answers whether the two pipelines agree on this slot at all. When they disagree the signature's
/// own type stands — the difference has nothing to do with references, and substituting it would
/// take away what the caller already had.
///
/// One disagreement is the same form read differently, not a lost arm: a marker in the slot's
/// description (`Структура - см. Модуль.Метод`) puts the reference where the hint parser does
/// not look, so the signature holds an untyped structure while the rebuild holds the top type.
/// Such a slot carries no keys and no nominal identity to take away — it is the placeholder the
/// reference fills; a slot holding anything else beside the placeholder is still rejected.
///
/// **A reference has to have been accepted.** Without that a slot whose references all failed to
/// resolve would still be replaced by a rebuild that carries nothing new.
fn resolve_own_slot(
    db: &dyn HirDatabase,
    driver: &Driver<'_>,
    module: ModuleId,
    symbol: &MethodDecl,
    slot: Slot,
    lowered_by_the_signature: TypeId,
) -> Option<TypeId> {
    let alternatives = slot_alternatives(symbol, slot)?;
    let rebuilt = lower_alternatives(db, alternatives, &SeePolicy::Permissive, driver.map_keys)?;
    if rebuilt != lowered_by_the_signature
        && !(rebuilt == db.any() && is_only_placeholder_structures(db, lowered_by_the_signature))
    {
        return None;
    }

    let accepted_before = driver.state.borrow().accepted;
    let lowered = driver.lower_slot(db, module, symbol, slot)?;
    (driver.state.borrow().accepted > accepted_before).then_some(lowered)
}

/// Whether the slot's declared type is nothing but untyped structures: one, or a union of them.
/// Such a slot has no keys and no nominal identity — it is the placeholder a documented
/// structure fills. Both halves are checked: a structure with keys carries the identity a
/// reference must not overwrite.
fn is_only_placeholder_structures(db: &dyn TypeKernelDb, ty: TypeId) -> bool {
    use bsl_types::kind::TypeKind;
    match db.lookup_type(ty) {
        TypeKind::Structure(facet) => facet.fields.is_none() && facet.keys.is_none(),
        TypeKind::Union(members) => {
            !members.is_empty()
                && members.iter().all(|member| is_only_placeholder_structures(db, *member))
        }
        _ => false,
    }
}
