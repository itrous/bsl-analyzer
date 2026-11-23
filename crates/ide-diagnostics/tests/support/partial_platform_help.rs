use bsl_platform::{
    PlatformHelp, PlatformHelpOrigin, PlatformHelpRequest, PlatformHelpSourceKind, PlatformSnapshot,
};

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
