use crate::error::{MetadataError, Result};
use crate::metadata_object::{EnumValue, MdoType, MetadataObject};

use super::helpers::{
    child_text, find_child, find_mdo_element, parse_extension_ownership, parse_uuid, parse_xml,
};

pub fn parse_enum_xml(xml: &str) -> Result<MetadataObject> {
    let _span = tracing::debug_span!("parse_enum_xml").entered();

    let doc = parse_xml(xml)?;
    let mdo = find_mdo_element(&doc)
        .ok_or_else(|| MetadataError::InvalidFormat("No Enum element found".to_string()))?;

    let props = find_child(mdo, "Properties")
        .ok_or_else(|| MetadataError::InvalidFormat("Enum missing Properties".to_string()))?;

    let name = child_text(props, "Name").unwrap_or("").to_string();

    let mut enum_values = Vec::new();

    if let Some(child_objects) = find_child(mdo, "ChildObjects") {
        for ev_node in child_objects
            .children()
            .filter(|n| n.is_element() && n.tag_name().name() == "EnumValue")
        {
            let uuid = ev_node.attribute("uuid").unwrap_or("").to_string();

            let ev_props = find_child(ev_node, "Properties").ok_or_else(|| {
                MetadataError::InvalidFormat("EnumValue missing Properties".to_string())
            })?;
            let ev_name = child_text(ev_props, "Name").unwrap_or("").to_string();

            enum_values.push(EnumValue { name: ev_name, name_en: None, uuid });
        }
    }

    let mut mdo_obj = MetadataObject::new(MdoType::Enum, name);
    mdo_obj.read_properties = super::read_properties::parse_read_properties(mdo);
    if let Some(uuid_str) = mdo.attribute("uuid") {
        match parse_uuid(uuid_str, "Enum root") {
            Ok(uuid) => mdo_obj.set_uuid(uuid),
            Err(err) => tracing::warn!(
                name = %mdo_obj.name,
                uuid_raw = %uuid_str,
                %err,
                "ignored malformed Enum root UUID"
            ),
        }
    }
    mdo_obj.enum_values = enum_values;
    let (belonging, extends) = parse_extension_ownership(props);
    mdo_obj.set_object_belonging(belonging);
    if let Some(uuid) = extends {
        mdo_obj.set_extends_uuid(uuid);
    }

    tracing::debug!(
        enum_name = %mdo_obj.name,
        enum_values_count = mdo_obj.enum_values.len(),
        "parsed enum"
    );

    Ok(mdo_obj)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_enum_xml_with_values() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.20">
    <Enum uuid="379167c7-29f4-479f-8803-914fd95e350f">
        <Properties>
            <Name>Статусы</Name>
        </Properties>
        <ChildObjects>
            <EnumValue uuid="f32053d4-6092-498a-9107-b042289e65ae">
                <Properties>
                    <Name>Активный</Name>
                </Properties>
            </EnumValue>
            <EnumValue uuid="7af3bc2d-f14b-4e93-a8db-0d3fece96257">
                <Properties>
                    <Name>Неактивный</Name>
                </Properties>
            </EnumValue>
            <EnumValue uuid="ff25e312-3fc6-49f2-8382-e5ed6f9746bb">
                <Properties>
                    <Name>Завершенный</Name>
                </Properties>
            </EnumValue>
        </ChildObjects>
    </Enum>
</MetaDataObject>"#;

        let enum_obj = parse_enum_xml(xml).unwrap();

        assert_eq!(enum_obj.name, "Статусы");
        assert_eq!(enum_obj.mdo_type, MdoType::Enum);
        assert_eq!(
            enum_obj.uuid().map(|u| u.to_string()),
            Some("379167c7-29f4-479f-8803-914fd95e350f".to_string())
        );
        assert_eq!(enum_obj.enum_values.len(), 3);

        assert_eq!(enum_obj.enum_values[0].name, "Активный");
        assert_eq!(enum_obj.enum_values[0].uuid, "f32053d4-6092-498a-9107-b042289e65ae");

        assert_eq!(enum_obj.enum_values[1].name, "Неактивный");
        assert_eq!(enum_obj.enum_values[1].uuid, "7af3bc2d-f14b-4e93-a8db-0d3fece96257");

        assert_eq!(enum_obj.enum_values[2].name, "Завершенный");
        assert_eq!(enum_obj.enum_values[2].uuid, "ff25e312-3fc6-49f2-8382-e5ed6f9746bb");

        assert!(enum_obj.find_enum_value("Активный").is_some());
        assert!(enum_obj.find_enum_value("активный").is_some());
        assert!(enum_obj.find_enum_value("АКТИВНЫЙ").is_some());
        assert!(enum_obj.find_enum_value("Несуществующий").is_none());
    }

    #[test]
    fn test_parse_enum_xml_no_values() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.20">
    <Enum uuid="12345678-1234-1234-1234-123456789012">
        <Properties>
            <Name>ПустоеПеречисление</Name>
        </Properties>
    </Enum>
</MetaDataObject>"#;

        let enum_obj = parse_enum_xml(xml).unwrap();

        assert_eq!(enum_obj.name, "ПустоеПеречисление");
        assert_eq!(enum_obj.mdo_type, MdoType::Enum);
        assert_eq!(enum_obj.enum_values.len(), 0);
    }

    #[test]
    fn test_parse_enum_xml_reads_extension_ownership() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.20">
    <Enum uuid="22222222-2222-2222-2222-222222222222">
        <Properties>
            <ObjectBelonging>Adopted</ObjectBelonging>
            <Name>Статусы</Name>
            <ExtendedConfigurationObject>11111111-1111-1111-1111-111111111111</ExtendedConfigurationObject>
        </Properties>
    </Enum>
</MetaDataObject>"#;

        let enum_obj = parse_enum_xml(xml).unwrap();

        assert_eq!(enum_obj.object_belonging(), crate::ObjectBelonging::Adopted);
        assert_eq!(
            enum_obj.extends_uuid().map(|u| u.to_string()),
            Some("11111111-1111-1111-1111-111111111111".to_string())
        );
    }
}
