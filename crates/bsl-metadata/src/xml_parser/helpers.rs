use crate::enums::ObjectBelonging;
use crate::error::{MetadataError, Result};
use crate::metadata_object::{MdoType, StandardAttributeKind};
use crate::register::RegisterAttribute;
use uuid::Uuid;

pub(crate) fn parse_uuid(s: &str, context: &str) -> Result<Uuid> {
    s.parse::<Uuid>()
        .map_err(|e| MetadataError::InvalidFormat(format!("Invalid {} UUID: {}", context, e)))
}

/// The `uuid` attribute of the MDO element, tolerant: an absent or malformed
/// uuid degrades to `nil` instead of failing the whole object — a consumer of
/// the uuid (the vendor-support map) then sees an unidentified object, which it
/// already must handle, while the object keeps serving its other consumers.
pub(crate) fn mdo_uuid(mdo: roxmltree::Node<'_, '_>) -> Uuid {
    mdo.attribute("uuid")
        .and_then(|raw| {
            raw.parse()
                .inspect_err(|err| {
                    tracing::debug!(uuid_raw = %raw, %err, "ignored malformed object uuid");
                })
                .ok()
        })
        .unwrap_or(Uuid::nil())
}

pub(crate) fn create_register_standard_attribute(
    kind: &StandardAttributeKind,
    mdo_type: MdoType,
    object_name: &str,
) -> RegisterAttribute {
    let nil_uuid = Uuid::nil();
    let mut attr = RegisterAttribute::new(nil_uuid, kind.russian_name());
    attr.set_name_en(kind.english_name());
    attr.set_attr_type(kind.attribute_type(mdo_type, object_name));
    attr
}

pub(crate) fn find_child<'a>(
    node: roxmltree::Node<'a, 'a>,
    tag: &str,
) -> Option<roxmltree::Node<'a, 'a>> {
    node.children().find(|n| n.is_element() && n.tag_name().name() == tag)
}

pub(crate) fn child_text<'a>(node: roxmltree::Node<'a, 'a>, tag: &str) -> Option<&'a str> {
    find_child(node, tag).and_then(|n| n.text())
}

pub(crate) fn parse_object_belonging(props: roxmltree::Node<'_, '_>) -> ObjectBelonging {
    match find_child(props, "ObjectBelonging") {
        None => ObjectBelonging::Own,
        Some(node) => match node.text().map(str::trim) {
            Some("Native" | "Own") => ObjectBelonging::Own,
            Some("Adopted") => ObjectBelonging::Adopted,
            _ => ObjectBelonging::Unknown,
        },
    }
}

/// Reads whether an object is adopted from the base configuration and which base
/// object it extends. A malformed base reference is dropped rather than failing the
/// object, so the object still loads, only without being merged into its base.
pub(crate) fn parse_extension_ownership(
    props: roxmltree::Node<'_, '_>,
) -> (ObjectBelonging, Option<Uuid>) {
    let belonging = parse_object_belonging(props);
    let extends = child_text(props, "ExtendedConfigurationObject").and_then(|raw| {
        Uuid::parse_str(raw)
            .inspect_err(|err| {
                tracing::warn!(uuid_raw = %raw, %err, "ignored malformed ExtendedConfigurationObject");
            })
            .ok()
    });
    (belonging, extends)
}

pub(crate) fn child_bool(node: roxmltree::Node<'_, '_>, tag: &str) -> bool {
    child_text(node, tag).is_some_and(|s| s.eq_ignore_ascii_case("true"))
}

pub(crate) fn child_u32(node: roxmltree::Node<'_, '_>, tag: &str) -> Option<u32> {
    // Configurator XML is tight, but a dumped or hand-edited element can carry a newline or a
    // space around the digits. Parsing that as absent would drop a count the file did state.
    child_text(node, tag).and_then(|s| s.trim().parse().ok())
}

pub(crate) fn find_mdo_element<'a>(
    doc: &'a roxmltree::Document<'a>,
) -> Option<roxmltree::Node<'a, 'a>> {
    doc.root_element().children().find(|n| n.is_element())
}

pub(crate) fn parse_xml(xml: &str) -> Result<roxmltree::Document<'_>> {
    roxmltree::Document::parse(xml)
        .map_err(|e| MetadataError::InvalidFormat(format!("XML parse error: {}", e)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::MdObject;
    use crate::xml_parser::*;

    fn object(kind: &str, ownership: &str) -> String {
        format!(
            r#"<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses"><{kind} uuid="00000000-0000-0000-0000-000000000001"><Properties><Name>Object</Name>{ownership}</Properties><ChildObjects/></{kind}></MetaDataObject>"#
        )
    }

    #[test]
    fn native_catalog_exports_with_omitted_ownership_remain_own() {
        for xml in [
            include_str!("../../fixtures/extension_common_attributes/base/Catalogs/BaseCatalog.xml"),
            include_str!("../../fixtures/extension_common_attributes/extensions/explicit-use/Catalogs/NativeCatalog.xml"),
        ] {
            let doc = parse_xml(xml).unwrap();
            let props = find_child(find_mdo_element(&doc).unwrap(), "Properties").unwrap();
            assert!(find_child(props, "ObjectBelonging").is_none());
            let mdo = parse_catalog_xml(xml).unwrap();
            assert_eq!(mdo.object_belonging, ObjectBelonging::Own);
            assert!(mdo.extends_uuid().is_none());
        }
    }

    #[test]
    fn every_ownership_reader_distinguishes_native_defaults_from_explicit_unknown() {
        type MdoParser = fn(&str) -> Result<crate::MetadataObject>;
        let objects: &[(&str, MdoParser)] = &[
            ("Catalog", parse_catalog_xml),
            ("Document", parse_document_xml),
            ("BusinessProcess", parse_business_process_xml),
            ("ChartOfCharacteristicTypes", parse_chart_of_characteristic_types_xml),
            ("Task", parse_task_xml),
            ("ExchangePlan", parse_exchange_plan_xml),
            ("ChartOfAccounts", parse_chart_of_accounts_xml),
            ("ChartOfCalculationTypes", parse_chart_of_calculation_types_xml),
            ("DataProcessor", parse_data_processor_xml),
            ("Report", parse_report_xml),
            ("ExternalDataProcessor", parse_external_data_processor_xml),
            ("ExternalReport", parse_external_report_xml),
            ("Enum", parse_enum_xml),
            ("Constant", parse_constant_xml),
        ];
        type RegisterParser = fn(&str) -> Result<crate::Register>;
        let registers: &[(&str, RegisterParser)] = &[
            ("InformationRegister", parse_information_register_xml),
            ("AccumulationRegister", parse_accumulation_register_xml),
            ("AccountingRegister", parse_accounting_register_xml),
            ("CalculationRegister", parse_calculation_register_xml),
        ];
        for (marker, expected) in [
            ("", ObjectBelonging::Own),
            ("<ObjectBelonging>Native</ObjectBelonging>", ObjectBelonging::Own),
            ("<ObjectBelonging>Own</ObjectBelonging>", ObjectBelonging::Own),
            ("<ObjectBelonging>Adopted</ObjectBelonging>", ObjectBelonging::Adopted),
            ("<ObjectBelonging/>", ObjectBelonging::Unknown),
            ("<ObjectBelonging> </ObjectBelonging>", ObjectBelonging::Unknown),
            ("<ObjectBelonging>Future</ObjectBelonging>", ObjectBelonging::Unknown),
        ] {
            for (kind, parse) in objects {
                assert_eq!(
                    parse(&object(kind, marker)).unwrap().object_belonging,
                    expected,
                    "{kind}: {marker}"
                );
            }
            for (kind, parse) in registers {
                assert_eq!(
                    parse(&object(kind, marker)).unwrap().object_belonging(),
                    expected,
                    "{kind}: {marker}"
                );
            }
            assert_eq!(
                parse_common_module_xml(&object("CommonModule", marker))
                    .unwrap()
                    .object_belonging(),
                expected,
                "CommonModule: {marker}"
            );
            assert_eq!(
                parse_subsystem_xml(&object("Subsystem", marker)).unwrap().object_belonging,
                expected,
                "Subsystem: {marker}"
            );
            assert_eq!(
                parse_common_attribute_xml(&object("CommonAttribute", marker))
                    .unwrap()
                    .object_belonging,
                expected,
                "CommonAttribute: {marker}"
            );
        }
    }

    #[test]
    fn adopted_objects_still_require_a_valid_matching_base_identity() {
        let base = parse_catalog_xml(&object("Catalog", "")).unwrap();
        for target in ["", "<ExtendedConfigurationObject>invalid</ExtendedConfigurationObject>"] {
            let borrowed = parse_catalog_xml(&object(
                "Catalog",
                &format!("<ObjectBelonging>Adopted</ObjectBelonging>{target}"),
            ))
            .unwrap();
            assert_eq!(borrowed.object_belonging, ObjectBelonging::Adopted);
            assert!(borrowed.extends_uuid().is_none());
            assert!(!borrowed.adopts(&base));
        }
        let borrowed = parse_catalog_xml(&object("Catalog", "<ObjectBelonging>Adopted</ObjectBelonging><ExtendedConfigurationObject>00000000-0000-0000-0000-000000000001</ExtendedConfigurationObject>")).unwrap();
        assert!(borrowed.adopts(&base));
        let mut other = base.clone();
        other.set_uuid(Uuid::from_u128(2));
        assert!(!borrowed.adopts(&other));
        other = base.clone();
        other.name = "Other".into();
        assert!(!borrowed.adopts(&other));
        other = base.clone();
        other.mdo_type = MdoType::Document;
        assert!(!borrowed.adopts(&other));
    }
}
