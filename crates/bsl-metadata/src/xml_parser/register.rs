use crate::dimension::Dimension;
use crate::error::{MetadataError, Result};
use crate::metadata_object::MdoType;
use crate::register::{
    AccumulationRegisterType, Register, RegisterAttribute, RegisterPeriodicity, RegisterResource,
};

use super::helpers::{
    child_bool, child_text, find_child, find_mdo_element, parse_extension_ownership, parse_uuid,
    parse_xml,
};
use super::standard_attributes::{
    add_accumulation_register_standard_attrs, add_information_register_standard_attrs,
};
use super::type_parser::parse_type_xml;

pub fn parse_information_register_xml(xml: &str) -> Result<Register> {
    parse_register_xml(xml, MdoType::InformationRegister)
}

pub fn parse_accumulation_register_xml(xml: &str) -> Result<Register> {
    parse_register_xml(xml, MdoType::AccumulationRegister)
}

pub fn parse_accounting_register_xml(xml: &str) -> Result<Register> {
    parse_register_xml(xml, MdoType::AccountingRegister)
}

pub fn parse_calculation_register_xml(xml: &str) -> Result<Register> {
    parse_register_xml(xml, MdoType::CalculationRegister)
}

fn parse_register_xml(xml: &str, mdo_type: MdoType) -> Result<Register> {
    let _span = tracing::debug_span!("parse_register_xml", ?mdo_type).entered();

    let doc = parse_xml(xml)?;

    let mdo = find_mdo_element(&doc)
        .ok_or_else(|| MetadataError::InvalidFormat("No register element found".to_string()))?;

    let uuid_str = mdo.attribute("uuid").unwrap_or("");
    let uuid = parse_uuid(uuid_str, "register")?;

    let props = find_child(mdo, "Properties")
        .ok_or_else(|| MetadataError::InvalidFormat("Register missing Properties".to_string()))?;

    let object_name = child_text(props, "Name").unwrap_or("").to_string();
    let (belonging, target) = parse_extension_ownership(props);
    let periodicity_str =
        child_text(props, "InformationRegisterPeriodicity").map(|s| s.to_string());
    let recorder_subordinate = mdo_type == MdoType::InformationRegister
        && child_text(props, "WriteMode") == Some("RecorderSubordinate");
    let enable_totals_slice_first = child_bool(props, "EnableTotalsSliceFirst");
    let enable_totals_slice_last = child_bool(props, "EnableTotalsSliceLast");
    let register_type_str = child_text(props, "RegisterType").map(|s| s.to_string());
    let (correspondence, chart_of_accounts) = if mdo_type == MdoType::AccountingRegister {
        (
            optional_bool(props, "Correspondence"),
            child_text(props, "ChartOfAccounts").and_then(|raw| {
                let (_, name) = raw.trim().split_once('.')?;
                let name = name.trim();
                (!name.is_empty()).then(|| name.to_string())
            }),
        )
    } else {
        (None, None)
    };

    let mut attributes = Vec::new();
    match mdo_type {
        MdoType::InformationRegister => {
            add_information_register_standard_attrs(
                &mut attributes,
                &object_name,
                periodicity_str.as_deref(),
                recorder_subordinate,
            );
        }
        MdoType::AccumulationRegister => {
            add_accumulation_register_standard_attrs(&mut attributes, &object_name);
        }
        _ => {}
    }

    let mut dimensions = Vec::new();
    let mut resources = Vec::new();

    if let Some(child_objects) = find_child(mdo, "ChildObjects") {
        for child in child_objects.children().filter(|n| n.is_element()) {
            match child.tag_name().name() {
                "Dimension" => {
                    dimensions.push(parse_dimension_node(child)?);
                }
                "Resource" => {
                    resources.push(parse_resource_node(child)?);
                }
                "Attribute" => {
                    parse_register_attr_node(child, &mut attributes)?;
                }
                _ => {}
            }
        }
    }

    let periodicity = parse_periodicity(periodicity_str.as_deref(), mdo_type);
    let register_type = parse_register_type(register_type_str.as_deref(), mdo_type);

    let mut register = Register::builder()
        .uuid(uuid)
        .extension_ownership(belonging, target)
        .name(object_name)
        .mdo_type(mdo_type)
        .dimensions(dimensions)
        .resources(resources)
        .attributes(attributes)
        .periodicity(periodicity)
        .recorder_subordinate(recorder_subordinate)
        .register_type(register_type)
        .enable_totals_slice_first(enable_totals_slice_first)
        .enable_totals_slice_last(enable_totals_slice_last)
        .correspondence(correspondence)
        .chart_of_accounts(chart_of_accounts)
        .build();
    register.set_read_properties(super::read_properties::parse_read_properties(mdo));

    tracing::debug!(
        register_name = %register.name(),
        uuid = %register.uuid(),
        mdo_type = ?register.mdo_type(),
        dimensions = register.dimensions().len(),
        resources = register.resources().len(),
        attributes = register.attributes().len(),
        "parsed register"
    );

    Ok(register)
}

fn parse_dimension_node(node: roxmltree::Node<'_, '_>) -> Result<Dimension> {
    let uuid_str = node.attribute("uuid").unwrap_or("");
    let dim_uuid = parse_uuid(uuid_str, "dimension")?;

    let props = find_child(node, "Properties")
        .ok_or_else(|| MetadataError::InvalidFormat("Dimension missing Properties".to_string()))?;

    let name = child_text(props, "Name").unwrap_or("").to_string();

    let _span = tracing::debug_span!("parse_dimension", dim_name = %name).entered();

    let deny_incomplete_values = child_bool(props, "DenyIncompleteValues");
    let master = child_bool(props, "Master");
    let indexing = child_text(props, "Indexing").unwrap_or("").to_string();

    let mut dimension = Dimension::builder()
        .uuid(dim_uuid)
        .name(name)
        .deny_incomplete_values(deny_incomplete_values)
        .master(master)
        .indexing(indexing)
        .balance(optional_bool(props, "Balance"))
        .build();

    if let Some(type_node) = find_child(props, "Type") {
        let dim_type = parse_type_xml(type_node)?;
        dimension.set_type_str(format!("{}", dim_type));
        dimension.set_attr_type(dim_type);
    }

    Ok(dimension)
}

fn parse_resource_node(node: roxmltree::Node<'_, '_>) -> Result<RegisterResource> {
    let uuid_str = node.attribute("uuid").unwrap_or("");
    let resource_uuid = parse_uuid(uuid_str, "resource")?;

    let props = find_child(node, "Properties")
        .ok_or_else(|| MetadataError::InvalidFormat("Resource missing Properties".to_string()))?;

    let name = child_text(props, "Name").unwrap_or("").to_string();

    let _span = tracing::debug_span!("parse_resource", resource_name = %name).entered();

    let mut resource = RegisterResource::new(resource_uuid, name);
    resource.set_balance(optional_bool(props, "Balance"));

    let type_node = find_child(props, "Type")
        .ok_or_else(|| MetadataError::InvalidFormat("Resource missing Type".to_string()))?;
    let resource_type = parse_type_xml(type_node)?;
    resource.set_type_str(format!("{}", resource_type));
    resource.set_attr_type(resource_type);

    Ok(resource)
}

fn parse_register_attr_node(
    node: roxmltree::Node<'_, '_>,
    attributes: &mut Vec<RegisterAttribute>,
) -> Result<()> {
    let uuid_str = node.attribute("uuid").unwrap_or("");
    let attr_uuid = parse_uuid(uuid_str, "attribute")?;

    let props = find_child(node, "Properties").ok_or_else(|| {
        MetadataError::InvalidFormat("RegisterAttribute missing Properties".to_string())
    })?;

    let name = child_text(props, "Name").unwrap_or("").to_string();

    let _span = tracing::debug_span!("parse_register_attr", attr_name = %name).entered();

    let mut attribute = RegisterAttribute::new(attr_uuid, name);

    let type_node = find_child(props, "Type").ok_or_else(|| {
        MetadataError::InvalidFormat("RegisterAttribute missing Type".to_string())
    })?;
    let attr_type = parse_type_xml(type_node)?;
    attribute.set_type_str(format!("{}", attr_type));
    attribute.set_attr_type(attr_type);

    attributes.push(attribute);
    Ok(())
}

/// A boolean property that only some register kinds carry: absent stays `None`, so a consumer
/// can tell "the XML says no" from "this kind has no such property".
fn optional_bool(props: roxmltree::Node<'_, '_>, tag: &str) -> Option<bool> {
    child_text(props, tag).map(|text| text.trim().eq_ignore_ascii_case("true"))
}

fn parse_periodicity(periodicity: Option<&str>, mdo_type: MdoType) -> Option<RegisterPeriodicity> {
    if mdo_type != MdoType::InformationRegister {
        return None;
    }

    periodicity.and_then(|p| match p {
        "Nonperiodical" => Some(RegisterPeriodicity::Nonperiodical),
        "Second" => Some(RegisterPeriodicity::Second),
        "Day" => Some(RegisterPeriodicity::Day),
        "Month" => Some(RegisterPeriodicity::Month),
        "RecorderPosition" => Some(RegisterPeriodicity::RecorderPosition),
        _ => None,
    })
}

fn parse_register_type(
    register_type: Option<&str>,
    mdo_type: MdoType,
) -> Option<AccumulationRegisterType> {
    if mdo_type != MdoType::AccumulationRegister {
        return None;
    }

    register_type.and_then(|rt| match rt {
        "Balance" => Some(AccumulationRegisterType::Balance),
        "Turnovers" => Some(AccumulationRegisterType::Turnovers),
        "BalanceAndTurnovers" => Some(AccumulationRegisterType::BalanceAndTurnovers),
        _ => None,
    })
}
