use crate::read_properties::{
    LocalizedString, MemberReadProperties, MetadataValue, ObjectReadProperties,
};

use super::helpers::{child_text, child_u32, find_child};
use super::type_parser::parse_type_xml;

pub(crate) fn parse_localized(props: roxmltree::Node<'_, '_>) -> Option<LocalizedString> {
    let synonym = find_child(props, "Synonym")?;
    let mut result = LocalizedString::new();
    for item in synonym.children().filter(|node| node.is_element()) {
        let lang = child_text(item, "lang")?;
        let content = find_child(item, "content")?;
        if result.insert(lang.to_string(), content.text().unwrap_or_default().to_string()).is_some()
        {
            return None;
        }
    }
    Some(result)
}

pub(crate) fn parse_value(node: roxmltree::Node<'_, '_>) -> MetadataValue {
    let fields: Vec<_> = node
        .children()
        .filter(|node| node.is_element())
        .map(|child| (child.tag_name().name().to_string(), parse_value(child)))
        .collect();
    let type_name =
        node.attribute(("http://www.w3.org/2001/XMLSchema-instance", "type")).map(|raw| {
            if let Some((prefix, name)) = raw.split_once(':') {
                if let Some(namespace) = node.lookup_namespace_uri(Some(prefix)) {
                    return format!("{{{namespace}}}{name}");
                }
            }
            raw.to_string()
        });
    MetadataValue {
        type_name,
        value: fields.is_empty().then(|| node.text().unwrap_or_default().to_string()),
        nil: node
            .attribute(("http://www.w3.org/2001/XMLSchema-instance", "nil"))
            .is_some_and(|value| value == "true" || value == "1"),
        fields,
        runtime_predefined_identity: None,
    }
}

pub(crate) fn parse_read_properties(mdo: roxmltree::Node<'_, '_>) -> ObjectReadProperties {
    let Some(props) = find_child(mdo, "Properties") else {
        return ObjectReadProperties::default();
    };
    let synonym = parse_localized(props);
    let synonym_complete = synonym.as_ref().map(|_| true);
    let mut result = ObjectReadProperties {
        synonym,
        synonym_complete,
        hierarchical: match child_text(props, "Hierarchical") {
            Some("true" | "1") => Some(true),
            Some("false" | "0") => Some(false),
            _ => None,
        },
        hierarchy_type: child_text(props, "HierarchyType").map(str::to_owned),
        code_type: child_text(props, "CodeType").map(str::to_owned),
        code_length: child_u32(props, "CodeLength"),
        code_allowed_length: child_text(props, "CodeAllowedLength").map(str::to_owned),
        periodicity: child_text(props, "InformationRegisterPeriodicity").map(str::to_owned),
        write_mode: child_text(props, "WriteMode").map(str::to_owned),
        characteristic_value_type: (mdo.tag_name().name() == "ChartOfCharacteristicTypes")
            .then(|| find_child(props, "Type").and_then(|node| parse_type_xml(node).ok()))
            .flatten(),
        ..ObjectReadProperties::default()
    };
    collect_members(mdo, &[], &mut result.members);
    result
}

fn collect_members(
    node: roxmltree::Node<'_, '_>,
    parent: &[String],
    members: &mut Vec<MemberReadProperties>,
) {
    let Some(children) = find_child(node, "ChildObjects") else {
        return;
    };
    for child in children.children().filter(|node| node.is_element()) {
        let Some(props) = find_child(child, "Properties") else {
            continue;
        };
        let Some(name) = child_text(props, "Name") else {
            continue;
        };
        let mut path = parent.to_vec();
        path.extend([child.tag_name().name().to_string(), name.to_string()]);
        let synonym = parse_localized(props);
        let synonym_complete = synonym.as_ref().map(|_| true);
        members.push(MemberReadProperties {
            path: path.clone(),
            synonym,
            synonym_complete,
            use_mode: child_text(props, "Use").map(str::to_owned),
            fill_value: find_child(props, "FillValue").map(parse_value),
            choice: super::form_inspection::ChoiceProperties::from_node(props, ""),
        });
        collect_members(child, &path, members);
    }
}

#[cfg(test)]
mod tests {
    use crate::{xml_parser::*, AttributeType, Configuration};

    fn object(kind: &str, properties: &str, children: &str) -> String {
        format!(
            r#"<MetaDataObject xmlns:v8="http://v8.1c.ru/8.1/data/core" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xmlns:primitive="http://www.w3.org/2001/XMLSchema" xmlns:own="http://v8.1c.ru/8.1/data/enterprise/current-config"><{kind} uuid="00000000-0000-0000-0000-000000000001"><Properties><Name>Sample</Name>{properties}</Properties><ChildObjects>{children}</ChildObjects></{kind}></MetaDataObject>"#
        )
    }

    #[test]
    fn source_read_properties_preserve_locales_empty_unknown_and_all_member_levels() {
        let synonym = r#"<Synonym><v8:item><v8:lang>ru</v8:lang><v8:content>Товар &amp; услуга</v8:content></v8:item><v8:item><v8:lang>en</v8:lang><v8:content>Product</v8:content></v8:item></Synonym>"#;
        let children = format!(
            r#"<Attribute><Properties><Name>Code2</Name>{synonym}<Use>ForItem</Use><Type><v8:Type>primitive:string</v8:Type></Type></Properties></Attribute><TabularSection uuid="00000000-0000-0000-0000-000000000002"><Properties><Name>Rows</Name><Synonym/></Properties><ChildObjects><Attribute uuid="00000000-0000-0000-0000-000000000003"><Properties><Name>Quantity</Name>{synonym}<Type><v8:Type>primitive:decimal</v8:Type></Type></Properties></Attribute></ChildObjects></TabularSection>"#
        );
        let value = parse_catalog_xml(&object("Catalog", synonym, &children)).unwrap();
        let properties = value.read_properties;
        assert_eq!(properties.synonym.as_ref().unwrap()["ru"], "Товар & услуга");
        assert_eq!(properties.synonym.as_ref().unwrap()["en"], "Product");
        assert_eq!(properties.synonym_complete, Some(true));
        assert_eq!(properties.members.len(), 3);
        assert_eq!(properties.members[0].use_mode.as_deref(), Some("ForItem"));
        assert!(properties.members[1].synonym.as_ref().unwrap().is_empty());
        assert_eq!(properties.members[2].path, ["TabularSection", "Rows", "Attribute", "Quantity"]);
        assert_eq!(properties.members[2].synonym.as_ref().unwrap()["en"], "Product");
        let missing = parse_catalog_xml(&object("Catalog", "", "")).unwrap();
        assert!(missing.read_properties.synonym.is_none());
        assert_eq!(missing.read_properties.synonym_complete, None);
    }

    #[test]
    fn source_read_properties_catalog_hierarchy_and_code_are_independent_facts() {
        for (hierarchical, kind, has_folder, has_parent) in [
            (false, "HierarchyFoldersAndItems", false, false),
            (true, "HierarchyItems", false, true),
            (true, "HierarchyFoldersAndItems", true, true),
        ] {
            let properties = format!(
                "<Hierarchical>{hierarchical}</Hierarchical><HierarchyType>{kind}</HierarchyType><CodeType>String</CodeType><CodeLength>12</CodeLength><CodeAllowedLength>Fixed</CodeAllowedLength>"
            );
            let value = parse_catalog_xml(&object("Catalog", &properties, "")).unwrap();
            assert_eq!(
                value.find_attribute("ЭтоГруппа").is_some(),
                has_folder,
                "{hierarchical} {kind}"
            );
            assert_eq!(value.find_attribute("Родитель").is_some(), has_parent);
            assert_eq!(value.read_properties.code_length, Some(12));
            assert_eq!(value.read_properties.code_allowed_length.as_deref(), Some("Fixed"));
        }
        let characteristic = parse_chart_of_characteristic_types_xml(&object(
            "ChartOfCharacteristicTypes",
            "<Hierarchical>true</Hierarchical>",
            "",
        ))
        .unwrap();
        assert!(characteristic.find_attribute("ЭтоГруппа").is_some());
    }

    #[test]
    fn source_read_properties_register_standard_fields_follow_write_mode_and_periodicity() {
        for (write, subordinate) in [("Independent", false), ("RecorderSubordinate", true)] {
            for (periodicity, periodic) in [("Nonperiodical", false), ("Second", true)] {
                let properties = format!(
                    "<WriteMode>{write}</WriteMode><InformationRegisterPeriodicity>{periodicity}</InformationRegisterPeriodicity>"
                );
                let value =
                    parse_information_register_xml(&object("InformationRegister", &properties, ""))
                        .unwrap();
                let names: Vec<_> = value.attributes().iter().map(|field| field.name()).collect();
                for name in ["Регистратор", "НомерСтроки", "Активность"]
                {
                    assert_eq!(names.contains(&name), subordinate, "{write} {periodicity}: {name}");
                }
                assert_eq!(names.contains(&"Период"), periodic);
                assert_eq!(value.read_properties().write_mode.as_deref(), Some(write));
                assert_eq!(value.read_properties().periodicity.as_deref(), Some(periodicity));
            }
        }
    }

    #[test]
    fn source_read_properties_fill_value_preserves_typed_empty_null_and_missing() {
        for (fill, expected) in [
            ("<FillValue xsi:nil=\"true\"/>", Some((true, ""))),
            ("<FillValue xsi:type=\"primitive:string\"/>", Some((false, ""))),
            ("<FillValue xsi:type=\"primitive:decimal\">0</FillValue>", Some((false, "0"))),
            ("", None),
        ] {
            let child = format!(
                "<Resource uuid=\"00000000-0000-0000-0000-000000000003\"><Properties><Name>Amount</Name><Type><v8:Type>primitive:decimal</v8:Type></Type>{fill}</Properties></Resource>"
            );
            let value =
                parse_information_register_xml(&object("InformationRegister", "", &child)).unwrap();
            let fill = &value.read_properties().members[0].fill_value;
            assert_eq!(
                fill.as_ref().map(|fill| (fill.nil, fill.value.as_deref().unwrap())),
                expected
            );
            if let Some(fill) = fill {
                assert!(fill.runtime_predefined_identity.is_none());
                let json = serde_json::to_value(fill).unwrap();
                assert_eq!(json.as_object().unwrap().len(), 4);
                assert!(json.get("runtime_predefined_identity").is_none());
                if let Some(ty) = &fill.type_name {
                    assert!(ty.starts_with("{http://www.w3.org/2001/XMLSchema}"));
                }
            }
        }
    }

    #[test]
    fn source_read_properties_characteristic_type_resolves_namespace_aliases() {
        let properties = "<Type><v8:Type>primitive:boolean</v8:Type><v8:Type>own:CatalogRef.ExtensionCatalog</v8:Type><v8:TypeSet>own:DefinedType.ExtensionType</v8:TypeSet></Type>";
        let value = parse_chart_of_characteristic_types_xml(&object(
            "ChartOfCharacteristicTypes",
            properties,
            "",
        ))
        .unwrap();
        assert_eq!(
            value.read_properties.characteristic_value_type,
            Some(AttributeType::Composite {
                types: vec![
                    AttributeType::Boolean,
                    AttributeType::Ref {
                        mdo_type: crate::MdoType::Catalog,
                        name: "ExtensionCatalog".into()
                    },
                    AttributeType::DefinedType { name: "ExtensionType".into() }
                ]
            })
        );
        let unknown = parse_chart_of_characteristic_types_xml(&object(
            "ChartOfCharacteristicTypes",
            "<Type><v8:Type>own:Unsupported.Future</v8:Type></Type>",
            "",
        ))
        .unwrap();
        assert!(matches!(
            unknown.read_properties.characteristic_value_type,
            Some(AttributeType::UnknownNamed(_))
        ));
    }

    #[test]
    fn source_read_properties_extension_overlay_retains_unstated_and_replaces_explicit_empty() {
        let mut base = parse_catalog_xml(&object("Catalog","<Synonym><v8:item><v8:lang>ru</v8:lang><v8:content>База</v8:content></v8:item></Synonym><CodeLength>9</CodeLength>","")).unwrap();
        let overlay = parse_catalog_xml(&object("Catalog", "<Synonym/>", "")).unwrap();
        base.apply_extension_overlay(&overlay);
        assert!(base.read_properties.synonym.unwrap().is_empty());
        assert_eq!(base.read_properties.code_length, Some(9));
    }

    #[test]
    fn source_read_properties_configuration_modes_are_source_values() {
        for (compatibility, modality) in [("Version8_2_16", "Use"), ("Version8_3_27", "DontUse")] {
            let config = Configuration::from_xml_str(&object("Configuration",&format!("<CompatibilityMode>{compatibility}</CompatibilityMode><ModalityUseMode>{modality}</ModalityUseMode>"),"")).unwrap();
            assert_eq!(config.compatibility_mode(), Some(compatibility));
            assert_eq!(config.modality_use_mode(), Some(modality));
        }
        let unknown = Configuration::from_xml_str(&object("Configuration", "", "")).unwrap();
        assert_eq!(unknown.compatibility_mode(), None);
        assert_eq!(unknown.modality_use_mode(), None);
    }

    #[test]
    fn source_read_properties_namespace_binding_is_not_guessed_from_prefix() {
        let xml = r#"<MetaDataObject xmlns:cfg="urn:foreign"><ChartOfCharacteristicTypes><Properties><Name>Sample</Name><Type><Type>cfg:CatalogRef.Foreign</Type></Type></Properties></ChartOfCharacteristicTypes></MetaDataObject>"#;
        let value = parse_chart_of_characteristic_types_xml(xml).unwrap();
        assert!(matches!(
            value.read_properties.characteristic_value_type,
            Some(AttributeType::UnknownNamed(_))
        ));
    }
}
