use bsl_types::builders::Builders;
use bsl_types::kind::TypeId;
use bsl_types::testing::RootConfigCtx;
use hir_def::resolver::Resolver;
use hir_def::Name;

use crate::db::HirDatabase;

/// A member of the module's own object: of `ЭтотОбъект` in an object module, and of
/// the value manager in a constant's value-manager module. There `ДополнительныеСвойства`,
/// `Значение`, `ОбменДанными` and a separator the constant belongs to compile as bare names
/// on 8.3.17 and 8.3.27, while a name the value manager lacks is "Переменная не определена".
pub(crate) fn resolve_this_object_member(
    db: &dyn HirDatabase,
    resolver: &Resolver,
    name: &Name,
) -> Option<TypeId> {
    let (kind, mdo_name) = match crate::this_object::resolve_this_object_owner(db, resolver) {
        Some((mdo_type, mdo_name)) => {
            (hir_def::ty::MetadataKind::object_kind_for(mdo_type)?, mdo_name)
        }
        None => (
            hir_def::ty::MetadataKind::ConstantValueManager,
            crate::this_object::resolve_this_value_manager_owner(db, resolver)?,
        ),
    };
    let module_id = resolver.module_id()?;
    let obj_resolver = crate::object_resolver::DbObjectResolver::new(db, module_id.file_id);
    let receiver_id = db.metadata_ref(kind, mdo_name.as_str().to_string(), &RootConfigCtx);
    crate::field_lookup::lookup_field(db, &obj_resolver, receiver_id, name).map(|info| info.ty)
}

pub(crate) fn resolve_this_record_set_member(
    db: &dyn HirDatabase,
    resolver: &Resolver,
    name: &Name,
) -> Option<TypeId> {
    let (mdo_type, mdo_name) = crate::this_object::resolve_this_record_set_owner(db, resolver)?;
    let kind = hir_def::ty::MetadataKind::record_set_kind_for(mdo_type)?;
    let module_id = resolver.module_id()?;
    let obj_resolver = crate::object_resolver::DbObjectResolver::new(db, module_id.file_id);
    let receiver_id = db.metadata_ref(kind, mdo_name.as_str().to_string(), &RootConfigCtx);
    crate::field_lookup::lookup_field(db, &obj_resolver, receiver_id, name).map(|info| info.ty)
}

/// `ЭтоГруппа`, `Родитель` and `Владелец` as bare names of a catalog's own object module,
/// whatever the catalog's hierarchy and owner flags say: the platform compiles them in a flat
/// catalog without owners (measured on vendor code that runs on every write), while the same
/// names read through a reference stay absent. So this is the implicit receiver's surface only,
/// and it never feeds the typed field lookup of a reference.
pub(crate) fn resolve_this_catalog_conditional_member(
    db: &dyn HirDatabase,
    resolver: &Resolver,
    name: &Name,
) -> Option<TypeId> {
    use bsl_platform::{standard_attributes_for, MdoTemplateKind, ObjectView, PresenceCondition};
    use stdx::case::CaseExt;

    let (mdo_type, _) = crate::this_object::resolve_this_object_owner(db, resolver)?;
    if mdo_type != bsl_metadata::MdoType::Catalog {
        return None;
    }
    let wanted = name.as_str().fold_lower();
    standard_attributes_for(MdoTemplateKind::Catalog, ObjectView::Object)
        .iter()
        .filter(|spec| {
            matches!(spec.condition, PresenceCondition::Hierarchical | PresenceCondition::HasOwners)
        })
        .any(|spec| {
            spec.kind.russian_name().fold_lower() == wanted
                || spec.kind.english_name().fold_lower() == wanted
        })
        .then(|| db.unknown())
}
