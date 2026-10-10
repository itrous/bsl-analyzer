//! Runtime decoding of a help corpus into a snapshot, on the project's own small
//! corpus fixture (structural names only, self-written texts).

use bsl_platform::{
    PlatformData, PlatformHelp, PlatformHelpOrigin, PlatformHelpRequest, PlatformHelpSourceKind,
    PlatformHelpStatus, PlatformSnapshot,
};
use serde_json::Value;

const FIXTURE: &str = include_str!("fixtures/help/corpus.json");

fn fixture_value() -> Value {
    serde_json::from_str(FIXTURE).unwrap()
}

fn decode(value: &Value) -> Result<PlatformSnapshot, bsl_platform::SnapshotDecodeError> {
    PlatformSnapshot::from_corpus_json(&serde_json::to_vec(value).unwrap())
}

fn served(snapshot: PlatformSnapshot, version: Option<&str>) -> PlatformData {
    PlatformData::from_help(PlatformHelp::loaded(
        PlatformHelpRequest::ExternalPath("corpus.json".into()),
        snapshot,
        PlatformHelpOrigin {
            source: PlatformHelpSourceKind::External,
            location: Some("corpus.json".to_owned()),
            platform_version: version.map(str::to_owned),
            digest: None,
        },
    ))
}

fn method_mut<'a>(value: &'a mut Value, english_name: &str) -> &'a mut Value {
    value["methods"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|m| m["type_name"] == "Array" && m["english_name"] == english_name)
        .unwrap()
}

#[test]
fn fixture_corpus_serves_lookups_docs_and_overlays() {
    let data = served(decode(&fixture_value()).unwrap(), Some("8.3.27.2214"));

    let add = data.get_method("Массив", "Добавить").expect("RU lookup");
    assert_eq!(data.get_method("array", "add").map(|m| m.id), Some(add.id), "EN, any case");
    let docs = data.get_method_docs(add.id).expect("method docs by id");
    assert!(docs.description.contains("appends one element"));
    assert_eq!(docs.params.len(), 1);
    assert_eq!(docs.examples.len(), 1);
    assert_eq!(docs.examples[0].code, "Список.Добавить(1);");
    assert_eq!(docs.examples[0].description.as_deref(), Some("Fixture example."));
    assert_eq!(docs.see_also, vec!["Массив.Вставить".to_owned()]);

    let strlen = data.get_global_function("strlen").expect("global function");
    assert_eq!(
        data.get_global_function_docs(strlen.id).and_then(|d| d.notes),
        Some("Fixture note.".to_owned())
    );
    let ctor = data.get_constructors("Массив");
    assert_eq!(ctor.len(), 1);
    assert!(data.get_constructor_docs(ctor[0].id).is_some());
    let global = data.get_global_property("FixtureGlobal").expect("global property");
    assert!(data.get_property_docs(global.id).is_some());

    // The curated overlay attached its addition to the fixture's form type.
    assert!(data.get_property("ClientApplicationForm", "РежимОткрытияОкна").is_some());
    // And widened an overridden parameter to the whole metadata-object family.
    let contains = data.get_method("MetadataObjectCollection", "Contains").unwrap();
    let widened = contains.parameters[0].param_type.as_deref().unwrap();
    assert!(widened.contains("ОбъектМетаданных: "), "{widened}");

    assert_eq!(data.help_status_for_target(Some("8.3.27")), PlatformHelpStatus::Available);
    assert_eq!(data.help_status_for_target(Some("8.3.28")), PlatformHelpStatus::UnsupportedTarget);
}

#[test]
fn snapshot_diff_detects_changed_description_and_alias() {
    let base = decode(&fixture_value()).unwrap();

    let mut changed_text = fixture_value();
    method_mut(&mut changed_text, "Add")["documentation"]["description"] =
        Value::String("Fixture text: something else.".to_owned());
    assert_ne!(decode(&changed_text).unwrap(), base, "a changed description must be visible");

    let mut changed_alias = fixture_value();
    method_mut(&mut changed_alias, "Add")["name"] = Value::String("ДобавитьЭлемент".to_owned());
    let renamed = served(decode(&changed_alias).unwrap(), None);
    assert_ne!(decode(&changed_alias).unwrap(), base, "a changed alias must be visible");
    assert!(renamed.get_method("Массив", "Добавить").is_none());
    assert!(renamed.get_method("Массив", "ДобавитьЭлемент").is_some());
}

#[test]
fn unknown_version_is_unverified_and_missing_serves_nothing() {
    let unverified = served(decode(&fixture_value()).unwrap(), None);
    assert_eq!(unverified.help_status_for_target(Some("8.3.27")), PlatformHelpStatus::Unverified);
    assert!(unverified.get_method("Массив", "Добавить").is_some(), "facts stay usable");

    let missing =
        PlatformData::from_help(PlatformHelp::missing(PlatformHelpRequest::None, "disabled"));
    assert_eq!(missing.help_status_for_target(None), PlatformHelpStatus::Missing);
    assert_eq!(missing.help_missing_reason(), Some("disabled"));
    assert!(missing.all_methods().is_empty());
    assert!(missing.get_method("Массив", "Добавить").is_none());
    assert!(missing.get_keyword_docs("Если").is_some(), "static keyword docs stay");
}

#[test]
fn broken_corpus_is_rejected_whole() {
    assert!(PlatformSnapshot::from_corpus_json(b"{ not json").is_err());
    assert!(PlatformSnapshot::from_corpus_json(b"[]").is_err());

    let mut not_array = fixture_value();
    not_array["methods"] = Value::String("x".to_owned());
    assert!(decode(&not_array).unwrap_err().0.contains("must be an array"));

    let mut duplicate = fixture_value();
    let first_id = duplicate["methods"][0]["id"].clone();
    method_mut(&mut duplicate, "Count")["id"] = first_id;
    assert!(decode(&duplicate).unwrap_err().0.contains("duplicate method id"));

    let mut orphan = fixture_value();
    method_mut(&mut orphan, "Count")["type_name"] = Value::String("NoSuchType".to_owned());
    assert!(decode(&orphan).unwrap_err().0.contains("unknown type"));

    let mut no_id = fixture_value();
    method_mut(&mut no_id, "Count").as_object_mut().unwrap().remove("id");
    assert!(decode(&no_id).unwrap_err().0.contains("no numeric id"));

    // Present fields of the wrong kind and missing names are corruption, not
    // defaults.
    let mut numeric_name = fixture_value();
    method_mut(&mut numeric_name, "Count")["english_name"] = serde_json::json!(42);
    assert!(decode(&numeric_name).unwrap_err().0.contains("english_name must be a string"));
    let mut broken_params = fixture_value();
    method_mut(&mut broken_params, "Add")["parameters"] = serde_json::json!("broken");
    assert!(decode(&broken_params).unwrap_err().0.contains("parameters must be"));
    let mut nameless = fixture_value();
    method_mut(&mut nameless, "Count").as_object_mut().unwrap().remove("name");
    assert!(decode(&nameless).unwrap_err().0.contains(".name is missing"));
    let mut bad_docs = fixture_value();
    method_mut(&mut bad_docs, "Add")["documentation"]["see_also"] = serde_json::json!([1]);
    assert!(decode(&bad_docs).unwrap_err().0.contains("documentation"));
    let mut bad_context = fixture_value();
    method_mut(&mut bad_context, "Add")["context"]["server"] = serde_json::json!("yes");
    assert!(decode(&bad_context).unwrap_err().0.contains("context"));

    let mut nameless_param_docs = fixture_value();
    method_mut(&mut nameless_param_docs, "Add")["documentation"]["param_descriptions"] =
        serde_json::json!([{"description": "x"}]);
    assert!(decode(&nameless_param_docs).unwrap_err().0.contains("documentation"));

    // `null` stands for an absent optional value at every depth.
    let mut null_variant_params = fixture_value();
    method_mut(&mut null_variant_params, "Add")["variants"] =
        serde_json::json!([{"variant_name": "A", "parameters": null}]);
    assert!(decode(&null_variant_params).is_ok());

    // A corpus the curated overlay cannot apply to is refused, not half-applied.
    let mut overlay_target_gone = fixture_value();
    overlay_target_gone["methods"]
        .as_array_mut()
        .unwrap()
        .retain(|m| m["type_name"] != "DOMDocument");
    assert!(decode(&overlay_target_gone).unwrap_err().0.contains("platform overlay"));
}

#[test]
fn synthesized_docs_gap_method_needs_a_free_id() {
    use bsl_platform::{GlobalFunction, PlatformMethod, PlatformType};
    let ty = PlatformType {
        name: "МенеджерОбработкиСтрокиXML".into(),
        english_name: "XMLStringProcessingManager".into(),
        min_version: None,
        context: None,
        iter_element_types: vec![],
        xdto_name: None,
    };
    let source = GlobalFunction {
        id: 0,
        name: "УдалитьНедопустимыеСимволыXML".into(),
        english_name: "DeleteDisallowedXMLCharacters".into(),
        return_type: None,
        parameters: vec![],
        variants: vec![],
        min_version: None,
        context: None,
    };
    let top = PlatformMethod {
        id: u32::MAX,
        type_name: "XMLStringProcessingManager".into(),
        name: "Найти".into(),
        english_name: "Find".into(),
        return_type: None,
        parameters: vec![],
        variants: vec![],
        min_version: None,
        context: None,
    };
    let snapshot = PlatformSnapshot {
        types: vec![ty],
        methods: vec![top],
        global_functions: vec![source],
        ..PlatformSnapshot::default()
    };
    let data = served(snapshot.clone(), None);
    assert_eq!(data.all_methods().len(), 1, "no id left: nothing is synthesized");

    let mut room = snapshot;
    room.methods[0].id = 7;
    let data = served(room, None);
    let synthesized =
        data.get_method("XMLStringProcessingManager", "УдалитьНедопустимыеСимволыXML");
    assert_eq!(synthesized.map(|m| m.id), Some(8));
}
