use std::str::FromStr;

use crate::error::{MetadataError, Result};
use crate::metadata_object::MdoType;
use crate::subsystem::Subsystem;

use super::helpers::{child_text, find_child, find_mdo_element, parse_xml};

/// Parse a subsystem `.xml` file: its name, its `<Content>` member objects (each a
/// `Type.Name` MDObjectRef), and its directly-nested child subsystems.
pub fn parse_subsystem_xml(xml: &str) -> Result<Subsystem> {
    let _span = tracing::debug_span!("parse_subsystem_xml").entered();

    let doc = parse_xml(xml)?;
    let mdo = find_mdo_element(&doc)
        .ok_or_else(|| MetadataError::InvalidFormat("No metadata element found".to_string()))?;
    // Only a `<Subsystem>` element is a subsystem — a different metadata object that happens
    // to sit under a `Subsystems/` directory (or a malformed file) must be rejected, not
    // silently treated as a subsystem.
    if mdo.tag_name().name() != "Subsystem" {
        return Err(MetadataError::InvalidFormat(format!(
            "expected a Subsystem element, found {}",
            mdo.tag_name().name()
        )));
    }
    let props = find_child(mdo, "Properties")
        .ok_or_else(|| MetadataError::InvalidFormat("Subsystem missing Properties".to_string()))?;

    let name = child_text(props, "Name").unwrap_or("").to_string();

    let mut content = Vec::new();
    if let Some(content_node) = find_child(props, "Content") {
        for item in
            content_node.children().filter(|n| n.is_element() && n.tag_name().name() == "Item")
        {
            if let Some((mdo_type, obj_name)) =
                item.text().and_then(|raw| parse_mdo_ref(raw.trim()))
            {
                content.push((mdo_type, obj_name));
            }
        }
    }

    let mut child_subsystems = Vec::new();
    if let Some(children_node) = find_child(mdo, "ChildObjects") {
        for child in children_node
            .children()
            .filter(|n| n.is_element() && n.tag_name().name() == "Subsystem")
        {
            if let Some(text) = child.text() {
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    child_subsystems.push(trimmed.to_string());
                }
            }
        }
    }

    let mut subsystem =
        Subsystem::new(name).with_content(content).with_child_subsystems(child_subsystems);
    subsystem.picture =
        find_child(props, "Picture").map(|node| node.text().unwrap_or_default().to_string());
    subsystem.content_refs = find_child(props, "Content").and_then(|node| {
        node.children()
            .filter(|child| child.is_element())
            .map(|child| {
                (child.tag_name().name() == "Item")
                    .then(|| child.text().map(str::to_owned))
                    .flatten()
            })
            .collect()
    });
    subsystem.uuid = mdo.attribute("uuid").and_then(|value| uuid::Uuid::parse_str(value).ok());
    (subsystem.object_belonging, subsystem.extended_configuration_object) =
        super::helpers::parse_extension_ownership(props);
    subsystem.content_unread = subsystem.content_refs.is_none();
    Ok(subsystem)
}

/// Parse a `Type.Name` metadata reference (e.g. `Catalog.Пользователи`) from a subsystem's
/// `<Content>` into a recognised `(MdoType, name)`. The prefix is the English MDObjectRef
/// type; unknown types (or malformed refs) are skipped so an unparseable member never
/// invents an edge.
fn parse_mdo_ref(raw: &str) -> Option<(MdoType, String)> {
    let Some((prefix, name)) = raw.split_once('.') else {
        if !raw.is_empty() {
            tracing::debug!(raw, "subsystem content ref has no type prefix; skipped");
        }
        return None;
    };
    if name.is_empty() {
        return None;
    }
    match MdoType::from_str(prefix) {
        Ok(mdo_type) => Some((mdo_type, name.to_string())),
        Err(_) => {
            tracing::debug!(raw, prefix, "subsystem content ref has unknown type; skipped");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_name_content_and_children() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns:xr="http://v8.1c.ru/8.3/xcf/readable" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
  <Subsystem uuid="00000000-0000-0000-0000-000000000000">
    <Properties>
      <Name>Администрирование</Name>
      <Content>
        <xr:Item xsi:type="xr:MDObjectRef">Catalog.Пользователи</xr:Item>
        <xr:Item xsi:type="xr:MDObjectRef">Document.КорректировкаРегистров</xr:Item>
        <xr:Item xsi:type="xr:MDObjectRef">Bogus.Unknown</xr:Item>
      </Content>
    </Properties>
    <ChildObjects>
      <Subsystem>ФизическиеЛица</Subsystem>
    </ChildObjects>
  </Subsystem>
</MetaDataObject>"#;
        let sub = parse_subsystem_xml(xml).unwrap();
        assert_eq!(sub.name(), "Администрирование");
        assert_eq!(
            sub.content(),
            &[
                (MdoType::Catalog, "Пользователи".to_string()),
                (MdoType::Document, "КорректировкаРегистров".to_string()),
            ],
            "recognised refs parse; an unknown type is skipped, not guessed"
        );
        assert_eq!(sub.child_subsystems(), &["ФизическиеЛица".to_string()]);
    }

    #[test]
    fn rejects_a_non_subsystem_root() {
        // A different metadata object that happens to have <Properties> must not be parsed
        // as a subsystem (e.g. a stray file under a Subsystems/ directory).
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject>
  <Catalog uuid="00000000-0000-0000-0000-000000000000">
    <Properties>
      <Name>Справочник1</Name>
    </Properties>
  </Catalog>
</MetaDataObject>"#;
        assert!(parse_subsystem_xml(xml).is_err(), "a non-Subsystem root must be rejected");
    }
}
