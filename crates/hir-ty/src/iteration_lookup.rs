use std::sync::Arc;
use stdx::case::CaseExt;

use bsl_metadata::MdoType;
use bsl_platform::PlatformData;
use bsl_types::builders::Builders;
use bsl_types::facet::TableSource;
use bsl_types::intern::TypeKernelDb;
use bsl_types::kind::{MetadataKind, Projection, TypeId, TypeKind};
use bsl_types::testing::RootConfigCtx;
use hir_def::Name;
use smol_str::SmolStr;

use crate::lower::type_string::lower_platform_type_name_typeid;
use crate::platform_manager_lookup::{
    map_generic_metadata_return_type_typeid, metadata_kind_to_prefix_and_mdo,
};

pub(crate) fn resolve_iter_element_ty(db: &dyn TypeKernelDb, collection: TypeId) -> Option<TypeId> {
    resolve_iter_element_ty_inner(db, collection)
}

enum IterShape {
    Row { projection: Option<Arc<Projection>>, source: TableSource },
    Element(TypeId),
    Templates { templates: Vec<SmolStr>, context: Option<(MdoType, Name)> },
    Union(Vec<TypeId>),
    Unsupported,
}

fn resolve_iter_element_ty_inner(db: &dyn TypeKernelDb, collection: TypeId) -> Option<TypeId> {
    let from_name = |name: &str| match lookup_by_type_name(name) {
        Some(templates) => IterShape::Templates { templates, context: None },
        None => IterShape::Unsupported,
    };

    let shape = match db.lookup_type(collection) {
        TypeKind::ValueTable(f) if f.projection.is_some() => {
            IterShape::Row { projection: f.projection.clone(), source: f.source }
        }
        TypeKind::PlatformObject(f) => from_name(f.name.as_str()),
        // A tabular section iterates over its own rows: the same row facet that
        // `Добавить()` returns, so both keep the section's column schema.
        TypeKind::MetadataRef(f) => match f.kind {
            MetadataKind::TabularSection { parent } => IterShape::Element(db.metadata_ref(
                MetadataKind::TabularSectionRow { parent },
                f.name.as_str().to_string(),
                &RootConfigCtx,
            )),
            kind => match lookup_by_metadata_kind(kind) {
                Some(templates) => {
                    let context = metadata_kind_to_prefix_and_mdo(kind)
                        .map(|(_, mdo)| (mdo, Name::new(f.name.as_str())));
                    IterShape::Templates { templates, context }
                }
                None => IterShape::Unsupported,
            },
        },
        TypeKind::Array(f) => match f.element {
            Some(elem) => IterShape::Element(elem),
            None => from_name("Массив"),
        },
        TypeKind::Map(_) => from_name("Соответствие"),
        TypeKind::ValueTable(_) => from_name("ТаблицаЗначений"),
        TypeKind::ValueTableRow(_) => from_name("СтрокаТаблицыЗначений"),
        TypeKind::ValueList(_) => from_name("СписокЗначений"),
        TypeKind::Structure(_) => from_name("Структура"),
        TypeKind::Union(arms) => IterShape::Union(arms.to_vec()),
        // The element stays the platform's union of every description kind rather than the
        // collection's own kind: a loop variable is the receiver most often re-bound from an
        // unknown collection (`Для Каждого М Из Метаданные[Вид]`), which leaves it holding
        // the precise type of an earlier loop, and every member check downstream would judge
        // the wrong kind.
        TypeKind::MetadataObjectCollection(_) => from_name("КоллекцияОбъектовМетаданных"),
        _ => IterShape::Unsupported,
    };

    match shape {
        IterShape::Row { projection, source } => Some(db.value_table_row(projection, source)),
        IterShape::Element(elem) => Some(elem),
        IterShape::Templates { templates, context } => resolve_templates(db, &templates, context),
        IterShape::Union(arms) => resolve_union(db, &arms),
        IterShape::Unsupported => None,
    }
}

fn lookup_by_type_name(name: &str) -> Option<Vec<SmolStr>> {
    let t = PlatformData::instance().get_type(name)?;
    if t.iter_element_types.is_empty() {
        None
    } else {
        Some(t.iter_element_types.clone())
    }
}

fn lookup_by_metadata_kind(kind: MetadataKind) -> Option<Vec<SmolStr>> {
    let (prefix, _) = metadata_kind_to_prefix_and_mdo(kind)?;
    let needle = format!("{}.", prefix.fold_lower());
    PlatformData::instance()
        .all_types()
        .iter()
        .find(|t| t.english_name.fold_lower().starts_with(&needle))
        .filter(|t| !t.iter_element_types.is_empty())
        .map(|t| t.iter_element_types.clone())
}

fn resolve_union(db: &dyn TypeKernelDb, arms: &[TypeId]) -> Option<TypeId> {
    let alive: Vec<TypeId> = arms
        .iter()
        .copied()
        .filter(|t| !matches!(db.lookup_type(*t), TypeKind::Undefined | TypeKind::Null))
        .collect();
    if alive.is_empty() {
        return None;
    }
    let resolved: Option<Vec<TypeId>> =
        alive.iter().map(|t| resolve_iter_element_ty_inner(db, *t)).collect();
    let resolved = resolved?;
    // A wildcard element (an untyped «Массив» iterates per the platform docs
    // as Произвольный) must not absorb a concrete sibling arm: the union
    // receiver is an over-approximation and the concrete arm is what the loop
    // variable is actually shaped like.
    let concrete: Vec<TypeId> = resolved
        .iter()
        .copied()
        .filter(|t| !matches!(db.lookup_type(*t), TypeKind::Unknown | TypeKind::Any))
        .collect();
    let picked = if concrete.is_empty() { resolved } else { concrete };
    if picked.len() == 1 {
        Some(picked[0])
    } else {
        Some(db.union(picked))
    }
}

fn resolve_templates(
    db: &dyn TypeKernelDb,
    templates: &[SmolStr],
    context: Option<(MdoType, Name)>,
) -> Option<TypeId> {
    let ctx = context.as_ref().map(|(m, n)| (*m, n));
    let resolved: Vec<TypeId> =
        templates.iter().filter_map(|tpl| resolve_one_template(db, tpl, ctx)).collect();
    match resolved.len() {
        0 => None,
        1 => Some(resolved[0]),
        _ => Some(db.union(resolved)),
    }
}

fn resolve_one_template(
    db: &dyn TypeKernelDb,
    template: &str,
    context: Option<(MdoType, &Name)>,
) -> Option<TypeId> {
    if let Some(dot_pos) = template.find('.') {
        let head = &template[..dot_pos];
        let (mdo, mdo_name) = context?;
        return map_generic_metadata_return_type_typeid(db, head, mdo, mdo_name);
    }

    Some(lower_platform_type_name_typeid(db, template))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bsl_types::testing::{InMemoryDb, RootConfigCtx};

    fn resolve(db: &InMemoryDb, collection: TypeId) -> Option<TypeId> {
        super::resolve_iter_element_ty(db, collection)
    }

    fn metadata_ref_id(db: &dyn TypeKernelDb, kind: MetadataKind, name: &str) -> TypeId {
        db.metadata_ref(kind, name.to_string(), &RootConfigCtx)
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn array_yields_any_per_platform_arbitrary() {
        let db = InMemoryDb::new();
        let elem = resolve(&db, db.array(None));
        assert_eq!(elem, Some(db.any()));
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn map_yields_kluch_i_znachenie() {
        let db = InMemoryDb::new();
        let elem = resolve(&db, db.map(None, None));
        assert_eq!(elem, Some(db.platform_object("КлючИЗначение".to_string())));
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn value_table_yields_row() {
        let db = InMemoryDb::new();
        let elem = resolve(&db, db.value_table(None, TableSource::Unknown));
        assert_eq!(elem, Some(db.platform_object("СтрокаТаблицыЗначений".to_string())));
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn information_register_record_set_yields_record_with_mdo_name() {
        let db = InMemoryDb::new();
        let receiver = metadata_ref_id(
            &db,
            MetadataKind::InformationRegisterRecordSet,
            "БУС_ЗаполнениеКаталога",
        );
        let elem = resolve(&db, receiver);
        assert_eq!(
            elem,
            Some(metadata_ref_id(
                &db,
                MetadataKind::InformationRegisterRecord,
                "БУС_ЗаполнениеКаталога"
            ))
        );
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn accumulation_register_record_set_yields_record_with_mdo_name() {
        let db = InMemoryDb::new();
        let receiver =
            metadata_ref_id(&db, MetadataKind::AccumulationRegisterRecordSet, "ОстаткиТоваров");
        let elem = resolve(&db, receiver);
        assert_eq!(
            elem,
            Some(metadata_ref_id(&db, MetadataKind::AccumulationRegisterRecord, "ОстаткиТоваров"))
        );
    }

    #[test]
    fn typed_array_yields_inner_element_directly() {
        let db = InMemoryDb::new();
        let elem = resolve(&db, db.array(Some(db.string(None, false))));
        assert_eq!(elem, Some(db.string(None, false)));
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn union_of_array_and_typed_array_iterates_to_concrete_arm() {
        let db = InMemoryDb::new();
        let union = db.union(vec![db.array(None), db.array(Some(db.string(None, false)))]);
        let elem = resolve(&db, union);
        assert_eq!(elem, Some(db.string(None, false)));
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn union_receiver_with_unknown_arm_absorbs_to_concrete_iterable() {
        let db = InMemoryDb::new();
        let union = db.union(vec![db.unknown(), db.array(None)]);
        assert_eq!(resolve(&db, union), Some(db.any()));

        let union = db.union(vec![db.unknown(), db.array(Some(db.string(None, false)))]);
        assert_eq!(resolve(&db, union), Some(db.string(None, false)));
    }

    #[test]
    fn typed_array_with_metadata_ref_element_passes_through_unchanged() {
        let db = InMemoryDb::new();
        let row = metadata_ref_id(
            &db,
            MetadataKind::TabularSectionRow { parent: MdoType::Document },
            "ПКО.Товары",
        );
        let collection = db.array(Some(row));
        let elem = resolve(&db, collection);
        assert_eq!(elem, Some(row));
    }

    #[test]
    fn non_iterable_string_returns_none() {
        let db = InMemoryDb::new();
        let elem = resolve(&db, db.string(None, false));
        assert_eq!(elem, None);
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn union_with_undefined_filters_dead_arm() {
        let db = InMemoryDb::new();
        let union = db.union(vec![db.array(None), db.undefined()]);
        let elem = resolve(&db, union);
        assert_eq!(elem, Some(db.any()));
    }

    #[test]
    fn union_with_non_iterable_arm_returns_none() {
        let db = InMemoryDb::new();
        let union = db.union(vec![db.array(None), db.string(None, false)]);
        let elem = resolve(&db, union);
        assert_eq!(elem, None);
    }
}
