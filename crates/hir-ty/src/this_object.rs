use bsl_metadata::{MdoType, ModuleType};
use bsl_types::builders::{Builders, ConfigCtx};
use bsl_types::intern::TypeKernelDb;
use bsl_types::kind::{ConfigId, TypeId, TypeKind};
use hir_def::resolver::Resolver;
use hir_def::ty::MetadataKind;
use hir_def::{DefDatabase, Name};

pub(crate) struct FixedConfigCtx(pub(crate) ConfigId);

impl ConfigCtx for FixedConfigCtx {
    fn resolve_config_id(&self, _kind: MetadataKind, _name: &bsl_metadata::Name) -> ConfigId {
        self.0.clone()
    }

    fn resolve_manager_config_id(&self, _mdo: MdoType, _name: &bsl_metadata::Name) -> ConfigId {
        self.0.clone()
    }
}

pub fn coerce_to_metadata_ref_id(db: &dyn TypeKernelDb, receiver: TypeId) -> Option<TypeId> {
    match db.lookup_type(receiver) {
        TypeKind::ThisObject { config_id, owner } => {
            let kind = MetadataKind::object_kind_for(owner.mdo_type)?;
            let cfg = FixedConfigCtx(config_id.clone());
            Some(db.metadata_ref(kind, owner.name.clone(), &cfg))
        }
        TypeKind::ThisManager { config_id, owner } => {
            owner.mdo_type.manager_type_prefix()?;
            let cfg = FixedConfigCtx(config_id.clone());
            Some(db.object_manager(owner.mdo_type, owner.name.clone(), &cfg))
        }
        _ => None,
    }
}

pub fn resolve_this_object_owner(
    db: &dyn DefDatabase,
    resolver: &Resolver,
) -> Option<(MdoType, Name)> {
    let module_id = resolver.module_id()?;
    let metadata = db.module_metadata(module_id);
    let mdo = metadata.mdo.as_ref()?;

    if metadata.module_type != ModuleType::ObjectModule {
        return None;
    }

    MetadataKind::object_kind_for(mdo.mdo_type)?;

    Some((mdo.mdo_type, Name::new(&mdo.name)))
}

/// The constant whose value-manager module is being analyzed.
pub fn resolve_this_value_manager_owner(db: &dyn DefDatabase, resolver: &Resolver) -> Option<Name> {
    let module_id = resolver.module_id()?;
    let metadata = db.module_metadata(module_id);
    if metadata.module_type != ModuleType::ValueManagerModule {
        return None;
    }
    let mdo = metadata.mdo.as_ref()?;
    (mdo.mdo_type == MdoType::Constant).then(|| Name::new(&mdo.name))
}

pub fn resolve_this_manager_owner(
    db: &dyn DefDatabase,
    resolver: &Resolver,
) -> Option<(MdoType, Name)> {
    let module_id = resolver.module_id()?;
    let metadata = db.module_metadata(module_id);

    if metadata.module_type != ModuleType::ManagerModule {
        return None;
    }

    let (mdo_type, name) = match (metadata.mdo.as_ref(), metadata.register.as_ref()) {
        (Some(mdo), _) => (mdo.mdo_type, Name::new(&mdo.name)),
        (None, Some(reg)) => (reg.mdo_type(), Name::new(reg.name())),
        (None, None) => return None,
    };

    mdo_type.manager_type_prefix()?;

    Some((mdo_type, name))
}

pub fn resolve_this_record_set_owner(
    db: &dyn DefDatabase,
    resolver: &Resolver,
) -> Option<(MdoType, Name)> {
    let module_id = resolver.module_id()?;
    let metadata = db.module_metadata(module_id);

    if metadata.module_type != ModuleType::RecordSetModule {
        return None;
    }

    let (mdo_type, name) = match (metadata.mdo.as_ref(), metadata.register.as_ref()) {
        (Some(mdo), _) => (mdo.mdo_type, Name::new(&mdo.name)),
        (None, Some(reg)) => (reg.mdo_type(), Name::new(reg.name())),
        (None, None) => return None,
    };

    MetadataKind::record_set_kind_for(mdo_type)?;

    Some((mdo_type, name))
}

pub fn form_module_metadata_missing(db: &dyn DefDatabase, resolver: &Resolver) -> bool {
    resolver.module_id().is_some_and(|module_id| {
        let metadata = db.module_metadata(module_id);
        metadata.module_type == ModuleType::FormModule && metadata.form.is_none()
    })
}

pub fn is_managed_form_module(db: &dyn DefDatabase, resolver: &Resolver) -> bool {
    let Some(module_id) = resolver.module_id() else { return false };
    let metadata = db.module_metadata(module_id);

    metadata.module_type == ModuleType::FormModule
        && metadata.form.as_ref().is_some_and(|form| form.is_managed())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bsl_metadata::MdoType;
    use bsl_types::facet::MdoRefFacet;
    use bsl_types::kind::ConfigId;
    use bsl_types::testing::{InMemoryDb, RootConfigCtx};

    fn this_object(db: &InMemoryDb, mdo_type: MdoType, name: &str) -> TypeId {
        db.mk_this_object(ConfigId::Root, MdoRefFacet::new(mdo_type, name.to_string()))
    }

    fn this_manager(db: &InMemoryDb, mdo_type: MdoType, name: &str) -> TypeId {
        db.mk_this_manager(ConfigId::Root, MdoRefFacet::new(mdo_type, name.to_string()))
    }

    #[test]
    fn coerces_catalog_this_object_to_catalog_object() {
        let db = InMemoryDb::new();
        let coerced =
            coerce_to_metadata_ref_id(&db, this_object(&db, MdoType::Catalog, "Номенклатура"))
                .expect("catalog coerces");
        assert_eq!(
            coerced,
            db.metadata_ref(
                MetadataKind::CatalogObject,
                "Номенклатура".to_string(),
                &RootConfigCtx
            )
        );
    }

    #[test]
    fn coerces_document_this_object_to_document_object() {
        let db = InMemoryDb::new();
        let coerced = coerce_to_metadata_ref_id(&db, this_object(&db, MdoType::Document, "ПКО"))
            .expect("document coerces");
        assert_eq!(
            coerced,
            db.metadata_ref(MetadataKind::DocumentObject, "ПКО".to_string(), &RootConfigCtx)
        );
    }

    #[test]
    fn coerces_exchange_plan_this_object_to_exchange_plan_object() {
        let db = InMemoryDb::new();
        let coerced =
            coerce_to_metadata_ref_id(&db, this_object(&db, MdoType::ExchangePlan, "Обмен"))
                .expect("exchange plan");
        assert_eq!(
            coerced,
            db.metadata_ref(MetadataKind::ExchangePlanObject, "Обмен".to_string(), &RootConfigCtx)
        );
    }

    #[test]
    fn coerces_chart_of_accounts_this_object_to_chart_of_accounts_object() {
        let db = InMemoryDb::new();
        let coerced =
            coerce_to_metadata_ref_id(&db, this_object(&db, MdoType::ChartOfAccounts, "Основной"))
                .expect("chart of accounts");
        assert_eq!(
            coerced,
            db.metadata_ref(
                MetadataKind::ChartOfAccountsObject,
                "Основной".to_string(),
                &RootConfigCtx
            )
        );
    }

    #[test]
    fn no_coercion_for_non_object_kinds() {
        let db = InMemoryDb::new();
        for mdo in [
            MdoType::InformationRegister,
            MdoType::AccumulationRegister,
            MdoType::AccountingRegister,
            MdoType::CalculationRegister,
            MdoType::Enum,
        ] {
            assert!(
                coerce_to_metadata_ref_id(&db, this_object(&db, mdo, "X")).is_none(),
                "expected no coercion for {mdo:?}"
            );
        }
    }

    #[test]
    fn coerces_business_process_and_task_and_data_processor_and_report_this_object() {
        let db = InMemoryDb::new();
        for (mdo, expected_kind) in [
            (MdoType::BusinessProcess, MetadataKind::BusinessProcessObject),
            (MdoType::Task, MetadataKind::TaskObject),
            (MdoType::DataProcessor, MetadataKind::DataProcessorObject),
            (MdoType::Report, MetadataKind::ReportObject),
        ] {
            let coerced = coerce_to_metadata_ref_id(&db, this_object(&db, mdo, "X"))
                .unwrap_or_else(|| panic!("must coerce {mdo:?}"));
            assert_eq!(
                coerced,
                db.metadata_ref(expected_kind, "X".to_string(), &RootConfigCtx),
                "{mdo:?} must coerce to {expected_kind:?}"
            );
        }
    }

    #[test]
    fn no_coercion_for_non_this_object_receivers() {
        let db = InMemoryDb::new();
        assert!(coerce_to_metadata_ref_id(&db, db.number(None, None)).is_none());
        assert!(coerce_to_metadata_ref_id(&db, db.unknown()).is_none());
        assert!(coerce_to_metadata_ref_id(
            &db,
            db.metadata_ref(MetadataKind::CatalogRef, "X".to_string(), &RootConfigCtx)
        )
        .is_none());
        assert!(coerce_to_metadata_ref_id(
            &db,
            db.object_manager(MdoType::Catalog, "X".to_string(), &RootConfigCtx)
        )
        .is_none());
    }

    #[test]
    fn coerces_catalog_this_manager_to_object_manager() {
        let db = InMemoryDb::new();
        let coerced =
            coerce_to_metadata_ref_id(&db, this_manager(&db, MdoType::Catalog, "Номенклатура"))
                .expect("catalog manager coerces");
        assert_eq!(
            coerced,
            db.object_manager(MdoType::Catalog, "Номенклатура".to_string(), &RootConfigCtx)
        );
    }

    #[test]
    fn coerces_document_this_manager_to_object_manager() {
        let db = InMemoryDb::new();
        let coerced = coerce_to_metadata_ref_id(&db, this_manager(&db, MdoType::Document, "ПКО"))
            .expect("document manager coerces");
        assert_eq!(
            coerced,
            db.object_manager(MdoType::Document, "ПКО".to_string(), &RootConfigCtx)
        );
    }

    #[test]
    fn coerces_register_this_manager_to_object_manager() {
        let db = InMemoryDb::new();
        let coerced =
            coerce_to_metadata_ref_id(&db, this_manager(&db, MdoType::InformationRegister, "Курс"))
                .expect("register manager coerces");
        assert_eq!(
            coerced,
            db.object_manager(MdoType::InformationRegister, "Курс".to_string(), &RootConfigCtx)
        );
        assert!(
            coerce_to_metadata_ref_id(&db, this_object(&db, MdoType::InformationRegister, "Курс"))
                .is_none(),
            "register kind has no `*Object` companion"
        );
    }

    #[test]
    fn coerces_business_process_and_task_and_data_processor_and_report_this_manager() {
        let db = InMemoryDb::new();
        for mdo in
            [MdoType::BusinessProcess, MdoType::Task, MdoType::DataProcessor, MdoType::Report]
        {
            let coerced = coerce_to_metadata_ref_id(&db, this_manager(&db, mdo, "X"))
                .unwrap_or_else(|| panic!("must coerce manager for {mdo:?}"));
            assert_eq!(
                coerced,
                db.object_manager(mdo, "X".to_string(), &RootConfigCtx),
                "{mdo:?} ThisManager must coerce to its own ObjectManager"
            );
        }
    }

    #[test]
    fn no_coercion_for_this_manager_on_kinds_without_manager_surface() {
        let db = InMemoryDb::new();
        assert!(
            coerce_to_metadata_ref_id(&db, this_manager(&db, MdoType::CommonModule, "X")).is_none(),
            "kinds without a manager surface must not coerce"
        );
    }
}
