//! Source form facts which retain their owner and declaration location.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{inspection_sources::BinaryFingerprint, AttributeType, Form, MetadataError};

use super::form_sidecar::BOUND_HANDLER_EVENT;
use super::helpers::{child_text, find_child};
use super::type_parser::parse_type_xml;

/// A declared XML property, preserving namespaces and attributes of choice values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PropertyValue {
    pub namespace: Option<String>,
    pub name: String,
    pub attributes: BTreeMap<String, String>,
    pub text: Option<String>,
    pub children: Vec<Self>,
}

impl PropertyValue {
    pub fn estimated_heap_size(&self) -> usize {
        self.namespace.as_ref().map_or(0, String::capacity)
            + self.name.capacity()
            + self.text.as_ref().map_or(0, String::capacity)
            + self
                .attributes
                .iter()
                .map(|(k, v)| std::mem::size_of::<(String, String)>() + k.capacity() + v.capacity())
                .sum::<usize>()
            + stdx::heap::vec_bytes::<Self>(self.children.len())
            + self.children.iter().map(Self::estimated_heap_size).sum::<usize>()
    }
    pub(crate) fn from_node(node: roxmltree::Node<'_, '_>) -> Self {
        let children: Vec<_> =
            node.children().filter(|n| n.is_element()).map(Self::from_node).collect();
        Self {
            namespace: node.tag_name().namespace().map(str::to_owned),
            name: node.tag_name().name().to_string(),
            attributes: node
                .attributes()
                .map(|a| {
                    let key = a
                        .namespace()
                        .map_or_else(|| a.name().to_string(), |ns| format!("{{{ns}}}{}", a.name()));
                    let value = if a.namespace()
                        == Some("http://www.w3.org/2001/XMLSchema-instance")
                        && a.name() == "type"
                    {
                        let (prefix, name) = a
                            .value()
                            .split_once(':')
                            .map_or((None, a.value()), |(prefix, name)| (Some(prefix), name));
                        node.lookup_namespace_uri(prefix)
                            .map_or_else(|| a.value().to_string(), |ns| format!("{{{ns}}}{name}"))
                    } else {
                        a.value().to_string()
                    };
                    (key, value)
                })
                .collect(),
            text: children.is_empty().then(|| node.text().unwrap_or_default().to_string()),
            children,
        }
    }
}

/// `None` means the property was not stated; an empty node is explicitly empty.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ChoiceProperties {
    pub choice_parameters: Option<PropertyValue>,
    pub choice_parameter_links: Option<PropertyValue>,
    pub fill_checking: Option<String>,
    pub property_origins: BTreeMap<String, String>,
}

impl ChoiceProperties {
    pub fn estimated_heap_size(&self) -> usize {
        self.choice_parameters.as_ref().map_or(0, PropertyValue::estimated_heap_size)
            + self.choice_parameter_links.as_ref().map_or(0, PropertyValue::estimated_heap_size)
            + self.fill_checking.as_ref().map_or(0, String::capacity)
            + self
                .property_origins
                .iter()
                .map(|(k, v)| std::mem::size_of::<(String, String)>() + k.capacity() + v.capacity())
                .sum::<usize>()
    }
    pub(crate) fn from_node(node: roxmltree::Node<'_, '_>, origin: &str) -> Self {
        Self {
            choice_parameters: find_child(node, "ChoiceParameters").map(PropertyValue::from_node),
            choice_parameter_links: find_child(node, "ChoiceParameterLinks")
                .map(PropertyValue::from_node),
            fill_checking: child_text(node, "FillChecking").map(str::to_owned),
            property_origins: ["ChoiceParameters", "ChoiceParameterLinks", "FillChecking"]
                .into_iter()
                .filter(|name| find_child(node, name).is_some())
                .map(|name| (name.to_string(), origin.to_string()))
                .collect(),
        }
    }

    pub(crate) fn apply_overlay(&mut self, overlay: &Self) {
        self.property_origins.extend(overlay.property_origins.clone());
        if overlay.choice_parameters.is_some() {
            self.choice_parameters.clone_from(&overlay.choice_parameters);
        }
        if overlay.choice_parameter_links.is_some() {
            self.choice_parameter_links.clone_from(&overlay.choice_parameter_links);
        }
        if overlay.fill_checking.is_some() {
            self.fill_checking.clone_from(&overlay.fill_checking);
        }
    }
}

/// Event identity includes its owner and call slot, independently of its procedure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FormBinding {
    pub owner_kind: String,
    pub owner: Option<String>,
    pub event: String,
    pub procedure: String,
    pub call_type: Option<String>,
    pub origin: String,
}

/// A form parameter, kept separately from attributes and their columns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FormParameter {
    pub name: String,
    pub attr_type: Option<AttributeType>,
    pub type_origin: Option<String>,
    pub key_parameter: Option<bool>,
    pub origin: String,
}

/// An element in source order, including attached command bars and their children.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InspectedElement {
    pub name: String,
    pub id: u32,
    pub kind: String,
    pub parent_id: Option<u32>,
    pub data_path: Option<String>,
    pub data_path_origin: Option<String>,
    pub page: Option<String>,
    pub source_identity: Option<String>,
    pub data_path_coverage: Option<String>,
    pub properties: ChoiceProperties,
    pub origin: String,
}

/// A full attribute/column address avoids conflating similarly named fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InspectedAttribute {
    pub path: Vec<String>,
    pub attr_type: Option<AttributeType>,
    pub type_origin: Option<String>,
    pub properties: ChoiceProperties,
    pub origin: String,
}

/// Facts read from the current form content; the historical `BaseForm` is excluded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FormInspection {
    pub form_type: String,
    pub elements: Vec<InspectedElement>,
    pub bindings: Vec<FormBinding>,
    pub parameters: Option<Vec<FormParameter>>,
    pub attributes: Option<Vec<InspectedAttribute>>,
    pub pages: Option<Vec<String>>,
    pub producer: Option<serde_json::Value>,
    pub freshness: String,
    pub has_base_snapshot: bool,
    pub origins: Vec<String>,
    pub incomplete_reasons: Vec<String>,
}

impl FormInspection {
    /// Merge only after the caller has established descriptor adoption by UUID.
    pub fn apply_overlay(&mut self, overlay: &Self) {
        for item in &overlay.elements {
            if let Some(base) = self.elements.iter_mut().find(|base| base.id == item.id) {
                base.name.clone_from(&item.name);
                base.kind.clone_from(&item.kind);
                base.parent_id = item.parent_id;
                if item.data_path.is_some() {
                    base.data_path.clone_from(&item.data_path);
                    base.data_path_origin.clone_from(&item.data_path_origin);
                }
                base.properties.apply_overlay(&item.properties);
                base.origin.clone_from(&item.origin);
            } else {
                self.elements.push(item.clone());
            }
        }
        for binding in &overlay.bindings {
            if let Some(base) = self.bindings.iter_mut().find(|base| {
                base.owner_kind == binding.owner_kind
                    && base.owner == binding.owner
                    && base.event == binding.event
                    && base.call_type == binding.call_type
                    && (binding.owner_kind != "unknown"
                        || binding.owner.is_some()
                        || stdx::case::eq_ignore_case(&base.procedure, &binding.procedure))
            }) {
                base.clone_from(binding);
            } else {
                self.bindings.push(binding.clone());
            }
        }
        if self.parameters.is_none() && overlay.parameters.is_some() {
            self.incomplete_reasons.push("inherited_parameters_unread".into());
        }
        if self.attributes.is_none() && overlay.attributes.is_some() {
            self.incomplete_reasons.push("inherited_attributes_unread".into());
        }
        if let Some(parameters) = &overlay.parameters {
            let base = self.parameters.get_or_insert_default();
            for parameter in parameters {
                if let Some(existing) = base.iter_mut().find(|p| p.name == parameter.name) {
                    if parameter.attr_type.is_some() {
                        existing.attr_type.clone_from(&parameter.attr_type);
                        existing.type_origin.clone_from(&parameter.type_origin);
                    }
                    if parameter.key_parameter.is_some() {
                        existing.key_parameter = parameter.key_parameter;
                    }
                    existing.origin.clone_from(&parameter.origin);
                } else {
                    base.push(parameter.clone());
                }
            }
        }
        if let Some(attributes) = &overlay.attributes {
            let base = self.attributes.get_or_insert_default();
            for attribute in attributes {
                if let Some(existing) = base.iter_mut().find(|p| p.path == attribute.path) {
                    if attribute.attr_type.is_some() {
                        existing.attr_type.clone_from(&attribute.attr_type);
                        existing.type_origin.clone_from(&attribute.type_origin);
                    }
                    existing.properties.apply_overlay(&attribute.properties);
                    existing.origin.clone_from(&attribute.origin);
                } else {
                    base.push(attribute.clone());
                }
            }
        }
        for origin in &overlay.origins {
            if !self.origins.contains(origin) {
                self.origins.push(origin.clone());
            }
        }
        self.incomplete_reasons.extend(overlay.incomplete_reasons.iter().cloned());
        self.has_base_snapshot |= overlay.has_base_snapshot;
    }
}

/// Read a managed Designer form without treating a historical snapshot as current.
pub fn parse_managed_form_inspection(xml: &str, origin: &str) -> crate::Result<FormInspection> {
    let document = roxmltree::Document::parse(xml.trim_start_matches('\u{feff}'))
        .map_err(|e| MetadataError::InvalidFormat(format!("Invalid form XML: {e}")))?;
    let root = document.root_element();
    if root.tag_name().name() != "Form"
        || root.tag_name().namespace() != Some("http://v8.1c.ru/8.3/xcf/logform")
    {
        return Err(MetadataError::InvalidFormat("Expected managed logform Form content".into()));
    }
    let parsed = super::form::parse_form_xml(xml.trim_start_matches('\u{feff}'))?;
    let nodes: BTreeMap<_, _> = root
        .descendants()
        .filter(|n| {
            n.is_element()
                && !n.ancestors().any(|p| {
                    matches!(
                        p.tag_name().name(),
                        "BaseForm" | "Attributes" | "Commands" | "Parameters"
                    )
                })
        })
        .filter_map(|n| {
            Some(((n.attribute("name")?.to_string(), n.attribute("id")?.to_string()), n))
        })
        .collect();
    let mut bindings = Vec::new();
    collect_bindings(root, "form", None, origin, &mut bindings);
    let mut elements = Vec::new();
    for element in parsed.elements() {
        let id = if element.id == u32::MAX { "-1".to_string() } else { element.id.to_string() };
        let Some(node) = nodes.get(&(element.name.clone(), id)) else { continue };
        collect_bindings(*node, "element", Some(&element.name), origin, &mut bindings);
        elements.push(InspectedElement {
            name: element.name.clone(),
            id: element.id,
            kind: node.tag_name().name().to_string(),
            parent_id: element.parent_id,
            data_path: element.data_path.clone(),
            data_path_origin: element.data_path.as_ref().map(|_| origin.into()),
            properties: ChoiceProperties::from_node(*node, origin),
            origin: origin.to_string(),
            page: None,
            source_identity: None,
            data_path_coverage: None,
        });
    }
    if let Some(commands) = find_child(root, "Commands") {
        for command in commands.children().filter(|n| n.has_tag_name("Command")) {
            if let Some(action) = find_child(command, "Action") {
                bindings.push(FormBinding {
                    owner_kind: "command".into(),
                    owner: command.attribute("name").map(str::to_owned),
                    event: "Action".into(),
                    procedure: action.text().unwrap_or_default().trim().into(),
                    call_type: action.attribute("callType").map(str::to_owned),
                    origin: origin.into(),
                });
            }
        }
    }
    let parameters = find_child(root, "Parameters").map(|parameters| {
        parameters
            .children()
            .filter(|n| n.has_tag_name("Parameter"))
            .filter_map(|node| {
                Some(FormParameter {
                    name: node.attribute("name")?.into(),
                    attr_type: find_child(node, "Type").and_then(|n| parse_type_xml(n).ok()),
                    type_origin: find_child(node, "Type").map(|_| origin.into()),
                    key_parameter: optional_bool(node, "KeyParameter"),
                    origin: origin.into(),
                })
            })
            .collect()
    });
    let attributes = find_child(root, "Attributes").map(|attributes| {
        let mut result = Vec::new();
        for node in attributes.children().filter(|n| n.has_tag_name("Attribute")) {
            collect_attribute(node, &[], origin, &mut result);
        }
        result
    });
    Ok(FormInspection {
        form_type: "Managed".into(),
        elements,
        bindings,
        parameters,
        attributes,
        pages: None,
        producer: None,
        freshness: "versioned_source".into(),
        has_base_snapshot: find_child(root, "BaseForm").is_some(),
        origins: vec![origin.into()],
        incomplete_reasons: Vec::new(),
    })
}

/// Read the existing ordinary sidecar without substituting managed form content.
/// A fingerprinted sidecar is usable only against the actual source fingerprint.
pub fn parse_ordinary_form_inspection(
    json: &str,
    name: &str,
    fingerprint: Option<&str>,
    origin: &str,
) -> crate::Result<FormInspection> {
    let form = super::form_sidecar::parse_ordinary_form_sidecar(json, name)?;
    let source: serde_json::Value = serde_json::from_str(json.trim_start_matches('\u{feff}'))
        .map_err(|e| MetadataError::InvalidFormat(e.to_string()))?;
    inspect_ordinary_form(form, &source, fingerprint, origin)
}

/// Read a sidecar against the fingerprint and legacy freshness recorded by the source owner.
pub fn parse_prepared_ordinary_form_inspection(
    json: &str,
    name: &str,
    fingerprint: Option<&BinaryFingerprint>,
    origin: &str,
) -> Result<FormInspection, String> {
    let source: serde_json::Value =
        serde_json::from_str(json).map_err(|error| error.to_string())?;
    if source.pointer("/source/sha256").is_none()
        && fingerprint.and_then(|value| value.legacy_sidecar_stale) == Some(true)
    {
        return Err("ordinary_sidecar_stale".into());
    }
    let form = super::form_sidecar::parse_ordinary_form_sidecar(json, name)
        .map_err(|error| error.to_string())?;
    inspect_ordinary_form(form, &source, fingerprint.map(|value| value.sha256.as_str()), origin)
        .map_err(|error| error.to_string())
}

fn inspect_ordinary_form(
    form: Form,
    source: &serde_json::Value,
    fingerprint: Option<&str>,
    origin: &str,
) -> crate::Result<FormInspection> {
    validate_sidecar_fingerprint(source, fingerprint)?;
    let mut controls = BTreeMap::new();
    fn index<'a>(
        values: &'a [serde_json::Value],
        result: &mut BTreeMap<u32, &'a serde_json::Value>,
    ) -> crate::Result<()> {
        for value in values {
            if let Some(id) =
                value.get("id").and_then(|id| id.as_u64()).and_then(|id| u32::try_from(id).ok())
            {
                if result.insert(id, value).is_some() {
                    return Err(MetadataError::InvalidFormat(
                        "Duplicate ordinary control id".into(),
                    ));
                }
            }
            if let Some(children) = value.get("children").and_then(|c| c.as_array()) {
                index(children, result)?;
            }
        }
        Ok(())
    }
    if let Some(values) = source.get("controls").and_then(|v| v.as_array()) {
        index(values, &mut controls)?;
    }
    let mut bindings = Vec::new();
    fn handlers(
        value: &serde_json::Value,
        kind: &str,
        owner: Option<&str>,
        origin: &str,
        bindings: &mut Vec<FormBinding>,
    ) {
        if let Some(handlers) = value.get("handlers").and_then(|h| h.as_object()) {
            for (event, procedure) in handlers {
                if let Some(procedure) = procedure.as_str().map(str::trim).filter(|s| !s.is_empty())
                {
                    bindings.push(FormBinding {
                        owner_kind: kind.into(),
                        owner: owner.map(str::to_owned),
                        event: event.clone(),
                        procedure: procedure.to_string(),
                        call_type: None,
                        origin: origin.into(),
                    });
                }
            }
        }
    }
    if let Some(form) = source.get("form") {
        handlers(form, "form", None, origin, &mut bindings);
    }
    fn control_handlers(value: &serde_json::Value, origin: &str, bindings: &mut Vec<FormBinding>) {
        let name = value.get("name").and_then(|v| v.as_str()).filter(|s| !s.is_empty());
        handlers(value, if name.is_some() { "element" } else { "unknown" }, name, origin, bindings);
        if let Some(columns) = value.get("columns").and_then(|v| v.as_array()) {
            for column in columns {
                let column_name =
                    column.get("name").and_then(|v| v.as_str()).filter(|s| !s.is_empty());
                let owner = name.zip(column_name).map(|(name, column)| format!("{name}.{column}"));
                handlers(
                    column,
                    if owner.is_some() { "column" } else { "unknown" },
                    owner.as_deref(),
                    origin,
                    bindings,
                );
            }
        }
        if let Some(children) = value.get("children").and_then(|v| v.as_array()) {
            for child in children {
                control_handlers(child, origin, bindings);
            }
        }
    }
    if let Some(controls) = source.get("controls").and_then(|v| v.as_array()) {
        for control in controls {
            control_handlers(control, origin, &mut bindings);
        }
    }
    let elements = form
        .elements()
        .iter()
        .map(|element| {
            let control = controls.get(&element.id).copied();
            let text = |key: &str| {
                control.and_then(|v| v.get(key)).and_then(|v| v.as_str()).map(str::to_owned)
            };
            InspectedElement {
                name: element.name.clone(),
                id: element.id,
                kind: text("kind").unwrap_or_else(|| "unknown".into()),
                parent_id: element.parent_id,
                data_path: element.data_path.clone(),
                data_path_origin: element.data_path.as_ref().map(|_| origin.into()),
                page: text("page"),
                source_identity: text("sourceIdentity"),
                data_path_coverage: text("dataPathCoverage"),
                properties: ChoiceProperties::default(),
                origin: origin.into(),
            }
        })
        .collect();
    bindings.extend(
        form.event_handlers()
            .iter()
            .filter(|binding| binding.event_type == BOUND_HANDLER_EVENT)
            .map(|binding| FormBinding {
                owner_kind: "unknown".into(),
                owner: None,
                event: binding.event_type.clone(),
                procedure: binding.handler_name.clone(),
                call_type: None,
                origin: origin.into(),
            }),
    );
    let pages = source
        .pointer("/form/pages")
        .and_then(|v| v.as_array())
        .map(|pages| pages.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect());
    let mut incomplete_reasons = source
        .get("inspectionLimitations")
        .and_then(|v| v.as_array())
        .map(|v| v.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect::<Vec<_>>())
        .unwrap_or_default();
    if source.pointer("/form/handlers").is_none()
        && !incomplete_reasons.iter().any(|r| r == "event_bindings_unread")
    {
        incomplete_reasons.push("event_bindings_unread".into());
    }
    if pages.is_none() {
        incomplete_reasons.push("page_order_unread".into());
    }
    let attributes = source.get("attributes").and_then(|v| v.as_array()).map(|_| {
        form.attributes()
            .iter()
            .map(|a| InspectedAttribute {
                path: vec![a.name.clone()],
                attr_type: (a.attr_type != AttributeType::Unknown).then(|| a.attr_type.clone()),
                type_origin: (a.attr_type != AttributeType::Unknown).then(|| origin.into()),
                properties: ChoiceProperties::default(),
                origin: origin.into(),
            })
            .collect()
    });
    let freshness = if source.pointer("/source/sha256").is_some() {
        "sha256"
    } else {
        incomplete_reasons.push("legacy_freshness_mtime_only".into());
        "mtime_only"
    };
    Ok(FormInspection {
        form_type: "Ordinary".into(),
        elements,
        bindings,
        parameters: None,
        attributes,
        pages,
        producer: source.get("producer").cloned(),
        freshness: freshness.into(),
        has_base_snapshot: false,
        origins: vec![origin.into()],
        incomplete_reasons,
    })
}

pub(crate) fn validate_sidecar_fingerprint(
    source: &serde_json::Value,
    actual: Option<&str>,
) -> crate::Result<()> {
    if let Some(hash) = source.pointer("/source/sha256") {
        let expected = hash
            .as_str()
            .filter(|hash| hash.len() == 64 && hash.bytes().all(|c| c.is_ascii_hexdigit()))
            .ok_or_else(|| MetadataError::InvalidFormat("Invalid ordinary source SHA256".into()))?;
        match actual {
            Some(actual) if expected.eq_ignore_ascii_case(actual) => {}
            Some(_) => {
                return Err(MetadataError::InvalidFormat("Ordinary source SHA256 mismatch".into()));
            }
            None => {
                return Err(MetadataError::InvalidFormat(
                    "Ordinary source fingerprint unavailable".into(),
                ));
            }
        }
    }
    Ok(())
}

/// Choice/fill facts on metadata attributes, independently from form controls.
pub fn parse_metadata_choice_properties(
    xml: &str,
    origin: &str,
) -> crate::Result<Vec<InspectedAttribute>> {
    let document = roxmltree::Document::parse(xml.trim_start_matches('\u{feff}'))
        .map_err(|e| MetadataError::InvalidFormat(e.to_string()))?;
    let root = document.root_element();
    if root.tag_name().name() != "MetaDataObject"
        || root.tag_name().namespace() != Some("http://v8.1c.ru/8.3/MDClasses")
    {
        return Err(MetadataError::InvalidFormat("Expected metadata declaration".into()));
    }
    let mut result = Vec::new();
    fn visit(
        node: roxmltree::Node<'_, '_>,
        parent: &[String],
        origin: &str,
        result: &mut Vec<InspectedAttribute>,
    ) {
        if let Some(children) = find_child(node, "ChildObjects") {
            for child in children.children().filter(|n| n.is_element()) {
                let Some(props) = find_child(child, "Properties") else { continue };
                let Some(name) = child_text(props, "Name") else { continue };
                let mut path = parent.to_vec();
                path.extend([child.tag_name().name().to_string(), name.to_string()]);
                let properties = ChoiceProperties::from_node(props, origin);
                if matches!(
                    child.tag_name().name(),
                    "Attribute"
                        | "Dimension"
                        | "Resource"
                        | "AccountingFlag"
                        | "ExtDimensionAccountingFlag"
                ) {
                    result.push(InspectedAttribute {
                        path: path.clone(),
                        attr_type: find_child(props, "Type").and_then(|n| parse_type_xml(n).ok()),
                        type_origin: find_child(props, "Type").map(|_| origin.into()),
                        properties,
                        origin: origin.into(),
                    });
                }
                visit(child, &path, origin, result);
            }
        }
    }
    for object in root.children().filter(|n| n.is_element()) {
        visit(object, &[], origin, &mut result);
    }
    Ok(result)
}

fn optional_bool(node: roxmltree::Node<'_, '_>, tag: &str) -> Option<bool> {
    match child_text(node, tag) {
        Some("true" | "1") => Some(true),
        Some("false" | "0") => Some(false),
        _ => None,
    }
}

fn collect_attribute(
    node: roxmltree::Node<'_, '_>,
    parent: &[String],
    origin: &str,
    result: &mut Vec<InspectedAttribute>,
) {
    let Some(name) = node.attribute("name") else { return };
    let mut path = parent.to_vec();
    path.push(name.to_string());
    result.push(InspectedAttribute {
        path: path.clone(),
        attr_type: find_child(node, "Type").and_then(|n| parse_type_xml(n).ok()),
        type_origin: find_child(node, "Type").map(|_| origin.into()),
        properties: ChoiceProperties::from_node(node, origin),
        origin: origin.into(),
    });
    if let Some(columns) = find_child(node, "Columns") {
        for child in columns.children().filter(|n| n.is_element()) {
            if child.tag_name().name() == "AdditionalColumns" {
                let mut target = path.clone();
                target.push(child.attribute("table").unwrap_or_default().to_string());
                for column in child.children().filter(|n| n.has_tag_name("Column")) {
                    collect_attribute(column, &target, origin, result);
                }
            } else if child.tag_name().name() == "Column" {
                collect_attribute(child, &path, origin, result);
            }
        }
    }
}

fn collect_bindings(
    node: roxmltree::Node<'_, '_>,
    kind: &str,
    owner: Option<&str>,
    origin: &str,
    result: &mut Vec<FormBinding>,
) {
    let Some(events) = find_child(node, "Events") else { return };
    for event in events.children().filter(|n| n.has_tag_name("Event")) {
        result.push(FormBinding {
            owner_kind: kind.into(),
            owner: owner.map(str::to_owned),
            event: event.attribute("name").unwrap_or_default().into(),
            procedure: event.text().unwrap_or_default().trim().into(),
            call_type: event.attribute("callType").map(str::to_owned),
            origin: origin.into(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn managed_form_inspection_keeps_slots_parameters_and_attached_hierarchy() {
        let form = parse_managed_form_inspection(
            include_str!("../../fixtures/inspection/managed.xml"),
            "extension",
        )
        .unwrap();
        assert_eq!(
            form.elements.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            ["Rows", "RowsBar", "Actions", "Refresh", "Partner"]
        );
        assert_eq!(form.elements[1].parent_id, Some(1));
        assert_eq!(form.elements[3].parent_id, Some(3));
        assert_eq!(form.bindings.len(), 4);
        assert_eq!(form.bindings[0].call_type.as_deref(), Some("After"));
        assert_eq!(form.bindings[1].owner.as_deref(), Some("Partner"));
        assert!(form.bindings.iter().all(|b| b.procedure != "Historical"));
        let parameters = form.parameters.as_ref().unwrap();
        assert_eq!(
            parameters[0].attr_type,
            Some(AttributeType::Ref { mdo_type: crate::MdoType::Enum, name: "Kinds".into() })
        );
        assert_eq!(parameters[1].attr_type, None);
        assert_eq!(parameters[0].name, "Kind");
        assert_eq!(parameters[0].type_origin.as_deref(), Some("extension"));
        assert_eq!(parameters[0].key_parameter, Some(true));
        assert_eq!(parameters[1].type_origin, None);
        assert_eq!(parameters[1].key_parameter, None);
        assert_eq!(form.attributes.as_ref().unwrap()[0].path, ["Kind"]);
        assert_ne!(form.attributes.as_ref().unwrap()[0].attr_type, parameters[0].attr_type);
        let props = &form.elements[4].properties;
        assert_eq!(
            props.choice_parameters.as_ref().unwrap().children[0].attributes["name"],
            "Filter.Kind"
        );
        assert!(props.choice_parameter_links.as_ref().unwrap().children.is_empty());
        assert_eq!(props.fill_checking.as_deref(), Some("ShowError"));
        assert_eq!(form.elements[0].properties.choice_parameters, None);
    }

    #[test]
    fn managed_overlay_preserves_inherited_parameter_type_when_not_redeclared() {
        let mut base = parse_managed_form_inspection(
            include_str!("../../fixtures/inspection/managed-base.xml"),
            "base",
        )
        .unwrap();
        let overlay = parse_managed_form_inspection("<Form xmlns='http://v8.1c.ru/8.3/xcf/logform'><Parameters><Parameter name='Inherited'><KeyParameter>true</KeyParameter></Parameter></Parameters><Attributes/></Form>","extension").unwrap();
        base.apply_overlay(&overlay);
        let parameters = base.parameters.as_ref().unwrap();
        assert_eq!(parameters.len(), 2);
        assert_eq!(
            parameters[1].attr_type,
            Some(AttributeType::Ref { mdo_type: crate::MdoType::Catalog, name: "Partners".into() })
        );
        assert_eq!(parameters[1].origin, "extension");
        assert_eq!(parameters[1].type_origin.as_deref(), Some("base"));
        assert_eq!(parameters[1].key_parameter, Some(true));
        assert_eq!(base.attributes.as_ref().unwrap()[0].origin, "base");
    }

    #[test]
    fn managed_form_inspection_distinguishes_missing_and_empty_sections() {
        let missing = parse_managed_form_inspection(
            "<Form xmlns='http://v8.1c.ru/8.3/xcf/logform'/>",
            "base",
        )
        .unwrap();
        let empty = parse_managed_form_inspection(
            "<Form xmlns='http://v8.1c.ru/8.3/xcf/logform'><Parameters/><Attributes/></Form>",
            "base",
        )
        .unwrap();
        assert_eq!(missing.parameters, None);
        assert_eq!(empty.parameters, Some(vec![]));
        assert_eq!(missing.attributes, None);
        assert_eq!(empty.attributes, Some(vec![]));
        assert!(parse_managed_form_inspection("<Form/>", "foreign").is_err());
    }

    #[test]
    fn ordinary_inspection_preserves_known_flat_bindings_once() {
        let source = include_str!(
            "../../fixtures/designer/Catalogs/Справочник1/Forms/ФормаОбычнаяСОписанием/Ext/Form/Form.json"
        );
        let form = parse_ordinary_form_inspection(
            source,
            "ФормаОбычнаяСОписанием",
            None,
            "base:Catalogs/Справочник1/Forms/ФормаОбычнаяСОписанием",
        )
        .unwrap();
        let flat = form
            .bindings
            .iter()
            .filter(|b| b.procedure == "ДействияФормыВыполнить")
            .collect::<Vec<_>>();
        assert_eq!(flat.len(), 1);
        assert_eq!(flat[0].owner_kind, "unknown");
        assert_eq!(flat[0].owner, None);
        assert_eq!(flat[0].event, super::super::form_sidecar::BOUND_HANDLER_EVENT);
        assert_eq!(flat[0].call_type, None);
        assert!(form
            .bindings
            .iter()
            .all(|b| b.origin == "base:Catalogs/Справочник1/Forms/ФормаОбычнаяСОписанием"));
        let explicit =
            form.bindings.iter().filter(|b| b.procedure == "ПриОткрытии").collect::<Vec<_>>();
        assert_eq!(explicit.len(), 1);
        assert_eq!(explicit[0].owner_kind, "form");
        assert_eq!(explicit[0].event, "ПриОткрытии");
        assert!(form.bindings.iter().any(|b| b.procedure == "Поле1ПриИзменении"
            && b.owner.as_deref() == Some("Поле1")
            && b.event == "ПриИзменении"));

        let mut source: serde_json::Value = serde_json::from_str(source).unwrap();
        source["form"]["boundHandlers"].as_array_mut().unwrap().extend(
            ["", " ", "приоткрытии", "ДействияФормыВыполнить"].map(|s| serde_json::json!(s)),
        );
        let repeated = parse_ordinary_form_inspection(
            &source.to_string(),
            "ФормаОбычнаяСОписанием",
            None,
            "base:Catalogs/Справочник1/Forms/ФормаОбычнаяСОписанием",
        )
        .unwrap();
        assert_eq!(repeated.bindings, form.bindings);
    }

    #[test]
    fn ordinary_inspection_preserves_column_and_nameless_event_maps() {
        let mut source: serde_json::Value =
            serde_json::from_str(super::super::form_sidecar::tests::SIDECAR).unwrap();
        source["controls"][3]["handlers"] =
            serde_json::json!({"ПриОткрытии":" БезымянныйПриОткрытии ","Пустое":" "});
        source["form"]["boundHandlers"].as_array_mut().unwrap().extend(
            ["КодПриИзменении", "БезымянныйПриОткрытии"].map(|name| serde_json::json!(name)),
        );
        let form =
            parse_ordinary_form_inspection(&source.to_string(), "ФормаСписка", None, "base:Form")
                .unwrap();
        let columns =
            form.bindings.iter().filter(|b| b.procedure == "КодПриИзменении").collect::<Vec<_>>();
        let nameless = form
            .bindings
            .iter()
            .filter(|b| b.procedure == "БезымянныйПриОткрытии")
            .collect::<Vec<_>>();
        assert_eq!(
            (
                columns.len(),
                nameless.len(),
                form.bindings.iter().filter(|b| b.procedure.trim().is_empty()).count()
            ),
            (1, 1, 0)
        );
        assert_eq!(columns[0].owner_kind, "column");
        assert_eq!(columns[0].owner.as_deref(), Some("Список.Код"));
        assert_eq!(columns[0].event, "ПриИзменении");
        assert_eq!(nameless[0].owner_kind, "unknown");
        assert_eq!(nameless[0].owner, None);
        assert_eq!(nameless[0].event, "ПриОткрытии");
        assert!(form.bindings.iter().all(|b| b.call_type.is_none() && b.origin == "base:Form"));
        assert_eq!(form.elements.len(), 4);
        assert_eq!(
            form.bindings
                .iter()
                .filter(|b| b.procedure == "ДействияФормыПодбор" && b.event == BOUND_HANDLER_EVENT)
                .count(),
            1
        );
    }

    #[test]
    fn form_contract_unattributed_overlay_keeps_distinct_procedures_and_known_slots() {
        let mut base_source: serde_json::Value =
            serde_json::from_str(super::super::form_sidecar::tests::SIDECAR).unwrap();
        let mut overlay_source: serde_json::Value=serde_json::from_str(r#"{"schema":2,"form":{"handlers":{"ПередОткрытием":"РасширенныйПередОткрытием"},"boundHandlers":["действияформыподбор","РасширениеПодбор"]},"controls":[{"kind":"list","id":1,"name":"Список","handlers":{"ПриАктивизацииСтроки":"РасширенныйСписок"},"columns":[{"name":"Код","handlers":{"ПриИзменении":"РасширенныйКод"}}]}]}"#).unwrap();
        for (source, column_handler, table_handler, id) in [
            (&mut base_source, "БезИмениБаза", "БезТаблицыБаза", 20),
            (&mut overlay_source, "БезИмениРасширение", "БезТаблицыРасширение", 21),
        ] {
            source["controls"][0]["columns"]
                .as_array_mut()
                .unwrap()
                .push(serde_json::json!({"handlers":{"ПриИзменении":column_handler}}));
            source["controls"].as_array_mut().unwrap().push(serde_json::json!({"kind":"list","id":id,"columns":[{"name":"Код","handlers":{"ПриИзменении":table_handler}}]}));
        }
        let mut base =
            parse_ordinary_form_inspection(&base_source.to_string(), "ФормаСписка", None, "base")
                .unwrap();
        let overlay = parse_ordinary_form_inspection(
            &overlay_source.to_string(),
            "ФормаСписка",
            None,
            "extension",
        )
        .unwrap();
        base.apply_overlay(&overlay);
        let unknown_columns = [
            ("БезИмениБаза", "base"),
            ("БезИмениРасширение", "extension"),
            ("БезТаблицыБаза", "base"),
            ("БезТаблицыРасширение", "extension"),
        ];
        assert_eq!(
            unknown_columns.map(|(name, _)| base
                .bindings
                .iter()
                .filter(|b| b.procedure == name)
                .count()),
            [1, 1, 1, 1]
        );
        for (name, origin) in unknown_columns {
            let binding = base.bindings.iter().find(|b| b.procedure == name).unwrap();
            assert_eq!(binding.owner_kind, "unknown");
            assert_eq!(binding.owner, None);
            assert_eq!(binding.event, "ПриИзменении");
            assert_eq!(binding.origin, origin);
        }
        let flat = base
            .bindings
            .iter()
            .filter(|b| {
                b.owner_kind == "unknown" && b.owner.is_none() && b.event == BOUND_HANDLER_EVENT
            })
            .collect::<Vec<_>>();
        assert_eq!(flat.len(), 2);
        for name in ["ДействияФормыПодбор", "РасширениеПодбор"] {
            assert_eq!(
                flat.iter().filter(|b| stdx::case::eq_ignore_case(&b.procedure, name)).count(),
                1
            );
        }
        for (kind, owner, event, procedure) in [
            ("form", None, "ПередОткрытием", "РасширенныйПередОткрытием"),
            ("element", Some("Список"), "ПриАктивизацииСтроки", "РасширенныйСписок"),
            ("column", Some("Список.Код"), "ПриИзменении", "РасширенныйКод"),
        ] {
            let bindings = base
                .bindings
                .iter()
                .filter(|b| b.owner_kind == kind && b.owner.as_deref() == owner && b.event == event)
                .collect::<Vec<_>>();
            assert_eq!(bindings.len(), 1);
            assert_eq!(bindings[0].procedure, procedure);
            assert_eq!(bindings[0].origin, "extension");
        }
        assert!(base
            .bindings
            .iter()
            .any(|b| b.procedure == "ОбновитьОтображение" && b.origin == "base"));
        assert_eq!(base.origins, ["base", "extension"]);
    }

    #[test]
    fn ordinary_inspection_reads_its_own_pages_panels_bindings_and_hash() {
        let json = include_str!("../../fixtures/inspection/ordinary.json");
        let form =
            parse_ordinary_form_inspection(json, "Dialog", Some(&"a".repeat(64)), "ordinary")
                .unwrap();
        assert_eq!(form.form_type, "Ordinary");
        assert_eq!(form.pages, Some(vec!["Main".into(), "Details".into()]));
        assert_eq!(
            form.elements.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            ["DialogPanel", "Partner", "Accept"]
        );
        assert_eq!(form.elements[1].parent_id, Some(2));
        assert_eq!(form.elements[1].page.as_deref(), Some("Details"));
        assert_eq!(form.elements[1].data_path.as_deref(), Some("Document"));
        assert_eq!(form.elements[1].data_path_coverage.as_deref(), Some("reader_property"));
        assert_eq!(form.freshness, "sha256");
        assert!(form.incomplete_reasons.iter().any(|r| r == "nested_data_paths_unread"));
        assert!(parse_ordinary_form_inspection(json, "Dialog", Some(&"b".repeat(64)), "ordinary")
            .is_err());
        assert!(parse_ordinary_form_inspection(json, "Dialog", None, "ordinary").is_err());
    }

    #[test]
    fn metadata_choice_facts_distinguish_explicit_empty_and_missing() {
        let fields=parse_metadata_choice_properties("<MetaDataObject xmlns='http://v8.1c.ru/8.3/MDClasses'><Catalog><ChildObjects><Attribute><Properties><Name>Required</Name><ChoiceParameters/><FillChecking>ShowError</FillChecking></Properties></Attribute><Attribute><Properties><Name>Unread</Name></Properties></Attribute></ChildObjects></Catalog></MetaDataObject>","metadata").unwrap();
        assert_eq!(fields[0].properties.fill_checking.as_deref(), Some("ShowError"));
        assert!(fields[0].properties.choice_parameters.as_ref().unwrap().children.is_empty());
        assert_eq!(fields[1].properties.choice_parameters, None);
    }
    #[test]
    fn managed_overlay_does_not_turn_unknown_base_sections_into_confirmed_empty() {
        let mut base = parse_managed_form_inspection(
            "<Form xmlns='http://v8.1c.ru/8.3/xcf/logform'/>",
            "base",
        )
        .unwrap();
        let overlay = parse_managed_form_inspection(
            "<Form xmlns='http://v8.1c.ru/8.3/xcf/logform'><Parameters/><Attributes/></Form>",
            "extension",
        )
        .unwrap();
        base.apply_overlay(&overlay);
        assert!(base.incomplete_reasons.iter().any(|r| r == "inherited_parameters_unread"));
        assert!(base.incomplete_reasons.iter().any(|r| r == "inherited_attributes_unread"));
    }
}
