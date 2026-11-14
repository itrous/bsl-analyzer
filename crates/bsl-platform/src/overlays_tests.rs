use super::*;
use crate::snapshot::{AttestedSurfaces, MemberSurface};
use serde_json::json;

fn base_data() -> Value {
    json!({
        "methods": [
            {
                "id": 1,
                "type_name": "DOMDocument",
                "name": "ДобавитьДочерний",
                "english_name": "AppendChild",
                "min_version": "8.1",
                "parameters": [{"name": "НовыйУзел", "param_type": "ЭлементDOM"}]
            }
        ]
    })
}

fn curated_overlay() -> Value {
    serde_json::from_str(include_str!("../data/platform_overlays.json")).unwrap()
}

/// The curated overlay narrowed to its DOM `AppendChild` override, the one
/// method [`base_data`] carries; the other sections stay as curated.
fn valid_overlay() -> String {
    let mut overlay = curated_overlay();
    overlay["method_parameter_overrides"].as_array_mut().unwrap().truncate(1);
    overlay.as_object_mut().unwrap().remove("global_function_parameter_overrides");
    serde_json::to_string_pretty(&overlay).unwrap()
}

fn overlay_with_versions(min_version: Option<&str>, max_version: Option<&str>) -> String {
    let mut overlay: Value = serde_json::from_str(&valid_overlay()).unwrap();
    let entry = overlay["method_parameter_overrides"][0].as_object_mut().unwrap();

    match min_version {
        Some(version) => entry.insert("min_version".to_owned(), Value::String(version.to_owned())),
        None => entry.remove("min_version"),
    };
    match max_version {
        Some(version) => entry.insert("max_version".to_owned(), Value::String(version.to_owned())),
        None => entry.remove("max_version"),
    };

    overlay.to_string()
}

fn base_data_with_version(version: Option<&str>) -> Value {
    let mut data = base_data();
    let method = data["methods"][0].as_object_mut().unwrap();

    match version {
        Some(version) => method.insert("min_version".to_owned(), Value::String(version.to_owned())),
        None => method.remove("min_version"),
    };

    data
}

#[test]
fn applies_curated_dom_append_child_override() {
    let mut data = base_data();

    apply_method_parameter_overlays(&mut data, &valid_overlay()).unwrap();

    assert_eq!(
        data["methods"][0]["parameters"][0]["param_type"],
        "АтрибутDOM, ДокументDOM, ЭлементDOM, ОпределениеТипаДокументаDOM, НотацияDOM, АтрибутHTML, ЭлементHTML, ЭлементКнопкаHTML, ЭлементВводаHTML, ЭлементЗаголовокHTML"
    );
}

#[test]
fn rejects_missing_target_deterministically() {
    let mut data = base_data();
    let overlay = valid_overlay().replace("DOMDocument", "MissingDocument");

    let error = apply_method_parameter_overlays(&mut data, &overlay).unwrap_err();

    assert_eq!(
        error.to_string(),
        "overlay target MissingDocument.AppendChild is missing or ambiguous"
    );
}

#[test]
fn rejects_duplicate_alias_override_deterministically() {
    let mut data = base_data();
    let mut overlay: Value = serde_json::from_str(&valid_overlay()).unwrap();
    let duplicate = overlay["method_parameter_overrides"][0].clone();
    overlay["method_parameter_overrides"].as_array_mut().unwrap().push(duplicate);

    let error = apply_method_parameter_overlays(&mut data, &overlay.to_string()).unwrap_err();

    assert_eq!(error.to_string(), "duplicate override for DOMDocument.AppendChild parameter 0");
}

#[test]
fn rejects_aliases_that_resolve_to_different_methods() {
    let mut data = base_data();
    data["methods"].as_array_mut().unwrap().push(json!({
        "id": 2,
        "type_name": "DOMDocument",
        "name": "ДругоеИмя",
        "english_name": "OtherName",
        "min_version": "8.1",
        "parameters": [{"name": "НовыйУзел", "param_type": "ЭлементDOM"}]
    }));
    let overlay = valid_overlay().replace("AppendChild", "OtherName");

    let error = apply_method_parameter_overlays(&mut data, &overlay).unwrap_err();

    assert_eq!(
        error.to_string(),
        "overlay aliases for DOMDocument.OtherName resolve to different methods (0 and 1)"
    );
}

#[test]
fn rejects_out_of_range_parameter_index_deterministically() {
    let mut data = base_data();
    let overlay = valid_overlay().replace("\"parameter_index\": 0", "\"parameter_index\": 1");

    let error = apply_method_parameter_overlays(&mut data, &overlay).unwrap_err();

    assert_eq!(
        error.to_string(),
        "overlay target DOMDocument.AppendChild has no parameter at index 1"
    );
}

#[test]
fn rejects_malformed_schema_deterministically() {
    let mut data = base_data();

    let error =
        apply_method_parameter_overlays(&mut data, "{\"schema_version\": \"1\"}").unwrap_err();

    assert_eq!(error.to_string(), "overlay schema_version must be the number 1");
}

#[test]
fn rejects_max_only_bound_above_target_version() {
    let mut data = base_data();

    let error =
        apply_method_parameter_overlays(&mut data, &overlay_with_versions(None, Some("8.0")))
            .unwrap_err();

    assert_eq!(
        error.to_string(),
        "overlay target DOMDocument.AppendChild version is outside its declared bounds"
    );
}

#[test]
fn accepts_max_only_bound_at_target_version() {
    let mut data = base_data();

    apply_method_parameter_overlays(&mut data, &overlay_with_versions(None, Some("8.1"))).unwrap();
}

#[test]
fn rejects_min_only_bound_below_target_version() {
    let mut data = base_data();

    let error =
        apply_method_parameter_overlays(&mut data, &overlay_with_versions(Some("8.2"), None))
            .unwrap_err();

    assert_eq!(
        error.to_string(),
        "overlay target DOMDocument.AppendChild version is outside its declared bounds"
    );
}

#[test]
fn accepts_min_only_bound_at_target_version() {
    let mut data = base_data();

    apply_method_parameter_overlays(&mut data, &overlay_with_versions(Some("8.1"), None)).unwrap();
}

#[test]
fn rejects_reversed_version_bounds() {
    let mut data = base_data();

    let error = apply_method_parameter_overlays(
        &mut data,
        &overlay_with_versions(Some("8.2"), Some("8.1")),
    )
    .unwrap_err();

    assert_eq!(error.to_string(), "overlay entry 0 has min_version after max_version");
}

#[test]
fn accepts_both_bounds_when_target_is_interior() {
    let mut data = base_data();

    apply_method_parameter_overlays(&mut data, &overlay_with_versions(Some("8.0"), Some("8.2")))
        .unwrap();
}

#[test]
fn accepts_both_bounds_at_inclusive_endpoint() {
    let mut data = base_data();

    apply_method_parameter_overlays(&mut data, &overlay_with_versions(Some("8.1"), Some("8.1")))
        .unwrap();
}

#[test]
fn rejects_missing_target_version_when_bounds_exist() {
    let mut data = base_data_with_version(None);

    let error =
        apply_method_parameter_overlays(&mut data, &overlay_with_versions(None, Some("8.1")))
            .unwrap_err();

    assert_eq!(
        error.to_string(),
        "overlay target DOMDocument.AppendChild has no valid minimum version"
    );
}

fn property_data() -> Value {
    json!({
        "types": [
            {"name": "ФормаКлиентскогоПриложения", "english_name": "ClientApplicationForm"}
        ],
        "properties": [
            {"id": 7, "type_name": "ClientApplicationForm", "name": "Заголовок", "english_name": "Title"}
        ]
    })
}

fn property_addition_overlay(entry: Value) -> String {
    json!({
        "schema_version": 1,
        "method_parameter_overrides": [],
        "type_property_additions": [entry]
    })
    .to_string()
}

fn window_mode_addition() -> Value {
    json!({
        "canonical_type": "ClientApplicationForm",
        "russian_name": "РежимОткрытияОкна",
        "english_name": "WindowOpeningMode",
        "property_types": ["РежимОткрытияОкнаФормы"],
        "is_readonly": false,
        "min_version": "8.2",
        "evidence_source": "v8std #std742",
        "rationale": "help archive files it under the enum type name"
    })
}

#[test]
fn appends_type_property_addition_with_synthetic_id() {
    let mut data = property_data();

    apply_type_property_additions(&mut data, &property_addition_overlay(window_mode_addition()))
        .unwrap();

    let properties = data["properties"].as_array().unwrap();
    assert_eq!(properties.len(), 2);
    let added = &properties[1];
    assert_eq!(added["type_name"], "ClientApplicationForm");
    assert_eq!(added["name"], "РежимОткрытияОкна");
    assert_eq!(added["english_name"], "WindowOpeningMode");
    assert_eq!(added["property_types"][0], "РежимОткрытияОкнаФормы");
    // max existing id (7) + 1
    assert_eq!(added["id"], 8);
}

#[test]
fn absent_additions_section_is_a_no_op() {
    let mut data = property_data();
    let overlay = json!({
        "schema_version": 1,
        "method_parameter_overrides": []
    })
    .to_string();

    apply_type_property_additions(&mut data, &overlay).unwrap();

    assert_eq!(data["properties"].as_array().unwrap().len(), 1);
}

#[test]
fn rejects_addition_for_unknown_type() {
    let mut data = property_data();
    let mut entry = window_mode_addition();
    entry["canonical_type"] = Value::String("NoSuchType".to_owned());

    let error =
        apply_type_property_additions(&mut data, &property_addition_overlay(entry)).unwrap_err();

    assert_eq!(
        error.to_string(),
        "type property addition target NoSuchType is not a known platform type (canonical_type must be the English type name)"
    );
}

#[test]
fn rejects_russian_canonical_type() {
    // The property index keys on the English type name, so a Russian alias would
    // produce a property `get_type_properties` can never resolve.
    let mut data = property_data();
    let mut entry = window_mode_addition();
    entry["canonical_type"] = Value::String("ФормаКлиентскогоПриложения".to_owned());

    let error =
        apply_type_property_additions(&mut data, &property_addition_overlay(entry)).unwrap_err();

    assert_eq!(
        error.to_string(),
        "type property addition target ФормаКлиентскогоПриложения is not a known platform type (canonical_type must be the English type name)"
    );
}

#[test]
fn skips_addition_the_extracted_data_already_carries() {
    // 8.5 archives file РежимОткрытияОкна on the form itself: the archive's
    // record, which has documentation, stays and the addition is dropped.
    let mut data = property_data();
    data["properties"][0]["documentation"] = json!({"description": "Заголовок формы."});
    let before = data["properties"].clone();
    let mut entry = window_mode_addition();
    entry["russian_name"] = Value::String("заголовок".to_owned());
    entry["english_name"] = Value::String("Title".to_owned());

    apply_type_property_additions(&mut data, &property_addition_overlay(entry)).unwrap();

    assert_eq!(data["properties"], before);
}

#[test]
fn rejects_full_match_alongside_a_property_sharing_one_alias() {
    // Заголовок/Title is a full match; Подпись/Title shares only the English
    // alias, so the name index would hold two entries for "Title". The verdict
    // must not depend on which comes first.
    let full = json!({"id": 7, "type_name": "ClientApplicationForm", "name": "Заголовок", "english_name": "Title"});
    let partial = json!({"id": 9, "type_name": "ClientApplicationForm", "name": "Подпись", "english_name": "Title"});
    let mut entry = window_mode_addition();
    entry["russian_name"] = Value::String("Заголовок".to_owned());
    entry["english_name"] = Value::String("Title".to_owned());

    for properties in [json!([full, partial]), json!([partial, full])] {
        let mut data = property_data();
        data["properties"] = properties;

        let error =
            apply_type_property_additions(&mut data, &property_addition_overlay(entry.clone()))
                .unwrap_err();

        assert_eq!(
            error.to_string(),
            "type property addition ClientApplicationForm.Title already exists in the extracted data"
        );
    }
}

#[test]
fn rejects_several_full_matches() {
    let mut data = property_data();
    let duplicate = json!({"id": 9, "type_name": "ClientApplicationForm", "name": "Заголовок", "english_name": "Title"});
    data["properties"].as_array_mut().unwrap().push(duplicate);
    let mut entry = window_mode_addition();
    entry["russian_name"] = Value::String("Заголовок".to_owned());
    entry["english_name"] = Value::String("Title".to_owned());

    let error =
        apply_type_property_additions(&mut data, &property_addition_overlay(entry)).unwrap_err();

    assert_eq!(
        error.to_string(),
        "type property addition ClientApplicationForm.Title already exists in the extracted data"
    );
}

#[test]
fn adds_after_skipping_a_present_addition_at_its_enumerate_offset() {
    // The skipped first addition still consumes offset 0, so the second takes
    // base_id (max existing id 7, plus 1) plus 1.
    let mut data = property_data();
    let mut present = window_mode_addition();
    present["russian_name"] = Value::String("Заголовок".to_owned());
    present["english_name"] = Value::String("Title".to_owned());
    let overlay = json!({
        "schema_version": 1,
        "method_parameter_overrides": [],
        "type_property_additions": [present, window_mode_addition()]
    })
    .to_string();

    apply_type_property_additions(&mut data, &overlay).unwrap();

    let properties = data["properties"].as_array().unwrap();
    assert_eq!(properties.len(), 2);
    assert_eq!(properties[1]["english_name"], "WindowOpeningMode");
    assert_eq!(properties[1]["id"], 9);
}

#[test]
fn rejects_addition_sharing_one_name_with_an_existing_property() {
    let mut data = property_data();
    let mut entry = window_mode_addition();
    entry["russian_name"] = Value::String("Заголовок".to_owned());
    entry["english_name"] = Value::String("Caption".to_owned());

    let error =
        apply_type_property_additions(&mut data, &property_addition_overlay(entry)).unwrap_err();

    assert_eq!(
        error.to_string(),
        "type property addition ClientApplicationForm.Caption already exists in the extracted data"
    );
}

#[test]
fn rejects_addition_colliding_with_existing_english_name_via_russian() {
    // property_data() has Заголовок/Title. An addition whose Russian name is
    // "Title" would shadow that property in the shared name index, so it must be
    // rejected even though no RU→RU or EN→EN pair matches directly.
    let mut data = property_data();
    let mut entry = window_mode_addition();
    entry["russian_name"] = Value::String("Title".to_owned());

    let error =
        apply_type_property_additions(&mut data, &property_addition_overlay(entry)).unwrap_err();

    assert_eq!(
        error.to_string(),
        "type property addition ClientApplicationForm.WindowOpeningMode already exists in the extracted data"
    );
}

#[test]
fn rejects_addition_with_empty_property_types() {
    let mut data = property_data();
    let mut entry = window_mode_addition();
    entry["property_types"] = json!([]);

    let error =
        apply_type_property_additions(&mut data, &property_addition_overlay(entry)).unwrap_err();

    assert_eq!(error.to_string(), "type property addition 0 property_types must not be empty");
}

#[test]
fn curated_overlay_adds_window_opening_mode() {
    let mut data = property_data();

    apply_type_property_additions(&mut data, &valid_overlay()).unwrap();

    let added = data["properties"].as_array().unwrap().iter().any(|prop| {
        prop["type_name"] == "ClientApplicationForm" && prop["name"] == "РежимОткрытияОкна"
    });
    assert!(added, "curated overlay must add ClientApplicationForm.РежимОткрытияОкна");
}

fn range_data() -> Value {
    json!({
        "types": [
            {"name": "ОбластьЯчеекТабличногоДокумента", "english_name": "SpreadsheetDocumentRange"},
            {"name": "ТабличныйДокумент", "english_name": "SpreadsheetDocument"}
        ],
        "properties": [
            {
                "id": 12748,
                "type_name": "SpreadsheetDocumentRange",
                "name": "Значение",
                "english_name": "Value",
                "property_types": ["Произвольный"],
                "is_readonly": false
            }
        ]
    })
}

fn correction_overlay(entries: Value) -> String {
    json!({
        "schema_version": 1,
        "method_parameter_overrides": [],
        "type_property_corrections": entries
    })
    .to_string()
}

fn range_is_readonly_correction() -> Value {
    json!({
        "type": "SpreadsheetDocumentRange",
        "name": "Значение",
        "is_readonly": true,
        "evidence_source": "runtime probe: the platform refuses the write",
        "rationale": "the help page omits the read-only mark"
    })
}

#[test]
fn corrects_an_existing_property_in_place() {
    let mut data = range_data();

    apply_type_property_corrections(
        &mut data,
        &correction_overlay(json!([range_is_readonly_correction()])),
    )
    .unwrap();

    let property = &data["properties"][0];
    assert_eq!(property["is_readonly"], true);
    assert_eq!(property["id"], 12748, "a correction keeps the record's identity and docs");
    assert_eq!(property["name"], "Значение");
    assert_eq!(
        property["property_types"][0], "Произвольный",
        "an untouched field stays as extracted"
    );
}

#[test]
fn corrects_a_property_named_by_its_russian_type_and_english_property() {
    // Both spellings of either name resolve, because the correction names a
    // record that exists rather than declaring one.
    let mut data = range_data();
    let mut entry = range_is_readonly_correction();
    entry["type"] = Value::String("ОбластьЯчеекТабличногоДокумента".to_owned());
    entry["name"] = Value::String("Value".to_owned());

    apply_type_property_corrections(&mut data, &correction_overlay(json!([entry]))).unwrap();

    assert_eq!(data["properties"][0]["is_readonly"], true);
}

#[test]
fn correction_replaces_the_extracted_type_list() {
    let mut data = range_data();
    let mut entry = range_is_readonly_correction();
    entry.as_object_mut().unwrap().remove("is_readonly");
    entry["property_types"] = json!(["Число"]);

    apply_type_property_corrections(&mut data, &correction_overlay(json!([entry]))).unwrap();

    assert_eq!(data["properties"][0]["property_types"][0], "Число");
    assert_eq!(data["properties"][0]["is_readonly"], false, "no is_readonly means no change");
}

#[test]
fn a_correction_without_a_target_leaves_the_corpus_alone() {
    // The overlay serves every source, and a corpus of another platform release
    // may well lack the property: the correction then simply does not apply,
    // and the corpus is not refused.
    let mut absent_type_data = range_data();
    let mut absent_property_data = range_data();
    let mut no_properties_data = range_data();
    no_properties_data.as_object_mut().unwrap().remove("properties");

    let mut absent_type = range_is_readonly_correction();
    absent_type["type"] = Value::String("СовсемДругойТип".to_owned());
    let mut absent_property = range_is_readonly_correction();
    absent_property["name"] = Value::String("НетТакогоСвойства".to_owned());

    apply_type_property_corrections(
        &mut absent_type_data,
        &correction_overlay(json!([absent_type])),
    )
    .expect("an absent type is not a corpus error");
    apply_type_property_corrections(
        &mut absent_property_data,
        &correction_overlay(json!([absent_property])),
    )
    .expect("an absent property is not a corpus error");
    apply_type_property_corrections(
        &mut no_properties_data,
        &correction_overlay(json!([range_is_readonly_correction()])),
    )
    .expect("a corpus without the properties section is not a corpus error");

    assert_eq!(absent_type_data["properties"][0]["is_readonly"], false);
    assert_eq!(absent_property_data["properties"][0]["is_readonly"], false);
    assert!(no_properties_data.get("properties").is_none());

    // Control: the same entry does correct a corpus that carries the target, so
    // the silence above is the absent target and not a dead section.
    let mut control = range_data();
    apply_type_property_corrections(
        &mut control,
        &correction_overlay(json!([range_is_readonly_correction()])),
    )
    .unwrap();
    assert_eq!(control["properties"][0]["is_readonly"], true);
}

#[test]
fn corrects_the_curated_spreadsheet_range_value() {
    let mut data = range_data();

    apply_type_property_corrections(&mut data, include_str!("../data/platform_overlays.json"))
        .unwrap();

    assert_eq!(
        data["properties"][0]["is_readonly"], true,
        "the curated overlay must correct ОбластьЯчеекТабличногоДокумента.Значение"
    );
}

#[test]
fn a_correction_without_evidence_is_rejected_by_the_schema() {
    let mut data = range_data();
    let mut entry = range_is_readonly_correction();
    entry.as_object_mut().unwrap().remove("evidence_source");

    let error = apply_type_property_corrections(&mut data, &correction_overlay(json!([entry])))
        .unwrap_err();

    assert_eq!(error.to_string(), "overlay entry 0 evidence_source must be a non-empty string");
    assert_eq!(data["properties"][0]["is_readonly"], false, "a refused document changes nothing");
}

#[test]
fn a_correction_that_changes_nothing_is_rejected_by_the_schema() {
    let mut data = range_data();
    let mut entry = range_is_readonly_correction();
    entry.as_object_mut().unwrap().remove("is_readonly");

    let error = apply_type_property_corrections(&mut data, &correction_overlay(json!([entry])))
        .unwrap_err();

    assert_eq!(
        error.to_string(),
        "type property correction 0 changes nothing: it needs is_readonly or property_types"
    );
}

#[test]
fn duplicate_corrections_of_one_property_are_rejected() {
    let mut data = range_data();
    let error = apply_type_property_corrections(
        &mut data,
        &correction_overlay(json!([
            range_is_readonly_correction(),
            range_is_readonly_correction()
        ])),
    )
    .unwrap_err();

    assert_eq!(
        error.to_string(),
        "duplicate type property correction for SpreadsheetDocumentRange.Значение"
    );
}

fn attestation_overlay(entries: Value) -> String {
    json!({
        "schema_version": 1,
        "method_parameter_overrides": [],
        "type_member_surface_attestations": entries
    })
    .to_string()
}

fn range_attestation() -> Value {
    json!({
        "type": "ОбластьЯчеекТабличногоДокумента",
        "verified_against_version": "8.3.27",
        "evidence_source": "the corpus page lists every member of the type",
        "rationale": "the surface is complete, so a miss is a defect"
    })
}

#[test]
fn an_attestation_resolves_its_type_to_the_canonical_name() {
    let data = range_data();

    let attestations = parse_resolved_member_surface_attestations(
        &data,
        &attestation_overlay(json!([range_attestation()])),
    )
    .unwrap();

    assert_eq!(attestations.len(), 1);
    assert_eq!(attestations[0].type_name, "SpreadsheetDocumentRange");
    assert_eq!(attestations[0].verified_against_version, "8.3.27");
}

#[test]
fn an_attestation_for_an_absent_type_is_dropped() {
    let data = range_data();
    let mut entry = range_attestation();
    entry["type"] = Value::String("ЗаведомоНесуществующийТип".to_owned());

    let attestations = parse_resolved_member_surface_attestations(
        &data,
        &attestation_overlay(json!([entry, range_attestation()])),
    )
    .unwrap();

    assert_eq!(attestations.len(), 1, "only the type this corpus has is attested");
    assert_eq!(attestations[0].type_name, "SpreadsheetDocumentRange");
}

#[test]
fn an_attestation_without_evidence_is_rejected_by_the_schema() {
    let data = range_data();
    let mut entry = range_attestation();
    entry.as_object_mut().unwrap().remove("evidence_source");

    let error =
        parse_resolved_member_surface_attestations(&data, &attestation_overlay(json!([entry])))
            .unwrap_err();

    assert_eq!(error.to_string(), "overlay entry 0 evidence_source must be a non-empty string");
}

#[test]
fn an_attestation_needs_a_dotted_numeric_version() {
    let data = range_data();
    let mut entry = range_attestation();
    entry["verified_against_version"] = Value::String("latest".to_owned());

    let error =
        parse_resolved_member_surface_attestations(&data, &attestation_overlay(json!([entry])))
            .unwrap_err();

    assert_eq!(
        error.to_string(),
        "type member surface attestation 0 verified_against_version must be a dotted numeric version"
    );
}

fn range_attestation_with_surface(surface: Value) -> Value {
    let mut attestation = range_attestation();
    attestation["verified_surface"] = surface;
    attestation
}

#[test]
fn an_attestation_without_verified_surface_attests_both_surfaces() {
    let attestations = parse_resolved_member_surface_attestations(
        &range_data(),
        &attestation_overlay(json!([range_attestation()])),
    )
    .unwrap();

    assert_eq!(attestations[0].surfaces, AttestedSurfaces::BOTH);
    assert!(attestations[0].surfaces.contains(MemberSurface::Methods));
    assert!(attestations[0].surfaces.contains(MemberSurface::Fields));
}

#[test]
fn verified_surface_names_the_surfaces_an_attestation_covers() {
    for (surface, methods, fields) in [
        (json!(["methods"]), true, false),
        (json!(["fields"]), false, true),
        (json!(["methods", "fields"]), true, true),
        (json!(["fields", "methods"]), true, true),
    ] {
        let attestations = parse_resolved_member_surface_attestations(
            &range_data(),
            &attestation_overlay(json!([range_attestation_with_surface(surface.clone())])),
        )
        .unwrap();

        assert_eq!(
            (
                attestations[0].surfaces.contains(MemberSurface::Methods),
                attestations[0].surfaces.contains(MemberSurface::Fields)
            ),
            (methods, fields),
            "verified_surface {surface}"
        );
    }
}

/// An empty list, a repeated value, an unknown value and a value that is not
/// a list at all are each refused at load time, and the error names the type
/// and the content it refuses, so the author of the entry can find it.
#[test]
fn a_malformed_verified_surface_is_rejected_with_the_type_and_the_content() {
    let prefix =
        "type member surface attestation 0 (ОбластьЯчеекТабличногоДокумента) verified_surface";
    for (surface, expected) in [
        (json!([]), format!("{prefix} must not be empty: omit it to attest both surfaces")),
        (json!(["methods", "methods"]), format!("{prefix} lists \"methods\" more than once")),
        (
            json!(["methods", "fields", "fields"]),
            format!("{prefix} lists \"fields\" more than once"),
        ),
        (
            json!(["methods", "properties"]),
            format!(
                "{prefix} has an unknown value \"properties\", expected \"methods\" or \"fields\""
            ),
        ),
        (
            json!([1]),
            format!("{prefix} has an unknown value 1, expected \"methods\" or \"fields\""),
        ),
        (
            json!("methods"),
            format!("{prefix} must be an array of \"methods\" and \"fields\", got \"methods\""),
        ),
    ] {
        let error = parse_resolved_member_surface_attestations(
            &range_data(),
            &attestation_overlay(json!([range_attestation_with_surface(surface.clone())])),
        )
        .unwrap_err();

        assert_eq!(error.to_string(), expected, "verified_surface {surface}");
    }
}

#[test]
fn the_curated_attestations_resolve_against_the_types_they_name() {
    let mut data = range_data();
    data["types"].as_array_mut().unwrap().push(json!({
        "name": "ЗаписьJSON", "english_name": "JSONWriter"
    }));
    data["types"].as_array_mut().unwrap().push(json!({
        "name": "ТаблицаЗначений", "english_name": "ValueTable"
    }));

    let attestations = parse_resolved_member_surface_attestations(
        &data,
        include_str!("../data/platform_overlays.json"),
    )
    .unwrap();

    let names: Vec<&str> = attestations.iter().map(|a| a.type_name.as_str()).collect();
    assert_eq!(
        names,
        ["SpreadsheetDocumentRange", "JSONWriter", "ValueTable"],
        "the curated document attests exactly these three types"
    );
    assert!(attestations.iter().all(|a| a.verified_against_version == "8.3.27"));
}

/// Two entries for one property under different spellings are one correction
/// twice: which of them won would be an accident of their order, so the document
/// is refused once their targets are resolved.
#[test]
fn corrections_that_resolve_to_one_property_are_rejected() {
    let mut data = range_data();
    let first = range_is_readonly_correction();
    let mut second = range_is_readonly_correction();
    second["type"] = Value::String("ОбластьЯчеекТабличногоДокумента".to_owned());
    second["name"] = Value::String("Value".to_owned());
    second["is_readonly"] = Value::Bool(false);

    let error = apply_type_property_corrections(
        &mut data,
        &correction_overlay(json!([first, second.clone()])),
    )
    .unwrap_err();

    assert_eq!(
        error.to_string(),
        "type property corrections SpreadsheetDocumentRange.Значение and \
         ОбластьЯчеекТабличногоДокумента.Value resolve to the same property of SpreadsheetDocumentRange"
    );

    // Control: the second entry alone does resolve — the refusal above is the
    // collision and not an unresolvable target.
    let mut control = range_data();
    apply_type_property_corrections(&mut control, &correction_overlay(json!([second]))).unwrap();
    assert_eq!(control["properties"][0]["is_readonly"], false);
}

/// The same rule for attestations: one type is attested once, whatever spelling
/// each entry uses.
#[test]
fn attestations_that_resolve_to_one_type_are_rejected() {
    let data = range_data();
    let mut second = range_attestation();
    second["type"] = Value::String("SpreadsheetDocumentRange".to_owned());
    second["verified_against_version"] = Value::String("8.2.1".to_owned());

    let error = parse_resolved_member_surface_attestations(
        &data,
        &attestation_overlay(json!([range_attestation(), second.clone()])),
    )
    .unwrap_err();

    assert_eq!(
        error.to_string(),
        "type member surface attestations ОбластьЯчеекТабличногоДокумента and \
         SpreadsheetDocumentRange resolve to the same type SpreadsheetDocumentRange"
    );

    // Control: the second entry alone resolves to that one type.
    let resolved =
        parse_resolved_member_surface_attestations(&data, &attestation_overlay(json!([second])))
            .unwrap();
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].type_name, "SpreadsheetDocumentRange");
}

/// One English name can denote two records — the corpus has four such pairs, all
/// of them form/extension types — and `is_ambiguous_type_name` refuses such a
/// name. A target resolved through it would be an arbitrary namesake, so the
/// name must not resolve at all.
#[test]
fn a_type_named_through_a_shared_english_name_is_no_target() {
    fn shared_english_name_data() -> Value {
        json!({
            "types": [
                {"name": "ПервыйТип", "english_name": "Dup"},
                {"name": "ВторойТип", "english_name": "Dup"}
            ],
            "properties": [
                {"id": 1, "type_name": "Dup", "name": "Значение", "english_name": "Value",
                 "property_types": ["Произвольный"], "is_readonly": false}
            ]
        })
    }

    let mut entry = range_is_readonly_correction();
    entry["type"] = Value::String("ПервыйТип".to_owned());

    let mut data = shared_english_name_data();
    apply_type_property_corrections(&mut data, &correction_overlay(json!([entry.clone()])))
        .unwrap();
    assert_eq!(
        data["properties"][0]["is_readonly"], false,
        "a shared English name denotes two records, so neither is a target"
    );

    let attestations = parse_resolved_member_surface_attestations(
        &shared_english_name_data(),
        &attestation_overlay(json!([range_attestation_with_type("ПервыйТип")])),
    )
    .unwrap();
    assert!(attestations.is_empty(), "the attestation names an arbitrary namesake");

    // Control: with the English names distinct the very same entry resolves.
    let mut unique = shared_english_name_data();
    unique["types"][1]["english_name"] = Value::String("Dup2".to_owned());
    apply_type_property_corrections(&mut unique, &correction_overlay(json!([entry.clone()])))
        .unwrap();
    assert_eq!(unique["properties"][0]["is_readonly"], true);

    let mut unique_attestation = shared_english_name_data();
    unique_attestation["types"][1]["english_name"] = Value::String("Dup2".to_owned());
    let resolved = parse_resolved_member_surface_attestations(
        &unique_attestation,
        &attestation_overlay(json!([range_attestation_with_type("ПервыйТип")])),
    )
    .unwrap();
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].type_name, "Dup");
}

fn range_attestation_with_type(type_name: &str) -> Value {
    let mut attestation = range_attestation();
    attestation["type"] = Value::String(type_name.to_owned());
    attestation
}

#[test]
fn an_attestation_needs_at_least_three_version_components() {
    // `8` and `8.3` are release lines, not releases: they would open the gate on
    // every release of the line, including the ones the surface was never
    // verified against.
    for version in ["8", "8.3"] {
        let data = range_data();
        let mut entry = range_attestation();
        entry["verified_against_version"] = Value::String(version.to_owned());

        let error =
            parse_resolved_member_surface_attestations(&data, &attestation_overlay(json!([entry])))
                .unwrap_err();

        assert_eq!(
            error.to_string(),
            "type member surface attestation 0 verified_against_version must be at least major.minor.patch",
            "version {version}"
        );
    }

    // Three components are enough, and a deeper patch version is not more than one.
    for version in ["8.3.27", "8.3.27.2214"] {
        let resolved = parse_resolved_member_surface_attestations(
            &range_data(),
            &attestation_overlay(json!([range_attestation_with_version(version)])),
        )
        .unwrap();
        assert_eq!(resolved[0].verified_against_version, version);
    }
}

#[test]
fn a_version_component_is_plain_digits() {
    // `u32::from_str` accepts a sign, so `+8.3.27` would otherwise parse as a
    // version and an attestation could carry a spelling no release has.
    for version in ["+8.3.27", "8.+3.27", "8.3.27 ", "8 .3.27", "8.3.27.2214-beta"] {
        let data = range_data();
        let mut entry = range_attestation();
        entry["verified_against_version"] = Value::String(version.to_owned());

        let error =
            parse_resolved_member_surface_attestations(&data, &attestation_overlay(json!([entry])))
                .unwrap_err();

        assert_eq!(
            error.to_string(),
            "type member surface attestation 0 verified_against_version must be a dotted numeric version",
            "version {version:?}"
        );
    }
}

fn range_attestation_with_version(version: &str) -> Value {
    let mut attestation = range_attestation();
    attestation["verified_against_version"] = Value::String(version.to_owned());
    attestation
}

/// The test-file markers of the page: only fenced blocks introduced by
/// `<!-- overlay-document -->` are overlay documents, and those are the ones a reader
/// copies out and loads. Illustrations of a shape (`{ "a": 1 }`) stay free to be
/// incomplete, so a future example of a fragment does not have to be a working
/// document. See "Every example on this page loads" in `OVERLAYS.md`.
const DOCUMENT_FENCE_MARKER: &str = "<!-- overlay-document -->";

/// Every documented example marked as an overlay document loads as it stands: a
/// reader copies one out of `OVERLAYS.md`, and a document the loader refuses — a
/// missing required root key, an unknown field — would be a lie about the schema.
/// The blocks are taken from the page itself, so the test cannot pass on an example
/// that only looks right next to prose.
#[test]
fn the_documented_examples_load_as_they_stand() {
    let page = include_str!("../data/OVERLAYS.md");
    let blocks = documented_overlay_blocks(page);
    assert!(
        blocks.len() >= 8,
        "the page documents eight loadable examples, found {}",
        blocks.len()
    );

    for block in &blocks {
        // The root of the document: schema version, the root field allow-list
        // and the required `method_parameter_overrides` array.
        assert!(
            super::overlay_schema::parse_overrides(block).is_ok(),
            "an example on the page is not a loadable overlay document:\n{block}"
        );
        let entries = super::overlay_schema::parse_overrides(block).unwrap().len()
            + super::overlay_schema::parse_global_function_overrides(block).unwrap().len()
            + super::overlay_schema::parse_property_additions(block).unwrap().len()
            + super::overlay_schema::parse_property_corrections(block).unwrap().len()
            + super::overlay_schema::parse_method_return_type_corrections(block).unwrap().len()
            + super::overlay_schema::parse_member_surface_attestations(block).unwrap().len()
            + super::overlay_schema::parse_parameter_passing_attestations(block).unwrap().len();
        assert!(entries > 0, "an example documents no section entry:\n{block}");
    }
}

/// The marker selects exactly the fence it introduces: a marked block is a document,
/// an unmarked one is an illustration whatever preceded it, and any other fence in
/// between ends the marker's reach. The page is synthetic here so the cases stay
/// readable; the real page is checked by [`the_documented_examples_load_as_they_stand`].
#[test]
fn only_a_marked_fence_is_a_document() {
    let page = r#"# Page

<!-- overlay-document -->
```json
{ "marked": true }
```

<!-- overlay-document -->
```text
a note, not a document
```
```json
{ "unmarked": true }
```

```json
{ "unmarked too": true }
```
"#;

    let blocks = documented_overlay_blocks(page);
    assert_eq!(blocks.len(), 1, "only the marked fence is a document: {blocks:?}");
    assert!(blocks[0].contains("marked"), "and it is the marked one: {blocks:?}");

    // A page with no marker at all documents nothing — which is what the real page's
    // `>= 5` threshold reports as "found N" when a marker is removed.
    assert!(documented_overlay_blocks("```json\n{ \"a\": 1 }\n```\n").is_empty());
}

/// The `DOCUMENT_FENCE_MARKER`-marked ```json blocks of a Markdown page, in page order.
///
/// The marker covers the fence it introduces and nothing else: any other fence
/// opening in between — a ```text note, another ```json — clears it, so a later
/// unmarked block is never dragged into the contract by a stale marker.
fn documented_overlay_blocks(page: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut current: Option<String> = None;
    let mut marked = false;
    for line in page.lines() {
        let line = line.trim_end();
        match &mut current {
            None if line == DOCUMENT_FENCE_MARKER => marked = true,
            None if line == "```json" && marked => current = Some(String::new()),
            None if line.starts_with("```") => marked = false,
            Some(block) if line == "```" => {
                blocks.push(std::mem::take(block));
                current = None;
                marked = false;
            }
            Some(block) => {
                block.push_str(line);
                block.push('\n');
            }
            None => {}
        }
    }
    blocks
}

fn global_function_data() -> Value {
    json!({
        "types": [
            {"name": "ОбъектМетаданных: Поле", "english_name": "MetadataObject: Field"},
            {"name": "ОбъектМетаданных: Справочник", "english_name": "MetadataObject: Catalog"},
            {"name": "ОбъектМетаданныхКонфигурация", "english_name": "ConfigurationMetadataObject"},
            {"name": "ОбъектМетаданных: ТаблицаИзмерения", "english_name": "MetadataObject: DimensionTable"}
        ],
        "methods": [
            {
                "id": 1,
                "type_name": "MetadataObjectCollection",
                "name": "ПравоДоступа",
                "english_name": "AccessRight",
                "min_version": "8.0",
                "parameters": [{"name": "Право", "param_type": "Строка"}, {"name": "Объект", "param_type": "Строка"}]
            }
        ],
        "global_functions": [
            {
                "id": 459,
                "name": "ПравоДоступа",
                "english_name": "AccessRight",
                "min_version": "8.0",
                "parameters": [
                    {"name": "Право", "param_type": "Строка"},
                    {"name": "ОбъектМетаданных", "param_type": "ОбъектМетаданных: ТаблицаИзмерения, ОбъектМетаданных: Поле"}
                ]
            }
        ]
    })
}

fn global_function_overlay(entry: Value) -> String {
    json!({
        "schema_version": 1,
        "method_parameter_overrides": [],
        "global_function_parameter_overrides": [entry]
    })
    .to_string()
}

fn access_right_override() -> Value {
    json!({
        "russian_name": "ПравоДоступа",
        "english_name": "AccessRight",
        "min_version": "8.0",
        "parameter_index": 1,
        "replacement_type_list": ["ОбъектМетаданных: *"],
        "evidence_source": "the parameter description lists every metadata-object kind",
        "rationale": "the extract cut the list to five kinds"
    })
}

#[test]
fn applies_global_function_override_by_name_alone() {
    let mut data = global_function_data();

    apply_global_function_parameter_overlays(
        &mut data,
        &global_function_overlay(access_right_override()),
    )
    .unwrap();

    assert_eq!(
        data["global_functions"][0]["parameters"][1]["param_type"],
        "ОбъектМетаданных: Поле, ОбъектМетаданных: Справочник, ОбъектМетаданных: ТаблицаИзмерения"
    );
    assert_eq!(
        data["methods"][0]["parameters"][1]["param_type"], "Строка",
        "a type method of the same name is not a global function"
    );
}

#[test]
fn global_function_override_leaves_method_overrides_alone() {
    let mut data = global_function_data();

    apply_method_parameter_overlays(&mut data, &global_function_overlay(access_right_override()))
        .unwrap();

    assert_eq!(
        data["global_functions"][0]["parameters"][1]["param_type"],
        "ОбъектМетаданных: ТаблицаИзмерения, ОбъектМетаданных: Поле"
    );
}

#[test]
fn absent_global_function_section_is_a_no_op() {
    let mut data = global_function_data();
    data.as_object_mut().unwrap().remove("global_functions");

    apply_global_function_parameter_overlays(&mut data, &valid_overlay()).unwrap();
}

#[test]
fn rejects_missing_global_function_target() {
    let mut data = global_function_data();
    let mut entry = access_right_override();
    entry["english_name"] = Value::String("NoSuchFunction".to_owned());
    entry["russian_name"] = Value::String("НетТакойФункции".to_owned());

    let error =
        apply_global_function_parameter_overlays(&mut data, &global_function_overlay(entry))
            .unwrap_err();

    assert_eq!(error.to_string(), "overlay target NoSuchFunction is missing or ambiguous");
}

#[test]
fn rejects_global_function_aliases_that_resolve_to_different_functions() {
    let mut data = global_function_data();
    data["global_functions"].as_array_mut().unwrap().push(json!({
        "id": 460,
        "name": "ДругаяФункция",
        "english_name": "OtherFunction",
        "min_version": "8.0",
        "parameters": [{"name": "Право", "param_type": "Строка"}, {"name": "Объект", "param_type": "Строка"}]
    }));
    let mut entry = access_right_override();
    entry["english_name"] = Value::String("OtherFunction".to_owned());

    let error =
        apply_global_function_parameter_overlays(&mut data, &global_function_overlay(entry))
            .unwrap_err();

    assert_eq!(
        error.to_string(),
        "overlay aliases for OtherFunction resolve to different methods (0 and 1)"
    );
}

#[test]
fn rejects_canonical_type_on_a_global_function_override() {
    let mut data = global_function_data();
    let mut entry = access_right_override();
    entry["canonical_type"] = Value::String("MetadataObjectCollection".to_owned());

    let error =
        apply_global_function_parameter_overlays(&mut data, &global_function_overlay(entry))
            .unwrap_err();

    assert_eq!(error.to_string(), "overlay entry 0 has unknown field canonical_type");
}

#[test]
fn rejects_duplicate_global_function_override() {
    let mut data = global_function_data();
    let overlay = json!({
        "schema_version": 1,
        "method_parameter_overrides": [],
        "global_function_parameter_overrides": [access_right_override(), access_right_override()]
    })
    .to_string();

    let error = apply_global_function_parameter_overlays(&mut data, &overlay).unwrap_err();

    assert_eq!(error.to_string(), "duplicate override for AccessRight parameter 1");
}

#[test]
fn rejects_global_function_override_outside_its_version_bounds() {
    let mut data = global_function_data();
    let mut entry = access_right_override();
    entry["min_version"] = Value::String("8.2".to_owned());

    let error =
        apply_global_function_parameter_overlays(&mut data, &global_function_overlay(entry))
            .unwrap_err();

    assert_eq!(
        error.to_string(),
        "overlay target AccessRight version is outside its declared bounds"
    );
}

#[test]
fn family_wildcard_expands_to_every_extracted_member() {
    let mut data = global_function_data();
    let mut entry = access_right_override();
    entry["replacement_type_list"] =
        json!(["Массив", "ОбъектМетаданных: *", "ОбъектМетаданных: Поле"]);

    apply_global_function_parameter_overlays(&mut data, &global_function_overlay(entry)).unwrap();

    assert_eq!(
        data["global_functions"][0]["parameters"][1]["param_type"],
        "Массив, ОбъектМетаданных: Поле, ОбъектМетаданных: Справочник, ОбъектМетаданных: ТаблицаИзмерения",
        "members in extract order, once each; a name merely sharing the stem is no member"
    );
}

#[test]
fn rejects_family_wildcard_without_members() {
    let mut data = global_function_data();
    let mut entry = access_right_override();
    entry["replacement_type_list"] = json!(["ОбъектНетТакого: *"]);

    let error =
        apply_global_function_parameter_overlays(&mut data, &global_function_overlay(entry))
            .unwrap_err();

    assert_eq!(
        error.to_string(),
        "overlay target AccessRight replacement ОбъектНетТакого: * names no extracted type"
    );
}

#[test]
fn a_star_without_a_family_prefix_is_a_plain_name() {
    let mut data = global_function_data();
    let mut entry = access_right_override();
    entry["replacement_type_list"] = json!(["ОбъектМетаданных*"]);

    apply_global_function_parameter_overlays(&mut data, &global_function_overlay(entry)).unwrap();

    assert_eq!(data["global_functions"][0]["parameters"][1]["param_type"], "ОбъектМетаданных*");
}

fn xdto_factory_data() -> Value {
    json!({
        "types": [
            {"name": "ФабрикаXDTO", "english_name": "XDTOFactory"},
            {"name": "ОбъектXDTO", "english_name": "XDTODataObject"},
            {"name": "ЗначениеXDTO", "english_name": "XDTODataValue"}
        ],
        "methods": [
            {
                "id": 2070,
                "type_name": "XDTOFactory",
                "name": "Создать",
                "english_name": "Create",
                "return_type": "ЗначениеXDTO",
                "parameters": [{"name": "Тип", "param_type": "ТипОбъектаXDTO"}]
            },
            {
                "id": 2072,
                "type_name": "XDTOFactory",
                "name": "ПрочитатьXML",
                "english_name": "ReadXML",
                "return_type": "ОбъектXDTO",
                "parameters": []
            }
        ]
    })
}

fn return_type_overlay(entries: Value) -> String {
    json!({
        "schema_version": 1,
        "method_parameter_overrides": [],
        "method_return_type_corrections": entries
    })
    .to_string()
}

fn xdto_create_correction() -> Value {
    json!({
        "type": "XDTOFactory",
        "name": "Создать",
        "return_types": ["ОбъектXDTO", "ЗначениеXDTO"],
        "evidence_source": "runtime probe: ТипЗнч of the result for each call variant",
        "rationale": "the help page documents one return type for every variant"
    })
}

#[test]
fn corrects_a_method_return_type_in_place() {
    let mut data = xdto_factory_data();

    apply_method_return_type_corrections(
        &mut data,
        &return_type_overlay(json!([xdto_create_correction()])),
    )
    .unwrap();

    let method = &data["methods"][0];
    assert_eq!(method["return_type"], "ОбъектXDTO, ЗначениеXDTO");
    assert_eq!(method["id"], 2070, "a correction keeps the record's identity and docs");
    assert_eq!(method["parameters"][0]["param_type"], "ТипОбъектаXDTO");
    assert_eq!(data["methods"][1]["return_type"], "ОбъектXDTO", "a neighbour is not touched");
}

#[test]
fn corrects_a_return_type_named_by_its_russian_type_and_english_method() {
    let mut data = xdto_factory_data();
    let mut entry = xdto_create_correction();
    entry["type"] = Value::String("ФабрикаXDTO".to_owned());
    entry["name"] = Value::String("Create".to_owned());

    apply_method_return_type_corrections(&mut data, &return_type_overlay(json!([entry]))).unwrap();

    assert_eq!(data["methods"][0]["return_type"], "ОбъектXDTO, ЗначениеXDTO");
}

#[test]
fn a_return_type_correction_without_a_target_leaves_the_corpus_alone() {
    let mut absent_type_data = xdto_factory_data();
    let mut absent_method_data = xdto_factory_data();
    let mut absent_type = xdto_create_correction();
    absent_type["type"] = Value::String("СовсемДругойТип".to_owned());
    let mut absent_method = xdto_create_correction();
    absent_method["name"] = Value::String("НетТакогоМетода".to_owned());

    apply_method_return_type_corrections(
        &mut absent_type_data,
        &return_type_overlay(json!([absent_type])),
    )
    .expect("an absent type is not a corpus error");
    apply_method_return_type_corrections(
        &mut absent_method_data,
        &return_type_overlay(json!([absent_method])),
    )
    .expect("an absent method is not a corpus error");

    assert_eq!(absent_type_data["methods"][0]["return_type"], "ЗначениеXDTO");
    assert_eq!(absent_method_data["methods"][0]["return_type"], "ЗначениеXDTO");

    // Control: the same entry with its real target does correct the corpus.
    let mut control = xdto_factory_data();
    apply_method_return_type_corrections(
        &mut control,
        &return_type_overlay(json!([xdto_create_correction()])),
    )
    .unwrap();
    assert_eq!(control["methods"][0]["return_type"], "ОбъектXDTO, ЗначениеXDTO");
}

#[test]
fn two_return_type_corrections_of_one_method_are_rejected() {
    let mut data = xdto_factory_data();
    let mut second = xdto_create_correction();
    second["type"] = Value::String("ФабрикаXDTO".to_owned());
    second["name"] = Value::String("Create".to_owned());

    let error = apply_method_return_type_corrections(
        &mut data,
        &return_type_overlay(json!([xdto_create_correction(), second])),
    )
    .unwrap_err();

    assert_eq!(
        error.to_string(),
        "method return type corrections XDTOFactory.Создать and ФабрикаXDTO.Create resolve to the same method of XDTOFactory"
    );
}

#[test]
fn a_return_type_correction_needs_a_non_empty_unique_type_list() {
    for (return_types, message) in [
        (json!([]), "method return type correction 0 return_types must not be empty"),
        (
            json!(["ОбъектXDTO", "ОбъектXDTO"]),
            "method return type correction 0 return_types contains duplicate types",
        ),
        (
            json!(["ОбъектXDTO", "объектxdto"]),
            "method return type correction 0 return_types contains duplicate types",
        ),
        (json!("ОбъектXDTO"), "method return type correction 0 return_types must be an array"),
    ] {
        let mut data = xdto_factory_data();
        let mut entry = xdto_create_correction();
        entry["return_types"] = return_types;

        let error =
            apply_method_return_type_corrections(&mut data, &return_type_overlay(json!([entry])))
                .unwrap_err();

        assert_eq!(error.to_string(), message);
        assert_eq!(data["methods"][0]["return_type"], "ЗначениеXDTO");
    }
}

#[test]
fn corrects_the_curated_xdto_factory_create() {
    let mut data = xdto_factory_data();

    apply_method_return_type_corrections(&mut data, include_str!("../data/platform_overlays.json"))
        .unwrap();

    assert_eq!(
        data["methods"][0]["return_type"], "ОбъектXDTO, ЗначениеXDTO",
        "the curated overlay must correct ФабрикаXDTO.Создать"
    );
}

fn passing_data() -> Value {
    json!({
        "types": [],
        "methods": [],
        "global_functions": [
            {
                "id": 64,
                "name": "ВвестиСтроку",
                "english_name": "InputString",
                "min_version": "8.0",
                "parameters": [
                    {"name": "Строка", "param_type": "Строка"},
                    {"name": "Подсказка", "param_type": "Строка"}
                ]
            },
            {
                "id": 488,
                "name": "ПоместитьФайл",
                "english_name": "PutFile",
                "min_version": "8.2",
                "parameters": [{"name": "Адрес", "param_type": "Строка"}]
            }
        ]
    })
}

fn input_string_passing() -> Value {
    json!({
        "russian_name": "ВвестиСтроку",
        "english_name": "InputString",
        "min_version": "8.0",
        "parameter_index": 0,
        "passing": "by_ref",
        "evidence_source": "the parameter description says the entered string is stored in it",
        "rationale": "the call overwrites the caller's variable"
    })
}

#[test]
fn a_global_function_override_carrying_only_passing_keeps_the_extracted_type_list() {
    let mut data = passing_data();

    apply_global_function_parameter_overlays(
        &mut data,
        &global_function_overlay(input_string_passing()),
    )
    .unwrap();

    let parameters = &data["global_functions"][0]["parameters"];
    assert_eq!(parameters[0]["passing"], "by_ref");
    assert_eq!(parameters[0]["param_type"], "Строка", "a record without a type list rewrites none");
    assert!(parameters[1].get("passing").is_none(), "the neighbour parameter states nothing");
}

#[test]
fn a_record_carrying_a_type_list_and_passing_applies_both() {
    let mut data = passing_data();
    let mut entry = input_string_passing();
    entry["replacement_type_list"] = json!(["Число"]);
    entry["passing"] = json!("by_value");

    apply_global_function_parameter_overlays(&mut data, &global_function_overlay(entry)).unwrap();

    let parameter = &data["global_functions"][0]["parameters"][0];
    assert_eq!(parameter["param_type"], "Число");
    assert_eq!(parameter["passing"], "by_value");
}

#[test]
fn a_record_that_states_neither_a_type_list_nor_passing_is_refused() {
    let mut data = passing_data();
    let mut entry = input_string_passing();
    entry.as_object_mut().unwrap().remove("passing");

    let error =
        apply_global_function_parameter_overlays(&mut data, &global_function_overlay(entry))
            .unwrap_err();

    assert_eq!(
        error.to_string(),
        "overlay entry 0 changes nothing: it needs replacement_type_list or passing"
    );
}

#[test]
fn a_passing_spelled_any_other_way_is_refused_naming_both_literals() {
    for given in [json!("byref"), json!("BY_REF"), json!(""), json!(true)] {
        let mut data = passing_data();
        let mut entry = input_string_passing();
        entry["passing"] = given.clone();

        let error =
            apply_global_function_parameter_overlays(&mut data, &global_function_overlay(entry))
                .unwrap_err();

        assert_eq!(
            error.to_string(),
            format!("overlay entry 0 passing must be \"by_value\" or \"by_ref\", got {given}"),
        );
    }
}

#[test]
fn two_passing_records_for_one_parameter_are_refused_by_the_duplicate_guard() {
    let mut data = passing_data();
    let overlay = json!({
        "schema_version": 1,
        "method_parameter_overrides": [],
        "global_function_parameter_overrides": [input_string_passing(), input_string_passing()]
    })
    .to_string();

    let error = apply_global_function_parameter_overlays(&mut data, &overlay).unwrap_err();

    assert_eq!(error.to_string(), "duplicate override for InputString parameter 0");
}

#[test]
fn a_passing_only_record_still_has_its_target_checked() {
    let mut missing = input_string_passing();
    missing["russian_name"] = json!("НетТакойФункции");
    missing["english_name"] = json!("NoSuchFunction");
    let mut out_of_range = input_string_passing();
    out_of_range["parameter_index"] = json!(5);
    let mut below_version = input_string_passing();
    below_version["min_version"] = json!("8.3");

    for (entry, expected) in [
        (missing, "overlay target NoSuchFunction is missing or ambiguous"),
        (out_of_range, "overlay target InputString has no parameter at index 5"),
        (below_version, "overlay target InputString version is outside its declared bounds"),
    ] {
        let mut data = passing_data();

        let error =
            apply_global_function_parameter_overlays(&mut data, &global_function_overlay(entry))
                .unwrap_err();

        assert_eq!(error.to_string(), expected);
    }
}

fn passing_attestation_overlay(entries: Value) -> String {
    json!({
        "schema_version": 1,
        "method_parameter_overrides": [],
        "parameter_passing_attestations": entries
    })
    .to_string()
}

fn global_passing_attestation() -> Value {
    json!({
        "scope": "global_context",
        "verified_against_version": "8.3.27",
        "unverified_functions": ["PutFile"],
        "evidence_source": "the sweep of every parameter description of the global context",
        "rationale": "a parameter with no record is passed by value, except in the listed function"
    })
}

#[test]
fn a_passing_attestation_reaches_the_snapshot_with_its_functions_by_russian_name() {
    let corpus = serde_json::to_vec(&passing_data()).unwrap();
    let overlays = passing_attestation_overlay(json!([global_passing_attestation()]));

    let snapshot =
        crate::snapshot::PlatformSnapshot::from_corpus_json_with_overlays(&corpus, &overlays)
            .unwrap();

    assert_eq!(
        snapshot.parameter_passing_attestations,
        vec![ParameterPassingAttestationRecord {
            scope: "global_context".to_owned(),
            verified_against_version: "8.3.27".to_owned(),
            unverified_functions: vec!["ПоместитьФайл".to_owned()],
        }],
        "the English spelling of the function is carried under the extract's Russian one"
    );
}

#[test]
fn a_corpus_without_a_passing_attestation_has_none() {
    let corpus = serde_json::to_vec(&passing_data()).unwrap();
    let overlays = passing_attestation_overlay(Value::Null);

    let snapshot =
        crate::snapshot::PlatformSnapshot::from_corpus_json_with_overlays(&corpus, &overlays)
            .unwrap();

    assert!(snapshot.parameter_passing_attestations.is_empty());
}

#[test]
fn two_passing_attestations_of_one_scope_are_refused() {
    let overlay = passing_attestation_overlay(json!([
        global_passing_attestation(),
        global_passing_attestation()
    ]));

    let error =
        parse_resolved_parameter_passing_attestations(&passing_data(), &overlay).unwrap_err();

    assert_eq!(error.to_string(), "duplicate parameter passing attestation for global_context");
}

#[test]
fn a_passing_attestation_needs_at_least_three_version_components() {
    const TOO_SHORT: &str =
        "parameter passing attestation 0 verified_against_version must be at least major.minor.patch";
    const NOT_DOTTED: &str =
        "parameter passing attestation 0 verified_against_version must be a dotted numeric version";
    for (version, expected) in [("8.3", TOO_SHORT), ("8", TOO_SHORT), ("+8.3.27", NOT_DOTTED)] {
        let mut entry = global_passing_attestation();
        entry["verified_against_version"] = json!(version);

        let error = parse_resolved_parameter_passing_attestations(
            &passing_data(),
            &passing_attestation_overlay(json!([entry])),
        )
        .unwrap_err();

        assert_eq!(error.to_string(), expected, "version {version:?}");
    }
}

#[test]
fn an_empty_unverified_functions_list_is_refused() {
    let mut entry = global_passing_attestation();
    entry["unverified_functions"] = json!([]);

    let error = parse_resolved_parameter_passing_attestations(
        &passing_data(),
        &passing_attestation_overlay(json!([entry])),
    )
    .unwrap_err();

    assert_eq!(
        error.to_string(),
        "parameter passing attestation 0 unverified_functions must not be empty: omit it when the scope is judged in full"
    );
}

#[test]
fn an_unverified_function_listed_twice_is_refused() {
    let mut entry = global_passing_attestation();
    entry["unverified_functions"] = json!(["ПоместитьФайл", "поместитьфайл"]);

    let error = parse_resolved_parameter_passing_attestations(
        &passing_data(),
        &passing_attestation_overlay(json!([entry])),
    )
    .unwrap_err();

    assert_eq!(
        error.to_string(),
        "parameter passing attestation 0 unverified_functions contains duplicate functions"
    );
}

#[test]
fn an_unverified_function_the_corpus_lacks_resolves_without_error_and_is_left_out() {
    let mut entry = global_passing_attestation();
    entry["unverified_functions"] = json!(["ПоместитьФайл", "ПоместитьФайлы"]);

    let resolved = parse_resolved_parameter_passing_attestations(
        &passing_data(),
        &passing_attestation_overlay(json!([entry])),
    )
    .unwrap();

    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].unverified_functions, vec!["ПоместитьФайл".to_owned()]);
}

/// Guards OUR data, not the loader: the loader drops a name its corpus lacks, so a typo in
/// `platform_overlays.json` would pass silently; it is caught here, against the real facts.
#[test]
fn every_unverified_function_of_the_shipped_attestations_exists_in_the_bundled_corpus() {
    let facts: Value = serde_json::from_str(include_str!("../data/platform_facts.json")).unwrap();
    let overlays = include_str!("../data/platform_overlays.json");
    let listed: Vec<String> = curated_overlay()["parameter_passing_attestations"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|attestation| attestation["unverified_functions"].as_array().unwrap().clone())
        .map(|name| name.as_str().unwrap().to_owned())
        .collect();
    assert!(!listed.is_empty(), "the guard would be vacuous without listed functions");

    let resolved = parse_resolved_parameter_passing_attestations(&facts, overlays).unwrap();

    let kept: Vec<String> =
        resolved.into_iter().flat_map(|attestation| attestation.unverified_functions).collect();
    assert_eq!(kept, listed, "a listed name resolved to no function of the bundled corpus");
}

#[test]
fn a_passing_attestation_of_another_scope_is_not_supported_yet() {
    let mut entry = global_passing_attestation();
    entry["scope"] = json!("XMLWriter");

    let error = parse_resolved_parameter_passing_attestations(
        &passing_data(),
        &passing_attestation_overlay(json!([entry])),
    )
    .unwrap_err();

    assert_eq!(
        error.to_string(),
        "parameter passing attestation scope XMLWriter is not supported yet: only global_context"
    );
}

#[test]
fn the_shipped_overlay_records_eleven_passing_parameters_and_one_attestation() {
    let facts = include_str!("../data/platform_facts.json");
    let overlays = include_str!("../data/platform_overlays.json");

    let mut data: Value = serde_json::from_str(facts).unwrap();
    apply_global_function_parameter_overlays(&mut data, overlays).unwrap();
    let mut recorded: Vec<(String, usize, String)> = Vec::new();
    for function in data["global_functions"].as_array().unwrap() {
        let parameters = function.get("parameters").and_then(Value::as_array);
        for (index, parameter) in parameters.into_iter().flatten().enumerate() {
            if let Some(passing) = parameter.get("passing") {
                recorded.push((
                    function["name"].as_str().unwrap().to_owned(),
                    index,
                    passing.as_str().unwrap().to_owned(),
                ));
            }
        }
    }
    recorded.sort();
    let mut expected: Vec<(String, usize, String)> = [
        ("ВвестиСтроку", 0),
        ("ВвестиЧисло", 0),
        ("ВвестиДату", 0),
        ("ВвестиЗначение", 0),
        ("ПоместитьФайл", 0),
        ("ПоместитьФайл", 2),
        ("ПоместитьФайлы", 1),
        ("ПолучитьФайлы", 1),
        ("ЗапуститьПриложение", 3),
        ("ЗапуститьСистему", 2),
        ("УдалитьОбъекты", 2),
    ]
    .into_iter()
    .map(|(name, index)| (name.to_owned(), index, "by_ref".to_owned()))
    .collect();
    expected.sort();
    assert_eq!(recorded, expected);

    let snapshot = crate::snapshot::PlatformSnapshot::from_corpus_json(facts.as_bytes()).unwrap();
    let curated = curated_overlay();
    let listed: Vec<String> = curated["parameter_passing_attestations"][0]["unverified_functions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|name| name.as_str().unwrap().to_owned())
        .collect();
    assert_eq!(listed.len(), 23);
    assert_eq!(
        snapshot.parameter_passing_attestations,
        vec![ParameterPassingAttestationRecord {
            scope: "global_context".to_owned(),
            verified_against_version: "8.3.27".to_owned(),
            unverified_functions: listed,
        }]
    );
}

fn stamped_corpus() -> Value {
    json!({
        "types": [{"name": "Массив", "english_name": "Array", "min_version": "8.0"}],
        "methods": [{
            "id": 1,
            "type_name": "Array",
            "name": "Добавить",
            "english_name": "Add",
            "min_version": "8.0",
            "parameters": [{"name": "Значение", "param_type": "Произвольный"}]
        }],
        "global_functions": [
            {
                "id": 64,
                "name": "ВвестиСтроку",
                "english_name": "InputString",
                "min_version": "8.0",
                "parameters": [
                    {"name": "Строка", "param_type": "Строка"},
                    {"name": "Подсказка", "param_type": "Строка"}
                ]
            },
            {
                "id": 488,
                "name": "ПоместитьФайл",
                "english_name": "PutFile",
                "min_version": "8.2",
                "parameters": [
                    {"name": "Адрес", "param_type": "Строка"},
                    {"name": "Описание", "param_type": "Строка"}
                ]
            },
            {
                "id": 70,
                "name": "Сообщить",
                "english_name": "Message",
                "min_version": "8.0",
                "parameters": [{"name": "Текст", "param_type": "Строка"}],
                "variants": [{
                    "variant_name": "Вариант1",
                    "parameters": [
                        {"name": "Текст", "param_type": "Строка"},
                        {"name": "Статус", "param_type": "Строка"}
                    ]
                }]
            }
        ]
    })
}

fn stamped_overlays() -> String {
    json!({
        "schema_version": 1,
        "method_parameter_overrides": [],
        "global_function_parameter_overrides": [input_string_passing()],
        "parameter_passing_attestations": [global_passing_attestation()]
    })
    .to_string()
}

fn stamped_snapshot() -> crate::snapshot::PlatformSnapshot {
    let corpus = serde_json::to_vec(&stamped_corpus()).unwrap();
    crate::snapshot::PlatformSnapshot::from_corpus_json_with_overlays(&corpus, &stamped_overlays())
        .unwrap()
}

fn global_function<'a>(
    snapshot: &'a crate::snapshot::PlatformSnapshot,
    name: &str,
) -> &'a crate::types::GlobalFunction {
    snapshot.global_functions.iter().find(|function| function.name == name).unwrap()
}

#[test]
fn both_passing_literals_decode_into_their_variants() {
    let mut data = stamped_corpus();
    data["global_functions"][0]["parameters"][0]["passing"] = json!("by_ref");
    data["global_functions"][0]["parameters"][1]["passing"] = json!("by_value");
    let corpus = serde_json::to_vec(&data).unwrap();
    let overlays = passing_attestation_overlay(Value::Null);

    let snapshot =
        crate::snapshot::PlatformSnapshot::from_corpus_json_with_overlays(&corpus, &overlays)
            .unwrap();

    let parameters = &global_function(&snapshot, "ВвестиСтроку").parameters;
    assert_eq!(parameters[0].passing, Some(ParamPassing::ByRef));
    assert_eq!(parameters[1].passing, Some(ParamPassing::ByVal));
}

#[test]
fn an_unrecognised_passing_literal_is_refused_naming_the_value() {
    let mut data = stamped_corpus();
    data["global_functions"][0]["parameters"][0]["passing"] = json!("byref");
    let corpus = serde_json::to_vec(&data).unwrap();
    let overlays = passing_attestation_overlay(Value::Null);

    let error =
        crate::snapshot::PlatformSnapshot::from_corpus_json_with_overlays(&corpus, &overlays)
            .unwrap_err();

    assert_eq!(
        error.to_string(),
        "parameter passing `\"byref\"` is not one of `by_value`, `by_ref`"
    );
}

/// The attestation is applied by the database, once the served platform version is known; the
/// snapshot only carries it. A snapshot that stamped it would apply it to a corpus the claim
/// was never verified against.
#[test]
fn the_snapshot_carries_an_attestation_without_stamping_it() {
    let snapshot = stamped_snapshot();

    let input = &global_function(&snapshot, "ВвестиСтроку").parameters;
    assert_eq!(input[0].passing, Some(ParamPassing::ByRef), "an explicit record decodes");
    assert_eq!(input[1].passing, None, "no stamping before the version is known");
    let message = global_function(&snapshot, "Сообщить");
    assert_eq!(message.parameters[0].passing, None);
    assert!(message.variants[0].parameters.iter().all(|parameter| parameter.passing.is_none()));
    assert_eq!(snapshot.methods[0].parameters[0].passing, None);
    assert_eq!(snapshot.parameter_passing_attestations.len(), 1);
}
