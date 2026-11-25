//! Field sets of register tables, judged the way the unknown-field rule sees them.
//!
//! Every test pairs names the platform has with names it rejects, on one table: a field model
//! that silently went incomplete would pass the first half and fail the second, and one that
//! lost a real field would fail the first.

use std::sync::Arc;

use bsl_metadata::xml_parser::{
    parse_accounting_register_xml, parse_accumulation_register_xml, parse_chart_of_accounts_xml,
    parse_information_register_xml,
};
use bsl_metadata::Configuration;

use crate::diagnostics::SdblDiagnostic;
use crate::lower::lower_sdbl_to_hir;

const CHART_OF_ACCOUNTS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <ChartOfAccounts uuid="10000000-0000-0000-0000-000000000001">
        <Properties>
            <Name>План1</Name>
            <MaxExtDimensionCount>3</MaxExtDimensionCount>
        </Properties>
    </ChartOfAccounts>
</MetaDataObject>"#;

/// A correspondence register with one balance and one non-balance member of each kind — the
/// split the query names follow.
const ACCOUNTING_REGISTER: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:v8="http://v8.1c.ru/8.1/data/core" xmlns:xs="http://www.w3.org/2001/XMLSchema" version="2.10">
    <AccountingRegister uuid="10000000-0000-0000-0000-000000000002">
        <Properties>
            <Name>Бухгалтерия1</Name>
            <ChartOfAccounts>ChartOfAccounts.План1</ChartOfAccounts>
            <Correspondence>true</Correspondence>
        </Properties>
        <ChildObjects>
            <Dimension uuid="10000000-0000-0000-0000-000000000003">
                <Properties><Name>Измерение1</Name><Type><v8:Type>xs:string</v8:Type></Type><Balance>true</Balance></Properties>
            </Dimension>
            <Dimension uuid="10000000-0000-0000-0000-000000000004">
                <Properties><Name>Измерение2</Name><Type><v8:Type>xs:string</v8:Type></Type><Balance>false</Balance></Properties>
            </Dimension>
            <Resource uuid="10000000-0000-0000-0000-000000000005">
                <Properties><Name>Ресурс1</Name><Type><v8:Type>xs:decimal</v8:Type></Type><Balance>true</Balance></Properties>
            </Resource>
            <Resource uuid="10000000-0000-0000-0000-000000000006">
                <Properties><Name>Ресурс2</Name><Type><v8:Type>xs:decimal</v8:Type></Type><Balance>false</Balance></Properties>
            </Resource>
            <Attribute uuid="10000000-0000-0000-0000-000000000007">
                <Properties><Name>Реквизит1</Name><Type><v8:Type>xs:string</v8:Type></Type></Properties>
            </Attribute>
        </ChildObjects>
    </AccountingRegister>
</MetaDataObject>"#;

const BALANCE_REGISTER: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:v8="http://v8.1c.ru/8.1/data/core" xmlns:xs="http://www.w3.org/2001/XMLSchema" version="2.10">
    <AccumulationRegister uuid="20000000-0000-0000-0000-000000000001">
        <Properties><Name>Остатки1</Name><RegisterType>Balance</RegisterType></Properties>
        <ChildObjects>
            <Dimension uuid="20000000-0000-0000-0000-000000000002">
                <Properties><Name>Измерение1</Name><Type><v8:Type>xs:string</v8:Type></Type></Properties>
            </Dimension>
            <Resource uuid="20000000-0000-0000-0000-000000000003">
                <Properties><Name>Ресурс2</Name><Type><v8:Type>xs:decimal</v8:Type></Type></Properties>
            </Resource>
            <Attribute uuid="20000000-0000-0000-0000-000000000004">
                <Properties><Name>Реквизит1</Name><Type><v8:Type>xs:string</v8:Type></Type></Properties>
            </Attribute>
        </ChildObjects>
    </AccumulationRegister>
</MetaDataObject>"#;

const TURNOVERS_REGISTER: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:v8="http://v8.1c.ru/8.1/data/core" xmlns:xs="http://www.w3.org/2001/XMLSchema" version="2.10">
    <AccumulationRegister uuid="30000000-0000-0000-0000-000000000001">
        <Properties><Name>Обороты1</Name><RegisterType>Turnovers</RegisterType></Properties>
        <ChildObjects>
            <Dimension uuid="30000000-0000-0000-0000-000000000002">
                <Properties><Name>Измерение1</Name><Type><v8:Type>xs:string</v8:Type></Type></Properties>
            </Dimension>
            <Resource uuid="30000000-0000-0000-0000-000000000003">
                <Properties><Name>Ресурс1</Name><Type><v8:Type>xs:decimal</v8:Type></Type></Properties>
            </Resource>
        </ChildObjects>
    </AccumulationRegister>
</MetaDataObject>"#;

const INFORMATION_REGISTER: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:v8="http://v8.1c.ru/8.1/data/core" xmlns:xs="http://www.w3.org/2001/XMLSchema" version="2.10">
    <InformationRegister uuid="40000000-0000-0000-0000-000000000001">
        <Properties><Name>Сведения1</Name><InformationRegisterPeriodicity>Day</InformationRegisterPeriodicity><WriteMode>Independent</WriteMode></Properties>
        <ChildObjects>
            <Dimension uuid="40000000-0000-0000-0000-000000000002">
                <Properties><Name>Измерение2</Name><Type><v8:Type>xs:string</v8:Type></Type></Properties>
            </Dimension>
            <Resource uuid="40000000-0000-0000-0000-000000000003">
                <Properties><Name>Ресурс1</Name><Type><v8:Type>xs:decimal</v8:Type></Type></Properties>
            </Resource>
            <Attribute uuid="40000000-0000-0000-0000-000000000004">
                <Properties><Name>Реквизит1</Name><Type><v8:Type>xs:string</v8:Type></Type></Properties>
            </Attribute>
        </ChildObjects>
    </InformationRegister>
</MetaDataObject>"#;

fn configuration(with_chart: bool) -> Arc<Configuration> {
    let mut config = Configuration::new("Тест");
    if with_chart {
        config.add_metadata_object(parse_chart_of_accounts_xml(CHART_OF_ACCOUNTS).unwrap());
    }
    config.add_register(parse_accounting_register_xml(ACCOUNTING_REGISTER).unwrap());
    config.add_register(parse_accumulation_register_xml(BALANCE_REGISTER).unwrap());
    config.add_register(parse_accumulation_register_xml(TURNOVERS_REGISTER).unwrap());
    config.add_register(parse_information_register_xml(INFORMATION_REGISTER).unwrap());
    Arc::new(config)
}

fn diagnostics_of(config: Arc<Configuration>, query: &str) -> Vec<SdblDiagnostic> {
    let ast = parser::parse_sdbl(query);
    lower_sdbl_to_hir(&ast, Some(config)).all_diagnostics().cloned().collect()
}

fn unknown_fields_in(config: Arc<Configuration>, query: &str) -> Vec<String> {
    let mut names: Vec<String> = diagnostics_of(config, query)
        .into_iter()
        .filter_map(|diag| match diag {
            SdblDiagnostic::UnknownField { field_name, .. } => Some(field_name),
            _ => None,
        })
        .collect();
    names.sort();
    names
}

fn missing_tables_in(config: Arc<Configuration>, query: &str) -> Vec<String> {
    diagnostics_of(config, query)
        .into_iter()
        .filter_map(|diag| match diag {
            SdblDiagnostic::QueryToMissingMetadata { table_name, .. } => Some(table_name),
            _ => None,
        })
        .collect()
}

/// `ВЫБРАТЬ Т.<f1> КАК П1, … ИЗ <source> КАК Т`, so one query probes many names.
fn select_from(source: &str, fields: &[&str]) -> String {
    let list: Vec<String> =
        fields.iter().enumerate().map(|(i, f)| format!("Т.{f} КАК П{i}")).collect();
    format!("ВЫБРАТЬ {} ИЗ {source} КАК Т", list.join(", "))
}

fn assert_table(source: &str, present: &[&str], absent: &[&str]) {
    let config = configuration(true);
    let reported = unknown_fields_in(config.clone(), &select_from(source, present));
    assert!(reported.is_empty(), "[{source}] fields the platform has were reported: {reported:?}");

    let reported = unknown_fields_in(config, &select_from(source, absent));
    let mut expected: Vec<String> = absent.iter().map(|s| s.to_string()).collect();
    expected.sort();
    assert_eq!(reported, expected, "[{source}] every missing field must be reported");
}

#[test]
fn accounting_main_table_names_non_balance_members_by_side() {
    assert_table(
        "РегистрБухгалтерии.Бухгалтерия1",
        &[
            "Период",
            "Регистратор",
            "НомерСтроки",
            "Активность",
            "МоментВремени",
            "СчетДт",
            "СчетКт",
            "Измерение1",
            "Измерение2Дт",
            "Измерение2Кт",
            "Ресурс1",
            "Ресурс2Дт",
            "Ресурс2Кт",
            "Реквизит1",
            "СубконтоДт1",
        ],
        &["НетТакогоПоля123", "Измерение2", "Ресурс2", "Ресурс1Дт", "Счет", "ВидДвижения"],
    );
}

#[test]
fn accounting_balance_table() {
    assert_table(
        "РегистрБухгалтерии.Бухгалтерия1.Остатки(&Период, Счет = &Счет, , )",
        &[
            "Счет",
            "Субконто1",
            "Субконто3",
            "Измерение1",
            "Измерение2",
            "Ресурс1Остаток",
            "Ресурс1ОстатокДт",
            "Ресурс1ОстатокКт",
            "Ресурс2РазвернутыйОстатокДт",
            "Ресурс2РазвернутыйОстатокКт",
        ],
        &["НетТакогоПоля123", "Субконто4", "Ресурс1Оборот", "СчетДт", "Период"],
    );
}

#[test]
fn accounting_turnovers_table_has_correspondent_turnovers_only_for_non_balance_resources() {
    assert_table(
        "РегистрБухгалтерии.Бухгалтерия1.Обороты(&Начало, &Конец, Месяц, , , , , )",
        &[
            "Период",
            "ПериодМесяц",
            "Регистратор",
            "Счет",
            "КорСчет",
            "Субконто2",
            "КорСубконто3",
            "Измерение1",
            "Измерение2",
            "Измерение2Кор",
            "Ресурс1Оборот",
            "Ресурс1ОборотДт",
            "Ресурс1ОборотКт",
            "Ресурс2КорОборот",
            "Ресурс2КорОборотДт",
            "Ресурс2КорОборотКт",
        ],
        &["Ресурс1КорОборотДт", "Ресурс1КорОборот", "Измерение1Кор", "Ресурс1Остаток"],
    );
}

#[test]
fn accounting_dr_cr_turnovers_table() {
    assert_table(
        "РегистрБухгалтерии.Бухгалтерия1.ОборотыДтКт(&Начало, &Конец, , , , , , )",
        &[
            "СчетДт",
            "СчетКт",
            "СубконтоДт1",
            "СубконтоКт3",
            "Измерение1",
            "Измерение2Дт",
            "Измерение2Кт",
            "Ресурс1Оборот",
            "Ресурс2ОборотДт",
            "Ресурс2ОборотКт",
        ],
        &["Счет", "Измерение2", "Ресурс1ОборотДт", "Ресурс2Оборот", "Субконто1"],
    );
}

#[test]
fn accounting_balance_and_turnovers_table() {
    assert_table(
        "РегистрБухгалтерии.Бухгалтерия1.ОстаткиИОбороты(&Начало, &Конец, Период, , , , )",
        &[
            "Счет",
            "Субконто1",
            "ПериодДень",
            "Ресурс1НачальныйОстаток",
            "Ресурс1НачальныйОстатокДт",
            "Ресурс2НачальныйРазвернутыйОстатокКт",
            "Ресурс1Оборот",
            "Ресурс2ОборотДт",
            "Ресурс1КонечныйОстатокКт",
            "Ресурс2КонечныйРазвернутыйОстатокДт",
        ],
        &["Ресурс1Приход", "Ресурс1КорОборот", "СчетДт"],
    );
}

#[test]
fn accounting_records_with_ext_dimensions_table() {
    assert_table(
        "РегистрБухгалтерии.Бухгалтерия1.ДвиженияССубконто(, , Активность, , )",
        &[
            "Период",
            "Регистратор",
            "МоментВремени",
            "СчетДт",
            "СчетКт",
            "СубконтоДт1",
            "СубконтоКт3",
            "ВидСубконтоДт2",
            "Измерение1",
            "Измерение2Кт",
            "Ресурс1",
            "Ресурс2Дт",
            "Реквизит1",
        ],
        &["НесуществующееПоле_Тест123", "Субконто1", "СубконтоДт4", "Измерение2", "Ресурс2"],
    );
}

/// Without the chart of accounts the `Субконто<N>` family cannot be bounded; listing too few
/// would report fields the platform has, so the table must fall silent instead.
#[test]
fn an_accounting_table_without_its_chart_stays_silent() {
    let query = select_from(
        "РегистрБухгалтерии.Бухгалтерия1.Остатки(&Период, , , )",
        &["Субконто7", "НетТакогоПоля123"],
    );
    assert_eq!(unknown_fields_in(configuration(false), &query), Vec::<String>::new());
}

#[test]
fn accumulation_virtual_tables() {
    assert_table(
        "РегистрНакопления.Остатки1.Остатки(&Период, )",
        &["Измерение1", "Ресурс2Остаток"],
        &["Ресурс2", "Ресурс2Оборот", "Реквизит1", "Период", "НетТакогоПоля123"],
    );
    assert_table(
        "РегистрНакопления.Остатки1.Обороты(&Начало, &Конец, Месяц, )",
        &[
            "Измерение1",
            "Период",
            "ПериодМесяц",
            "Регистратор",
            "НомерСтроки",
            "Ресурс2Оборот",
            "Ресурс2Приход",
            "Ресурс2Расход",
        ],
        &["Ресурс2Остаток", "Ресурс2", "НетТакогоПоля123"],
    );
    assert_table(
        "РегистрНакопления.Остатки1.ОстаткиИОбороты(&Начало, &Конец, , , )",
        &[
            "Измерение1",
            "ПериодГод",
            "Ресурс2НачальныйОстаток",
            "Ресурс2Оборот",
            "Ресурс2Приход",
            "Ресурс2Расход",
            "Ресурс2КонечныйОстаток",
        ],
        &["Ресурс2Остаток", "НетТакогоПоля123"],
    );
}

#[test]
fn english_spelling_of_derived_names_is_accepted() {
    assert_table(
        "AccumulationRegister.Остатки1.Balance(&Period, )",
        &["Ресурс2Balance", "Ресурс2Остаток", "Измерение1"],
        &["Ресурс2Turnover"],
    );
}

#[test]
fn information_register_slice() {
    assert_table(
        "РегистрСведений.Сведения1.СрезПоследних(&Период, )",
        &["Период", "Измерение2", "Ресурс1", "Реквизит1"],
        &["НетТакогоПоля123", "Ресурс1Остаток"],
    );
}

/// The field is reached through the alias of a joined virtual table, not of the first source.
#[test]
fn a_field_of_a_joined_virtual_table_is_checked_through_its_alias() {
    let query = "ВЫБРАТЬ Сведения.Ресурс1 КАК Ресурс1, Остатки.НетТакогоПоля123 КАК П \
                 ИЗ РегистрСведений.Сведения1.СрезПоследних(&Период, ) КАК Сведения \
                 ЛЕВОЕ СОЕДИНЕНИЕ РегистрНакопления.Остатки1.Остатки(&Период, ) КАК Остатки \
                 ПО ИСТИНА";
    assert_eq!(unknown_fields_in(configuration(true), query), vec!["НетТакогоПоля123"]);
}

#[test]
fn a_table_the_register_does_not_have_is_missing_metadata() {
    let config = configuration(true);
    for (query, table) in [
        (
            "ВЫБРАТЬ Т.Счет КАК Счет ИЗ РегистрБухгалтерии.Бухгалтерия1.НетТакойТаблицы(&Период, , , ) КАК Т",
            "РегистрБухгалтерии.Бухгалтерия1.НетТакойТаблицы",
        ),
        (
            "ВЫБРАТЬ Т.Ресурс1 КАК Ресурс1 ИЗ РегистрСведений.Сведения1.Остатки(&Период, ) КАК Т",
            "РегистрСведений.Сведения1.Остатки",
        ),
        (
            "ВЫБРАТЬ Т.Измерение1 КАК Н ИЗ РегистрНакопления.Обороты1.Остатки(&Период, ) КАК Т",
            "РегистрНакопления.Обороты1.Остатки",
        ),
    ] {
        assert_eq!(missing_tables_in(config.clone(), query), vec![table], "{query}");
    }

    for query in [
        "ВЫБРАТЬ Т.Измерение1 КАК Н ИЗ РегистрНакопления.Обороты1.Обороты(&Начало, &Конец, , ) КАК Т",
        "ВЫБРАТЬ Т.Счет КАК Счет ИЗ РегистрБухгалтерии.Бухгалтерия1.Субконто КАК Т",
        "ВЫБРАТЬ Т.Измерение2 КАК В ИЗ РегистрСведений.Сведения1.СрезПервых(&Период, ) КАК Т",
    ] {
        assert_eq!(missing_tables_in(config.clone(), query), Vec::<String>::new(), "{query}");
    }
}

fn ambiguous_in(config: Arc<Configuration>, query: &str) -> Vec<String> {
    diagnostics_of(config, query)
        .into_iter()
        .filter_map(|diag| match diag {
            SdblDiagnostic::AmbiguousColumnRef { column_name, .. } => Some(column_name),
            _ => None,
        })
        .collect()
}

/// A condition inside a virtual table's parameters speaks of that table's own fields. Two
/// balance tables in one join both offer `Счет`; the condition of the second one must not be
/// read as a bare reference into the whole query.
#[test]
fn a_condition_in_virtual_table_parameters_does_not_reach_the_other_sources() {
    let query = "ВЫБРАТЬ Начало.Ресурс1Остаток КАК Начало, Конец.Ресурс1Остаток КАК Конец \
                 ИЗ РегистрБухгалтерии.Бухгалтерия1.Остатки(&Начало, Счет В (&Счета), , ) КАК Начало \
                 ЛЕВОЕ СОЕДИНЕНИЕ РегистрБухгалтерии.Бухгалтерия1.Остатки(&Конец, Счет В (&Счета), , \
                 Измерение1 = &Измерение1) КАК Конец \
                 ПО Начало.Счет = Конец.Счет";
    assert_eq!(ambiguous_in(configuration(true), query), Vec::<String>::new());
}

/// `ИТОГИ` names result columns. Both sources offer `Счет`, the select list names it once.
#[test]
fn totals_resolve_against_the_select_list() {
    let query = "ВЫБРАТЬ Начало.Счет КАК Счет, Начало.Ресурс1Остаток КАК Ресурс1 \
                 ИЗ РегистрБухгалтерии.Бухгалтерия1.Остатки(&Начало, , , ) КАК Начало \
                 ЛЕВОЕ СОЕДИНЕНИЕ РегистрБухгалтерии.Бухгалтерия1.Остатки(&Конец, , , ) КАК Конец \
                 ПО Начало.Счет = Конец.Счет \
                 ИТОГИ СУММА(Ресурс1) ПО Счет";
    assert_eq!(ambiguous_in(configuration(true), query), Vec::<String>::new());
}

/// `УПОРЯДОЧИТЬ ПО` names a result column first; the sources are consulted only when no output
/// has the name.
#[test]
fn order_by_resolves_against_the_select_list_first() {
    let query = "ВЫБРАТЬ Начало.Счет КАК Счет, Начало.Ресурс1Остаток КАК Ресурс1 \
                 ИЗ РегистрБухгалтерии.Бухгалтерия1.Остатки(&Начало, , , ) КАК Начало \
                 ЛЕВОЕ СОЕДИНЕНИЕ РегистрБухгалтерии.Бухгалтерия1.Остатки(&Конец, , , ) КАК Конец \
                 ПО Начало.Счет = Конец.Счет \
                 УПОРЯДОЧИТЬ ПО Счет";
    assert_eq!(ambiguous_in(configuration(true), query), Vec::<String>::new());

    let not_an_output = query.replace("УПОРЯДОЧИТЬ ПО Счет", "УПОРЯДОЧИТЬ ПО Измерение1");
    assert_eq!(
        ambiguous_in(configuration(true), &not_an_output),
        vec!["Измерение1"],
        "a name no output carries is still a source field, and both sources offer it",
    );
}

fn ambiguity_reports_in(config: Arc<Configuration>, query: &str) -> Vec<(String, Vec<String>)> {
    diagnostics_of(config, query)
        .into_iter()
        .filter_map(|diag| match diag {
            SdblDiagnostic::AmbiguousColumnRef { column_name, possible_tables, .. } => {
                Some((column_name, possible_tables))
            }
            _ => None,
        })
        .collect()
}

const TWO_BALANCE_TABLES: &str = "ИЗ РегистрБухгалтерии.Бухгалтерия1.Остатки(&Начало, , , ) КАК Начало      ЛЕВОЕ СОЕДИНЕНИЕ РегистрБухгалтерии.Бухгалтерия1.Остатки(&Конец, , , ) КАК Конец      ПО Начало.Счет = Конец.Счет";

/// An output owns its name in `УПОРЯДОЧИТЬ ПО` whatever its type. A parameter has no type the
/// lowering knows, and skipping such an output sent the name on to the two sources that both
/// offer `Счет`, which reported an ambiguity naming no source at all.
#[test]
fn an_output_of_unknown_type_still_owns_its_name_in_order_by() {
    let query = format!(
        "ВЫБРАТЬ &Значение КАК Счет, Начало.Ресурс1Остаток КАК Ресурс1 {TWO_BALANCE_TABLES}          УПОРЯДОЧИТЬ ПО Счет"
    );
    assert_eq!(ambiguity_reports_in(configuration(true), &query), Vec::new());
}

/// `Субконто<N>` has no type the lowering knows. Two sources that both offer the name are
/// still ambiguous: the collision is about the name, not the type.
#[test]
fn an_unknown_typed_ext_dimension_is_still_ambiguous_across_two_sources() {
    let query =
        "ВЫБРАТЬ Субконто1 ИЗ РегистрБухгалтерии.Бухгалтерия1.Остатки(&Период, , , ) КАК А \
                 ЛЕВОЕ СОЕДИНЕНИЕ РегистрБухгалтерии.Бухгалтерия1.Остатки(&Период, , , ) КАК Б \
                 ПО ИСТИНА";
    assert_eq!(
        ambiguity_reports_in(configuration(true), query),
        vec![("Субконто1".to_string(), vec!["А".to_string(), "Б".to_string()])],
    );
}

/// An ambiguous name in the select list is reported where it is written, naming both sources.
/// `УПОРЯДОЧИТЬ ПО` that names the output refers to the output, so it adds no second report —
/// least of all one with an empty list of sources.
#[test]
fn an_ambiguous_output_is_reported_once_with_its_sources() {
    let query = format!("ВЫБРАТЬ Счет КАК Счет {TWO_BALANCE_TABLES} УПОРЯДОЧИТЬ ПО Счет");
    assert_eq!(
        ambiguity_reports_in(configuration(true), &query),
        vec![("Счет".to_string(), vec!["Конец".to_string(), "Начало".to_string()])],
    );
}

/// A split balance the reference names outright is an occurrence in every source that offers
/// it. `Ресурс1ОстатокДт` already is; the extended spelling must be too.
#[test]
fn an_explicit_split_balance_is_ambiguous_across_two_balance_tables() {
    let query = "ВЫБРАТЬ Ресурс1РазвернутыйОстатокДт КАК Р ИЗ РегистрБухгалтерии.Бухгалтерия1.Остатки(&Начало, , , ) КАК Начало ЛЕВОЕ СОЕДИНЕНИЕ РегистрБухгалтерии.Бухгалтерия1.Остатки(&Конец, , , ) КАК Конец ПО Начало.Счет = Конец.Счет";
    assert_eq!(
        ambiguity_reports_in(configuration(true), query),
        vec![(
            "Ресурс1РазвернутыйОстатокДт".to_string(),
            vec!["Конец".to_string(), "Начало".to_string()],
        )],
    );

    for name in ["Ресурс1НачальныйРазвернутыйОстатокДт", "Ресурс1КонечныйРазвернутыйОстатокДт"]
    {
        let query = format!(
            "ВЫБРАТЬ {name} КАК Р ИЗ РегистрБухгалтерии.Бухгалтерия1.ОстаткиИОбороты(&Начало, &Конец, , , , , ) КАК Начало ЛЕВОЕ СОЕДИНЕНИЕ РегистрБухгалтерии.Бухгалтерия1.ОстаткиИОбороты(&Начало, &Конец, , , , , ) КАК Конец ПО Начало.Счет = Конец.Счет"
        );
        assert_eq!(
            ambiguity_reports_in(configuration(true), &query),
            vec![(name.to_string(), vec!["Конец".to_string(), "Начало".to_string()])],
            "{name}",
        );
    }

    // The unattested English spelling stays listed, so a query that uses it is not told the
    // field is missing. It is provisional, so it is not an occurrence for the ambiguity rule.
    let english = "ВЫБРАТЬ Ресурс1SplittedBalanceDr ИЗ РегистрБухгалтерии.Бухгалтерия1.Остатки(&Период, , , ) КАК Т";
    assert_eq!(unknown_fields_in(configuration(true), english), Vec::<String>::new());
}

/// Code shipped in a typical configuration selects a turnovers-register attribute from
/// `Обороты`; whether the platform allows it is not settled, so the field must not be reported.
#[test]
fn an_attribute_of_an_accumulation_register_is_not_reported_in_turnovers() {
    let query =
        select_from("РегистрНакопления.Остатки1.Обороты(&Начало, &Конец, Месяц, )", &["Реквизит1"]);
    assert_eq!(unknown_fields_in(configuration(true), &query), Vec::<String>::new());
}

fn with_common_attributes(open: bool) -> Arc<Configuration> {
    let applied = || bsl_metadata::ObjectCommonAttributes {
        fields: vec![
            bsl_metadata::CommonAttributeField {
                name: "Разделитель1".to_string(),
                attr_type: bsl_metadata::AttributeType::String { length: Some(10) },
                separator: true,
            },
            bsl_metadata::CommonAttributeField {
                name: "ОбщийРеквизит1".to_string(),
                attr_type: bsl_metadata::AttributeType::String { length: Some(10) },
                separator: false,
            },
        ],
        open,
    };
    let mut config = Configuration::new("Тест");
    config.add_metadata_object(parse_chart_of_accounts_xml(CHART_OF_ACCOUNTS).unwrap());
    let mut accounting = parse_accounting_register_xml(ACCOUNTING_REGISTER).unwrap();
    accounting.set_common_attributes(applied());
    config.add_register(accounting);
    let mut balance = parse_accumulation_register_xml(BALANCE_REGISTER).unwrap();
    balance.set_common_attributes(applied());
    config.add_register(balance);
    Arc::new(config)
}

/// A separator is a column of every table of the register; a plain common attribute is a
/// column of the tables that carry register attributes. Neither may be reported, and the
/// tables stay checked for everything else.
#[test]
fn common_attributes_are_columns_of_the_register_tables() {
    for (source, present) in [
        ("РегистрНакопления.Остатки1.Остатки(&Период, )", &["Разделитель1"][..]),
        ("РегистрНакопления.Остатки1.Обороты(&Начало, &Конец, , )", &["Разделитель1"][..]),
        ("РегистрБухгалтерии.Бухгалтерия1", &["Разделитель1", "ОбщийРеквизит1"][..]),
        ("РегистрБухгалтерии.Бухгалтерия1.Остатки(&Период, , , )", &["Разделитель1"][..]),
        (
            "РегистрБухгалтерии.Бухгалтерия1.ОборотыДтКт(&Начало, &Конец, , , , , , )",
            &["Разделитель1"][..],
        ),
    ] {
        let mut fields = present.to_vec();
        fields.push("НетТакогоПоля123");
        let query = select_from(source, &fields);
        assert_eq!(
            unknown_fields_in(with_common_attributes(false), &query),
            vec!["НетТакогоПоля123"],
            "{source}",
        );
        assert_eq!(
            unknown_fields_in(with_common_attributes(true), &query),
            Vec::<String>::new(),
            "{source}: a common attribute that may apply unlisted leaves the table unchecked",
        );
    }
}

/// The virtual-table rules as `(rule, table)` pairs, in report order.
fn virtual_table_rules_in(config: Arc<Configuration>, query: &str) -> Vec<(&'static str, String)> {
    diagnostics_of(config, query)
        .into_iter()
        .filter_map(|diag| match diag {
            SdblDiagnostic::VirtualTableCallWithoutParameters { table_name, .. } => {
                Some(("VirtualTableCallWithoutParameters", table_name))
            }
            SdblDiagnostic::JoinWithVirtualTable { table_name, .. } => {
                Some(("JoinWithVirtualTable", table_name))
            }
            _ => None,
        })
        .collect()
}

/// A temporary table or a tabular section may be called `Остатки` too. Only a register that
/// owns such a table makes the name a virtual table; the stored change-registration table
/// takes no parameters at all.
#[test]
fn a_stored_table_named_like_a_virtual_one_is_not_a_virtual_table_call() {
    for query in [
        "ВЫБРАТЬ Т.Реквизит1 КАК Р ИЗ Документ.Документ1.Остатки КАК Т",
        "ВЫБРАТЬ 1 КАК А ПОМЕСТИТЬ Остатки; ВЫБРАТЬ Т.А КАК А ИЗ Остатки КАК Т",
        "ВЫБРАТЬ 1 КАК А ПОМЕСТИТЬ Остатки;          ВЫБРАТЬ Т.А КАК А ИЗ Справочник.Справочник1 КАК С ЛЕВОЕ СОЕДИНЕНИЕ Остатки КАК Т ПО ИСТИНА",
        "ВЫБРАТЬ Т.Узел КАК У ИЗ РегистрСведений.Сведения1.Изменения КАК Т",
    ] {
        assert_eq!(virtual_table_rules_in(configuration(true), query), Vec::new(), "{query}");
    }
}

/// `ОборотыДтКт` and `ДвиженияССубконто` are computed tables of an accounting register like
/// `Остатки`, so the rules about calls without parameters reach them too.
#[test]
fn every_computed_table_of_an_accounting_register_is_a_virtual_table() {
    for table in ["ОборотыДтКт", "ДвиженияССубконто"] {
        let query =
            format!("ВЫБРАТЬ Т.Период КАК П ИЗ РегистрБухгалтерии.Бухгалтерия1.{table} КАК Т");
        assert_eq!(
            virtual_table_rules_in(configuration(true), &query),
            vec![(
                "VirtualTableCallWithoutParameters",
                format!("РегистрБухгалтерии.Бухгалтерия1.{table}")
            )],
        );
    }
}

/// A data-composition argument lowers to nothing but is an argument all the same.
#[test]
fn a_data_composition_argument_counts_as_an_argument() {
    let query = "ВЫБРАТЬ Т.Ресурс1Оборот КАК С \
                 ИЗ РегистрБухгалтерии.Бухгалтерия1.ОборотыДтКт({(&Начало)}, {(&Конец)}, , , , , , ) КАК Т";
    let without_parameters = diagnostics_of(configuration(true), query)
        .into_iter()
        .filter(|diag| matches!(diag, SdblDiagnostic::VirtualTableCallWithoutParameters { .. }))
        .count();
    assert_eq!(without_parameters, 0);
}

/// The change-registration table has fields of its own (`Узел`, `НомерСообщения`) that the
/// model does not list; it must not be judged by its register's main table. The Russian name is
/// also the `ДЛЯ ИЗМЕНЕНИЯ` keyword, which is how it could lose its segment.
#[test]
fn a_change_table_is_not_judged_by_the_field_set_of_its_register() {
    for source in [
        "РегистрСведений.Сведения1.Изменения",
        "InformationRegister.Сведения1.Changes",
        "РегистрБухгалтерии.Бухгалтерия1.Субконто",
    ] {
        let query = select_from(source, &["Узел", "НомерСообщения", "Вид", "Значение"]);
        let config = configuration(true);
        assert_eq!(unknown_fields_in(config.clone(), &query), Vec::<String>::new(), "{source}");
        assert_eq!(missing_tables_in(config, &query), Vec::<String>::new(), "{source}");
    }
}

/// A calculation register names tables after its base registers and recalculations, which the
/// platform does not fix; an unfamiliar third segment is therefore not reported there.
#[test]
fn an_unknown_table_of_a_calculation_register_is_not_reported() {
    let mut config = Configuration::new("Тест");
    config.add_register(
        bsl_metadata::Register::builder()
            .name("Расчет1")
            .mdo_type(bsl_metadata::MdoType::CalculationRegister)
            .build(),
    );
    let query = select_from("РегистрРасчета.Расчет1.БазаРасчет1(, , , )", &["Измерение1"]);
    let config = Arc::new(config);
    assert_eq!(missing_tables_in(config.clone(), &query), Vec::<String>::new());
    assert_eq!(unknown_fields_in(config, &query), Vec::<String>::new());
}

/// Without metadata nothing is known about the register, so its tables cannot be judged.
#[test]
fn without_metadata_an_unknown_register_table_is_not_reported() {
    let ast = parser::parse_sdbl(
        "ВЫБРАТЬ Т.Счет КАК Счет ИЗ РегистрБухгалтерии.Бухгалтерия1.НетТакойТаблицы(&П, , , ) КАК Т",
    );
    let package = lower_sdbl_to_hir(&ast, None);
    assert!(!package
        .all_diagnostics()
        .any(|diag| matches!(diag, SdblDiagnostic::QueryToMissingMetadata { .. })),);
}

/// A register without correspondence keeps one account per record, so it has no
/// debit-credit turnovers table at all; the flag is read from its metadata.
#[test]
fn a_register_without_correspondence_has_no_dr_cr_turnovers_table() {
    let mut config = Configuration::new("Тест");
    config.add_metadata_object(parse_chart_of_accounts_xml(CHART_OF_ACCOUNTS).unwrap());
    config.add_register(
        parse_accounting_register_xml(
            &ACCOUNTING_REGISTER
                .replace("<Correspondence>true", "<Correspondence>false")
                .replace("Бухгалтерия1", "Бухгалтерия2"),
        )
        .unwrap(),
    );
    config.add_register(parse_accounting_register_xml(ACCOUNTING_REGISTER).unwrap());
    config.add_register(
        parse_accounting_register_xml(
            &ACCOUNTING_REGISTER
                .replace("<Correspondence>true</Correspondence>", "")
                .replace("Бухгалтерия1", "Бухгалтерия3"),
        )
        .unwrap(),
    );
    let config = Arc::new(config);

    assert_eq!(
        missing_tables_in(
            config.clone(),
            "ВЫБРАТЬ Т.Период КАК П ИЗ РегистрБухгалтерии.Бухгалтерия2.ОборотыДтКт(&Начало, &Конец, , , , , , ) КАК Т",
        ),
        vec!["РегистрБухгалтерии.Бухгалтерия2.ОборотыДтКт"],
    );
    assert_eq!(
        missing_tables_in(
            config.clone(),
            "ВЫБРАТЬ Т.Период КАК П ИЗ РегистрБухгалтерии.Бухгалтерия1.ОборотыДтКт(&Начало, &Конец, , , , , , ) КАК Т",
        ),
        Vec::<String>::new(),
    );
    assert_eq!(
        missing_tables_in(
            config,
            "ВЫБРАТЬ Т.Период КАК П ИЗ РегистрБухгалтерии.Бухгалтерия3.ОборотыДтКт(&Начало, &Конец, , , , , , ) КАК Т",
        ),
        Vec::<String>::new(),
        "correspondence left unset is unknown, not a missing table",
    );
}

/// An ext-dimension count no chart of accounts can have is not trusted: the `Субконто<N>`
/// family is not expanded from it, and the table falls silent as if the chart were absent.
#[test]
fn an_implausible_ext_dimension_count_leaves_the_table_unjudged() {
    let mut config = Configuration::new("Тест");
    config.add_metadata_object(
        parse_chart_of_accounts_xml(
            &CHART_OF_ACCOUNTS.replace(
                "<MaxExtDimensionCount>3<",
                &format!("<MaxExtDimensionCount>{}<", u32::MAX),
            ),
        )
        .unwrap(),
    );
    config.add_register(parse_accounting_register_xml(ACCOUNTING_REGISTER).unwrap());
    let query = select_from(
        "РегистрБухгалтерии.Бухгалтерия1.Остатки(&Период, , , )",
        &["Субконто7", "НетТакогоПоля123"],
    );
    assert_eq!(unknown_fields_in(Arc::new(config), &query), Vec::<String>::new());
}

/// Every field an accounting virtual table derives from a resource is a read of that resource,
/// the unattested English split-balance spellings included: the graph links the read to
/// `Ресурс1`, which the register declares, not to a name built from it.
#[test]
fn derived_accounting_fields_are_reads_of_their_resource() {
    use crate::hir::SdblHir;

    for (source, field) in [
        ("РегистрБухгалтерии.Бухгалтерия1", "Ресурс2Дт"),
        ("РегистрБухгалтерии.Бухгалтерия1.Остатки(&Период, , , )", "Ресурс1ОстатокДт"),
        ("РегистрБухгалтерии.Бухгалтерия1.Остатки(&Период, , , )", "Ресурс1SplittedBalanceDr"),
        (
            "РегистрБухгалтерии.Бухгалтерия1.ОстаткиИОбороты(&Начало, &Конец, , , , , )",
            "Ресурс1OpeningExtendedBalanceCr",
        ),
        (
            "РегистрБухгалтерии.Бухгалтерия1.ОстаткиИОбороты(&Начало, &Конец, , , , , )",
            "Ресурс1ClosingExtendedBalanceDr",
        ),
        ("РегистрБухгалтерии.Бухгалтерия1.Обороты(&Начало, &Конец, , , , , , )", "Ресурс1ОборотДт"),
    ] {
        let query = select_from(source, &[field]);
        let package = lower_sdbl_to_hir(&parser::parse_sdbl(&query), Some(configuration(true)));
        let mut attrs = Vec::new();
        for q in package.queries() {
            SdblHir::collect_resolved_attributes(&q.hir, &mut attrs);
        }
        let names: Vec<&str> = attrs.iter().map(|(_, _, a)| a.as_str()).collect();
        let expected =
            if field.starts_with("Ресурс2") { "Ресурс2" } else { "Ресурс1" };
        assert_eq!(names, vec![expected], "{query}");
    }
}

mod nested_field_existence {
    //! `Алиас.Ссылочное.Поле…`: a hop past the first is reported only when it is provably absent
    //! from a field set that is known to be complete.

    use std::sync::Arc;

    use bsl_metadata::xml_parser::{
        parse_catalog_xml, parse_chart_of_characteristic_types_xml, parse_document_xml,
        parse_information_register_xml,
    };
    use bsl_metadata::Configuration;

    use crate::diagnostics::SdblDiagnostic;
    use crate::lower::lower_sdbl_to_hir;

    const HEADER: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:v8="http://v8.1c.ru/8.1/data/core" xmlns:xs="http://www.w3.org/2001/XMLSchema" xmlns:cfg="http://v8.1c.ru/8.1/data/enterprise/current-config" version="2.10">"#;

    const COUNTERPARTY: &str = r#"
    <Catalog uuid="00000000-0000-0000-0000-000000000010">
        <Properties><Name>Контрагенты</Name><CodeLength>9</CodeLength><DescriptionLength>100</DescriptionLength></Properties>
        <ChildObjects>
            <Attribute uuid="00000000-0000-0000-0000-000000000011">
                <Properties><Name>ИНН</Name><Type><v8:Type>xs:string</v8:Type></Type></Properties>
            </Attribute>
        </ChildObjects>
    </Catalog>"#;

    const ORDER: &str = r#"
    <Document uuid="00000000-0000-0000-0000-000000000020">
        <Properties><Name>Заказ</Name><NumberType>String</NumberType><NumberLength>11</NumberLength></Properties>
        <ChildObjects>
            <Attribute uuid="00000000-0000-0000-0000-000000000021">
                <Properties><Name>Контрагент</Name><Type><v8:Type>cfg:CatalogRef.Контрагенты</v8:Type></Type></Properties>
            </Attribute>
            <Attribute uuid="00000000-0000-0000-0000-000000000022">
                <Properties><Name>Основание</Name><Type><v8:Type>cfg:DocumentRef.Заказ</v8:Type></Type></Properties>
            </Attribute>
            <Attribute uuid="00000000-0000-0000-0000-000000000023">
                <Properties><Name>Составной</Name><Type><v8:Type>cfg:DocumentRef.Заказ</v8:Type><v8:Type>cfg:CatalogRef.Контрагенты</v8:Type></Type></Properties>
            </Attribute>
            <TabularSection uuid="00000000-0000-0000-0000-000000000024">
                <Properties><Name>Товары</Name></Properties>
                <ChildObjects>
                    <Attribute uuid="00000000-0000-0000-0000-000000000025">
                        <Properties><Name>Количество</Name><Type><v8:Type>xs:decimal</v8:Type></Type></Properties>
                    </Attribute>
                </ChildObjects>
            </TabularSection>
        </ChildObjects>
    </Document>"#;

    const QUEUE: &str = r#"
    <InformationRegister uuid="00000000-0000-0000-0000-000000000030">
        <Properties><Name>Очередь</Name><WriteMode>RecorderSubordinate</WriteMode><InformationRegisterPeriodicity>Nonperiodical</InformationRegisterPeriodicity></Properties>
        <ChildObjects>
            <Dimension uuid="00000000-0000-0000-0000-000000000031">
                <Properties><Name>Заказ</Name><Type><v8:Type>cfg:DocumentRef.Заказ</v8:Type></Type></Properties>
            </Dimension>
            <Dimension uuid="00000000-0000-0000-0000-000000000032">
                <Properties><Name>Составной</Name><Type><v8:Type>cfg:DocumentRef.Заказ</v8:Type><v8:Type>cfg:CatalogRef.Контрагенты</v8:Type></Type></Properties>
            </Dimension>
            <Dimension uuid="00000000-0000-0000-0000-000000000033">
                <Properties><Name>Потерянный</Name><Type><v8:Type>cfg:DocumentRef.НеСуществует</v8:Type></Type></Properties>
            </Dimension>
            <Dimension uuid="00000000-0000-0000-0000-000000000035">
                <Properties><Name>Значение</Name><Type><v8:TypeSet>cfg:Characteristic.Виды</v8:TypeSet></Type></Properties>
            </Dimension>
            <Dimension uuid="00000000-0000-0000-0000-000000000036">
                <Properties><Name>ВидСсылка</Name><Type><v8:Type>cfg:ChartOfCharacteristicTypesRef.Виды</v8:Type></Type></Properties>
            </Dimension>
            <Resource uuid="00000000-0000-0000-0000-000000000034">
                <Properties><Name>Ресурс1</Name><Type><v8:Type>xs:decimal</v8:Type></Type></Properties>
            </Resource>
        </ChildObjects>
    </InformationRegister>"#;

    const KINDS: &str = r#"
    <ChartOfCharacteristicTypes uuid="00000000-0000-0000-0000-000000000040">
        <Properties><Name>Виды</Name><CodeLength>9</CodeLength><DescriptionLength>100</DescriptionLength></Properties>
        <ChildObjects>
            <Attribute uuid="00000000-0000-0000-0000-000000000041">
                <Properties><Name>Описание</Name><Type><v8:Type>xs:string</v8:Type></Type></Properties>
            </Attribute>
        </ChildObjects>
    </ChartOfCharacteristicTypes>"#;

    fn configuration(counterparty_open: bool) -> Arc<Configuration> {
        let mut config = Configuration::new("Тест");
        let mut counterparty =
            parse_catalog_xml(&format!("{HEADER}{COUNTERPARTY}\n</MetaDataObject>")).unwrap();
        counterparty.common_attributes_open = counterparty_open;
        config.add_metadata_object(counterparty);
        config.add_metadata_object(
            parse_document_xml(&format!("{HEADER}{ORDER}\n</MetaDataObject>")).unwrap(),
        );
        config.add_metadata_object(
            parse_chart_of_characteristic_types_xml(&format!("{HEADER}{KINDS}\n</MetaDataObject>"))
                .unwrap(),
        );
        config.add_register(
            parse_information_register_xml(&format!("{HEADER}{QUEUE}\n</MetaDataObject>")).unwrap(),
        );
        Arc::new(config)
    }

    /// `(table, field, text under the reported range)` of every `UnknownField`.
    fn reports_in(config: Arc<Configuration>, expression: &str) -> Vec<(String, String, String)> {
        let query = format!("ВЫБРАТЬ {expression} КАК П ИЗ РегистрСведений.Очередь КАК О");
        let ast = parser::parse_sdbl(&query);
        lower_sdbl_to_hir(&ast, Some(config))
            .all_diagnostics()
            .filter_map(|diag| match diag {
                SdblDiagnostic::UnknownField { table_name, field_name, range } => Some((
                    table_name.clone(),
                    field_name.clone(),
                    query[usize::from(range.start())..usize::from(range.end())].to_string(),
                )),
                _ => None,
            })
            .collect()
    }

    fn reports(expression: &str) -> Vec<(String, String, String)> {
        reports_in(configuration(false), expression)
    }

    fn triple(table: &str, field: &str) -> Vec<(String, String, String)> {
        vec![(table.to_string(), field.to_string(), field.to_string())]
    }

    #[test]
    fn a_missing_field_behind_a_reference_is_reported_once() {
        assert_eq!(reports("О.Заказ.НетТакогоПоля"), triple("Документ.Заказ", "НетТакогоПоля"));
    }

    #[test]
    fn an_existing_attribute_behind_a_reference_is_silent() {
        assert!(reports("О.Заказ.Контрагент").is_empty());
        assert!(reports("О.Заказ.Контрагент.ИНН").is_empty());
    }

    #[test]
    fn standard_attributes_of_the_referenced_object_are_silent() {
        for field in ["Ссылка", "Номер", "Дата", "ПометкаУдаления", "Проведен"]
        {
            assert!(reports(&format!("О.Заказ.{field}")).is_empty(), "{field}");
        }
        for field in ["Ссылка", "Код", "Наименование", "ПометкаУдаления"]
        {
            assert!(reports(&format!("О.Заказ.Контрагент.{field}")).is_empty(), "{field}");
        }
    }

    #[test]
    fn virtual_query_fields_of_the_referenced_object_are_silent() {
        for field in ["Представление", "МоментВремени"] {
            assert!(reports(&format!("О.Заказ.{field}")).is_empty(), "{field}");
        }
        assert!(reports("О.Заказ.Контрагент.Представление").is_empty());
    }

    #[test]
    fn english_spelling_of_the_hop_is_accepted() {
        assert!(reports("О.Заказ.Date").is_empty());
        assert!(reports("О.Заказ.Ref").is_empty());
    }

    #[test]
    fn a_composite_typed_hop_stops_the_walk() {
        assert!(reports("О.Составной.ЧтоУгодно").is_empty());
        assert!(reports("О.Заказ.Составной.ЧтоУгодно").is_empty());
    }

    #[test]
    fn a_missing_field_in_a_tabular_section_is_reported() {
        assert_eq!(
            reports("О.Заказ.Товары.НетКолонки"),
            triple("Документ.Заказ.Товары", "НетКолонки")
        );
    }

    #[test]
    fn existing_columns_of_a_tabular_section_are_silent() {
        for column in ["Количество", "НомерСтроки", "Ссылка"] {
            assert!(reports(&format!("О.Заказ.Товары.{column}")).is_empty(), "{column}");
        }
    }

    #[test]
    fn only_the_last_of_three_hops_is_reported() {
        assert_eq!(reports("О.Заказ.Основание.Нету"), triple("Документ.Заказ", "Нету"));
        assert_eq!(reports("О.Заказ.Основание.Основание.Нету"), triple("Документ.Заказ", "Нету"));
        assert_eq!(reports("О.Заказ.Контрагент.Нету"), triple("Справочник.Контрагенты", "Нету"));
    }

    #[test]
    fn the_first_miss_stops_the_walk() {
        assert_eq!(reports("О.Заказ.Нету1.Нету2"), triple("Документ.Заказ", "Нету1"),);
    }

    #[test]
    fn a_missing_first_hop_is_reported_once_not_twice() {
        let found = reports("О.НетПервого.Ещё.Поле");
        assert_eq!(found, triple("РегистрСведений.Очередь", "НетПервого"));
    }

    #[test]
    fn an_object_with_open_common_attributes_is_not_judged() {
        assert!(reports_in(configuration(true), "О.Заказ.Контрагент.Нету").is_empty());
        assert_eq!(
            reports_in(configuration(true), "О.Заказ.Нету"),
            triple("Документ.Заказ", "Нету"),
            "the document itself is still complete",
        );
    }

    #[test]
    fn a_reference_to_an_object_that_does_not_resolve_is_silent() {
        assert!(reports("О.Потерянный.Нету").is_empty());
    }
    fn reports_query(query: &str) -> Vec<(String, String)> {
        let ast = parser::parse_sdbl(query);
        lower_sdbl_to_hir(&ast, Some(configuration(false)))
            .all_diagnostics()
            .filter_map(|diag| match diag {
                SdblDiagnostic::UnknownField { table_name, field_name, .. } => {
                    Some((table_name.clone(), field_name.clone()))
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_characteristic_value_is_not_walked_into() {
        // `cfg:Characteristic.Виды` is an open type: any reference the chart allows.
        assert!(reports("О.Значение.Нету").is_empty());
        assert!(reports("О.Значение.Описание").is_empty());
        // The chart is modelled as a reference to itself, so a genuine reference is silent too.
        assert!(reports("О.ВидСсылка.Нету").is_empty());
    }

    #[test]
    fn a_column_of_a_temporary_table_is_not_walked_from() {
        // `ВЫРАЗИТЬ(... КАК Т).Поле` is typed as `Т`, so a walk from it would look for `Код` on
        // the document instead of the counterparty.
        let query = "ВЫБРАТЬ ВЫРАЗИТЬ(О.Заказ КАК Документ.Заказ).Контрагент КАК Контр \
                     ПОМЕСТИТЬ ВТ ИЗ РегистрСведений.Очередь КАК О; \
                     ВЫБРАТЬ Вр.Контр.Код КАК П ИЗ ВТ КАК Вр";
        assert_eq!(reports_query(query), Vec::<(String, String)>::new());
    }

    #[test]
    fn an_inferred_column_of_a_temporary_table_is_not_walked_from() {
        let query = "ВЫБРАТЬ О.Заказ КАК Заказ ПОМЕСТИТЬ ВТ ИЗ РегистрСведений.Очередь КАК О; \
                     ВЫБРАТЬ Вр.Заказ.Нету КАК Н ИЗ ВТ КАК Вр";
        assert_eq!(reports_query(query), Vec::<(String, String)>::new());
    }

    #[test]
    fn a_column_of_a_nested_query_is_not_walked_from() {
        let query = "ВЫБРАТЬ Вр.Заказ.Нету КАК Н ИЗ \
                     (ВЫБРАТЬ О.Заказ КАК Заказ ИЗ РегистрСведений.Очередь КАК О) КАК Вр";
        assert_eq!(reports_query(query), Vec::<(String, String)>::new());
    }
    #[test]
    fn a_declared_attribute_is_still_walked_from() {
        let query = "ВЫБРАТЬ О.Заказ.Контрагент.Нету КАК П ИЗ РегистрСведений.Очередь КАК О";
        assert_eq!(
            reports_query(query),
            vec![("Справочник.Контрагенты".to_string(), "Нету".to_string())]
        );
    }
}
