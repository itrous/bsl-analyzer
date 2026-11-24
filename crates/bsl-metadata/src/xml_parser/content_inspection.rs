//! Readable Designer template and role content, without binary format inference.

use serde::{Deserialize, Serialize};

use crate::{MetadataError, Result};

use super::form_inspection::PropertyValue;
use super::helpers::{child_text, find_child};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleRight {
    pub name: Option<String>,
    pub value: Option<bool>,
    pub restrictions: Vec<PropertyValue>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleObjectRights {
    /// The full identity includes attribute/tabular-section tails and Configuration.
    pub object: Option<String>,
    pub rights: Vec<RoleRight>,
}

/// These are declarations, not a computation of a user's effective permissions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleInspection {
    pub set_for_new_objects: Option<bool>,
    pub set_for_attributes_by_default: Option<bool>,
    pub independent_rights_of_child_objects: Option<bool>,
    pub objects: Vec<RoleObjectRights>,
    pub restriction_templates: Vec<PropertyValue>,
    /// Source paths that this reader did not project into the card.
    pub unsupported_paths: Vec<String>,
    pub origin: String,
}

pub fn parse_role_inspection(xml: &str, origin: &str) -> Result<RoleInspection> {
    let document = checked_document(xml, "Rights", "http://v8.1c.ru/8.2/roles")?;
    let root = document.root_element();
    let mut unsupported_paths = Vec::new();
    collect_role_remainder(root, "/Rights", &mut unsupported_paths);
    Ok(RoleInspection {
        set_for_new_objects: optional_bool(root, "setForNewObjects"),
        set_for_attributes_by_default: optional_bool(root, "setForAttributesByDefault"),
        independent_rights_of_child_objects: optional_bool(root, "independentRightsOfChildObjects"),
        objects: children(root, "object")
            .map(|object| RoleObjectRights {
                object: child_text(object, "name").map(str::to_owned),
                rights: children(object, "right")
                    .map(|right| RoleRight {
                        name: child_text(right, "name").map(str::to_owned),
                        value: optional_bool(right, "value"),
                        restrictions: children(right, "restrictionByCondition")
                            .map(PropertyValue::from_node)
                            .collect(),
                    })
                    .collect(),
            })
            .collect(),
        restriction_templates: children(root, "restrictionTemplate")
            .map(PropertyValue::from_node)
            .collect(),
        unsupported_paths,
        origin: origin.to_string(),
    })
}

fn collect_role_remainder(node: roxmltree::Node<'_, '_>, path: &str, result: &mut Vec<String>) {
    for child in node.children().filter(|n| n.is_element()) {
        let name = child.tag_name().name();
        let namespace = child.tag_name().namespace();
        let supported = namespace == node.tag_name().namespace()
            && match node.tag_name().name() {
                "Rights" => matches!(
                    name,
                    "setForNewObjects"
                        | "setForAttributesByDefault"
                        | "independentRightsOfChildObjects"
                        | "object"
                        | "restrictionTemplate"
                ),
                "object" => matches!(name, "name" | "right"),
                "right" => matches!(name, "name" | "value" | "restrictionByCondition"),
                _ => false,
            };
        if !supported {
            result.push(format!("{path}/{{{}}}{name}", namespace.unwrap_or_default()));
        } else if matches!(name, "object" | "right") {
            collect_role_remainder(child, &format!("{path}/{name}"), result);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompositionDataSet {
    pub name: Option<String>,
    pub kind: Option<String>,
    pub data_source: Option<String>,
    pub query: Option<String>,
    pub fields: Vec<PropertyValue>,
    pub data_sets: Vec<Self>,
    pub parameter_bindings: Vec<PropertyValue>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompositionInspection {
    pub data_sources: Vec<PropertyValue>,
    pub data_sets: Vec<CompositionDataSet>,
    pub parameters: Vec<PropertyValue>,
    pub calculated_fields: Vec<PropertyValue>,
    pub settings_variants: Vec<PropertyValue>,
    /// Source paths that this reader did not project into the card.
    pub unsupported_paths: Vec<String>,
    pub origin: String,
}

pub fn parse_composition_inspection(xml: &str, origin: &str) -> Result<CompositionInspection> {
    let document = checked_document(
        xml,
        "DataCompositionSchema",
        "http://v8.1c.ru/8.1/data-composition-system/schema",
    )?;
    let root = document.root_element();
    let mut unsupported_paths = Vec::new();
    collect_composition_remainder(root, "/DataCompositionSchema", false, &mut unsupported_paths);
    Ok(CompositionInspection {
        data_sources: children(root, "dataSource").map(PropertyValue::from_node).collect(),
        data_sets: children(root, "dataSet").map(parse_data_set).collect(),
        parameters: children(root, "parameter").map(PropertyValue::from_node).collect(),
        calculated_fields: children(root, "calculatedField")
            .map(PropertyValue::from_node)
            .collect(),
        settings_variants: children(root, "settingsVariant")
            .map(PropertyValue::from_node)
            .collect(),
        unsupported_paths,
        origin: origin.to_string(),
    })
}

fn collect_composition_remainder(
    node: roxmltree::Node<'_, '_>,
    path: &str,
    data_set: bool,
    result: &mut Vec<String>,
) {
    for child in node.children().filter(|n| n.is_element()) {
        let name = child.tag_name().name();
        let namespace = child.tag_name().namespace();
        let supported = namespace == node.tag_name().namespace()
            && if data_set {
                matches!(name, "name" | "dataSource" | "query" | "field" | "dataSet" | "parameter")
            } else {
                matches!(
                    name,
                    "dataSource" | "dataSet" | "parameter" | "calculatedField" | "settingsVariant"
                )
            };
        if !supported {
            result.push(format!("{path}/{{{}}}{name}", namespace.unwrap_or_default()));
        } else if name == "dataSet" {
            collect_composition_remainder(child, &format!("{path}/dataSet"), true, result);
        }
    }
}

fn parse_data_set(node: roxmltree::Node<'_, '_>) -> CompositionDataSet {
    CompositionDataSet {
        name: child_text(node, "name").map(str::to_owned),
        kind: node.attribute(("http://www.w3.org/2001/XMLSchema-instance", "type")).map(|kind| {
            let (prefix, name) =
                kind.split_once(':').map_or((None, kind), |(prefix, name)| (Some(prefix), name));
            node.lookup_namespace_uri(prefix)
                .map_or_else(|| kind.to_string(), |ns| format!("{{{ns}}}{name}"))
        }),
        data_source: child_text(node, "dataSource").map(str::to_owned),
        query: child_text(node, "query").map(str::to_owned),
        fields: children(node, "field").map(PropertyValue::from_node).collect(),
        data_sets: children(node, "dataSet").map(parse_data_set).collect(),
        parameter_bindings: children(node, "parameter").map(PropertyValue::from_node).collect(),
    }
}

/// Coordinates are the original zero-based Designer indices; -1 means unrestricted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpreadsheetArea {
    pub name: Option<String>,
    pub kind: Option<String>,
    pub begin_row: Option<i32>,
    pub end_row: Option<i32>,
    pub begin_column: Option<i32>,
    pub end_column: Option<i32>,
    pub columns_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpreadsheetParameter {
    pub row: u32,
    pub column: u32,
    pub columns_id: Option<String>,
    pub name: Option<String>,
    pub template: Option<PropertyValue>,
    pub template_parameters: Vec<String>,
    pub detail_parameter: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpreadsheetInspection {
    pub areas: Vec<SpreadsheetArea>,
    pub parameters: Vec<SpreadsheetParameter>,
    pub origin: String,
}

/// Read the platform's SpreadsheetDocument XML export, not a binary MXL container.
pub fn parse_spreadsheet_inspection(xml: &str, origin: &str) -> Result<SpreadsheetInspection> {
    let document = checked_document(xml, "document", "http://v8.1c.ru/8.2/data/spreadsheet")?;
    let root = document.root_element();
    let areas = children(root, "namedItem")
        .map(|node| {
            let area = find_child(node, "area");
            let number = |name| area.and_then(|a| child_text(a, name)).and_then(|n| n.parse().ok());
            SpreadsheetArea {
                name: child_text(node, "name").map(str::to_owned),
                kind: area.and_then(|a| child_text(a, "type")).map(str::to_owned),
                begin_row: number("beginRow"),
                end_row: number("endRow"),
                begin_column: number("beginColumn"),
                end_column: number("endColumn"),
                columns_id: area.and_then(|a| child_text(a, "columnsID")).map(str::to_owned),
            }
        })
        .collect();
    let formats: Vec<_> = children(root, "format").collect();
    let mut parameters = Vec::new();
    for rows_item in children(root, "rowsItem") {
        let Some(row_index) = child_text(rows_item, "index").and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        let Some(row) = find_child(rows_item, "row") else { continue };
        let columns_id = child_text(row, "columnsID").map(str::to_owned);
        let mut column = 0u32;
        for group in children(row, "c") {
            if let Some(index) = child_text(group, "i").and_then(|n| n.parse().ok()) {
                column = index;
            }
            if let Some(content) = find_child(group, "c") {
                let fill = child_text(content, "f")
                    .and_then(|n| n.parse::<usize>().ok())
                    .and_then(|i| i.checked_sub(1))
                    .and_then(|i| formats.get(i))
                    .and_then(|f| child_text(*f, "fillType"));
                let parameter = find_child(content, "parameter");
                let detail = find_child(content, "detailParameter");
                if parameter.is_some()
                    || detail.is_some()
                    || matches!(fill, Some("Parameter" | "Template"))
                {
                    let template =
                        (fill == Some("Template")).then(|| find_child(content, "tl")).flatten();
                    let mut template_parameters = Vec::new();
                    if let Some(template) = template {
                        for text in template
                            .descendants()
                            .filter(|n| n.is_element() && n.tag_name().name() == "content")
                            .filter_map(|n| n.text())
                        {
                            for name in template_names(text) {
                                if !template_parameters.contains(&name) {
                                    template_parameters.push(name);
                                }
                            }
                        }
                    }
                    parameters.push(SpreadsheetParameter {
                        row: row_index,
                        column,
                        columns_id: columns_id.clone(),
                        name: parameter.map(|n| n.text().unwrap_or_default().to_string()),
                        template: template.map(PropertyValue::from_node),
                        template_parameters,
                        detail_parameter: detail.map(|n| n.text().unwrap_or_default().to_string()),
                    });
                }
            }
            column = column.saturating_add(1);
        }
    }
    Ok(SpreadsheetInspection { areas, parameters, origin: origin.to_string() })
}

fn template_names(text: &str) -> Vec<String> {
    let mut result = Vec::new();
    let mut rest = text;
    while let Some((_, suffix)) = rest.split_once('[') {
        let Some((name, tail)) = suffix.split_once(']') else { break };
        let name = name.trim();
        if !name.is_empty()
            && !name.contains('[')
            && !name.starts_with('&')
            && !name.chars().all(|c| c.is_ascii_digit())
            && !result.iter().any(|n| n == name)
        {
            result.push(name.to_string());
        }
        rest = tail;
    }
    result
}

fn children<'a>(
    node: roxmltree::Node<'a, 'a>,
    name: &'a str,
) -> impl Iterator<Item = roxmltree::Node<'a, 'a>> {
    node.children().filter(move |n| {
        n.is_element()
            && n.tag_name().name() == name
            && n.tag_name().namespace() == node.tag_name().namespace()
    })
}

fn optional_bool(node: roxmltree::Node<'_, '_>, name: &str) -> Option<bool> {
    match child_text(node, name) {
        Some("true" | "1") => Some(true),
        Some("false" | "0") => Some(false),
        _ => None,
    }
}

fn checked_document<'a>(
    xml: &'a str,
    name: &str,
    namespace: &str,
) -> Result<roxmltree::Document<'a>> {
    let document = roxmltree::Document::parse(xml.trim_start_matches('\u{feff}'))
        .map_err(|e| MetadataError::InvalidFormat(e.to_string()))?;
    let root = document.root_element();
    if root.tag_name().name() != name || root.tag_name().namespace() != Some(namespace) {
        return Err(MetadataError::InvalidFormat(format!("Expected {name} in {namespace}")));
    }
    Ok(document)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_inspection_keeps_full_objects_false_unknown_and_rls_owners() {
        let role =
            parse_role_inspection(include_str!("../../fixtures/inspection/rights.xml"), "root-a")
                .unwrap();
        assert_eq!(role.set_for_new_objects, Some(false));
        assert_eq!(role.set_for_attributes_by_default, None);
        assert_eq!(role.objects[0].object.as_deref(), Some("Catalog.Partners.Attribute.Secret"));
        assert_eq!(role.objects[0].rights[0].value, Some(false));
        assert_eq!(
            role.objects[0].rights[1].restrictions[0].children[0].text.as_deref(),
            Some("Partner = &AllowedPartner")
        );
        assert_eq!(role.objects[1].object.as_deref(), Some("Configuration.Demo"));
        assert_eq!(role.objects[1].rights[0].value, None);
        assert_eq!(role.restriction_templates.len(), 1);
        assert_eq!(role.origin, "root-a");
    }

    #[test]
    fn composition_inspection_keeps_nested_sets_query_fields_and_settings() {
        let schema = parse_composition_inspection(
            include_str!("../../fixtures/inspection/composition.xml"),
            "report",
        )
        .unwrap();
        assert_eq!(schema.data_sets.len(), 1);
        let query = &schema.data_sets[0].data_sets[0];
        assert_eq!(
            query.query.as_deref(),
            Some("SELECT Ref\nFROM Catalog.Partners WHERE Code = &Code")
        );
        assert_eq!(query.fields.len(), 1);
        assert_eq!(schema.parameters.len(), 1);
        assert_eq!(schema.settings_variants.len(), 1);
        assert!(schema.data_sets[0].kind.as_ref().unwrap().ends_with("}DataSetUnion"));
    }

    #[test]
    fn spreadsheet_inspection_keeps_named_areas_and_parameter_coordinates() {
        let sheet = parse_spreadsheet_inspection(
            include_str!("../../fixtures/inspection/spreadsheet.xml"),
            "print",
        )
        .unwrap();
        assert_eq!(sheet.areas[0].name.as_deref(), Some("Header"));
        assert_eq!(sheet.areas[0].begin_row, Some(0));
        assert_eq!(sheet.areas[0].begin_column, Some(-1));
        assert_eq!(sheet.parameters.len(), 2);
        assert_eq!(sheet.parameters[0].row, 0);
        assert_eq!(sheet.parameters[0].column, 3);
        assert_eq!(sheet.parameters[0].name.as_deref(), Some("Title"));
        assert_eq!(sheet.parameters[1].template_parameters, ["Partner", "Amount"]);
        assert!(parse_spreadsheet_inspection("<document/>", "bad").is_err());
        assert!(parse_role_inspection("<Rights/>", "bad").is_err());
    }
}
