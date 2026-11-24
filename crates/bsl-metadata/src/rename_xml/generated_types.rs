use super::*;

fn prefix(kind: MdoType, category: &str) -> Option<&'static str> {
    match (kind, category) {
        (MdoType::Catalog, "Object") => Some("CatalogObject"),
        (MdoType::Catalog, "Ref") => Some("CatalogRef"),
        (MdoType::Catalog, "Selection") => Some("CatalogSelection"),
        (MdoType::Catalog, "List") => Some("CatalogList"),
        (MdoType::Catalog, "Manager") => Some("CatalogManager"),
        (MdoType::Document, "Object") => Some("DocumentObject"),
        (MdoType::Document, "Ref") => Some("DocumentRef"),
        (MdoType::Document, "Selection") => Some("DocumentSelection"),
        (MdoType::Document, "List") => Some("DocumentList"),
        (MdoType::Document, "Manager") => Some("DocumentManager"),
        (MdoType::ExchangePlan, "Object") => Some("ExchangePlanObject"),
        (MdoType::ExchangePlan, "Ref") => Some("ExchangePlanRef"),
        (MdoType::ExchangePlan, "Selection") => Some("ExchangePlanSelection"),
        (MdoType::ExchangePlan, "List") => Some("ExchangePlanList"),
        (MdoType::ExchangePlan, "Manager") => Some("ExchangePlanManager"),
        (MdoType::Report, "Object") => Some("ReportObject"),
        (MdoType::Report, "Manager") => Some("ReportManager"),
        (MdoType::InformationRegister, "Record") => Some("InformationRegisterRecord"),
        (MdoType::InformationRegister, "RecordSet") => Some("InformationRegisterRecordSet"),
        (MdoType::InformationRegister, "RecordKey") => Some("InformationRegisterRecordKey"),
        (MdoType::InformationRegister, "RecordManager") => Some("InformationRegisterRecordManager"),
        (MdoType::InformationRegister, "Selection") => Some("InformationRegisterSelection"),
        (MdoType::InformationRegister, "List") => Some("InformationRegisterList"),
        (MdoType::InformationRegister, "Manager") => Some("InformationRegisterManager"),
        _ => None,
    }
}

impl MetadataXmlSource {
    pub(super) fn generated_types(
        &self,
        root: roxmltree::Node<'_, '_>,
        result: &mut XmlMetadataReferences,
    ) {
        let mut seen = std::collections::BTreeSet::new();
        for node in root
            .descendants()
            .filter(|node| node.is_element() && node.tag_name().name() == "GeneratedType")
        {
            match self.generated_type(node, &result.declarations) {
                Some(reference)
                    if seen.insert((
                        node.parent_element()
                            .and_then(|internal| internal.parent_element())
                            .and_then(|owner| owner.attribute("uuid"))
                            .unwrap_or("")
                            .to_string(),
                        node.attribute("category").unwrap_or("").to_string(),
                    )) =>
                {
                    result.references.push(reference)
                }
                _ => result.problems.push(XmlReferenceProblem::GeneratedTypeContract),
            }
        }
    }

    fn generated_type(
        &self,
        node: roxmltree::Node<'_, '_>,
        declarations: &[XmlDeclarationName],
    ) -> Option<XmlMetadataReference> {
        if !node.has_tag_name((READABLE, "GeneratedType"))
            || node.attributes().len() != 2
            || node.attributes().any(|attribute| {
                attribute.namespace().is_some() || !matches!(attribute.name(), "name" | "category")
            })
        {
            return None;
        }
        let internal = node.parent_element().filter(|parent| {
            parent.has_tag_name((MD, "InternalInfo")) && parent.attributes().len() == 0
        })?;
        let owner =
            internal.parent_element().filter(|owner| owner.tag_name().namespace() == Some(MD))?;
        let uuid =
            owner.attribute("uuid")?.parse::<crate::Uuid>().ok().filter(|uuid| !uuid.is_nil())?;
        let declaration =
            declarations.iter().find(|declaration| declaration.declaration.uuid == uuid)?;
        let path = declaration.declaration.path.as_slice();
        let site = path.first()?;
        if path.last()?.kind != owner.tag_name().name() {
            return None;
        }
        let kind = site.kind.parse().ok()?;
        let category = node.attribute("category")?;
        let expected = match path {
            [_] => prefix(kind, category)?,
            [_, member] if member.kind == "TabularSection" => match (kind, category) {
                (MdoType::Catalog, "TabularSection") => "CatalogTabularSection",
                (MdoType::Catalog, "TabularSectionRow") => "CatalogTabularSectionRow",
                (MdoType::Document, "TabularSection") => "DocumentTabularSection",
                (MdoType::Document, "TabularSectionRow") => "DocumentTabularSectionRow",
                _ => return None,
            },
            _ => return None,
        };
        let attribute = node.attributes().find(|attribute| attribute.name() == "name")?;
        let names = self.value_at(attribute.range_value(), attribute.value(), true)?.tokens()?;
        if names.len() != path.len() + 1
            || names[0].text != expected
            || !names
                .iter()
                .skip(1)
                .zip(path)
                .all(|(name, site)| stdx::case::eq_ignore_case(&name.text, &site.name))
        {
            return None;
        }
        let children = node.children().filter(|child| child.is_element()).collect::<Vec<_>>();
        if children.len() != 2
            || !children[0].has_tag_name((READABLE, "TypeId"))
            || !children[1].has_tag_name((READABLE, "ValueId"))
            || children.iter().any(|child| {
                child.attributes().len() != 0
                    || child.children().any(|child| child.is_element())
                    || child
                        .text()
                        .and_then(|id| id.parse::<crate::Uuid>().ok())
                        .is_none_or(|id| id.is_nil())
            })
            || node
                .children()
                .filter(|child| child.is_text())
                .any(|child| child.text().is_some_and(|text| !text.trim().is_empty()))
        {
            return None;
        }
        Some(XmlMetadataReference {
            anchor: XmlReferenceAnchor::Metadata { kind, name: names[1].clone() },
            members: names[2..].to_vec(),
            declared_member_kinds: path.iter().skip(1).map(|site| site.kind.clone()).collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CATALOG: &str =
        include_str!("../../fixtures/extension_common_attributes/base/Catalogs/BaseCatalog.xml");
    const CATALOG_TABLES: &str =
        include_str!("../../fixtures/rename/generated-types/catalog-tables.xml");
    const DOCUMENT_TABLES: &str =
        include_str!("../../fixtures/rename/generated-types/document-tables.xml");

    fn inspect(xml: &str) -> XmlMetadataReferences {
        MetadataXmlSource::decode(xml.as_bytes()).unwrap().inspect(None)
    }

    #[test]
    fn native_categories_have_owned_name_spans_with_opaque_ids_and_bom_unchanged() {
        let source = MetadataXmlSource::decode(CATALOG.as_bytes()).unwrap();
        let facts = source.inspect(None);
        assert!(facts.problems.is_empty(), "{facts:#?}");
        assert_eq!(facts.references.len(), 5);
        let mut edits = facts
            .references
            .iter()
            .map(|reference| {
                let XmlReferenceAnchor::Metadata { kind, name } = &reference.anchor else {
                    panic!("owned metadata name required")
                };
                assert_eq!(*kind, MdoType::Catalog);
                assert_eq!(&CATALOG.as_bytes()[name.range.clone()], b"BaseCatalog");
                name.range.clone()
            })
            .collect::<Vec<_>>();
        edits.sort_by_key(|range| std::cmp::Reverse(range.start));
        let mut after = CATALOG.as_bytes().to_vec();
        for range in edits {
            after.splice(range, source.encode_name("RenamedCatalog"));
        }
        let after = String::from_utf8(after).unwrap();
        let mut expected = CATALOG.to_string();
        for class in
            ["CatalogObject", "CatalogRef", "CatalogSelection", "CatalogList", "CatalogManager"]
        {
            expected = expected.replacen(
                &format!("name=\"{class}.BaseCatalog\""),
                &format!("name=\"{class}.RenamedCatalog\""),
                1,
            );
        }
        assert_eq!(after, expected);
    }

    #[test]
    fn unproved_uuid_owner_category_prefix_and_id_shapes_remain_incomplete() {
        for xml in [
            CATALOG.replacen(
                "018e4d6b-f86e-5cbd-b1eb-c6310ea5d8fa",
                "00000000-0000-0000-0000-000000000000",
                1,
            ),
            CATALOG.replacen("category=\"Object\"", "category=\"Ref\"", 1),
            CATALOG.replacen("<InternalInfo>", "<InternalInfo owner='unproved'>", 1),
            CATALOG.replacen("CatalogObject.BaseCatalog", "DocumentObject.BaseCatalog", 1),
            CATALOG.replacen("CatalogObject.BaseCatalog", "CatalogObject.OtherCatalog", 1),
            CATALOG.replacen("134cbc3f-1950-5b1c-81cc-4767a6d15220", "unknown-id", 1),
            CATALOG.replacen("<xr:GeneratedType ", "<xr:GeneratedType xmlns:xr='urn:foreign' ", 1),
        ] {
            assert!(inspect(&xml).problems.contains(&XmlReferenceProblem::GeneratedTypeContract));
        }
    }

    #[test]
    fn entity_encoded_name_is_attested_against_the_same_source_declaration() {
        let xml = CATALOG.replacen("CatalogObject.BaseCatalog", "CatalogObject.BaseC&#97;talog", 1);
        let facts = inspect(&xml);
        assert!(facts.problems.is_empty(), "{facts:#?}");
        let XmlReferenceAnchor::Metadata { name, .. } = &facts.references[0].anchor else {
            panic!("metadata reference required")
        };
        assert_eq!(&xml.as_bytes()[name.range.clone()], b"BaseC&#97;talog");
    }

    #[test]
    fn full_native_document_exchange_report_and_table_descriptors_are_complete() {
        for (xml, kind, owner, count) in [
            (
                include_str!("../../fixtures/rename/generated-types/document.xml"),
                MdoType::Document,
                "Partners",
                5,
            ),
            (
                include_str!("../../fixtures/rename/generated-types/exchange-plan.xml"),
                MdoType::ExchangePlan,
                "NativeExchange",
                5,
            ),
            (
                include_str!("../../fixtures/rename/generated-types/report.xml"),
                MdoType::Report,
                "NativeReport",
                2,
            ),
            (CATALOG_TABLES, MdoType::Catalog, "NativeCatalog", 8),
            (DOCUMENT_TABLES, MdoType::Document, "NativeReceipt", 7),
        ] {
            let facts = inspect(xml);
            assert!(facts.problems.is_empty(), "{:?}", facts.problems);
            let references = facts.references.iter().filter(|reference| matches!(
                &reference.anchor,
                XmlReferenceAnchor::Metadata { kind: actual, name } if *actual == kind && name.text == owner
            )).collect::<Vec<_>>();
            assert_eq!(references.len(), count);
            for reference in references {
                let XmlReferenceAnchor::Metadata { name, .. } = &reference.anchor else {
                    unreachable!()
                };
                assert_eq!(&xml.as_bytes()[name.range.clone()], owner.as_bytes());
            }
            for invalid in [
                xml.replacen("category=\"Object\"", "category=\"Unproved\"", 1),
                xml.replacen(&format!("Object.{owner}"), &format!("Object.Other{owner}"), 1),
                xml.replacen(
                    "<xr:GeneratedType ",
                    "<xr:GeneratedType xmlns:xr=\"urn:foreign\" ",
                    1,
                ),
            ] {
                assert!(inspect(&invalid)
                    .problems
                    .contains(&XmlReferenceProblem::GeneratedTypeContract));
            }
        }
    }

    #[test]
    fn nested_native_table_names_have_distinct_owner_and_member_spans() {
        for (xml, kind, owner) in [
            (CATALOG_TABLES, MdoType::Catalog, "NativeCatalog"),
            (DOCUMENT_TABLES, MdoType::Document, "NativeReceipt"),
        ] {
            let source = MetadataXmlSource::decode(xml.as_bytes()).unwrap();
            let facts = source.inspect(None);
            let nested = facts
                .references
                .iter()
                .filter(|reference| reference.declared_member_kinds == ["TabularSection"])
                .collect::<Vec<_>>();
            assert_eq!(nested.len(), 2, "{:?}", facts.problems);
            let mut ranges = Vec::new();
            for reference in nested {
                let XmlReferenceAnchor::Metadata { kind: actual, name } = &reference.anchor else {
                    panic!("metadata owner required")
                };
                assert_eq!(*actual, kind);
                assert_eq!(&xml.as_bytes()[name.range.clone()], owner.as_bytes());
                assert_eq!(reference.members.len(), 1);
                let member = &reference.members[0];
                assert_eq!(&xml.as_bytes()[member.range.clone()], b"Rows");
                ranges.push(member.range.clone());
            }
            ranges.sort_by_key(|range| std::cmp::Reverse(range.start));
            let mut after = xml.as_bytes().to_vec();
            for range in ranges {
                after.splice(range, source.encode_name("Lines"));
            }
            let expected = xml.replace(&format!(".{owner}.Rows\""), &format!(".{owner}.Lines\""));
            assert_eq!(after, expected.as_bytes());
            assert!(after.starts_with(b"\xef\xbb\xbf"));
            assert!(after.windows(2).any(|bytes| bytes == b"\r\n"));
        }
    }

    #[test]
    fn nested_generated_names_refuse_unattested_owner_category_namespace_and_ids() {
        let document =
            roxmltree::Document::parse(CATALOG_TABLES.trim_start_matches('\u{feff}')).unwrap();
        let table =
            document.descendants().find(|node| node.has_tag_name((MD, "TabularSection"))).unwrap();
        let table_uuid = table.attribute("uuid").unwrap();
        let generated = table
            .descendants()
            .find(|node| node.has_tag_name((READABLE, "GeneratedType")))
            .unwrap();
        let type_id = generated
            .children()
            .find(|node| node.has_tag_name((READABLE, "TypeId")))
            .unwrap()
            .text()
            .unwrap();
        let generated_xml = &document.input_text()[generated.range()];
        for xml in [
            CATALOG_TABLES.replace(
                "CatalogTabularSection.NativeCatalog.Rows",
                "CatalogTabularSection.Other.Rows",
            ),
            CATALOG_TABLES.replace(
                "CatalogTabularSection.NativeCatalog.Rows",
                "CatalogTabularSection.NativeCatalog.Other",
            ),
            CATALOG_TABLES.replace("category=\"TabularSection\"", "category=\"TabularSectionRow\""),
            CATALOG_TABLES.replace(
                "CatalogTabularSection.NativeCatalog.Rows",
                "DocumentTabularSection.NativeCatalog.Rows",
            ),
            CATALOG_TABLES.replace(table_uuid, "00000000-0000-0000-0000-000000000000"),
            CATALOG_TABLES.replace(type_id, "not-a-uuid"),
            CATALOG_TABLES.replace(
                "<xr:GeneratedType name=\"CatalogTabularSection.",
                "<xr:GeneratedType xmlns:xr=\"urn:foreign\" name=\"CatalogTabularSection.",
            ),
            CATALOG_TABLES.replace(
                &format!("<xr:TypeId>{type_id}</xr:TypeId>"),
                &format!("<xr:TypeId>{type_id}</xr:TypeId><xr:TypeId>{type_id}</xr:TypeId>"),
            ),
            CATALOG_TABLES
                .replace("CatalogObject.NativeCatalog", "CatalogObject.NativeCatalog.Rows"),
            CATALOG_TABLES.replace(generated_xml, &format!("{generated_xml}{generated_xml}")),
        ] {
            assert!(inspect(&xml).problems.contains(&XmlReferenceProblem::GeneratedTypeContract));
        }
    }

    #[test]
    fn native_property_references_are_read_without_accepting_unproved_shapes() {
        let xml = DOCUMENT_TABLES.replace("Catalog.Контрагенты", "Catalog.Partners");
        let facts = inspect(&xml);
        assert!(facts.problems.is_empty(), "{:?}", facts.problems);
        assert!(facts.references.iter().any(|reference| matches!(
            &reference.anchor,
            XmlReferenceAnchor::Metadata { kind: MdoType::Catalog, name } if name.text == "Partners"
        )));
        let report = include_str!("../../fixtures/rename/generated-types/report.xml");
        let exchange = include_str!("../../fixtures/rename/generated-types/exchange-plan.xml");
        for malformed in [
            xml.replacen(
                "<UseStandardCommands>true</UseStandardCommands>",
                "<UseStandardCommands>perhaps</UseStandardCommands>",
                1,
            ),
            xml.replacen("<NumberLength>9</NumberLength>", "<NumberLength>-1</NumberLength>", 1),
            xml.replacen("<Numerator/>", "<Numerator><Unknown/></Numerator>", 1),
            xml.replacen(
                "<Name>NativeReceipt</Name>",
                "<Name>NativeReceipt</Name><UnknownProperty/>",
                1,
            ),
            xml.replacen("xsi:type=\"xr:MDObjectRef\"", "xsi:type=\"xs:MDObjectRef\"", 1),
            xml.replacen("<xr:Item xsi:type=\"xr:MDObjectRef\">", "<xr:Item>", 1),
            xml.replacen(
                "<xr:Item xsi:type=\"xr:MDObjectRef\">",
                "<xr:Item xsi:type=\"xr:MDObjectRef\" extra=\"unproved\">",
                1,
            ),
            report.replace(
                "Report.NativeReport.Template.ОсновнаяСхемаКомпоновкиДанных",
                "Catalog.Partners",
            ),
            report.replacen(
                "<MainDataCompositionSchema>",
                "<MainDataCompositionSchema xmlns=\"urn:foreign\">",
                1,
            ),
            exchange.replacen(
                "<CodeLength>10</CodeLength>",
                "<CodeLength><Nested>10</Nested></CodeLength>",
                1,
            ),
        ] {
            assert!(inspect(&malformed)
                .problems
                .contains(&XmlReferenceProblem::UnknownReferenceContract));
        }
    }
}
