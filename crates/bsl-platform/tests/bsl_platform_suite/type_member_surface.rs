//! `type_member_surface_attestations`: what makes an absent member of a
//! platform type a verdict. The attestation data is curated, so these tests
//! drive it with a corpus of their own and check the two facts the gate must
//! not lose — the served corpus must be of the attested release or newer, and
//! the name must denote a single type.

use bsl_platform::{
    MemberSurface, PlatformData, PlatformHelp, PlatformHelpOrigin, PlatformHelpRequest,
    PlatformHelpSourceKind, PlatformSnapshot,
};
use serde_json::{json, Value};

const RANGE_TYPE: &str = "ОбластьЯчеекТабличногоДокумента";
const RANGE_EN: &str = "SpreadsheetDocumentRange";

/// What the gate answers for the methods and for the fields of `name`, each
/// asked on its own.
fn surfaces_of(data: &PlatformData, name: &str) -> (bool, bool) {
    (
        data.platform_type_member_surface_verified(name, MemberSurface::Methods),
        data.platform_type_member_surface_verified(name, MemberSurface::Fields),
    )
}

const BOTH: (bool, bool) = (true, true);
const NEITHER: (bool, bool) = (false, false);

/// A corpus with one unambiguous attested type, one homonym pair sharing a
/// Russian name, and one type nobody attests.
fn corpus() -> Vec<u8> {
    let corpus = json!({
        "types": [
            {"name": RANGE_TYPE, "english_name": RANGE_EN, "min_version": "8.2"},
            {"name": "Тёзка", "english_name": "NamesakeA"},
            {"name": "Тёзка", "english_name": "NamesakeB"},
            {"name": "ОбычныйТип", "english_name": "OrdinaryType"}
        ],
        "methods": [
            {"id": 1, "type_name": RANGE_EN, "name": "Объединить", "english_name": "Merge"},
            {"id": 2, "type_name": "NamesakeA", "name": "Метод", "english_name": "Method"},
            {"id": 3, "type_name": "NamesakeB", "name": "Метод", "english_name": "Method"},
            {"id": 4, "type_name": "OrdinaryType", "name": "Метод", "english_name": "Method"}
        ],
        "properties": [
            {"id": 12748, "type_name": RANGE_EN, "name": "Значение", "english_name": "Value",
             "property_types": ["Произвольный"], "is_readonly": false}
        ]
    });
    serde_json::to_vec(&corpus).unwrap()
}

/// The same attestation document for each case: the range by its Russian name
/// (the spelling inference ends up with), one namesake of the homonym pair by
/// the English spelling only it has, and a type the corpus does not have.
fn attestations() -> String {
    json!({
        "schema_version": 1,
        "method_parameter_overrides": [],
        "type_member_surface_attestations": [
            {
                "type": RANGE_TYPE,
                "verified_against_version": "8.3.27",
                "evidence_source": "test fixture",
                "rationale": "the fixture lists every member of the range"
            },
            {
                "type": "NamesakeA",
                "verified_against_version": "8.3.27",
                "evidence_source": "test fixture",
                "rationale": "a namesake is unambiguously named by the spelling only it has"
            },
            {
                "type": "ЗаведомоНесуществующийТип",
                "verified_against_version": "8.3.27",
                "evidence_source": "test fixture",
                "rationale": "absent from this corpus, so it must not reject it"
            }
        ]
    })
    .to_string()
}

fn served(version: Option<&str>) -> PlatformData {
    let snapshot =
        PlatformSnapshot::from_corpus_json_with_overlays(&corpus(), &attestations()).unwrap();
    PlatformData::from_help(PlatformHelp::loaded(
        PlatformHelpRequest::ExternalPath("attested.json".into()),
        snapshot,
        PlatformHelpOrigin {
            source: PlatformHelpSourceKind::External,
            location: Some("attested.json".to_owned()),
            platform_version: version.map(str::to_owned),
            digest: None,
        },
    ))
}

#[test]
fn an_attested_type_is_verified_at_or_after_the_attested_release() {
    let data = served(Some("8.3.27"));
    assert!(surfaces_of(&data, RANGE_TYPE) == BOTH);
    assert!(
        surfaces_of(&data, RANGE_EN) == BOTH,
        "both spellings of the type name denote the attested surface"
    );

    let newer = served(Some("8.3.28.1"));
    assert!(surfaces_of(&newer, RANGE_TYPE) == BOTH, "a newer corpus keeps the surface");
    assert!(surfaces_of(&newer, RANGE_EN) == BOTH);

    // The attested release is 8.3.27 and the corpus is a deeper version of it:
    // `8.3.27.2214` must not read as older than `8.3.27`.
    let patch = served(Some("8.3.27.2214"));
    assert!(surfaces_of(&patch, RANGE_TYPE) == BOTH);
}

#[test]
fn an_older_or_unknown_corpus_is_not_verified() {
    let older = served(Some("8.3.26.3"));
    assert!(
        surfaces_of(&older, RANGE_TYPE) == NEITHER,
        "a corpus older than the attestation may have lost the member it never covered"
    );
    let unknown = served(None);
    assert!(
        surfaces_of(&unknown, RANGE_TYPE) == NEITHER,
        "without a version the corpus says nothing about completeness"
    );
}

#[test]
fn an_ambiguous_name_is_never_verified() {
    let data = served(Some("8.3.27"));
    assert!(
        surfaces_of(&data, "NamesakeA") == BOTH,
        "the namesake is attested under the spelling only it has"
    );
    assert!(
        surfaces_of(&data, "Тёзка") == NEITHER,
        "the name both namesakes share denotes no single surface"
    );
    assert!(data.is_ambiguous_type_name("Тёзка"), "the fixture really is ambiguous");
    assert!(!data.is_ambiguous_type_name("NamesakeA"));
}

/// An attestation whose type the corpus does not have is dropped: the corpus
/// still loads, and no spelling of that name opens the gate.
#[test]
fn an_attestation_for_an_absent_type_does_not_reject_the_corpus() {
    let data = served(Some("8.3.27"));
    assert!(
        surfaces_of(&data, "ЗаведомоНесуществующийТип") == NEITHER,
        "the entry names no type of this corpus"
    );
    assert!(surfaces_of(&data, "ОбычныйТип") == NEITHER);
    assert!(surfaces_of(&data, "") == NEITHER);
}

/// The corpus has four pairs of records sharing one English name (form and
/// extension twins, and `PredefinedDataUpdate`): the name then denotes neither
/// of them, so an attestation written through either spelling must leave the
/// gate shut for both.
#[test]
fn a_shared_english_name_leaves_the_gate_shut_for_both_records() {
    let corpus = serde_json::to_vec(&json!({
        "types": [
            {"name": "ПервыйТип", "english_name": "Dup"},
            {"name": "ВторойТип", "english_name": "Dup"}
        ],
        "methods": [
            {"id": 1, "type_name": "Dup", "name": "Метод", "english_name": "Method"}
        ],
        "properties": []
    }))
    .unwrap();
    let overlays = json!({
        "schema_version": 1,
        "method_parameter_overrides": [],
        "type_member_surface_attestations": [{
            "type": "ПервыйТип",
            "verified_against_version": "8.3.27",
            "evidence_source": "test fixture",
            "rationale": "the fixture lists every member of the type"
        }]
    })
    .to_string();

    let snapshot = PlatformSnapshot::from_corpus_json_with_overlays(&corpus, &overlays).unwrap();
    assert!(
        snapshot.member_surface_attestations.is_empty(),
        "an attestation through a shared English name covers an arbitrary namesake"
    );

    let data = PlatformData::from_help(PlatformHelp::loaded(
        PlatformHelpRequest::ExternalPath("shared-name.json".into()),
        snapshot,
        PlatformHelpOrigin {
            source: PlatformHelpSourceKind::External,
            location: Some("shared-name.json".to_owned()),
            platform_version: Some("8.3.27".to_owned()),
            digest: None,
        },
    ));
    assert!(data.is_ambiguous_type_name("Dup"), "the fixture really shares the name");
    for name in ["ПервыйТип", "ВторойТип", "Dup"] {
        assert!(
            surfaces_of(&data, name) == NEITHER,
            "{name} must not be verified: the shared name denotes two records"
        );
    }
}

/// The curated document restricted to the two sections of this round: the
/// parameter overrides it also carries target types this small corpus does not
/// have, and their rejection is what the crate's own corpus tests cover.
fn curated_new_sections() -> String {
    let document: Value =
        serde_json::from_str(include_str!("../../data/platform_overlays.json")).unwrap();
    json!({
        "schema_version": document["schema_version"].clone(),
        "method_parameter_overrides": [],
        "type_property_corrections": document["type_property_corrections"].clone(),
        "type_member_surface_attestations": document["type_member_surface_attestations"].clone(),
    })
    .to_string()
}

fn served_with_curated_sections() -> PlatformData {
    let snapshot =
        PlatformSnapshot::from_corpus_json_with_overlays(&corpus(), &curated_new_sections())
            .unwrap();
    PlatformData::from_help(PlatformHelp::loaded(
        PlatformHelpRequest::ExternalPath("attested.json".into()),
        snapshot,
        PlatformHelpOrigin {
            source: PlatformHelpSourceKind::External,
            location: Some("attested.json".to_owned()),
            platform_version: Some("8.3.27.2214".to_owned()),
            digest: None,
        },
    ))
}

/// The curated entries themselves, on a corpus that carries the corrected
/// property: the correction reaches it and the attestation of a type this
/// corpus does not have is dropped rather than rejecting the load.
#[test]
fn the_curated_correction_reaches_a_corpus_that_carries_the_target() {
    let snapshot =
        PlatformSnapshot::from_corpus_json_with_overlays(&corpus(), &curated_new_sections())
            .unwrap();
    let value = snapshot
        .properties
        .iter()
        .find(|p| p.id == 12748)
        .expect("the fixture property survives decoding");
    assert!(value.is_readonly, "ОбластьЯчеекТабличногоДокумента.Значение is read-only at run time");
    assert_eq!(value.name.as_str(), "Значение");
    let types: Vec<&str> = value.property_types.iter().map(|t| t.as_str()).collect();
    assert_eq!(types, ["Произвольный"], "an untouched property keeps its extracted types");
    let attested: Vec<&str> =
        snapshot.member_surface_attestations.iter().map(|a| a.type_name.as_str()).collect();
    assert_eq!(
        attested,
        [RANGE_EN],
        "the curated attestation of this corpus's type is kept; JSONWriter is not in it"
    );
    assert!(
        surfaces_of(&served_with_curated_sections(), RANGE_TYPE) == BOTH,
        "the curated attestation opens the gate on a corpus of the attested release"
    );
}

/// The corpus is one the curated sections cannot apply to at all: no target
/// type is there, and the load still succeeds with every fact it carries.
#[test]
fn a_corpus_without_any_target_loads_unchanged() {
    let corpus: Value = json!({
        "types": [{"name": "СовсемДругой", "english_name": "TotallyDifferent"}],
        "methods": [],
        "properties": []
    });
    let bytes = serde_json::to_vec(&corpus).unwrap();
    let snapshot =
        PlatformSnapshot::from_corpus_json_with_overlays(&bytes, &curated_new_sections()).unwrap();
    assert!(snapshot.member_surface_attestations.is_empty());
    assert!(snapshot.properties.is_empty());
}

/// A corpus with a collection that addresses its elements by name: the entry
/// covers its methods only, and the same collection attested without
/// `verified_surface` covers both.
fn surface_corpus() -> Vec<u8> {
    serde_json::to_vec(&json!({
        "types": [
            {"name": "ИменованнаяКоллекция", "english_name": "NamedCollection"},
            {"name": "ПолнаяКоллекция", "english_name": "FullCollection"}
        ],
        "methods": [
            {"id": 1, "type_name": "NamedCollection", "name": "Количество", "english_name": "Count"},
            {"id": 2, "type_name": "FullCollection", "name": "Количество", "english_name": "Count"}
        ],
        "properties": []
    }))
    .unwrap()
}

fn served_with_surfaces(surface: Option<Value>) -> PlatformData {
    let mut named = json!({
        "type": "NamedCollection",
        "verified_against_version": "8.3.27",
        "evidence_source": "test fixture",
        "rationale": "the elements are addressed by name, so only the methods are complete"
    });
    if let Some(surface) = surface {
        named["verified_surface"] = surface;
    }
    let overlays = json!({
        "schema_version": 1,
        "method_parameter_overrides": [],
        "type_member_surface_attestations": [named, {
            "type": "FullCollection",
            "verified_against_version": "8.3.27",
            "evidence_source": "test fixture",
            "rationale": "no verified_surface: both surfaces are attested"
        }]
    })
    .to_string();
    let snapshot =
        PlatformSnapshot::from_corpus_json_with_overlays(&surface_corpus(), &overlays).unwrap();
    PlatformData::from_help(PlatformHelp::loaded(
        PlatformHelpRequest::ExternalPath("surfaces.json".into()),
        snapshot,
        PlatformHelpOrigin {
            source: PlatformHelpSourceKind::External,
            location: Some("surfaces.json".to_owned()),
            platform_version: Some("8.3.27".to_owned()),
            digest: None,
        },
    ))
}

#[test]
fn an_entry_without_verified_surface_attests_both_surfaces() {
    let data = served_with_surfaces(None);
    for name in ["NamedCollection", "ИменованнаяКоллекция", "FullCollection", "ПолнаяКоллекция"]
    {
        assert!(surfaces_of(&data, name) == BOTH, "{name} carries no verified_surface");
    }
}

#[test]
fn a_methods_only_entry_attests_the_methods_and_not_the_fields() {
    let data = served_with_surfaces(Some(json!(["methods"])));
    for name in ["NamedCollection", "ИменованнаяКоллекция"] {
        assert!(
            surfaces_of(&data, name) == (true, false),
            "{name} is attested for its methods only"
        );
    }
    // Control: the other entry of the same document, without the field, is
    // attested for both, so the narrowing above is the field and nothing else.
    assert!(surfaces_of(&data, "FullCollection") == BOTH);

    let fields_only = served_with_surfaces(Some(json!(["fields"])));
    assert!(surfaces_of(&fields_only, "NamedCollection") == (false, true));
}
