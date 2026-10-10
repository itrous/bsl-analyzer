use std::sync::Arc;

use bsl_metadata::{MdoType, MetadataObject, ModuleType};
use bsl_types::builders::Builders;
use bsl_types::intern::TypeKernelDb;
use bsl_types::kind::{ConfigId, MetadataKind, TypeId, TypeKind};
use bsl_types::testing::RootConfigCtx;
use hir_def::Name;

use crate::object_resolver::ObjectResolver;
use crate::this_object::FixedConfigCtx;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagerMemberInfo {
    pub ty: TypeId,
}

pub fn lookup_manager_field(
    db: &dyn TypeKernelDb,
    resolver: &dyn ObjectResolver,
    receiver: TypeId,
    member: &Name,
) -> Option<ManagerMemberInfo> {
    lookup_manager_field_inner(db, resolver, receiver, member)
}

fn lookup_manager_field_inner(
    db: &dyn TypeKernelDb,
    resolver: &dyn ObjectResolver,
    receiver: TypeId,
    member: &Name,
) -> Option<ManagerMemberInfo> {
    enum Shape {
        Collection(MdoType),
        Manager { mdo: MdoType, name: String, config_id: ConfigId },
        Other,
    }
    let shape = match db.lookup_type(receiver) {
        TypeKind::ManagerCollection(kind) => Shape::Collection(*kind),
        TypeKind::ObjectManager(facet) => Shape::Manager {
            mdo: facet.mdo,
            name: facet.name.clone(),
            config_id: facet.config_id.clone(),
        },
        _ => Shape::Other,
    };
    match shape {
        Shape::Collection(kind) => promote_collection_member(db, resolver, kind, member),
        Shape::Manager { mdo, name, config_id } => {
            lookup_predefined(db, resolver, mdo, &name, member, &config_id).or_else(|| {
                resolver
                    .has_effective_module_variable(
                        ModuleType::ManagerModule,
                        mdo,
                        &name,
                        member.as_str(),
                    )
                    .then(|| ManagerMemberInfo { ty: db.unknown() })
            })
        }
        Shape::Other => None,
    }
}

fn promote_collection_member(
    db: &dyn TypeKernelDb,
    resolver: &dyn ObjectResolver,
    kind: MdoType,
    mdo_name: &Name,
) -> Option<ManagerMemberInfo> {
    collection_member_exists(resolver, kind, mdo_name.as_str()).then(|| ManagerMemberInfo {
        ty: db.object_manager(kind, mdo_name.as_str().to_string(), &RootConfigCtx),
    })
}

/// Whether the configuration holds an object of `kind` named `name` — the one
/// membership test shared by a manager collection (`Справочники.X`) and the
/// metadata collection of the same kind (`Метаданные.Справочники.X`), so the two
/// cannot disagree about what exists.
pub(crate) fn collection_member_exists(
    resolver: &dyn ObjectResolver,
    kind: MdoType,
    name: &str,
) -> bool {
    resolver.resolve_metadata_object(kind, name).is_some()
        || resolver.resolve_register(kind, name).is_some()
        || resolver.manager_module_without_config(kind, name)
}

pub(crate) fn lookup_predefined(
    db: &dyn TypeKernelDb,
    resolver: &dyn ObjectResolver,
    kind: MdoType,
    owner_name: &str,
    member_name: &Name,
    config_id: &ConfigId,
) -> Option<ManagerMemberInfo> {
    let ref_kind = predefined_ref_kind_for(kind)?;
    let mdo = find_mdo(resolver, kind, owner_name)?;
    let hit = match kind {
        MdoType::Enum => mdo.find_enum_value(member_name.as_str()).is_some(),
        MdoType::Catalog
        | MdoType::ChartOfAccounts
        | MdoType::ChartOfCharacteristicTypes
        | MdoType::ChartOfCalculationTypes => {
            mdo.find_predefined_item(member_name.as_str()).is_some()
        }
        _ => false,
    };

    hit.then(|| {
        let cfg = FixedConfigCtx(config_id.clone());
        ManagerMemberInfo { ty: db.metadata_ref(ref_kind, owner_name.to_string(), &cfg) }
    })
}

fn predefined_ref_kind_for(kind: MdoType) -> Option<MetadataKind> {
    match kind {
        MdoType::Enum => Some(MetadataKind::EnumRef),
        MdoType::Catalog => Some(MetadataKind::CatalogRef),
        MdoType::ChartOfAccounts => Some(MetadataKind::ChartOfAccountsRef),
        MdoType::ChartOfCharacteristicTypes => Some(MetadataKind::ChartOfCharacteristicTypesRef),
        MdoType::ChartOfCalculationTypes => Some(MetadataKind::ChartOfCalculationTypesRef),
        _ => None,
    }
}

fn find_mdo(
    resolver: &dyn ObjectResolver,
    kind: MdoType,
    name: &str,
) -> Option<Arc<MetadataObject>> {
    resolver.resolve_metadata_object(kind, name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bsl_config::VisibleConfig;
    use bsl_metadata::metadata_object::{EnumValue, PredefinedItem};
    use bsl_metadata::Configuration;
    use bsl_types::testing::InMemoryDb;

    use crate::object_resolver::ConfigsObjectResolver;

    fn lookup_manager_field(
        db: &InMemoryDb,
        configs: &[VisibleConfig],
        base_ty: TypeId,
        member: &Name,
    ) -> Option<ManagerMemberInfo> {
        super::lookup_manager_field(db, &ConfigsObjectResolver(configs), base_ty, member)
    }
    use std::sync::Arc;

    fn wrap(config: Configuration) -> Vec<VisibleConfig> {
        vec![VisibleConfig { name: None, configuration: Arc::new(config) }]
    }

    fn catalog(name: &str, predefined: Vec<&str>) -> MetadataObject {
        let mut mdo = MetadataObject::new(MdoType::Catalog, name);
        for n in predefined {
            mdo.predefined_items.push(PredefinedItem {
                name: n.to_string(),
                name_en: None,
                uuid: String::new(),
            });
        }
        mdo
    }

    fn enum_mdo(name: &str, values: Vec<&str>) -> MetadataObject {
        let mut mdo = MetadataObject::new(MdoType::Enum, name);
        for v in values {
            mdo.enum_values.push(EnumValue {
                name: v.to_string(),
                name_en: None,
                uuid: String::new(),
            });
        }
        mdo
    }

    fn chart_of_accounts(name: &str, predefined: Vec<&str>) -> MetadataObject {
        let mut mdo = MetadataObject::new(MdoType::ChartOfAccounts, name);
        for n in predefined {
            mdo.predefined_items.push(PredefinedItem {
                name: n.to_string(),
                name_en: None,
                uuid: String::new(),
            });
        }
        mdo
    }

    fn chart_of_characteristic_types(name: &str, predefined: Vec<&str>) -> MetadataObject {
        let mut mdo = MetadataObject::new(MdoType::ChartOfCharacteristicTypes, name);
        for n in predefined {
            mdo.predefined_items.push(PredefinedItem {
                name: n.to_string(),
                name_en: None,
                uuid: String::new(),
            });
        }
        mdo
    }

    #[test]
    fn promotion_manager_collection_to_object_manager_when_mdo_exists() {
        let mut config = Configuration::new("Test");
        config.add_metadata_object(catalog("Валюты", vec![]));
        let configs = wrap(config);
        let db = InMemoryDb::new();

        let info = lookup_manager_field(
            &db,
            &configs,
            db.manager_collection(MdoType::Catalog),
            &Name::new("Валюты"),
        )
        .expect("ManagerCollection(Catalog).Валюты must promote when MDO exists");
        assert_eq!(
            info.ty,
            db.object_manager(MdoType::Catalog, "Валюты".to_string(), &RootConfigCtx)
        );
    }

    #[test]
    fn promotion_returns_none_when_mdo_not_in_config() {
        let configs = wrap(Configuration::new("Test"));
        let db = InMemoryDb::new();
        assert!(lookup_manager_field(
            &db,
            &configs,
            db.manager_collection(MdoType::Catalog),
            &Name::new("НеСуществует"),
        )
        .is_none());
    }

    #[test]
    fn promotion_works_for_registers_via_registers_vec() {
        let mut config = Configuration::new("Test");
        config.add_register(
            bsl_metadata::Register::builder()
                .name("РегистрСведений1")
                .mdo_type(MdoType::InformationRegister)
                .build(),
        );
        let configs = wrap(config);
        let db = InMemoryDb::new();

        let info = lookup_manager_field(
            &db,
            &configs,
            db.manager_collection(MdoType::InformationRegister),
            &Name::new("РегистрСведений1"),
        )
        .expect("register promotion must consult Configuration.registers");
        assert_eq!(
            info.ty,
            db.object_manager(
                MdoType::InformationRegister,
                "РегистрСведений1".to_string(),
                &RootConfigCtx,
            )
        );
    }

    #[test]
    fn lookup_enum_value_resolves_to_enum_ref() {
        let mut config = Configuration::new("Test");
        config.add_metadata_object(enum_mdo("Состояния", vec!["Активен", "Закрыт"]));
        let configs = wrap(config);
        let db = InMemoryDb::new();

        let info = lookup_manager_field(
            &db,
            &configs,
            db.object_manager(MdoType::Enum, "Состояния".to_string(), &RootConfigCtx),
            &Name::new("Активен"),
        )
        .expect("enum value must resolve on ObjectManager<Enum, Состояния>");
        assert_eq!(
            info.ty,
            db.metadata_ref(MetadataKind::EnumRef, "Состояния".to_string(), &RootConfigCtx)
        );
    }

    #[test]
    fn lookup_catalog_predefined_resolves_to_catalog_ref() {
        let mut config = Configuration::new("Test");
        config.add_metadata_object(catalog("Валюты", vec!["Доллар", "Евро"]));
        let configs = wrap(config);
        let db = InMemoryDb::new();

        let info = lookup_manager_field(
            &db,
            &configs,
            db.object_manager(MdoType::Catalog, "Валюты".to_string(), &RootConfigCtx),
            &Name::new("Доллар"),
        )
        .expect("predefined item must resolve on ObjectManager<Catalog, Валюты>");
        assert_eq!(
            info.ty,
            db.metadata_ref(MetadataKind::CatalogRef, "Валюты".to_string(), &RootConfigCtx)
        );
    }

    #[test]
    fn lookup_chart_of_accounts_predefined_resolves_to_chart_of_accounts_ref() {
        let mut config = Configuration::new("Test");
        config.add_metadata_object(chart_of_accounts("Хозрасчетный", vec!["Касса"]));
        let configs = wrap(config);
        let db = InMemoryDb::new();

        let info = lookup_manager_field(
            &db,
            &configs,
            db.object_manager(MdoType::ChartOfAccounts, "Хозрасчетный".to_string(), &RootConfigCtx),
            &Name::new("Касса"),
        )
        .expect("chart-of-accounts predefined item must resolve");
        assert_eq!(
            info.ty,
            db.metadata_ref(
                MetadataKind::ChartOfAccountsRef,
                "Хозрасчетный".to_string(),
                &RootConfigCtx,
            )
        );
    }

    #[test]
    fn lookup_chart_of_characteristic_types_predefined_resolves_to_ref() {
        let mut config = Configuration::new("Test");
        config.add_metadata_object(chart_of_characteristic_types(
            "СтатьиРасходов",
            vec!["ОсновноеПодразделение"],
        ));
        let configs = wrap(config);
        let db = InMemoryDb::new();

        let info = lookup_manager_field(
            &db,
            &configs,
            db.object_manager(
                MdoType::ChartOfCharacteristicTypes,
                "СтатьиРасходов".to_string(),
                &RootConfigCtx,
            ),
            &Name::new("ОсновноеПодразделение"),
        )
        .expect("chart-of-characteristic-types predefined item must resolve");
        assert_eq!(
            info.ty,
            db.metadata_ref(
                MetadataKind::ChartOfCharacteristicTypesRef,
                "СтатьиРасходов".to_string(),
                &RootConfigCtx,
            )
        );
    }

    #[test]
    fn lookup_unknown_member_returns_none() {
        let mut config = Configuration::new("Test");
        config.add_metadata_object(catalog("Валюты", vec!["Доллар"]));
        let configs = wrap(config);
        let db = InMemoryDb::new();

        assert!(lookup_manager_field(
            &db,
            &configs,
            db.object_manager(MdoType::Catalog, "Валюты".to_string(), &RootConfigCtx),
            &Name::new("Несуществующий"),
        )
        .is_none());
    }

    #[test]
    fn lookup_on_unsupported_owner_kind_returns_none() {
        let mut config = Configuration::new("Test");
        let doc = MetadataObject::new(MdoType::Document, "ПКО");
        config.add_metadata_object(doc);
        let configs = wrap(config);
        let db = InMemoryDb::new();

        assert!(lookup_manager_field(
            &db,
            &configs,
            db.object_manager(MdoType::Document, "ПКО".to_string(), &RootConfigCtx),
            &Name::new("Любой"),
        )
        .is_none());
    }

    #[test]
    fn lookup_on_non_manager_receiver_returns_none() {
        let configs = wrap(Configuration::new("Test"));
        let db = InMemoryDb::new();
        for ty in [
            db.unknown(),
            db.number(None, None),
            db.string(None, false),
            db.array(None),
            db.metadata_ref(MetadataKind::CatalogRef, "X".to_string(), &RootConfigCtx),
            db.union(vec![db.number(None, None), db.string(None, false)]),
        ] {
            assert!(
                lookup_manager_field(&db, &configs, ty, &Name::new("Любой")).is_none(),
                "no manager lookup on {:?}",
                db.lookup_type(ty),
            );
        }
    }

    #[test]
    fn promotion_merges_base_and_extension_predefined_items() {
        use bsl_metadata::ObjectBelonging;

        let mut main = Configuration::new("Main");
        let base_uuid = uuid::Uuid::new_v4();
        let mut base_catalog = catalog("Валюты", vec!["Доллар"]);
        base_catalog.set_uuid(base_uuid);
        main.add_metadata_object(base_catalog);

        let mut ext = Configuration::new("Ext");
        let mut adopted_catalog = catalog("Валюты", vec!["Евро"]);
        adopted_catalog.set_object_belonging(ObjectBelonging::Adopted);
        adopted_catalog.set_extends_uuid(base_uuid);
        ext.add_metadata_object(adopted_catalog);
        let configs = vec![
            VisibleConfig { name: None, configuration: Arc::new(main) },
            VisibleConfig { name: Some("Ext".into()), configuration: Arc::new(ext) },
        ];
        let db = InMemoryDb::new();

        // An extension that borrows Валюты and adds the predefined Евро does not
        // hide the base's Доллар — both predefined items resolve on the merged MDO.
        for item in ["Евро", "Доллар"] {
            let info = lookup_manager_field(
                &db,
                &configs,
                db.object_manager(MdoType::Catalog, "Валюты".to_string(), &RootConfigCtx),
                &Name::new(item),
            )
            .unwrap_or_else(|| panic!("predefined item {item} must resolve on the merged catalog"));
            assert_eq!(
                info.ty,
                db.metadata_ref(MetadataKind::CatalogRef, "Валюты".to_string(), &RootConfigCtx)
            );
        }
    }
}
