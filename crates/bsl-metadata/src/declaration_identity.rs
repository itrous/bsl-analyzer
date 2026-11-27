//! Declaration provenance for consumers which must distinguish identity from spelling.

use crate::xml_parser::helpers::{child_text, find_child, parse_extension_ownership};
use crate::{MetadataError, ObjectBelonging, Result, Uuid};

/// A path segment names a declaration kind as well as its name.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DeclarationSegment {
    pub kind: String,
    pub name: String,
}

/// A declaration's exported identity; it contains no proposed XML edit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataDeclaration {
    pub path: Vec<DeclarationSegment>,
    pub uuid: Uuid,
    pub object_belonging: ObjectBelonging,
    pub extends: Option<Uuid>,
}

/// Read identities from supported Designer declarations without inferring adoption by name.
pub fn metadata_declarations(xml: &str) -> Result<Vec<MetadataDeclaration>> {
    let document = roxmltree::Document::parse(xml.trim_start_matches('\u{feff}'))
        .map_err(|e| MetadataError::InvalidFormat(e.to_string()))?;
    let root = document.root_element();
    if root.tag_name().name() != "MetaDataObject"
        || root.tag_name().namespace() != Some("http://v8.1c.ru/8.3/MDClasses")
    {
        return Err(MetadataError::InvalidFormat("Expected Designer metadata declarations".into()));
    }
    let mut result = Vec::new();
    for declaration in root.children().filter(|n| n.is_element()) {
        collect(declaration, &[], &mut result)?;
    }
    let mut identities = std::collections::BTreeSet::new();
    if result.iter().any(|declaration| !identities.insert(declaration.uuid)) {
        return Err(MetadataError::InvalidFormat(
            "Duplicate declaration UUID in one source object".into(),
        ));
    }
    Ok(result)
}

fn collect(
    node: roxmltree::Node<'_, '_>,
    parent: &[DeclarationSegment],
    result: &mut Vec<MetadataDeclaration>,
) -> Result<()> {
    let props = find_child(node, "Properties")
        .ok_or_else(|| MetadataError::InvalidFormat("Declaration Properties unavailable".into()))?;
    let name = child_text(props, "Name")
        .filter(|name| !name.is_empty())
        .ok_or_else(|| MetadataError::InvalidFormat("Declaration Name unavailable".into()))?;
    let uuid = node
        .attribute("uuid")
        .and_then(|value| value.parse::<Uuid>().ok())
        .filter(|uuid| !uuid.is_nil())
        .ok_or_else(|| {
            MetadataError::InvalidFormat(format!("Declaration UUID unavailable: {name}"))
        })?;
    let (object_belonging, extends) = parse_extension_ownership(props);
    if object_belonging == ObjectBelonging::Adopted && extends.is_none_or(|uuid| uuid.is_nil()) {
        return Err(MetadataError::InvalidFormat(format!(
            "Adopted declaration has no base identity: {name}"
        )));
    }
    let mut path = parent.to_vec();
    path.push(DeclarationSegment { kind: node.tag_name().name().into(), name: name.into() });
    result.push(MetadataDeclaration { path: path.clone(), uuid, object_belonging, extends });
    if let Some(children) = find_child(node, "ChildObjects") {
        for child in children.children().filter(|n| n.is_element()) {
            if child.attribute("uuid").is_none()
                && !matches!(
                    child.tag_name().name(),
                    "Attribute" | "Dimension" | "Resource" | "TabularSection" | "EnumValue"
                )
            {
                continue;
            }
            collect(child, &path, result)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declaration_provenance_preserves_nested_identity_and_adoption() {
        let declarations =
            metadata_declarations(include_str!("../fixtures/rename/adopted.xml")).unwrap();
        assert_eq!(declarations.len(), 3);
        assert_eq!(declarations[0].object_belonging, ObjectBelonging::Adopted);
        assert_eq!(
            declarations[1].path.iter().map(|s| s.kind.as_str()).collect::<Vec<_>>(),
            ["Catalog", "TabularSection"]
        );
        assert_eq!(
            declarations[2].path.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["Partners", "Rows", "Caption"]
        );
        assert_eq!(declarations[2].extends, Some(Uuid::from_u128(3)));
    }

    #[test]
    fn declaration_provenance_does_not_guess_missing_adopted_uuid() {
        assert!(metadata_declarations("<MetaDataObject xmlns='http://v8.1c.ru/8.3/MDClasses'><Catalog uuid='00000000-0000-0000-0000-000000000001'><Properties><Name>Partners</Name><ObjectBelonging>Adopted</ObjectBelonging></Properties></Catalog></MetaDataObject>").is_err());
        assert!(metadata_declarations("<MetaDataObject><Catalog/></MetaDataObject>").is_err());
    }
}
