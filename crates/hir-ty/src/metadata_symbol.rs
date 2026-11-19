//! Metadata paths proven by the existing receiver type and field resolver.

mod address;
mod xml_binding;

pub use address::metadata_path_for_address;
pub use xml_binding::{metadata_paths_for_form_binding, metadata_paths_for_sdbl_value};

use bsl_metadata::declaration_identity::DeclarationSegment;
use bsl_metadata::MdoType;
use bsl_types::intern::TypeKernelDb;
use bsl_types::kind::{MetadataKind, TypeId, TypeKind};
use hir_def::Name;

use crate::field_enum::{mdo_type_for_kind, register_parent_for_kind, FieldOrigin};
use crate::object_resolver::MetadataResolution;

pub fn metadata_path_for_implicit_field(
    db: &dyn crate::db::HirDatabase,
    file_id: vfs::FileId,
    name: &Name,
) -> MetadataPathResolution {
    let fields = crate::module_implicit::module_implicit_fields(db, file_id);
    let mut found = fields.iter().filter(|field| {
        field.name.eq_ignore_case(name)
            || field.name_en.as_ref().is_some_and(|alias| alias.eq_ignore_case(name))
    });
    let Some(field) = found.next() else { return MetadataPathResolution::NonMetadata };
    if found.next().is_some() {
        return MetadataPathResolution::Ambiguous;
    }
    let member_kind = match field.origin {
        FieldOrigin::UserAttribute => "Attribute",
        FieldOrigin::TabularSection => "TabularSection",
        FieldOrigin::RegisterDimension => "Dimension",
        FieldOrigin::RegisterResource => "Resource",
        FieldOrigin::RegisterAttribute => "Attribute",
        _ => return MetadataPathResolution::NonMetadata,
    };
    let metadata = db.module_metadata(hir_def::ModuleId::new(file_id));
    let owner =
        metadata.mdo.as_ref().map(|object| (object.mdo_type, Name::new(&object.name))).or_else(
            || {
                metadata
                    .register
                    .as_ref()
                    .map(|register| (register.mdo_type(), Name::new(register.name())))
            },
        );
    let Some((owner_kind, owner_name)) = owner else { return MetadataPathResolution::Unresolved };
    MetadataPathResolution::Resolved(MetadataSymbolPath {
        owner_kind,
        owner_name,
        members: vec![DeclarationSegment {
            kind: member_kind.into(),
            name: field.name.to_string(),
        }],
    })
}

/// A semantic lookup request; a source declaration must still establish its identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataSymbolPath {
    pub owner_kind: MdoType,
    pub owner_name: Name,
    pub members: Vec<DeclarationSegment>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetadataPathResolution {
    Resolved(MetadataSymbolPath),
    NonMetadata,
    Unresolved,
    Ambiguous,
}

pub fn is_query_receiver_type(db: &dyn TypeKernelDb, receiver: TypeId) -> bool {
    match db.lookup_type(receiver) {
        TypeKind::Query { .. } => true,
        TypeKind::PlatformObject(facet) => matches!(facet.name.as_str(), "Запрос" | "Query"),
        _ => false,
    }
}

pub fn metadata_receiver_has_members(db: &dyn TypeKernelDb, receiver: TypeId) -> bool {
    let receiver =
        crate::field_lookup::project_form_data_for_fields_id(db, receiver).unwrap_or(receiver);
    let receiver = crate::this_object::coerce_to_metadata_ref_id(db, receiver).unwrap_or(receiver);
    match db.lookup_type(receiver) {
        TypeKind::ManagerCollection(_)
        | TypeKind::MetadataObjectCollection(_)
        | TypeKind::MetadataRef(_)
        | TypeKind::MetadataObject(_)
        | TypeKind::TabularSectionRow { .. } => true,
        TypeKind::Union(arms) => arms.iter().any(|arm| metadata_receiver_has_members(db, *arm)),
        _ => false,
    }
}

/// Declared spellings narrow the name index; they never establish reference identity.
pub fn metadata_reference_names(
    resolver: &dyn MetadataResolution,
    path: &MetadataSymbolPath,
) -> Vec<Name> {
    let mut names = Vec::new();
    let mut add = |name: &str, english: Option<&str>| {
        names.push(Name::new(name));
        if let Some(english) = english {
            names.push(Name::new(english));
        }
    };
    if let Some(object) =
        resolver.resolve_metadata_object(path.owner_kind, path.owner_name.as_str())
    {
        match path.members.as_slice() {
            [] => add(&object.name, object.name_en.as_deref()),
            [member] if member.kind == "Attribute" => {
                if let Some(attribute) = object.find_attribute(&member.name) {
                    add(&attribute.name, attribute.name_en.as_deref());
                }
            }
            [member] if member.kind == "TabularSection" => {
                if let Some(section) = object.find_tabular_section(&member.name) {
                    add(section.name(), section.name_en());
                }
            }
            [member] if member.kind == "EnumValue" => {
                if let Some(value) = object
                    .enum_values
                    .iter()
                    .find(|value| stdx::case::eq_ignore_case(&value.name, &member.name))
                {
                    add(&value.name, value.name_en.as_deref());
                }
            }
            [section, member] if section.kind == "TabularSection" && member.kind == "Attribute" => {
                if let Some(section) = object.find_tabular_section(&section.name) {
                    if let Some(attribute) = section
                        .attributes()
                        .iter()
                        .find(|a| stdx::case::eq_ignore_case(a.name(), &member.name))
                    {
                        add(attribute.name(), attribute.name_en());
                    }
                }
            }
            _ => {}
        }
    } else if let Some(register) =
        resolver.resolve_register(path.owner_kind, path.owner_name.as_str())
    {
        match path.members.as_slice() {
            [] => add(register.name(), None),
            [member] => {
                for dimension in register.dimensions().iter().filter(|d| {
                    member.kind == "Dimension" && stdx::case::eq_ignore_case(d.name(), &member.name)
                }) {
                    add(dimension.name(), None);
                }
                for resource in register.resources().iter().filter(|r| {
                    member.kind == "Resource" && stdx::case::eq_ignore_case(r.name(), &member.name)
                }) {
                    add(resource.name(), resource.name_en());
                }
                for attribute in register.attributes().iter().filter(|a| {
                    member.kind == "Attribute" && stdx::case::eq_ignore_case(a.name(), &member.name)
                }) {
                    add(attribute.name(), attribute.name_en());
                }
            }
            _ => {}
        }
    }
    names
}

/// Resolve a member only through the same type/field facts used by inference.
pub fn metadata_path_for_receiver(
    db: &dyn TypeKernelDb,
    resolver: &dyn MetadataResolution,
    receiver: TypeId,
    name: &Name,
) -> MetadataPathResolution {
    let receiver =
        crate::field_lookup::project_form_data_for_fields_id(db, receiver).unwrap_or(receiver);
    let receiver = crate::this_object::coerce_to_metadata_ref_id(db, receiver).unwrap_or(receiver);
    if let TypeKind::Union(arms) = db.lookup_type(receiver) {
        let mut resolved = None;
        let mut non_metadata = false;
        for arm in arms.iter().copied().filter(|arm| {
            !matches!(db.lookup_type(*arm), TypeKind::Null | TypeKind::Undefined | TypeKind::Never)
        }) {
            match metadata_path_for_receiver(db, resolver, arm, name) {
                MetadataPathResolution::Resolved(path) => match &resolved {
                    _ if non_metadata => return MetadataPathResolution::Ambiguous,
                    Some(existing) if existing != &path => {
                        return MetadataPathResolution::Ambiguous
                    }
                    _ => resolved = Some(path),
                },
                MetadataPathResolution::NonMetadata => {
                    if resolved.is_some() {
                        return MetadataPathResolution::Ambiguous;
                    }
                    non_metadata = true;
                }
                other => return other,
            }
        }
        return resolved.map_or(
            if non_metadata {
                MetadataPathResolution::NonMetadata
            } else {
                MetadataPathResolution::Unresolved
            },
            MetadataPathResolution::Resolved,
        );
    }
    let kind = db.lookup_type(receiver);
    if let TypeKind::ManagerCollection(owner_kind)
    | TypeKind::MetadataObjectCollection(owner_kind) = kind
    {
        let owner_kind = *owner_kind;
        let owner_name = resolver
            .resolve_metadata_object(owner_kind, name.as_str())
            .map(|object| Name::new(&object.name))
            .or_else(|| {
                resolver
                    .resolve_register(owner_kind, name.as_str())
                    .map(|register| Name::new(register.name()))
            });
        return owner_name.map_or(MetadataPathResolution::Unresolved, |owner_name| {
            MetadataPathResolution::Resolved(MetadataSymbolPath {
                owner_kind,
                owner_name,
                members: Vec::new(),
            })
        });
    }
    let (owner_kind, owner_name, section) = match kind {
        TypeKind::MetadataRef(facet) => {
            if let MetadataKind::TabularSectionRow { parent } = facet.kind {
                let Some((owner, section)) = crate::field_enum::split_parent_section(&facet.name)
                else {
                    return MetadataPathResolution::Unresolved;
                };
                (parent, Name::new(owner), Some(section.to_string()))
            } else {
                let Some(owner) =
                    mdo_type_for_kind(facet.kind).or_else(|| register_parent_for_kind(facet.kind))
                else {
                    return MetadataPathResolution::NonMetadata;
                };
                (owner, Name::new(&facet.name), None)
            }
        }
        TypeKind::MetadataObject(facet) => {
            let Some(owner) =
                mdo_type_for_kind(facet.kind).or_else(|| register_parent_for_kind(facet.kind))
            else {
                return MetadataPathResolution::NonMetadata;
            };
            (owner, Name::new(&facet.name), None)
        }
        TypeKind::ObjectManager(facet) if facet.mdo == MdoType::Enum => {
            let Some(object) = resolver.resolve_metadata_object(facet.mdo, facet.name.as_str())
            else {
                return MetadataPathResolution::Unresolved;
            };
            let mut matches = object.enum_values.iter().filter(|value| {
                stdx::case::eq_ignore_case(&value.name, name.as_str())
                    || value
                        .name_en
                        .as_deref()
                        .is_some_and(|alias| stdx::case::eq_ignore_case(alias, name.as_str()))
            });
            let Some(value) = matches.next() else { return MetadataPathResolution::NonMetadata };
            if matches.next().is_some() {
                return MetadataPathResolution::Ambiguous;
            }
            return MetadataPathResolution::Resolved(MetadataSymbolPath {
                owner_kind: facet.mdo,
                owner_name: Name::new(&object.name),
                members: vec![DeclarationSegment {
                    kind: "EnumValue".into(),
                    name: value.name.clone(),
                }],
            });
        }
        TypeKind::TabularSectionRow { parent, name: section }
        | TypeKind::TabularSection { parent, name: section } => {
            let Some(owner) =
                mdo_type_for_kind(parent.kind).or_else(|| register_parent_for_kind(parent.kind))
            else {
                return MetadataPathResolution::NonMetadata;
            };
            (owner, Name::new(&parent.name), Some(section.clone()))
        }
        TypeKind::Unknown | TypeKind::Any | TypeKind::AnyRef | TypeKind::AnyMetadataRef { .. } => {
            return MetadataPathResolution::Unresolved
        }
        _ => return MetadataPathResolution::NonMetadata,
    };
    let owner_name = resolver
        .resolve_metadata_object(owner_kind, owner_name.as_str())
        .map(|object| Name::new(&object.name))
        .or_else(|| {
            resolver
                .resolve_register(owner_kind, owner_name.as_str())
                .map(|register| Name::new(register.name()))
        })
        .unwrap_or(owner_name);
    let Some(field) = crate::lookup_field(db, resolver, receiver, name) else {
        return MetadataPathResolution::Unresolved;
    };
    let member_kind = match field.origin {
        FieldOrigin::UserAttribute | FieldOrigin::TabularSectionRowColumn => "Attribute",
        FieldOrigin::TabularSection => "TabularSection",
        FieldOrigin::RegisterDimension => "Dimension",
        FieldOrigin::RegisterResource => "Resource",
        FieldOrigin::RegisterAttribute => "Attribute",
        _ => return MetadataPathResolution::NonMetadata,
    };
    let mut members = Vec::new();
    if let Some(section) = section {
        members
            .push(DeclarationSegment { kind: "TabularSection".into(), name: section.to_string() });
    }
    members.push(DeclarationSegment { kind: member_kind.into(), name: field.name.to_string() });
    MetadataPathResolution::Resolved(MetadataSymbolPath { owner_kind, owner_name, members })
}
