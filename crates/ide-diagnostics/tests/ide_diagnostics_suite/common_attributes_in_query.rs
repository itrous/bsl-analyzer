//! Common attributes (`ОбщиеРеквизиты`) are columns of every table whose object is in their
//! composition, although the object's own XML never mentions them.
//!
//! The fixture declares three of them: the data separator `ОбластьДанныхОсновныеДанные`
//! (`AutoUse = Use`, excluded from `СправочникБезРазделения`), the plain `Организация`
//! (`AutoUse = DontUse`, listed for `Справочник1`, `Документ1`, `РегистрСведений1`) and the plain
//! `ОбщийКомментарий` (`AutoUse = Use`, excluded from `Документ1`).

use ide_diagnostics::{DiagnosticCode, DiagnosticsConfig};
use std::path::PathBuf;

const FIXTURE: &str =
    concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-metadata/fixtures/common_attributes");

fn findings(query: &str) -> Vec<(String, String)> {
    let config = DiagnosticsConfig::all_enabled();
    let configuration =
        bsl_metadata::load_from_directory(PathBuf::from(FIXTURE)).expect("the fixture loads");

    ide_diagnostics::validate_query_text(&config, Some(&configuration), query)
        .into_iter()
        .map(|d| (d.code.as_str().to_string(), d.message))
        .collect()
}

fn unknown_fields(query: &str) -> Vec<String> {
    findings(query)
        .into_iter()
        .filter(|(code, _)| code == DiagnosticCode::UnknownFieldInQuery.as_str())
        .map(|(_, message)| message)
        .collect()
}

fn assert_known(query: &str) {
    let found = findings(query);
    assert!(
        !found.iter().any(|(code, _)| code == DiagnosticCode::UnknownFieldInQuery.as_str()
            || code == DiagnosticCode::QueryToMissingMetadata.as_str()),
        "every column of\n{query}\nexists, got {found:?}"
    );
}

fn assert_unknown(query: &str, field: &str) {
    let found = unknown_fields(query);
    assert!(
        found.iter().any(|message| message.contains(field)),
        "`{field}` is not a column of\n{query}\nyet nothing reported it: {:?}",
        findings(query)
    );
}

#[test]
fn a_common_attribute_is_a_column_of_every_object_in_its_composition() {
    assert_known(
        "ВЫБРАТЬ Т.Реквизит1, Т.Организация, Т.ОбластьДанныхОсновныеДанные, Т.ОбщийКомментарий \
         ИЗ Справочник.Справочник1 КАК Т",
    );
    assert_known(
        "ВЫБРАТЬ Т.Организация, Т.ОбластьДанныхОсновныеДанные ИЗ Документ.Документ1 КАК Т",
    );
}

#[test]
fn an_object_excluded_from_the_composition_has_no_such_column() {
    assert_unknown(
        "ВЫБРАТЬ Т.Организация ИЗ Справочник.СправочникБезРазделения КАК Т",
        "Организация",
    );
    assert_unknown(
        "ВЫБРАТЬ Т.ОбластьДанныхОсновныеДанные ИЗ Справочник.СправочникБезРазделения КАК Т",
        "ОбластьДанныхОсновныеДанные",
    );
}

#[test]
fn auto_use_reaches_unlisted_objects_but_not_excluded_ones() {
    assert_known("ВЫБРАТЬ Т.ОбщийКомментарий ИЗ Справочник.СправочникБезРазделения КАК Т");
    assert_unknown("ВЫБРАТЬ Т.ОбщийКомментарий ИЗ Документ.Документ1 КАК Т", "ОбщийКомментарий");
}

#[test]
fn a_separator_is_a_dimension_of_every_register_table() {
    for source in [
        "РегистрНакопления.РегистрНакопления1",
        "РегистрНакопления.РегистрНакопления1.Остатки",
        "РегистрНакопления.РегистрНакопления1.Обороты",
        "РегистрНакопления.РегистрНакопления1.ОстаткиИОбороты",
        "РегистрСведений.РегистрСведений1",
        "РегистрСведений.РегистрСведений1.СрезПоследних",
    ] {
        assert_known(&format!(
            "ВЫБРАТЬ Р.ОбластьДанныхОсновныеДанные, Р.Измерение1 ИЗ {source} КАК Р"
        ));
    }
    assert_known(
        "ВЫБРАТЬ Р.Измерение1 \
         ИЗ РегистрНакопления.РегистрНакопления1.Остатки(, ОбластьДанныхОсновныеДанные = 1) КАК Р",
    );
}

#[test]
fn a_plain_common_attribute_of_a_register_is_an_attribute() {
    assert_known("ВЫБРАТЬ Р.ОбщийКомментарий ИЗ РегистрНакопления.РегистрНакопления1 КАК Р");
    assert_known(
        "ВЫБРАТЬ Р.Организация, Р.ОбщийКомментарий ИЗ РегистрСведений.РегистрСведений1 КАК Р",
    );
    assert_unknown(
        "ВЫБРАТЬ Р.Организация ИЗ РегистрНакопления.РегистрНакопления1 КАК Р",
        "Организация",
    );
}
