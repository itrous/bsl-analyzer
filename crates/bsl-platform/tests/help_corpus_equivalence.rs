//! Corpus contract: the platform data served at runtime must be exactly what the
//! pinned help corpus describes. The corpus is the one formerly checked in at
//! `crates/bsl-platform/data/platform_data.json`, now supplied through
//! `BSL_PLATFORM_HELP_CORPUS`; the digest pins the observable surface — every
//! record, every docs entry reachable by its id and the by-name lookups — so a
//! change of how the corpus reaches `PlatformData` cannot alter answers
//! unnoticed.

use bsl_platform::PlatformData;
use std::fmt::Write as _;

/// Digest of [`observable_dump`] over the pinned corpus with its overlays.
const CORPUS_OBSERVABLE_DIGEST: &str =
    "c81a35bcf8f794d3c3e7ca774726e5e3f7b543446390fd74105005cdacca53f8";

fn observable_dump(data: &PlatformData) -> String {
    let mut out = String::new();
    for ty in data.all_types() {
        writeln!(out, "type {ty:?}").unwrap();
        let by_ru = data.get_type(&ty.name).map(|t| t.english_name.clone());
        let by_en = data.get_type(&ty.english_name).map(|t| t.english_name.clone());
        writeln!(out, "  lookup {by_ru:?} {by_en:?}").unwrap();
    }
    for method in data.all_methods() {
        writeln!(out, "method {method:?}").unwrap();
        writeln!(out, "  docs {:?}", data.get_method_docs(method.id)).unwrap();
        let by_name = data.get_method(&method.type_name, &method.english_name).map(|m| m.id);
        writeln!(out, "  lookup {by_name:?}").unwrap();
    }
    for function in data.all_global_functions() {
        writeln!(out, "function {function:?}").unwrap();
        writeln!(out, "  docs {:?}", data.get_global_function_docs(function.id)).unwrap();
        let by_ru = data.get_global_function(&function.name).map(|f| f.id);
        writeln!(out, "  lookup {by_ru:?}").unwrap();
    }
    for ctor in data.all_constructors() {
        writeln!(out, "constructor {ctor:?}").unwrap();
        writeln!(out, "  docs {:?}", data.get_constructor_docs(ctor.id)).unwrap();
    }
    for prop in data.all_properties() {
        writeln!(out, "property {prop:?}").unwrap();
        writeln!(out, "  docs {:?}", data.get_property_docs(prop.id)).unwrap();
        let by_name = data.get_property(&prop.type_name, &prop.english_name).map(|p| p.id);
        writeln!(out, "  lookup {by_name:?}").unwrap();
    }
    for keyword in ["Если", "For", "Попытка", "ВызватьИсключение"] {
        writeln!(out, "keyword {:?}", data.get_keyword_docs(keyword)).unwrap();
    }
    out
}

/// SHA-256 of the pinned corpus JSON: the input of the corpus-contract suite.
const PINNED_CORPUS_SHA256: &str =
    "3c759994cbd82a1522b1c497d9f2ef68c77b776d5c6723ba5a72623b570cacce";

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn served_corpus_observable_surface_is_pinned() {
    let data = PlatformData::instance();
    assert!(!data.all_methods().is_empty(), "corpus contract requires the help corpus");
    let digest = blake3::hash(observable_dump(data).as_bytes()).to_hex().to_string();
    assert_eq!(digest, CORPUS_OBSERVABLE_DIGEST);
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn corpus_contract_input_is_the_pinned_corpus() {
    use sha2::Digest;
    let path = std::env::var_os(bsl_platform::CORPUS_ENV)
        .unwrap_or_else(|| panic!("{} must name the pinned corpus", bsl_platform::CORPUS_ENV));
    let bytes = std::fs::read(&path).unwrap();
    let digest: String =
        sha2::Sha256::digest(&bytes).iter().map(|byte| format!("{byte:02x}")).collect();
    assert_eq!(digest, PINNED_CORPUS_SHA256, "the corpus-contract input drifted");
}
