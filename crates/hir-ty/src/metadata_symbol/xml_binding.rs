use bsl_metadata::{AttributeType, FormAttribute, PlatformValueType};
use bsl_types::intern::TypeKernelDb;
use bsl_types::kind::TypeId;
use hir_def::Name;

use super::{metadata_path_for_receiver, MetadataPathResolution};
use crate::object_resolver::MetadataResolution;

/// Follow the same form-data projection and field types used by form inference.
pub fn metadata_paths_for_form_binding(
    db: &dyn TypeKernelDb,
    resolver: &dyn MetadataResolution,
    attribute: &FormAttribute,
    members: &[Name],
) -> Vec<MetadataPathResolution> {
    if !attribute.is_main
        && matches!(
            attribute.attr_type,
            AttributeType::Platform(PlatformValueType::ValueTable | PlatformValueType::ValueTree)
        )
        && !members.is_empty()
        && !attribute.columns.is_empty()
    {
        let columns = attribute
            .columns
            .iter()
            .filter(|column| stdx::case::eq_ignore_case(&column.name, members[0].as_str()))
            .collect::<Vec<_>>();
        let [column] = columns.as_slice() else {
            return vec![MetadataPathResolution::Unresolved; members.len()];
        };
        let receiver = crate::field_enum::attribute_type_to_typeid(db, &column.attr_type, resolver);
        let mut paths = vec![MetadataPathResolution::NonMetadata];
        paths.extend(metadata_paths_for_value(db, resolver, receiver, &members[1..]));
        return paths;
    }
    let receiver = crate::form_attr::lower_form_attribute_to_typeid(db, attribute, resolver);
    metadata_paths_for_value(db, resolver, receiver, members)
}

pub fn metadata_paths_for_sdbl_value(
    db: &dyn TypeKernelDb,
    resolver: &dyn MetadataResolution,
    value: &sdbl_hir::SdblType,
    members: &[Name],
) -> Vec<MetadataPathResolution> {
    metadata_paths_for_value(
        db,
        resolver,
        crate::sdbl_bridge::sdbl_type_to_typeid(db, value),
        members,
    )
}

fn metadata_paths_for_value(
    db: &dyn TypeKernelDb,
    resolver: &dyn MetadataResolution,
    mut receiver: TypeId,
    members: &[Name],
) -> Vec<MetadataPathResolution> {
    let mut paths = Vec::with_capacity(members.len());
    for member in members {
        if let Some(target) = crate::form_items::data_path_value_target(db, receiver) {
            if let Some(row) = crate::form_items::row_typeid_of_tabular_section_target(db, &target)
            {
                receiver = row;
            }
        }
        paths.push(metadata_path_for_receiver(db, resolver, receiver, member));
        let Some(field) = crate::lookup_field(db, resolver, receiver, member) else {
            paths.resize(members.len(), MetadataPathResolution::Unresolved);
            break;
        };
        receiver = field.ty;
    }
    paths
}
