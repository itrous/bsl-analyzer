//! The corpus compiled into the analyzer holds interface facts only. The check
//! is structural: every key of every record is on an allow-list kept here,
//! independently of the generator script, so a text field of any name — a
//! description, a parameter note, an example — fails the build's tests rather
//! than entering the binary.

use bsl_platform::{
    PlatformData, PlatformHelp, PlatformHelpRequest, PlatformHelpSourceKind, PlatformHelpStatus,
    BUNDLED_PLATFORM_VERSION,
};
use serde_json::Value;

const FACTS: &str = include_str!("../data/platform_facts.json");

/// The type the curated property correction and the first attestation name.
const RANGE_TYPE: &str = "ОбластьЯчеекТабличногоДокумента";
const RANGE_EN: &str = "SpreadsheetDocumentRange";

const RECORD_KEYS: &[(&str, &[&str])] = &[
    (
        "types",
        &["name", "english_name", "min_version", "context", "iter_element_types", "xdto_name"],
    ),
    (
        "methods",
        &[
            "id",
            "type_name",
            "name",
            "english_name",
            "return_type",
            "parameters",
            "variants",
            "min_version",
            "context",
            "documentation",
        ],
    ),
    (
        "global_functions",
        &[
            "id",
            "name",
            "english_name",
            "return_type",
            "parameters",
            "variants",
            "min_version",
            "context",
            "documentation",
        ],
    ),
    (
        "constructors",
        &[
            "id",
            "type_name",
            "variant_name",
            "parameters",
            "min_version",
            "context",
            "documentation",
        ],
    ),
    (
        "properties",
        &[
            "id",
            "type_name",
            "name",
            "english_name",
            "property_types",
            "is_readonly",
            "min_version",
            "context",
            "documentation",
        ],
    ),
];
const DOCUMENTATION_KEYS: &[&str] = &["syntax"];
const PARAMETER_KEYS: &[&str] = &["name", "param_type", "is_optional", "is_variadic"];
const VARIANT_KEYS: &[&str] = &["variant_name", "parameters"];
const CONTEXT_KEYS: &[&str] = &[
    "thick_client",
    "thin_client",
    "web_client",
    "server",
    "mobile_client",
    "external_connection",
];

fn only_keys(where_: &str, value: &Value, allowed: &[&str], problems: &mut Vec<String>) {
    let Some(object) = value.as_object() else {
        problems.push(format!("{where_} must be an object"));
        return;
    };
    for key in object.keys() {
        if !allowed.contains(&key.as_str()) {
            problems.push(format!("{where_}.{key} is not an interface fact"));
        }
    }
}

fn parameters(where_: &str, value: Option<&Value>, problems: &mut Vec<String>) {
    for (index, parameter) in value.and_then(Value::as_array).into_iter().flatten().enumerate() {
        only_keys(&format!("{where_}.parameters[{index}]"), parameter, PARAMETER_KEYS, problems);
    }
}

#[test]
fn the_built_in_corpus_holds_interface_facts_and_no_texts() {
    let facts: Value = serde_json::from_str(FACTS).expect("the built-in corpus is JSON");
    let root = facts.as_object().expect("the corpus root is an object");
    let mut problems = Vec::new();
    for section in root.keys() {
        if !RECORD_KEYS.iter().any(|(name, _)| name == section) {
            problems.push(format!("unexpected section {section}"));
        }
    }
    for (section, allowed) in RECORD_KEYS {
        let records = root.get(*section).and_then(Value::as_array);
        let records = records.unwrap_or_else(|| panic!("section {section} is an array"));
        assert!(!records.is_empty(), "section {section} is not empty");
        for (index, record) in records.iter().enumerate() {
            let where_ = format!("{section}[{index}]");
            only_keys(&where_, record, allowed, &mut problems);
            if let Some(documentation) = record.get("documentation") {
                only_keys(
                    &format!("{where_}.documentation"),
                    documentation,
                    DOCUMENTATION_KEYS,
                    &mut problems,
                );
                if !documentation.get("syntax").is_some_and(Value::is_string) {
                    problems.push(format!("{where_}.documentation.syntax must be a string"));
                }
            }
            if let Some(context) = record.get("context").filter(|c| !c.is_null()) {
                only_keys(&format!("{where_}.context"), context, CONTEXT_KEYS, &mut problems);
            }
            parameters(&where_, record.get("parameters"), &mut problems);
            for (variant_index, variant) in
                record.get("variants").and_then(Value::as_array).into_iter().flatten().enumerate()
            {
                let variant_where = format!("{where_}.variants[{variant_index}]");
                only_keys(&variant_where, variant, VARIANT_KEYS, &mut problems);
                parameters(&variant_where, variant.get("parameters"), &mut problems);
            }
        }
    }
    assert!(
        problems.is_empty(),
        "{} problem(s), first: {:#?}",
        problems.len(),
        &problems[..problems.len().min(10)]
    );
}

/// The allow-list is what rejects a text: a record with a description must fail it.
#[test]
fn the_structural_check_rejects_a_description() {
    let mut problems = Vec::new();
    let record = serde_json::json!({
        "id": 1, "type_name": "Array", "name": "Добавить", "english_name": "Add",
        "documentation": {"syntax": "Добавить(<Значение>)", "description": "Adds."}
    });
    let (_, allowed) = RECORD_KEYS.iter().find(|(name, _)| *name == "methods").unwrap();
    only_keys("methods[0]", &record, allowed, &mut problems);
    only_keys(
        "methods[0].documentation",
        &record["documentation"],
        DOCUMENTATION_KEYS,
        &mut problems,
    );
    assert_eq!(problems, vec!["methods[0].documentation.description is not an interface fact"]);
}

#[test]
fn the_built_in_facts_serve_signatures_without_texts() {
    let help = PlatformHelp::without_io(&PlatformHelpRequest::Bundled).unwrap();
    let origin = help.origin.clone().unwrap_or_else(|| panic!("{:?}", help.missing_reason));
    assert_eq!(origin.source, PlatformHelpSourceKind::Bundled);
    assert_eq!(origin.platform_version.as_deref(), Some(BUNDLED_PLATFORM_VERSION));
    assert!(help.missing_reason.is_none());
    let data = PlatformData::from_help(help);
    assert_eq!(data.help_status_for_target(Some("8.3.27")), PlatformHelpStatus::Available);
    assert_eq!(data.help_status_for_target(Some("8.3.28")), PlatformHelpStatus::UnsupportedTarget);

    // Facts: names in both languages, signature, parameters, return type,
    // version and context, and the curated overlays on top.
    let add = data.get_method("Массив", "Добавить").expect("RU lookup");
    assert_eq!(data.get_method("Array", "Add").map(|m| m.id), Some(add.id));
    assert_eq!(add.parameters.len(), 1);
    assert_eq!(add.parameters[0].name, "Значение");
    assert!(add.min_version.is_some() && add.context.is_some());
    let docs = data.get_method_docs(add.id).expect("the signature line is kept");
    assert!(docs.syntax.starts_with("Добавить("), "{}", docs.syntax);
    assert!(docs.description.is_empty() && docs.params.is_empty() && docs.examples.is_empty());
    assert!(docs.notes.is_none() && docs.see_also.is_empty());

    let strlen = data.get_global_function("СтрДлина").expect("global function");
    assert_eq!(strlen.english_name, "StrLen");
    assert_eq!(strlen.return_type.as_deref(), Some("Число"));
    let ctor = data.get_constructors("Массив");
    assert!(!ctor.is_empty(), "constructors are facts");
    let count = data.get_property("Массив", "Количество");
    assert!(count.is_none(), "a method is not a property");
    assert!(data.get_type("ТаблицаЗначений").is_some() && data.get_type("ValueTable").is_some());

    // Texts of every record kind are empty, not just the method above.
    assert!(data.all_methods().iter().all(|m| data.get_method_docs(m.id).is_none_or(|d| d
        .description
        .is_empty()
        && d.params.is_empty()
        && d.examples.is_empty())));
    assert!(data.all_properties().iter().all(|p| data
        .get_property_docs(p.id)
        .is_none_or(|d| d.description.is_empty() && d.notes.is_none())));
    assert!(data.all_constructors().iter().all(|c| data
        .get_constructor_docs(c.id)
        .is_none_or(|d| d.description.is_empty() && d.params.is_empty() && d.examples.is_empty())));
}

/// The curated corrections and attestations of `platform_overlays.json` are
/// authored against the built-in facts, and a typo in either would leave them
/// silently unapplied — the overlay skips a target its corpus does not have.
/// Every fact here is therefore asserted against the built-in corpus: the
/// targets exist, and the values the entries name are the ones that come out.
#[test]
fn the_curated_overlays_reach_the_built_in_facts() {
    let help = PlatformHelp::without_io(&PlatformHelpRequest::Bundled).unwrap();
    let data = PlatformData::from_help(help);

    // The correction's target exists and carries the corrected fact; the record
    // keeps its identity, so documentation lookups still find it.
    let value = data
        .get_property("SpreadsheetDocumentRange", "Значение")
        .expect("the correction's target must exist in the built-in facts");
    assert_eq!(value.id, 12748);
    assert!(value.is_readonly, "ОбластьЯчеекТабличногоДокумента.Значение is read-only at run time");
    assert_eq!(
        data.get_property("ОбластьЯчеекТабличногоДокумента", "Value").map(|p| p.id),
        Some(12748)
    );

    // Only the named property moved: its neighbours and the identically named
    // property of the drawing keep their extracted flags.
    assert!(!data.get_property("SpreadsheetDocumentRange", "Текст").unwrap().is_readonly);
    assert!(
        !data.get_property("SpreadsheetDocumentDrawing", "Значение").unwrap().is_readonly,
        "the drawing's Значение is not part of the correction"
    );

    // The attested surfaces, under both spellings of their names.
    for name in [RANGE_TYPE, RANGE_EN] {
        assert!(
            data.platform_type_member_surface_verified(name),
            "{name} is attested and the built-in facts are of the attested release"
        );
    }
    for name in ["ЗаписьJSON", "JSONWriter"] {
        assert!(data.platform_type_member_surface_verified(name), "{name} is attested");
    }
    // ...and the types nobody attested stay open, however complete they look.
    assert!(!data.platform_type_member_surface_verified("ТабличныйДокумент"));
    assert!(!data.platform_type_member_surface_verified("Запрос"));
    assert!(!data.platform_type_member_surface_verified("ЗаписьJSONНеТакой"));

    // The members the attestation covers are the ones the positive tests call
    // absent: were the corpus to gain them, those tests would have to change.
    assert_eq!(
        data.get_type_methods("JSONWriter").len(),
        11,
        "the attested surface is the 8.3 one"
    );
    assert_eq!(data.get_type_properties("SpreadsheetDocumentRange").len(), 63);
    assert!(data.get_property("SpreadsheetDocumentRange", "ТекущаяОбласть").is_none());
    assert!(data.get_method("JSONWriter", "ЗаписатьНачалоТаблицы").is_none());
}

/// The curating rule of an attestation, held by a test rather than by prose in
/// `OVERLAYS.md`: a type whose extract had to be patched has no surface the
/// extract may call complete, and neither has a type a configuration extends.
///
/// The form half is a name check with a stated limit: a managed form gains
/// properties from its per-MDO extensions (`form_extension_property_names` in
/// `crates/bsl-platform/src/db.rs`), and the extract knows nothing of them, so
/// such a type must not be attested. The corpus spells those names both with and
/// without a space (`DataComposition…`, `…КомпоновкиДанных`), so the comparison
/// drops whitespace on both sides. A type whose name hides that it is extended
/// would still slip through (the corpus names are not a classification), which is
/// why the check is written down here as the rule's ceiling rather than as a
/// proof of it.
#[test]
fn curated_attestations_stay_off_types_the_extract_cannot_complete() {
    const EXTENDED_TYPE_MARKERS: &[&str] =
        &["form", "форма", "extension", "расширение", "datacomposition", "компоновкаданных"];

    fn normalized(name: &str) -> String {
        name.to_lowercase().chars().filter(|c| !c.is_whitespace()).collect()
    }

    let document: Value =
        serde_json::from_str(include_str!("../data/platform_overlays.json")).expect("overlay JSON");
    let entries = document["type_member_surface_attestations"]
        .as_array()
        .expect("the curated document carries the attestations");
    assert!(!entries.is_empty(), "the rule would be vacuous without curated attestations");

    let synthesized = bsl_platform::docs_gap_synthesized_member_types();
    assert!(
        !synthesized.is_empty(),
        "an empty synthesis list would let this check pass on any attestation"
    );

    let help = PlatformHelp::without_io(&PlatformHelpRequest::Bundled).unwrap();
    let data = PlatformData::from_help(help);
    for entry in entries {
        let name = entry["type"].as_str().expect("an attested type is named");
        let ty = data
            .get_type(name)
            .unwrap_or_else(|| panic!("attested type {name} must exist in the built-in facts"));
        assert!(
            !synthesized.iter().any(|patched| *patched == ty.english_name.as_str()),
            "{name} is a type whose member list the docs-gap overlays synthesize: \
             its extract is patched, so it has no attested-complete surface"
        );
        for alias in [ty.name.as_str(), ty.english_name.as_str()] {
            let folded = normalized(alias);
            assert!(
                !EXTENDED_TYPE_MARKERS.iter().any(|marker| folded.contains(marker)),
                "{alias} is extended by a configuration, so the extract cannot list its members"
            );
        }
    }
}

/// The marker check is only worth its name if it catches the corpus spellings: the
/// data-composition family is the biggest extended one (251 types), and its names are
/// written without a space. The check is exercised on the corpus's own names, not on
/// invented ones.
#[test]
fn the_extended_type_marker_check_catches_the_corpus_spellings() {
    fn normalized(name: &str) -> String {
        name.to_lowercase().chars().filter(|c| !c.is_whitespace()).collect()
    }
    const EXTENDED_TYPE_MARKERS: &[&str] =
        &["form", "форма", "extension", "расширение", "datacomposition", "компоновкаданных"];

    let help = PlatformHelp::without_io(&PlatformHelpRequest::Bundled).unwrap();
    let data = PlatformData::from_help(help);
    let is_extended = |name: &str| {
        data.get_type(name).is_some_and(|ty| {
            [ty.name.as_str(), ty.english_name.as_str()]
                .iter()
                .any(|alias| EXTENDED_TYPE_MARKERS.iter().any(|m| normalized(alias).contains(m)))
        })
    };

    let marked = data
        .all_types()
        .iter()
        .filter(|ty| {
            [ty.name.as_str(), ty.english_name.as_str()]
                .iter()
                .any(|alias| EXTENDED_TYPE_MARKERS.iter().any(|m| normalized(alias).contains(m)))
        })
        .count();
    assert!(
        marked > 250,
        "the corpus names a whole extended family with these markers, found {marked}"
    );

    // Real spellings of every marker, including the ones a spaced comparison missed:
    // the data-composition family and the extension types are written both ways.
    for name in [
        "DataCompositionSettings",
        "НастройкиКомпоновкиДанных",
        "ФормаКлиентскогоПриложения",
        "ClientApplicationForm",
        "Расширение табличного поля отбора",
        "Filter table box extension",
    ] {
        assert!(
            data.get_type(name).is_some(),
            "{name} must exist in the built-in facts for this check to mean anything"
        );
        assert!(is_extended(name), "{name} must be recognized as a type a configuration extends");
    }
}
