use std::sync::Arc;

use bsl_metadata::Name;

use crate::facet::{
    ArrayFacet, CommonModuleFacet, DateFacet, FormBindingFacet, FormDataFacet, FormElementFacet,
    FunctionFacet, ManagerFacet, MapFacet, MdoRefFacet, MetaObjFacet, MetaRefFacet, NumberFacet,
    PlatformObjectFacet, ProjectionFacet, ProjectionSource, StringFacet, StructureFacet,
    TableFacet, TableSource,
};
use crate::intern::TypeKernelDb;
use crate::kind::{
    ConfigId, MetadataKind, MetadataReferenceKind, Projection, ProjectionField,
    ProjectionFieldSource, ProjectionOrigin, TypeId, TypeKind, TypeOrigin,
};

pub trait ConfigCtx {
    fn resolve_config_id(&self, kind: MetadataKind, name: &Name) -> ConfigId;

    fn resolve_manager_config_id(&self, _mdo: bsl_metadata::MdoType, _name: &Name) -> ConfigId {
        ConfigId::Root
    }
}

impl ConfigCtx for crate::testing::RootConfigCtx {
    fn resolve_config_id(&self, _kind: MetadataKind, _name: &Name) -> ConfigId {
        ConfigId::Root
    }
}

pub trait Builders: TypeKernelDb {
    fn unknown(&self) -> TypeId {
        self.intern_type(TypeKind::Unknown)
    }

    fn never(&self) -> TypeId {
        self.intern_type(TypeKind::Never)
    }

    fn any(&self) -> TypeId {
        self.intern_type(TypeKind::Any)
    }

    fn number(&self, precision: Option<u8>, scale: Option<u8>) -> TypeId {
        self.intern_type(TypeKind::Number(NumberFacet { precision, scale, origin: None }))
    }

    fn string(&self, length: Option<u32>, fixed: bool) -> TypeId {
        self.intern_type(TypeKind::String(StringFacet { length, fixed, origin: None }))
    }

    fn date(&self, component: crate::facet::DateComponent) -> TypeId {
        self.intern_type(TypeKind::Date(DateFacet { component, origin: None }))
    }

    fn boolean(&self) -> TypeId {
        self.intern_type(TypeKind::Boolean)
    }

    fn null(&self) -> TypeId {
        self.intern_type(TypeKind::Null)
    }

    fn undefined(&self) -> TypeId {
        self.intern_type(TypeKind::Undefined)
    }

    fn array(&self, element: Option<TypeId>) -> TypeId {
        self.intern_type(TypeKind::Array(ArrayFacet { element }))
    }

    fn map(&self, key: Option<TypeId>, value: Option<TypeId>) -> TypeId {
        self.intern_type(TypeKind::Map(MapFacet { key, value }))
    }

    fn structure(&self, keys: Option<Arc<[Name]>>) -> TypeId {
        self.intern_type(TypeKind::Structure(StructureFacet {
            keys,
            fields: None,
            origin: None,
            closed: false,
        }))
    }

    /// A structure whose keys/value-types are known (inferred from a literal `Новый Структура` /
    /// `.Вставить` construction, or from a doc-comment). `keys` is derived from the projection's
    /// field order so name-only consumers keep working.
    fn structure_typed(&self, projection: Arc<Projection>, origin: TypeOrigin) -> TypeId {
        let keys: Arc<[Name]> = projection.fields.iter().map(|f| f.name.clone()).collect();
        self.intern_type(TypeKind::Structure(StructureFacet {
            keys: Some(keys),
            fields: Some(projection),
            origin: Some(origin),
            closed: false,
        }))
    }

    /// A typed structure whose non-empty projection is proven complete.
    fn structure_typed_closed(&self, projection: Arc<Projection>, origin: TypeOrigin) -> TypeId {
        if projection.fields.is_empty() {
            return self.structure_typed(projection, origin);
        }
        let keys: Arc<[Name]> = projection.fields.iter().map(|f| f.name.clone()).collect();
        self.intern_type(TypeKind::Structure(StructureFacet {
            keys: Some(keys),
            fields: Some(projection),
            origin: Some(origin),
            closed: true,
        }))
    }

    fn value_table(&self, projection: Option<Arc<Projection>>, source: TableSource) -> TypeId {
        self.intern_type(TypeKind::ValueTable(TableFacet { projection, source }))
    }

    fn value_table_row(&self, projection: Option<Arc<Projection>>, source: TableSource) -> TypeId {
        self.intern_type(TypeKind::ValueTableRow(TableFacet { projection, source }))
    }

    fn value_list(&self, element: Option<TypeId>) -> TypeId {
        self.intern_type(TypeKind::ValueList(element))
    }

    fn uuid(&self) -> TypeId {
        self.intern_type(TypeKind::Uuid)
    }

    fn value_storage(&self) -> TypeId {
        self.intern_type(TypeKind::ValueStorage)
    }

    fn type_descriptor(&self) -> TypeId {
        self.intern_type(TypeKind::TypeDescriptor)
    }

    fn metadata_ref(&self, kind: MetadataKind, name: Name, cfg: &dyn ConfigCtx) -> TypeId {
        let config_id = cfg.resolve_config_id(kind, &name);
        self.intern_type(TypeKind::MetadataRef(MetaRefFacet { kind, name, config_id }))
    }

    fn metadata_object(&self, kind: MetadataKind, name: Name, cfg: &dyn ConfigCtx) -> TypeId {
        let config_id = cfg.resolve_config_id(kind, &name);
        self.intern_type(TypeKind::MetadataObject(MetaObjFacet { kind, name, config_id }))
    }

    fn any_metadata_ref(&self, mdo_type: bsl_metadata::MdoType) -> TypeId {
        self.intern_type(TypeKind::AnyMetadataRef { mdo_type })
    }

    fn metadata_reference_collection(&self, kind: MetadataReferenceKind) -> TypeId {
        self.intern_type(TypeKind::MetadataReferenceCollection(kind))
    }

    fn metadata_reference(&self, kind: MetadataReferenceKind, name: Name) -> TypeId {
        self.intern_type(TypeKind::MetadataReference { kind, name })
    }

    fn metadata_object_collection(&self, mdo_type: bsl_metadata::MdoType) -> TypeId {
        self.intern_type(TypeKind::MetadataObjectCollection(mdo_type))
    }

    fn any_ref(&self) -> TypeId {
        self.intern_type(TypeKind::AnyRef)
    }

    fn manager_collection(&self, mdo_type: bsl_metadata::MdoType) -> TypeId {
        self.intern_type(TypeKind::ManagerCollection(mdo_type))
    }

    fn object_manager(
        &self,
        mdo: bsl_metadata::MdoType,
        name: Name,
        cfg: &dyn ConfigCtx,
    ) -> TypeId {
        let config_id = cfg.resolve_manager_config_id(mdo, &name);
        self.intern_type(TypeKind::ObjectManager(ManagerFacet { mdo, name, config_id }))
    }

    fn object_manager_with_config(
        &self,
        mdo: bsl_metadata::MdoType,
        name: Name,
        config_id: ConfigId,
    ) -> TypeId {
        self.intern_type(TypeKind::ObjectManager(ManagerFacet { mdo, name, config_id }))
    }

    fn tabular_section(&self, parent: MetaRefFacet, name: Name) -> TypeId {
        self.intern_type(TypeKind::TabularSection { parent, name })
    }

    fn tabular_section_row(&self, parent: MetaRefFacet, name: Name) -> TypeId {
        self.intern_type(TypeKind::TabularSectionRow { parent, name })
    }

    fn register_dimension(&self, parent: MetaRefFacet, name: Name) -> TypeId {
        self.intern_type(TypeKind::RegisterDimension { parent, name })
    }

    fn register_resource(&self, parent: MetaRefFacet, name: Name) -> TypeId {
        self.intern_type(TypeKind::RegisterResource { parent, name })
    }

    fn register_attribute(&self, parent: MetaRefFacet, name: Name) -> TypeId {
        self.intern_type(TypeKind::RegisterAttribute { parent, name })
    }

    fn register_filter(&self, parent: MetaRefFacet) -> TypeId {
        self.intern_type(TypeKind::RegisterFilter { parent })
    }

    fn attribute(&self, parent: MetaRefFacet, name: Name) -> TypeId {
        self.intern_type(TypeKind::Attribute { parent, name })
    }

    fn meta_ref_facet(&self, kind: MetadataKind, name: Name, cfg: &dyn ConfigCtx) -> MetaRefFacet {
        let config_id = cfg.resolve_config_id(kind, &name);
        MetaRefFacet { kind, name, config_id }
    }

    fn mk_form_data(&self, kind: FormDataFacet, underlying: Option<MdoRefFacet>) -> TypeId {
        self.intern_type(TypeKind::FormData { kind, underlying })
    }

    fn mk_form_control(&self, kind: FormElementFacet, binding: Option<FormBindingFacet>) -> TypeId {
        self.intern_type(TypeKind::FormControl { kind, binding })
    }

    fn mk_this_object(&self, config_id: ConfigId, owner: MdoRefFacet) -> TypeId {
        self.intern_type(TypeKind::ThisObject { config_id, owner })
    }

    fn mk_this_manager(&self, config_id: ConfigId, owner: MdoRefFacet) -> TypeId {
        self.intern_type(TypeKind::ThisManager { config_id, owner })
    }

    fn common_module(&self, name: Name, config_id: ConfigId) -> TypeId {
        self.intern_type(TypeKind::CommonModule(CommonModuleFacet { name, config_id }))
    }

    fn platform_object(&self, name: Name) -> TypeId {
        self.intern_type(TypeKind::PlatformObject(PlatformObjectFacet { name }))
    }

    fn union(&self, members: Vec<TypeId>) -> TypeId {
        self.intern_type(TypeKind::Union(members.into()))
    }

    fn query_result(
        &self,
        projection: Option<Arc<Projection>>,
        source: ProjectionSource,
    ) -> TypeId {
        self.intern_type(TypeKind::QueryResult(ProjectionFacet { projection, source }))
    }

    fn query_result_selection(
        &self,
        projection: Option<Arc<Projection>>,
        source: ProjectionSource,
    ) -> TypeId {
        self.intern_type(TypeKind::QueryResultSelection(ProjectionFacet { projection, source }))
    }

    fn query_batch_result(&self, per_query: Arc<[Option<Arc<Projection>>]>) -> TypeId {
        self.intern_type(TypeKind::QueryBatchResult { per_query })
    }

    fn query(&self, projections: Arc<[Option<Arc<Projection>>]>) -> TypeId {
        self.intern_type(TypeKind::Query { projections })
    }

    fn function(&self, facet: FunctionFacet) -> TypeId {
        self.intern_type(TypeKind::Function(facet))
    }

    fn projection_from_fields(
        &self,
        fields: Vec<(Name, TypeId)>,
        field_source: ProjectionFieldSource,
        origin: ProjectionOrigin,
    ) -> Arc<Projection> {
        let fields: Arc<[ProjectionField]> = fields
            .into_iter()
            .map(|(name, ty)| ProjectionField { name, ty, source: field_source })
            .collect();
        Arc::new(Projection { fields, origin, raw_sdbl_types: None })
    }
}

impl<T: TypeKernelDb + ?Sized> Builders for T {}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use bsl_metadata::MdoType;

    use crate::facet::{
        DateComponent, FormBindingTargetFacet, FormDataFacet, FormElementFacet, NumberFacet,
    };
    use crate::testing::{InMemoryDb, RootConfigCtx};

    #[test]
    fn builders_round_trip_primitives() {
        let db = InMemoryDb::new();
        let a = db.number(Some(15), Some(2));
        let b = db.number(Some(15), Some(2));
        assert_eq!(a, b);
        let manual = db.intern_type(TypeKind::Number(NumberFacet::with_scale(15, 2)));
        assert_eq!(a, manual);
    }

    #[test]
    fn builder_unknown_returns_sentinel() {
        let db = InMemoryDb::new();
        let via_builder = <InMemoryDb as Builders>::unknown(&db);
        let via_sentinel_accessor = InMemoryDb::unknown(&db);
        assert_eq!(via_builder, via_sentinel_accessor);
    }

    #[test]
    fn builder_string_with_length() {
        let db = InMemoryDb::new();
        let a = db.string(Some(50), false);
        let b = db.string(Some(50), false);
        assert_eq!(a, b);
        let c = db.string(Some(100), false);
        assert_ne!(a, c);
        let d = db.string(Some(50), true);
        assert_ne!(a, d);
    }

    #[test]
    fn builder_structure_keyless_has_no_fields() {
        let db = InMemoryDb::new();
        let a = db.structure(None);
        let kind = db.lookup_type(a);
        let TypeKind::Structure(facet) = &kind else { panic!("not a structure") };
        assert!(facet.keys.is_none());
        assert!(facet.fields.is_none());
        assert!(facet.origin.is_none());
        assert!(!facet.closed);
    }

    #[test]
    fn builder_structure_typed_round_trips_and_interns() {
        let db = InMemoryDb::new();
        let str_ty = db.string(None, false);
        let num_ty = db.number(None, None);
        let mk = || {
            Arc::new(Projection::new(
                Arc::from([
                    ProjectionField::new(
                        "Город".to_string(),
                        str_ty,
                        ProjectionFieldSource::StructureLiteral,
                    ),
                    ProjectionField::new(
                        "Индекс".to_string(),
                        num_ty,
                        ProjectionFieldSource::StructureLiteral,
                    ),
                ]),
                ProjectionOrigin::StructureLiteral,
                None,
            ))
        };
        let a = db.structure_typed(mk(), TypeOrigin::BslLiteral);
        let b = db.structure_typed(mk(), TypeOrigin::BslLiteral);
        assert_eq!(a, b, "equal projections must intern to the same TypeId");
        assert_ne!(a, db.structure(None), "typed structure differs from untyped");

        let kind = db.lookup_type(a);
        let TypeKind::Structure(facet) = &kind else { panic!("not a structure") };
        let fields = facet.fields.as_ref().expect("typed structure has fields");
        assert_eq!(fields.fields.len(), 2);
        assert_eq!(fields.fields[0].name, "Город");
        assert_eq!(fields.fields[0].ty, str_ty);
        assert_eq!(fields.fields[1].ty, num_ty);
        assert_eq!(facet.origin, Some(TypeOrigin::BslLiteral));
        assert!(!facet.closed, "existing typed builder stays open");
        // `keys` is derived from field order for name-only consumers.
        let keys = facet.keys.as_ref().expect("typed structure derives keys");
        assert_eq!(keys.as_ref(), &["Город".to_string(), "Индекс".to_string()]);

        let closed = db.structure_typed_closed(mk(), TypeOrigin::BslLiteral);
        let TypeKind::Structure(closed_facet) = db.lookup_type(closed) else {
            panic!("not a structure")
        };
        assert!(closed_facet.closed);
        assert_ne!(a, closed, "completeness participates in interning");

        let empty =
            Arc::new(Projection::new(Arc::from([]), ProjectionOrigin::StructureLiteral, None));
        let TypeKind::Structure(empty_facet) =
            db.lookup_type(db.structure_typed_closed(empty, TypeOrigin::BslLiteral))
        else {
            panic!("not a structure")
        };
        assert!(!empty_facet.closed, "an empty structure cannot be closed");
    }

    #[test]
    fn builder_metadata_ref_routes_through_config_ctx() {
        let db = InMemoryDb::new();
        let cfg = RootConfigCtx;
        let cat_a = db.metadata_ref(MetadataKind::CatalogRef, "Номенклатура".to_string(), &cfg);
        let cat_b = db.metadata_ref(MetadataKind::CatalogRef, "Номенклатура".to_string(), &cfg);
        assert_eq!(cat_a, cat_b);

        let cat_c = db.metadata_ref(MetadataKind::CatalogRef, "Контрагенты".to_string(), &cfg);
        assert_ne!(cat_a, cat_c);

        let doc = db.metadata_ref(MetadataKind::DocumentRef, "Номенклатура".to_string(), &cfg);
        assert_ne!(cat_a, doc);

        match db.lookup_type(cat_a) {
            TypeKind::MetadataRef(facet) => {
                assert_eq!(facet.kind, MetadataKind::CatalogRef);
                assert_eq!(facet.name, "Номенклатура");
                assert_eq!(facet.config_id, ConfigId::Root);
            }
            other => panic!("expected MetadataRef; got {:?}", other),
        }
    }

    #[test]
    fn builder_union_canonicalises() {
        let db = InMemoryDb::new();
        let n = db.number(Some(15), Some(2));
        let s = db.string(None, false);
        let u1 = db.union(vec![n, s]);
        let u2 = db.union(vec![s, n]);
        assert_eq!(u1, u2);
    }

    #[test]
    fn builders_dyn_compatible() {
        let db = InMemoryDb::new();
        let dyn_db: &dyn TypeKernelDb = &db;
        let id = dyn_db.number(Some(15), Some(2));
        assert_eq!(dyn_db.lookup_type(id), &TypeKind::Number(NumberFacet::with_scale(15, 2)));
    }

    #[test]
    fn builder_date_round_trip() {
        let db = InMemoryDb::new();
        let a = db.date(DateComponent::DateTime);
        let b = db.date(DateComponent::DateTime);
        assert_eq!(a, b);
        let c = db.date(DateComponent::Date);
        assert_ne!(a, c);
    }

    #[test]
    fn builder_array_with_and_without_element() {
        let db = InMemoryDb::new();
        let n = db.number(None, None);
        let arr_typed = db.array(Some(n));
        let arr_typed_2 = db.array(Some(n));
        let arr_untyped = db.array(None);
        assert_eq!(arr_typed, arr_typed_2);
        assert_ne!(arr_typed, arr_untyped);
    }

    #[test]
    fn builder_form_data_round_trip() {
        let db = InMemoryDb::new();
        let owner =
            MdoRefFacet { mdo_type: MdoType::Catalog, name: "Контрагенты".to_string() };
        let a = db.mk_form_data(FormDataFacet::Structure, Some(owner.clone()));
        let b = db.mk_form_data(FormDataFacet::Structure, Some(owner.clone()));
        assert_eq!(a, b);

        match db.lookup_type(a) {
            TypeKind::FormData { kind, underlying } => {
                assert_eq!(*kind, FormDataFacet::Structure);
                assert_eq!(underlying.as_ref(), Some(&owner));
            }
            other => panic!("expected FormData; got {:?}", other),
        }
    }

    #[test]
    fn builder_form_data_distinguishes_kind_and_underlying() {
        let db = InMemoryDb::new();
        let owner =
            MdoRefFacet { mdo_type: MdoType::Catalog, name: "Контрагенты".to_string() };
        let structure = db.mk_form_data(FormDataFacet::Structure, Some(owner.clone()));
        let collection = db.mk_form_data(FormDataFacet::Collection, Some(owner.clone()));
        let bare_structure = db.mk_form_data(FormDataFacet::Structure, None);

        assert_ne!(structure, collection);
        assert_ne!(structure, bare_structure);
    }

    #[test]
    fn builder_form_control_round_trip() {
        let db = InMemoryDb::new();
        let ty = db.string(Some(30), false);
        let binding = FormBindingFacet {
            path: Arc::from(["Объект".to_string(), "Наименование".to_string()]),
            target: FormBindingTargetFacet::Attribute { ty },
        };
        let id = db.mk_form_control(FormElementFacet::Field, Some(binding.clone()));

        match db.lookup_type(id) {
            TypeKind::FormControl { kind, binding: stored } => {
                assert_eq!(*kind, FormElementFacet::Field);
                assert_eq!(stored.as_ref(), Some(&binding));
            }
            other => panic!("expected FormControl; got {:?}", other),
        }
    }

    #[test]
    fn builder_form_control_distinguishes_kind_and_binding() {
        let db = InMemoryDb::new();
        let owner =
            MdoRefFacet { mdo_type: MdoType::Catalog, name: "Контрагенты".to_string() };
        let binding = FormBindingFacet {
            path: Arc::from(["Объект".to_string(), "Товары".to_string()]),
            target: FormBindingTargetFacet::TabularSection {
                mdo_ref: owner,
                section: "Товары".to_string(),
            },
        };
        let table = db.mk_form_control(FormElementFacet::Table, Some(binding.clone()));
        let field = db.mk_form_control(FormElementFacet::Field, Some(binding));
        let bare_table = db.mk_form_control(FormElementFacet::Table, None);

        assert_ne!(table, field);
        assert_ne!(table, bare_table);
    }

    #[test]
    fn builder_this_object_and_manager_round_trip() {
        let db = InMemoryDb::new();
        let owner = MdoRefFacet { mdo_type: MdoType::Document, name: "Заказ".to_string() };
        let object = db.mk_this_object(ConfigId::Root, owner.clone());
        let manager = db.mk_this_manager(ConfigId::Root, owner.clone());
        assert_ne!(object, manager);

        assert_eq!(
            db.lookup_type(object),
            &TypeKind::ThisObject { config_id: ConfigId::Root, owner: owner.clone() }
        );
        assert_eq!(
            db.lookup_type(manager),
            &TypeKind::ThisManager { config_id: ConfigId::Root, owner }
        );
    }

    #[test]
    fn builder_this_variants_distinguish_config_id() {
        let db = InMemoryDb::new();
        let owner = MdoRefFacet { mdo_type: MdoType::Document, name: "Заказ".to_string() };
        let root = db.mk_this_object(ConfigId::Root, owner.clone());
        let resolved = db.mk_this_object(ConfigId::Resolved(1), owner);

        assert_ne!(root, resolved);
    }

    #[test]
    fn projection_helper_builds_arc_projection() {
        let db = InMemoryDb::new();
        let n = db.number(Some(15), Some(2));
        let s = db.string(None, false);
        let proj = db.projection_from_fields(
            vec![("Цена".to_string(), n), ("Наименование".to_string(), s)],
            ProjectionFieldSource::Column,
            ProjectionOrigin::SdblQuery,
        );
        assert_eq!(proj.fields.len(), 2);
        assert_eq!(proj.fields[0].name, "Цена");
        assert_eq!(proj.fields[1].ty, s);
    }
}
