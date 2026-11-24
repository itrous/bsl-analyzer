use super::{MetadataXmlSource, XmlMetadataReferences, XmlNameToken, XmlReferenceProblem};

pub(super) const NAMESPACE: &str = "http://v8.1c.ru/8.3/xcf/dumpinfo";

/// The platform's ID and version remain opaque; consumers attest IDs against source declarations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XmlDumpInfoEntry {
    pub names: Vec<XmlNameToken>,
    pub id: String,
}

impl MetadataXmlSource {
    pub(super) fn dump_info(
        &self,
        root: roxmltree::Node<'_, '_>,
        result: &mut XmlMetadataReferences,
    ) {
        if root.attribute("format") != Some("Hierarchical")
            || root.attribute("version") != Some("2.20")
            || root.attributes().any(|attribute| {
                attribute.namespace().is_some() || !matches!(attribute.name(), "format" | "version")
            })
        {
            result.problems.push(XmlReferenceProblem::UnknownReferenceContract);
            return;
        }
        let children = root.children().filter(|node| node.is_element()).collect::<Vec<_>>();
        let [versions] = children.as_slice() else {
            result.problems.push(XmlReferenceProblem::UnknownReferenceContract);
            return;
        };
        if versions.tag_name().namespace() != Some(NAMESPACE)
            || versions.tag_name().name() != "ConfigVersions"
            || versions.attributes().len() != 0
        {
            result.problems.push(XmlReferenceProblem::UnknownReferenceContract);
            return;
        }
        let mut names_seen = std::collections::BTreeSet::new();
        for node in versions.descendants().filter(|node| node.is_element()).skip(1) {
            if node.tag_name().namespace() != Some(NAMESPACE)
                || node.tag_name().name() != "Metadata"
                || node.attributes().any(|attribute| {
                    attribute.namespace().is_some()
                        || !matches!(attribute.name(), "name" | "id" | "configVersion")
                })
            {
                result.problems.push(XmlReferenceProblem::UnknownReferenceContract);
                continue;
            }
            let name = node.attributes().find(|attribute| attribute.name() == "name");
            let names = name
                .and_then(|attribute| {
                    self.value_at(attribute.range_value(), attribute.value(), true)
                })
                .and_then(|value| value.tokens());
            let id = node.attribute("id").filter(|id| !id.is_empty());
            let Some((names, id)) = names.zip(id) else {
                result.problems.push(XmlReferenceProblem::UnknownReferenceContract);
                continue;
            };
            if names.len() < 2
                || names.iter().any(|part| !super::properties::identifier(&part.text))
            {
                result.problems.push(XmlReferenceProblem::UnknownReferenceContract);
                continue;
            }
            if let Some(parent) =
                node.parent_element().filter(|parent| parent.tag_name().name() == "Metadata")
            {
                let Some(parent_name) = parent.attribute("name") else {
                    result.problems.push(XmlReferenceProblem::UnknownReferenceContract);
                    continue;
                };
                let parent_parts = parent_name.split('.').collect::<Vec<_>>();
                if names.len() <= parent_parts.len()
                    || !parent_parts
                        .iter()
                        .zip(&names)
                        .all(|(parent, child)| stdx::case::eq_ignore_case(parent, &child.text))
                {
                    result.problems.push(XmlReferenceProblem::UnknownReferenceContract);
                    continue;
                }
            }
            use stdx::case::CaseExt;
            if !names_seen
                .insert(names.iter().map(|name| name.text.fold_lower()).collect::<Vec<_>>())
            {
                result.problems.push(XmlReferenceProblem::UnknownReferenceContract);
                continue;
            }
            result.dump_info.push(XmlDumpInfoEntry { names, id: id.into() });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = include_str!("../../fixtures/rename/config-dump-info.xml");

    #[test]
    fn names_have_exact_raw_spans_and_preserve_the_opaque_version_and_id() {
        let source = MetadataXmlSource::decode(MANIFEST.as_bytes()).unwrap();
        let facts = source.inspect(None);
        assert!(facts.problems.is_empty(), "{facts:#?}");
        assert_eq!(facts.dump_info.len(), 5);
        let member = &facts.dump_info[1];
        assert_eq!(member.id, "00000000-0000-0000-0000-000000000002");
        assert_eq!(
            member.names.iter().map(|part| part.text.as_str()).collect::<Vec<_>>(),
            ["Catalog", "Partners", "Attribute", "Caption"]
        );
        let token = &member.names[3];
        assert_eq!(&MANIFEST.as_bytes()[token.range.clone()], b"Cap&#116;ion");
        let after = [
            &MANIFEST.as_bytes()[..token.range.start],
            b"Heading",
            &MANIFEST.as_bytes()[token.range.end..],
        ]
        .concat();
        let after = String::from_utf8(after).unwrap();
        assert_eq!(
            after.matches("configVersion=").count(),
            MANIFEST.matches("configVersion=").count()
        );
        assert!(after.contains(
            "id='00000000-0000-0000-0000-000000000001' configVersion='opaque-version-1'"
        ));
        assert!(after.contains("id='00000000-0000-0000-0000-000000000050.0'"));
    }

    #[test]
    fn unknown_format_version_and_malformed_hierarchy_are_incomplete() {
        for xml in [
            MANIFEST.replace("version='2.20'", "version='99.0'"),
            MANIFEST.replace("format='Hierarchical'", "format='Plain'"),
            MANIFEST.replace("Catalog.Partners.Attribute.Cap", "Catalog.Customers.Attribute.Cap"),
            MANIFEST.replace("<ConfigVersions>", "<ConfigVersions unknown='1'>"),
        ] {
            assert!(!MetadataXmlSource::decode(xml.as_bytes())
                .unwrap()
                .inspect(None)
                .problems
                .is_empty());
        }
    }
}
