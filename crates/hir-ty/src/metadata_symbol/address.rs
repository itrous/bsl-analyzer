use bsl_metadata::MdoType;
use bsl_types::builders::Builders;
use bsl_types::intern::TypeKernelDb;
use bsl_types::kind::MetadataKind;
use bsl_types::testing::RootConfigCtx;
use hir_def::Name;

use super::{metadata_path_for_receiver, MetadataPathResolution, MetadataSymbolPath};
use crate::object_resolver::MetadataResolution;

/// A qualified metadata address uses the same object and field resolver as a receiver.
pub fn metadata_path_for_address(
    db: &dyn TypeKernelDb,
    resolver: &dyn MetadataResolution,
    address: &str,
) -> MetadataPathResolution {
    let parts = address.split('.').collect::<Vec<_>>();
    if parts.len() < 2 || parts.iter().any(|part| part.is_empty()) {
        return MetadataPathResolution::NonMetadata;
    }
    let kind = match parts[0].parse::<MdoType>().ok().or_else(|| MdoType::from_plural(parts[0])) {
        Some(kind) => kind,
        None => return MetadataPathResolution::NonMetadata,
    };
    let name = resolver
        .resolve_metadata_object(kind, parts[1])
        .map(|object| object.name.clone())
        .or_else(|| {
            resolver.resolve_register(kind, parts[1]).map(|register| register.name().to_string())
        });
    let Some(name) = name else { return MetadataPathResolution::Unresolved };
    let root =
        MetadataSymbolPath { owner_kind: kind, owner_name: Name::new(&name), members: Vec::new() };
    if parts.len() == 2 {
        return MetadataPathResolution::Resolved(root);
    }
    let mut receiver = if kind == MdoType::Enum {
        db.object_manager(kind, name, &RootConfigCtx)
    } else {
        let Some(facet) =
            MetadataKind::object_kind_for(kind).or_else(|| MetadataKind::record_set_kind_for(kind))
        else {
            return MetadataPathResolution::Unresolved;
        };
        db.metadata_ref(facet, name, &RootConfigCtx)
    };
    let mut result = MetadataPathResolution::Unresolved;
    for (index, part) in parts[2..].iter().enumerate() {
        if let Some(target) = crate::form_items::data_path_value_target(db, receiver) {
            if let Some(row) = crate::form_items::row_typeid_of_tabular_section_target(db, &target)
            {
                receiver = row;
            }
        }
        let member = Name::new(part);
        result = metadata_path_for_receiver(db, resolver, receiver, &member);
        let Some(field) = crate::lookup_field(db, resolver, receiver, &member) else {
            return if index + 3 == parts.len() {
                result
            } else {
                MetadataPathResolution::Unresolved
            };
        };
        receiver = field.ty;
    }
    result
}
