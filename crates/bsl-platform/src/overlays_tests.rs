use super::*;
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
fn rejects_addition_that_already_exists() {
    let mut data = property_data();
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

#[test]
fn the_curated_attestations_resolve_against_the_types_they_name() {
    let mut data = range_data();
    data["types"].as_array_mut().unwrap().push(json!({
        "name": "ЗаписьJSON", "english_name": "JSONWriter"
    }));

    let attestations = parse_resolved_member_surface_attestations(
        &data,
        include_str!("../data/platform_overlays.json"),
    )
    .unwrap();

    let names: Vec<&str> = attestations.iter().map(|a| a.type_name.as_str()).collect();
    assert_eq!(
        names,
        ["SpreadsheetDocumentRange", "JSONWriter"],
        "the curated document attests exactly the two types of this round"
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
        blocks.len() >= 5,
        "the page documents five sections with a loadable example, found {}",
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
            + super::overlay_schema::parse_member_surface_attestations(block).unwrap().len();
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
