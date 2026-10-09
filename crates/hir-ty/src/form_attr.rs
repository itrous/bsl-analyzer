use bsl_metadata::{AttributeType, FormAttribute, MdoType, MetadataResolver};
use bsl_types::builders::Builders;
use bsl_types::facet::{FormDataFacet, MdoRefFacet};
use bsl_types::intern::TypeKernelDb;
use bsl_types::kind::{MetadataKind, TypeId};
use bsl_types::testing::RootConfigCtx;
use hir_def::resolver::Resolver;
use hir_def::Name;

use crate::db::HirDatabase;
use crate::field_enum::attribute_type_to_typeid;

pub fn lower_form_attribute_to_typeid(
    db: &dyn TypeKernelDb,
    attr: &FormAttribute,
    resolver: &dyn MetadataResolver,
) -> TypeId {
    let has_columns = !attr.columns.is_empty();

    // The form module never holds the record manager itself, main attribute or not:
    // the platform hands it over as a form-data structure over the register's
    // columns, so `Запись.<Измерение>` is a field and `Запись.Записать()` is no method.
    if let AttributeType::InformationRegisterRecordManager { name } = &attr.attr_type {
        return db.mk_form_data(
            FormDataFacet::Structure,
            Some(MdoRefFacet::new(MdoType::InformationRegister, name.clone())),
        );
    }

    if attr.is_main {
        let kind = if has_columns {
            FormDataFacet::StructureWithCollection
        } else {
            FormDataFacet::Structure
        };

        if let AttributeType::Ref { mdo_type, name: mdo_name } = &attr.attr_type {
            if MetadataKind::object_kind_for(*mdo_type).is_some() {
                return db.mk_form_data(
                    kind,
                    Some(MdoRefFacet::new(*mdo_type, mdo_name.as_str().to_string())),
                );
            }
        }
        if matches!(&attr.attr_type, AttributeType::AnyObjectRef { .. }) {
            return db.mk_form_data(kind, None);
        }
    }

    // A value-tree attribute is ДанныеФормыДерево on a managed form; the
    // generic Collection shape is the value-table/tabular-section case.
    if matches!(
        &attr.attr_type,
        AttributeType::Platform(bsl_metadata::PlatformValueType::ValueTree)
    ) {
        return db.mk_form_data(FormDataFacet::Tree, None);
    }

    if has_columns {
        return db.mk_form_data(FormDataFacet::Collection, None);
    }

    attribute_type_to_typeid(db, &attr.attr_type, resolver)
}

/// The application value `РеквизитФормыВЗначение` turns a form attribute into, where
/// the attribute's declared type names it unambiguously.
///
/// The parser keeps one `Ref` shape for both `cfg:CatalogObject.X` and
/// `cfg:CatalogRef.X`, so a plain attribute of a kind that has references is not
/// trusted: a reference converts to itself, an object to an object, and the two
/// spellings are indistinguishable here. The main attribute is always the object,
/// and a data processor or report has no reference to be confused with.
pub(crate) fn form_attribute_value_typeid(
    db: &dyn TypeKernelDb,
    attr: &FormAttribute,
    resolver: &dyn MetadataResolver,
) -> Option<TypeId> {
    match &attr.attr_type {
        AttributeType::Ref { mdo_type, name }
            if attr.is_main || MetadataKind::ref_kind_for(*mdo_type).is_none() =>
        {
            let kind = MetadataKind::object_kind_for(*mdo_type)?;
            Some(db.metadata_ref(kind, name.clone(), &RootConfigCtx))
        }
        AttributeType::Platform(
            bsl_metadata::PlatformValueType::ValueTable
            | bsl_metadata::PlatformValueType::ValueTree,
        ) => Some(attribute_type_to_typeid(db, &attr.attr_type, resolver))
            .filter(|ty| *ty != db.unknown()),
        _ => None,
    }
}

/// [`form_attribute_value_typeid`] of the attribute `name` of the managed form this
/// module belongs to.
pub(crate) fn resolve_form_attribute_value(
    db: &dyn HirDatabase,
    resolver: &Resolver,
    name: &str,
) -> Option<TypeId> {
    if !crate::this_object::is_managed_form_module(db, resolver) {
        return None;
    }
    let module_id = resolver.module_id()?;
    let metadata = db.module_metadata(module_id);
    let attr = metadata.form.as_ref()?.find_attribute(name)?;
    let obj_resolver = crate::object_resolver::DbObjectResolver::new(db, module_id.file_id);
    form_attribute_value_typeid(db, attr, &obj_resolver)
}

pub(crate) fn resolve_form_attribute(
    db: &dyn HirDatabase,
    resolver: &Resolver,
    name: &Name,
) -> Option<TypeId> {
    if !crate::this_object::is_managed_form_module(db, resolver) {
        return None;
    }

    let module_id = resolver.module_id()?;
    let metadata = db.module_metadata(module_id);
    let form = metadata.form.as_ref()?;
    let attr = form.find_attribute(name.as_str())?;

    let obj_resolver = crate::object_resolver::DbObjectResolver::new(db, module_id.file_id);
    Some(lower_form_attribute_to_typeid(db, attr, &obj_resolver))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object_resolver::ConfigsObjectResolver;
    use bsl_metadata::{FormAttributeColumn, MdoType};
    use bsl_types::kind::TypeKind;
    use bsl_types::testing::InMemoryDb;

    fn plain(name: &str, attr_type: AttributeType) -> FormAttribute {
        FormAttribute::new(name, attr_type)
    }

    fn main_attr(name: &str, attr_type: AttributeType) -> FormAttribute {
        FormAttribute { name: name.to_string(), attr_type, is_main: true, columns: vec![] }
    }

    fn assert_metadata_ref(
        db: &InMemoryDb,
        id: TypeId,
        expected_kind: MetadataKind,
        expected_name: &str,
    ) {
        match db.lookup_type(id) {
            TypeKind::MetadataRef(facet) => {
                assert_eq!(facet.kind, expected_kind);
                assert_eq!(facet.name.as_str(), expected_name);
            }
            other => {
                panic!("expected MetadataRef({expected_kind:?}, {expected_name}), got {other:?}")
            }
        }
    }

    fn assert_form_data(
        db: &InMemoryDb,
        id: TypeId,
        expected_kind: FormDataFacet,
        expected_underlying: Option<(MdoType, &str)>,
    ) {
        match db.lookup_type(id) {
            TypeKind::FormData { kind, underlying } => {
                assert_eq!(*kind, expected_kind);
                match (underlying, expected_underlying) {
                    (Some(actual), Some((mdo_type, name))) => {
                        assert_eq!(actual.mdo_type, mdo_type);
                        assert_eq!(actual.name.as_str(), name);
                    }
                    (None, None) => {}
                    _ => panic!("unexpected FormData underlying: {underlying:?}"),
                }
            }
            other => panic!("expected FormData({expected_kind:?}), got {other:?}"),
        }
    }

    #[test]
    fn primitive_attribute_lowers_through_generic_adapter() {
        let db = InMemoryDb::new();
        let attr = plain("Замечание", AttributeType::String { length: Some(100) });
        assert_eq!(
            lower_form_attribute_to_typeid(&db, &attr, &ConfigsObjectResolver(&[])),
            db.string(None, false)
        );
    }

    #[test]
    fn boolean_attribute_lowers_to_boolean() {
        let db = InMemoryDb::new();
        let attr = plain("Флаг", AttributeType::Boolean);
        assert_eq!(
            lower_form_attribute_to_typeid(&db, &attr, &ConfigsObjectResolver(&[])),
            db.boolean()
        );
    }

    #[test]
    fn value_list_attribute_lowers_to_kernel_value_list() {
        use bsl_metadata::PlatformValueType;
        let db = InMemoryDb::new();
        let attr = plain("СценарииВыгрузки", AttributeType::Platform(PlatformValueType::ValueList));
        assert_eq!(
            lower_form_attribute_to_typeid(&db, &attr, &ConfigsObjectResolver(&[])),
            db.value_list(None)
        );
    }

    #[test]
    fn value_tree_attribute_lowers_to_form_data_tree() {
        use bsl_metadata::PlatformValueType;
        use bsl_types::facet::FormDataFacet;
        let db = InMemoryDb::new();
        let mut attr =
            plain("ДеревоРазделов", AttributeType::Platform(PlatformValueType::ValueTree));
        attr.columns = vec![FormAttributeColumn {
            name: "Раздел".to_string(),
            attr_type: AttributeType::Unknown,
        }];
        let ty = lower_form_attribute_to_typeid(&db, &attr, &ConfigsObjectResolver(&[]));
        match db.lookup_type(ty) {
            TypeKind::FormData { kind, underlying } => {
                assert_eq!(*kind, FormDataFacet::Tree);
                assert!(underlying.is_none());
            }
            other => panic!("expected FormData(Tree), got {other:?}"),
        }
    }

    #[test]
    fn value_tree_attribute_without_columns_still_lowers_to_tree() {
        use bsl_metadata::PlatformValueType;
        use bsl_types::facet::FormDataFacet;
        let db = InMemoryDb::new();
        let attr = plain("Дерево", AttributeType::Platform(PlatformValueType::ValueTree));
        let ty = lower_form_attribute_to_typeid(&db, &attr, &ConfigsObjectResolver(&[]));
        match db.lookup_type(ty) {
            TypeKind::FormData { kind, .. } => assert_eq!(*kind, FormDataFacet::Tree),
            other => panic!("expected FormData(Tree), got {other:?}"),
        }
    }

    #[test]
    fn record_manager_attribute_is_form_data_over_its_register_main_or_not() {
        let db = InMemoryDb::new();
        for attr in [
            main_attr(
                "Запись",
                AttributeType::InformationRegisterRecordManager { name: "Курсы".to_string() },
            ),
            plain(
                "ПредыдущаяЗапись",
                AttributeType::InformationRegisterRecordManager { name: "Курсы".to_string() },
            ),
        ] {
            let id = lower_form_attribute_to_typeid(&db, &attr, &ConfigsObjectResolver(&[]));
            assert_form_data(
                &db,
                id,
                FormDataFacet::Structure,
                Some((MdoType::InformationRegister, "Курсы")),
            );
        }
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn platform_object_fallback_attribute_lowers_to_platform_object() {
        use bsl_metadata::PlatformValueType;
        let db = InMemoryDb::new();
        let attr =
            plain("Документ", AttributeType::Platform(PlatformValueType::SpreadsheetDocument));
        let id = lower_form_attribute_to_typeid(&db, &attr, &ConfigsObjectResolver(&[]));
        assert_eq!(id, db.platform_object("ТабличныйДокумент".to_string()));
    }

    #[test]
    fn ref_attribute_lowers_to_metadata_ref() {
        let attr = plain(
            "Контрагент",
            AttributeType::Ref {
                mdo_type: MdoType::Catalog, name: "Контрагенты".to_string()
            },
        );
        let db = InMemoryDb::new();
        let id = lower_form_attribute_to_typeid(&db, &attr, &ConfigsObjectResolver(&[]));
        assert_metadata_ref(&db, id, MetadataKind::CatalogRef, "Контрагенты");
    }

    #[test]
    fn main_attribute_with_object_ref_projects_to_form_data_structure() {
        let attr = main_attr(
            "Объект",
            AttributeType::Ref { mdo_type: MdoType::Document, name: "Заказ".to_string() },
        );
        let db = InMemoryDb::new();
        let id = lower_form_attribute_to_typeid(&db, &attr, &ConfigsObjectResolver(&[]));
        assert_form_data(&db, id, FormDataFacet::Structure, Some((MdoType::Document, "Заказ")));
    }

    #[test]
    fn main_attribute_data_processor_projects_to_form_data_structure() {
        let attr = main_attr(
            "Объект",
            AttributeType::Ref {
                mdo_type: MdoType::DataProcessor,
                name: "БУС_ПомощникИмпортаТоваровБитрикс".to_string(),
            },
        );
        let db = InMemoryDb::new();
        let id = lower_form_attribute_to_typeid(&db, &attr, &ConfigsObjectResolver(&[]));
        assert_form_data(
            &db,
            id,
            FormDataFacet::Structure,
            Some((MdoType::DataProcessor, "БУС_ПомощникИмпортаТоваровБитрикс")),
        );
    }

    #[test]
    fn main_attribute_report_projects_to_form_data_structure() {
        let attr = main_attr(
            "Объект",
            AttributeType::Ref { mdo_type: MdoType::Report, name: "Анализ".to_string() },
        );
        let db = InMemoryDb::new();
        let id = lower_form_attribute_to_typeid(&db, &attr, &ConfigsObjectResolver(&[]));
        assert_form_data(&db, id, FormDataFacet::Structure, Some((MdoType::Report, "Анализ")));
    }

    #[test]
    fn main_attribute_business_process_projects_to_form_data_structure() {
        let attr = main_attr(
            "Объект",
            AttributeType::Ref {
                mdo_type: MdoType::BusinessProcess,
                name: "Согласование".to_string(),
            },
        );
        let db = InMemoryDb::new();
        let id = lower_form_attribute_to_typeid(&db, &attr, &ConfigsObjectResolver(&[]));
        assert_form_data(
            &db,
            id,
            FormDataFacet::Structure,
            Some((MdoType::BusinessProcess, "Согласование")),
        );
    }

    #[test]
    fn main_attribute_task_projects_to_form_data_structure() {
        let attr = main_attr(
            "Объект",
            AttributeType::Ref {
                mdo_type: MdoType::Task, name: "ЗадачаИсполнителя".to_string()
            },
        );
        let db = InMemoryDb::new();
        let id = lower_form_attribute_to_typeid(&db, &attr, &ConfigsObjectResolver(&[]));
        assert_form_data(
            &db,
            id,
            FormDataFacet::Structure,
            Some((MdoType::Task, "ЗадачаИсполнителя")),
        );
    }

    #[test]
    fn main_attribute_with_unsupported_mdo_falls_through() {
        let attr = main_attr(
            "Регистр",
            AttributeType::Ref { mdo_type: MdoType::InformationRegister, name: "X".to_string() },
        );
        let db = InMemoryDb::new();
        let id = lower_form_attribute_to_typeid(&db, &attr, &ConfigsObjectResolver(&[]));
        assert_metadata_ref(&db, id, MetadataKind::InformationRegisterRef, "X");
    }

    #[test]
    fn main_attribute_with_bare_object_kind_yields_form_data_without_underlying() {
        let attr = main_attr("Объект", AttributeType::AnyObjectRef { mdo_type: MdoType::Catalog });
        let db = InMemoryDb::new();
        let id = lower_form_attribute_to_typeid(&db, &attr, &ConfigsObjectResolver(&[]));
        assert_form_data(&db, id, FormDataFacet::Structure, None);
    }

    #[test]
    fn columns_attribute_lowers_to_form_data_collection() {
        let attr = FormAttribute {
            name: "Таблица".to_string(),
            attr_type: AttributeType::Unknown,
            is_main: false,
            columns: vec![FormAttributeColumn {
                name: "Колонка1".to_string(),
                attr_type: AttributeType::Boolean,
            }],
        };
        let db = InMemoryDb::new();
        let id = lower_form_attribute_to_typeid(&db, &attr, &ConfigsObjectResolver(&[]));
        assert_form_data(&db, id, FormDataFacet::Collection, None);
    }

    #[test]
    fn columns_take_precedence_over_main_marker() {
        let attr = FormAttribute {
            name: "Главная".to_string(),
            attr_type: AttributeType::Unknown,
            is_main: true,
            columns: vec![FormAttributeColumn {
                name: "К".to_string(),
                attr_type: AttributeType::Boolean,
            }],
        };
        let db = InMemoryDb::new();
        let id = lower_form_attribute_to_typeid(&db, &attr, &ConfigsObjectResolver(&[]));
        assert_form_data(&db, id, FormDataFacet::Collection, None);
    }

    #[test]
    fn unknown_attribute_lowers_to_unknown() {
        let db = InMemoryDb::new();
        let attr = plain("БезТипа", AttributeType::Unknown);
        assert_eq!(
            lower_form_attribute_to_typeid(&db, &attr, &ConfigsObjectResolver(&[])),
            db.unknown()
        );
    }

    #[test]
    fn main_attribute_with_columns_uses_structure_with_collection() {
        let attr = FormAttribute {
            name: "Объект".to_string(),
            attr_type: AttributeType::Ref {
                mdo_type: MdoType::Document,
                name: "Заказ".to_string(),
            },
            is_main: true,
            columns: vec![FormAttributeColumn {
                name: "Товары".to_string(),
                attr_type: AttributeType::Unknown,
            }],
        };
        let db = InMemoryDb::new();
        let id = lower_form_attribute_to_typeid(&db, &attr, &ConfigsObjectResolver(&[]));
        assert_form_data(
            &db,
            id,
            FormDataFacet::StructureWithCollection,
            Some((MdoType::Document, "Заказ")),
        );
    }

    #[test]
    fn form_data_structure_blocks_object_methods() {
        use crate::method_lookup::platform_type_key_id;

        let db = InMemoryDb::new();
        let main_obj = db.mk_form_data(
            FormDataFacet::Structure,
            Some(MdoRefFacet::new(MdoType::Document, "Заказ".to_string())),
        );
        assert_eq!(platform_type_key_id(&db, main_obj).as_deref(), Some("ДанныеФормыСтруктура"));

        let main_obj_with_columns = db.mk_form_data(
            FormDataFacet::StructureWithCollection,
            Some(MdoRefFacet::new(MdoType::Document, "Заказ".to_string())),
        );
        assert_eq!(
            platform_type_key_id(&db, main_obj_with_columns).as_deref(),
            Some("ДанныеФормыСтруктураСКоллекцией")
        );

        let table = db.mk_form_data(FormDataFacet::Collection, None);
        assert_eq!(platform_type_key_id(&db, table).as_deref(), Some("ДанныеФормыКоллекция"));

        for (mdo, name) in [
            (MdoType::DataProcessor, "Помощник"),
            (MdoType::Report, "Анализ"),
            (MdoType::BusinessProcess, "Согласование"),
            (MdoType::Task, "ЗадачаИсполнителя"),
        ] {
            let receiver = db.mk_form_data(
                FormDataFacet::Structure,
                Some(MdoRefFacet::new(mdo, name.to_string())),
            );
            assert_eq!(
                platform_type_key_id(&db, receiver).as_deref(),
                Some("ДанныеФормыСтруктура"),
                "FormData wrapper for {:?} must NOT route through {:?}'s HBK surface",
                mdo,
                mdo
            );
        }
    }

    #[test]
    fn form_data_structure_projects_for_fields() {
        use crate::field_lookup;
        let db = InMemoryDb::new();

        let receiver = db.mk_form_data(
            FormDataFacet::Structure,
            Some(MdoRefFacet::new(MdoType::Document, "Заказ".to_string())),
        );
        let _ = field_lookup::lookup_field(
            &db,
            &ConfigsObjectResolver(&[]),
            receiver,
            &Name::new("Дата"),
        );
        let table = db.mk_form_data(FormDataFacet::Collection, None);
        assert!(field_lookup::lookup_field(
            &db,
            &ConfigsObjectResolver(&[]),
            table,
            &Name::new("Дата")
        )
        .is_none());
    }

    fn value_of(db: &InMemoryDb, attr: &FormAttribute) -> Option<TypeId> {
        form_attribute_value_typeid(db, attr, &ConfigsObjectResolver(&[]))
    }

    #[test]
    fn the_main_attribute_converts_to_its_object() {
        let db = InMemoryDb::new();
        for (mdo_type, kind) in [
            (MdoType::DataProcessor, MetadataKind::DataProcessorObject),
            (MdoType::Report, MetadataKind::ReportObject),
            (MdoType::Catalog, MetadataKind::CatalogObject),
            (MdoType::Document, MetadataKind::DocumentObject),
            (MdoType::ExternalDataProcessor, MetadataKind::ExternalDataProcessorObject),
        ] {
            let attr =
                main_attr("Объект", AttributeType::Ref { mdo_type, name: "Имя".to_string() });
            let value = value_of(&db, &attr).expect("main attribute value");
            assert_metadata_ref(&db, value, kind, "Имя");
        }
    }

    #[test]
    fn a_plain_object_attribute_converts_only_where_no_reference_exists() {
        let db = InMemoryDb::new();
        let processor = plain(
            "Помощник",
            AttributeType::Ref {
                mdo_type: MdoType::DataProcessor, name: "Помощник".to_string()
            },
        );
        let value = value_of(&db, &processor).expect("a data processor has no reference");
        assert_metadata_ref(&db, value, MetadataKind::DataProcessorObject, "Помощник");

        let catalog = plain(
            "Контрагент",
            AttributeType::Ref {
                mdo_type: MdoType::Catalog, name: "Контрагенты".to_string()
            },
        );
        assert_eq!(value_of(&db, &catalog), None);
    }

    #[test]
    fn value_table_and_tree_attributes_convert_to_the_collections() {
        use bsl_metadata::PlatformValueType;
        let db = InMemoryDb::new();
        let mut table = plain("Таблица", AttributeType::Platform(PlatformValueType::ValueTable));
        table.columns = vec![FormAttributeColumn {
            name: "Колонка".to_string(),
            attr_type: AttributeType::Unknown,
        }];
        let value = value_of(&db, &table).expect("value table");
        assert!(matches!(db.lookup_type(value), TypeKind::ValueTable(_)), "{value:?}");

        let tree = plain("Дерево", AttributeType::Platform(PlatformValueType::ValueTree));
        let tree_value = value_of(&db, &tree).expect("value tree");
        assert_eq!(
            tree_value,
            attribute_type_to_typeid(&db, &tree.attr_type, &ConfigsObjectResolver(&[]))
        );
        assert_ne!(tree_value, db.unknown());
    }

    #[test]
    fn other_attributes_stay_unconverted() {
        use bsl_metadata::PlatformValueType;
        let db = InMemoryDb::new();
        for attr_type in [
            AttributeType::Platform(PlatformValueType::ValueList),
            AttributeType::Platform(PlatformValueType::DynamicList),
            AttributeType::String { length: None },
            AttributeType::AnyObjectRef { mdo_type: MdoType::Catalog },
            AttributeType::Unknown,
        ] {
            let attr = main_attr("Объект", attr_type.clone());
            assert_eq!(value_of(&db, &attr), None, "{attr_type:?}");
        }
    }
}
