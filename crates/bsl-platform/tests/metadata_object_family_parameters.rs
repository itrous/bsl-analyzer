//! Parameters the help documents as "any metadata object" reach the extract cut
//! to the first five kinds of the family (`ОбъектМетаданных: ТаблицаИзмерения,
//! …: HTTPСервис`). The curated overlay restores the whole family; these checks
//! read the generated data, so they also prove the overlay reached the build.

use bsl_platform::{MethodParam, PlatformData};

/// The kinds the cut list keeps: the extract stops a type list at five entries,
/// and the help lists the family starting with these.
const CUT_FAMILY: &[&str] = &[
    "ОбъектМетаданных: ТаблицаИзмерения",
    "ОбъектМетаданных: ВнешнийИсточникДанных",
    "ОбъектМетаданных: Поле",
    "ОбъектМетаданных: Таблица",
    "ОбъектМетаданных: HTTPСервис",
];

/// Five alternatives (the cut length) whose description kinds are a prefix of
/// the cut family: the signature of a list the extract stopped short.
fn is_cut_list(raw: &str) -> bool {
    let alternatives = bsl_platform::split_type_alternatives(raw);
    let kinds: Vec<&str> =
        alternatives.iter().copied().filter(|alt| alt.starts_with("ОбъектМетаданных: ")).collect();
    alternatives.len() == CUT_FAMILY.len() && !kinds.is_empty() && CUT_FAMILY.starts_with(&kinds)
}

/// Every `ОбъектМетаданных: <Вид>` the extract knows, in its order.
fn family() -> String {
    let members: Vec<&str> = PlatformData::instance()
        .all_types()
        .iter()
        .map(|ty| ty.name.as_str())
        .filter(|name| name.starts_with("ОбъектМетаданных: "))
        .collect();
    assert!(members.len() > 60, "the family has some seventy kinds, got {members:?}");
    members.join(", ")
}

fn param_type(param: &MethodParam) -> &str {
    param.param_type.as_deref().unwrap_or_default()
}

fn method_param(type_name: &str, method: &str, index: usize) -> String {
    let method = PlatformData::instance()
        .get_method(type_name, method)
        .unwrap_or_else(|| panic!("{type_name}.{method} must exist"));
    param_type(&method.parameters[index]).to_owned()
}

fn global_param(function: &str, index: usize) -> String {
    let function = PlatformData::instance()
        .get_global_function(function)
        .unwrap_or_else(|| panic!("{function} must exist"));
    param_type(&function.parameters[index]).to_owned()
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn metadata_collection_membership_takes_any_metadata_object() {
    assert_eq!(method_param("MetadataObjectCollection", "Содержит", 0), family());
    assert_eq!(method_param("MetadataObjectCollection", "Индекс", 0), family());
    assert_eq!(method_param("MetadataObjectPropertyValueCollection", "Содержит", 0), family());
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn global_functions_take_any_metadata_object() {
    assert_eq!(global_param("ПравоДоступа", 1), family());
    assert_eq!(global_param("AccessRight", 1), family());
    assert_eq!(global_param("ВыполнитьПроверкуПравДоступа", 1), family());
    assert_eq!(global_param("ЗаписьЖурналаРегистрации", 2), family());
    assert_eq!(global_param("ОткрытьСправку", 0), family());
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn types_outside_the_family_keep_their_place() {
    assert_eq!(
        method_param("FunctionalOptionContent", "Найти", 0),
        format!("Строка, {}", family())
    );
    assert_eq!(
        method_param("ExchangePlansManager", "ВыбратьИзменения", 2),
        format!("Неопределено, {}", family())
    );
    assert_eq!(global_param("ОбновитьНумерациюОбъектов", 0), format!("Массив, {}", family()));
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn a_genuine_narrowing_is_left_as_extracted() {
    assert_eq!(
        global_param("ПравоДоступа", 2),
        "ПользовательИнформационнойБазы, ОбъектМетаданных: Роль"
    );
}

/// No flat parameter list keeps the cut family. The variant-shaped
/// `СоставОбщегоРеквизита.Найти/Содержит` carry the same list inside their
/// variants as well, which a flat parameter override does not reach.
#[test]
fn no_flat_parameter_keeps_the_cut_family() {
    let data = PlatformData::instance();
    let mut cut: Vec<String> = data
        .all_methods()
        .iter()
        .filter(|method| method.variants.is_empty())
        .flat_map(|method| {
            method.parameters.iter().filter(|param| is_cut_list(param_type(param))).map(
                move |param| format!("{}.{} {}", method.type_name, method.english_name, param.name),
            )
        })
        .collect();
    cut.extend(data.all_global_functions().iter().flat_map(|function| {
        function
            .parameters
            .iter()
            .filter(|param| is_cut_list(param_type(param)))
            .map(move |param| format!("{} {}", function.english_name, param.name))
    }));
    assert!(cut.is_empty(), "parameters still typed by the cut family: {cut:?}");
}
