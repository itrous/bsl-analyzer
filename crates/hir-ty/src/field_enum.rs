use std::sync::Arc;
use stdx::case::CaseExt;

use bsl_metadata::{AttributeType, MdoType, MetadataObject, MetadataResolver, RegisterPeriodicity};
use bsl_platform::{
    standard_attributes_for, MdoTemplateKind, ObjectView, PlatformData, StandardKind,
};
use bsl_types::builders::Builders;
use bsl_types::facet::DateComponent;
use bsl_types::intern::TypeKernelDb;
use bsl_types::kind::{ConfigId, MetadataReferenceKind, TypeId, TypeKind};
use bsl_types::testing::RootConfigCtx;
use hir_def::ty::MetadataKind;
use hir_def::type_ref::TypeRef;
use hir_def::Name;

use crate::lower::TyLoweringContext;
use crate::object_resolver::{MetadataResolution, ObjectResolver};
use crate::this_object::FixedConfigCtx;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldOrigin {
    StandardAttribute,
    UserAttribute,
    FormAttribute,
    MainFormAttribute,
    TabularSection,
    TabularSectionRowColumn,
    RegisterDimension,
    RegisterResource,
    RegisterAttribute,
    MetadataReference,
    /// A property from the platform API, carrying its documented
    /// execution-environment availability.
    PlatformProperty {
        env: hir_def::execution_env::EnvFlags,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldInfo {
    pub name: Name,
    pub name_en: Option<Name>,
    pub ty: TypeId,
    pub value_ty: Option<TypeId>,
    pub is_readonly: bool,
    pub origin: FieldOrigin,
}

pub fn enumerate_fields(
    db: &dyn TypeKernelDb,
    resolver: &dyn MetadataResolution,
    receiver: TypeId,
) -> Vec<FieldInfo> {
    enumerate_fields_inner(db, resolver, receiver)
}

pub(crate) fn enumerate_fields_inner(
    db: &dyn TypeKernelDb,
    resolver: &dyn MetadataResolution,
    receiver: TypeId,
) -> Vec<FieldInfo> {
    let projected_form_data = crate::field_lookup::project_form_data_for_fields_id(db, receiver);
    let receiver = projected_form_data.unwrap_or(receiver);

    let ty = crate::this_object::coerce_to_metadata_ref_id(db, receiver).unwrap_or(receiver);

    if let Some(infos) = enumerate_projection_fields(db, ty) {
        return infos;
    }

    enum Shape {
        Union(Vec<TypeId>),
        MetadataRef { kind: MetadataKind, name: Name, config_id: ConfigId },
        MetadataReferenceCollection(MetadataReferenceKind),
        PlatformObject(Name),
        Other,
    }
    let shape = match db.lookup_type(ty) {
        TypeKind::Union(arms) => Shape::Union(arms.to_vec()),
        TypeKind::MetadataRef(facet) => Shape::MetadataRef {
            kind: facet.kind,
            name: Name::new(facet.name.as_str()),
            config_id: facet.config_id.clone(),
        },
        TypeKind::MetadataObject(facet) => Shape::MetadataRef {
            kind: facet.kind,
            name: Name::new(facet.name.as_str()),
            config_id: facet.config_id.clone(),
        },
        TypeKind::MetadataReferenceCollection(kind) => Shape::MetadataReferenceCollection(*kind),
        TypeKind::PlatformObject(facet) => Shape::PlatformObject(Name::new(&facet.name)),
        _ => Shape::Other,
    };

    match shape {
        Shape::Union(arms) => {
            let mut out: Vec<FieldInfo> = Vec::new();
            let mut seen: std::collections::HashSet<Name> = std::collections::HashSet::new();
            for arm in arms {
                if matches!(db.lookup_type(arm), TypeKind::Undefined | TypeKind::Null) {
                    continue;
                }
                for info in enumerate_fields_inner(db, resolver, arm) {
                    push_unique(&mut out, &mut seen, info);
                }
            }
            out
        }
        Shape::MetadataRef { kind, name, config_id } => {
            if let Some(mdo_type) = mdo_type_for_kind(kind) {
                return enumerate_mdo_fields(db, resolver, kind, mdo_type, &name, &config_id);
            }
            if let Some(parent) = register_parent_for_kind(kind) {
                return enumerate_register_fields(db, resolver, kind, parent, &name, &config_id);
            }
            if let MetadataKind::RegisterFilter { parent } = kind {
                return enumerate_filter_fields(db, resolver, parent, &name);
            }
            if let MetadataKind::TabularSectionRow { parent } = kind {
                let Some((parent_name, section_name)) = split_parent_section(name.as_str()) else {
                    return Vec::new();
                };
                return enumerate_tabular_row_fields(
                    db,
                    resolver,
                    parent,
                    parent_name,
                    section_name,
                );
            }
            Vec::new()
        }
        Shape::MetadataReferenceCollection(kind) => {
            enumerate_metadata_reference_collection(db, resolver, kind)
        }
        Shape::PlatformObject(name) => enumerate_platform_object_fields(db, &name),
        Shape::Other => Vec::new(),
    }
}

fn enumerate_platform_object_fields(db: &dyn TypeKernelDb, type_name: &Name) -> Vec<FieldInfo> {
    PlatformData::instance()
        .get_type_properties(type_name.as_str())
        .into_iter()
        .map(|property| {
            let resolution = crate::platform_property_lookup::to_resolution(db, property);
            FieldInfo {
                name: Name::new(&property.name),
                name_en: Some(Name::new(&property.english_name)),
                ty: resolution.return_ty,
                value_ty: None,
                is_readonly: resolution.is_readonly,
                origin: FieldOrigin::PlatformProperty { env: resolution.env },
            }
        })
        .collect()
}

fn enumerate_metadata_reference_collection(
    db: &dyn TypeKernelDb,
    resolver: &dyn MetadataResolution,
    kind: MetadataReferenceKind,
) -> Vec<FieldInfo> {
    resolver
        .metadata_reference_members(kind)
        .into_iter()
        .map(|name| FieldInfo {
            ty: db.metadata_reference(kind, name.as_str().to_string()),
            name,
            name_en: None,
            value_ty: None,
            is_readonly: true,
            origin: FieldOrigin::MetadataReference,
        })
        .collect()
}

fn enumerate_projection_fields(db: &dyn TypeKernelDb, ty: TypeId) -> Option<Vec<FieldInfo>> {
    // A query-result cursor refuses writes ("Поле объекта недоступно для записи"); a
    // value-table row is materialised data whose columns are as writable as a structure's keys.
    let (projection, is_readonly) = match db.lookup_type(ty) {
        TypeKind::QueryResultSelection(facet) => (facet.projection.clone()?, true),
        TypeKind::ValueTableRow(facet) => (facet.projection.clone()?, false),
        TypeKind::Structure(facet) => {
            (crate::structure_keys::structure_projection_fields(facet)?, false)
        }
        _ => return None,
    };
    let fields = projection
        .fields
        .iter()
        .map(|f| FieldInfo {
            name: Name::new(f.name.as_str()),
            name_en: None,
            ty: f.ty,
            value_ty: None,
            is_readonly,
            origin: FieldOrigin::UserAttribute,
        })
        .collect();
    Some(fields)
}

fn is_record_set_kind(kind: MetadataKind) -> bool {
    matches!(
        kind,
        MetadataKind::InformationRegisterRecordSet
            | MetadataKind::AccumulationRegisterRecordSet
            | MetadataKind::AccountingRegisterRecordSet
            | MetadataKind::CalculationRegisterRecordSet
    )
}

fn is_record_kind(kind: MetadataKind) -> bool {
    matches!(
        kind,
        MetadataKind::InformationRegisterRecord
            | MetadataKind::AccumulationRegisterRecord
            | MetadataKind::AccountingRegisterRecord
            | MetadataKind::CalculationRegisterRecord
    )
}

#[allow(clippy::too_many_arguments)]
fn push_platform_prefix_properties(
    db: &dyn TypeKernelDb,
    kind: MetadataKind,
    mdo_type: MdoType,
    mdo_name: &Name,
    config_id: &ConfigId,
    out: &mut Vec<FieldInfo>,
    seen: &mut std::collections::HashSet<Name>,
    mut ty_override: impl FnMut(&str) -> Option<TypeId>,
) {
    let data = PlatformData::instance();
    let properties = match (kind.platform_prefix(), kind.bare_platform_type()) {
        (Some(prefix), _) => data.get_manager_properties(prefix),
        (None, Some(type_name)) => data.get_type_properties(type_name),
        (None, None) => return,
    };
    let spec_names = standard_attribute_names_for(mdo_type);
    for prop in properties {
        let en_tail =
            prop.english_name.as_str().rsplit('.').next().unwrap_or(prop.english_name.as_str());
        if name_in_spec(&spec_names, prop.name.as_str(), en_tail) {
            continue;
        }
        let res = crate::platform_property_lookup::to_resolution(db, prop);
        let specialized = specialize_self_ref_ty(db, mdo_type, mdo_name, config_id, res.return_ty);
        let ty = ty_override(prop.name.as_str()).or(specialized).unwrap_or(res.return_ty);
        let info = FieldInfo {
            name: Name::new(prop.name.as_str()),
            name_en: Some(Name::new(en_tail)),
            ty,
            value_ty: None,
            is_readonly: res.is_readonly,
            origin: FieldOrigin::PlatformProperty { env: res.env },
        };
        push_unique(out, seen, info);
    }
}

fn specialize_self_ref_ty(
    db: &dyn TypeKernelDb,
    mdo_type: MdoType,
    mdo_name: &Name,
    config_id: &ConfigId,
    ty: TypeId,
) -> Option<TypeId> {
    let TypeKind::PlatformObject(facet) = db.lookup_type(ty) else {
        return None;
    };
    let base = facet.name.as_str().to_string();
    let candidates =
        [MetadataKind::object_kind_for(mdo_type), MetadataKind::ref_kind_for(mdo_type)];
    for candidate in candidates.into_iter().flatten() {
        let ru = candidate.display_label(base_db::Locale::Ru);
        let en = candidate.display_label(base_db::Locale::En);
        // An external object's self-returning members are typed by the bare
        // platform type (`ВнешняяОбработка`), which is the object type itself.
        let bare = candidate.bare_platform_type_names();
        let matches_bare = bare
            .is_some_and(|(bare_ru, bare_en)| eq_yo_insensitive(&base, bare_ru) || base == bare_en);
        if eq_yo_insensitive(&base, ru) || base == en || matches_bare {
            let cfg = FixedConfigCtx(config_id.clone());
            return Some(db.metadata_ref(candidate, mdo_name.as_str().to_string(), &cfg));
        }
    }
    None
}

fn eq_yo_insensitive(lhs: &str, rhs: &str) -> bool {
    if lhs == rhs {
        return true;
    }
    if lhs.len() != rhs.len() {
        return false;
    }
    lhs.chars().map(fold_yo).eq(rhs.chars().map(fold_yo))
}

fn fold_yo(c: char) -> char {
    match c {
        'ё' => 'е',
        'Ё' => 'Е',
        _ => c,
    }
}

fn standard_attribute_names_for(mdo_type: MdoType) -> std::collections::HashSet<String> {
    let mut names = std::collections::HashSet::new();
    let Some(template) = mdo_template_kind_for(mdo_type) else {
        return names;
    };
    for view in [ObjectView::Object, ObjectView::Ref, ObjectView::RecordSet] {
        for spec in standard_attributes_for(template, view) {
            if matches!(spec.condition, bsl_platform::PresenceCondition::Always) {
                continue;
            }
            insert_standard_kind_names(&mut names, spec.kind);
        }
    }
    names
}

fn insert_standard_kind_names(set: &mut std::collections::HashSet<String>, kind: StandardKind) {
    set.insert(kind.russian_name().fold_lower());
    set.insert(kind.english_name().fold_lower());
}

fn name_in_spec(spec_names: &std::collections::HashSet<String>, ru: &str, en: &str) -> bool {
    spec_names.contains(&ru.fold_lower()) || spec_names.contains(&en.fold_lower())
}

fn enumerate_mdo_fields(
    db: &dyn TypeKernelDb,
    resolver: &dyn MetadataResolution,
    kind: MetadataKind,
    mdo_type: MdoType,
    mdo_name: &Name,
    config_id: &ConfigId,
) -> Vec<FieldInfo> {
    let Some(mdo) = resolver.resolve_metadata_object(mdo_type, mdo_name.as_str()) else {
        return Vec::new();
    };

    let mut out = Vec::with_capacity(mdo.attributes.len() + mdo.tabular_sections.len());
    let mut seen: std::collections::HashSet<Name> =
        std::collections::HashSet::with_capacity(out.capacity() * 2);

    let template = mdo_template_kind_for(mdo_type);

    for attr in &mdo.attributes {
        let spec = classify_attr(template, &attr.name);
        let (origin, is_readonly) = match spec {
            Some(s) => (FieldOrigin::StandardAttribute, s.is_readonly),
            None => (FieldOrigin::UserAttribute, false),
        };
        let info = FieldInfo {
            name: Name::new(&attr.name),
            name_en: attr.name_en.as_deref().filter(|s| !s.is_empty()).map(Name::new),
            ty: attribute_type_to_typeid(db, &attr.attr_type, resolver),
            value_ty: None,
            is_readonly,
            origin,
        };
        push_unique(&mut out, &mut seen, info);
    }

    for common in &mdo.common_attributes {
        let info = FieldInfo {
            name: Name::new(&common.name),
            name_en: None,
            ty: attribute_type_to_typeid(db, &common.attr_type, resolver),
            value_ty: None,
            is_readonly: false,
            origin: FieldOrigin::UserAttribute,
        };
        push_unique(&mut out, &mut seen, info);
    }

    for ts in &mdo.tabular_sections {
        let qualified = format!("{}.{}", mdo_name.as_str(), ts.name());
        let info = FieldInfo {
            name: Name::new(ts.name()),
            name_en: ts.name_en().filter(|s| !s.is_empty()).map(Name::new),
            ty: db.metadata_ref(
                MetadataKind::TabularSection { parent: mdo_type },
                qualified,
                &RootConfigCtx,
            ),
            value_ty: None,
            is_readonly: false,
            origin: FieldOrigin::TabularSection,
        };
        push_unique(&mut out, &mut seen, info);
    }

    push_platform_prefix_properties(
        db,
        kind,
        mdo_type,
        mdo_name,
        config_id,
        &mut out,
        &mut seen,
        |_| None,
    );

    out
}

fn enumerate_register_fields(
    db: &dyn TypeKernelDb,
    resolver: &dyn MetadataResolution,
    kind: MetadataKind,
    parent: MdoType,
    register_name: &Name,
    config_id: &ConfigId,
) -> Vec<FieldInfo> {
    let Some(register) = resolver.resolve_register(parent, register_name.as_str()) else {
        return Vec::new();
    };

    {
        let cap = register.dimensions().len()
            + register.resources().len()
            + register.attributes().len()
            + 1;
        let mut out = Vec::with_capacity(cap);
        let mut seen: std::collections::HashSet<Name> =
            std::collections::HashSet::with_capacity(cap * 2);

        if is_record_set_kind(kind) {
            let info = FieldInfo {
                name: Name::new("Отбор"),
                name_en: Some(Name::new("Filter")),
                ty: db.metadata_ref(
                    MetadataKind::RegisterFilter { parent },
                    register_name.as_str().to_string(),
                    &RootConfigCtx,
                ),
                value_ty: None,
                is_readonly: true,
                origin: FieldOrigin::PlatformProperty {
                    env: hir_def::execution_env::EnvFlags::ALL,
                },
            };
            push_unique(&mut out, &mut seen, info);
        }

        for dim in register.dimensions() {
            let info = FieldInfo {
                name: Name::new(dim.name()),
                name_en: None,
                ty: register_part_typeid(
                    db,
                    dim.attr_type(),
                    MetadataKind::RegisterDimension { parent },
                    register_name,
                    dim.name(),
                    resolver,
                ),
                value_ty: None,
                is_readonly: false,
                origin: FieldOrigin::RegisterDimension,
            };
            push_unique(&mut out, &mut seen, info);
        }

        // A separator is part of the record key like a dimension; a plain common attribute
        // is a register attribute.
        for common in register.common_attributes() {
            let info = FieldInfo {
                name: Name::new(&common.name),
                name_en: None,
                ty: attribute_type_to_typeid(db, &common.attr_type, resolver),
                value_ty: None,
                is_readonly: false,
                origin: if common.separator {
                    FieldOrigin::RegisterDimension
                } else {
                    FieldOrigin::RegisterAttribute
                },
            };
            push_unique(&mut out, &mut seen, info);
        }

        for res in register.resources() {
            let info = FieldInfo {
                name: Name::new(res.name()),
                name_en: res.name_en().filter(|s| !s.is_empty()).map(Name::new),
                ty: register_part_typeid(
                    db,
                    res.attr_type(),
                    MetadataKind::RegisterResource { parent },
                    register_name,
                    res.name(),
                    resolver,
                ),
                value_ty: None,
                is_readonly: false,
                origin: FieldOrigin::RegisterResource,
            };
            push_unique(&mut out, &mut seen, info);
        }

        for attr in register.attributes() {
            let recorder_override = (is_record_kind(kind) && is_recorder_name(attr.name()))
                .then(|| recorder_union_typeid(db, resolver, parent, register_name))
                .flatten();
            let ty = recorder_override.unwrap_or_else(|| {
                register_part_typeid(
                    db,
                    attr.attr_type(),
                    MetadataKind::RegisterAttribute { parent },
                    register_name,
                    attr.name(),
                    resolver,
                )
            });
            let info = FieldInfo {
                name: Name::new(attr.name()),
                name_en: attr.name_en().filter(|s| !s.is_empty()).map(Name::new),
                ty,
                value_ty: None,
                is_readonly: false,
                origin: FieldOrigin::RegisterAttribute,
            };
            push_unique(&mut out, &mut seen, info);
        }

        push_platform_prefix_properties(
            db,
            kind,
            parent,
            register_name,
            config_id,
            &mut out,
            &mut seen,
            |prop_name| {
                if is_record_kind(kind) && is_recorder_name(prop_name) {
                    recorder_union_typeid(db, resolver, parent, register_name)
                } else {
                    None
                }
            },
        );

        out
    }
}

fn is_recorder_name(name: &str) -> bool {
    stdx::case::eq_ignore_case(name, "Recorder") || stdx::case::eq_ignore_case(name, "Регистратор")
}

fn recorder_union_typeid(
    db: &dyn TypeKernelDb,
    resolver: &dyn ObjectResolver,
    parent: MdoType,
    register_name: &Name,
) -> Option<TypeId> {
    let docs: Vec<TypeId> = resolver
        .recorders_for_register(parent, register_name.as_str())
        .into_iter()
        .map(|name| db.metadata_ref(MetadataKind::DocumentRef, name, &RootConfigCtx))
        .collect();
    if docs.is_empty() {
        None
    } else {
        Some(db.union(docs))
    }
}

fn enumerate_filter_fields(
    db: &dyn TypeKernelDb,
    resolver: &dyn MetadataResolution,
    parent: MdoType,
    register_name: &Name,
) -> Vec<FieldInfo> {
    let Some(register) = resolver.resolve_register(parent, register_name.as_str()) else {
        return Vec::new();
    };

    let mut standard_keys = standard_filter_keys(parent, register.periodicity()).to_vec();
    if register.is_recorder_subordinate() && !standard_keys.contains(&"Регистратор") {
        standard_keys.push("Регистратор");
    }
    let mut out = Vec::with_capacity(register.dimensions().len() + standard_keys.len());
    let mut seen: std::collections::HashSet<Name> =
        std::collections::HashSet::with_capacity(out.capacity() * 2);

    for dim in register.dimensions() {
        let value_ty = dim
            .attr_type()
            .map(|attr_type| attribute_type_to_typeid(db, attr_type, resolver))
            .unwrap_or_else(|| db.unknown());
        let info = FieldInfo {
            name: Name::new(dim.name()),
            name_en: None,
            ty: db.platform_object("ЭлементОтбора".to_string()),
            value_ty: Some(value_ty),
            is_readonly: false,
            origin: FieldOrigin::RegisterDimension,
        };
        push_unique(&mut out, &mut seen, info);
    }

    // A record set's filter carries every separator of the register (ITS, Приложение 7: a set
    // is read and written within one value of each separator).
    for common in register.common_attributes().iter().filter(|c| c.separator) {
        let info = FieldInfo {
            name: Name::new(&common.name),
            name_en: None,
            ty: db.platform_object("ЭлементОтбора".to_string()),
            value_ty: Some(attribute_type_to_typeid(db, &common.attr_type, resolver)),
            is_readonly: false,
            origin: FieldOrigin::RegisterDimension,
        };
        push_unique(&mut out, &mut seen, info);
    }

    for key in standard_keys {
        let value_ty = standard_filter_key_value_typeid(db, resolver, parent, register_name, key);
        let info = FieldInfo {
            name: Name::new(key),
            name_en: None,
            ty: db.platform_object("ЭлементОтбора".to_string()),
            value_ty: Some(value_ty),
            is_readonly: false,
            origin: FieldOrigin::PlatformProperty { env: hir_def::execution_env::EnvFlags::ALL },
        };
        push_unique(&mut out, &mut seen, info);
    }

    out
}

fn standard_filter_key_value_typeid(
    db: &dyn TypeKernelDb,
    resolver: &dyn MetadataResolution,
    parent: MdoType,
    register_name: &Name,
    key: &str,
) -> TypeId {
    match key {
        "Регистратор" => recorder_union_typeid(db, resolver, parent, register_name)
            .unwrap_or_else(|| db.unknown()),
        "Период" | "ПериодРегистрации" => db.date(DateComponent::DateTime),
        "Активность" => db.boolean(),
        "НомерСтроки" => db.number(None, None),
        "ВидРасчета" => db.unknown(),
        _ => db.unknown(),
    }
}

fn standard_filter_keys(
    parent: MdoType,
    periodicity: Option<RegisterPeriodicity>,
) -> &'static [&'static str] {
    match parent {
        MdoType::AccumulationRegister | MdoType::AccountingRegister => {
            &["Период", "Регистратор", "НомерСтроки", "Активность"]
        }
        MdoType::CalculationRegister => {
            let _ = periodicity;
            &["Регистратор", "НомерСтроки", "Активность", "ВидРасчета", "ПериодРегистрации"]
        }
        MdoType::InformationRegister => match periodicity {
            Some(RegisterPeriodicity::RecorderPosition) => &["Регистратор", "Активность", "Период"],
            Some(
                RegisterPeriodicity::Second | RegisterPeriodicity::Day | RegisterPeriodicity::Month,
            ) => &["Период", "Активность"],
            Some(RegisterPeriodicity::Nonperiodical) | None => &[],
        },
        _ => &[],
    }
}

fn enumerate_tabular_row_fields(
    db: &dyn TypeKernelDb,
    resolver: &dyn MetadataResolution,
    parent: MdoType,
    parent_name: &str,
    section_name: &str,
) -> Vec<FieldInfo> {
    let Some(mdo) = find_mdo(resolver, parent, parent_name) else {
        return Vec::new();
    };
    let Some(ts) = mdo.find_tabular_section(section_name) else {
        return Vec::new();
    };

    let mut out: Vec<FieldInfo> = ts
        .attributes()
        .iter()
        .map(|attr| FieldInfo {
            name: Name::new(attr.name()),
            name_en: attr.name_en().filter(|s| !s.is_empty()).map(Name::new),
            ty: attribute_type_to_typeid(db, attr.attr_type(), resolver),
            value_ty: None,
            is_readonly: false,
            origin: FieldOrigin::TabularSectionRowColumn,
        })
        .collect();

    let nr_name = Name::new("НомерСтроки");
    let nr_name_en = Name::new("LineNumber");
    let already_defined = out.iter().any(|f| {
        stdx::case::eq_ignore_case(f.name.as_str(), "НомерСтроки")
            || f.name_en
                .as_ref()
                .is_some_and(|en| stdx::case::eq_ignore_case(en.as_str(), "LineNumber"))
    });
    if !already_defined {
        if let Some(prop) = crate::platform_property_lookup::lookup_platform_property_by_type(
            db,
            "Line of a tabular section",
            &nr_name,
        ) {
            out.push(FieldInfo {
                name: nr_name,
                name_en: Some(nr_name_en),
                ty: prop.return_ty,
                value_ty: None,
                is_readonly: prop.is_readonly,
                origin: FieldOrigin::PlatformProperty { env: prop.env },
            });
        }
    }

    out
}

pub(crate) fn mdo_type_for_kind(kind: MetadataKind) -> Option<MdoType> {
    match kind {
        MetadataKind::CatalogRef | MetadataKind::CatalogObject => Some(MdoType::Catalog),
        MetadataKind::DocumentRef | MetadataKind::DocumentObject => Some(MdoType::Document),
        MetadataKind::EnumRef => Some(MdoType::Enum),
        MetadataKind::TaskRef | MetadataKind::TaskObject => Some(MdoType::Task),
        MetadataKind::BusinessProcessRef | MetadataKind::BusinessProcessObject => {
            Some(MdoType::BusinessProcess)
        }
        MetadataKind::DataProcessorObject => Some(MdoType::DataProcessor),
        MetadataKind::ReportObject => Some(MdoType::Report),
        MetadataKind::ExternalDataProcessorObject => Some(MdoType::ExternalDataProcessor),
        MetadataKind::ExternalReportObject => Some(MdoType::ExternalReport),
        MetadataKind::ExchangePlanRef | MetadataKind::ExchangePlanObject => {
            Some(MdoType::ExchangePlan)
        }
        MetadataKind::ChartOfAccountsRef | MetadataKind::ChartOfAccountsObject => {
            Some(MdoType::ChartOfAccounts)
        }
        MetadataKind::ChartOfCharacteristicTypesRef
        | MetadataKind::ChartOfCharacteristicTypesObject => {
            Some(MdoType::ChartOfCharacteristicTypes)
        }
        MetadataKind::ChartOfCalculationTypesRef | MetadataKind::ChartOfCalculationTypesObject => {
            Some(MdoType::ChartOfCalculationTypes)
        }
        MetadataKind::InformationRegisterRecordManager
        | MetadataKind::InformationRegisterRecordSet
        | MetadataKind::InformationRegisterRecord
        | MetadataKind::InformationRegisterRef
        | MetadataKind::AccumulationRegisterRecordSet
        | MetadataKind::AccumulationRegisterRecord
        | MetadataKind::AccumulationRegisterRef
        | MetadataKind::AccountingRegisterRecordSet
        | MetadataKind::AccountingRegisterRecord
        | MetadataKind::AccountingRegisterRef
        | MetadataKind::CalculationRegisterRecordSet
        | MetadataKind::CalculationRegisterRecord
        | MetadataKind::CalculationRegisterRef
        | MetadataKind::RegisterDimension { .. }
        | MetadataKind::RegisterResource { .. }
        | MetadataKind::RegisterAttribute { .. }
        | MetadataKind::RegisterFilter { .. }
        | MetadataKind::TabularSection { .. }
        | MetadataKind::TabularSectionRow { .. } => None,
    }
}

pub(crate) fn register_parent_for_kind(kind: MetadataKind) -> Option<MdoType> {
    match kind {
        MetadataKind::InformationRegisterRecordManager
        | MetadataKind::InformationRegisterRecordSet
        | MetadataKind::InformationRegisterRecord
        | MetadataKind::InformationRegisterRef => Some(MdoType::InformationRegister),
        MetadataKind::AccumulationRegisterRecordSet
        | MetadataKind::AccumulationRegisterRecord
        | MetadataKind::AccumulationRegisterRef => Some(MdoType::AccumulationRegister),
        MetadataKind::AccountingRegisterRecordSet
        | MetadataKind::AccountingRegisterRecord
        | MetadataKind::AccountingRegisterRef => Some(MdoType::AccountingRegister),
        MetadataKind::CalculationRegisterRecordSet
        | MetadataKind::CalculationRegisterRecord
        | MetadataKind::CalculationRegisterRef => Some(MdoType::CalculationRegister),
        _ => None,
    }
}

/// Whether the field set of a metadata receiver may be short of a field the platform has,
/// because a common attribute's composition could not be read. A miss on such a receiver
/// proves nothing about the code.
pub(crate) fn metadata_fields_open(
    db: &dyn TypeKernelDb,
    resolver: &dyn MetadataResolution,
    receiver: TypeId,
) -> bool {
    let ty = crate::this_object::coerce_to_metadata_ref_id(db, receiver).unwrap_or(receiver);
    let (kind, name) = match db.lookup_type(ty) {
        TypeKind::MetadataRef(facet) => (facet.kind, facet.name.clone()),
        TypeKind::MetadataObject(facet) => (facet.kind, facet.name.clone()),
        _ => return false,
    };
    if let Some(mdo_type) = mdo_type_for_kind(kind) {
        return resolver
            .resolve_metadata_object(mdo_type, name.as_str())
            .is_some_and(|mdo| mdo.common_attributes_open);
    }
    if let Some(parent) = register_parent_for_kind(kind).or(match kind {
        MetadataKind::RegisterFilter { parent } => Some(parent),
        _ => None,
    }) {
        return resolver
            .resolve_register(parent, name.as_str())
            .is_some_and(|register| register.common_attributes_open());
    }
    false
}

pub(crate) fn split_parent_section(name: &str) -> Option<(&str, &str)> {
    let (parent, section) = name.split_once('.')?;
    if parent.is_empty() || section.is_empty() {
        return None;
    }
    Some((parent, section))
}

pub(crate) fn find_mdo(
    resolver: &dyn ObjectResolver,
    mdo_type: MdoType,
    name: &str,
) -> Option<Arc<MetadataObject>> {
    resolver.resolve_metadata_object(mdo_type, name)
}

pub(crate) fn attribute_type_to_typeid(
    db: &dyn TypeKernelDb,
    attr_type: &AttributeType,
    resolver: &dyn MetadataResolver,
) -> TypeId {
    let type_ref = TypeRef::from_attribute_type(attr_type);
    TyLoweringContext::with_resolver(resolver).lower_type_ref_id(db, &type_ref)
}

pub(crate) fn register_part_typeid(
    db: &dyn TypeKernelDb,
    attr_type: Option<&AttributeType>,
    fallback_kind: MetadataKind,
    register_name: &Name,
    part_name: &str,
    resolver: &dyn MetadataResolver,
) -> TypeId {
    match attr_type {
        Some(at) => attribute_type_to_typeid(db, at, resolver),
        None => db.metadata_ref(
            fallback_kind,
            format!("{}.{}", register_name.as_str(), part_name),
            &RootConfigCtx,
        ),
    }
}

pub(crate) fn mdo_template_kind_for(mdo_type: MdoType) -> Option<MdoTemplateKind> {
    match mdo_type {
        MdoType::Catalog => Some(MdoTemplateKind::Catalog),
        MdoType::Document => Some(MdoTemplateKind::Document),
        MdoType::BusinessProcess => Some(MdoTemplateKind::BusinessProcess),
        MdoType::Task => Some(MdoTemplateKind::Task),
        MdoType::ChartOfAccounts => Some(MdoTemplateKind::ChartOfAccounts),
        MdoType::ChartOfCharacteristicTypes => Some(MdoTemplateKind::ChartOfCharacteristicTypes),
        MdoType::ChartOfCalculationTypes => Some(MdoTemplateKind::ChartOfCalculationTypes),
        MdoType::ExchangePlan => Some(MdoTemplateKind::ExchangePlan),
        MdoType::InformationRegister => Some(MdoTemplateKind::InformationRegister),
        MdoType::AccumulationRegister => Some(MdoTemplateKind::AccumulationRegister),
        MdoType::AccountingRegister => Some(MdoTemplateKind::AccountingRegister),
        MdoType::CalculationRegister => Some(MdoTemplateKind::CalculationRegister),
        _ => None,
    }
}

/// Folded (lowercased) bilingual attribute name -> spec, per template, for the
/// `Object` view. Built once; replaces a per-call linear scan that re-lowercased
/// every spec's Cyrillic `russian_name` on each attribute classification.
fn standard_attr_object_index() -> &'static std::collections::HashMap<
    MdoTemplateKind,
    std::collections::HashMap<String, &'static bsl_platform::StandardAttrSpec>,
> {
    use std::collections::HashMap;
    use std::sync::OnceLock;

    static INDEX: OnceLock<
        HashMap<MdoTemplateKind, HashMap<String, &'static bsl_platform::StandardAttrSpec>>,
    > = OnceLock::new();

    INDEX.get_or_init(|| {
        const TEMPLATES: [MdoTemplateKind; 12] = [
            MdoTemplateKind::Catalog,
            MdoTemplateKind::Document,
            MdoTemplateKind::BusinessProcess,
            MdoTemplateKind::Task,
            MdoTemplateKind::ChartOfAccounts,
            MdoTemplateKind::ChartOfCharacteristicTypes,
            MdoTemplateKind::ChartOfCalculationTypes,
            MdoTemplateKind::ExchangePlan,
            MdoTemplateKind::InformationRegister,
            MdoTemplateKind::AccumulationRegister,
            MdoTemplateKind::AccountingRegister,
            MdoTemplateKind::CalculationRegister,
        ];
        TEMPLATES
            .into_iter()
            .map(|tmpl| {
                let mut by_name: HashMap<String, &'static bsl_platform::StandardAttrSpec> =
                    HashMap::new();
                // First occurrence wins, matching the replaced `.iter().find()` scan.
                for spec in standard_attributes_for(tmpl, ObjectView::Object) {
                    by_name.entry(spec.kind.russian_name().fold_lower()).or_insert(spec);
                    by_name.entry(spec.kind.english_name().fold_lower()).or_insert(spec);
                }
                (tmpl, by_name)
            })
            .collect()
    })
}

fn classify_attr(
    template: Option<MdoTemplateKind>,
    attr_name: &str,
) -> Option<&'static bsl_platform::StandardAttrSpec> {
    let tmpl = template?;
    let needle = attr_name.fold_lower();
    standard_attr_object_index().get(&tmpl)?.get(&needle).copied()
}

fn push_unique(
    out: &mut Vec<FieldInfo>,
    seen: &mut std::collections::HashSet<Name>,
    info: FieldInfo,
) {
    if seen.contains(&info.name) {
        return;
    }
    if let Some(ref en) = info.name_en {
        if seen.contains(en) {
            return;
        }
    }
    seen.insert(info.name.clone());
    if let Some(ref en) = info.name_en {
        seen.insert(en.clone());
    }
    out.push(info);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object_resolver::ConfigsObjectResolver;
    use bsl_config::VisibleConfig;
    use bsl_types::facet::MdoRefFacet;
    use std::rc::Rc;

    #[derive(Clone)]
    struct FieldInfoForTest {
        name: Name,
        ty: ActualType,
        value_ty: Option<ActualType>,
        is_readonly: bool,
        origin: FieldOrigin,
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

    fn enumerate_fields(
        configs: &[VisibleConfig],
        receiver_ty: &TypeFixture,
    ) -> Vec<FieldInfoForTest> {
        let db = Rc::new(InMemoryDb::new());
        let receiver = receiver_ty.intern(&db);
        super::enumerate_fields(db.as_ref(), &ConfigsObjectResolver(configs), receiver)
            .into_iter()
            .map(|info| FieldInfoForTest {
                name: info.name,
                ty: ActualType { db: Rc::clone(&db), id: info.ty },
                value_ty: info.value_ty.map(|id| ActualType { db: Rc::clone(&db), id }),
                is_readonly: info.is_readonly,
                origin: info.origin,
            })
            .collect()
    }
    use bsl_metadata::tabular_section::{TabularSection, TabularSectionAttribute};
    use bsl_metadata::{Attribute, Configuration};
    use bsl_types::testing::InMemoryDb;
    use std::sync::Arc;
    use uuid::Uuid;

    fn metadata_ref(kind: MetadataKind, name: &str) -> TypeFixture {
        let name = Name::new(name);
        TypeFixture::new(format!("MetadataRef({kind:?}, {name})"), move |db| {
            db.metadata_ref(kind, name.to_string(), &RootConfigCtx)
        })
    }

    fn this_object(mdo_type: MdoType, name: &str) -> TypeFixture {
        let name = Name::new(name);
        TypeFixture::new(format!("ThisObject({mdo_type:?}, {name})"), move |db| {
            db.mk_this_object(ConfigId::Root, MdoRefFacet::new(mdo_type, name.to_string()))
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

    fn platform_object(name: &str) -> TypeFixture {
        let name = name.to_string();
        TypeFixture::new(format!("PlatformObject({name})"), move |db| {
            db.platform_object(name.clone())
        })
    }

    fn date() -> TypeFixture {
        TypeFixture::new("Date", |db| db.date(DateComponent::DateTime))
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

    fn assert_value_ty(field: &FieldInfoForTest, expected: TypeFixture) {
        let actual = field.value_ty.as_ref().expect("expected value_ty");
        assert_eq!(actual, &expected);
    }

    #[test]
    fn attribute_typeid_lowers_defined_type_to_underlying_kernel_type() {
        let db = InMemoryDb::new();
        let mut config = Configuration::new("main");
        config.add_defined_type(
            bsl_metadata::DefinedType::builder()
                .uuid(Uuid::new_v4())
                .name("ДенежнаяСумма")
                .underlying_type(AttributeType::Number { precision: 15, scale: 2 })
                .build(),
        );
        let configs = wrap(config);
        let attr_type =
            AttributeType::DefinedType { name: "ДенежнаяСумма".to_string() };
        assert_eq!(
            attribute_type_to_typeid(&db, &attr_type, &ConfigsObjectResolver(&configs)),
            db.number(None, None)
        );
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn platform_object_fields_include_form_extension_properties() {
        let fields = enumerate_fields(
            &[],
            &platform_object("Расширение формы клиентского приложения для документа"),
        );
        let repost = fields
            .iter()
            .find(|field| field.name.as_str() == "ПриЗаписиПерепроводить")
            .expect("document-form extension property");
        assert_eq!(repost.ty, boolean());
        assert!(matches!(repost.origin, FieldOrigin::PlatformProperty { .. }));
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
    fn enumerate_unknown_receiver_returns_empty_vec() {
        let configs = wrap(Configuration::new("Test"));
        for ty in [unknown(), number(), string(), array(), undefined()] {
            assert!(enumerate_fields(&configs, &ty).is_empty(), "no fields on {ty:?}");
        }
    }

    #[test]
    fn enumerate_this_object_coerces_to_object_metadata_ref() {
        let mut config = Configuration::new("Test");
        config.add_metadata_object(catalog(
            "Номенклатура",
            vec![attr("Цена", None, AttributeType::Number { precision: 15, scale: 2 })],
        ));
        let configs = wrap(config);

        let this_obj = this_object(MdoType::Catalog, "Номенклатура");
        let fields = enumerate_fields(&configs, &this_obj);
        assert!(!fields.is_empty(), "ThisObject must coerce and enumerate fields");
        assert!(fields.iter().any(|f| f.name.as_str() == "Цена"), "Цена must appear");
    }

    #[test]
    fn enumerate_catalog_ref_includes_standard_and_user() {
        let mut config = Configuration::new("Test");
        config.add_metadata_object(catalog(
            "Номенклатура",
            vec![
                attr("Код", Some("Code"), AttributeType::String { length: Some(9) }),
                attr("Цена", None, AttributeType::Number { precision: 15, scale: 2 }),
            ],
        ));
        let configs = wrap(config);

        let receiver = metadata_ref(MetadataKind::CatalogRef, "Номенклатура");
        let fields = enumerate_fields(&configs, &receiver);
        assert!(!fields.is_empty());

        let code_field = fields.iter().find(|f| f.name.as_str() == "Код").expect("Код must appear");
        assert_eq!(code_field.origin, FieldOrigin::StandardAttribute);

        let price_field =
            fields.iter().find(|f| f.name.as_str() == "Цена").expect("Цена must appear");
        assert_eq!(price_field.origin, FieldOrigin::UserAttribute);
    }

    #[test]
    fn field_origin_classifies_standard_vs_user_attribute() {
        let template = mdo_template_kind_for(MdoType::Catalog);
        assert!(classify_attr(template, "Код").is_some());
        assert!(classify_attr(template, "Code").is_some());
        assert!(classify_attr(template, "МойРеквизит").is_none());
        let no_template = mdo_template_kind_for(MdoType::Enum);
        assert!(classify_attr(no_template, "Код").is_none());
    }

    #[test]
    fn standard_attributes_carry_readonly_from_platform_spec() {
        let mut cat = MetadataObject::new(MdoType::Catalog, "Справочник1");
        cat.add_attribute(attr(
            "Ссылка",
            Some("Ref"),
            AttributeType::Ref {
                mdo_type: MdoType::Catalog, name: "Справочник1".to_string()
            },
        ));
        cat.add_attribute(attr("Предопределенный", Some("Predefined"), AttributeType::Boolean));
        cat.add_attribute(attr("ПометкаУдаления", Some("DeletionMark"), AttributeType::Boolean));
        cat.add_attribute(attr("МойРеквизит", None, AttributeType::Boolean));
        let mut config = Configuration::new("Test");
        config.add_metadata_object(cat);
        let configs = wrap(config);

        let receiver = metadata_ref(MetadataKind::CatalogRef, "Справочник1");
        let fields = enumerate_fields(&configs, &receiver);

        let by_name = |n: &str| fields.iter().find(|f| f.name.as_str() == n).cloned();
        assert!(
            by_name("Ссылка").expect("Ссылка").is_readonly,
            "Ссылка must be read-only per platform spec"
        );
        assert!(
            by_name("Предопределенный").expect("Предопределенный").is_readonly,
            "Предопределенный must be read-only"
        );
        assert!(!by_name("ПометкаУдаления").expect("ПометкаУдаления").is_readonly);
        assert!(!by_name("МойРеквизит").expect("МойРеквизит").is_readonly);
    }

    #[test]
    fn enumerate_document_object_yields_standard_user_and_tabular_sections() {
        let mut ts = TabularSection::new(Uuid::new_v4(), "Товары");
        ts.set_attributes(vec![]);
        let mut doc = MetadataObject::new(MdoType::Document, "ПКО");
        doc.add_attribute(attr("Дата", Some("Date"), AttributeType::DateTime));
        doc.add_attribute(attr("МойРеквизит", None, AttributeType::Boolean));
        doc.add_tabular_section(ts);

        let mut config = Configuration::new("Test");
        config.add_metadata_object(doc);
        let configs = wrap(config);

        let receiver = metadata_ref(MetadataKind::DocumentRef, "ПКО");
        let fields = enumerate_fields(&configs, &receiver);

        let date_field =
            fields.iter().find(|f| f.name.as_str() == "Дата").expect("Дата must appear");
        assert_eq!(date_field.origin, FieldOrigin::StandardAttribute);

        let my_field = fields
            .iter()
            .find(|f| f.name.as_str() == "МойРеквизит")
            .expect("МойРеквизит must appear");
        assert_eq!(my_field.origin, FieldOrigin::UserAttribute);

        let ts_field =
            fields.iter().find(|f| f.name.as_str() == "Товары").expect("Товары TS must appear");
        assert_eq!(ts_field.origin, FieldOrigin::TabularSection);
        assert_eq!(
            ts_field.ty,
            metadata_ref(MetadataKind::TabularSection { parent: MdoType::Document }, "ПКО.Товары")
        );
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn enumerate_tabular_section_row_yields_columns_and_line_number() {
        let mut ts = TabularSection::new(Uuid::new_v4(), "Услуги");
        ts.set_attributes(vec![TabularSectionAttribute::new(
            Uuid::new_v4(),
            "Количество",
            AttributeType::Number { precision: 15, scale: 3 },
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
        let fields = enumerate_fields(&configs, &receiver);

        let qty = fields
            .iter()
            .find(|f| f.name.as_str() == "Количество")
            .expect("Количество must appear");
        assert_eq!(qty.origin, FieldOrigin::TabularSectionRowColumn);
        assert_eq!(qty.ty, number());

        let nr = fields
            .iter()
            .find(|f| f.name.as_str() == "НомерСтроки")
            .expect("НомерСтроки must appear via platform fall-through");
        assert!(matches!(nr.origin, FieldOrigin::PlatformProperty { .. }));
        assert!(nr.is_readonly);
        assert_eq!(nr.ty, number());
    }

    #[test]
    fn enumerate_information_register_record_includes_dimensions_resources_attributes() {
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
            vec![resource_typed("Количество", AttributeType::Number { precision: 15, scale: 3 })],
            vec![attribute_typed("Комментарий", AttributeType::String { length: Some(100) })],
        ));
        let configs = wrap(config);

        let receiver = metadata_ref(MetadataKind::InformationRegisterRef, "РегистрСведений1");
        let fields = enumerate_fields(&configs, &receiver);

        let dim = fields
            .iter()
            .find(|f| f.name.as_str() == "Справочник1")
            .expect("dimension must appear");
        assert_eq!(dim.origin, FieldOrigin::RegisterDimension);

        let res =
            fields.iter().find(|f| f.name.as_str() == "Количество").expect("resource must appear");
        assert_eq!(res.origin, FieldOrigin::RegisterResource);
        assert_eq!(res.ty, number());

        let att = fields
            .iter()
            .find(|f| f.name.as_str() == "Комментарий")
            .expect("attribute must appear");
        assert_eq!(att.origin, FieldOrigin::RegisterAttribute);
        assert_eq!(att.ty, string());
    }

    #[test]
    fn enumerate_register_record_recorder_uses_document_recorders_union() {
        let mut config = Configuration::new("Test");
        let mut doc1 = MetadataObject::new(MdoType::Document, "Документ1");
        doc1.set_register_records(vec![(MdoType::InformationRegister, "РегистрСведений1".into())]);
        let mut doc2 = MetadataObject::new(MdoType::Document, "Документ2");
        doc2.set_register_records(vec![(MdoType::InformationRegister, "РегистрСведений1".into())]);
        config.add_metadata_object(doc1);
        config.add_metadata_object(doc2);
        config.add_register(register_with(
            "РегистрСведений1",
            MdoType::InformationRegister,
            vec![],
            vec![],
            vec![attribute_typed(
                "Регистратор",
                AttributeType::AnyObjectRef { mdo_type: MdoType::Document },
            )],
        ));
        let configs = wrap(config);

        let receiver = metadata_ref(MetadataKind::InformationRegisterRecord, "РегистрСведений1");
        let fields = enumerate_fields(&configs, &receiver);
        let recorder = fields
            .iter()
            .find(|f| f.name.as_str() == "Регистратор")
            .expect("Регистратор must appear on register record");

        assert_eq!(
            recorder.ty,
            union(vec![
                metadata_ref(MetadataKind::DocumentRef, "Документ1"),
                metadata_ref(MetadataKind::DocumentRef, "Документ2"),
            ]),
        );
    }

    #[test]
    fn enumerate_register_record_recorder_singleton_collapses_to_metadata_ref() {
        let mut config = Configuration::new("Test");
        let mut document = MetadataObject::new(MdoType::Document, "Документ1");
        document
            .set_register_records(vec![(MdoType::InformationRegister, "РегистрСведений1".into())]);
        config.add_metadata_object(document);
        config.add_register(register_with(
            "РегистрСведений1",
            MdoType::InformationRegister,
            vec![],
            vec![],
            vec![attribute_typed(
                "регистратор",
                AttributeType::AnyObjectRef { mdo_type: MdoType::Document },
            )],
        ));
        let configs = wrap(config);

        let receiver = metadata_ref(MetadataKind::InformationRegisterRecord, "РегистрСведений1");
        let fields = enumerate_fields(&configs, &receiver);
        let recorder = fields
            .iter()
            .find(|f| f.name.as_str() == "регистратор")
            .expect("регистратор must appear on register record");

        assert_eq!(recorder.ty, metadata_ref(MetadataKind::DocumentRef, "Документ1"),);
    }

    #[test]
    fn enumerate_register_record_recorder_aggregates_across_visible_extensions() {
        let mut base = Configuration::new("Base");
        let mut doc1 = MetadataObject::new(MdoType::Document, "Документ1");
        doc1.set_register_records(vec![(MdoType::InformationRegister, "РегистрСведений1".into())]);
        base.add_metadata_object(doc1);
        base.add_register(register_with(
            "РегистрСведений1",
            MdoType::InformationRegister,
            vec![],
            vec![],
            vec![attribute_typed(
                "Регистратор",
                AttributeType::AnyObjectRef { mdo_type: MdoType::Document },
            )],
        ));

        let mut ext = Configuration::new("Extension");
        let mut doc2 = MetadataObject::new(MdoType::Document, "Документ2");
        doc2.set_register_records(vec![(MdoType::InformationRegister, "РегистрСведений1".into())]);
        ext.add_metadata_object(doc2);

        let configs = vec![
            VisibleConfig { name: None, configuration: Arc::new(base) },
            VisibleConfig { name: Some("Extension".to_string()), configuration: Arc::new(ext) },
        ];

        let receiver = metadata_ref(MetadataKind::InformationRegisterRecord, "РегистрСведений1");
        let fields = enumerate_fields(&configs, &receiver);
        let recorder = fields
            .iter()
            .find(|f| f.name.as_str() == "Регистратор")
            .expect("Регистратор must appear on register record");

        let TypeKind::Union(parts) = recorder.ty.db.lookup_type(recorder.ty.id) else {
            panic!("expected TypeKind::Union for cross-config recorders, got {:?}", recorder.ty);
        };
        let names: std::collections::HashSet<&str> = parts
            .iter()
            .filter_map(|id| match recorder.ty.db.lookup_type(*id) {
                TypeKind::MetadataRef(facet) if facet.kind == MetadataKind::DocumentRef => {
                    Some(facet.name.as_str())
                }
                _ => None,
            })
            .collect();
        assert!(names.contains("Документ1"), "base recorder must be present, got {names:?}");
        assert!(names.contains("Документ2"), "extension recorder must be present, got {names:?}");
        assert_eq!(names.len(), 2, "exactly two recorders expected, got {names:?}");
    }

    #[test]
    fn enumerate_filter_fields_adds_standard_accumulation_keys_after_dimensions() {
        let mut config = Configuration::new("Test");
        config.add_register(register_with(
            "РегистрНакопления1",
            MdoType::AccumulationRegister,
            vec![dimension_typed("Регистратор", AttributeType::String { length: Some(10) })],
            vec![],
            vec![],
        ));
        let configs = wrap(config);

        let receiver = metadata_ref(
            MetadataKind::RegisterFilter { parent: MdoType::AccumulationRegister },
            "РегистрНакопления1",
        );
        let fields = enumerate_fields(&configs, &receiver);

        for key in ["Период", "Регистратор", "НомерСтроки", "Активность"]
        {
            assert!(
                fields.iter().any(|f| f.name.as_str() == key),
                "{key} must be exposed on accumulation register filter",
            );
        }
        let recorder = fields
            .iter()
            .find(|f| f.name.as_str() == "Регистратор")
            .expect("Регистратор filter member");
        assert_eq!(
            recorder.origin,
            FieldOrigin::RegisterDimension,
            "dimension must win over the standard filter key with the same name",
        );
    }

    #[test]
    fn enumerate_filter_fields_value_ty_for_information_register() {
        let register = bsl_metadata::Register::builder()
            .name("Курсы")
            .mdo_type(MdoType::InformationRegister)
            .periodicity(Some(RegisterPeriodicity::Second))
            .add_dimension(dimension_typed(
                "Валюта",
                AttributeType::Ref { mdo_type: MdoType::Catalog, name: "Валюты".into() },
            ))
            .add_dimension(dimension_typed(
                "Цена",
                AttributeType::Number { precision: 15, scale: 2 },
            ))
            .build();
        let mut config = Configuration::new("Test");
        config.add_register(register);
        let configs = wrap(config);

        let receiver = metadata_ref(
            MetadataKind::RegisterFilter { parent: MdoType::InformationRegister },
            "Курсы",
        );
        let fields = enumerate_fields(&configs, &receiver);

        let period = fields.iter().find(|f| f.name.as_str() == "Период").expect("Период");
        assert_value_ty(period, date());
        let active = fields.iter().find(|f| f.name.as_str() == "Активность").expect("Активность");
        assert_value_ty(active, boolean());
        let currency = fields.iter().find(|f| f.name.as_str() == "Валюта").expect("Валюта");
        assert_value_ty(currency, metadata_ref(MetadataKind::CatalogRef, "Валюты"));
        let price = fields.iter().find(|f| f.name.as_str() == "Цена").expect("Цена");
        assert_value_ty(price, number());
    }

    #[test]
    fn enumerate_filter_fields_value_ty_for_accumulation_register() {
        let mut config = Configuration::new("Test");
        let mut document = MetadataObject::new(MdoType::Document, "Поступление");
        document.set_register_records(vec![(MdoType::AccumulationRegister, "Остатки".into())]);
        config.add_metadata_object(document);
        config.add_register(register_with(
            "Остатки",
            MdoType::AccumulationRegister,
            vec![],
            vec![],
            vec![],
        ));
        let configs = wrap(config);

        let receiver = metadata_ref(
            MetadataKind::RegisterFilter { parent: MdoType::AccumulationRegister },
            "Остатки",
        );
        let fields = enumerate_fields(&configs, &receiver);

        let recorder =
            fields.iter().find(|f| f.name.as_str() == "Регистратор").expect("Регистратор");
        assert_value_ty(recorder, metadata_ref(MetadataKind::DocumentRef, "Поступление"));
    }

    #[test]
    fn enumerate_filter_fields_dimension_named_period_wins_over_standard_key() {
        let register = bsl_metadata::Register::builder()
            .name("Срез")
            .mdo_type(MdoType::InformationRegister)
            .periodicity(Some(RegisterPeriodicity::Second))
            .add_dimension(dimension_typed(
                "Период",
                AttributeType::Number { precision: 10, scale: 0 },
            ))
            .build();
        let mut config = Configuration::new("Test");
        config.add_register(register);
        let configs = wrap(config);

        let receiver = metadata_ref(
            MetadataKind::RegisterFilter { parent: MdoType::InformationRegister },
            "Срез",
        );
        let fields = enumerate_fields(&configs, &receiver);
        let period = fields.iter().find(|f| f.name.as_str() == "Период").expect("Период");

        assert_eq!(period.origin, FieldOrigin::RegisterDimension);
        assert_value_ty(period, number());
    }

    #[test]
    fn enumerate_filter_fields_calculation_register_uses_correct_standard_keys() {
        let mut config = Configuration::new("Test");
        config.add_register(register_with(
            "РегистрРасчета1",
            MdoType::CalculationRegister,
            vec![],
            vec![],
            vec![],
        ));
        let configs = wrap(config);

        let receiver = metadata_ref(
            MetadataKind::RegisterFilter { parent: MdoType::CalculationRegister },
            "РегистрРасчета1",
        );
        let fields = enumerate_fields(&configs, &receiver);
        let names: Vec<&str> = fields.iter().map(|f| f.name.as_str()).collect();

        assert_eq!(
            names,
            vec!["Регистратор", "НомерСтроки", "Активность", "ВидРасчета", "ПериодРегистрации"],
        );
        assert_eq!(
            standard_filter_keys(MdoType::CalculationRegister, Some(RegisterPeriodicity::Month)),
            standard_filter_keys(MdoType::CalculationRegister, None),
        );
    }

    #[test]
    fn enumerate_filter_fields_recorder_position_information_register_uses_register_keys() {
        let register = bsl_metadata::Register::builder()
            .name("ПозицияРегистратора")
            .mdo_type(MdoType::InformationRegister)
            .periodicity(Some(RegisterPeriodicity::RecorderPosition))
            .build();
        let mut config = Configuration::new("Test");
        config.add_register(register);
        let configs = wrap(config);

        let receiver = metadata_ref(
            MetadataKind::RegisterFilter { parent: MdoType::InformationRegister },
            "ПозицияРегистратора",
        );
        let fields = enumerate_fields(&configs, &receiver);
        let names: Vec<&str> = fields.iter().map(|f| f.name.as_str()).collect();

        assert_eq!(names, vec!["Регистратор", "Активность", "Период"]);
    }

    #[test]
    fn enumerate_filter_fields_adds_recorder_only_for_recorder_subordinate_info_register() {
        for (name, recorder_subordinate, expected) in
            [("Подчиненный", true, true), ("Независимый", false, false)]
        {
            let register = bsl_metadata::Register::builder()
                .name(name)
                .mdo_type(MdoType::InformationRegister)
                .recorder_subordinate(recorder_subordinate)
                .build();
            let mut config = Configuration::new("Test");
            config.add_register(register);
            let configs = wrap(config);
            let receiver = metadata_ref(
                MetadataKind::RegisterFilter { parent: MdoType::InformationRegister },
                name,
            );
            let fields = enumerate_fields(&configs, &receiver);
            assert_eq!(
                fields.iter().any(|field| field.name.as_str() == "Регистратор"),
                expected,
                "register filter recorder availability for {name}",
            );
        }
    }

    #[test]
    fn enumerate_filter_fields_adds_period_for_periodic_information_register() {
        let config = {
            let register = bsl_metadata::Register::builder()
                .name("ПериодическийРегистр")
                .mdo_type(MdoType::InformationRegister)
                .periodicity(Some(RegisterPeriodicity::Second))
                .build();
            let mut config = Configuration::new("Test");
            config.add_register(register);
            config
        };
        let configs = wrap(config);

        let receiver = metadata_ref(
            MetadataKind::RegisterFilter { parent: MdoType::InformationRegister },
            "ПериодическийРегистр",
        );
        let fields = enumerate_fields(&configs, &receiver);

        assert!(fields.iter().any(|f| f.name.as_str() == "Период"));
        assert!(fields.iter().any(|f| f.name.as_str() == "Активность"));
        assert!(!fields.iter().any(|f| f.name.as_str() == "Регистратор"));
    }

    #[test]
    fn enumerate_union_with_metadata_ref_arm_yields_fields() {
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
        let fields = enumerate_fields(&configs, &receiver);
        assert!(
            fields.iter().any(|f| f.name.as_str() == "Номенклатура"),
            "Union(MetadataRef.Row, Undefined) must surface row columns"
        );
    }

    #[test]
    fn document_attribute_typed_via_defined_type_lowers_to_underlying() {
        let mut config = Configuration::new("Test");
        config.add_defined_type(
            bsl_metadata::DefinedType::builder()
                .uuid(Uuid::new_v4())
                .name("ДенежнаяСуммаЛюбогоЗнака")
                .underlying_type(AttributeType::Number { precision: 15, scale: 2 })
                .build(),
        );
        config.add_metadata_object({
            let mut doc = MetadataObject::new(MdoType::Document, "ПКО");
            doc.add_attribute(attr(
                "СуммаДокумента",
                None,
                AttributeType::DefinedType {
                    name: "ДенежнаяСуммаЛюбогоЗнака".to_string()
                },
            ));
            doc
        });
        let configs = wrap(config);

        let receiver = metadata_ref(MetadataKind::DocumentRef, "ПКО");
        let fields = enumerate_fields(&configs, &receiver);
        let sum =
            fields.iter().find(|f| f.name.as_str() == "СуммаДокумента").expect("СуммаДокумента");
        assert_eq!(
            sum.ty,
            number(),
            "DefinedType-typed attribute must resolve to its underlying `Ty::Number`"
        );
    }

    #[test]
    fn extension_overrides_main_on_collision() {
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
        let fields = enumerate_fields(&configs, &receiver);
        let цена = fields.iter().find(|f| f.name.as_str() == "Цена").expect("Цена must appear");
        assert_eq!(цена.ty, string(), "extension type must win over main config");
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn point_in_time_present_on_three_record_flavours_absent_on_calc() {
        let pd = PlatformData::instance();
        let has_pit = |prefix: &str| {
            pd.get_manager_methods(prefix).iter().any(|m| {
                m.english_name
                    .as_str()
                    .rsplit('.')
                    .next()
                    .map(|tail| tail == "PointInTime")
                    .unwrap_or(false)
            })
        };
        assert!(has_pit("InformationRegisterRecord"), "InfoReg record must expose МоментВремени");
        assert!(has_pit("AccumulationRegisterRecord"), "AccumReg record must expose МоментВремени");
        assert!(has_pit("AccountingRegisterRecord"), "AcctReg record must expose МоментВремени");
        assert!(
            !has_pit("CalculationRegisterRecord"),
            "CalcReg record must NOT expose МоментВремени per HBK 8.3.27 — \
             surfacing one indicates a regression in platform_data or a wrong-prefix lookup",
        );
    }
}
