use bsl_metadata::MdoType;
use hir::{DefDatabase, MetadataKind, Name, Semantics, TypeId, TypeKernelDb, TypeKind};
use ide_db::{metadata, RootDatabaseImpl};
use syntax::{TextRange, TextSize};
use vfs::FileId;

use crate::goto_definition::{
    metadata_reference_name_range, metadata_reference_to_navigation_target,
};
use crate::{NavigationTarget, SymbolKind};

/// Navigate from the expression under the cursor to the definition of its TYPE:
/// a metadata object's XML (catalogs, documents, registers, …) or a common
/// module's source. Uses only the open file's inferred types; platform-only or
/// opaque types (`Число`, `Строка`, `ТаблицаЗначений`, …) yield `None`.
pub fn type_definition(
    db: &RootDatabaseImpl,
    file_id: FileId,
    offset: TextSize,
) -> Option<NavigationTarget> {
    let _span =
        tracing::info_span!("type_definition", ?file_id, offset = u32::from(offset)).entered();

    let sema = Semantics::new(db);
    let ty = sema.symbol_at(file_id, offset)?.ty?;
    type_to_navigation_target(db, file_id, ty)
}

fn type_to_navigation_target(
    db: &RootDatabaseImpl,
    from_file_id: FileId,
    ty: TypeId,
) -> Option<NavigationTarget> {
    // Flat metadata references (roles, subscriptions, HTTP/web services, …) already
    // have a goto helper that resolves their XML.
    if let Some(nav) = metadata_reference_to_navigation_target(db, from_file_id, ty) {
        return Some(nav);
    }

    match db.lookup_type(ty) {
        TypeKind::CommonModule(facet) => {
            common_module_target(db, from_file_id, facet.name.as_str())
        }
        kind => {
            let (mdo_type, name) = mdo_target(kind)?;
            navigate_to_mdo(db, mdo_type, &name)
        }
    }
}

/// The `(MdoType, name)` a navigable metadata type points at. Reference and
/// object types point at the object itself; a tabular section / register
/// dimension / attribute points at its owning object.
fn mdo_target(kind: &TypeKind) -> Option<(MdoType, String)> {
    match kind {
        TypeKind::MetadataRef(facet) => {
            Some((mdo_type_of_kind(facet.kind)?, owner_object_name(&facet.name)))
        }
        TypeKind::MetadataObject(facet) => {
            Some((mdo_type_of_kind(facet.kind)?, owner_object_name(&facet.name)))
        }
        TypeKind::TabularSection { parent, .. }
        | TypeKind::TabularSectionRow { parent, .. }
        | TypeKind::RegisterDimension { parent, .. }
        | TypeKind::RegisterResource { parent, .. }
        | TypeKind::RegisterAttribute { parent, .. }
        | TypeKind::RegisterFilter { parent }
        | TypeKind::Attribute { parent, .. } => {
            Some((mdo_type_of_kind(parent.kind)?, owner_object_name(&parent.name)))
        }
        TypeKind::ThisObject { owner, .. } | TypeKind::ThisManager { owner, .. } => {
            Some((owner.mdo_type, owner_object_name(&owner.name)))
        }
        TypeKind::ObjectManager(facet) => Some((facet.mdo, owner_object_name(&facet.name))),
        _ => None,
    }
}

/// A metadata-ref name for a tabular section or register part is qualified
/// (`Объект.Секция`); the navigable object is the part before the first dot. A
/// plain object ref has no dot and passes through unchanged.
fn owner_object_name(name: &str) -> String {
    name.split('.').next().unwrap_or(name).to_string()
}

/// Map a type-kernel `MetadataKind` (ref, object, or record-set form) to the
/// `MdoType` that identifies the object family on disk.
fn mdo_type_of_kind(kind: MetadataKind) -> Option<MdoType> {
    if let Some(mdo) = kind.ref_mdo_type() {
        return Some(mdo);
    }
    Some(match kind {
        MetadataKind::CatalogObject => MdoType::Catalog,
        MetadataKind::DocumentObject => MdoType::Document,
        MetadataKind::TaskObject => MdoType::Task,
        MetadataKind::BusinessProcessObject => MdoType::BusinessProcess,
        MetadataKind::ExchangePlanObject => MdoType::ExchangePlan,
        MetadataKind::ChartOfAccountsObject => MdoType::ChartOfAccounts,
        MetadataKind::ChartOfCharacteristicTypesObject => MdoType::ChartOfCharacteristicTypes,
        MetadataKind::ChartOfCalculationTypesObject => MdoType::ChartOfCalculationTypes,
        MetadataKind::DataProcessorObject => MdoType::DataProcessor,
        MetadataKind::ReportObject => MdoType::Report,
        MetadataKind::ConstantValueManager => MdoType::Constant,
        MetadataKind::InformationRegisterRecordSet
        | MetadataKind::InformationRegisterRecordManager
        | MetadataKind::InformationRegisterRecord => MdoType::InformationRegister,
        MetadataKind::AccumulationRegisterRecordSet | MetadataKind::AccumulationRegisterRecord => {
            MdoType::AccumulationRegister
        }
        MetadataKind::AccountingRegisterRecordSet | MetadataKind::AccountingRegisterRecord => {
            MdoType::AccountingRegister
        }
        MetadataKind::CalculationRegisterRecordSet | MetadataKind::CalculationRegisterRecord => {
            MdoType::CalculationRegister
        }
        // Tabular sections and register parts are typed as a qualified `MetadataRef`
        // whose kind carries the owning object's `MdoType`; navigate to the owner.
        MetadataKind::TabularSection { parent }
        | MetadataKind::TabularSectionRow { parent }
        | MetadataKind::RegisterDimension { parent }
        | MetadataKind::RegisterResource { parent }
        | MetadataKind::RegisterAttribute { parent }
        | MetadataKind::RegisterFilter { parent } => parent,
        _ => return None,
    })
}

fn navigate_to_mdo(
    db: &RootDatabaseImpl,
    mdo_type: MdoType,
    name: &str,
) -> Option<NavigationTarget> {
    let file_id = mdo_xml_file(db, mdo_type, name)?;
    let range = metadata_reference_name_range(db, file_id, name)
        .unwrap_or_else(|| TextRange::empty(TextSize::from(0)));
    Some(NavigationTarget { file_id, range, name: name.to_string(), kind: SymbolKind::Variable })
}

/// The object's `<Name>.xml` file id, taken from the metadata substrate listing
/// across all config roots (base first, then extensions).
fn mdo_xml_file(db: &RootDatabaseImpl, mdo_type: MdoType, name: &str) -> Option<FileId> {
    // Base configuration first, then extensions — the base holds an object's
    // authoritative definition, extensions only overlay it.
    let paths = db.all_config_paths();
    let base_first = paths
        .iter()
        .filter(|(label, _)| label.is_none())
        .chain(paths.iter().filter(|(label, _)| label.is_some()));
    for (_label, root) in base_first {
        let root = root.to_string_lossy();
        let Some(listing) = db.metadata_listing(root.as_ref()) else {
            continue;
        };
        if let Some(ids) = metadata::config_index(db, listing).lookup(mdo_type, name) {
            return Some(ids.main);
        }
    }
    None
}

fn common_module_target(
    db: &RootDatabaseImpl,
    from_file_id: FileId,
    name: &str,
) -> Option<NavigationTarget> {
    use ide_db::base_db::SourceDatabase;
    let source_root_id = db.file_source_root_input(from_file_id).source_root_id(db);
    let file_id = db.module_index(source_root_id).resolve_common_module(&Name::new(name))?;
    Some(NavigationTarget {
        file_id,
        range: TextRange::empty(TextSize::from(0)),
        name: name.to_string(),
        kind: SymbolKind::Variable,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ref_and_object_kinds_map_to_the_same_mdo_type() {
        assert_eq!(mdo_type_of_kind(MetadataKind::CatalogRef), Some(MdoType::Catalog));
        assert_eq!(mdo_type_of_kind(MetadataKind::CatalogObject), Some(MdoType::Catalog));
        assert_eq!(mdo_type_of_kind(MetadataKind::DocumentRef), Some(MdoType::Document));
        assert_eq!(mdo_type_of_kind(MetadataKind::DocumentObject), Some(MdoType::Document));
    }

    #[test]
    fn register_forms_map_to_their_register_type() {
        assert_eq!(
            mdo_type_of_kind(MetadataKind::InformationRegisterRecordSet),
            Some(MdoType::InformationRegister)
        );
        assert_eq!(
            mdo_type_of_kind(MetadataKind::AccumulationRegisterRef),
            Some(MdoType::AccumulationRegister)
        );
    }

    #[test]
    fn tabular_and_register_parts_map_to_their_owner_type() {
        assert_eq!(
            mdo_type_of_kind(MetadataKind::TabularSection { parent: MdoType::Catalog }),
            Some(MdoType::Catalog)
        );
        assert_eq!(
            mdo_type_of_kind(MetadataKind::RegisterDimension {
                parent: MdoType::InformationRegister
            }),
            Some(MdoType::InformationRegister)
        );
    }

    #[test]
    fn owner_object_name_strips_the_section_qualifier() {
        assert_eq!(owner_object_name("Номенклатура.Товары"), "Номенклатура");
        assert_eq!(owner_object_name("Номенклатура"), "Номенклатура");
    }

    #[test]
    fn object_only_and_enum_families_map() {
        // У обработки нет ref-формы, но объектная навигируема.
        assert_eq!(
            mdo_type_of_kind(MetadataKind::DataProcessorObject),
            Some(MdoType::DataProcessor)
        );
        // У перечисления есть только ref-форма.
        assert_eq!(mdo_type_of_kind(MetadataKind::EnumRef), Some(MdoType::Enum));
    }
}
