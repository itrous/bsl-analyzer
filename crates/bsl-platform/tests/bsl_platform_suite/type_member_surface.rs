//! `type_member_surface_attestations`: what makes an absent member of a
//! platform type a verdict. The attestation data is curated, so these tests
//! drive it with a corpus of their own and check the two facts the gate must
//! not lose — the served corpus must be of the attested release or newer, and
//! the name must denote a single type.

use bsl_platform::{
    PlatformData, PlatformHelp, PlatformHelpOrigin, PlatformHelpRequest, PlatformHelpSourceKind,
    PlatformSnapshot,
};
use serde_json::{json, Value};

const RANGE_TYPE: &str = "ОбластьЯчеекТабличногоДокумента";
const RANGE_EN: &str = "SpreadsheetDocumentRange";

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
    assert!(data.platform_type_member_surface_verified(RANGE_TYPE));
    assert!(
        data.platform_type_member_surface_verified(RANGE_EN),
        "both spellings of the type name denote the attested surface"
    );

    let newer = served(Some("8.3.28.1"));
    assert!(
        newer.platform_type_member_surface_verified(RANGE_TYPE),
        "a newer corpus keeps the surface"
    );
    assert!(newer.platform_type_member_surface_verified(RANGE_EN));

    // The attested release is 8.3.27 and the corpus is a deeper version of it:
    // `8.3.27.2214` must not read as older than `8.3.27`.
    let patch = served(Some("8.3.27.2214"));
    assert!(patch.platform_type_member_surface_verified(RANGE_TYPE));
}

#[test]
fn an_older_or_unknown_corpus_is_not_verified() {
    let older = served(Some("8.3.26.3"));
    assert!(
        !older.platform_type_member_surface_verified(RANGE_TYPE),
        "a corpus older than the attestation may have lost the member it never covered"
    );
    let unknown = served(None);
    assert!(
        !unknown.platform_type_member_surface_verified(RANGE_TYPE),
        "without a version the corpus says nothing about completeness"
    );
}

#[test]
fn an_ambiguous_name_is_never_verified() {
    let data = served(Some("8.3.27"));
    assert!(
        data.platform_type_member_surface_verified("NamesakeA"),
        "the namesake is attested under the spelling only it has"
    );
    assert!(
        !data.platform_type_member_surface_verified("Тёзка"),
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
        !data.platform_type_member_surface_verified("ЗаведомоНесуществующийТип"),
        "the entry names no type of this corpus"
    );
    assert!(!data.platform_type_member_surface_verified("ОбычныйТип"));
    assert!(!data.platform_type_member_surface_verified(""));
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
            !data.platform_type_member_surface_verified(name),
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
        served_with_curated_sections().platform_type_member_surface_verified(RANGE_TYPE),
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
