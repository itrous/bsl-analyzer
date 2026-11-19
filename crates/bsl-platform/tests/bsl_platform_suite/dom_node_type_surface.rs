use bsl_platform::{
    PlatformData, PlatformHelp, PlatformHelpOrigin, PlatformHelpRequest, PlatformHelpSourceKind,
    PlatformSnapshot,
};
use serde_json::Value;

const FACTS: &str = include_str!("../../data/platform_facts.json");
const OVERLAYS: &str = include_str!("../../data/platform_overlays.json");

fn served(version: Option<&str>, edit: impl FnOnce(&mut Value)) -> PlatformData {
    let mut corpus: Value = serde_json::from_str(FACTS).unwrap();
    edit(&mut corpus);
    let snapshot = PlatformSnapshot::from_corpus_json_with_overlays(
        &serde_json::to_vec(&corpus).unwrap(),
        OVERLAYS,
    )
    .unwrap();
    PlatformData::from_help(PlatformHelp::loaded(
        PlatformHelpRequest::ExternalPath("dom-values.json".into()),
        snapshot,
        PlatformHelpOrigin {
            source: PlatformHelpSourceKind::External,
            location: Some("dom-values.json".to_owned()),
            platform_version: version.map(str::to_owned),
            digest: None,
        },
    ))
}

#[test]
fn dom_surface_requires_the_attested_release_and_complete_value_set() {
    let data = served(Some("8.3.27"), |_| {});
    for name in ["DOMNodeType", "ТипУзлаDOM", "domnodetype"] {
        assert!(data.platform_type_member_surface_verified(name), "{name}");
    }
    assert_eq!(data.get_type_properties("DOMNodeType").len(), 13);
    assert!(data.get_type_methods("DOMNodeType").is_empty());
    assert!(data.get_property("DOMNodeType", "Element").is_some());
    assert!(data.get_property("DOMNodeType", "УзелЭлемента").is_none());
    for version in [None, Some("8.3.26"), Some("unknown")] {
        assert!(!served(version, |_| {}).platform_type_member_surface_verified("DOMNodeType"));
    }
}

#[test]
fn a_truncated_dom_surface_cannot_prove_a_missing_value() {
    let data = served(Some("8.3.27"), |corpus| {
        corpus["properties"].as_array_mut().unwrap().retain(|property| property["id"] != 2474);
    });
    assert_eq!(data.get_type_properties("DOMNodeType").len(), 12);
    assert!(!data.platform_type_member_surface_verified("DOMNodeType"));
    assert!(data.platform_type_member_surface_verified("QuestionDialogMode"));
}

#[test]
fn replacing_a_dom_value_is_not_the_attested_surface_even_at_the_same_size() {
    let data = served(Some("8.3.27"), |corpus| {
        let value = corpus["properties"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|property| property["id"] == 2474)
            .unwrap();
        value["name"] = Value::String("ДругойЭлемент".to_owned());
        value["english_name"] = Value::String("OtherElement".to_owned());
    });
    assert_eq!(data.get_type_properties("DOMNodeType").len(), 13);
    assert!(!data.platform_type_member_surface_verified("DOMNodeType"));
}

#[test]
fn a_namesake_does_not_borrow_the_dom_surface() {
    let data = served(Some("8.3.27"), |corpus| {
        corpus["types"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({"name":"ТипУзлаDOM","english_name":"OtherOwner"}));
    });
    assert!(data.is_ambiguous_type_name("ТипУзлаDOM"));
    assert!(!data.platform_type_member_surface_verified("ТипУзлаDOM"));
    assert!(!data.platform_type_member_surface_verified("OtherOwner"));
    assert!(data.platform_type_member_surface_verified("DOMNodeType"));
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn the_attested_dom_members_are_the_pinned_corpus_surface() {
    use sha2::{Digest, Sha256};
    let path = std::env::var("BSL_PLATFORM_HELP_CORPUS").expect("a pinned corpus path is required");
    let corpus = std::fs::read(&path).expect("the pinned corpus must be readable");
    assert_eq!(
        Sha256::digest(&corpus).iter().map(|byte| format!("{byte:02x}")).collect::<String>(),
        "3c759994cbd82a1522b1c497d9f2ef68c77b776d5c6723ba5a72623b570cacce"
    );
    let snapshot = PlatformSnapshot::from_corpus_json_with_overlays(&corpus, OVERLAYS).unwrap();
    let data = PlatformData::from_help(PlatformHelp::loaded(
        PlatformHelpRequest::ExternalPath(path.clone().into()),
        snapshot,
        PlatformHelpOrigin {
            source: PlatformHelpSourceKind::External,
            location: Some(path),
            platform_version: Some("8.3.27".to_owned()),
            digest: None,
        },
    ));
    assert_eq!(data.get_type_methods("DOMNodeType").len(), 0);
    let mut members: Vec<_> = data
        .get_type_properties("DOMNodeType")
        .into_iter()
        .map(|property| (property.id, property.name.as_str(), property.english_name.as_str()))
        .collect();
    members.sort_by_key(|member| member.0);
    assert_eq!(
        members,
        [
            (2468, "Атрибут", "Attribute"),
            (2469, "СекцияCDATA", "CDATASection"),
            (2470, "Комментарий", "Comment"),
            (2471, "Документ", "Document"),
            (2472, "ФрагментДокумента", "DocumentFragment"),
            (2473, "ОпределениеТипаДокумента", "DocumentType"),
            (2474, "Элемент", "Element"),
            (2475, "Сущность", "Entity"),
            (2476, "СсылкаНаСущность", "EntityReference"),
            (2477, "Нотация", "Notation"),
            (2478, "ИнструкцияОбработки", "ProcessingInstruction"),
            (2479, "Текст", "Text"),
            (2480, "ПространствоИменXPath", "XPathNamespace"),
        ]
    );
    assert!(data.platform_type_member_surface_verified("DOMNodeType"));
}
