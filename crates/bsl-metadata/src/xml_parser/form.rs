use crate::enums::FormType;
use crate::error::{MetadataError, Result};
use crate::form::{
    Form, FormAdditionalColumns, FormAttribute, FormAttributeColumn, FormElement, FormElementKind,
    FormEventHandler,
};
use crate::metadata_object::AttributeType;

use super::helpers::parse_uuid;
use super::type_parser::parse_type_xml;

pub fn parse_form_xml(xml: &str) -> Result<Form> {
    let _span = tracing::debug_span!("parse_form_xml").entered();

    let doc = roxmltree::Document::parse(xml)
        .map_err(|e| MetadataError::InvalidFormat(format!("Invalid form XML: {}", e)))?;

    let root = doc.root_element();

    let form_node = if root.tag_name().name() == "Form" {
        root
    } else {
        root.children()
            .find(|n| n.is_element() && n.tag_name().name() == "Form")
            .ok_or_else(|| MetadataError::InvalidFormat("No <Form> element found".to_string()))?
    };

    let uuid_str = form_node.attribute("uuid").unwrap_or("");
    let uuid = if uuid_str.is_empty() { uuid::Uuid::nil() } else { parse_uuid(uuid_str, "form")? };

    let name = form_node
        .children()
        .find(|n| n.is_element() && n.tag_name().name() == "Properties")
        .and_then(|props| {
            props.children().find(|n| n.is_element() && n.tag_name().name() == "Name")
        })
        .and_then(|n| n.text())
        .unwrap_or("")
        .to_string();

    let form_type_str = form_node
        .children()
        .find(|n| n.is_element() && n.tag_name().name() == "FormType")
        .and_then(|n| n.text())
        .unwrap_or("");
    let form_type = if form_type_str.is_empty() {
        FormType::Managed
    } else {
        FormType::from_name(form_type_str)
    };

    let mut elements = Vec::new();
    if let Some(child_items) =
        form_node.children().find(|n| n.is_element() && n.tag_name().name() == "ChildItems")
    {
        collect_child_items(child_items, &mut elements, None);
    }

    let event_handlers = collect_all_events(form_node);

    let command_handlers: Vec<String> = form_node
        .children()
        .find(|n| n.is_element() && n.tag_name().name() == "Commands")
        .map(|commands| {
            commands
                .children()
                .filter(|n| n.is_element() && n.tag_name().name() == "Command")
                .filter_map(|cmd| {
                    cmd.children()
                        .find(|n| n.is_element() && n.tag_name().name() == "Action")
                        .and_then(|n| n.text())
                        .filter(|t| !t.trim().is_empty())
                        .map(|t| t.trim().to_string())
                })
                .collect()
        })
        .unwrap_or_default();

    let attributes: Vec<FormAttribute> = form_node
        .children()
        .find(|n| n.is_element() && n.tag_name().name() == "Attributes")
        .map(|attrs| {
            attrs
                .children()
                .filter(|n| n.is_element() && n.tag_name().name() == "Attribute")
                .filter_map(parse_form_attribute)
                .collect()
        })
        .unwrap_or_default();

    let mut form =
        Form::with_handlers(name, form_type, uuid, elements, event_handlers, command_handlers);
    form.attributes = attributes;

    tracing::debug!(
        form_name = %form.name(),
        form_type = ?form.form_type(),
        uuid = %form.uuid(),
        elements_count = form.elements().len(),
        event_handlers_count = form.event_handlers().len(),
        command_handlers_count = form.command_handlers().len(),
        attributes_count = form.attributes().len(),
        "parsed form"
    );

    Ok(form)
}

fn parse_form_attribute(node: roxmltree::Node<'_, '_>) -> Option<FormAttribute> {
    let name = node.attribute("name").filter(|s| !s.is_empty())?.to_string();

    let attr_type = node
        .children()
        .find(|n| n.is_element() && n.tag_name().name() == "Type")
        .and_then(|t| parse_type_xml(t).ok())
        .unwrap_or(AttributeType::Unknown);

    let is_main = node
        .children()
        .find(|n| n.is_element() && n.tag_name().name() == "MainAttribute")
        .and_then(|n| n.text())
        .is_some_and(|t| t.trim().eq_ignore_ascii_case("true"));

    let columns_node = node.children().find(|n| n.is_element() && n.tag_name().name() == "Columns");

    let columns = columns_node
        .map(|cols| {
            cols.children()
                .filter(|n| n.is_element() && n.tag_name().name() == "Column")
                .filter_map(parse_form_attribute_column)
                .collect()
        })
        .unwrap_or_default();

    // `<AdditionalColumns table="Объект.Товары">` sits beside the attribute's own
    // columns and extends a table reached through the attribute, not the attribute.
    let additional_columns = columns_node
        .map(|cols| {
            cols.children()
                .filter(|n| n.is_element() && n.tag_name().name() == "AdditionalColumns")
                .filter_map(parse_form_additional_columns)
                .collect()
        })
        .unwrap_or_default();

    Some(FormAttribute { name, attr_type, is_main, columns, additional_columns })
}

fn parse_form_additional_columns(node: roxmltree::Node<'_, '_>) -> Option<FormAdditionalColumns> {
    let table = node.attribute("table").filter(|s| !s.is_empty())?.to_string();
    let columns = node
        .children()
        .filter(|n| n.is_element() && n.tag_name().name() == "Column")
        .filter_map(parse_form_attribute_column)
        .collect();
    Some(FormAdditionalColumns { table, columns })
}

fn parse_form_attribute_column(node: roxmltree::Node<'_, '_>) -> Option<FormAttributeColumn> {
    let name = node.attribute("name").filter(|s| !s.is_empty())?.to_string();
    let attr_type = node
        .children()
        .find(|n| n.is_element() && n.tag_name().name() == "Type")
        .and_then(|t| parse_type_xml(t).ok())
        .unwrap_or(AttributeType::Unknown);
    Some(FormAttributeColumn { name, attr_type })
}

fn collect_all_events(node: roxmltree::Node<'_, '_>) -> Vec<FormEventHandler> {
    let mut handlers = Vec::new();
    collect_events_recursive(node, &mut handlers);
    handlers
}

fn collect_events_recursive(node: roxmltree::Node<'_, '_>, handlers: &mut Vec<FormEventHandler>) {
    for child in node.children().filter(|n| n.is_element()) {
        if child.tag_name().name() == "Event" {
            if let Some(text) = child.text() {
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    let event_type = child.attribute("name").unwrap_or("").to_string();
                    handlers
                        .push(FormEventHandler { event_type, handler_name: trimmed.to_string() });
                }
            }
        } else {
            collect_events_recursive(child, handlers);
        }
    }
}

fn tag_to_kind(tag: &str) -> FormElementKind {
    match tag {
        "Table" => FormElementKind::Table,
        "UsualGroup" => FormElementKind::UsualGroup,
        "Pages" => FormElementKind::Pages,
        "Page" => FormElementKind::Page,
        "CommandBar" => FormElementKind::CommandBar,
        "ButtonGroup" => FormElementKind::ButtonGroup,
        "InputField"
        | "LabelField"
        | "CheckBoxField"
        | "RadioButtonField"
        | "HTMLField"
        | "PictureField"
        | "SpreadSheetDocumentField"
        | "SpreadsheetDocumentField"
        | "TextField"
        | "ProgressBarField"
        | "TrackBarField"
        | "CalendarField"
        | "TabField"
        | "Switch" => FormElementKind::Field,
        "Button" => FormElementKind::Button,
        "Decoration" => FormElementKind::Decoration,
        "ContextMenu"
        | "ExtendedTooltip"
        | "SearchStringAddition"
        | "ViewStatusAddition"
        | "SearchControlAddition"
        | "AutoCommandBar" => FormElementKind::Addition,
        _ => FormElementKind::Other,
    }
}

fn collect_child_items(
    child_items: roxmltree::Node<'_, '_>,
    elements: &mut Vec<FormElement>,
    parent_id: Option<u32>,
) {
    for node in child_items.children().filter(|n| n.is_element()) {
        let name = match node.attribute("name") {
            Some(n) if !n.is_empty() => n.to_string(),
            _ => continue,
        };
        let id: u32 = match node.attribute("id").and_then(|s| s.parse().ok()) {
            Some(v) => v,
            None => continue,
        };
        let data_path = node
            .children()
            .find(|n| n.is_element() && n.tag_name().name() == "DataPath")
            .and_then(|n| n.text())
            .map(|t| t.to_string());

        let kind = tag_to_kind(node.tag_name().name());
        elements.push(FormElement::with_kind(name, id, data_path, kind, parent_id));

        if let Some(nested) =
            node.children().find(|n| n.is_element() && n.tag_name().name() == "ChildItems")
        {
            collect_child_items(nested, elements, Some(id));
        }
    }
}

pub fn parse_form_from_bsl_path(bsl_path: &std::path::Path) -> Result<Form> {
    let mut forms_dir = bsl_path.to_path_buf();

    for _ in 0..4 {
        if !forms_dir.pop() {
            return Err(crate::error::MetadataError::InvalidFormat(format!(
                "Invalid form module path: {}",
                bsl_path.display()
            )));
        }
    }

    let form_name = bsl_path
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.parent())
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .ok_or_else(|| {
            crate::error::MetadataError::InvalidFormat(format!(
                "Cannot extract form name from: {}",
                bsl_path.display()
            ))
        })?;

    let ext_form_xml_path = bsl_path
        .parent()
        .and_then(|p| p.parent())
        .map(|p| {
            bsl_conventions::find_child_ci(
                p,
                bsl_conventions::ConventionalName::FormXml.canonical(),
            )
            .unwrap_or_else(|| p.join(bsl_conventions::ConventionalName::FormXml.canonical()))
        })
        .ok_or_else(|| {
            crate::error::MetadataError::InvalidFormat(format!(
                "Cannot build Ext/Form.xml path from: {}",
                bsl_path.display()
            ))
        })?;

    let metadata_xml_path = bsl_conventions::find_child_stem_exact(
        &forms_dir,
        form_name,
        bsl_conventions::XML_EXTENSION,
    )
    .unwrap_or_else(|| forms_dir.join(format!("{}.xml", form_name)));

    let metadata = std::fs::read_to_string(&metadata_xml_path)
        .ok()
        .and_then(|xml| parse_form_metadata_xml(&xml).ok());

    let mut form = match std::fs::read_to_string(&ext_form_xml_path) {
        Ok(ext_form_xml) => parse_form_xml(&ext_form_xml)?,
        // An ORDINARY form has no `Ext/Form.xml`: its dialog is the binary
        // `Ext/Form.bin`. The form's own descriptor still says `<FormType>Ordinary`,
        // which is enough to stop analyzing the module as a managed one.
        Err(_) if metadata.as_ref().is_some_and(|m| m.form_type == FormType::Ordinary) => {
            Form::ordinary_without_dialog(form_name)
        }
        Err(e) => {
            return Err(crate::error::MetadataError::InvalidFormat(format!(
                "Cannot read form XML at {}: {}",
                ext_form_xml_path.display(),
                e
            )));
        }
    };

    if let Some(metadata) = metadata {
        form.name = metadata.name;
        form.form_type = metadata.form_type;
        form.uuid = metadata.uuid;
    }

    Ok(form)
}

struct FormMetadataInfo {
    name: String,
    form_type: FormType,
    uuid: uuid::Uuid,
}

fn parse_form_metadata_xml(xml: &str) -> Result<FormMetadataInfo> {
    let doc = roxmltree::Document::parse(xml)
        .map_err(|e| MetadataError::InvalidFormat(format!("Invalid form metadata XML: {}", e)))?;

    let root = doc.root_element();

    let form_node = if matches!(root.tag_name().name(), "Form" | "CommonForm") {
        root
    } else {
        root.children()
            .find(|n| n.is_element() && matches!(n.tag_name().name(), "Form" | "CommonForm"))
            .ok_or_else(|| {
                MetadataError::InvalidFormat(
                    "No <Form> or <CommonForm> element in MetaDataObject".to_string(),
                )
            })?
    };

    let uuid_str = form_node.attribute("uuid").unwrap_or("");
    let uuid = if uuid_str.is_empty() { uuid::Uuid::nil() } else { parse_uuid(uuid_str, "form")? };

    let properties =
        form_node.children().find(|n| n.is_element() && n.tag_name().name() == "Properties");

    let form_type_str = properties
        .and_then(|props| {
            props.children().find(|n| n.is_element() && n.tag_name().name() == "FormType")
        })
        .or_else(|| {
            form_node.children().find(|n| n.is_element() && n.tag_name().name() == "FormType")
        })
        .and_then(|n| n.text())
        .unwrap_or("");
    let form_type = if form_type_str.is_empty() {
        FormType::Managed
    } else {
        FormType::from_name(form_type_str)
    };

    let name = properties
        .and_then(|props| {
            props.children().find(|n| n.is_element() && n.tag_name().name() == "Name")
        })
        .and_then(|n| n.text())
        .unwrap_or("")
        .to_string();

    Ok(FormMetadataInfo { name, form_type, uuid })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_managed_form() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<FormRoot xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <Form uuid="12345678-1234-1234-1234-123456789012">
        <Properties>
            <Name>ФормаЭлемента</Name>
        </Properties>
        <FormType>Managed</FormType>
    </Form>
</FormRoot>"#;

        let form = parse_form_xml(xml).unwrap();
        assert_eq!(form.name(), "ФормаЭлемента");
        assert_eq!(form.form_type(), FormType::Managed);
        assert!(form.is_managed());
    }

    #[test]
    fn test_parse_ordinary_form() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<FormRoot xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <Form uuid="12345678-1234-1234-1234-123456789012">
        <Properties>
            <Name>ОбычнаяФорма</Name>
        </Properties>
        <FormType>Ordinary</FormType>
    </Form>
</FormRoot>"#;

        let form = parse_form_xml(xml).unwrap();
        assert_eq!(form.name(), "ОбычнаяФорма");
        assert_eq!(form.form_type(), FormType::Ordinary);
        assert!(form.is_ordinary());
    }

    #[test]
    fn test_parse_form_default_type() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<FormRoot xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <Form uuid="12345678-1234-1234-1234-123456789012">
        <Properties>
            <Name>БезТипа</Name>
        </Properties>
    </Form>
</FormRoot>"#;

        let form = parse_form_xml(xml).unwrap();
        assert_eq!(form.form_type(), FormType::Managed);
    }

    #[test]
    fn test_parse_form_with_child_items() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<Form xmlns="http://v8.1c.ru/8.3/xcf/logform" version="2.10">
    <ChildItems>
        <InputField name="Код" id="1">
            <DataPath>Объект.Code</DataPath>
        </InputField>
        <InputField name="Наименование" id="2">
            <DataPath>Объект.Description</DataPath>
        </InputField>
        <InputField name="НесуществующийРеквизит" id="3">
            <DataPath>~Объект.НесуществующийРеквизит</DataPath>
        </InputField>
    </ChildItems>
</Form>"#;

        let form = parse_form_xml(xml).unwrap();
        assert_eq!(form.elements().len(), 3);

        let wrong: Vec<_> = form.elements_with_wrong_data_path().collect();
        assert_eq!(wrong.len(), 1);
        assert_eq!(wrong[0].name, "НесуществующийРеквизит");
        assert_eq!(wrong[0].data_path.as_deref(), Some("~Объект.НесуществующийРеквизит"));
    }

    #[test]
    fn test_parse_form_with_nested_groups() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<Form xmlns="http://v8.1c.ru/8.3/xcf/logform" version="2.10">
    <ChildItems>
        <UsualGroup name="Группа1" id="1">
            <ChildItems>
                <InputField name="ПолеВГруппе" id="2">
                    <DataPath>~Объект.УдаленноеПоле</DataPath>
                </InputField>
            </ChildItems>
        </UsualGroup>
    </ChildItems>
</Form>"#;

        let form = parse_form_xml(xml).unwrap();
        assert_eq!(form.elements().len(), 2);

        let wrong: Vec<_> = form.elements_with_wrong_data_path().collect();
        assert_eq!(wrong.len(), 1);
        assert_eq!(wrong[0].name, "ПолеВГруппе");
    }

    #[test]
    fn test_parse_form_with_events_and_commands() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<Form xmlns="http://v8.1c.ru/8.3/xcf/logform" version="2.20">
    <Events>
        <Event name="OnCreateAtServer">ПриСозданииНаСервере</Event>
        <Event name="OnOpen">ПриОткрытии</Event>
    </Events>
    <Commands>
        <Command name="Ок" id="1">
            <Action>Ок</Action>
        </Command>
        <Command name="Отмена" id="2">
            <Action>Отмена</Action>
        </Command>
    </Commands>
</Form>"#;

        let form = parse_form_xml(xml).unwrap();

        let event_handler_names = form.event_handler_names();
        assert_eq!(event_handler_names.len(), 2);
        assert!(event_handler_names.contains(&"ПриСозданииНаСервере"));
        assert!(event_handler_names.contains(&"ПриОткрытии"));

        assert_eq!(form.command_handlers().len(), 2);
        assert!(form.command_handlers().contains(&"Ок".to_string()));
        assert!(form.command_handlers().contains(&"Отмена".to_string()));

        assert!(form.is_handler("ПриСозданииНаСервере"));
        assert!(form.is_handler("присозданиинасервере"));
        assert!(form.is_handler("Ок"));
        assert!(!form.is_handler("НеСуществующийОбработчик"));
    }

    #[test]
    fn test_parse_form_with_element_events() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<Form xmlns="http://v8.1c.ru/8.3/xcf/logform" version="2.20">
    <Events>
        <Event name="OnCreateAtServer">ПриСозданииНаСервере</Event>
    </Events>
    <ChildItems>
        <Table name="Список" id="1">
            <Events>
                <Event name="OnActivateRow">СписокПриАктивизацииСтроки</Event>
            </Events>
        </Table>
        <InputField name="Поле1" id="2">
            <Events>
                <Event name="OnChange">Поле1ПриИзменении</Event>
            </Events>
        </InputField>
        <CheckBoxField name="Флаг" id="3">
            <Events>
                <Event name="OnChange">ФлагПриИзменении</Event>
            </Events>
        </CheckBoxField>
    </ChildItems>
</Form>"#;

        let form = parse_form_xml(xml).unwrap();

        assert_eq!(form.event_handler_names().len(), 4);
        assert!(form.is_handler("ПриСозданииНаСервере"));
        assert!(form.is_handler("СписокПриАктивизацииСтроки"));
        assert!(form.is_handler("Поле1ПриИзменении"));
        assert!(form.is_handler("ФлагПриИзменении"));
    }

    #[test]
    fn test_parse_form_attributes() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<Form xmlns="http://v8.1c.ru/8.3/xcf/logform" xmlns:v8="http://v8.1c.ru/8.1/data/core" version="2.20">
    <ChildItems>
        <InputField name="Замечание" id="1">
            <DataPath>Замечание</DataPath>
        </InputField>
        <InputField name="ТекущееОписание" id="4">
            <DataPath>ТекущееОписание</DataPath>
        </InputField>
    </ChildItems>
    <Attributes>
        <Attribute name="Замечание" id="1">
            <Title>
                <v8:item>
                    <v8:lang>ru</v8:lang>
                    <v8:content>Замечание</v8:content>
                </v8:item>
            </Title>
        </Attribute>
        <Attribute name="ТекущееОписание" id="2">
            <Title>
                <v8:item>
                    <v8:lang>ru</v8:lang>
                    <v8:content>Текущее описание</v8:content>
                </v8:item>
            </Title>
        </Attribute>
        <Attribute name="ИсправленноеОписание" id="3">
            <Title>
                <v8:item>
                    <v8:lang>ru</v8:lang>
                    <v8:content>Исправленное описание</v8:content>
                </v8:item>
            </Title>
        </Attribute>
    </Attributes>
</Form>"#;

        let form = parse_form_xml(xml).unwrap();

        assert_eq!(form.attributes().len(), 3);
        let names: Vec<&str> = form.attribute_names().collect();
        assert!(names.contains(&"Замечание"));
        assert!(names.contains(&"ТекущееОписание"));
        assert!(names.contains(&"ИсправленноеОписание"));

        for attr in form.attributes() {
            assert_eq!(
                attr.attr_type,
                crate::metadata_object::AttributeType::Unknown,
                "attribute {} has no <Type>, expected Unknown",
                attr.name
            );
            assert!(!attr.is_main, "no <MainAttribute> in fixture");
            assert!(attr.columns.is_empty());
        }

        assert_eq!(form.elements().len(), 2);
    }

    #[test]
    fn test_parse_real_form_xml() {
        let xml = include_str!(
            "../../fixtures/designer/Catalogs/Справочник1/Forms/ФормаЭлемента/Ext/Form.xml"
        );
        let form = parse_form_xml(xml).unwrap();

        assert_eq!(form.elements().len(), 3);

        let wrong: Vec<_> = form.elements_with_wrong_data_path().collect();
        assert_eq!(wrong.len(), 1);
        assert_eq!(wrong[0].name, "НесуществующийРеквизит");

        assert_eq!(form.event_handler_names().len(), 1);
        assert!(form.is_handler("ПриСозданииНаСервере"));
    }

    #[test]
    fn test_parse_form_with_interleaved_groups_and_attributes() {
        let xml = include_str!(
            "../../fixtures/designer/Catalogs/рдт_Рецептура/Forms/ФормаЭлемента/Ext/Form.xml"
        );
        let result = parse_form_xml(xml);
        assert!(result.is_ok(), "Form XML parsing failed: {:?}", result.err());

        let form = result.unwrap();

        assert_eq!(
            form.elements().len(),
            8,
            "Expected 8 elements (3 groups + 2 top-level inputs + 3 nested), got: {:?}",
            form.elements().iter().map(|e| &e.name).collect::<Vec<_>>()
        );

        assert_eq!(form.attributes().len(), 4);
        let attr_names: Vec<String> = form.attribute_names().map(|n| n.to_lowercase()).collect();
        assert!(attr_names.contains(&"объект".to_string()));
        assert!(attr_names.contains(&"новыйобъект".to_string()));
        assert!(attr_names.contains(&"пересчитать".to_string()));
        assert!(
            attr_names.contains(&"рольтекущегопользователявrnd".to_string()),
            "Must contain 'РольТекущегоПользователяВRnD' attribute, got: {:?}",
            form.attributes()
        );
    }

    #[test]
    fn test_parse_form_attributes_with_types() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<Form xmlns="http://v8.1c.ru/8.3/xcf/logform" xmlns:v8="http://v8.1c.ru/8.1/data/core" version="2.20">
    <Attributes>
        <Attribute name="Объект" id="1">
            <Type>
                <v8:Type>cfg:DocumentObject.Заказ</v8:Type>
            </Type>
            <MainAttribute>true</MainAttribute>
        </Attribute>
        <Attribute name="Замечание" id="2">
            <Type>
                <v8:Type>xs:string</v8:Type>
                <StringQualifiers><Length>100</Length></StringQualifiers>
            </Type>
        </Attribute>
        <Attribute name="Контрагент" id="3">
            <Type>
                <v8:Type>cfg:CatalogRef.Контрагенты</v8:Type>
            </Type>
        </Attribute>
        <Attribute name="Сумма" id="4">
            <Type>
                <v8:TypeSet>cfg:DefinedType.ДенежнаяСумма</v8:TypeSet>
            </Type>
        </Attribute>
        <Attribute name="Источник" id="5">
            <Type>
                <v8:Type>cfg:CatalogRef.Контрагенты</v8:Type>
                <v8:Type>cfg:DocumentRef.Заказ</v8:Type>
            </Type>
        </Attribute>
        <Attribute name="ТаблицаСтрок" id="6">
            <Type>
                <v8:Type>v8:ValueTable</v8:Type>
            </Type>
            <Columns>
                <Column name="Признак">
                    <Type><v8:Type>xs:boolean</v8:Type></Type>
                </Column>
                <Column name="Подразделение">
                    <Type><v8:Type>cfg:CatalogRef.СтруктураПредприятия</v8:Type></Type>
                </Column>
            </Columns>
        </Attribute>
        <Attribute name="БезТипа" id="7"/>
    </Attributes>
</Form>"#;

        let form = parse_form_xml(xml).unwrap();
        assert_eq!(form.attributes().len(), 7);

        let main = form.main_attribute().expect("Объект is MainAttribute");
        assert_eq!(main.name, "Объект");
        assert!(main.is_main);
        match &main.attr_type {
            crate::metadata_object::AttributeType::Ref { mdo_type, name } => {
                assert_eq!(*mdo_type, crate::metadata_object::MdoType::Document);
                assert_eq!(name, "Заказ");
            }
            other => panic!("Expected Ref{{Document,Заказ}}, got: {:?}", other),
        }

        let zamechanie = form.find_attribute("Замечание").unwrap();
        assert!(matches!(
            &zamechanie.attr_type,
            crate::metadata_object::AttributeType::String { length: Some(100) }
        ));
        assert!(!zamechanie.is_main);

        let kontragent = form.find_attribute("Контрагент").unwrap();
        assert!(matches!(
            &kontragent.attr_type,
            crate::metadata_object::AttributeType::Ref {
                mdo_type: crate::metadata_object::MdoType::Catalog,
                ..
            }
        ));

        let summa = form.find_attribute("Сумма").unwrap();
        assert!(matches!(
            &summa.attr_type,
            crate::metadata_object::AttributeType::DefinedType { .. }
        ));

        let source = form.find_attribute("Источник").unwrap();
        match &source.attr_type {
            crate::metadata_object::AttributeType::Composite { types } => {
                assert_eq!(types.len(), 2);
            }
            other => panic!("expected Composite, got {:?}", other),
        }

        let table = form.find_attribute("ТаблицаСтрок").unwrap();
        assert_eq!(table.columns.len(), 2);
        assert_eq!(table.columns[0].name, "Признак");
        assert!(matches!(
            &table.columns[0].attr_type,
            crate::metadata_object::AttributeType::Boolean
        ));
        assert!(matches!(
            &table.columns[1].attr_type,
            crate::metadata_object::AttributeType::Ref {
                mdo_type: crate::metadata_object::MdoType::Catalog,
                ..
            }
        ));

        let bez_tipa = form.find_attribute("БезТипа").unwrap();
        assert_eq!(bez_tipa.attr_type, crate::metadata_object::AttributeType::Unknown);
    }

    #[test]
    fn additional_columns_belong_to_the_table_they_name_not_to_the_attribute() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<Form xmlns="http://v8.1c.ru/8.3/xcf/logform" xmlns:v8="http://v8.1c.ru/8.1/data/core" version="2.10">
    <Attributes>
        <Attribute name="Объект" id="1">
            <Type>
                <v8:Type>cfg:CatalogObject.Товары</v8:Type>
            </Type>
            <MainAttribute>true</MainAttribute>
            <Columns>
                <AdditionalColumns table="Объект.Состав">
                    <Column name="ЭтоНабор" id="1">
                        <Type><v8:Type>xs:boolean</v8:Type></Type>
                    </Column>
                    <Column name="Пояснение" id="2">
                        <Type><v8:Type>xs:string</v8:Type></Type>
                    </Column>
                </AdditionalColumns>
                <AdditionalColumns table="Объект.Цены"/>
            </Columns>
        </Attribute>
    </Attributes>
</Form>"#;

        let form = parse_form_xml(xml).unwrap();
        let main = form.main_attribute().expect("Объект is MainAttribute");
        assert!(main.columns.is_empty(), "additional columns are not the attribute's own");
        assert_eq!(main.additional_columns.len(), 2);

        let composition = &main.additional_columns[0];
        assert_eq!(composition.table, "Объект.Состав");
        let names: Vec<&str> = composition.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["ЭтоНабор", "Пояснение"]);
        assert_eq!(
            composition.columns[0].attr_type,
            crate::metadata_object::AttributeType::Boolean
        );

        assert_eq!(main.additional_columns[1].table, "Объект.Цены");
        assert!(main.additional_columns[1].columns.is_empty());
    }

    #[test]
    fn test_find_attribute_case_insensitive() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<Form xmlns="http://v8.1c.ru/8.3/xcf/logform" xmlns:v8="http://v8.1c.ru/8.1/data/core" version="2.20">
    <Attributes>
        <Attribute name="Замечание" id="1">
            <Type><v8:Type>xs:string</v8:Type></Type>
        </Attribute>
    </Attributes>
</Form>"#;
        let form = parse_form_xml(xml).unwrap();
        assert!(form.find_attribute("замечание").is_some());
        assert!(form.find_attribute("ЗАМЕЧАНИЕ").is_some());
        assert!(form.find_attribute("Замечание").is_some());
        assert!(form.find_attribute("Замечани").is_none());
    }

    #[test]
    fn test_parse_form_attribute_malformed_type() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<Form xmlns="http://v8.1c.ru/8.3/xcf/logform" xmlns:v8="http://v8.1c.ru/8.1/data/core" version="2.20">
    <Attributes>
        <Attribute name="Странный" id="1">
            <Type><v8:Type>cfg:NoSuchKind.X</v8:Type></Type>
        </Attribute>
        <Attribute name="Пустой" id="2">
            <Type/>
        </Attribute>
    </Attributes>
</Form>"#;
        let form = parse_form_xml(xml).unwrap();
        assert_eq!(form.attributes().len(), 2);
        assert_eq!(
            form.find_attribute("Странный").unwrap().attr_type,
            crate::metadata_object::AttributeType::UnknownNamed("cfg:NoSuchKind.X".into())
        );
        assert_eq!(
            form.find_attribute("Пустой").unwrap().attr_type,
            crate::metadata_object::AttributeType::Unknown
        );
    }

    #[test]
    fn test_parse_form_from_bsl_path() {
        let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let bsl_path = manifest_dir
            .join("fixtures/designer/Catalogs/Справочник1/Forms/ФормаЭлемента/Ext/Form/Module.bsl");

        let module_dir = bsl_path.parent().unwrap();
        std::fs::create_dir_all(module_dir).ok();
        if !bsl_path.exists() {
            std::fs::write(&bsl_path, "// dummy").ok();
        }

        let form = parse_form_from_bsl_path(&bsl_path).unwrap();

        assert_eq!(form.name(), "ФормаЭлемента");
        assert_eq!(form.form_type(), FormType::Managed);

        assert_eq!(form.event_handler_names().len(), 1);
        assert!(form.is_handler("ПриСозданииНаСервере"));
    }

    #[test]
    fn test_parse_common_form_from_bsl_path() {
        let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let bsl_path =
            manifest_dir.join("fixtures/designer/CommonForms/ТестоваяФорма/Ext/Form/Module.bsl");

        let form = parse_form_from_bsl_path(&bsl_path).unwrap();

        assert_eq!(form.name(), "ТестоваяФорма");
        assert_eq!(form.form_type(), FormType::Ordinary);
        assert!(form.is_handler("ПриСозданииНаСервере"));
        assert!(form.is_handler("КомандаОК"));
    }

    #[test]
    fn ordinary_form_without_form_xml_is_read_from_its_descriptor() {
        let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let forms = manifest_dir.join("fixtures/designer/Catalogs/Справочник1/Forms");
        assert!(!forms.join("ФормаОбычная/Ext/Form.xml").exists());

        let form =
            parse_form_from_bsl_path(&forms.join("ФормаОбычная/Ext/Form/Module.bsl")).unwrap();

        assert_eq!(form.name(), "ФормаОбычная");
        assert_eq!(form.form_type(), FormType::Ordinary);
        assert_eq!(form.uuid().to_string(), "0f0e0d0c-0b0a-4908-8706-050403020101");
        assert!(form.elements().is_empty());
        // The dialog is unreadable, so the form's own events get their default handlers ...
        assert!(form.is_handler("ПередОткрытием"));
        assert!(form.is_handler("OnOpen"));
        assert!(form.is_handler("ПередЗаписью"));
        assert!(form.is_handler("ПослеВосстановленияЗначений"));
        // ... and nothing else: element events and managed-form events are not guessed.
        assert!(!form.is_handler("ПриСозданииНаСервере"));
        assert!(!form.is_handler("СписокПриАктивизацииСтроки"));
    }

    #[test]
    fn a_form_without_form_xml_needs_an_ordinary_descriptor() {
        let dir = tempfile::tempdir().unwrap();
        let forms = dir.path().join("Catalogs/Справочник/Forms");
        let module = forms.join("Ф/Ext/Form/Module.bsl");
        std::fs::create_dir_all(module.parent().unwrap()).unwrap();
        std::fs::write(&module, "").unwrap();
        let descriptor = |form_type: &str| {
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
	<Form uuid="0f0e0d0c-0b0a-4908-8706-050403020102">
		<Properties>
			<Name>Ф</Name>
			<FormType>{form_type}</FormType>
		</Properties>
	</Form>
</MetaDataObject>"#
            )
        };

        // No descriptor at all: nothing says what the form is.
        assert!(parse_form_from_bsl_path(&module).is_err());

        // A managed form whose `Form.xml` is missing is a broken dump, not an ordinary form.
        std::fs::write(forms.join("Ф.xml"), descriptor("Managed")).unwrap();
        assert!(parse_form_from_bsl_path(&module).is_err());

        std::fs::write(forms.join("Ф.xml"), descriptor("Ordinary")).unwrap();
        let form = parse_form_from_bsl_path(&module).unwrap();
        assert!(form.is_ordinary());
        assert_eq!(form.name(), "Ф");
    }

    #[test]
    fn test_tag_to_kind_table() {
        let cases = [
            ("Table", FormElementKind::Table),
            ("UsualGroup", FormElementKind::UsualGroup),
            ("Pages", FormElementKind::Pages),
            ("Page", FormElementKind::Page),
            ("CommandBar", FormElementKind::CommandBar),
            ("ButtonGroup", FormElementKind::ButtonGroup),
            ("InputField", FormElementKind::Field),
            ("LabelField", FormElementKind::Field),
            ("CheckBoxField", FormElementKind::Field),
            ("RadioButtonField", FormElementKind::Field),
            ("HTMLField", FormElementKind::Field),
            ("PictureField", FormElementKind::Field),
            ("SpreadSheetDocumentField", FormElementKind::Field),
            ("SpreadsheetDocumentField", FormElementKind::Field),
            ("TextField", FormElementKind::Field),
            ("ProgressBarField", FormElementKind::Field),
            ("TrackBarField", FormElementKind::Field),
            ("CalendarField", FormElementKind::Field),
            ("TabField", FormElementKind::Field),
            ("Switch", FormElementKind::Field),
            ("Button", FormElementKind::Button),
            ("Decoration", FormElementKind::Decoration),
            ("ContextMenu", FormElementKind::Addition),
            ("ExtendedTooltip", FormElementKind::Addition),
            ("SearchStringAddition", FormElementKind::Addition),
            ("ViewStatusAddition", FormElementKind::Addition),
            ("SearchControlAddition", FormElementKind::Addition),
            ("AutoCommandBar", FormElementKind::Addition),
            ("CompletelyUnknownTag", FormElementKind::Other),
        ];
        for (tag, expected) in cases {
            assert_eq!(
                tag_to_kind(tag),
                expected,
                "tag {tag:?} expected {expected:?}, got {:?}",
                tag_to_kind(tag)
            );
        }
    }

    #[test]
    fn test_collect_child_items_kind_and_parent_id() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<Form xmlns="http://v8.1c.ru/8.3/xcf/logform" version="2.10">
    <ChildItems>
        <UsualGroup name="Группа1" id="1">
            <ChildItems>
                <InputField name="ПолеВГруппе" id="2">
                    <DataPath>Объект.Code</DataPath>
                </InputField>
                <Button name="КнопкаВГруппе" id="3"/>
            </ChildItems>
        </UsualGroup>
        <Decoration name="Картинка" id="4"/>
    </ChildItems>
</Form>"#;

        let form = parse_form_xml(xml).unwrap();
        assert_eq!(form.elements().len(), 4);

        let group = form.find_element("Группа1").unwrap();
        assert_eq!(group.kind, FormElementKind::UsualGroup);
        assert_eq!(group.parent_id, None);

        let inner_field = form.find_element("ПолеВГруппе").unwrap();
        assert_eq!(inner_field.kind, FormElementKind::Field);
        assert_eq!(inner_field.parent_id, Some(1));

        let inner_button = form.find_element("КнопкаВГруппе").unwrap();
        assert_eq!(inner_button.kind, FormElementKind::Button);
        assert_eq!(inner_button.parent_id, Some(1));

        let decoration = form.find_element("Картинка").unwrap();
        assert_eq!(decoration.kind, FormElementKind::Decoration);
        assert_eq!(decoration.parent_id, None);

        let group_children: Vec<_> = form.children_of(1).map(|e| e.name.as_str()).collect();
        assert_eq!(group_children, vec!["ПолеВГруппе", "КнопкаВГруппе"]);
    }

    #[test]
    fn test_spreadsheet_document_field_parses_as_field() {
        // Report forms expose their result as a SpreadSheetDocumentField (note the
        // capital S in "Sheet" — the canonical 1C tag); it must type as ПолеФормы.
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<Form xmlns="http://v8.1c.ru/8.3/xcf/logform" version="2.10">
    <ChildItems>
        <SpreadSheetDocumentField name="Результат" id="3">
            <DataPath>Результат</DataPath>
        </SpreadSheetDocumentField>
    </ChildItems>
</Form>"#;
        let form = parse_form_xml(xml).unwrap();
        let result = form.find_element("Результат").unwrap();
        assert_eq!(result.kind, FormElementKind::Field);
        assert_eq!(result.kind.base_platform_type_name(), Some("ПолеФормы"));
    }

    #[test]
    fn test_collect_child_items_multilevel_parent_chain() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<Form xmlns="http://v8.1c.ru/8.3/xcf/logform" version="2.10">
    <ChildItems>
        <UsualGroup name="Внешняя" id="1">
            <ChildItems>
                <UsualGroup name="Внутренняя" id="2">
                    <ChildItems>
                        <Table name="ТабВнутри" id="3">
                            <ChildItems>
                                <InputField name="КолонкаА" id="4">
                                    <DataPath>Объект.Таб.А</DataPath>
                                </InputField>
                            </ChildItems>
                        </Table>
                    </ChildItems>
                </UsualGroup>
            </ChildItems>
        </UsualGroup>
    </ChildItems>
</Form>"#;
        let form = parse_form_xml(xml).unwrap();
        assert_eq!(form.elements().len(), 4);

        let outer = form.find_element("Внешняя").unwrap();
        assert_eq!(outer.kind, FormElementKind::UsualGroup);
        assert_eq!(outer.parent_id, None);

        let inner = form.find_element("Внутренняя").unwrap();
        assert_eq!(inner.kind, FormElementKind::UsualGroup);
        assert_eq!(inner.parent_id, Some(outer.id));

        let table = form.find_element("ТабВнутри").unwrap();
        assert_eq!(table.kind, FormElementKind::Table);
        assert_eq!(table.parent_id, Some(inner.id));

        let column = form.find_element("КолонкаА").unwrap();
        assert_eq!(column.kind, FormElementKind::Field);
        assert_eq!(column.parent_id, Some(table.id));

        assert_eq!(form.children_of(outer.id).count(), 1);
        assert_eq!(form.children_of(inner.id).count(), 1);
        assert_eq!(form.children_of(table.id).count(), 1);
        assert_eq!(form.children_of(column.id).count(), 0);
    }

    #[test]
    fn test_parse_real_form_with_table_kind_propagation() {
        let xml = include_str!(
            "../../fixtures/designer/Documents/Документ1/Forms/ФормаДокумента/Ext/Form.xml"
        );
        let form = parse_form_xml(xml).unwrap();

        let table = form
            .find_element("ТабличнаяЧасть1")
            .expect("fixture has <Table name=\"ТабличнаяЧасть1\">");
        assert_eq!(table.kind, FormElementKind::Table);
        assert_eq!(table.parent_id, None, "table is top-level under form's <ChildItems>");

        let table_children: Vec<_> = form.children_of(table.id).collect();
        assert!(
            !table_children.is_empty(),
            "table id={} has no children in <ChildItems>",
            table.id
        );
        assert!(
            table_children.iter().all(|c| c.parent_id == Some(table.id)),
            "every child of Table must carry parent_id=Some(table.id)"
        );
        assert!(
            table_children.iter().any(|c| c.kind == FormElementKind::Field),
            "table must have at least one Field column inside its <ChildItems>"
        );

        assert!(
            form.elements()
                .iter()
                .any(|e| e.kind == FormElementKind::Field && e.parent_id.is_none()),
            "fixture must contain at least one top-level <InputField>"
        );
    }
}
