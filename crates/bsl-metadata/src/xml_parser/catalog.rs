use crate::enums::CodeSeries;
use crate::error::{MetadataError, Result};
use crate::metadata_object::{Attribute, AttributeType, MdoType, MetadataObject};
use crate::tabular_section::{TabularSection, TabularSectionAttribute};
use std::str::FromStr;

use super::helpers::{child_text, child_u32, find_child, find_mdo_element, parse_uuid, parse_xml};
use super::standard_attributes::{add_standard_attributes_from_spec, MdoProperties};
use super::type_parser::parse_type_xml;
use crate::metadata_object::object_standard_template;
use bsl_platform::ObjectView;

pub fn parse_catalog_xml(xml: &str) -> Result<MetadataObject> {
    let _span = tracing::debug_span!("parse_catalog_xml").entered();
    parse_metadata_object_xml(xml, MdoType::Catalog)
}

pub fn parse_document_xml(xml: &str) -> Result<MetadataObject> {
    let _span = tracing::debug_span!("parse_document_xml").entered();
    parse_metadata_object_xml(xml, MdoType::Document)
}

pub fn parse_business_process_xml(xml: &str) -> Result<MetadataObject> {
    let _span = tracing::debug_span!("parse_business_process_xml").entered();
    parse_metadata_object_xml(xml, MdoType::BusinessProcess)
}

pub fn parse_chart_of_characteristic_types_xml(xml: &str) -> Result<MetadataObject> {
    let _span = tracing::debug_span!("parse_chart_of_characteristic_types_xml").entered();
    parse_metadata_object_xml(xml, MdoType::ChartOfCharacteristicTypes)
}

pub fn parse_task_xml(xml: &str) -> Result<MetadataObject> {
    let _span = tracing::debug_span!("parse_task_xml").entered();
    parse_metadata_object_xml(xml, MdoType::Task)
}

pub fn parse_exchange_plan_xml(xml: &str) -> Result<MetadataObject> {
    let _span = tracing::debug_span!("parse_exchange_plan_xml").entered();
    parse_metadata_object_xml(xml, MdoType::ExchangePlan)
}

pub fn parse_chart_of_accounts_xml(xml: &str) -> Result<MetadataObject> {
    let _span = tracing::debug_span!("parse_chart_of_accounts_xml").entered();
    parse_metadata_object_xml(xml, MdoType::ChartOfAccounts)
}

pub fn parse_chart_of_calculation_types_xml(xml: &str) -> Result<MetadataObject> {
    let _span = tracing::debug_span!("parse_chart_of_calculation_types_xml").entered();
    parse_metadata_object_xml(xml, MdoType::ChartOfCalculationTypes)
}

pub fn parse_data_processor_xml(xml: &str) -> Result<MetadataObject> {
    let _span = tracing::debug_span!("parse_data_processor_xml").entered();
    parse_metadata_object_xml(xml, MdoType::DataProcessor)
}

pub fn parse_report_xml(xml: &str) -> Result<MetadataObject> {
    let _span = tracing::debug_span!("parse_report_xml").entered();
    parse_metadata_object_xml(xml, MdoType::Report)
}

pub fn parse_external_data_processor_xml(xml: &str) -> Result<MetadataObject> {
    let _span = tracing::debug_span!("parse_external_data_processor_xml").entered();
    parse_metadata_object_xml(xml, MdoType::ExternalDataProcessor)
}

pub fn parse_external_report_xml(xml: &str) -> Result<MetadataObject> {
    let _span = tracing::debug_span!("parse_external_report_xml").entered();
    parse_metadata_object_xml(xml, MdoType::ExternalReport)
}

fn parse_metadata_object_xml(xml: &str, mdo_type: MdoType) -> Result<MetadataObject> {
    let doc = parse_xml(xml)?;

    let mdo_node = find_mdo_element(&doc)
        .ok_or_else(|| MetadataError::InvalidFormat("No MDO element found".to_string()))?;

    // Inside a configuration dump the collection directory vouches for the kind,
    // so the element is not checked. An export root has no such directory: the
    // element is the only thing that says what the export is, and reading an
    // internal object's export — or the other external kind — as this kind
    // would mint an object of a type the file never declared.
    if let Some(kind) = crate::external_object::ExternalObjectKind::of_mdo_type(mdo_type) {
        let element = mdo_node.tag_name().name();
        if element != kind.element_name() {
            return Err(MetadataError::InvalidFormat(format!(
                "expected <{}>, found <{element}>",
                kind.element_name()
            )));
        }
    }

    let props_node = find_child(mdo_node, "Properties")
        .ok_or_else(|| MetadataError::InvalidFormat("No Properties element found".to_string()))?;

    let properties = MdoProperties::from_node(props_node);

    let mut attributes = Vec::new();
    let mut tabular_sections = Vec::new();

    if let Some(template) = object_standard_template(mdo_type) {
        add_standard_attributes_from_spec(
            &mut attributes,
            &properties,
            mdo_type,
            template,
            ObjectView::Object,
        );
    }
    if mdo_type == MdoType::ChartOfAccounts {
        // `Вид` and `Забалансовый` are standard chart-of-accounts fields, but they are
        // deliberately NOT StandardAttributeKind members: `is_standard_attribute_name`
        // is a name-only global set, and «Вид» is a common user attribute name on other
        // objects — listing it there would hide those user attributes from every
        // standard-name filter (graph catalog, SDBL scope builder).
        attributes.push(Attribute {
            name: "Вид".to_string(),
            name_en: Some("Type".to_string()),
            attr_type: AttributeType::PlatformNamed("ВидСчета".to_string()),
        });
        attributes.push(Attribute {
            name: "Забалансовый".to_string(),
            name_en: Some("OffBalance".to_string()),
            attr_type: AttributeType::Boolean,
        });
    }

    let mut ext_dimension_flags = Vec::new();
    if let Some(child_objects) = find_child(mdo_node, "ChildObjects") {
        for child in child_objects.children().filter(|n| n.is_element()) {
            match child.tag_name().name() {
                // `AddressingAttribute` is a Task's addressing requisite
                // (Исполнитель, РольИсполнителя, …) — same `Properties>Name>Type`
                // shape as a regular attribute and a real member of the task
                // object, so resolve it like one. `AccountingFlag` is a
                // chart-of-accounts accounting flag — a Boolean column of the
                // account itself, same node shape.
                "Attribute"
                | "Resource"
                | "Dimension"
                | "AddressingAttribute"
                | "AccountingFlag" => {
                    attributes.push(parse_attribute_node(child)?);
                }
                // Ext-dimension accounting flags are columns of the implicit
                // ВидыСубконто tabular section, not of the account row.
                "ExtDimensionAccountingFlag" => {
                    ext_dimension_flags.push(parse_attribute_node(child)?);
                }
                "TabularSection" => {
                    tabular_sections.push(parse_tabular_section_node(child)?);
                }
                _ => {}
            }
        }
    }

    if mdo_type == MdoType::ChartOfAccounts {
        tabular_sections.push(build_ext_dimension_types_section(props_node, ext_dimension_flags));
    }

    if mdo_type == MdoType::ChartOfCalculationTypes && properties.depends_on_calculation_types() {
        tabular_sections.extend(build_calculation_type_dependency_sections(&properties.name));
    }

    let mut mdo = MetadataObject::new(mdo_type, properties.name.clone());
    if let Some(template) = object_standard_template(mdo_type) {
        mdo.known_absent_standard_attributes =
            properties.known_absent_conditional_attributes(template);
        mdo.unknown_conditional_standard_attributes =
            properties.unknown_conditional_attributes(template);
    }
    if let Some(uuid_str) = mdo_node.attribute("uuid") {
        match parse_uuid(uuid_str, "MDO root") {
            Ok(uuid) => mdo.set_uuid(uuid),
            Err(err) => tracing::warn!(
                ?mdo_type,
                name = %mdo.name,
                uuid_raw = %uuid_str,
                %err,
                "ignored malformed MDO root UUID"
            ),
        }
    }
    mdo.set_object_belonging(properties.object_belonging);
    if let Some(uuid) = properties.extended_configuration_object {
        mdo.set_extends_uuid(uuid);
    }
    if mdo_type == MdoType::Document {
        mdo.set_register_records(parse_register_records(props_node));
    }
    if mdo_type == MdoType::ChartOfAccounts {
        mdo.set_max_ext_dimension_count(child_u32(props_node, "MaxExtDimensionCount"));
    }
    for attr in attributes {
        mdo.add_attribute(attr);
    }
    for ts in tabular_sections {
        mdo.add_tabular_section(ts);
    }

    mdo.set_check_unique(properties.check_unique);
    if let Some(code_series_str) = &properties.code_series {
        mdo.set_code_series(parse_code_series(code_series_str));
    }

    tracing::debug!(
        mdo_name = %mdo.name,
        mdo_type = ?mdo.mdo_type,
        attributes = mdo.attributes.len(),
        tabular_sections = mdo.tabular_sections.len(),
        check_unique = mdo.check_unique,
        code_series = ?mdo.code_series,
        "parsed metadata object"
    );

    Ok(mdo)
}

/// The implicit `ВидыСубконто` tabular section every chart of accounts carries.
/// Its row holds the ext-dimension kind (typed by the `ExtDimensionTypes`
/// chart-of-characteristic-types property), the predefined/turnovers-only flags
/// and one column per ext-dimension accounting flag declared in the XML;
/// `НомерСтроки`/`Ссылка` come from the generic tabular-section machinery.
fn build_ext_dimension_types_section(
    props_node: roxmltree::Node<'_, '_>,
    ext_dimension_flags: Vec<Attribute>,
) -> TabularSection {
    let ext_dim_kind_type = child_text(props_node, "ExtDimensionTypes")
        .and_then(|raw| {
            let (prefix, name) = raw.split_once('.')?;
            let mdo_type = MdoType::from_str(prefix).ok()?;
            Some(AttributeType::Ref { mdo_type, name: name.to_string() })
        })
        .unwrap_or(AttributeType::Unknown);

    let nil_uuid = uuid::Uuid::nil();
    let mut section = TabularSection::new(nil_uuid, "ВидыСубконто");
    section.set_name_en(Some("ExtDimensionTypes".to_string()));

    let mut columns = Vec::new();
    for (name, name_en, attr_type) in [
        ("ВидСубконто", "ExtDimensionType", ext_dim_kind_type),
        ("Предопределенное", "Predefined", AttributeType::Boolean),
        ("ТолькоОбороты", "TurnoversOnly", AttributeType::Boolean),
    ] {
        let mut column = TabularSectionAttribute::new(nil_uuid, name, attr_type);
        column.set_name_en(Some(name_en.to_string()));
        columns.push(column);
    }
    for flag in ext_dimension_flags {
        let mut column = TabularSectionAttribute::new(nil_uuid, flag.name, flag.attr_type);
        column.set_name_en(flag.name_en);
        columns.push(column);
    }
    section.set_attributes(columns);
    section
}

/// The three standard tabular sections every dependent chart of calculation types
/// carries: `ВытесняющиеВидыРасчета`, `ВедущиеВидыРасчета`, `БазовыеВидыРасчета`.
/// Each row points at another calculation type of the same chart through its
/// `ВидРасчета` column (self-ref); `НомерСтроки`/`Ссылка` come from the generic
/// tabular-section machinery. The platform omits all three when the chart does not
/// depend on calculation types, which the caller already gated on.
fn build_calculation_type_dependency_sections(chart_name: &str) -> Vec<TabularSection> {
    let nil_uuid = uuid::Uuid::nil();
    [
        ("ВытесняющиеВидыРасчета", "DisplacingCalculationTypes"),
        ("ВедущиеВидыРасчета", "LeadingCalculationTypes"),
        ("БазовыеВидыРасчета", "BaseCalculationTypes"),
    ]
    .into_iter()
    .map(|(name, name_en)| {
        let mut section = TabularSection::new(nil_uuid, name);
        section.set_name_en(Some(name_en.to_string()));

        let mut column = TabularSectionAttribute::new(
            nil_uuid,
            "ВидРасчета",
            AttributeType::Ref {
                mdo_type: MdoType::ChartOfCalculationTypes,
                name: chart_name.to_string(),
            },
        );
        column.set_name_en(Some("CalculationType".to_string()));
        section.set_attributes(vec![column]);
        section
    })
    .collect()
}

fn parse_register_records(props_node: roxmltree::Node<'_, '_>) -> Vec<(MdoType, String)> {
    let Some(records_node) = find_child(props_node, "RegisterRecords") else {
        return Vec::new();
    };

    records_node
        .children()
        .filter(|n| n.is_element() && n.tag_name().name() == "Item")
        .filter_map(|item| {
            let raw = item.text()?.trim();
            let (prefix, name) = raw.split_once('.')?;
            if name.is_empty() {
                return None;
            }
            let mdo_type = MdoType::from_str(prefix).ok()?;
            match mdo_type {
                MdoType::InformationRegister
                | MdoType::AccumulationRegister
                | MdoType::AccountingRegister
                | MdoType::CalculationRegister => Some((mdo_type, name.to_string())),
                _ => None,
            }
        })
        .collect()
}

fn parse_code_series(s: &str) -> CodeSeries {
    match s {
        "WholeCatalog" | "WholeCharacteristicKind" | "WholeChartOfAccounts" => {
            CodeSeries::WholeCatalog
        }
        "WithinSubordination" => CodeSeries::WithinSubordination,
        "WithinOwnerSubordination" | "WithinOwner" => CodeSeries::WithinOwnerSubordination,
        _ => CodeSeries::Unknown,
    }
}

fn parse_attribute_node(node: roxmltree::Node<'_, '_>) -> Result<Attribute> {
    let props = find_child(node, "Properties")
        .ok_or_else(|| MetadataError::InvalidFormat("Attribute missing Properties".to_string()))?;
    let name = child_text(props, "Name").unwrap_or("").to_string();
    let _span = tracing::debug_span!("parse_attribute", attr_name = %name).entered();

    let type_node = find_child(props, "Type").ok_or_else(|| {
        MetadataError::InvalidFormat(format!("Attribute '{}' missing Type", name))
    })?;
    let attr_type = parse_type_xml(type_node)?;
    Ok(Attribute { name, name_en: None, attr_type })
}

fn parse_tabular_section_node(node: roxmltree::Node<'_, '_>) -> Result<TabularSection> {
    let uuid_str = node.attribute("uuid").unwrap_or("");
    let uuid = parse_uuid(uuid_str, "tabular section")?;

    let props = find_child(node, "Properties").ok_or_else(|| {
        MetadataError::InvalidFormat("TabularSection missing Properties".to_string())
    })?;

    let name = child_text(props, "Name").unwrap_or("").to_string();
    let mut tabular_section = TabularSection::new(uuid, name);

    if let Some(synonym_node) = find_child(props, "Synonym") {
        tabular_section.set_synonym(synonym_node.text().map(|s| s.to_string()));
    }

    let use_mode = child_text(props, "Use").map(|s| s.to_string());
    tabular_section.set_use_mode(use_mode);

    let Some(child_objects) = find_child(node, "ChildObjects") else {
        return Ok(tabular_section);
    };

    let mut ts_attributes = Vec::new();
    for attr_node in
        child_objects.children().filter(|n| n.is_element() && n.tag_name().name() == "Attribute")
    {
        let attr_uuid_str = attr_node.attribute("uuid").unwrap_or("");
        let attr_uuid = parse_uuid(attr_uuid_str, "tabular section attribute")?;

        let attr_props = find_child(attr_node, "Properties").ok_or_else(|| {
            MetadataError::InvalidFormat("TS Attribute missing Properties".to_string())
        })?;
        let attr_name = child_text(attr_props, "Name").unwrap_or("").to_string();

        let _attr_span = tracing::debug_span!(
            "parse_ts_attribute",
            ts_name = %tabular_section.name(),
            attr_name = %attr_name
        )
        .entered();

        let type_node = find_child(attr_props, "Type").ok_or_else(|| {
            MetadataError::InvalidFormat(format!("TS Attribute '{}' missing Type", attr_name))
        })?;
        let attr_type = parse_type_xml(type_node)?;

        ts_attributes.push(TabularSectionAttribute::new(attr_uuid, attr_name, attr_type));
    }

    tabular_section.set_attributes(ts_attributes);
    Ok(tabular_section)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CATALOG_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.20">
    <Catalog uuid="d11b89e1-90a2-47e7-b43f-7f231ec64b2f">
        <Properties>
            <Name>Валюты</Name>
        </Properties>
    </Catalog>
</MetaDataObject>"#;

    #[test]
    fn parse_catalog_xml_reads_root_uuid() {
        let mdo = parse_catalog_xml(CATALOG_XML).unwrap();
        assert_eq!(mdo.name, "Валюты");
        assert_eq!(mdo.mdo_type, MdoType::Catalog);
        assert_eq!(
            mdo.uuid().map(|u| u.to_string()),
            Some("d11b89e1-90a2-47e7-b43f-7f231ec64b2f".to_string())
        );
    }

    #[test]
    fn adopted_document_parses_target_uuid_without_inventing_own_token() {
        let mdo = parse_document_xml(
            r#"<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.20">
                <Document uuid="d11b89e1-90a2-47e7-b43f-7f231ec64b2f">
                    <Properties>
                        <Name>Заказ</Name>
                        <ObjectBelonging>Adopted</ObjectBelonging>
                        <ExtendedConfigurationObject>9de71b46-e9bf-4b0b-8f3c-4abcd6a385de</ExtendedConfigurationObject>
                    </Properties>
                </Document>
            </MetaDataObject>"#,
        )
        .unwrap();

        assert_eq!(mdo.object_belonging(), crate::ObjectBelonging::Adopted);
        assert_eq!(mdo.extends_uuid().unwrap().to_string(), "9de71b46-e9bf-4b0b-8f3c-4abcd6a385de");
        assert_eq!(mdo.uuid().unwrap().to_string(), "d11b89e1-90a2-47e7-b43f-7f231ec64b2f");
    }

    fn code_attr_type(mdo: &crate::metadata_object::MetadataObject) -> &AttributeType {
        mdo.attributes
            .iter()
            .find(|a| a.name == "Код")
            .map(|a| &a.attr_type)
            .expect("catalog must expose the Код standard attribute")
    }

    #[test]
    fn catalog_numeric_code_type_is_typed_as_number() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.20">
    <Catalog uuid="d11b89e1-90a2-47e7-b43f-7f231ec64b30">
        <Properties>
            <Name>ИнтервалыМониторинга</Name>
            <CodeLength>5</CodeLength>
            <DescriptionLength>50</DescriptionLength>
            <CodeType>Number</CodeType>
        </Properties>
    </Catalog>
</MetaDataObject>"#;
        let mdo = parse_catalog_xml(xml).unwrap();
        assert_eq!(code_attr_type(&mdo), &AttributeType::Number { precision: 5, scale: 0 });
    }

    #[test]
    fn catalog_default_code_type_is_string() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.20">
    <Catalog uuid="d11b89e1-90a2-47e7-b43f-7f231ec64b31">
        <Properties>
            <Name>Контрагенты</Name>
            <CodeLength>9</CodeLength>
            <DescriptionLength>50</DescriptionLength>
            <CodeType>String</CodeType>
        </Properties>
    </Catalog>
</MetaDataObject>"#;
        let mdo = parse_catalog_xml(xml).unwrap();
        assert_eq!(code_attr_type(&mdo), &AttributeType::String { length: Some(9) });
    }

    #[test]
    fn document_numeric_number_type_is_typed_as_number() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.20">
    <Document uuid="9de71b46-e9bf-4b0b-8f3c-4abcd6a385de">
        <Properties>
            <Name>ЗаказКлиента</Name>
            <NumberLength>11</NumberLength>
            <NumberType>Number</NumberType>
        </Properties>
    </Document>
</MetaDataObject>"#;
        let mdo = parse_document_xml(xml).unwrap();
        let number = mdo
            .attributes
            .iter()
            .find(|a| a.name == "Номер")
            .expect("document must expose the Номер standard attribute");
        assert_eq!(number.attr_type, AttributeType::Number { precision: 11, scale: 0 });
    }

    #[test]
    fn parse_document_xml_reads_root_uuid() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.20">
    <Document uuid="9de71b46-e9bf-4b0b-8f3c-4abcd6a385dd">
        <Properties><Name>АвансовыйОтчет</Name></Properties>
    </Document>
</MetaDataObject>"#;
        let mdo = parse_document_xml(xml).unwrap();
        assert_eq!(
            mdo.uuid().map(|u| u.to_string()),
            Some("9de71b46-e9bf-4b0b-8f3c-4abcd6a385dd".to_string())
        );
    }

    #[test]
    fn parse_chart_of_accounts_xml_builds_full_account_row() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:v8="http://v8.1c.ru/8.1/data/core" version="2.20">
    <ChartOfAccounts uuid="b1c40e57-2a31-44f0-9c91-1d70f25ad101">
        <Properties>
            <Name>Хозрасчетный</Name>
            <ExtDimensionTypes>ChartOfCharacteristicTypes.ВидыСубконто1</ExtDimensionTypes>
            <CodeLength>10</CodeLength>
            <DescriptionLength>50</DescriptionLength>
        </Properties>
        <ChildObjects>
            <Attribute uuid="11111111-1111-1111-1111-111111111111">
                <Properties>
                    <Name>Долгосрочный</Name>
                    <Type><v8:Type>xs:boolean</v8:Type></Type>
                </Properties>
            </Attribute>
            <AccountingFlag uuid="22222222-2222-2222-2222-222222222222">
                <Properties>
                    <Name>Валютный</Name>
                    <Type><v8:Type>xs:boolean</v8:Type></Type>
                </Properties>
            </AccountingFlag>
            <ExtDimensionAccountingFlag uuid="33333333-3333-3333-3333-333333333333">
                <Properties>
                    <Name>Суммовой</Name>
                    <Type><v8:Type>xs:boolean</v8:Type></Type>
                </Properties>
            </ExtDimensionAccountingFlag>
        </ChildObjects>
    </ChartOfAccounts>
</MetaDataObject>"#;
        let mdo = parse_chart_of_accounts_xml(xml).unwrap();
        let names: Vec<&str> = mdo.attributes.iter().map(|a| a.name.as_str()).collect();
        for expected in ["Родитель", "Порядок", "Вид", "Забалансовый", "Валютный", "Долгосрочный"]
        {
            assert!(names.contains(&expected), "account row must have {expected}: {names:?}");
        }
        assert!(!names.contains(&"ЭтоГруппа"), "charts of accounts have no folders: {names:?}");
        assert!(
            !names.contains(&"Суммовой"),
            "ext-dimension flag belongs to ВидыСубконто, not the account row: {names:?}"
        );

        let subkonto =
            mdo.find_tabular_section("ВидыСубконто").expect("implicit ВидыСубконто section");
        let columns: Vec<&str> = subkonto.attributes().iter().map(|a| a.name()).collect();
        for expected in ["ВидСубконто", "Предопределенное", "ТолькоОбороты", "Суммовой"]
        {
            assert!(columns.contains(&expected), "ВидыСубконто must have {expected}: {columns:?}");
        }
        let kind_column = subkonto.attributes().iter().find(|a| a.name() == "ВидСубконто").unwrap();
        assert_eq!(
            kind_column.attr_type(),
            &AttributeType::Ref {
                mdo_type: MdoType::ChartOfCharacteristicTypes,
                name: "ВидыСубконто1".to_string()
            },
            "ВидСубконто is typed by the ExtDimensionTypes property"
        );
    }

    #[test]
    fn parse_chart_of_calculation_types_xml_builds_dependency_sections() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:v8="http://v8.1c.ru/8.1/data/core" version="2.20">
    <ChartOfCalculationTypes uuid="b1c40e57-2a31-44f0-9c91-1d70f25ad201">
        <Properties>
            <Name>ОсновныеНачисления</Name>
            <DescriptionLength>50</DescriptionLength>
            <DependenceOnCalculationTypes>OnActionPeriod</DependenceOnCalculationTypes>
        </Properties>
        <ChildObjects>
            <Attribute uuid="11111111-1111-1111-1111-111111111111">
                <Properties>
                    <Name>СпособРасчета</Name>
                    <Type><v8:Type>xs:string</v8:Type></Type>
                </Properties>
            </Attribute>
        </ChildObjects>
    </ChartOfCalculationTypes>
</MetaDataObject>"#;
        let mdo = parse_chart_of_calculation_types_xml(xml).unwrap();
        let names: Vec<&str> = mdo.attributes.iter().map(|a| a.name.as_str()).collect();
        for expected in ["Ссылка", "Наименование", "Предопределенный", "ПериодДействияБазовый"]
        {
            assert!(names.contains(&expected), "ПВР row must have {expected}: {names:?}");
        }

        for expected in ["ВытесняющиеВидыРасчета", "ВедущиеВидыРасчета", "БазовыеВидыРасчета"]
        {
            let section = mdo
                .find_tabular_section(expected)
                .unwrap_or_else(|| panic!("dependent ПВР must carry {expected}"));
            let kind_column =
                section.attributes().iter().find(|a| a.name() == "ВидРасчета").unwrap();
            assert_eq!(
                kind_column.attr_type(),
                &AttributeType::Ref {
                    mdo_type: MdoType::ChartOfCalculationTypes,
                    name: "ОсновныеНачисления".to_string()
                },
                "{expected}.ВидРасчета is a self-reference to the chart"
            );
        }
    }

    #[test]
    fn parse_chart_of_calculation_types_xml_independent_has_no_dependency_sections() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:v8="http://v8.1c.ru/8.1/data/core" version="2.20">
    <ChartOfCalculationTypes uuid="b1c40e57-2a31-44f0-9c91-1d70f25ad202">
        <Properties>
            <Name>ПростыеНачисления</Name>
            <DependenceOnCalculationTypes>DontUse</DependenceOnCalculationTypes>
        </Properties>
    </ChartOfCalculationTypes>
</MetaDataObject>"#;
        let mdo = parse_chart_of_calculation_types_xml(xml).unwrap();
        for absent in ["ВытесняющиеВидыРасчета", "ВедущиеВидыРасчета", "БазовыеВидыРасчета"]
        {
            assert!(
                mdo.find_tabular_section(absent).is_none(),
                "independent ПВР must not carry {absent}"
            );
        }
        let names: Vec<&str> = mdo.attributes.iter().map(|a| a.name.as_str()).collect();
        assert!(
            !names.contains(&"ПериодДействияБазовый"),
            "independent ПВР has no ПериодДействияБазовый: {names:?}"
        );
    }

    #[test]
    fn parse_task_xml_reads_addressing_attributes_as_attributes() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:v8="http://v8.1c.ru/8.1/data/core" version="2.20">
    <Task uuid="0408002c-9255-43a2-8022-82f02ee0e8f4">
        <Properties><Name>ЗадачаИсполнителя</Name></Properties>
        <ChildObjects>
            <AddressingAttribute uuid="11111111-1111-1111-1111-111111111111">
                <Properties>
                    <Name>Исполнитель</Name>
                    <Type><v8:Type>cfg:CatalogRef.Пользователи</v8:Type></Type>
                </Properties>
            </AddressingAttribute>
            <Attribute uuid="22222222-2222-2222-2222-222222222222">
                <Properties>
                    <Name>Описание</Name>
                    <Type><v8:Type>xs:string</v8:Type></Type>
                </Properties>
            </Attribute>
        </ChildObjects>
    </Task>
</MetaDataObject>"#;
        let mdo = parse_task_xml(xml).unwrap();
        assert_eq!(mdo.mdo_type, MdoType::Task);
        let names: Vec<&str> = mdo.attributes.iter().map(|a| a.name.as_str()).collect();
        assert!(names.contains(&"Исполнитель"), "addressing attribute must resolve: {names:?}");
        assert!(names.contains(&"Описание"), "regular attribute still present: {names:?}");
    }

    #[test]
    fn parse_xml_without_uuid_attribute_degrades_to_none() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.20">
    <Catalog>
        <Properties><Name>NoUuid</Name></Properties>
    </Catalog>
</MetaDataObject>"#;
        let mdo = parse_catalog_xml(xml).unwrap();
        assert_eq!(mdo.uuid(), None);
    }

    #[test]
    fn malformed_uuid_attribute_degrades_to_none() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.20">
    <Catalog uuid="not-a-uuid">
        <Properties><Name>BadUuid</Name></Properties>
    </Catalog>
</MetaDataObject>"#;
        let mdo = parse_catalog_xml(xml).unwrap();
        assert_eq!(mdo.uuid(), None);
    }
}
