pub use crate::field_enum::FieldInfo;

use bsl_metadata::{MdoType, ModuleType};
use bsl_types::builders::Builders;
use bsl_types::facet::FormDataFacet;
use bsl_types::intern::TypeKernelDb;
use bsl_types::kind::{MetadataKind, MetadataReferenceKind, TypeId, TypeKind};
use bsl_types::testing::RootConfigCtx;
use hir_def::Name;

use crate::field_enum::enumerate_fields_inner;
use crate::object_resolver::MetadataResolution;

pub(crate) fn project_form_data_for_fields_id(db: &dyn TypeKernelDb, ty: TypeId) -> Option<TypeId> {
    let TypeKind::FormData { kind, underlying: Some(owner) } = db.lookup_type(ty) else {
        return None;
    };
    if !matches!(kind, FormDataFacet::Structure | FormDataFacet::StructureWithCollection) {
        return None;
    }
    let object_kind = MetadataKind::object_kind_for(owner.mdo_type)?;
    Some(db.metadata_ref(object_kind, owner.name.clone(), &RootConfigCtx))
}

fn lookup_form_data_tabular_section_field(
    db: &dyn TypeKernelDb,
    resolver: &dyn MetadataResolution,
    receiver: TypeId,
    field_name: &Name,
) -> Option<FieldInfo> {
    let (mdo_type, mdo_name) = match db.lookup_type(receiver) {
        TypeKind::FormData {
            kind: FormDataFacet::Structure | FormDataFacet::StructureWithCollection,
            underlying: Some(owner),
        } => (owner.mdo_type, owner.name.clone()),
        _ => return None,
    };

    let needle = field_name.as_str();
    let mdo = resolver.resolve_metadata_object(mdo_type, mdo_name.as_str())?;
    let ts = mdo.tabular_sections.iter().find(|ts| {
        stdx::case::eq_ignore_case(ts.name(), needle)
            || ts.name_en().is_some_and(|en| stdx::case::eq_ignore_case(en, needle))
    })?;

    let qualified = format!("{}.{}", mdo_name.as_str(), ts.name());
    let ty = db.mk_form_data(
        FormDataFacet::Collection,
        Some(bsl_types::facet::MdoRefFacet::new(mdo_type, qualified)),
    );
    Some(FieldInfo {
        name: Name::new(ts.name()),
        name_en: ts.name_en().filter(|s| !s.is_empty()).map(Name::new),
        ty,
        value_ty: None,
        is_readonly: false,
        origin: crate::field_enum::FieldOrigin::TabularSection,
    })
}

pub fn lookup_field(
    db: &dyn TypeKernelDb,
    resolver: &dyn MetadataResolution,
    receiver: TypeId,
    field_name: &Name,
) -> Option<FieldInfo> {
    lookup_field_inner(db, resolver, receiver, field_name)
}

/// All lookups — including per-arm union recursion — funnel through here, so
/// the form-data neutralization below applies to a `FormData` union arm too,
/// before the arms' environments merge.
fn lookup_field_inner(
    db: &dyn TypeKernelDb,
    resolver: &dyn MetadataResolution,
    receiver: TypeId,
    field_name: &Name,
) -> Option<FieldInfo> {
    let mut info = lookup_field_raw(db, resolver, receiver, field_name)?;
    // A form-data structure proxies its backing object: a member reached
    // through the projection is exposed by the form wherever the form itself
    // runs, so the backing object page's availability must not be judged
    // against the call site (`Отчет.КомпоновщикНастроек` in client form code
    // is legal even though the raw report object is server-side).
    if let crate::field_enum::FieldOrigin::PlatformProperty { env } = &mut info.origin {
        if project_form_data_for_fields_id(db, receiver).is_some() {
            *env = hir_def::execution_env::EnvFlags::ALL;
        }
    }
    Some(info)
}

fn lookup_field_raw(
    db: &dyn TypeKernelDb,
    resolver: &dyn MetadataResolution,
    receiver: TypeId,
    field_name: &Name,
) -> Option<FieldInfo> {
    if let Some(info) = lookup_field_in_query_projection(db, receiver, field_name) {
        return Some(info);
    }

    if let Some(info) = lookup_form_data_tabular_section_field(db, resolver, receiver, field_name) {
        return Some(info);
    }

    let projected_ty = project_form_data_for_fields_id(db, receiver).unwrap_or(receiver);

    let effective_ty =
        crate::this_object::coerce_to_metadata_ref_id(db, projected_ty).unwrap_or(projected_ty);

    if let Some(info) = lookup_configuration_metadata_field(db, effective_ty, field_name) {
        return Some(info);
    }

    if let TypeKind::MetadataReferenceCollection(kind) = db.lookup_type(effective_ty) {
        return lookup_metadata_reference_member(db, resolver, *kind, field_name);
    }

    if let TypeKind::Structure(facet) = db.lookup_type(effective_ty) {
        // A known literal key resolves here. A miss is classified by inference from the facet's
        // explicit completeness proof; lookup itself stays unchanged for completion/hover callers.
        let field_ty =
            crate::structure_keys::structure_projection_field(facet, field_name.as_str())?;
        // SOFT: only a *nested structure* type is surfaced (so `С.Адрес.` can chain). Scalar / leaf
        // value types are hidden as `Unknown`, because field-access inference feeds `TypeMismatch`
        // — letting a heuristic key value type through would manufacture false positives. Keys and
        // their display types still live on the facet for completion/hover.
        let ty = if matches!(db.lookup_type(field_ty), TypeKind::Structure(_)) {
            field_ty
        } else {
            db.unknown()
        };
        return Some(FieldInfo {
            name: Name::new(field_name.as_str()),
            name_en: None,
            ty,
            value_ty: None,
            is_readonly: false,
            origin: crate::field_enum::FieldOrigin::UserAttribute,
        });
    }

    match db.lookup_type(effective_ty) {
        TypeKind::Union(arms) => {
            let arms = arms.to_vec();
            return lookup_field_in_union_intersection(db, resolver, &arms, field_name);
        }
        TypeKind::MetadataRef(_) => {
            return lookup_field_on_metadata_ref(db, resolver, effective_ty, field_name);
        }
        _ => {}
    }
    if let Some(refined) = crate::form_items::refine_form_control_property(db, receiver, field_name)
    {
        return Some(refined);
    }
    lookup_field_via_platform_property(db, receiver, field_name)
}

fn lookup_configuration_metadata_field(
    db: &dyn TypeKernelDb,
    receiver: TypeId,
    field_name: &Name,
) -> Option<FieldInfo> {
    if !is_configuration_metadata_root(db, receiver) {
        return None;
    }

    // Cold (non-manager) kinds only. Manager plurals (`Метаданные.Справочники`) name a
    // collection of metadata-DESCRIPTION objects (`ОбъектМетаданных`), not the
    // `СправочникМенеджер`, so they must keep falling through to platform-property
    // resolution rather than being typed as a manager collection here.
    let kind = MetadataReferenceKind::from_plural(field_name.as_str())?;
    Some(FieldInfo {
        name: Name::new(kind.russian_plural()),
        name_en: Some(Name::new(kind.english_plural())),
        ty: db.metadata_reference_collection(kind),
        value_ty: None,
        is_readonly: true,
        origin: crate::field_enum::FieldOrigin::MetadataReference,
    })
}

fn is_configuration_metadata_root(db: &dyn TypeKernelDb, receiver: TypeId) -> bool {
    let TypeKind::PlatformObject(facet) = db.lookup_type(receiver) else {
        return false;
    };
    matches!(facet.name.as_str(), "ОбъектМетаданныхКонфигурация" | "ConfigurationMetadataObject")
}

fn lookup_metadata_reference_member(
    db: &dyn TypeKernelDb,
    resolver: &dyn MetadataResolution,
    kind: MetadataReferenceKind,
    field_name: &Name,
) -> Option<FieldInfo> {
    let name = resolver.resolve_metadata_reference(kind, field_name.as_str())?;
    Some(FieldInfo {
        ty: db.metadata_reference(kind, name.as_str().to_string()),
        name,
        name_en: None,
        value_ty: None,
        is_readonly: true,
        origin: crate::field_enum::FieldOrigin::MetadataReference,
    })
}

fn lookup_field_in_union_intersection(
    db: &dyn TypeKernelDb,
    resolver: &dyn MetadataResolution,
    arms: &[TypeId],
    field_name: &Name,
) -> Option<FieldInfo> {
    let live: Vec<TypeId> = arms
        .iter()
        .copied()
        .filter(|t| !matches!(db.lookup_type(*t), TypeKind::Undefined | TypeKind::Null))
        .collect();
    if live.is_empty() {
        return None;
    }
    if live.len() == 1 {
        return lookup_field_inner(db, resolver, live[0], field_name);
    }
    let mut per_arm: Vec<FieldInfo> = Vec::with_capacity(live.len());
    for arm in &live {
        let info = lookup_field_inner(db, resolver, *arm, field_name)?;
        per_arm.push(info);
    }
    let first = &per_arm[0];
    let merged_ty = db.union(per_arm.iter().map(|f| f.ty).collect());
    let merged_readonly = per_arm.iter().any(|f| f.is_readonly);
    // The member is available wherever ANY arm provides it, so platform
    // availability unites across arms; one non-platform arm (a user
    // attribute) makes it unrestricted — taking the first arm's origin
    // verbatim would make the verdict depend on arm order.
    let merged_origin = if per_arm
        .iter()
        .all(|f| matches!(f.origin, crate::field_enum::FieldOrigin::PlatformProperty { .. }))
    {
        let mut env = hir_def::execution_env::EnvFlags::EMPTY;
        for f in &per_arm {
            if let crate::field_enum::FieldOrigin::PlatformProperty { env: arm_env } = f.origin {
                env = env | arm_env;
            }
        }
        crate::field_enum::FieldOrigin::PlatformProperty { env }
    } else {
        per_arm
            .iter()
            .map(|f| f.origin)
            .find(|o| !matches!(o, crate::field_enum::FieldOrigin::PlatformProperty { .. }))
            .unwrap_or(first.origin)
    };
    Some(FieldInfo {
        name: first.name.clone(),
        name_en: first.name_en.clone(),
        ty: merged_ty,
        value_ty: None,
        is_readonly: merged_readonly,
        origin: merged_origin,
    })
}

fn lookup_field_on_metadata_ref(
    db: &dyn TypeKernelDb,
    resolver: &dyn MetadataResolution,
    effective_ty: TypeId,
    field_name: &Name,
) -> Option<FieldInfo> {
    let needle = field_name.as_str();
    let metadata_field = enumerate_fields_inner(db, resolver, effective_ty).into_iter().find(|f| {
        stdx::case::eq_ignore_case(f.name.as_str(), needle)
            || f.name_en.as_ref().is_some_and(|en| stdx::case::eq_ignore_case(en.as_str(), needle))
    });
    if metadata_field.is_some() {
        return metadata_field;
    }

    let TypeKind::MetadataRef(facet) = db.lookup_type(effective_ty) else { return None };
    let mdo_type = match facet.kind {
        MetadataKind::CatalogObject => MdoType::Catalog,
        MetadataKind::DocumentObject => MdoType::Document,
        MetadataKind::ExchangePlanObject => MdoType::ExchangePlan,
        MetadataKind::ChartOfAccountsObject => MdoType::ChartOfAccounts,
        MetadataKind::TaskObject => MdoType::Task,
        MetadataKind::BusinessProcessObject => MdoType::BusinessProcess,
        MetadataKind::DataProcessorObject => MdoType::DataProcessor,
        MetadataKind::ReportObject => MdoType::Report,
        MetadataKind::ExternalDataProcessorObject => MdoType::ExternalDataProcessor,
        MetadataKind::ExternalReportObject => MdoType::ExternalReport,
        MetadataKind::ChartOfCharacteristicTypesObject => MdoType::ChartOfCharacteristicTypes,
        MetadataKind::ChartOfCalculationTypesObject => MdoType::ChartOfCalculationTypes,
        _ => return None,
    };
    resolver
        .has_effective_module_variable(
            ModuleType::ObjectModule,
            mdo_type,
            facet.name.as_str(),
            needle,
        )
        .then(|| FieldInfo {
            name: field_name.clone(),
            name_en: None,
            ty: db.unknown(),
            value_ty: None,
            is_readonly: false,
            origin: crate::field_enum::FieldOrigin::UserAttribute,
        })
}

fn lookup_field_in_query_projection(
    db: &dyn TypeKernelDb,
    receiver: TypeId,
    field_name: &Name,
) -> Option<FieldInfo> {
    // Same split as `enumerate_projection_fields`: a selection cursor is read-only, the
    // columns of an unloaded value-table row are not.
    let (projection, is_readonly) = match db.lookup_type(receiver) {
        TypeKind::QueryResultSelection(facet) => (facet.projection.clone()?, true),
        TypeKind::ValueTableRow(facet) => (facet.projection.clone()?, false),
        _ => return None,
    };
    let needle = field_name.as_str();
    projection.fields.iter().find(|f| stdx::case::eq_ignore_case(f.name.as_str(), needle)).map(
        |f| FieldInfo {
            name: Name::new(f.name.as_str()),
            name_en: None,
            ty: f.ty,
            value_ty: None,
            is_readonly,
            origin: crate::field_enum::FieldOrigin::UserAttribute,
        },
    )
}

fn lookup_field_via_platform_property(
    db: &dyn TypeKernelDb,
    receiver: TypeId,
    field_name: &Name,
) -> Option<FieldInfo> {
    let res = crate::platform_property_lookup::lookup_platform_property(db, receiver, field_name)?;
    Some(FieldInfo {
        name: field_name.clone(),
        name_en: None,
        ty: res.return_ty,
        value_ty: None,
        is_readonly: res.is_readonly,
        origin: crate::field_enum::FieldOrigin::PlatformProperty { env: res.env },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object_resolver::ConfigsObjectResolver;
    use bsl_config::VisibleConfig;
    use bsl_types::builders::Builders;
    use bsl_types::facet::ProjectionSource;
    use bsl_types::kind::TypeKind;
    use bsl_types::testing::{InMemoryDb, RootConfigCtx};
    use std::rc::Rc;

    #[derive(Clone)]
    struct FieldInfoForTest {
        ty: ActualType,
        is_readonly: bool,
    }

    #[derive(Clone)]
    struct ActualType {
        db: Rc<InMemoryDb>,
        id: TypeId,
    }

    impl std::fmt::Debug for ActualType {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            self.db.lookup_type(self.id).fmt(f)
        }
    }

    #[derive(Clone)]
    struct TypeFixture {
        label: String,
        intern: Rc<dyn Fn(&InMemoryDb) -> TypeId>,
    }

    impl TypeFixture {
        fn new(label: impl Into<String>, intern: impl Fn(&InMemoryDb) -> TypeId + 'static) -> Self {
            Self { label: label.into(), intern: Rc::new(intern) }
        }

        fn intern(&self, db: &InMemoryDb) -> TypeId {
            (self.intern)(db)
        }
    }

    impl std::fmt::Debug for TypeFixture {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(&self.label)
        }
    }

    impl PartialEq<TypeFixture> for ActualType {
        fn eq(&self, other: &TypeFixture) -> bool {
            self.id == other.intern(&self.db)
        }
    }

    impl PartialEq for ActualType {
        fn eq(&self, other: &Self) -> bool {
            self.db.lookup_type(self.id) == other.db.lookup_type(other.id)
        }
    }

    fn lookup_field(
        configs: &[VisibleConfig],
        receiver_ty: &TypeFixture,
        field_name: &Name,
    ) -> Option<FieldInfoForTest> {
        let db = Rc::new(InMemoryDb::new());
        let receiver = receiver_ty.intern(&db);
        super::lookup_field(db.as_ref(), &ConfigsObjectResolver(configs), receiver, field_name).map(
            |info| FieldInfoForTest {
                ty: ActualType { db: Rc::clone(&db), id: info.ty },
                is_readonly: info.is_readonly,
            },
        )
    }
    use bsl_metadata::tabular_section::{TabularSection, TabularSectionAttribute};
    use bsl_metadata::{Attribute, AttributeType, Configuration, MdoType, MetadataObject};
    use hir_def::ty::MetadataKind;
    use std::sync::Arc;
    use uuid::Uuid;

    fn metadata_ref(kind: MetadataKind, name: &str) -> TypeFixture {
        let name = Name::new(name);
        TypeFixture::new(format!("MetadataRef({kind:?}, {name})"), move |db| {
            db.metadata_ref(kind, name.to_string(), &RootConfigCtx)
        })
    }

    fn platform_object(name: &str) -> TypeFixture {
        let name = Name::new(name);
        TypeFixture::new(format!("PlatformObject({name})"), move |db| {
            db.platform_object(name.to_string())
        })
    }

    fn union(parts: Vec<TypeFixture>) -> TypeFixture {
        TypeFixture::new("Union", move |db| {
            db.union(parts.iter().map(|part| part.intern(db)).collect())
        })
    }

    fn number() -> TypeFixture {
        TypeFixture::new("Number", |db| db.number(None, None))
    }

    fn string() -> TypeFixture {
        TypeFixture::new("String", |db| db.string(None, false))
    }

    fn boolean() -> TypeFixture {
        TypeFixture::new("Boolean", |db| db.boolean())
    }

    fn date() -> TypeFixture {
        TypeFixture::new("Date", |db| db.date(bsl_types::facet::DateComponent::DateTime))
    }

    fn structure() -> TypeFixture {
        TypeFixture::new("Structure", |db| db.structure(None))
    }

    fn array() -> TypeFixture {
        TypeFixture::new("Array", |db| db.array(None))
    }

    fn unknown() -> TypeFixture {
        TypeFixture::new("Unknown", |db| db.unknown())
    }

    fn undefined() -> TypeFixture {
        TypeFixture::new("Undefined", |db| db.undefined())
    }

    fn wrap(config: Configuration) -> Vec<VisibleConfig> {
        vec![VisibleConfig { name: None, configuration: Arc::new(config) }]
    }

    fn attr(name: &str, name_en: Option<&str>, attr_type: AttributeType) -> Attribute {
        Attribute { name: name.to_string(), name_en: name_en.map(String::from), attr_type }
    }

    fn catalog(name: &str, attrs: Vec<Attribute>) -> MetadataObject {
        let mut mdo = MetadataObject::new(MdoType::Catalog, name);
        for a in attrs {
            mdo.add_attribute(a);
        }
        mdo
    }

    fn document(name: &str, attrs: Vec<Attribute>) -> MetadataObject {
        let mut mdo = MetadataObject::new(MdoType::Document, name);
        for a in attrs {
            mdo.add_attribute(a);
        }
        mdo
    }

    fn mdo_of(mdo_type: MdoType, name: &str, attrs: Vec<Attribute>) -> MetadataObject {
        let mut mdo = MetadataObject::new(mdo_type, name);
        for a in attrs {
            mdo.add_attribute(a);
        }
        mdo
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn form_data_projection_neutralizes_platform_property_env() {
        let config = {
            let mut c = Configuration::new("Test");
            c.add_metadata_object(mdo_of(MdoType::Report, "Продажи", vec![]));
            c
        };
        let configs = wrap(config);

        let receiver = TypeFixture::new("form_data(Отчет)", |db| {
            db.mk_form_data(
                FormDataFacet::Structure,
                Some(bsl_types::facet::MdoRefFacet::new(MdoType::Report, "Продажи".to_string())),
            )
        });

        let db = Rc::new(InMemoryDb::new());
        let receiver_id = receiver.intern(&db);
        let info = super::lookup_field(
            db.as_ref(),
            &ConfigsObjectResolver(&configs),
            receiver_id,
            &Name::new("КомпоновщикНастроек"),
        )
        .expect("projected report property must resolve");
        match info.origin {
            crate::field_enum::FieldOrigin::PlatformProperty { env } => assert_eq!(
                env,
                hir_def::execution_env::EnvFlags::ALL,
                "the form-data proxy is available wherever the form runs"
            ),
            ref other => panic!("expected a platform property origin, got {other:?}"),
        }
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn form_data_union_arm_also_neutralizes_platform_property_env() {
        let config = {
            let mut c = Configuration::new("Test");
            c.add_metadata_object(mdo_of(MdoType::Report, "Продажи", vec![]));
            c
        };
        let configs = wrap(config);

        let db = Rc::new(InMemoryDb::new());
        let form_data = db.mk_form_data(
            FormDataFacet::Structure,
            Some(bsl_types::facet::MdoRefFacet::new(MdoType::Report, "Продажи".to_string())),
        );
        let receiver = db.union(vec![form_data, db.undefined()]);
        let info = super::lookup_field(
            db.as_ref(),
            &ConfigsObjectResolver(&configs),
            receiver,
            &Name::new("КомпоновщикНастроек"),
        )
        .expect("union with a projected form-data arm must resolve");
        match info.origin {
            crate::field_enum::FieldOrigin::PlatformProperty { env } => assert_eq!(
                env,
                hir_def::execution_env::EnvFlags::ALL,
                "the union arm must be neutralized before the merge"
            ),
            ref other => panic!("expected a platform property origin, got {other:?}"),
        }
    }

    #[test]
    fn field_lookup_mdo_attribute_exchange_plan_and_chart_of_accounts() {
        let mut config = Configuration::new("Test");
        config.add_metadata_object(mdo_of(
            MdoType::ExchangePlan,
            "Контрагенты",
            vec![attr("Признак", None, AttributeType::Boolean)],
        ));
        config.add_metadata_object(mdo_of(
            MdoType::ChartOfAccounts,
            "Хозрасчетный",
            vec![attr("Порядок", None, AttributeType::Number { precision: 15, scale: 0 })],
        ));
        let configs = wrap(config);

        let ep_info = lookup_field(
            &configs,
            &metadata_ref(MetadataKind::ExchangePlanRef, "Контрагенты"),
            &Name::new("Признак"),
        )
        .expect("ExchangePlanRef.Признак resolves");
        assert_eq!(ep_info.ty, boolean());

        let coa_info = lookup_field(
            &configs,
            &metadata_ref(MetadataKind::ChartOfAccountsRef, "Хозрасчетный"),
            &Name::new("Порядок"),
        )
        .expect("ChartOfAccountsRef.Порядок resolves");
        assert_eq!(coa_info.ty, number());
    }

    #[test]
    fn field_lookup_mdo_attribute_catalog() {
        let mut config = Configuration::new("Test");
        config.add_metadata_object(catalog(
            "Номенклатура",
            vec![attr("Цена", None, AttributeType::Number { precision: 15, scale: 2 })],
        ));
        let configs = wrap(config);

        let receiver = metadata_ref(MetadataKind::CatalogRef, "Номенклатура");
        let info = lookup_field(&configs, &receiver, &Name::new("Цена"))
            .expect("Цена resolves on Номенклатура");
        assert_eq!(info.ty, number());
    }

    #[test]
    fn field_lookup_standard_attribute_code() {
        let mut config = Configuration::new("Test");
        config.add_metadata_object(catalog(
            "Номенклатура",
            vec![attr("Код", Some("Code"), AttributeType::String { length: Some(9) })],
        ));
        let configs = wrap(config);

        let receiver = metadata_ref(MetadataKind::CatalogRef, "Номенклатура");
        let info = lookup_field(&configs, &receiver, &Name::new("Код"))
            .expect("standard Code attribute resolves");
        assert_eq!(info.ty, string());

        let info_en = lookup_field(&configs, &receiver, &Name::new("Code"))
            .expect("Code (en) resolves through bilingual match");
        assert_eq!(info_en.ty, string());
    }

    #[test]
    fn field_lookup_tabular_section() {
        let mut ts = TabularSection::new(Uuid::new_v4(), "Товары");
        ts.set_attributes(vec![TabularSectionAttribute::new(
            Uuid::new_v4(),
            "Количество",
            AttributeType::Number { precision: 15, scale: 3 },
        )]);
        let mut doc = document("ПКО", vec![]);
        doc.add_tabular_section(ts);

        let mut config = Configuration::new("Test");
        config.add_metadata_object(doc);
        let configs = wrap(config);

        let receiver = metadata_ref(MetadataKind::DocumentRef, "ПКО");
        let info = lookup_field(&configs, &receiver, &Name::new("Товары"))
            .expect("tabular section name resolves to TabularSection Ty");
        assert_eq!(
            info.ty,
            metadata_ref(MetadataKind::TabularSection { parent: MdoType::Document }, "ПКО.Товары")
        );
    }

    #[test]
    fn field_lookup_tabular_row_attribute() {
        let mut ts = TabularSection::new(Uuid::new_v4(), "Товары");
        ts.set_attributes(vec![TabularSectionAttribute::new(
            Uuid::new_v4(),
            "Количество",
            AttributeType::Number { precision: 15, scale: 3 },
        )]);
        let mut doc = document("ПКО", vec![]);
        doc.add_tabular_section(ts);

        let mut config = Configuration::new("Test");
        config.add_metadata_object(doc);
        let configs = wrap(config);

        let receiver = metadata_ref(
            MetadataKind::TabularSectionRow { parent: MdoType::Document },
            "ПКО.Товары",
        );
        let info = lookup_field(&configs, &receiver, &Name::new("Количество"))
            .expect("row attribute Количество resolves to Number");
        assert_eq!(info.ty, number());
    }

    #[test]
    fn field_lookup_same_name_catalog_and_document_disambiguated_by_parent() {
        let make_ts = |attr_type: AttributeType| {
            let mut ts = TabularSection::new(Uuid::new_v4(), "Товары");
            ts.set_attributes(vec![TabularSectionAttribute::new(
                Uuid::new_v4(),
                "Количество",
                attr_type,
            )]);
            ts
        };

        let mut cat = catalog("X", vec![]);
        cat.add_tabular_section(make_ts(AttributeType::String { length: Some(10) }));
        let mut doc = document("X", vec![]);
        doc.add_tabular_section(make_ts(AttributeType::Number { precision: 15, scale: 3 }));

        let mut config = Configuration::new("Test");
        config.add_metadata_object(cat);
        config.add_metadata_object(doc);
        let configs = wrap(config);

        let cat_row =
            metadata_ref(MetadataKind::TabularSectionRow { parent: MdoType::Catalog }, "X.Товары");
        let doc_row =
            metadata_ref(MetadataKind::TabularSectionRow { parent: MdoType::Document }, "X.Товары");
        assert_eq!(
            lookup_field(&configs, &cat_row, &Name::new("Количество")).unwrap().ty,
            string(),
            "Catalog row must resolve via its own tabular section",
        );
        assert_eq!(
            lookup_field(&configs, &doc_row, &Name::new("Количество")).unwrap().ty,
            number(),
            "Document row must resolve via its own tabular section — not Catalog's",
        );
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn field_lookup_tabular_row_line_number_resolves_via_platform() {
        let ts = TabularSection::new(Uuid::new_v4(), "Услуги");
        let mut cat = catalog("Номенклатура", vec![]);
        cat.add_tabular_section(ts);
        let mut config = Configuration::new("Test");
        config.add_metadata_object(cat);
        let configs = wrap(config);

        let receiver = metadata_ref(
            MetadataKind::TabularSectionRow { parent: MdoType::Catalog },
            "Номенклатура.Услуги",
        );
        let info = lookup_field(&configs, &receiver, &Name::new("НомерСтроки"))
            .expect("НомерСтроки resolves through platform property fall-through");
        assert_eq!(info.ty, number());
    }

    #[test]
    fn field_lookup_tabular_row_custom_attribute_wins_over_platform() {
        let mut ts = TabularSection::new(Uuid::new_v4(), "Услуги");
        ts.set_attributes(vec![TabularSectionAttribute::new(
            Uuid::new_v4(),
            "НомерСтроки",
            AttributeType::String { length: Some(36) },
        )]);
        let mut cat = catalog("Номенклатура", vec![]);
        cat.add_tabular_section(ts);
        let mut config = Configuration::new("Test");
        config.add_metadata_object(cat);
        let configs = wrap(config);

        let receiver = metadata_ref(
            MetadataKind::TabularSectionRow { parent: MdoType::Catalog },
            "Номенклатура.Услуги",
        );
        let info = lookup_field(&configs, &receiver, &Name::new("НомерСтроки"))
            .expect("custom attribute named НомерСтроки must still resolve");
        assert_eq!(
            info.ty,
            string(),
            "custom XML attribute must win over the platform standard row property",
        );
    }

    #[test]
    fn field_lookup_unknown_receiver_returns_none() {
        let configs = wrap(Configuration::new("Test"));
        for ty in
            [unknown(), number(), string(), array(), undefined(), union(vec![number(), string()])]
        {
            assert!(
                lookup_field(&configs, &ty, &Name::new("Любой")).is_none(),
                "no field lookup on {ty:?}"
            );
        }
    }

    #[test]
    fn field_lookup_missing_attribute_returns_none() {
        let mut config = Configuration::new("Test");
        config.add_metadata_object(catalog("Номенклатура", vec![]));
        let configs = wrap(config);

        let receiver = metadata_ref(MetadataKind::CatalogRef, "Номенклатура");
        assert!(lookup_field(&configs, &receiver, &Name::new("НесуществующееПоле")).is_none());
    }

    #[test]
    fn field_lookup_register_missing_in_config_returns_none() {
        let configs = wrap(Configuration::new("Test"));
        let r = metadata_ref(MetadataKind::AccumulationRegisterRef, "ТоварыНаСкладах");
        assert!(lookup_field(&configs, &r, &Name::new("Количество")).is_none());
    }

    fn register_with(
        name: &str,
        mdo_type: MdoType,
        dimensions: Vec<bsl_metadata::dimension::Dimension>,
        resources: Vec<bsl_metadata::register::RegisterResource>,
        attributes: Vec<bsl_metadata::register::RegisterAttribute>,
    ) -> bsl_metadata::Register {
        let mut builder = bsl_metadata::Register::builder().name(name).mdo_type(mdo_type);
        for d in dimensions {
            builder = builder.add_dimension(d);
        }
        for r in resources {
            builder = builder.add_resource(r);
        }
        for a in attributes {
            builder = builder.add_attribute(a);
        }
        builder.build()
    }

    fn dimension_typed(name: &str, attr_type: AttributeType) -> bsl_metadata::dimension::Dimension {
        let mut d = bsl_metadata::dimension::Dimension::builder().name(name).build();
        d.set_attr_type(attr_type);
        d
    }

    fn resource_typed(
        name: &str,
        attr_type: AttributeType,
    ) -> bsl_metadata::register::RegisterResource {
        let mut r = bsl_metadata::register::RegisterResource::new(Uuid::new_v4(), name);
        r.set_attr_type(attr_type);
        r
    }

    fn attribute_typed(
        name: &str,
        attr_type: AttributeType,
    ) -> bsl_metadata::register::RegisterAttribute {
        let mut a = bsl_metadata::register::RegisterAttribute::new(Uuid::new_v4(), name);
        a.set_attr_type(attr_type);
        a
    }

    #[test]
    fn field_lookup_register_dimension_typed_returns_lowered_ty() {
        let mut config = Configuration::new("Test");
        config.add_register(register_with(
            "РегистрСведений1",
            MdoType::InformationRegister,
            vec![dimension_typed(
                "Справочник1",
                AttributeType::Ref {
                    mdo_type: MdoType::Catalog, name: "Справочник1".into()
                },
            )],
            vec![],
            vec![],
        ));
        let configs = wrap(config);

        let receiver = metadata_ref(MetadataKind::InformationRegisterRef, "РегистрСведений1");
        let info = lookup_field(&configs, &receiver, &Name::new("Справочник1"))
            .expect("dimension resolves against Configuration.registers");
        assert_eq!(
            info.ty,
            metadata_ref(MetadataKind::CatalogRef, "Справочник1"),
            "typed dimension must lower through TyLoweringContext to a concrete MetadataRef",
        );
    }

    #[test]
    fn field_lookup_register_resource_typed_on_accumulation() {
        let mut config = Configuration::new("Test");
        config.add_register(register_with(
            "ТоварыНаСкладах",
            MdoType::AccumulationRegister,
            vec![],
            vec![resource_typed("Количество", AttributeType::Number { precision: 15, scale: 3 })],
            vec![],
        ));
        let configs = wrap(config);

        let receiver = metadata_ref(MetadataKind::AccumulationRegisterRecordSet, "ТоварыНаСкладах");
        let info = lookup_field(&configs, &receiver, &Name::new("Количество"))
            .expect("resource resolves against Configuration.registers");
        assert_eq!(info.ty, number());
    }

    #[test]
    fn field_lookup_register_attribute_typed_on_information() {
        let mut config = Configuration::new("Test");
        config.add_register(register_with(
            "РегистрСведений1",
            MdoType::InformationRegister,
            vec![],
            vec![],
            vec![attribute_typed("Комментарий", AttributeType::String { length: Some(100) })],
        ));
        let configs = wrap(config);

        let receiver =
            metadata_ref(MetadataKind::InformationRegisterRecordManager, "РегистрСведений1");
        let info = lookup_field(&configs, &receiver, &Name::new("Комментарий"))
            .expect("attribute resolves against Configuration.registers");
        assert_eq!(info.ty, string());
    }

    #[test]
    fn field_lookup_register_untyped_part_returns_symbolic_fallback() {
        let mut config = Configuration::new("Test");
        config.add_register(register_with(
            "РегистрСведений1",
            MdoType::InformationRegister,
            vec![bsl_metadata::dimension::Dimension::builder().name("Справочник1").build()],
            vec![],
            vec![],
        ));
        let configs = wrap(config);

        let receiver = metadata_ref(MetadataKind::InformationRegisterRef, "РегистрСведений1");
        let info = lookup_field(&configs, &receiver, &Name::new("Справочник1"))
            .expect("untyped dimension still resolves with symbolic fallback");
        assert_eq!(
            info.ty,
            metadata_ref(
                MetadataKind::RegisterDimension { parent: MdoType::InformationRegister },
                "РегистрСведений1.Справочник1"
            ),
            "fallback must carry parent flavour + `Register.Part` name for provenance",
        );
    }

    #[test]
    fn field_lookup_register_all_four_flavours_resolve() {
        let mut config = Configuration::new("Test");
        config.add_register(register_with(
            "РегСвед",
            MdoType::InformationRegister,
            vec![],
            vec![resource_typed("R", AttributeType::Number { precision: 15, scale: 0 })],
            vec![],
        ));
        config.add_register(register_with(
            "РегНак",
            MdoType::AccumulationRegister,
            vec![],
            vec![resource_typed("R", AttributeType::Number { precision: 15, scale: 0 })],
            vec![],
        ));
        config.add_register(register_with(
            "РегБух",
            MdoType::AccountingRegister,
            vec![],
            vec![resource_typed("R", AttributeType::Number { precision: 15, scale: 0 })],
            vec![],
        ));
        config.add_register(register_with(
            "РегРасч",
            MdoType::CalculationRegister,
            vec![],
            vec![resource_typed("R", AttributeType::Number { precision: 15, scale: 0 })],
            vec![],
        ));
        let configs = wrap(config);

        let cases = [
            (MetadataKind::InformationRegisterRef, "РегСвед"),
            (MetadataKind::AccumulationRegisterRef, "РегНак"),
            (MetadataKind::AccountingRegisterRef, "РегБух"),
            (MetadataKind::CalculationRegisterRef, "РегРасч"),
        ];
        for (kind, name) in cases {
            let receiver = metadata_ref(kind, name);
            let info = lookup_field(&configs, &receiver, &Name::new("R"))
                .unwrap_or_else(|| panic!("resource R must resolve on {kind:?}/{name}"));
            assert_eq!(info.ty, number(), "{kind:?}/{name}.R must lower to Ty::Number");
        }
    }

    #[test]
    fn field_lookup_register_leaf_parts_have_no_field_surface() {
        let configs = wrap(Configuration::new("Test"));
        for kind in [
            MetadataKind::RegisterDimension { parent: MdoType::InformationRegister },
            MetadataKind::RegisterResource { parent: MdoType::AccumulationRegister },
            MetadataKind::RegisterAttribute { parent: MdoType::CalculationRegister },
        ] {
            let receiver = metadata_ref(kind, "РегистрСведений1.Справочник1");
            assert!(
                lookup_field(&configs, &receiver, &Name::new("ЛюбоеПоле")).is_none(),
                "leaf part kind {kind:?} must not expose a field surface",
            );
        }
    }

    #[test]
    fn field_lookup_register_wrong_flavour_returns_none() {
        let mut config = Configuration::new("Test");
        config.add_register(register_with(
            "X",
            MdoType::InformationRegister,
            vec![],
            vec![resource_typed("R", AttributeType::Number { precision: 15, scale: 0 })],
            vec![],
        ));
        let configs = wrap(config);

        let wrong_flavour_receiver = metadata_ref(MetadataKind::AccumulationRegisterRef, "X");
        assert!(
            lookup_field(&configs, &wrong_flavour_receiver, &Name::new("R")).is_none(),
            "AccumulationRegisterRef must not resolve against an InformationRegister even with the same name",
        );
    }

    #[test]
    fn field_lookup_information_register_record_set_synthesizes_filter() {
        let mut config = Configuration::new("Test");
        config.add_register(register_with(
            "Курсы",
            MdoType::InformationRegister,
            vec![dimension_typed(
                "Валюта",
                AttributeType::Ref { mdo_type: MdoType::Catalog, name: "Валюты".into() },
            )],
            vec![],
            vec![],
        ));
        let configs = wrap(config);

        let receiver = metadata_ref(MetadataKind::InformationRegisterRecordSet, "Курсы");
        let info = lookup_field(&configs, &receiver, &Name::new("Отбор"))
            .expect("synthetic .Отбор must resolve on InformationRegisterRecordSet");
        assert_eq!(
            info.ty,
            metadata_ref(
                MetadataKind::RegisterFilter { parent: MdoType::InformationRegister },
                "Курсы"
            ),
        );

        let info_en = lookup_field(&configs, &receiver, &Name::new("Filter"))
            .expect("English alias `.Filter` must resolve too");
        assert_eq!(info_en.ty, info.ty);
    }

    #[test]
    fn recorder_subordinate_information_register_exposes_recorder_filter_only_when_enabled() {
        let receiver = metadata_ref(MetadataKind::InformationRegisterRecordSet, "Регистр");
        let filter = metadata_ref(
            MetadataKind::RegisterFilter { parent: MdoType::InformationRegister },
            "Регистр",
        );

        let mut subordinate_config = Configuration::new("Subordinate");
        subordinate_config.add_register(
            bsl_metadata::Register::builder()
                .name("Регистр")
                .mdo_type(MdoType::InformationRegister)
                .recorder_subordinate(true)
                .build(),
        );
        let subordinate = wrap(subordinate_config);
        let filter_field = lookup_field(&subordinate, &receiver, &Name::new("Отбор"))
            .expect("record set must expose its filter");
        assert_eq!(filter_field.ty, filter);
        assert_eq!(
            lookup_field(&subordinate, &filter, &Name::new("Регистратор"))
                .expect("RecorderSubordinate enables the filter key")
                .ty,
            platform_object("ЭлементОтбора"),
        );

        let mut ordinary_config = Configuration::new("Ordinary");
        ordinary_config.add_register(
            bsl_metadata::Register::builder()
                .name("Регистр")
                .mdo_type(MdoType::InformationRegister)
                .build(),
        );
        let ordinary = wrap(ordinary_config);
        assert!(
            lookup_field(&ordinary, &filter, &Name::new("Регистратор")).is_none(),
            "an ordinary non-periodic information register has no Recorder filter key"
        );
    }

    #[test]
    fn field_lookup_register_filter_dimension_resolves_as_filter_item() {
        let mut config = Configuration::new("Test");
        config.add_register(register_with(
            "Курсы",
            MdoType::InformationRegister,
            vec![dimension_typed(
                "Валюта",
                AttributeType::Ref { mdo_type: MdoType::Catalog, name: "Валюты".into() },
            )],
            vec![resource_typed("Курс", AttributeType::Number { precision: 15, scale: 4 })],
            vec![],
        ));
        let configs = wrap(config);

        let filter_receiver = metadata_ref(
            MetadataKind::RegisterFilter { parent: MdoType::InformationRegister },
            "Курсы",
        );
        let dim_info = lookup_field(&configs, &filter_receiver, &Name::new("Валюта"))
            .expect("dimension Валюта resolves through the synthetic Filter receiver");
        assert_eq!(
            dim_info.ty,
            platform_object("ЭлементОтбора"),
            "Filter members must lower to platform `ЭлементОтбора` so FilterItem methods apply",
        );

        assert!(
            lookup_field(&configs, &filter_receiver, &Name::new("Курс")).is_none(),
            "resources must not appear as Filter members",
        );
    }

    #[test]
    fn field_lookup_register_filter_dim_named_otbor_loses_to_synthetic() {
        let mut config = Configuration::new("Test");
        config.add_register(register_with(
            "РегСвед",
            MdoType::InformationRegister,
            vec![dimension_typed("Отбор", AttributeType::String { length: Some(50) })],
            vec![],
            vec![],
        ));
        let configs = wrap(config);

        let receiver = metadata_ref(MetadataKind::InformationRegisterRecordSet, "РегСвед");
        let info = lookup_field(&configs, &receiver, &Name::new("Отбор"))
            .expect("synthetic .Отбор must win over a same-named dimension");
        assert_eq!(
            info.ty,
            metadata_ref(
                MetadataKind::RegisterFilter { parent: MdoType::InformationRegister },
                "РегСвед"
            ),
            "synthetic Filter wins over a register dimension named `Отбор`",
        );

        let filter_receiver = metadata_ref(
            MetadataKind::RegisterFilter { parent: MdoType::InformationRegister },
            "РегСвед",
        );
        let dim_via_filter = lookup_field(&configs, &filter_receiver, &Name::new("Отбор"))
            .expect("dimension stays reachable as <recordSet>.Отбор.Отбор");
        assert_eq!(dim_via_filter.ty, platform_object("ЭлементОтбора"));
    }

    #[test]
    fn field_lookup_register_filter_unknown_dimension_returns_none() {
        let mut config = Configuration::new("Test");
        config.add_register(register_with(
            "РегСвед",
            MdoType::InformationRegister,
            vec![dimension_typed("Валюта", AttributeType::String { length: Some(3) })],
            vec![],
            vec![],
        ));
        let configs = wrap(config);

        let filter_receiver = metadata_ref(
            MetadataKind::RegisterFilter { parent: MdoType::InformationRegister },
            "РегСвед",
        );
        assert!(
            lookup_field(&configs, &filter_receiver, &Name::new("НетТакогоИзмерения")).is_none(),
            "unknown dimension on Filter receiver must return None",
        );
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn field_lookup_information_register_record_set_pulls_platform_properties() {
        let mut config = Configuration::new("Test");
        config.add_register(register_with(
            "Курсы",
            MdoType::InformationRegister,
            vec![dimension_typed("Валюта", AttributeType::String { length: Some(3) })],
            vec![],
            vec![],
        ));
        let configs = wrap(config);

        let receiver = metadata_ref(MetadataKind::InformationRegisterRecordSet, "Курсы");
        for prop in ["Записывать", "ДополнительныеСвойства", "ЗаписьИсторииДанных"]
        {
            assert!(
                lookup_field(&configs, &receiver, &Name::new(prop)).is_some(),
                "platform property `{prop}` must surface on InformationRegisterRecordSet",
            );
        }
        assert!(
            lookup_field(&configs, &receiver, &Name::new("Write")).is_some(),
            "English alias `Write` must resolve via bilingual rsplit on english_name",
        );

        assert!(
            lookup_field(&configs, &receiver, &Name::new("БлокироватьДляИзменения")).is_none(),
            "Accounting-only property must not leak into InformationRegister surface",
        );
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn field_lookup_accounting_register_record_set_has_lock_for_update() {
        let mut config = Configuration::new("Test");
        config.add_register(register_with(
            "Хозрасчетный",
            MdoType::AccountingRegister,
            vec![],
            vec![],
            vec![],
        ));
        let configs = wrap(config);

        let receiver = metadata_ref(MetadataKind::AccountingRegisterRecordSet, "Хозрасчетный");
        assert!(
            lookup_field(&configs, &receiver, &Name::new("БлокироватьДляИзменения")).is_some(),
            "AccountingRegisterRecordSet must expose БлокироватьДляИзменения from HBK",
        );
        assert!(
            lookup_field(&configs, &receiver, &Name::new("LockForUpdate")).is_some(),
            "English alias LockForUpdate must resolve via bilingual rsplit",
        );
    }

    #[test]
    fn field_lookup_register_filter_synthesized_for_all_record_set_flavours() {
        let mut config = Configuration::new("Test");
        for (name, mdo_type) in [
            ("РегСвед", MdoType::InformationRegister),
            ("РегНак", MdoType::AccumulationRegister),
            ("РегБух", MdoType::AccountingRegister),
            ("РегРасч", MdoType::CalculationRegister),
        ] {
            config.add_register(register_with(
                name,
                mdo_type,
                vec![dimension_typed("Дим", AttributeType::String { length: Some(10) })],
                vec![],
                vec![],
            ));
        }
        let configs = wrap(config);

        let cases = [
            (MetadataKind::InformationRegisterRecordSet, "РегСвед", MdoType::InformationRegister),
            (MetadataKind::AccumulationRegisterRecordSet, "РегНак", MdoType::AccumulationRegister),
            (MetadataKind::AccountingRegisterRecordSet, "РегБух", MdoType::AccountingRegister),
            (MetadataKind::CalculationRegisterRecordSet, "РегРасч", MdoType::CalculationRegister),
        ];
        for (kind, name, parent) in cases {
            let receiver = metadata_ref(kind, name);
            let info = lookup_field(&configs, &receiver, &Name::new("Отбор"))
                .unwrap_or_else(|| panic!("{kind:?}/{name}: synthetic .Отбор must resolve"));
            assert_eq!(
                info.ty,
                metadata_ref(MetadataKind::RegisterFilter { parent }, name),
                "{kind:?}/{name}: RegisterFilter parent must match register flavour",
            );
        }
    }

    #[test]
    fn field_lookup_extension_wins_on_collision() {
        let mut main = Configuration::new("Main");
        main.add_metadata_object(catalog(
            "Номенклатура",
            vec![attr("Цена", None, AttributeType::Number { precision: 15, scale: 2 })],
        ));
        let mut ext = Configuration::new("Ext");
        ext.add_metadata_object(catalog(
            "Номенклатура",
            vec![attr("Цена", None, AttributeType::String { length: Some(64) })],
        ));
        let configs = vec![
            VisibleConfig { name: None, configuration: Arc::new(main) },
            VisibleConfig { name: Some("Ext".into()), configuration: Arc::new(ext) },
        ];

        let receiver = metadata_ref(MetadataKind::CatalogRef, "Номенклатура");
        let info = lookup_field(&configs, &receiver, &Name::new("Цена"))
            .expect("Цена resolves via extension override");
        assert_eq!(info.ty, string(), "extension type wins over main config");
    }

    #[test]
    fn field_lookup_register_extension_wins_on_collision() {
        let mut main = Configuration::new("Main");
        main.add_register(register_with(
            "РегистрСведений1",
            MdoType::InformationRegister,
            vec![],
            vec![resource_typed("R", AttributeType::Number { precision: 15, scale: 2 })],
            vec![],
        ));
        let mut ext = Configuration::new("Ext");
        ext.add_register(register_with(
            "РегистрСведений1",
            MdoType::InformationRegister,
            vec![],
            vec![resource_typed("R", AttributeType::String { length: Some(64) })],
            vec![],
        ));
        let configs = vec![
            VisibleConfig { name: None, configuration: Arc::new(main) },
            VisibleConfig { name: Some("Ext".into()), configuration: Arc::new(ext) },
        ];

        let receiver = metadata_ref(MetadataKind::InformationRegisterRef, "РегистрСведений1");
        let info = lookup_field(&configs, &receiver, &Name::new("R"))
            .expect("R resolves via extension override");
        assert_eq!(info.ty, string(), "extension register type wins over main config");
    }

    #[test]
    fn split_parent_section_rejects_malformed() {
        use crate::field_enum::split_parent_section;
        assert_eq!(split_parent_section("ПКО.Товары"), Some(("ПКО", "Товары")));
        assert_eq!(split_parent_section("ПКО"), None);
        assert_eq!(split_parent_section(""), None);
        assert_eq!(split_parent_section("."), None);
        assert_eq!(split_parent_section("ПКО."), None);
        assert_eq!(split_parent_section(".Товары"), None);
    }

    #[test]
    fn lookup_field_on_union_with_undefined_resolves_to_arm() {
        let mut ts = TabularSection::new(Uuid::new_v4(), "Товары");
        ts.set_attributes(vec![TabularSectionAttribute::new(
            Uuid::new_v4(),
            "Номенклатура",
            AttributeType::Ref {
                mdo_type: MdoType::Catalog, name: "Номенклатура".into()
            },
        )]);
        let mut doc = MetadataObject::new(MdoType::Document, "ПКО");
        doc.add_tabular_section(ts);
        let mut config = Configuration::new("Test");
        config.add_metadata_object(doc);
        let configs = wrap(config);

        let row = metadata_ref(
            MetadataKind::TabularSectionRow { parent: MdoType::Document },
            "ПКО.Товары",
        );
        let receiver = union(vec![row, undefined()]);
        let info = lookup_field(&configs, &receiver, &Name::new("Номенклатура"))
            .expect("union arm column must resolve");
        assert!(
            matches!(info.ty.db.lookup_type(info.ty.id), TypeKind::MetadataRef(_)),
            "Номенклатура is a Ref"
        );
    }

    #[test]
    fn lookup_field_on_union_intersection_requires_field_in_every_arm() {
        let mut config = Configuration::new("Test");
        config.add_metadata_object(catalog(
            "A",
            vec![
                attr(
                    "Ссылка",
                    Some("Ref"),
                    AttributeType::Ref { mdo_type: MdoType::Catalog, name: "A".into() },
                ),
                attr("Код", Some("Code"), AttributeType::String { length: Some(9) }),
                attr("OnlyInA", None, AttributeType::Boolean),
            ],
        ));
        config.add_metadata_object(catalog(
            "B",
            vec![
                attr(
                    "Ссылка",
                    Some("Ref"),
                    AttributeType::Ref { mdo_type: MdoType::Catalog, name: "B".into() },
                ),
                attr("Код", Some("Code"), AttributeType::String { length: Some(11) }),
            ],
        ));
        let configs = wrap(config);

        let a = metadata_ref(MetadataKind::CatalogRef, "A");
        let b = metadata_ref(MetadataKind::CatalogRef, "B");
        let receiver = union(vec![a, b]);

        let common = lookup_field(&configs, &receiver, &Name::new("Код"))
            .expect("Код is in both arms — intersection succeeds");
        assert_eq!(common.ty, string(), "merged type collapses identical String arms");

        let only_a = lookup_field(&configs, &receiver, &Name::new("OnlyInA"));
        assert!(only_a.is_none(), "field absent in B must not resolve under union");
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn lookup_field_on_union_intersection_readonly_merges_via_or() {
        let mut config = Configuration::new("Test");
        config.add_metadata_object(catalog("A", vec![]));
        config.add_metadata_object(catalog(
            "B",
            vec![attr("ДополнительныеСвойства", None, AttributeType::Boolean)],
        ));
        let configs = wrap(config);

        let a = metadata_ref(MetadataKind::CatalogObject, "A");
        let b = metadata_ref(MetadataKind::CatalogObject, "B");
        let receiver = union(vec![a, b]);

        let info = lookup_field(&configs, &receiver, &Name::new("ДополнительныеСвойства"))
            .expect("ДополнительныеСвойства is in both arms — intersection succeeds");
        assert!(
            info.is_readonly,
            "OR-merge: arm A is read-only (HBK), so union read-only regardless of arm B"
        );
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn field_lookup_document_object_pulls_platform_properties() {
        let mut config = Configuration::new("Test");
        config.add_metadata_object(document("ПКО", vec![]));
        let configs = wrap(config);

        let receiver = metadata_ref(MetadataKind::DocumentObject, "ПКО");

        let dop = lookup_field(&configs, &receiver, &Name::new("ДополнительныеСвойства"))
            .expect("ДополнительныеСвойства must surface on DocumentObject via HBK");
        assert_eq!(dop.ty, structure(), "HBK declares Структура");
        assert!(dop.is_readonly, "DocumentObject.ДополнительныеСвойства is read-only per HBK");

        for prop in [
            "Движения",
            "ОбменДанными",
            "ВерсияДанных",
            "ЗаписьИсторииДанных",
            "ПринадлежностьПоследовательностям",
            "ЭтотОбъект",
        ] {
            assert!(
                lookup_field(&configs, &receiver, &Name::new(prop)).is_some(),
                "platform property `{prop}` must surface on DocumentObject",
            );
        }

        assert!(
            lookup_field(&configs, &receiver, &Name::new("AdditionalProperties")).is_some(),
            "English alias AdditionalProperties must resolve through bilingual rsplit",
        );
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn field_lookup_this_object_is_typed_metadata_ref_not_generic_platform_object() {
        let mut config = Configuration::new("Test");
        config.add_metadata_object(document("ПКО", vec![]));
        config.add_metadata_object(catalog("Номенклатура", vec![]));
        let configs = wrap(config);

        let doc = metadata_ref(MetadataKind::DocumentObject, "ПКО");
        let this_obj = lookup_field(&configs, &doc, &Name::new("ЭтотОбъект"))
            .expect("ЭтотОбъект resolves on DocumentObject");
        assert_eq!(
            this_obj.ty,
            metadata_ref(MetadataKind::DocumentObject, "ПКО"),
            "ЭтотОбъект must be a typed MetadataRef (self), not a generic PlatformObject",
        );

        let cat = metadata_ref(MetadataKind::CatalogObject, "Номенклатура");
        let cat_this_obj = lookup_field(&configs, &cat, &Name::new("ЭтотОбъект"))
            .expect("ЭтотОбъект resolves on CatalogObject");
        assert_eq!(
            cat_this_obj.ty,
            metadata_ref(MetadataKind::CatalogObject, "Номенклатура"),
            "CatalogObject.ЭтотОбъект must specialize to typed self",
        );

        let r#ref = lookup_field(&configs, &doc, &Name::new("Ссылка"))
            .expect("Ссылка resolves on DocumentObject via cascade");
        assert_eq!(
            r#ref.ty,
            metadata_ref(MetadataKind::DocumentRef, "ПКО"),
            "Ссылка must specialize to typed DocumentRef self",
        );
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn field_lookup_this_object_specializes_across_yo_spelling_difference() {
        let mut config = Configuration::new("Test");
        config.add_metadata_object(mdo_of(MdoType::Report, "ОстаткиТоваров", vec![]));
        config.add_metadata_object(mdo_of(MdoType::DataProcessor, "Обработка1", vec![]));
        let configs = wrap(config);

        let report = metadata_ref(MetadataKind::ReportObject, "ОстаткиТоваров");
        let report_this = lookup_field(&configs, &report, &Name::new("ЭтотОбъект"))
            .expect("ЭтотОбъект resolves on ReportObject");
        assert_eq!(
            report_this.ty,
            metadata_ref(MetadataKind::ReportObject, "ОстаткиТоваров"),
            "ReportObject.ЭтотОбъект must specialize through ё↔е folding",
        );

        let dp = metadata_ref(MetadataKind::DataProcessorObject, "Обработка1");
        let dp_this = lookup_field(&configs, &dp, &Name::new("ЭтотОбъект"))
            .expect("ЭтотОбъект resolves on DataProcessorObject");
        assert_eq!(
            dp_this.ty,
            metadata_ref(MetadataKind::DataProcessorObject, "Обработка1"),
            "DataProcessorObject.ЭтотОбъект must specialize to typed self",
        );
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn field_lookup_catalog_object_pulls_platform_properties() {
        let mut config = Configuration::new("Test");
        config.add_metadata_object(catalog("Номенклатура", vec![]));
        let configs = wrap(config);

        let receiver = metadata_ref(MetadataKind::CatalogObject, "Номенклатура");

        let dop = lookup_field(&configs, &receiver, &Name::new("ДополнительныеСвойства"))
            .expect("ДополнительныеСвойства must surface on CatalogObject via HBK");
        assert_eq!(dop.ty, structure());
        assert!(dop.is_readonly);

        assert!(lookup_field(&configs, &receiver, &Name::new("ВерсияДанных")).is_some());
        assert!(lookup_field(&configs, &receiver, &Name::new("ЗаписьИсторииДанных")).is_some());
        assert!(lookup_field(&configs, &receiver, &Name::new("ЭтотОбъект")).is_some());
    }

    fn document_with_standard_attrs(name: &str) -> MetadataObject {
        document(
            name,
            vec![
                attr(
                    "Ссылка",
                    Some("Ref"),
                    AttributeType::Ref { mdo_type: MdoType::Document, name: name.to_string() },
                ),
                attr("ПометкаУдаления", Some("DeletionMark"), AttributeType::Boolean),
                attr("Дата", Some("Date"), AttributeType::DateTime),
                attr("Проведен", Some("Posted"), AttributeType::Boolean),
            ],
        )
    }

    #[test]
    fn field_lookup_document_object_priority_pin_keeps_typed_standard_attrs() {
        let mut config = Configuration::new("Test");
        config.add_metadata_object(document_with_standard_attrs("ПКО"));
        let configs = wrap(config);

        let receiver = metadata_ref(MetadataKind::DocumentObject, "ПКО");

        let date_info = lookup_field(&configs, &receiver, &Name::new("Дата"))
            .expect("standard Дата resolves on DocumentObject");
        assert_eq!(date_info.ty, date(), "spec-typed Дата must win over HBK cascade");
        assert!(!date_info.is_readonly, "Дата on DocumentObject is writable per spec");

        let r#ref = lookup_field(&configs, &receiver, &Name::new("Ссылка"))
            .expect("standard Ссылка resolves on DocumentObject");
        assert_eq!(
            r#ref.ty,
            metadata_ref(MetadataKind::DocumentRef, "ПКО"),
            "Ссылка must remain a typed self-ref, not a stringly-typed HBK entry",
        );

        let posted = lookup_field(&configs, &receiver, &Name::new("Проведен"))
            .expect("standard Проведен resolves on DocumentObject");
        assert_eq!(posted.ty, boolean(), "spec-typed Проведен must keep its Boolean type");
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn field_lookup_document_ref_caveats_and_cascade() {
        let mut config = Configuration::new("Test");
        config.add_metadata_object(document_with_standard_attrs("ПКО"));
        let configs = wrap(config);

        let receiver = metadata_ref(MetadataKind::DocumentRef, "ПКО");

        assert!(
            lookup_field(&configs, &receiver, &Name::new("ДополнительныеСвойства")).is_none(),
            "Object-only property must not leak into DocumentRef surface",
        );
        assert!(
            lookup_field(&configs, &receiver, &Name::new("Движения")).is_none(),
            "Движения is DocumentObject-only per HBK",
        );
        assert!(
            lookup_field(&configs, &receiver, &Name::new("ЭтотОбъект")).is_none(),
            "ЭтотОбъект is DocumentObject-only per HBK",
        );

        assert!(
            lookup_field(&configs, &receiver, &Name::new("ВерсияДанных")).is_some(),
            "DocumentRef.ВерсияДанных must surface via HBK cascade",
        );

        let date = lookup_field(&configs, &receiver, &Name::new("Дата"))
            .expect("standard Дата resolves on DocumentRef");
        assert!(
            !date.is_readonly,
            "Phase A limitation: DocumentRef.Дата is_readonly is not uplifted from Object-view spec (see B1)",
        );
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn field_lookup_cascade_respects_presence_conditional_standard_attrs() {
        let mut config = Configuration::new("Test");
        config.add_metadata_object(document("ПКО", vec![]));
        config.add_metadata_object(catalog("Номенклатура", vec![]));
        let configs = wrap(config);

        let doc = metadata_ref(MetadataKind::DocumentObject, "ПКО");
        assert!(
            lookup_field(&configs, &doc, &Name::new("Номер")).is_none(),
            "Номер must NOT leak from HBK when the document has no NumberLength",
        );

        let cat = metadata_ref(MetadataKind::CatalogObject, "Номенклатура");
        assert!(
            lookup_field(&configs, &cat, &Name::new("Код")).is_none(),
            "Код must NOT leak from HBK when the catalog has no CodeLength",
        );
        assert!(
            lookup_field(&configs, &cat, &Name::new("ЭтоГруппа")).is_none(),
            "ЭтоГруппа must NOT leak from HBK when the catalog is not Hierarchical",
        );
        assert!(
            lookup_field(&configs, &cat, &Name::new("Родитель")).is_none(),
            "Родитель must NOT leak from HBK when the catalog is not Hierarchical",
        );
        assert!(
            lookup_field(&configs, &cat, &Name::new("Владелец")).is_none(),
            "Владелец must NOT leak from HBK when the catalog has no Owners",
        );

        assert!(
            lookup_field(&configs, &cat, &Name::new("ДополнительныеСвойства")).is_some(),
            "non-spec HBK properties remain visible after the gate",
        );
    }

    #[test]
    fn field_lookup_non_mdo_receivers_unaffected_by_mdo_cascade() {
        let mut config = Configuration::new("Test");
        config.add_register(register_with(
            "РегСвед",
            MdoType::InformationRegister,
            vec![dimension_typed("Валюта", AttributeType::String { length: Some(3) })],
            vec![],
            vec![],
        ));
        config.add_metadata_object({
            let mut mdo = catalog("Товары", vec![]);
            let mut ts = TabularSection::new(Uuid::new_v4(), "Цены");
            ts.set_attributes(vec![TabularSectionAttribute::new(
                Uuid::new_v4(),
                "Цена",
                AttributeType::Number { precision: 15, scale: 2 },
            )]);
            mdo.add_tabular_section(ts);
            mdo
        });
        let configs = wrap(config);

        let filter = metadata_ref(
            MetadataKind::RegisterFilter { parent: MdoType::InformationRegister },
            "РегСвед",
        );
        assert!(
            lookup_field(&configs, &filter, &Name::new("ДополнительныеСвойства")).is_none(),
            "RegisterFilter must not pull DocumentObject/CatalogObject cascade",
        );

        let row = metadata_ref(
            MetadataKind::TabularSectionRow { parent: MdoType::Catalog },
            "Товары.Цены",
        );
        assert!(
            lookup_field(&configs, &row, &Name::new("ДополнительныеСвойства")).is_none(),
            "TabularSectionRow must not pull MDO cascade",
        );
    }

    fn projection_with_two_fields(
        db: &dyn TypeKernelDb,
    ) -> std::sync::Arc<bsl_types::kind::Projection> {
        use bsl_types::kind::{ProjectionField, ProjectionFieldSource, ProjectionOrigin};
        std::sync::Arc::new(bsl_types::kind::Projection::new(
            std::sync::Arc::from([
                ProjectionField::new(
                    "КодТов".to_string(),
                    db.string(None, false),
                    ProjectionFieldSource::Column,
                ),
                ProjectionField::new(
                    "Наименование".to_string(),
                    db.string(None, false),
                    ProjectionFieldSource::Column,
                ),
            ]),
            ProjectionOrigin::SdblQuery,
            None,
        ))
    }

    #[test]
    fn sdbl_projection_field_resolves_via_projection_table() {
        let db = InMemoryDb::new();
        let receiver = db.query_result_selection(
            Some(projection_with_two_fields(&db)),
            ProjectionSource::Unknown,
        );
        let info =
            super::lookup_field(&db, &ConfigsObjectResolver(&[]), receiver, &Name::new("КодТов"))
                .expect("projection field must resolve");
        assert_eq!(info.ty, db.string(None, false));
        assert!(info.is_readonly, "SDBL projection fields are read-only");
    }

    #[test]
    fn value_table_row_projection_field_is_writable() {
        let db = InMemoryDb::new();
        let receiver = db.value_table_row(
            Some(projection_with_two_fields(&db)),
            bsl_types::facet::TableSource::SdblUnload,
        );
        let info =
            super::lookup_field(&db, &ConfigsObjectResolver(&[]), receiver, &Name::new("КодТов"))
                .expect("value-table row column must resolve");
        assert_eq!(info.ty, db.string(None, false));
        assert!(!info.is_readonly, "value-table row columns accept assignment");
    }

    #[test]
    fn sdbl_projection_field_lookup_is_case_insensitive() {
        let db = InMemoryDb::new();
        let receiver = db.query_result_selection(
            Some(projection_with_two_fields(&db)),
            ProjectionSource::Unknown,
        );
        assert!(super::lookup_field(
            &db,
            &ConfigsObjectResolver(&[]),
            receiver,
            &Name::new("кодтов")
        )
        .is_some());
        assert!(super::lookup_field(
            &db,
            &ConfigsObjectResolver(&[]),
            receiver,
            &Name::new("НАИМЕНОВАНИЕ")
        )
        .is_some());
    }

    #[test]
    fn sdbl_projection_falls_through_to_platform_on_miss() {
        let db = InMemoryDb::new();
        let receiver_id = db.query_result_selection(
            Some(projection_with_two_fields(&db)),
            ProjectionSource::Unknown,
        );
        assert!(
            lookup_field_in_query_projection(&db, receiver_id, &Name::new("НесуществующееПоле"))
                .is_none(),
            "projection lookup must miss on unknown field — orchestrator continues to platform fallback",
        );
    }

    #[test]
    fn sdbl_projection_lookup_no_op_for_none_projection() {
        let db = InMemoryDb::new();
        let receiver_id = db.query_result_selection(None, ProjectionSource::Unknown);
        assert!(lookup_field_in_query_projection(&db, receiver_id, &Name::new("Имя")).is_none());
    }

    #[test]
    fn sdbl_projection_lookup_no_op_for_non_selection_receiver() {
        let db = InMemoryDb::new();
        let receiver_id = db.query(Arc::from([Some(projection_with_two_fields(&db))]));
        assert!(lookup_field_in_query_projection(&db, receiver_id, &Name::new("КодТов")).is_none());
    }

    /// An EPF export's object carries its own attributes and specializes
    /// `ЭтотОбъект` to itself; its platform properties come from the bare type
    /// `ВнешняяОбработка`, which has no manager prefix in the corpus.
    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn field_lookup_external_object_has_its_attributes_self_and_platform_properties() {
        let mut config = Configuration::new("Test");
        config.add_metadata_object(mdo_of(
            MdoType::ExternalDataProcessor,
            "АРМПроизводство",
            vec![attr(
                "АвторизованныйПользователь",
                None,
                AttributeType::Ref {
                    mdo_type: MdoType::Catalog, name: "Пользователи".into()
                },
            )],
        ));
        // A same-named INTERNAL object with a different attribute: the external
        // kind must never borrow it.
        config.add_metadata_object(mdo_of(
            MdoType::DataProcessor,
            "АРМПроизводство",
            vec![attr("Внутренний", None, AttributeType::String { length: None })],
        ));
        let configs = wrap(config);
        let receiver = metadata_ref(MetadataKind::ExternalDataProcessorObject, "АРМПроизводство");

        let this = lookup_field(&configs, &receiver, &Name::new("ЭтотОбъект"))
            .expect("ЭтотОбъект resolves on ExternalDataProcessorObject");
        assert_eq!(this.ty, receiver);

        let user = lookup_field(&configs, &receiver, &Name::new("АвторизованныйПользователь"))
            .expect("the export's own attribute resolves");
        assert_eq!(user.ty, metadata_ref(MetadataKind::CatalogRef, "Пользователи"));

        assert!(
            lookup_field(&configs, &receiver, &Name::new("Внутренний")).is_none(),
            "the internal object's attribute is not the external's"
        );
        assert!(
            lookup_field(&configs, &receiver, &Name::new("ИспользуемоеИмяФайла")).is_some(),
            "ВнешняяОбработка's own platform property resolves through the bare type"
        );
    }
}
