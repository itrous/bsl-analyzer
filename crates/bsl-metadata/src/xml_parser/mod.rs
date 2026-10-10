mod catalog;
mod common_attribute;
mod common_module;
mod constant;
mod defined_type;
mod enum_parser;
mod event_subscription;
mod form;
pub(crate) mod helpers;
mod http_service;
mod integration_service;
mod predefined_parser;
mod register;
mod role;
mod scheduled_job;
mod standard_attributes;
mod subsystem;
mod type_parser;
mod web_service;

pub use catalog::{
    parse_business_process_xml, parse_catalog_xml, parse_chart_of_accounts_xml,
    parse_chart_of_calculation_types_xml, parse_chart_of_characteristic_types_xml,
    parse_data_processor_xml, parse_document_xml, parse_exchange_plan_xml,
    parse_external_data_processor_xml, parse_external_report_xml, parse_report_xml, parse_task_xml,
};
pub use common_attribute::parse_common_attribute_xml;
pub use common_module::parse_common_module_xml;
pub use constant::parse_constant_xml;
pub use defined_type::parse_defined_type_xml;
pub use enum_parser::parse_enum_xml;
pub use event_subscription::parse_event_subscription_xml;
pub use form::{parse_form_from_bsl_path, parse_form_xml};
pub use http_service::parse_http_service_xml;
pub use integration_service::parse_integration_service_xml;
pub use predefined_parser::parse_predefined_xml;
pub use register::{
    parse_accounting_register_xml, parse_accumulation_register_xml, parse_calculation_register_xml,
    parse_information_register_xml,
};
pub use role::{parse_rights_xml, parse_role_xml};
pub use scheduled_job::parse_scheduled_job_xml;
pub use subsystem::parse_subsystem_xml;
pub use web_service::parse_web_service_xml;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::enums::ReturnValueReuse;
    use crate::metadata_object::MdoType;
    use crate::traits::MdObject;

    const SAMPLE_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <CommonModule uuid="42869cb5-6361-4e4e-aee7-d4098cfe964d">
        <Properties>
            <Name>ГлобальныйСерверныйМодуль</Name>
            <Global>true</Global>
            <Server>true</Server>
            <ClientManagedApplication>false</ClientManagedApplication>
            <ClientOrdinaryApplication>false</ClientOrdinaryApplication>
            <ExternalConnection>false</ExternalConnection>
            <ServerCall>false</ServerCall>
            <Privileged>false</Privileged>
            <ReturnValuesReuse>DontUse</ReturnValuesReuse>
        </Properties>
    </CommonModule>
</MetaDataObject>"#;

    #[test]
    fn test_parse_common_module_xml() {
        let module = parse_common_module_xml(SAMPLE_XML).unwrap();

        assert_eq!(module.name(), "ГлобальныйСерверныйМодуль");
        assert_eq!(module.uuid().to_string(), "42869cb5-6361-4e4e-aee7-d4098cfe964d");
        assert!(module.is_server());
        assert!(module.is_global());
        assert!(!module.is_client_managed_application());
        assert!(!module.is_client_ordinary_application());
        assert!(!module.is_external_connection());
        assert!(!module.is_server_call());
        assert!(!module.is_privileged());
        assert_eq!(module.return_values_reuse(), ReturnValueReuse::DontUse);
    }

    #[test]
    fn test_parse_client_module() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <CommonModule uuid="4f304035-6a04-4455-9ce5-a5203bcb3081">
        <Properties>
            <Name>КлиентскийОбщийМодуль</Name>
            <Global>false</Global>
            <ClientManagedApplication>true</ClientManagedApplication>
            <Server>false</Server>
            <ExternalConnection>false</ExternalConnection>
            <ClientOrdinaryApplication>true</ClientOrdinaryApplication>
            <ServerCall>false</ServerCall>
            <Privileged>false</Privileged>
            <ReturnValuesReuse>DontUse</ReturnValuesReuse>
        </Properties>
    </CommonModule>
</MetaDataObject>"#;

        let module = parse_common_module_xml(xml).unwrap();

        assert_eq!(module.name(), "КлиентскийОбщийМодуль");
        assert!(!module.is_server());
        assert!(!module.is_global());
        assert!(module.is_client_managed_application());
        assert!(module.is_client_ordinary_application());
    }

    #[test]
    fn test_parse_information_register_xml() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <InformationRegister uuid="59f8d329-f39c-4999-b470-ae9fc74511ac">
        <Properties>
            <Name>РегистрСведений1</Name>
        </Properties>
        <ChildObjects>
            <Dimension uuid="532f2a7f-4c1e-4a49-8281-3c21232da2d7">
                <Properties>
                    <Name>Справочник1</Name>
                    <Master>false</Master>
                    <DenyIncompleteValues>false</DenyIncompleteValues>
                    <Indexing>DontIndex</Indexing>
                </Properties>
            </Dimension>
        </ChildObjects>
    </InformationRegister>
</MetaDataObject>"#;

        let register = parse_information_register_xml(xml).unwrap();

        assert_eq!(register.name(), "РегистрСведений1");
        assert_eq!(register.uuid().to_string(), "59f8d329-f39c-4999-b470-ae9fc74511ac");
        assert!(register.is_information_register());
        assert!(
            !register.is_recorder_subordinate(),
            "ordinary non-periodic register has no Recorder key"
        );
        assert_eq!(register.dimensions().len(), 1);

        let dimension = &register.dimensions()[0];
        assert_eq!(dimension.name(), "Справочник1");
        assert_eq!(dimension.uuid().to_string(), "532f2a7f-4c1e-4a49-8281-3c21232da2d7");
        assert!(!dimension.is_deny_incomplete_values());
        assert!(!dimension.is_master());
        assert_eq!(dimension.indexing(), "DontIndex");
    }

    #[test]
    fn information_register_write_mode_sets_recorder_subordinate_metadata() {
        let xml = r#"<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.20">
<InformationRegister uuid="59f8d329-f39c-4999-b470-ae9fc74511ad">
<Properties><Name>ПодчиненныйРегистр</Name><WriteMode>RecorderSubordinate</WriteMode></Properties>
</InformationRegister></MetaDataObject>"#;

        let register = parse_information_register_xml(xml).unwrap();
        assert!(register.is_recorder_subordinate());
    }

    #[test]
    fn test_parse_accumulation_register_xml() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <AccumulationRegister uuid="11111111-1111-1111-1111-111111111111">
        <Properties>
            <Name>РегистрНакопления1</Name>
        </Properties>
        <ChildObjects>
            <Dimension uuid="22222222-2222-2222-2222-222222222222">
                <Properties>
                    <Name>Измерение1</Name>
                    <Master>true</Master>
                    <DenyIncompleteValues>true</DenyIncompleteValues>
                    <Indexing>Index</Indexing>
                </Properties>
            </Dimension>
        </ChildObjects>
    </AccumulationRegister>
</MetaDataObject>"#;

        let register = parse_accumulation_register_xml(xml).unwrap();

        assert_eq!(register.name(), "РегистрНакопления1");
        assert!(register.is_accumulation_register());
        assert_eq!(register.dimensions().len(), 1);

        let dimension = &register.dimensions()[0];
        assert_eq!(dimension.name(), "Измерение1");
        assert!(dimension.is_deny_incomplete_values());
        assert!(dimension.is_master());
        assert_eq!(dimension.indexing(), "Index");

        let attrs = register.attributes();
        let attr_names: Vec<&str> = attrs.iter().map(|a| a.name()).collect();

        assert!(attr_names.contains(&"Активность"));
        assert!(attr_names.contains(&"НомерСтроки"));
        assert!(attr_names.contains(&"Регистратор"));
        assert!(attr_names.contains(&"Период"));
    }

    #[test]
    fn test_parse_register_with_multiple_dimensions() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <InformationRegister uuid="33333333-3333-3333-3333-333333333333">
        <Properties>
            <Name>МногоИзмерений</Name>
        </Properties>
        <ChildObjects>
            <Dimension uuid="44444444-4444-4444-4444-444444444444">
                <Properties>
                    <Name>Измерение1</Name>
                    <DenyIncompleteValues>true</DenyIncompleteValues>
                    <Master>false</Master>
                    <Indexing>Index</Indexing>
                </Properties>
            </Dimension>
            <Dimension uuid="55555555-5555-5555-5555-555555555555">
                <Properties>
                    <Name>Измерение2</Name>
                    <DenyIncompleteValues>false</DenyIncompleteValues>
                    <Master>true</Master>
                    <Indexing>DontIndex</Indexing>
                </Properties>
            </Dimension>
        </ChildObjects>
    </InformationRegister>
</MetaDataObject>"#;

        let register = parse_information_register_xml(xml).unwrap();

        assert_eq!(register.name(), "МногоИзмерений");
        assert_eq!(register.dimensions().len(), 2);

        assert_eq!(register.dimensions()[0].name(), "Измерение1");
        assert!(register.dimensions()[0].is_deny_incomplete_values());
        assert!(!register.dimensions()[0].is_master());

        assert_eq!(register.dimensions()[1].name(), "Измерение2");
        assert!(!register.dimensions()[1].is_deny_incomplete_values());
        assert!(register.dimensions()[1].is_master());
    }

    #[test]
    fn test_parse_register_without_dimensions() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <InformationRegister uuid="66666666-6666-6666-6666-666666666666">
        <Properties>
            <Name>БезИзмерений</Name>
        </Properties>
    </InformationRegister>
</MetaDataObject>"#;

        let register = parse_information_register_xml(xml).unwrap();

        assert_eq!(register.name(), "БезИзмерений");
        assert_eq!(register.dimensions().len(), 0);
    }

    #[test]
    fn test_parse_all_register_types() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <InformationRegister uuid="77777777-7777-7777-7777-777777777777">
        <Properties>
            <Name>ТестРегистр</Name>
        </Properties>
    </InformationRegister>
</MetaDataObject>"#;
        let register = parse_information_register_xml(xml).unwrap();
        assert_eq!(register.name(), "ТестРегистр");
        assert_eq!(register.mdo_type(), MdoType::InformationRegister);

        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <AccumulationRegister uuid="77777777-7777-7777-7777-777777777777">
        <Properties>
            <Name>ТестРегистр</Name>
        </Properties>
    </AccumulationRegister>
</MetaDataObject>"#;
        let register = parse_accumulation_register_xml(xml).unwrap();
        assert_eq!(register.name(), "ТестРегистр");
        assert_eq!(register.mdo_type(), MdoType::AccumulationRegister);

        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <AccountingRegister uuid="77777777-7777-7777-7777-777777777777">
        <Properties>
            <Name>ТестРегистр</Name>
        </Properties>
    </AccountingRegister>
</MetaDataObject>"#;
        let register = parse_accounting_register_xml(xml).unwrap();
        assert_eq!(register.name(), "ТестРегистр");
        assert_eq!(register.mdo_type(), MdoType::AccountingRegister);

        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <CalculationRegister uuid="77777777-7777-7777-7777-777777777777">
        <Properties>
            <Name>ТестРегистр</Name>
        </Properties>
    </CalculationRegister>
</MetaDataObject>"#;
        let register = parse_calculation_register_xml(xml).unwrap();
        assert_eq!(register.name(), "ТестРегистр");
        assert_eq!(register.mdo_type(), MdoType::CalculationRegister);
    }

    #[test]
    fn test_parse_event_subscription_xml_with_handler() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:v8="http://v8.1c.ru/8.1/data/core" version="2.10">
    <EventSubscription uuid="e557865e-afb4-4f72-b89b-5e7cf98d2029">
        <Properties>
            <Name>ПриЗаписиСправочника</Name>
            <Comment></Comment>
            <Source>
                <v8:Type>cfg:CatalogObject.Справочник1</v8:Type>
            </Source>
            <Event>OnWrite</Event>
            <Handler>CommonModule.ОбщийПодпискиНаСобытия.ПриЗаписиСправочника</Handler>
        </Properties>
    </EventSubscription>
</MetaDataObject>"#;

        let subscription = parse_event_subscription_xml(xml).unwrap();

        assert_eq!(subscription.name(), "ПриЗаписиСправочника");
        assert_eq!(subscription.uuid.to_string(), "e557865e-afb4-4f72-b89b-5e7cf98d2029");
        assert_eq!(subscription.event(), "OnWrite");
        assert_eq!(
            subscription.handler_string(),
            "CommonModule.ОбщийПодпискиНаСобытия.ПриЗаписиСправочника"
        );

        let handler = subscription.parse_handler().unwrap();
        assert_eq!(handler.module_name, "ОбщийПодпискиНаСобытия");
        assert_eq!(handler.method_name, "ПриЗаписиСправочника");
    }

    #[test]
    fn test_parse_event_subscription_xml_empty_handler() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:v8="http://v8.1c.ru/8.1/data/core" version="2.10">
    <EventSubscription uuid="90047d26-54b2-4ea0-a566-a6adc71b4d15">
        <Properties>
            <Name>ПередЗаписьюКонстанты</Name>
            <Comment></Comment>
            <Source>
                <v8:TypeSet>cfg:ConstantValueManager</v8:TypeSet>
            </Source>
            <Event>BeforeWrite</Event>
            <Handler></Handler>
        </Properties>
    </EventSubscription>
</MetaDataObject>"#;

        let subscription = parse_event_subscription_xml(xml).unwrap();

        assert_eq!(subscription.name(), "ПередЗаписьюКонстанты");
        assert_eq!(subscription.event(), "BeforeWrite");
        assert_eq!(subscription.handler_string(), "");
        assert!(subscription.parse_handler().is_none());
    }

    #[test]
    fn test_parse_catalog_xml_with_attributes() {
        use crate::metadata_object::AttributeType;

        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:v8="http://v8.1c.ru/8.1/data/core" xmlns:cfg="http://v8.1c.ru/8.1/data/enterprise/current-config" version="2.10">
    <Catalog uuid="1d6b8425-360c-4ab1-9bab-cc9a3b590bb2">
        <Properties>
            <Name>Валюты</Name>
        </Properties>
        <ChildObjects>
            <Attribute uuid="9f67d228-79aa-44e6-8dc7-fae4fbdfef2a">
                <Properties>
                    <Name>ЗагружаетсяИзИнтернета</Name>
                    <Type>
                        <v8:Type>xs:boolean</v8:Type>
                    </Type>
                </Properties>
            </Attribute>
            <Attribute uuid="231d3950-f363-4e63-83cd-8ddb81507c27">
                <Properties>
                    <Name>НаименованиеПолное</Name>
                    <Type>
                        <v8:Type>xs:string</v8:Type>
                        <v8:StringQualifiers>
                            <v8:Length>50</v8:Length>
                        </v8:StringQualifiers>
                    </Type>
                </Properties>
            </Attribute>
            <Attribute uuid="87429f11-bf95-4013-bf13-da904570f88d">
                <Properties>
                    <Name>Наценка</Name>
                    <Type>
                        <v8:Type>xs:decimal</v8:Type>
                        <v8:NumberQualifiers>
                            <v8:Digits>10</v8:Digits>
                            <v8:FractionDigits>2</v8:FractionDigits>
                        </v8:NumberQualifiers>
                    </Type>
                </Properties>
            </Attribute>
            <Attribute uuid="6173cab2-e0f5-40c1-8e74-4f41fc8bd68f">
                <Properties>
                    <Name>ОсновнаяВалюта</Name>
                    <Type>
                        <v8:Type>cfg:CatalogRef.Валюты</v8:Type>
                    </Type>
                </Properties>
            </Attribute>
        </ChildObjects>
    </Catalog>
</MetaDataObject>"#;

        let catalog = parse_catalog_xml(xml).unwrap();

        assert_eq!(catalog.name, "Валюты");
        assert!(catalog.attributes.len() >= 4);

        let attr1 = catalog.find_attribute("ЗагружаетсяИзИнтернета").unwrap();
        assert_eq!(attr1.name, "ЗагружаетсяИзИнтернета");
        assert_eq!(attr1.attr_type, AttributeType::Boolean);

        let attr2 = catalog.find_attribute("НаименованиеПолное").unwrap();
        assert_eq!(attr2.name, "НаименованиеПолное");
        assert_eq!(attr2.attr_type, AttributeType::String { length: Some(50) });

        let attr3 = catalog.find_attribute("Наценка").unwrap();
        assert_eq!(attr3.name, "Наценка");
        assert_eq!(attr3.attr_type, AttributeType::Number { precision: 10, scale: 2 });

        let attr4 = catalog.find_attribute("ОсновнаяВалюта").unwrap();
        assert_eq!(attr4.name, "ОсновнаяВалюта");
        assert_eq!(
            attr4.attr_type,
            AttributeType::Ref { mdo_type: MdoType::Catalog, name: "Валюты".to_string() }
        );
    }

    #[test]
    fn test_parse_catalog_xml_no_custom_attributes() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <Catalog uuid="aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa">
        <Properties>
            <Name>ПростойСправочник</Name>
        </Properties>
    </Catalog>
</MetaDataObject>"#;

        let catalog = parse_catalog_xml(xml).unwrap();

        assert_eq!(catalog.name, "ПростойСправочник");
        assert!(catalog.attributes.len() >= 4);
        assert!(catalog.find_attribute("Ссылка").is_some());
        assert!(catalog.find_attribute("ПометкаУдаления").is_some());
        assert!(catalog.find_attribute("Предопределенный").is_some());
        assert!(catalog.find_attribute("ИмяПредопределенныхДанных").is_some());
    }

    #[test]
    fn test_parse_document_xml_with_attributes() {
        use crate::metadata_object::AttributeType;

        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:v8="http://v8.1c.ru/8.1/data/core" xmlns:cfg="http://v8.1c.ru/8.1/data/enterprise/current-config" version="2.10">
    <Document uuid="bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb">
        <Properties>
            <Name>ЗаказПокупателя</Name>
        </Properties>
        <ChildObjects>
            <Attribute uuid="cccccccc-cccc-cccc-cccc-cccccccccccc">
                <Properties>
                    <Name>Контрагент</Name>
                    <Type>
                        <v8:Type>cfg:CatalogRef.Контрагенты</v8:Type>
                    </Type>
                </Properties>
            </Attribute>
            <Attribute uuid="dddddddd-dddd-dddd-dddd-dddddddddddd">
                <Properties>
                    <Name>Дата</Name>
                    <Type>
                        <v8:Type>xs:dateTime</v8:Type>
                        <v8:DateQualifiers>
                            <v8:DateFractions>DateTime</v8:DateFractions>
                        </v8:DateQualifiers>
                    </Type>
                </Properties>
            </Attribute>
        </ChildObjects>
    </Document>
</MetaDataObject>"#;

        let document = parse_document_xml(xml).unwrap();

        assert_eq!(document.name, "ЗаказПокупателя");
        assert!(document.attributes.len() >= 2);

        let attr1 = document.find_attribute("Контрагент").unwrap();
        assert_eq!(attr1.name, "Контрагент");
        assert_eq!(
            attr1.attr_type,
            AttributeType::Ref {
                mdo_type: MdoType::Catalog, name: "Контрагенты".to_string()
            }
        );

        let attr2 = document.find_attribute("Дата").unwrap();
        assert_eq!(attr2.name, "Дата");
        assert_eq!(attr2.attr_type, AttributeType::DateTime);
    }

    #[test]
    fn test_parse_document_xml_register_records() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:xr="http://v8.1c.ru/8.3/xcf/readable" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" version="2.10">
    <Document uuid="bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb">
        <Properties>
            <Name>ДокументРегистратор</Name>
            <RegisterRecords>
                <xr:Item xsi:type="xr:MDObjectRef">AccumulationRegister.РегистрНакопления1</xr:Item>
                <xr:Item xsi:type="xr:MDObjectRef">InformationRegister.РегистрСведений1</xr:Item>
                <xr:Item xsi:type="xr:MDObjectRef">AccountingRegister.РегистрБухгалтерии1</xr:Item>
                <xr:Item xsi:type="xr:MDObjectRef">CalculationRegister.РегистрРасчета1</xr:Item>
            </RegisterRecords>
        </Properties>
    </Document>
</MetaDataObject>"#;

        let document = parse_document_xml(xml).unwrap();

        assert_eq!(
            document.register_records(),
            &[
                (MdoType::AccumulationRegister, "РегистрНакопления1".to_string()),
                (MdoType::InformationRegister, "РегистрСведений1".to_string()),
                (MdoType::AccountingRegister, "РегистрБухгалтерии1".to_string()),
                (MdoType::CalculationRegister, "РегистрРасчета1".to_string()),
            ],
        );
    }

    #[test]
    fn parse_register_records_filters_invalid_prefix() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:xr="http://v8.1c.ru/8.3/xcf/readable" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" version="2.10">
    <Document uuid="bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb">
        <Properties>
            <Name>ДокументРегистратор</Name>
            <RegisterRecords>
                <xr:Item xsi:type="xr:MDObjectRef">Catalog.X</xr:Item>
                <xr:Item xsi:type="xr:MDObjectRef">BogusPrefix.Y</xr:Item>
            </RegisterRecords>
        </Properties>
    </Document>
</MetaDataObject>"#;

        let document = parse_document_xml(xml).unwrap();

        assert!(document.register_records().is_empty());
    }

    #[test]
    fn test_designer_document_fixture_register_records() {
        let xml = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/fixtures/designer/Documents/Документ1.xml"
        ));
        let document = parse_document_xml(xml).unwrap();

        assert_eq!(
            document.register_records(),
            &[
                (MdoType::AccumulationRegister, "РегистрНакопления1".to_string()),
                (MdoType::CalculationRegister, "РегистрРасчета1".to_string()),
                (MdoType::InformationRegister, "РегистрСведений2".to_string()),
                (MdoType::AccountingRegister, "РегистрБухгалтерии1".to_string()),
            ],
        );
    }

    #[test]
    fn test_parse_type_xml_date() {
        use crate::metadata_object::AttributeType;

        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:v8="http://v8.1c.ru/8.1/data/core" version="2.10">
    <Catalog uuid="eeeeeeee-eeee-eeee-eeee-eeeeeeeeeeee">
        <Properties>
            <Name>Тест</Name>
        </Properties>
        <ChildObjects>
            <Attribute uuid="ffffffff-ffff-ffff-ffff-ffffffffffff">
                <Properties>
                    <Name>ДатаБезВремени</Name>
                    <Type>
                        <v8:Type>xs:dateTime</v8:Type>
                        <v8:DateQualifiers>
                            <v8:DateFractions>Date</v8:DateFractions>
                        </v8:DateQualifiers>
                    </Type>
                </Properties>
            </Attribute>
        </ChildObjects>
    </Catalog>
</MetaDataObject>"#;

        let catalog = parse_catalog_xml(xml).unwrap();

        let attr = catalog.find_attribute("ДатаБезВремени").unwrap();
        assert_eq!(attr.attr_type, AttributeType::Date);
    }

    #[test]
    fn test_parse_type_xml_special_types() {
        use crate::metadata_object::AttributeType;

        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:v8="http://v8.1c.ru/8.1/data/core" xmlns:cfg="http://v8.1c.ru/8.1/data/enterprise/current-config" version="2.10">
    <Catalog uuid="aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa">
        <Properties>
            <Name>Тест</Name>
        </Properties>
        <ChildObjects>
            <Attribute uuid="bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb">
                <Properties>
                    <Name>УникальныйИдентификатор</Name>
                    <Type>
                        <v8:Type>v8:UUID</v8:Type>
                    </Type>
                </Properties>
            </Attribute>
            <Attribute uuid="cccccccc-cccc-cccc-cccc-cccccccccccc">
                <Properties>
                    <Name>Хранилище</Name>
                    <Type>
                        <v8:Type>v8:ValueStorage</v8:Type>
                    </Type>
                </Properties>
            </Attribute>
            <Attribute uuid="dddddddd-dddd-dddd-dddd-dddddddddddd">
                <Properties>
                    <Name>Перечисление</Name>
                    <Type>
                        <v8:Type>cfg:EnumRef.Статусы</v8:Type>
                    </Type>
                </Properties>
            </Attribute>
        </ChildObjects>
    </Catalog>
</MetaDataObject>"#;

        let catalog = parse_catalog_xml(xml).unwrap();

        let attr1 = catalog.find_attribute("УникальныйИдентификатор").unwrap();
        assert_eq!(attr1.attr_type, AttributeType::Uuid);

        let attr2 = catalog.find_attribute("Хранилище").unwrap();
        assert_eq!(attr2.attr_type, AttributeType::ValueStorage);

        let attr3 = catalog.find_attribute("Перечисление").unwrap();
        assert_eq!(
            attr3.attr_type,
            AttributeType::Ref { mdo_type: MdoType::Enum, name: "Статусы".to_string() }
        );
    }

    #[test]
    fn data_processor_parses_full_xml() {
        use crate::metadata_object::AttributeType;

        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:cfg="http://v8.1c.ru/8.1/data/enterprise/current-config" xmlns:v8="http://v8.1c.ru/8.1/data/core" xmlns:xs="http://www.w3.org/2001/XMLSchema" version="2.10">
    <DataProcessor uuid="11111111-1111-1111-1111-111111111111">
        <Properties>
            <Name>Помощник</Name>
        </Properties>
        <ChildObjects>
            <Attribute uuid="22222222-2222-2222-2222-222222222222">
                <Properties>
                    <Name>АдресСайта</Name>
                    <Type>
                        <v8:Type>xs:string</v8:Type>
                        <v8:StringQualifiers>
                            <v8:Length>200</v8:Length>
                        </v8:StringQualifiers>
                    </Type>
                </Properties>
            </Attribute>
            <Attribute uuid="33333333-3333-3333-3333-333333333333">
                <Properties>
                    <Name>Флаг</Name>
                    <Type>
                        <v8:Type>xs:boolean</v8:Type>
                    </Type>
                </Properties>
            </Attribute>
        </ChildObjects>
    </DataProcessor>
</MetaDataObject>"#;

        let mdo = parse_data_processor_xml(xml).unwrap();
        assert_eq!(mdo.name, "Помощник");
        assert_eq!(mdo.mdo_type, MdoType::DataProcessor);
        assert_eq!(mdo.attributes.len(), 2);
        assert!(matches!(
            mdo.find_attribute("АдресСайта").unwrap().attr_type,
            AttributeType::String { length: Some(200) }
        ));
        assert!(matches!(mdo.find_attribute("Флаг").unwrap().attr_type, AttributeType::Boolean));
    }

    #[test]
    fn report_parses_full_xml() {
        use crate::metadata_object::AttributeType;

        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:cfg="http://v8.1c.ru/8.1/data/enterprise/current-config" xmlns:v8="http://v8.1c.ru/8.1/data/core" xmlns:xs="http://www.w3.org/2001/XMLSchema" version="2.10">
    <Report uuid="44444444-4444-4444-4444-444444444444">
        <Properties>
            <Name>Анализ</Name>
        </Properties>
        <ChildObjects>
            <Attribute uuid="55555555-5555-5555-5555-555555555555">
                <Properties>
                    <Name>ПериодОтчёта</Name>
                    <Type>
                        <v8:Type>xs:dateTime</v8:Type>
                        <v8:DateQualifiers>
                            <v8:DateFractions>Date</v8:DateFractions>
                        </v8:DateQualifiers>
                    </Type>
                </Properties>
            </Attribute>
        </ChildObjects>
    </Report>
</MetaDataObject>"#;

        let mdo = parse_report_xml(xml).unwrap();
        assert_eq!(mdo.name, "Анализ");
        assert_eq!(mdo.mdo_type, MdoType::Report);
        assert_eq!(mdo.attributes.len(), 1);
        assert_eq!(mdo.find_attribute("ПериодОтчёта").unwrap().attr_type, AttributeType::Date);
    }

    #[test]
    fn parses_data_processor_object_type() {
        use crate::metadata_object::AttributeType;

        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:v8="http://v8.1c.ru/8.1/data/core" xmlns:cfg="http://v8.1c.ru/8.1/data/enterprise/current-config" version="2.10">
    <Catalog uuid="aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa">
        <Properties>
            <Name>Тест</Name>
        </Properties>
        <ChildObjects>
            <Attribute uuid="bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb">
                <Properties>
                    <Name>Обработка</Name>
                    <Type>
                        <v8:Type>cfg:DataProcessorObject.Помощник</v8:Type>
                    </Type>
                </Properties>
            </Attribute>
        </ChildObjects>
    </Catalog>
</MetaDataObject>"#;

        let catalog = parse_catalog_xml(xml).unwrap();
        let attr = catalog.find_attribute("Обработка").unwrap();
        assert_eq!(
            attr.attr_type,
            AttributeType::Ref {
                mdo_type: MdoType::DataProcessor, name: "Помощник".to_string()
            }
        );
    }

    #[test]
    fn parses_report_object_type() {
        use crate::metadata_object::AttributeType;

        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:v8="http://v8.1c.ru/8.1/data/core" xmlns:cfg="http://v8.1c.ru/8.1/data/enterprise/current-config" version="2.10">
    <Catalog uuid="aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa">
        <Properties>
            <Name>Тест</Name>
        </Properties>
        <ChildObjects>
            <Attribute uuid="cccccccc-cccc-cccc-cccc-cccccccccccc">
                <Properties>
                    <Name>Отчёт</Name>
                    <Type>
                        <v8:Type>cfg:ReportObject.Анализ</v8:Type>
                    </Type>
                </Properties>
            </Attribute>
        </ChildObjects>
    </Catalog>
</MetaDataObject>"#;

        let catalog = parse_catalog_xml(xml).unwrap();
        let attr = catalog.find_attribute("Отчёт").unwrap();
        assert_eq!(
            attr.attr_type,
            AttributeType::Ref { mdo_type: MdoType::Report, name: "Анализ".to_string() }
        );
    }

    #[test]
    fn test_parse_defined_type_xml() {
        use crate::metadata_object::AttributeType;

        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:v8="http://v8.1c.ru/8.1/data/core" xmlns:cfg="http://v8.1c.ru/8.1/data/enterprise/current-config" version="2.10">
    <DefinedType uuid="b4407946-85d9-4f8d-9c0f-7e4c2852275e">
        <Properties>
            <Name>ОтметкаВремени</Name>
            <Type>
                <v8:Type>xs:string</v8:Type>
                <v8:StringQualifiers>
                    <v8:Length>17</v8:Length>
                    <v8:AllowedLength>Fixed</v8:AllowedLength>
                </v8:StringQualifiers>
            </Type>
        </Properties>
    </DefinedType>
</MetaDataObject>"#;

        let defined_type = parse_defined_type_xml(xml).unwrap();

        assert_eq!(defined_type.name(), "ОтметкаВремени");
        assert_eq!(defined_type.uuid().to_string(), "b4407946-85d9-4f8d-9c0f-7e4c2852275e");

        match defined_type.underlying_type() {
            AttributeType::String { length } => {
                assert_eq!(*length, Some(17));
            }
            _ => panic!("Expected String type, got {:?}", defined_type.underlying_type()),
        }
    }

    #[test]
    fn test_parse_information_register_periodic() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.20">
    <InformationRegister uuid="12345678-1234-1234-1234-123456789012">
        <Properties>
            <Name>Курсы</Name>
            <InformationRegisterPeriodicity>Second</InformationRegisterPeriodicity>
        </Properties>
    </InformationRegister>
</MetaDataObject>"#;

        let register = parse_information_register_xml(xml).unwrap();

        let attrs = register.attributes();
        let attr_names: Vec<&str> = attrs.iter().map(|a| a.name()).collect();

        assert!(attr_names.contains(&"Активность"));
        assert!(attr_names.contains(&"НомерСтроки"));
        assert!(attr_names.contains(&"Регистратор"));
        assert!(attr_names.contains(&"Период"));
    }

    #[test]
    fn test_parse_information_register_nonperiodic() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.20">
    <InformationRegister uuid="12345678-1234-1234-1234-123456789012">
        <Properties>
            <Name>Настройки</Name>
            <InformationRegisterPeriodicity>Nonperiodical</InformationRegisterPeriodicity>
        </Properties>
    </InformationRegister>
</MetaDataObject>"#;

        let register = parse_information_register_xml(xml).unwrap();

        let attrs = register.attributes();
        let attr_names: Vec<&str> = attrs.iter().map(|a| a.name()).collect();

        assert!(!attr_names.contains(&"Период"));
        assert!(attr_names.contains(&"Активность"));
        assert!(attr_names.contains(&"НомерСтроки"));
        assert!(attr_names.contains(&"Регистратор"));
    }

    /// The synthesiser is the single owner of the register's standard attributes, so it is
    /// also the one that must give them their English spellings: consumers that match by
    /// `name_en` (the SDBL scope, completion, hover) have no other source.
    #[test]
    fn register_standard_attribute_synthesis_carries_english_names() {
        let xml = r#"<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.20">
<InformationRegister uuid="12345678-1234-1234-1234-123456789012">
<Properties><Name>Курсы</Name><InformationRegisterPeriodicity>Second</InformationRegisterPeriodicity></Properties>
</InformationRegister></MetaDataObject>"#;

        let register = parse_information_register_xml(xml).unwrap();

        let name_en = |ru: &str| {
            register
                .attributes()
                .iter()
                .find(|attr| attr.name() == ru)
                .unwrap_or_else(|| panic!("`{ru}` must be synthesised"))
                .name_en()
                .map(str::to_string)
        };
        assert_eq!(name_en("Регистратор").as_deref(), Some("Recorder"));
        assert_eq!(name_en("Активность").as_deref(), Some("Active"));
        assert_eq!(name_en("НомерСтроки").as_deref(), Some("LineNumber"));
        assert_eq!(name_en("Период").as_deref(), Some("Period"));
    }

    #[test]
    fn test_catalog_standard_attributes_with_code_description() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.20">
    <Catalog uuid="...">
        <Properties>
            <Name>Валюты</Name>
            <CodeLength>3</CodeLength>
            <DescriptionLength>50</DescriptionLength>
            <Hierarchical>false</Hierarchical>
            <Owners/>
        </Properties>
    </Catalog>
</MetaDataObject>"#;

        let catalog = parse_catalog_xml(xml).unwrap();

        assert!(catalog.find_attribute("Ссылка").is_some());
        assert!(catalog.find_attribute("ПометкаУдаления").is_some());
        assert!(catalog.find_attribute("Код").is_some());
        assert!(catalog.find_attribute("Наименование").is_some());
        assert!(catalog.find_attribute("Предопределенный").is_some());
        assert!(catalog.find_attribute("ИмяПредопределенныхДанных").is_some());

        assert!(catalog.find_attribute("ЭтоГруппа").is_none());
        assert!(catalog.find_attribute("Родитель").is_none());
    }

    #[test]
    fn test_catalog_hierarchical_standard_attributes() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.20">
    <Catalog uuid="...">
        <Properties>
            <Name>Папки</Name>
            <Hierarchical>true</Hierarchical>
        </Properties>
    </Catalog>
</MetaDataObject>"#;

        let catalog = parse_catalog_xml(xml).unwrap();

        assert!(catalog.find_attribute("ЭтоГруппа").is_some());
        assert!(catalog.find_attribute("Родитель").is_some());
    }

    #[test]
    fn test_catalog_owner_single_type() {
        use crate::metadata_object::{AttributeType, MdoType};

        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:xr="http://v8.1c.ru/8.3/xcf/readable" version="2.20">
    <Catalog uuid="...">
        <Properties>
            <Name>Товары</Name>
            <Owners>
                <xr:Item>Catalog.Контрагенты</xr:Item>
            </Owners>
        </Properties>
    </Catalog>
</MetaDataObject>"#;

        let catalog = parse_catalog_xml(xml).unwrap();

        let owner = catalog.find_attribute("Владелец").expect("Owner attribute should exist");

        match &owner.attr_type {
            AttributeType::Ref { mdo_type, name } => {
                assert_eq!(*mdo_type, MdoType::Catalog);
                assert_eq!(name, "Контрагенты");
            }
            _ => panic!("Expected Ref type for single Owner, got {:?}", owner.attr_type),
        }
    }

    #[test]
    fn test_catalog_owner_multiple_types() {
        use crate::metadata_object::{AttributeType, MdoType};

        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:xr="http://v8.1c.ru/8.3/xcf/readable" version="2.20">
    <Catalog uuid="...">
        <Properties>
            <Name>БанковскиеСчета</Name>
            <Owners>
                <xr:Item>Catalog.Контрагенты</xr:Item>
                <xr:Item>Catalog.Организации</xr:Item>
            </Owners>
        </Properties>
    </Catalog>
</MetaDataObject>"#;

        let catalog = parse_catalog_xml(xml).unwrap();

        let owner = catalog.find_attribute("Владелец").expect("Owner attribute should exist");

        match &owner.attr_type {
            AttributeType::Composite { types } => {
                assert_eq!(types.len(), 2);
                assert_eq!(
                    types[0],
                    AttributeType::Ref {
                        mdo_type: MdoType::Catalog,
                        name: "Контрагенты".to_string()
                    }
                );
                assert_eq!(
                    types[1],
                    AttributeType::Ref {
                        mdo_type: MdoType::Catalog,
                        name: "Организации".to_string()
                    }
                );
            }
            _ => panic!("Expected Composite type for multiple Owners, got {:?}", owner.attr_type),
        }
    }

    #[test]
    fn test_catalog_no_owner() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.20">
    <Catalog uuid="...">
        <Properties>
            <Name>Валюты</Name>
            <Owners/>
        </Properties>
    </Catalog>
</MetaDataObject>"#;

        let catalog = parse_catalog_xml(xml).unwrap();

        assert!(catalog.find_attribute("Владелец").is_none());
    }

    #[test]
    fn test_document_standard_attributes() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.20">
    <Document uuid="...">
        <Properties>
            <Name>ПриходнаяНакладная</Name>
            <NumberLength>11</NumberLength>
        </Properties>
    </Document>
</MetaDataObject>"#;

        let document = parse_document_xml(xml).unwrap();

        assert!(document.find_attribute("Ссылка").is_some());
        assert!(document.find_attribute("ПометкаУдаления").is_some());
        assert!(document.find_attribute("Номер").is_some());
        assert!(document.find_attribute("Дата").is_some());
        assert!(document.find_attribute("Проведен").is_some());

        assert!(document.find_attribute("Код").is_none());
        assert!(document.find_attribute("Наименование").is_none());

        let number = document.find_attribute("Номер").unwrap();
        assert_eq!(number.name_en, Some("Number".to_string()));
        let date = document.find_attribute("Дата").unwrap();
        assert_eq!(date.name_en, Some("Date".to_string()));
    }

    #[test]
    fn test_business_process_standard_attributes() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.20">
    <BusinessProcess uuid="...">
        <Properties>
            <Name>Исполнение</Name>
            <NumberLength>9</NumberLength>
        </Properties>
    </BusinessProcess>
</MetaDataObject>"#;

        let bp = parse_business_process_xml(xml).unwrap();

        assert!(bp.find_attribute("Ссылка").is_some());
        assert!(bp.find_attribute("ПометкаУдаления").is_some());
        assert!(bp.find_attribute("Номер").is_some());
        assert!(bp.find_attribute("Дата").is_some());
        assert!(bp.find_attribute("Стартован").is_some());
        assert!(bp.find_attribute("Завершен").is_some());
        assert!(bp.find_attribute("ГлавнаяЗадача").is_some());

        assert!(bp.find_attribute("Проведен").is_none());
        assert!(bp.find_attribute("Код").is_none());
    }

    #[test]
    fn test_task_standard_attributes() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.20">
    <Task uuid="...">
        <Properties>
            <Name>ЗадачаИсполнителя</Name>
            <NumberLength>9</NumberLength>
        </Properties>
    </Task>
</MetaDataObject>"#;

        let task = parse_task_xml(xml).unwrap();

        assert!(task.find_attribute("Ссылка").is_some());
        assert!(task.find_attribute("ПометкаУдаления").is_some());
        assert!(task.find_attribute("Номер").is_some());
        assert!(task.find_attribute("Дата").is_some());
        assert!(task.find_attribute("Выполнена").is_some());
        assert!(task.find_attribute("БизнесПроцесс").is_some());
        assert!(task.find_attribute("ТочкаМаршрута").is_some());

        assert!(task.find_attribute("Стартован").is_none());
    }

    #[test]
    fn test_exchange_plan_standard_attributes() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.20">
    <ExchangePlan uuid="...">
        <Properties>
            <Name>ОбменДанными</Name>
            <CodeLength>9</CodeLength>
            <DescriptionLength>150</DescriptionLength>
        </Properties>
    </ExchangePlan>
</MetaDataObject>"#;

        let ep = parse_exchange_plan_xml(xml).unwrap();

        assert!(ep.find_attribute("Ссылка").is_some());
        assert!(ep.find_attribute("Код").is_some());
        assert!(ep.find_attribute("Наименование").is_some());
        assert!(ep.find_attribute("ЭтотУзел").is_some());
    }

    #[test]
    fn test_chart_of_characteristic_types_standard_attributes() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.20">
    <ChartOfCharacteristicTypes uuid="...">
        <Properties>
            <Name>ДополнительныеРеквизитыИСведения</Name>
            <CodeLength>11</CodeLength>
            <DescriptionLength>100</DescriptionLength>
        </Properties>
    </ChartOfCharacteristicTypes>
</MetaDataObject>"#;

        let cct = parse_chart_of_characteristic_types_xml(xml).unwrap();

        assert!(cct.find_attribute("Ссылка").is_some());
        assert!(cct.find_attribute("Код").is_some());
        assert!(cct.find_attribute("Наименование").is_some());
        assert!(cct.find_attribute("ТипЗначения").is_some());
    }

    #[test]
    fn test_chart_of_accounts_standard_attributes() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.20">
    <ChartOfAccounts uuid="...">
        <Properties>
            <Name>Хозрасчетный</Name>
            <CodeLength>4</CodeLength>
            <DescriptionLength>100</DescriptionLength>
        </Properties>
    </ChartOfAccounts>
</MetaDataObject>"#;

        let coa = parse_chart_of_accounts_xml(xml).unwrap();

        assert!(coa.find_attribute("Ссылка").is_some());
        assert!(coa.find_attribute("Код").is_some());
        assert!(coa.find_attribute("Наименование").is_some());
        assert!(coa.find_attribute("Порядок").is_some());
    }
}
