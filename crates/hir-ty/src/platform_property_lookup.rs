use bsl_platform::{PlatformData, PlatformProperty};
use bsl_types::builders::Builders;
use bsl_types::intern::TypeKernelDb;
use bsl_types::kind::{TypeId, TypeKind};
use hir_def::Name;

use crate::lower::type_string::lower_platform_type_name_typeid;
use crate::method_lookup::platform_type_key_id;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlatformPropertyResolution {
    pub return_ty: TypeId,
    pub is_readonly: bool,
    /// Execution environments the property is available in.
    pub env: hir_def::execution_env::EnvFlags,
}

pub fn lookup_platform_property(
    db: &dyn TypeKernelDb,
    receiver: TypeId,
    prop_name: &Name,
) -> Option<PlatformPropertyResolution> {
    let form_kind = match db.lookup_type(receiver) {
        TypeKind::FormControl { kind, .. } => Some(*kind),
        _ => None,
    };
    if let Some(kind) = form_kind {
        return hir_def::ty::form_control_chain_first_hit(kind, |type_name| {
            lookup_platform_property_by_type(db, type_name, prop_name)
        });
    }
    let type_key = platform_type_key_id(db, receiver)?;
    let mut res = lookup_platform_property_by_type(db, &type_key, prop_name)?;
    // Availability resolved through a homonymous type name may belong to the
    // wrong namesake — don't judge it.
    if PlatformData::instance().is_ambiguous_type_name(&type_key) {
        res.env = hir_def::execution_env::EnvFlags::ALL;
    }
    Some(res)
}

pub(crate) fn lookup_platform_property_by_type(
    db: &dyn TypeKernelDb,
    type_name: &str,
    prop_name: &Name,
) -> Option<PlatformPropertyResolution> {
    let data = PlatformData::instance();
    let prop = data.get_property(type_name, prop_name.as_str())?;
    Some(to_resolution(db, prop))
}

pub(crate) fn to_resolution(
    db: &dyn TypeKernelDb,
    prop: &PlatformProperty,
) -> PlatformPropertyResolution {
    PlatformPropertyResolution {
        return_ty: map_property_type_list(db, &prop.property_types),
        is_readonly: prop.is_readonly,
        env: hir_def::execution_env::EnvFlags::from_platform_context(prop.context.as_ref()),
    }
}

fn map_property_type_list(db: &dyn TypeKernelDb, types: &[smol_str::SmolStr]) -> TypeId {
    match types.len() {
        0 => db.unknown(),
        1 => lower_platform_type_name_typeid(db, types[0].as_str()),
        _ => {
            let mapped: Vec<TypeId> =
                types.iter().map(|s| lower_platform_type_name_typeid(db, s.as_str())).collect();
            db.union(mapped)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bsl_metadata::MdoType;
    use bsl_types::facet::DateComponent;
    use bsl_types::kind::MetadataKind;
    use bsl_types::testing::{InMemoryDb, RootConfigCtx};

    struct ResolvedForTest {
        return_ty: TypeId,
        is_readonly: bool,
    }

    fn lookup_platform_property(
        db: &InMemoryDb,
        receiver: TypeId,
        prop_name: &Name,
    ) -> Option<ResolvedForTest> {
        super::lookup_platform_property(db, receiver, prop_name)
            .map(|res| ResolvedForTest { return_ty: res.return_ty, is_readonly: res.is_readonly })
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn query_text_resolves_to_string_writable() {
        let db = InMemoryDb::new();
        let receiver = db.platform_object("Запрос".to_string());
        let res = lookup_platform_property(&db, receiver, &Name::new("Текст"))
            .expect("Query.Текст must resolve through platform property data");
        assert_eq!(res.return_ty, db.string(None, false));
        assert!(!res.is_readonly);
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn query_parameters_resolves_to_structure_readonly() {
        let db = InMemoryDb::new();
        let receiver = db.platform_object("Запрос".to_string());
        let res = lookup_platform_property(&db, receiver, &Name::new("Параметры"))
            .expect("Query.Параметры must resolve");
        assert_eq!(res.return_ty, db.structure(None));
        assert!(res.is_readonly);
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn query_temp_tables_manager_resolves_to_union() {
        let db = InMemoryDb::new();
        let receiver = db.platform_object("Запрос".to_string());
        let res = lookup_platform_property(&db, receiver, &Name::new("МенеджерВременныхТаблиц"))
            .expect("Query.МенеджерВременныхТаблиц must resolve");
        match db.lookup_type(res.return_ty) {
            TypeKind::Union(members) => {
                assert_eq!(members.len(), 2);
            }
            other => panic!("Expected TypeKind::Union for TempTablesManager, got {other:?}"),
        }
        assert!(!res.is_readonly);
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn bilingual_english_property_name_resolves() {
        let db = InMemoryDb::new();
        let receiver = db.platform_object("Query".to_string());
        let res = lookup_platform_property(&db, receiver, &Name::new("Parameters"))
            .expect("Query.Parameters must resolve via bilingual index");
        assert_eq!(res.return_ty, db.structure(None));
        assert!(res.is_readonly);
    }

    #[test]
    fn unknown_property_returns_none() {
        let db = InMemoryDb::new();
        let receiver = db.platform_object("Запрос".to_string());
        assert!(
            lookup_platform_property(&db, receiver, &Name::new("ЗаведомоНесуществующее")).is_none()
        );
    }

    #[test]
    fn metadata_ref_and_manager_receivers_return_none() {
        let db = InMemoryDb::new();
        let mdo = db.metadata_ref(
            MetadataKind::CatalogObject,
            "Номенклатура".to_string(),
            &RootConfigCtx,
        );
        assert!(lookup_platform_property(&db, mdo, &Name::new("Код")).is_none());

        let mgr = db.object_manager(MdoType::Catalog, "Валюты".to_string(), &RootConfigCtx);
        assert!(lookup_platform_property(&db, mgr, &Name::new("Любой")).is_none());
    }

    #[test]
    fn primitive_receivers_return_none() {
        let db = InMemoryDb::new();
        assert!(lookup_platform_property(&db, db.number(None, None), &Name::new("Любая")).is_none());
        assert!(
            lookup_platform_property(&db, db.string(None, false), &Name::new("Любая")).is_none()
        );
        assert!(lookup_platform_property(&db, db.boolean(), &Name::new("Любая")).is_none());
        assert!(lookup_platform_property(
            &db,
            db.date(DateComponent::DateTime),
            &Name::new("Любая")
        )
        .is_none());
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn form_control_pages_resolves_extension_only_property() {
        use bsl_metadata::FormElementKind;
        let db = InMemoryDb::new();
        let receiver = db.mk_form_control(FormElementKind::Pages, None);
        let res = lookup_platform_property(&db, receiver, &Name::new("ТекущаяСтраница"))
            .expect("<Pages>.ТекущаяСтраница must resolve through extension chain");
        assert!(!res.is_readonly);
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn form_control_pages_falls_through_to_base_for_shared_property() {
        use bsl_metadata::FormElementKind;
        let db = InMemoryDb::new();
        let receiver = db.mk_form_control(FormElementKind::Pages, None);
        let res = lookup_platform_property(&db, receiver, &Name::new("Видимость"))
            .expect("<Pages>.Видимость must fall through to ГруппаФормы base");
        assert_eq!(res.return_ty, db.boolean());
    }

    #[test]
    fn form_control_usual_group_does_not_see_pages_extension() {
        use bsl_metadata::FormElementKind;
        let db = InMemoryDb::new();
        let receiver = db.mk_form_control(FormElementKind::UsualGroup, None);
        assert!(
            lookup_platform_property(&db, receiver, &Name::new("ТекущаяСтраница")).is_none(),
            "UsualGroup chain must not borrow Pages-extension properties"
        );
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn form_control_input_field_still_resolves_base_only() {
        use bsl_metadata::FormElementKind;
        let db = InMemoryDb::new();
        let receiver = db.mk_form_control(FormElementKind::Field, None);
        let res = lookup_platform_property(&db, receiver, &Name::new("Видимость"))
            .expect("<InputField>.Видимость must resolve via base ПолеФормы");
        assert_eq!(res.return_ty, db.boolean());
    }

    #[test]
    fn form_control_other_returns_none_with_empty_chain() {
        use bsl_metadata::FormElementKind;
        let db = InMemoryDb::new();
        let receiver = db.mk_form_control(FormElementKind::Other, None);
        assert!(lookup_platform_property(&db, receiver, &Name::new("Видимость")).is_none());
    }
}
