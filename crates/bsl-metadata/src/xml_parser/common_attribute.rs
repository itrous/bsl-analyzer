use rustc_hash::FxHashMap;
use stdx::case::CaseExt;

use crate::common_attribute::{
    classify_content_reference, CommonAttribute, CommonAttributeUse, ContentTarget,
};
use crate::error::{MetadataError, Result};
use crate::metadata_object::AttributeType;

use super::helpers::{child_text, find_child, find_mdo_element, parse_xml};
use super::type_parser::parse_type_xml;

pub fn parse_common_attribute_xml(xml: &str) -> Result<CommonAttribute> {
    let doc = parse_xml(xml)?;
    let mdo =
        find_mdo_element(&doc).filter(|n| n.tag_name().name() == "CommonAttribute").ok_or_else(
            || MetadataError::InvalidFormat("No CommonAttribute element found".to_string()),
        )?;
    let props = find_child(mdo, "Properties").ok_or_else(|| {
        MetadataError::InvalidFormat("CommonAttribute missing Properties".to_string())
    })?;

    let name = child_text(props, "Name").map(str::trim).unwrap_or("").to_string();
    if name.is_empty() {
        return Err(MetadataError::InvalidFormat("CommonAttribute without a Name".to_string()));
    }
    let _span = tracing::debug_span!("parse_common_attribute_xml", name = %name).entered();

    let attr_type = match find_child(props, "Type") {
        Some(type_node) => parse_type_xml(type_node)?,
        None => AttributeType::Unknown,
    };

    // An adopted attribute in an extension carries no AutoUse of its own; its Content then
    // lists exactly the extension objects it applies to, which is the `DontUse` reading.
    let auto_use = child_text(props, "AutoUse").is_some_and(|s| s.trim() == "Use");
    let separator = child_text(props, "DataSeparation").is_some_and(|s| s.trim() == "Separate");

    let mut content = FxHashMap::default();
    let mut unattributed_members = false;
    if let Some(content_node) = find_child(props, "Content") {
        let items =
            content_node.children().filter(|n| n.is_element() && n.tag_name().name() == "Item");
        for item in items {
            let reference = child_text(item, "Metadata").map(str::trim).unwrap_or("");
            let usage = match child_text(item, "Use").map(str::trim) {
                Some("Use") => Some(CommonAttributeUse::Use),
                Some("DontUse") => Some(CommonAttributeUse::DontUse),
                Some("Auto") => Some(CommonAttributeUse::Auto),
                _ => None,
            };
            match (classify_content_reference(reference), usage) {
                (ContentTarget::Object(mdo_type, object), Some(usage)) => {
                    content.insert((mdo_type, object.fold_lower()), usage);
                }
                (ContentTarget::Ignored, _) => {}
                // An unreadable setting on a known object: it may be the only thing that puts
                // the object into the composition.
                (ContentTarget::Object(..), None) => unattributed_members = true,
                // `DontUse` keeps an object out and `Auto` defers to `AutoUse`, so only an
                // explicit or unreadable `Use` could have put the unknown object in.
                (ContentTarget::Unknown, Some(CommonAttributeUse::Use) | None) => {
                    unattributed_members = true
                }
                (
                    ContentTarget::Unknown,
                    Some(CommonAttributeUse::DontUse | CommonAttributeUse::Auto),
                ) => {}
            }
        }
    }

    tracing::debug!(
        name = %name,
        auto_use,
        separator,
        content = content.len(),
        unattributed_members,
        "parsed common attribute"
    );

    Ok(CommonAttribute { name, attr_type, auto_use, separator, content, unattributed_members })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata_object::MdoType;

    const SEPARATOR: &str = include_str!(
        "../../fixtures/common_attributes/CommonAttributes/ОбластьДанныхОсновныеДанные.xml"
    );
    const PLAIN: &str =
        include_str!("../../fixtures/common_attributes/CommonAttributes/Организация.xml");

    #[test]
    fn reads_a_data_separator_with_auto_use() {
        let attr = parse_common_attribute_xml(SEPARATOR).expect("separator parses");
        assert_eq!(attr.name, "ОбластьДанныхОсновныеДанные");
        assert!(attr.separator);
        assert!(attr.auto_use);
        assert_eq!(attr.attr_type, AttributeType::Number { precision: 7, scale: 0 });
        assert_eq!(
            attr.content.get(&(MdoType::Catalog, "справочникбезразделения".to_string())),
            Some(&CommonAttributeUse::DontUse)
        );
        assert!(!attr.unattributed_members);
    }

    #[test]
    fn reads_a_plain_attribute_with_explicit_content() {
        let attr = parse_common_attribute_xml(PLAIN).expect("plain attribute parses");
        assert_eq!(attr.name, "Организация");
        assert!(!attr.separator);
        assert!(!attr.auto_use);
        assert_eq!(attr.includes(MdoType::Document, "Документ1"), Some(true));
        assert_eq!(attr.includes(MdoType::Catalog, "Справочник1"), Some(true));
        assert_eq!(attr.includes(MdoType::Catalog, "СправочникБезРазделения"), Some(false));
    }

    #[test]
    fn unknown_reference_with_use_leaves_membership_open() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:xr="http://v8.1c.ru/8.3/xcf/readable" xmlns:v8="http://v8.1c.ru/8.1/data/core">
    <CommonAttribute uuid="2dfd1ee1-6abb-4b03-b03e-08354ff0f6fc">
        <Properties>
            <Name>Странный</Name>
            <Type><v8:Type>xs:boolean</v8:Type></Type>
            <Content>
                <xr:Item>
                    <xr:Metadata>НовыйВидОбъекта.Х</xr:Metadata>
                    <xr:Use>Use</xr:Use>
                </xr:Item>
                <xr:Item>
                    <xr:Metadata>НовыйВидОбъекта.Y</xr:Metadata>
                    <xr:Use>DontUse</xr:Use>
                </xr:Item>
            </Content>
            <AutoUse>DontUse</AutoUse>
            <DataSeparation>DontUse</DataSeparation>
        </Properties>
    </CommonAttribute>
</MetaDataObject>"#;
        let attr = parse_common_attribute_xml(xml).expect("parses");
        assert!(attr.unattributed_members);
        assert_eq!(attr.includes(MdoType::Catalog, "Любой"), None);
    }

    #[test]
    fn unknown_reference_with_auto_follows_auto_use() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:xr="http://v8.1c.ru/8.3/xcf/readable" xmlns:v8="http://v8.1c.ru/8.1/data/core">
    <CommonAttribute uuid="2dfd1ee1-6abb-4b03-b03e-08354ff0f6fc">
        <Properties>
            <Name>Странный</Name>
            <Type><v8:Type>xs:boolean</v8:Type></Type>
            <Content>
                <xr:Item>
                    <xr:Metadata>НовыйВидОбъекта.Х</xr:Metadata>
                    <xr:Use>Auto</xr:Use>
                </xr:Item>
            </Content>
            <AutoUse>DontUse</AutoUse>
            <DataSeparation>DontUse</DataSeparation>
        </Properties>
    </CommonAttribute>
</MetaDataObject>"#;
        let attr = parse_common_attribute_xml(xml).expect("parses");
        assert!(!attr.unattributed_members);
        assert_eq!(attr.includes(MdoType::Catalog, "Любой"), Some(false));
    }

    #[test]
    fn a_file_without_a_name_is_rejected() {
        let xml = r#"<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses">
    <CommonAttribute uuid="2dfd1ee1-6abb-4b03-b03e-08354ff0f6fc"><Properties/></CommonAttribute>
</MetaDataObject>"#;
        assert!(parse_common_attribute_xml(xml).is_err());
    }
}
