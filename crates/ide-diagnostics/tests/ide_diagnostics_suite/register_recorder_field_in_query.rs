//! `RegisterRecorderFieldInQuery` needs to know that the source is a register with a recorder,
//! so it speaks only when the query is checked against metadata.

use ide_diagnostics::{DiagnosticCode, DiagnosticsConfig, METADATA_DEPENDENT_CODES};

const READS_ATTRIBUTE: &str = "ВЫБРАТЬ Движения.Регистратор.Контрагент КАК Контрагент \
                               ИЗ РегистрНакопления.ТоварыНаСкладах КАК Движения";
const READS_STANDARD_FIELD: &str = "ВЫБРАТЬ Движения.Регистратор.Дата КАК Дата \
                                    ИЗ РегистрНакопления.ТоварыНаСкладах КАК Движения";

fn configuration() -> bsl_metadata::Configuration {
    let xml = r#"<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
<AccumulationRegister uuid="11111111-1111-1111-1111-111111111111">
<Properties><Name>ТоварыНаСкладах</Name></Properties>
<ChildObjects>
<Dimension uuid="22222222-2222-2222-2222-222222222222"><Properties><Name>Склад</Name></Properties></Dimension>
</ChildObjects>
</AccumulationRegister></MetaDataObject>"#;
    let register = bsl_metadata::xml_parser::parse_accumulation_register_xml(xml)
        .expect("accumulation register fixture must parse");
    let mut configuration = bsl_metadata::Configuration::new("TestConfig");
    configuration.add_register(register);
    configuration
}

fn messages(query: &str, with_metadata: bool) -> Vec<String> {
    let configuration = configuration();
    let resolver: Option<&dyn bsl_metadata::QueryMetadataResolver> =
        with_metadata.then_some(&configuration);
    ide_diagnostics::validate_query_text(&DiagnosticsConfig::all_enabled(), resolver, query)
        .into_iter()
        .filter(|d| d.code == DiagnosticCode::RegisterRecorderFieldInQuery)
        .map(|d| d.message)
        .collect()
}

#[test]
fn register_recorder_field_is_listed_as_a_code_that_needs_metadata() {
    assert!(METADATA_DEPENDENT_CODES.contains(&DiagnosticCode::RegisterRecorderFieldInQuery));
}

#[test]
fn register_recorder_attribute_is_reported_with_its_name() {
    assert_eq!(
        messages(READS_ATTRIBUTE, true),
        vec!["В запросе к регистру читается реквизит «Контрагент» документа-регистратора. \
             Регистр должен быть самодостаточен: нужные данные хранятся его измерениями и \
             реквизитами (#std477)"
            .to_string()]
    );
}

#[test]
fn register_recorder_standard_field_is_not_reported() {
    assert_eq!(messages(READS_STANDARD_FIELD, true), Vec::<String>::new());
}

#[test]
fn register_recorder_attribute_is_absent_without_metadata() {
    assert_eq!(messages(READS_ATTRIBUTE, true).len(), 1, "the control: with metadata it is there");
    assert_eq!(messages(READS_ATTRIBUTE, false), Vec::<String>::new());
}
