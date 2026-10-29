//! `QueryNestedFieldsByDot` against metadata: a dotted path is a finding only when it
//! dereferences a field whose type is a composite reference (v8std #654 §1.2).
//!
//! Every silence below is paired with a path of the same shape that does fire, so a
//! configuration that failed to load cannot pass for a rule that holds its tongue.

use std::sync::Arc;

use bsl_metadata::{
    tabular_section::{TabularSection, TabularSectionAttribute},
    xml_parser::{parse_accumulation_register_xml, parse_information_register_xml},
    Attribute, AttributeType, Configuration, DefinedType, MdoType, MetadataObject, Uuid,
};
use sdbl_hir::{lower_sdbl_to_hir, SdblDiagnostic};

const NOMENCLATURE: &str = "cfg:CatalogRef.Номенклатура";
const WAREHOUSES: &str = "cfg:CatalogRef.Склады";
const PARTNERS: &str = "cfg:CatalogRef.Контрагенты";

fn reference(name: &str) -> AttributeType {
    AttributeType::Ref { mdo_type: MdoType::Catalog, name: name.into() }
}

fn defined(name: &str) -> AttributeType {
    AttributeType::DefinedType { name: name.into() }
}

fn composite(types: Vec<AttributeType>) -> AttributeType {
    AttributeType::Composite { types }
}

fn catalog(name: &str, attributes: Vec<(&str, AttributeType)>) -> MetadataObject {
    let mut object = MetadataObject::new(MdoType::Catalog, name);
    for (name, attr_type) in attributes {
        object.add_attribute(Attribute { name: name.into(), name_en: None, attr_type });
    }
    object
}

/// A dimension type written `set:X` is a `<v8:TypeSet>`, as the platform writes `AnyIBRef` and
/// a reference to every object of a kind; any other entry is a `<v8:Type>`.
fn register_xml(
    kind: &str,
    name: &str,
    properties: &str,
    dimensions: &[(&str, &[&str])],
) -> String {
    let dimensions: String = dimensions
        .iter()
        .enumerate()
        .map(|(idx, (name, types))| {
            let types: String = types
                .iter()
                .map(|ty| match ty.strip_prefix("set:") {
                    Some(set) => format!("<v8:TypeSet>{set}</v8:TypeSet>"),
                    None => format!("<v8:Type>{ty}</v8:Type>"),
                })
                .collect();
            format!(
                "<Dimension uuid=\"22222222-2222-2222-2222-{idx:012}\"><Properties><Name>{name}</Name><Type>{types}</Type></Properties></Dimension>"
            )
        })
        .collect();
    format!(
        r#"<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:cfg="http://v8.1c.ru/8.1/data/enterprise/current-config" xmlns:v8="http://v8.1c.ru/8.1/data/core" version="2.20">
<{kind} uuid="11111111-1111-1111-1111-111111111111"><Properties><Name>{name}</Name>{properties}</Properties>
<ChildObjects>{dimensions}</ChildObjects></{kind}></MetaDataObject>"#
    )
}

/// `Номенклатура.Поставщик` is a composite reference and `Номенклатура.Производитель` a single
/// one; `ЦеныОбъектов.Объект` and `Остатки.Партия` are composite, `ЦеныОбъектов.Номенклатура` and
/// `Остатки.Номенклатура` single. `Заказ.Товары` is a tabular section whose `Объект` is composite.
fn base_configuration() -> Configuration {
    let mut config = Configuration::new("TestConfig");
    for (name, underlying) in [
        ("СоставнойПоставщик", composite(vec![reference("Контрагенты"), reference("Склады")])),
        ("ОдинПроизводитель", reference("Контрагенты")),
    ] {
        config.add_defined_type(
            DefinedType::builder()
                .uuid(Uuid::new_v4())
                .name(name)
                .underlying_type(underlying)
                .build(),
        );
    }
    config.add_metadata_object(catalog("Контрагенты", vec![]));
    config.add_metadata_object(catalog("Склады", vec![]));
    config.add_metadata_object(catalog(
        "Номенклатура",
        vec![
            ("Поставщик", composite(vec![reference("Контрагенты"), reference("Склады")])),
            ("Производитель", reference("Контрагенты")),
            ("ПоставщикОпр", defined("СоставнойПоставщик")),
            ("ПроизводительОпр", defined("ОдинПроизводитель")),
            ("НеизвестноеОпр", defined("НетТакогоТипа")),
            ("Загадка", AttributeType::Unknown),
            (
                "Смесь",
                composite(vec![reference("Склады"), AttributeType::String { length: Some(5) }]),
            ),
            (
                "Числа",
                composite(vec![
                    AttributeType::String { length: Some(5) },
                    AttributeType::Number { precision: 10, scale: 0 },
                ]),
            ),
        ],
    ));

    let mut order = MetadataObject::new(MdoType::Document, "Заказ");
    let mut goods = TabularSection::new(Uuid::nil(), "Товары");
    goods.set_attributes(vec![
        TabularSectionAttribute::new(Uuid::nil(), "Номенклатура", reference("Номенклатура")),
        TabularSectionAttribute::new(
            Uuid::nil(),
            "Объект",
            composite(vec![reference("Номенклатура"), reference("Склады")]),
        ),
        TabularSectionAttribute::new(
            Uuid::nil(),
            "Количество",
            AttributeType::Number { precision: 10, scale: 0 },
        ),
    ]);
    order.add_tabular_section(goods);
    config.add_metadata_object(order);

    config.add_register(
        parse_information_register_xml(&register_xml(
            "InformationRegister",
            "ЦеныОбъектов",
            "",
            &[("Объект", &[NOMENCLATURE, WAREHOUSES]), ("Номенклатура", &[NOMENCLATURE])],
        ))
        .expect("information register fixture must parse"),
    );
    config.add_register(
        parse_accumulation_register_xml(&register_xml(
            "AccumulationRegister",
            "Остатки",
            "",
            &[
                ("Партия", &[NOMENCLATURE, PARTNERS]),
                ("Номенклатура", &[NOMENCLATURE]),
                ("Любая", &["set:cfg:AnyIBRef"]),
                ("ЛюбойСправочник", &["set:cfg:CatalogRef"]),
            ],
        ))
        .expect("accumulation register fixture must parse"),
    );
    config
}

/// Every finding of the query as the text it points at and its `parts_count`.
fn findings(config: Option<Configuration>, query: &str) -> Vec<(String, Option<u32>)> {
    let parse = parser::parse_sdbl(query);
    assert!(parse.errors().is_empty(), "{query}: {:?}", parse.errors());
    let package = lower_sdbl_to_hir(&parse, config.map(Arc::new));
    let mut found: Vec<_> = package
        .all_diagnostics()
        .filter_map(|d| match d {
            SdblDiagnostic::QueryNestedFieldsByDot { range, parts_count } => {
                Some((*range, *parts_count))
            }
            _ => None,
        })
        .collect();
    found.sort_by_key(|(range, _)| (range.start(), range.end()));
    found
        .into_iter()
        .map(|(range, depth)| {
            (query[usize::from(range.start())..usize::from(range.end())].to_string(), depth)
        })
        .collect()
}

fn path(text: &str, depth: Option<u32>) -> (String, Option<u32>) {
    (text.to_string(), depth)
}

#[test]
fn query_nested_fields_are_silent_where_the_metadata_does_not_establish_a_type() {
    let composite_path = "ВЫБРАТЬ Ц.Объект.Код КАК К ИЗ РегистрСведений.ЦеныОбъектов КАК Ц";
    assert_eq!(
        findings(Some(base_configuration()), composite_path),
        vec![path("Ц.Объект.Код", Some(3))],
        "the control: the same path fires once the metadata is there"
    );

    for (label, config, query) in [
        ("no metadata", None, composite_path),
        (
            "table is absent from the metadata",
            Some(base_configuration()),
            "ВЫБРАТЬ Н.Объект.Код КАК К ИЗ Справочник.НетТакого КАК Н",
        ),
        (
            "field is absent from the table",
            Some(base_configuration()),
            "ВЫБРАТЬ Ц.НетТакого.Код КАК К ИЗ РегистрСведений.ЦеныОбъектов КАК Ц",
        ),
        (
            "field type is Unknown",
            Some(base_configuration()),
            "ВЫБРАТЬ Н.Загадка.Код КАК К ИЗ Справочник.Номенклатура КАК Н",
        ),
        (
            "defined type is not in the metadata",
            Some(base_configuration()),
            "ВЫБРАТЬ Н.НеизвестноеОпр.Код КАК К ИЗ Справочник.Номенклатура КАК Н",
        ),
    ] {
        assert_eq!(findings(config, query), vec![], "[{label}] {query}");
    }
}

#[test]
fn query_nested_fields_tell_a_composite_with_a_reference_from_one_without() {
    assert_eq!(
        findings(
            Some(base_configuration()),
            "ВЫБРАТЬ Н.Смесь.Код КАК К1, Н.Числа.Длина КАК К2, Н.Производитель.Код КАК К3, Н.Поставщик.Код КАК К4 \
             ИЗ Справочник.Номенклатура КАК Н"
        ),
        vec![path("Н.Смесь.Код", Some(3)), path("Н.Поставщик.Код", Some(3))],
        "a reference and a string fire, a string and a number and a single reference do not"
    );
}

#[test]
fn query_nested_fields_report_the_recorder_of_an_accumulation_register() {
    let config = base_configuration();
    assert_eq!(
        findings(
            Some(config.clone()),
            "ВЫБРАТЬ Продажи.Регистратор.Дата КАК Д ИЗ РегистрНакопления.Остатки КАК Продажи"
        ),
        vec![path("Продажи.Регистратор.Дата", Some(3))]
    );
    assert_eq!(
        findings(
            Some(config.clone()),
            "SELECT Sales.Recorder.Date AS D FROM AccumulationRegister.Остатки AS Sales"
        ),
        vec![path("Sales.Recorder.Date", Some(3))]
    );
    assert_eq!(
        findings(
            Some(config),
            "ВЫБРАТЬ Остатки.Номенклатура.Наименование КАК Н ИЗ РегистрНакопления.Остатки КАК Остатки"
        ),
        vec![],
        "a single-reference dimension of the same register stays silent"
    );
}

#[test]
fn query_nested_fields_report_a_field_that_may_hold_any_reference() {
    assert_eq!(
        findings(
            Some(base_configuration()),
            "ВЫБРАТЬ О.Любая.Код КАК К1, О.ЛюбойСправочник.Код КАК К2, О.Номенклатура.Код КАК К3 \
             ИЗ РегистрНакопления.Остатки КАК О"
        ),
        vec![path("О.Любая.Код", Some(3)), path("О.ЛюбойСправочник.Код", Some(3))],
        "AnyIBRef and a catalog reference without a name fire, one named catalog does not"
    );
}

#[test]
fn query_nested_fields_are_silent_on_the_recorder_of_an_information_register() {
    for write_mode in ["Independent", "RecorderSubordinate"] {
        let mut config = base_configuration();
        config.add_register(
            parse_information_register_xml(&register_xml(
                "InformationRegister",
                "Курсы",
                &format!(
                    "<InformationRegisterPeriodicity>Second</InformationRegisterPeriodicity><WriteMode>{write_mode}</WriteMode>"
                ),
                &[("Объект", &[NOMENCLATURE, WAREHOUSES])],
            ))
            .expect("information register fixture must parse"),
        );

        let on_alias = |field: &str| {
            findings(
                Some(config.clone()),
                &format!("ВЫБРАТЬ К.{field} КАК Д ИЗ РегистрСведений.Курсы КАК К"),
            )
        };
        assert_eq!(on_alias("Регистратор.Дата"), vec![], "[{write_mode}] alias path");
        assert_eq!(
            on_alias("Объект.Код"),
            vec![path("К.Объект.Код", Some(3))],
            "[{write_mode}] the control: a dimension of the same register fires"
        );

        let in_slice = |field: &str| {
            findings(
                Some(config.clone()),
                &format!(
                    "ВЫБРАТЬ С.Объект КАК О ИЗ РегистрСведений.Курсы.СрезПоследних(, {field} = &Д) КАК С"
                ),
            )
        };
        assert_eq!(in_slice("Регистратор.Дата"), vec![], "[{write_mode}] slice parameter");
        assert_eq!(
            in_slice("Объект.Код"),
            vec![path("Объект.Код", None)],
            "[{write_mode}] the control: a dimension in the slice parameter fires"
        );
    }
}

#[test]
fn query_nested_fields_expand_a_defined_type_to_the_references_it_holds() {
    let config = base_configuration();
    assert_eq!(
        findings(
            Some(config.clone()),
            "ВЫБРАТЬ Н.ПоставщикОпр.Код КАК К1, Н.ПроизводительОпр.Код КАК К2 ИЗ Справочник.Номенклатура КАК Н"
        ),
        vec![path("Н.ПоставщикОпр.Код", Some(3))],
        "a defined type of composite references fires, one of a single reference does not"
    );

    // `Номенклатура` is one reference and the defined types are fields of the catalog it
    // points to, so the field type comes through the reference without its underlying type.
    let mut through_reference = config;
    through_reference.add_register(
        parse_accumulation_register_xml(&register_xml(
            "AccumulationRegister",
            "Закупки",
            "",
            &[("Номенклатура", &[NOMENCLATURE])],
        ))
        .expect("accumulation register fixture must parse"),
    );
    assert_eq!(
        findings(
            Some(through_reference),
            "ВЫБРАТЬ З.Номенклатура.ПоставщикОпр.Код КАК К1, З.Номенклатура.ПроизводительОпр.Код КАК К2, \
             З.Номенклатура.НеизвестноеОпр.Код КАК К3 ИЗ РегистрНакопления.Закупки КАК З"
        ),
        vec![path("З.Номенклатура.ПоставщикОпр.Код", Some(4))],
        "through a reference: composite fires, single reference and a missing defined type do not"
    );
}

#[test]
fn query_nested_fields_look_past_the_first_field_of_a_tabular_section() {
    let config = base_configuration();
    assert_eq!(
        findings(
            Some(config.clone()),
            "ВЫБРАТЬ Т.Товары.Объект.Код КАК К1, Т.Товары.Номенклатура.Код КАК К2 ИЗ Документ.Заказ КАК Т"
        ),
        vec![path("Т.Товары.Объект.Код", Some(4))]
    );
    assert_eq!(
        findings(
            Some(config),
            "ВЫБРАТЬ Т.Товары.(Номенклатура, Количество) ИЗ Документ.Заказ КАК Т"
        ),
        vec![],
        "an inline field list of a tabular section is not a dereference"
    );
}

#[test]
fn query_nested_fields_a_cast_or_a_reference_test_does_not_silence_the_path_inside() {
    let config = base_configuration();
    let register = "ИЗ РегистрСведений.ЦеныОбъектов КАК Цены";
    assert_eq!(
        findings(
            Some(config.clone()),
            &format!("ВЫБРАТЬ ВЫРАЗИТЬ(Цены.Объект.Код КАК Строка(10)) КАК К {register}")
        ),
        vec![path("Цены.Объект.Код", Some(3))],
        "the path is inside the cast argument, not after the cast"
    );
    assert_eq!(
        findings(
            Some(config),
            &format!(
                "ВЫБРАТЬ Цены.Объект.Код КАК К {register} ГДЕ Цены.Объект ССЫЛКА Справочник.Номенклатура"
            )
        ),
        vec![path("Цены.Объект.Код", Some(3))],
        "ССЫЛКА on the field does not stand for ВЫРАЗИТЬ on the path"
    );
}

#[test]
fn query_nested_fields_after_a_cast_need_a_composite_member_before_the_last() {
    let config = base_configuration();
    let register = "ИЗ РегистрСведений.ЦеныОбъектов КАК Цены";
    for (label, query, expected) in [
        (
            "composite member",
            format!(
                "ВЫБРАТЬ ВЫРАЗИТЬ(Цены.Объект КАК Справочник.Номенклатура).Поставщик.Код КАК К {register}"
            ),
            vec![path("ВЫРАЗИТЬ(Цены.Объект КАК Справочник.Номенклатура).Поставщик.Код", None)],
        ),
        (
            "composite member, English spelling",
            "SELECT CAST(Цены.Объект AS Catalog.Номенклатура).Поставщик.Код AS К FROM InformationRegister.ЦеныОбъектов AS Цены".to_string(),
            vec![path("CAST(Цены.Объект AS Catalog.Номенклатура).Поставщик.Код", None)],
        ),
        (
            "member names compared without case",
            format!(
                "ВЫБРАТЬ ВЫРАЗИТЬ(Цены.Объект КАК Справочник.Номенклатура).поставщик.Код КАК К {register}"
            ),
            vec![path("ВЫРАЗИТЬ(Цены.Объект КАК Справочник.Номенклатура).поставщик.Код", None)],
        ),
        (
            "single reference member",
            format!(
                "ВЫБРАТЬ ВЫРАЗИТЬ(Цены.Объект КАК Справочник.Номенклатура).Производитель.Код КАК К {register}"
            ),
            vec![],
        ),
        (
            "one member after the cast",
            format!(
                "ВЫБРАТЬ ВЫРАЗИТЬ(Цены.Объект КАК Справочник.Номенклатура).Поставщик КАК К {register}"
            ),
            vec![],
        ),
        (
            "cast target is not an object of the metadata",
            format!(
                "ВЫБРАТЬ ВЫРАЗИТЬ(Цены.Объект КАК Справочник.НетТакого).Поставщик.Код КАК К {register}"
            ),
            vec![],
        ),
        (
            "cast to a primitive type",
            format!("ВЫБРАТЬ ВЫРАЗИТЬ(Цены.Объект КАК Строка(10)).Поставщик.Код КАК К {register}"),
            vec![],
        ),
    ] {
        assert_eq!(findings(Some(config.clone()), &query), expected, "[{label}] {query}");
    }
}

#[test]
fn query_nested_fields_in_a_virtual_table_parameter_need_a_composite_field() {
    let config = base_configuration();
    let table = "РегистрНакопления.Остатки.Остатки";
    assert_eq!(
        findings(
            Some(config.clone()),
            &format!("ВЫБРАТЬ О.Партия КАК П ИЗ {table}(, Партия.Код = &К) КАК О")
        ),
        vec![path("Партия.Код", None)]
    );
    assert_eq!(
        findings(
            Some(config),
            &format!("ВЫБРАТЬ О.Партия КАК П ИЗ {table}(, Номенклатура.Наименование = &К) КАК О")
        ),
        vec![],
        "a dimension of one reference type in the same parameter position stays silent"
    );
}

#[test]
fn query_nested_fields_follow_a_field_through_a_temporary_table() {
    let config = base_configuration();
    assert_eq!(
        findings(
            Some(config.clone()),
            "ВЫБРАТЬ Ц.Объект КАК Объект ПОМЕСТИТЬ ВТ ИЗ РегистрСведений.ЦеныОбъектов КАК Ц; \
             ВЫБРАТЬ Вр.Объект.Код КАК Код ИЗ ВТ КАК Вр"
        ),
        vec![path("Вр.Объект.Код", Some(3))]
    );
    assert_eq!(
        findings(
            Some(config),
            "ВЫБРАТЬ Ц.Номенклатура КАК Объект ПОМЕСТИТЬ ВТ ИЗ РегистрСведений.ЦеныОбъектов КАК Ц; \
             ВЫБРАТЬ Вр.Объект.Код КАК Код ИЗ ВТ КАК Вр"
        ),
        vec![],
        "the temporary table carries a single reference"
    );
}

#[test]
fn query_nested_fields_in_every_union_branch_are_reported_once_each() {
    assert_eq!(
        findings(
            Some(base_configuration()),
            "ВЫБРАТЬ Ц.Объект.Код КАК К ИЗ РегистрСведений.ЦеныОбъектов КАК Ц \
             ОБЪЕДИНИТЬ ВСЕ ВЫБРАТЬ Ц.Объект.Код КАК К ИЗ РегистрСведений.ЦеныОбъектов КАК Ц"
        ),
        vec![path("Ц.Объект.Код", Some(3)), path("Ц.Объект.Код", Some(3))]
    );
}
