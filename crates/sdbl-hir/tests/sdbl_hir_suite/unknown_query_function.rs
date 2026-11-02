//! A function name the query language does not have is reported; a real function that the
//! lowering merely has no dedicated kind for is not.

use sdbl_hir::{lower_sdbl_to_hir, SdblDiagnostic};

fn unknown_function_names(query: &str) -> Vec<String> {
    let package = lower_sdbl_to_hir(&parser::parse_sdbl(query), None);
    package
        .all_diagnostics()
        .filter_map(|diagnostic| match diagnostic {
            SdblDiagnostic::UnknownFunction { function_name, .. } => Some(function_name.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn a_typo_in_a_function_name_is_reported_once() {
    let names = unknown_function_names("ВЫБРАТЬ ПОДСТРКА(Т.Имя, 1, 2) ИЗ Справочник.Товары КАК Т");
    assert_eq!(names, ["ПОДСТРКА"]);
}

#[test]
fn an_invented_function_name_is_reported_once() {
    let names =
        unknown_function_names("ВЫБРАТЬ ВЫДУМАННАЯФУНКЦИЯ(Т.Имя) ИЗ Справочник.Товары КАК Т");
    assert_eq!(names, ["ВЫДУМАННАЯФУНКЦИЯ"]);
}

#[test]
fn real_functions_without_a_dedicated_kind_stay_silent() {
    for function in [
        "СокрЛП(Т.Имя)",
        "Цел(Т.Имя)",
        "Окр(Т.Имя)",
        "Sqrt(Т.Имя)",
        "Pow(Т.Имя, 2)",
        "Лев(Т.Имя, 2)",
        "Прав(Т.Имя, 2)",
        "УНИКАЛЬНЫЙИДЕНТИФИКАТОР(Т.Имя)",
        "ПРЕДСТАВЛЕНИЕССЫЛКИ(Т.Имя)",
        "ТИП(Справочник.Товары)",
    ] {
        let query = format!("ВЫБРАТЬ {function} КАК Поле ИЗ Справочник.Товары КАК Т");
        let names = unknown_function_names(&query);
        assert!(names.is_empty(), "{function} was reported as unknown: {names:?}");
    }
}

#[test]
fn a_metadata_root_or_period_name_called_as_a_function_is_reported() {
    for function in
        ["Справочник", "Catalog", "РегистрСведений", "ВнешнийИсточникДанных", "Декада", "HalfYear"]
    {
        let query = format!("ВЫБРАТЬ {function}(1) КАК Поле ИЗ Справочник.Товары КАК Т");
        assert_eq!(unknown_function_names(&query), [function], "{query}");
    }
}

#[test]
fn a_virtual_table_call_is_not_a_function_call() {
    let names = unknown_function_names(
        "ВЫБРАТЬ Т.Остаток ИЗ РегистрНакопления.ТоварыНаСкладах.Остатки(&Дата, Склад = &Склад) КАК Т",
    );
    assert!(names.is_empty(), "virtual table reported as unknown function: {names:?}");
}

#[test]
fn index_by_sets_is_clause_syntax_not_a_function_call() {
    let names = unknown_function_names(
        "ВЫБРАТЬ Т.А, Т.Б ПОМЕСТИТЬ ВТ ИЗ Справочник.Товары КАК Т ИНДЕКСИРОВАТЬ ПО НАБОРАМ ((А, Б), (Б))",
    );
    assert!(names.is_empty(), "the clause keyword reported as unknown: {names:?}");
}

#[test]
fn the_index_by_sets_spelling_is_still_a_typo_outside_that_clause() {
    let names = unknown_function_names("ВЫБРАТЬ НАБОРАМ(Т.Имя) ИЗ Справочник.Товары КАК Т");
    assert_eq!(names, ["НАБОРАМ"]);
}
