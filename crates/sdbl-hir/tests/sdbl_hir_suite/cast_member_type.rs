//! The type of `ВЫРАЗИТЬ(... КАК Документ.Х).Поле`: the field read after the cast decides it,
//! the cast target alone does not.

use std::sync::Arc;

use bsl_metadata::{Attribute, AttributeType, Configuration, MdoType, MetadataObject};
use sdbl_hir::{lower_sdbl_to_hir, ExprHir, FunctionKind, MdoRef, SdblType};

fn attribute(name: &str, attr_type: AttributeType) -> Attribute {
    Attribute { name: name.into(), name_en: None, attr_type }
}

fn catalog_ref(name: &str) -> AttributeType {
    AttributeType::Ref { mdo_type: MdoType::Catalog, name: name.into() }
}

fn string_type() -> AttributeType {
    AttributeType::String { length: Some(10) }
}

fn object(mdo_type: MdoType, name: &str, attributes: Vec<(&str, AttributeType)>) -> MetadataObject {
    let mut object = MetadataObject::new(mdo_type, name);
    for (name, attr_type) in attributes {
        object.add_attribute(attribute(name, attr_type));
    }
    object
}

/// `Заказ.Контрагент` is a reference to `Контрагенты`, whose `Страна` is a reference to
/// `Страны`; `Заказ.Номер` and `Страны.Название` are strings.
fn configuration() -> Configuration {
    let mut config = Configuration::new("TestConfig");
    config.add_metadata_object(object(
        MdoType::Document,
        "Заказ",
        vec![("Контрагент", catalog_ref("Контрагенты")), ("Номер", string_type())],
    ));
    config.add_metadata_object(object(
        MdoType::Catalog,
        "Контрагенты",
        vec![("Страна", catalog_ref("Страны"))],
    ));
    config.add_metadata_object(object(
        MdoType::Catalog,
        "Страны",
        vec![("Название", string_type())],
    ));
    config
}

/// The first selected expression of `ВЫБРАТЬ <expression> КАК Р ИЗ Справочник.Страны КАК Т`.
fn cast_call(config: Option<Configuration>, expression: &str) -> (SdblType, Option<MdoRef>) {
    let query = format!("ВЫБРАТЬ {expression} КАК Р ИЗ Справочник.Страны КАК Т");
    let parse = parser::parse_sdbl(&query);
    assert!(parse.errors().is_empty(), "{query}: {:?}", parse.errors());
    let package = lower_sdbl_to_hir(&parse, config.map(Arc::new));
    match &package.queries()[0].hir.select.fields[0].expr {
        ExprHir::FunctionCall { function: FunctionKind::Cast, cast_target, ty, .. } => {
            (ty.clone(), cast_target.clone())
        }
        other => panic!("{query}: not a cast call: {other:?}"),
    }
}

fn reference(mdo_type: MdoType, name: &str) -> SdblType {
    SdblType::reference(mdo_type, name)
}

fn string_sdbl_type() -> SdblType {
    SdblType::from_attribute_type(&string_type())
}

#[test]
fn a_field_after_the_cast_gives_the_field_type_not_the_cast_target() {
    let (ty, _) =
        cast_call(Some(configuration()), "ВЫРАЗИТЬ(Т.Регистратор КАК Документ.Заказ).Контрагент");
    assert_eq!(ty, reference(MdoType::Catalog, "Контрагенты"));
    assert_ne!(ty, reference(MdoType::Document, "Заказ"));

    let (ty, _) =
        cast_call(Some(configuration()), "ВЫРАЗИТЬ(Т.Регистратор КАК Документ.Заказ).Номер");
    assert_eq!(ty, string_sdbl_type(), "a primitive field surfaces as itself");
}

#[test]
fn a_chain_of_two_fields_walks_through_the_reference() {
    let (ty, _) = cast_call(
        Some(configuration()),
        "ВЫРАЗИТЬ(Т.Регистратор КАК Документ.Заказ).Контрагент.Страна",
    );
    assert_eq!(ty, reference(MdoType::Catalog, "Страны"));

    let (ty, _) = cast_call(
        Some(configuration()),
        "ВЫРАЗИТЬ(Т.Регистратор КАК Документ.Заказ).Контрагент.Страна.Название",
    );
    assert_eq!(ty, string_sdbl_type());
}

#[test]
fn a_name_the_target_does_not_have_is_unknown() {
    for expression in [
        "ВЫРАЗИТЬ(Т.Регистратор КАК Документ.Заказ).НетТакого",
        "ВЫРАЗИТЬ(Т.Регистратор КАК Документ.Заказ).Контрагент.НетТакого",
        "ВЫРАЗИТЬ(Т.Регистратор КАК Документ.Заказ).Номер.Длина",
    ] {
        let (ty, _) = cast_call(Some(configuration()), expression);
        assert_eq!(ty, SdblType::Unknown, "{expression}");
    }
}

#[test]
fn a_cast_without_field_access_keeps_the_cast_target_as_its_type() {
    let (ty, target) =
        cast_call(Some(configuration()), "ВЫРАЗИТЬ(Т.Регистратор КАК Документ.Заказ)");
    assert_eq!(ty, reference(MdoType::Document, "Заказ"));
    assert_eq!(target, Some(MdoRef { mdo_type: MdoType::Document, name: "Заказ".into() }));
}

#[test]
fn the_cast_target_survives_a_field_access() {
    let (ty, target) = cast_call(
        Some(configuration()),
        "ВЫРАЗИТЬ(Т.Регистратор КАК Документ.Заказ).Контрагент.Страна",
    );
    assert_eq!(ty, reference(MdoType::Catalog, "Страны"));
    assert_eq!(
        target,
        Some(MdoRef { mdo_type: MdoType::Document, name: "Заказ".into() }),
        "the cast target is not replaced by the field's type"
    );

    let (ty, target) =
        cast_call(Some(configuration()), "ВЫРАЗИТЬ(Т.Регистратор КАК Документ.Заказ).НетТакого");
    assert_eq!(ty, SdblType::Unknown);
    assert_eq!(target, Some(MdoRef { mdo_type: MdoType::Document, name: "Заказ".into() }));
}

#[test]
fn a_target_without_metadata_gives_unknown_not_a_confident_type() {
    let expression = "ВЫРАЗИТЬ(Т.Регистратор КАК Документ.Заказ).Контрагент";
    let (ty, target) = cast_call(None, expression);
    assert_eq!(ty, SdblType::Unknown, "no configuration at all");
    assert_eq!(target, Some(MdoRef { mdo_type: MdoType::Document, name: "Заказ".into() }));

    let (ty, _) = cast_call(
        Some(configuration()),
        "ВЫРАЗИТЬ(Т.Регистратор КАК Документ.НетТакого).Контрагент",
    );
    assert_eq!(ty, SdblType::Unknown, "the target is absent from the configuration");

    let (ty, _) = cast_call(Some(configuration()), "ВЫРАЗИТЬ(Т.Номер КАК Строка(10)).Длина");
    assert_eq!(ty, SdblType::Unknown, "a primitive target has no fields");
}

#[test]
fn virtual_query_fields_after_cast_keep_their_types() {
    for expression in [
        "ВЫРАЗИТЬ(Т.Регистратор КАК Документ.Заказ).Представление",
        "ВЫРАЗИТЬ(Т.Регистратор КАК Документ.Заказ).Presentation",
        "ВЫРАЗИТЬ(Т.Регистратор КАК Документ.Заказ).ВерсияДанных",
        "ВЫРАЗИТЬ(Т.Регистратор КАК Документ.Заказ).DataVersion",
        "ВЫРАЗИТЬ(Т.Регистратор КАК Документ.Заказ).Контрагент.Представление",
    ] {
        let (ty, _) = cast_call(Some(configuration()), expression);
        assert_eq!(ty, SdblType::string(), "{expression}");
    }

    for expression in [
        "ВЫРАЗИТЬ(Т.Регистратор КАК Документ.Заказ).МоментВремени",
        "ВЫРАЗИТЬ(Т.Регистратор КАК Документ.Заказ).PointInTime",
    ] {
        let (ty, _) = cast_call(Some(configuration()), expression);
        assert_eq!(ty, SdblType::DateTime, "{expression}");
    }

    let (ty, _) = cast_call(
        Some(configuration()),
        "ВЫРАЗИТЬ(Т.Регистратор КАК Справочник.Контрагенты).МоментВремени",
    );
    assert_eq!(ty, SdblType::Unknown, "PointInTime belongs to documents only");

    let (ty, _) = cast_call(
        Some(configuration()),
        "ВЫРАЗИТЬ(Т.Регистратор КАК Документ.Заказ).Представление.Длина",
    );
    assert_eq!(ty, SdblType::Unknown, "cannot traverse past a primitive virtual field");
}
