use crate::error::{MetadataError, Result};
use crate::metadata_object::{MdoType, MetadataObject};

use super::helpers::{
    child_text, find_child, find_mdo_element, parse_extension_ownership, parse_uuid, parse_xml,
};
use super::type_parser::parse_type_xml;

pub fn parse_constant_xml(xml: &str) -> Result<MetadataObject> {
    let _span = tracing::debug_span!("parse_constant_xml").entered();

    let doc = parse_xml(xml)?;
    let mdo = find_mdo_element(&doc)
        .ok_or_else(|| MetadataError::InvalidFormat("No Constant element found".to_string()))?;

    let props = find_child(mdo, "Properties")
        .ok_or_else(|| MetadataError::InvalidFormat("Constant missing Properties".to_string()))?;

    let name = child_text(props, "Name").unwrap_or("").to_string();
    let mut mdo_obj = MetadataObject::new(MdoType::Constant, name);
    mdo_obj.read_properties = super::read_properties::parse_read_properties(mdo);

    if let Some(uuid_str) = mdo.attribute("uuid") {
        match parse_uuid(uuid_str, "Constant root") {
            Ok(uuid) => mdo_obj.set_uuid(uuid),
            Err(err) => tracing::warn!(
                name = %mdo_obj.name,
                uuid_raw = %uuid_str,
                %err,
                "ignored malformed Constant root UUID"
            ),
        }
    }

    let (belonging, extends) = parse_extension_ownership(props);
    mdo_obj.set_object_belonging(belonging);
    if let Some(uuid) = extends {
        mdo_obj.set_extends_uuid(uuid);
    }

    if let Some(type_node) = find_child(props, "Type") {
        let attr_type = parse_type_xml(type_node)?;
        mdo_obj.set_constant_type(attr_type);
    }

    Ok(mdo_obj)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata_object::AttributeType;

    fn wrap_constant(props: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:v8="http://v8.1c.ru/8.1/data/core" xmlns:xs="http://www.w3.org/2001/XMLSchema" xmlns:cfg="http://v8.1c.ru/8.1/data/enterprise/current-config">
  <Constant>
    <Properties>
      <Name>X</Name>
      {props}
    </Properties>
  </Constant>
</MetaDataObject>"#
        )
    }

    #[test]
    fn parses_string_constant_with_qualifier() {
        let xml = wrap_constant(
            r#"<Type>
                 <v8:Type>xs:string</v8:Type>
                 <StringQualifiers><Length>50</Length></StringQualifiers>
               </Type>"#,
        );
        let mdo = parse_constant_xml(&xml).expect("constant parses");
        assert_eq!(mdo.name, "X");
        match mdo.constant_type {
            Some(AttributeType::String { length }) => assert_eq!(length, Some(50)),
            other => panic!("expected String, got {other:?}"),
        }
    }

    #[test]
    fn parses_number_constant_with_qualifiers() {
        let xml = wrap_constant(
            r#"<Type>
                 <v8:Type>xs:decimal</v8:Type>
                 <NumberQualifiers><Digits>10</Digits><FractionDigits>2</FractionDigits></NumberQualifiers>
               </Type>"#,
        );
        let mdo = parse_constant_xml(&xml).expect("constant parses");
        assert!(matches!(mdo.constant_type, Some(AttributeType::Number { .. })));
    }

    #[test]
    fn parses_ref_constant() {
        let xml = wrap_constant(r#"<Type><v8:Type>cfg:CatalogRef.Справочник1</v8:Type></Type>"#);
        let mdo = parse_constant_xml(&xml).expect("constant parses");
        match mdo.constant_type {
            Some(AttributeType::Ref { mdo_type, name }) => {
                assert_eq!(mdo_type, MdoType::Catalog);
                assert_eq!(name, "Справочник1");
            }
            other => panic!("expected CatalogRef, got {other:?}"),
        }
    }

    #[test]
    fn parses_composite_constant() {
        let xml = wrap_constant(
            r#"<Type>
                 <v8:Type>xs:string</v8:Type>
                 <v8:Type>cfg:CatalogRef.Справочник1</v8:Type>
               </Type>"#,
        );
        let mdo = parse_constant_xml(&xml).expect("constant parses");
        match mdo.constant_type {
            Some(AttributeType::Composite { types }) => {
                assert_eq!(types.len(), 2);
                assert!(matches!(types[0], AttributeType::String { .. }));
                assert!(matches!(types[1], AttributeType::Ref { .. }));
            }
            other => panic!("expected Composite, got {other:?}"),
        }
    }

    #[test]
    fn missing_type_block_is_none() {
        let xml = wrap_constant("");
        let mdo = parse_constant_xml(&xml).expect("constant parses without <Type>");
        assert!(mdo.constant_type.is_none());
    }

    #[test]
    fn parses_root_uuid() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.20">
    <Constant uuid="9893e2d6-f3f8-4d73-bb06-19bf26d216ab">
        <Properties><Name>X</Name></Properties>
    </Constant>
</MetaDataObject>"#;
        let mdo = parse_constant_xml(xml).expect("constant parses");
        assert_eq!(
            mdo.uuid().map(|u| u.to_string()),
            Some("9893e2d6-f3f8-4d73-bb06-19bf26d216ab".to_string())
        );
    }

    #[test]
    fn parses_extension_ownership() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.20">
    <Constant uuid="22222222-2222-2222-2222-222222222222">
        <Properties>
            <ObjectBelonging>Adopted</ObjectBelonging>
            <Name>X</Name>
            <ExtendedConfigurationObject>11111111-1111-1111-1111-111111111111</ExtendedConfigurationObject>
        </Properties>
    </Constant>
</MetaDataObject>"#;
        let mdo = parse_constant_xml(xml).expect("constant parses");
        assert_eq!(mdo.object_belonging(), crate::ObjectBelonging::Adopted);
        assert_eq!(
            mdo.extends_uuid().map(|u| u.to_string()),
            Some("11111111-1111-1111-1111-111111111111".to_string())
        );
    }
}
