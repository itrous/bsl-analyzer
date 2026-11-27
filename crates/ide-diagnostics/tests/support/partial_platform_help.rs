use bsl_platform::{
    PlatformHelp, PlatformHelpOrigin, PlatformHelpRequest, PlatformHelpSourceKind, PlatformSnapshot,
};

/// A served snapshot that keeps the callback description type and its constructor but
/// renames the handler parameter beyond recognition. The resolver can still prove arity
/// verdicts from the five-parameter signature, while
/// [`bsl_platform::has_callback_description_constructor_facts`] stays false, so semantic
/// normalization runs with partial callback facts.
#[allow(
    dead_code,
    reason = "The shared support module is included by suites that serve different snapshots."
)]
pub fn unreadable_callback_constructor_snapshot() -> PlatformHelp {
    let mut snapshot = PlatformSnapshot::from_corpus_json(include_bytes!(
        "../../../bsl-platform/data/platform_facts.json"
    ))
    .expect("bundled platform facts decode");
    let constructor = snapshot
        .constructors
        .iter_mut()
        .find(|constructor| constructor.type_name.eq_ignore_ascii_case("CallbackDescription"))
        .expect("bundled callback constructor exists");
    constructor.parameters[0].name = "НеизвестныйПараметр".into();
    PlatformHelp::loaded(
        PlatformHelpRequest::ExternalPath("partial-unreadable-callback-constructor".into()),
        snapshot,
        PlatformHelpOrigin {
            source: PlatformHelpSourceKind::External,
            location: None,
            platform_version: None,
            digest: None,
        },
    )
}

#[allow(
    dead_code,
    reason = "The shared support module is included by suites that serve different snapshots."
)]
pub fn array_only_snapshot() -> PlatformHelp {
    let mut snapshot = PlatformSnapshot::from_corpus_json(include_bytes!(
        "../../../bsl-platform/data/platform_facts.json"
    ))
    .expect("bundled platform facts decode");
    snapshot.types.retain(|ty| ty.name == "Массив");
    snapshot.constructors.retain(|constructor| constructor.type_name == "Массив");
    PlatformHelp::loaded(
        PlatformHelpRequest::ExternalPath("partial-array-only".into()),
        snapshot,
        PlatformHelpOrigin {
            source: PlatformHelpSourceKind::External,
            location: None,
            platform_version: None,
            digest: None,
        },
    )
}
