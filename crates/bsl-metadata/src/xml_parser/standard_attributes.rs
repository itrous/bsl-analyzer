use bsl_platform::{
    standard_attributes_for, AttrValueKind, MdoTemplateKind, ObjectView, PresenceCondition,
    StandardAttrSpec, StandardKind,
};

use crate::enums::ObjectBelonging;
use crate::metadata_object::{Attribute, AttributeType, MdoType};
use crate::register::RegisterAttribute;
use uuid::Uuid;

use super::helpers::{
    child_bool, child_text, child_u32, create_register_standard_attribute, find_child,
    parse_extension_ownership,
};

pub(crate) struct MdoProperties {
    pub name: String,
    pub code_length: Option<u32>,
    pub code_type: Option<String>,
    pub number_length: Option<u32>,
    pub number_type: Option<String>,
    pub description_length: Option<u32>,
    pub hierarchical: bool,
    pub owners: Vec<String>,
    pub periodicity: Option<String>,
    pub check_unique: bool,
    pub code_series: Option<String>,
    pub dependence_on_calculation_types: Option<String>,
    explicit_hierarchical: Option<bool>,
    hierarchical_explicitly_supplied: bool,
    code_length_explicitly_supplied: bool,
    description_length_explicitly_supplied: bool,
    number_length_explicitly_supplied: bool,
    owners_explicitly_supplied: bool,
    owners_explicitly_empty: bool,
    pub object_belonging: ObjectBelonging,
    pub extended_configuration_object: Option<Uuid>,
}

impl MdoProperties {
    pub(crate) fn from_node(props_node: roxmltree::Node<'_, '_>) -> Self {
        let name = child_text(props_node, "Name").unwrap_or("").to_string();
        let code_length_explicitly_supplied = find_child(props_node, "CodeLength").is_some();
        let code_length = child_u32(props_node, "CodeLength");
        let code_type = child_text(props_node, "CodeType").map(|s| s.to_string());
        let number_length_explicitly_supplied = find_child(props_node, "NumberLength").is_some();
        let number_length = child_u32(props_node, "NumberLength");
        let number_type = child_text(props_node, "NumberType").map(|s| s.to_string());
        let description_length_explicitly_supplied =
            find_child(props_node, "DescriptionLength").is_some();
        let description_length = child_u32(props_node, "DescriptionLength");
        let hierarchical_explicitly_supplied = find_child(props_node, "Hierarchical").is_some();
        let hierarchical_raw = child_text(props_node, "Hierarchical");
        let explicit_hierarchical = hierarchical_raw.and_then(|value| {
            if value.eq_ignore_ascii_case("true") {
                Some(true)
            } else if value.eq_ignore_ascii_case("false") {
                Some(false)
            } else {
                None
            }
        });
        let hierarchical = child_bool(props_node, "Hierarchical");
        let check_unique = child_bool(props_node, "CheckUnique");
        let periodicity =
            child_text(props_node, "InformationRegisterPeriodicity").map(|s| s.to_string());
        let code_series = child_text(props_node, "CodeSeries").map(|s| s.to_string());
        let dependence_on_calculation_types =
            child_text(props_node, "DependenceOnCalculationTypes").map(|s| s.to_string());
        let (object_belonging, extended_configuration_object) =
            parse_extension_ownership(props_node);

        let owners_node = find_child(props_node, "Owners");
        let owners_explicitly_supplied = owners_node.is_some();
        let owners_explicitly_empty = owners_node.is_some_and(|node| {
            node.children().all(|child| {
                !child.is_element() && child.text().map_or(true, |text| text.trim().is_empty())
            })
        });
        let owners = owners_node
            .map(|owners_node| {
                owners_node
                    .children()
                    .filter(|n| n.is_element() && n.tag_name().name() == "Item")
                    .filter_map(|n| n.text())
                    .map(|s| s.to_string())
                    .collect()
            })
            .unwrap_or_default();

        MdoProperties {
            name,
            code_length,
            code_type,
            number_length,
            number_type,
            description_length,
            hierarchical,
            owners,
            periodicity,
            check_unique,
            code_series,
            dependence_on_calculation_types,
            explicit_hierarchical,
            hierarchical_explicitly_supplied,
            code_length_explicitly_supplied,
            description_length_explicitly_supplied,
            number_length_explicitly_supplied,
            owners_explicitly_supplied,
            owners_explicitly_empty,
            object_belonging,
            extended_configuration_object,
        }
    }

    /// A chart of calculation types depends on calculation types — and so carries the
    /// displacing/leading/base standard tabular sections and the `ПериодДействияБазовый`
    /// standard attribute — unless its dependency is `DontUse` (НеЗависит).
    pub(crate) fn depends_on_calculation_types(&self) -> bool {
        self.dependence_on_calculation_types.as_deref().is_some_and(|dep| dep != "DontUse")
    }

    pub(crate) fn unknown_conditional_attributes(&self, template: MdoTemplateKind) -> Vec<String> {
        standard_attributes_for(template, ObjectView::Object)
            .iter()
            .filter_map(|spec| {
                let malformed = match spec.condition {
                    PresenceCondition::Always => false,
                    PresenceCondition::HasCode => {
                        self.code_length_explicitly_supplied && self.code_length.is_none()
                    }
                    PresenceCondition::HasDescription => {
                        self.description_length_explicitly_supplied
                            && self.description_length.is_none()
                    }
                    PresenceCondition::HasNumber => {
                        self.number_length_explicitly_supplied && self.number_length.is_none()
                    }
                    PresenceCondition::Hierarchical => {
                        self.hierarchical_explicitly_supplied
                            && self.explicit_hierarchical.is_none()
                    }
                    PresenceCondition::HasOwners => {
                        self.owners_explicitly_supplied
                            && !self.owners_explicitly_empty
                            && self.owners.is_empty()
                    }
                    PresenceCondition::IsPeriodic
                    | PresenceCondition::DependsOnCalculationTypes => false,
                };
                malformed.then(|| spec.kind.russian_name().to_string())
            })
            .collect()
    }

    pub(crate) fn known_absent_conditional_attributes(
        &self,
        template: MdoTemplateKind,
    ) -> Vec<String> {
        standard_attributes_for(template, ObjectView::Object)
            .iter()
            .filter_map(|spec| {
                let refuted = match spec.condition {
                    PresenceCondition::Always => false,
                    PresenceCondition::HasCode => self.code_length == Some(0),
                    PresenceCondition::HasDescription => self.description_length == Some(0),
                    PresenceCondition::HasNumber => self.number_length == Some(0),
                    PresenceCondition::Hierarchical => self.explicit_hierarchical == Some(false),
                    PresenceCondition::HasOwners => self.owners_explicitly_empty,
                    PresenceCondition::IsPeriodic => {
                        self.periodicity.as_deref() == Some("Nonperiodical")
                    }
                    PresenceCondition::DependsOnCalculationTypes => {
                        self.dependence_on_calculation_types.as_deref() == Some("DontUse")
                    }
                };
                refuted.then(|| spec.kind.russian_name().to_string())
            })
            .collect()
    }
}

fn condition_satisfied(cond: PresenceCondition, p: &MdoProperties) -> bool {
    match cond {
        PresenceCondition::Always => true,
        PresenceCondition::HasCode => p.code_length.is_some_and(|l| l > 0),
        PresenceCondition::HasDescription => p.description_length.is_some_and(|l| l > 0),
        PresenceCondition::HasNumber => p.number_length.is_some_and(|l| l > 0),
        PresenceCondition::Hierarchical => p.hierarchical,
        PresenceCondition::HasOwners => !p.owners.is_empty(),
        PresenceCondition::IsPeriodic => {
            p.periodicity.as_deref().unwrap_or("Nonperiodical") != "Nonperiodical"
        }
        PresenceCondition::DependsOnCalculationTypes => p.depends_on_calculation_types(),
    }
}

/// `CodeType` / `NumberType` carry the localized-agnostic platform token `Number`
/// (the alternative being `String`, the default).
fn is_numeric_kind(kind: Option<&str>) -> bool {
    kind == Some("Number")
}

/// An integer code/number has no fractional part; its digit count is the precision.
fn numeric_attr_type(length: Option<u32>) -> AttributeType {
    let precision = length.unwrap_or(0).min(u8::MAX as u32) as u8;
    AttributeType::Number { precision, scale: 0 }
}

fn build_attr_type(spec: &StandardAttrSpec, p: &MdoProperties, mdo_type: MdoType) -> AttributeType {
    match spec.value {
        AttrValueKind::Boolean => AttributeType::Boolean,
        AttrValueKind::DateTime => AttributeType::DateTime,
        AttrValueKind::StringCodeOrDescription => {
            let length = match spec.kind {
                StandardKind::Code => p.code_length,
                StandardKind::Description => p.description_length,
                _ => None,
            };
            // A catalog's `Код` is `Строка` by default, but `CodeType = Number` makes it a
            // `Число` — `ЧислоПрописью(Объект.Код, …)` is well-typed only in that case.
            if spec.kind == StandardKind::Code && is_numeric_kind(p.code_type.as_deref()) {
                numeric_attr_type(length)
            } else {
                AttributeType::String { length }
            }
        }
        // A document's `Номер` follows `NumberType` the same way `Код` follows `CodeType`.
        AttrValueKind::StringNumber => {
            if is_numeric_kind(p.number_type.as_deref()) {
                numeric_attr_type(p.number_length)
            } else {
                AttributeType::String { length: p.number_length }
            }
        }
        AttrValueKind::StringUnbounded => AttributeType::String { length: None },
        AttrValueKind::NumberLineNumber => AttributeType::Number { precision: 10, scale: 0 },
        AttrValueKind::SelfRef => AttributeType::Ref { mdo_type, name: p.name.clone() },
        AttrValueKind::OwnerRef => owner_attr_type(&p.owners),
        AttrValueKind::AnyDocumentRef => {
            AttributeType::AnyObjectRef { mdo_type: MdoType::Document }
        }
        AttrValueKind::TypeDescription => AttributeType::PlatformNamed("ОписаниеТипов".to_string()),
        AttrValueKind::Unknown => AttributeType::Unknown,
    }
}

fn build_attribute(spec: &StandardAttrSpec, p: &MdoProperties, mdo_type: MdoType) -> Attribute {
    Attribute {
        name: spec.kind.russian_name().to_string(),
        name_en: Some(spec.kind.english_name().to_string()),
        attr_type: build_attr_type(spec, p, mdo_type),
    }
}

pub(crate) fn add_standard_attributes_from_spec(
    attributes: &mut Vec<Attribute>,
    properties: &MdoProperties,
    mdo_type: MdoType,
    template: MdoTemplateKind,
    view: ObjectView,
) {
    for spec in standard_attributes_for(template, view) {
        if !condition_satisfied(spec.condition, properties) {
            continue;
        }
        attributes.push(build_attribute(spec, properties, mdo_type));
    }
}

pub(crate) fn add_register_common_attrs(
    attributes: &mut Vec<RegisterAttribute>,
    object_name: &str,
    mdo_type: MdoType,
) {
    use crate::metadata_object::StandardAttributeKind;
    attributes.push(create_register_standard_attribute(
        &StandardAttributeKind::Active,
        mdo_type,
        object_name,
    ));
    attributes.push(create_register_standard_attribute(
        &StandardAttributeKind::LineNumber,
        mdo_type,
        object_name,
    ));
    attributes.push(create_register_standard_attribute(
        &StandardAttributeKind::Recorder,
        mdo_type,
        object_name,
    ));
}

pub(crate) fn add_register_period_attr(
    attributes: &mut Vec<RegisterAttribute>,
    object_name: &str,
    mdo_type: MdoType,
    periodicity: Option<&str>,
) {
    use crate::metadata_object::StandardAttributeKind;
    let should_add_period = match mdo_type {
        MdoType::InformationRegister => periodicity.unwrap_or("Nonperiodical") != "Nonperiodical",
        MdoType::AccumulationRegister => true,
        MdoType::AccountingRegister | MdoType::CalculationRegister => false,
        _ => false,
    };

    if should_add_period {
        attributes.push(create_register_standard_attribute(
            &StandardAttributeKind::Period,
            mdo_type,
            object_name,
        ));
    }
}

pub(crate) fn add_information_register_standard_attrs(
    attributes: &mut Vec<RegisterAttribute>,
    object_name: &str,
    periodicity: Option<&str>,
) {
    add_register_common_attrs(attributes, object_name, MdoType::InformationRegister);
    add_register_period_attr(attributes, object_name, MdoType::InformationRegister, periodicity);
}

pub(crate) fn add_accumulation_register_standard_attrs(
    attributes: &mut Vec<RegisterAttribute>,
    object_name: &str,
) {
    add_register_common_attrs(attributes, object_name, MdoType::AccumulationRegister);
    add_register_period_attr(attributes, object_name, MdoType::AccumulationRegister, None);
}

fn owner_attr_type(owners: &[String]) -> AttributeType {
    let types = parse_owner_types(owners);
    match types.len() {
        0 => AttributeType::Unknown,
        1 => types.into_iter().next().unwrap(),
        _ => AttributeType::Composite { types },
    }
}

fn parse_owner_types(items: &[String]) -> Vec<AttributeType> {
    items
        .iter()
        .filter_map(|item| {
            let parts: Vec<&str> = item.split('.').collect();
            if parts.len() != 2 {
                tracing::warn!(value = %item, "Invalid owner format, expected Type.Name");
                return None;
            }

            let type_str = parts[0];
            let name = parts[1].to_string();

            match type_str.parse::<MdoType>() {
                Ok(mdo_type) => Some(AttributeType::Ref { mdo_type, name }),
                Err(e) => {
                    tracing::warn!(type_str = %type_str, error = %e, "Unknown MDO type in owner");
                    None
                }
            }
        })
        .collect()
}
