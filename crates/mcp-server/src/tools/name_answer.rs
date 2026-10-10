//! The one wire shape of a name lookup.
//!
//! Two tools publish it — `graph action=resolve` and `symbol_info`'s near-miss
//! list — and the defect this exists to close was that they assembled it twice
//! and disagreed: one offered durable ids nothing accepted back, the other an
//! empty list that read as a proven zero. Ranking and completeness are decided
//! once in [`ide::lookup_names`]; the rendering is decided once here.

use serde_json::{json, Map, Value};

use crate::tools::location as loc;

/// A rendered lookup: what a tool puts in its body, and how complete it is.
pub(crate) struct NameAnswer {
    pub candidates: Vec<Value>,
    pub providers: Vec<Value>,
    pub total: usize,
    pub total_exact: bool,
    pub truncated: bool,
    pub completeness: loc::Completeness,
}

impl NameAnswer {
    /// Render one lookup result. `db` and `roots` are needed to place a
    /// candidate: a place is held as a file id and byte ranges, and the contract
    /// publishes a `(root_id, path)` pair with UTF-16 columns.
    pub(crate) fn render(
        db: &ide::RootDatabaseImpl,
        roots: Option<&bsl_search::WorkspaceRoots>,
        found: &ide::NameLookupResult,
    ) -> Self {
        let candidates =
            found.candidates.iter().map(|c| candidate_value(db, roots, c)).collect::<Vec<_>>();
        let providers = found
            .providers
            .iter()
            .map(|r| json!({ "provider": r.provider.as_str(), "state": r.state.as_str() }))
            .collect::<Vec<_>>();

        let completeness = loc::Completeness::complete()
            .when(
                found.truncated,
                loc::ReasonCode::ResultCap,
                "candidate list capped; raise the limit or refine the query",
            )
            .when(found.is_partial(), loc::ReasonCode::IndexBuilding, incomplete_detail(found));

        Self {
            candidates,
            providers,
            total: found.total,
            total_exact: found.total_exact,
            truncated: found.truncated,
            completeness,
        }
    }

    /// The envelope of an answer assembled from several artefacts at once.
    ///
    /// It borrows nobody's revision. The graph has one, the resident has
    /// another, the platform has none, and stamping any of them here would
    /// declare the other four as fresh as the one that lent it — while the
    /// per-source truth already travels in `providers`.
    pub(crate) fn freshness(completeness: loc::Completeness) -> loc::Freshness {
        loc::Freshness::new(loc::FreshnessSource::NameDictionary, completeness)
    }

    /// The fields both tools carry, so neither can drift into naming them
    /// differently.
    pub(crate) fn insert_into(self, body: &mut Map<String, Value>) -> loc::Completeness {
        body.insert("candidates".into(), Value::Array(self.candidates));
        body.insert("total".into(), json!(self.total));
        body.insert("total_exact".into(), json!(self.total_exact));
        body.insert("truncated".into(), json!(self.truncated));
        body.insert("providers".into(), Value::Array(self.providers));
        self.completeness
    }
}

/// Which indexes were missing, by name. The reason code says an index was
/// building; only this says which, and guessing from an empty candidate list is
/// what the field exists to replace.
fn incomplete_detail(found: &ide::NameLookupResult) -> String {
    let named: Vec<String> = found
        .incomplete_providers()
        .iter()
        .map(|r| format!("{}: {}", r.provider.as_str(), r.state.as_str()))
        .collect();
    format!("some name sources could not be consulted ({})", named.join(", "))
}

/// One candidate, with every address that works and no address that does not.
fn candidate_value(
    db: &ide::RootDatabaseImpl,
    roots: Option<&bsl_search::WorkspaceRoots>,
    candidate: &ide::NameCandidate,
) -> Value {
    let mut address = Map::new();
    if let Some(symbol) =
        candidate.symbol.as_deref().filter(|name| ide::is_well_formed_symbol(name))
    {
        address.insert("symbol".into(), json!(symbol));
    }
    if let Some(id) = &candidate.graph_id {
        address.insert("graph_id".into(), json!(id));
    }
    if let Some(platform) = &candidate.platform_ref {
        let mut reference = Map::new();
        reference.insert("name".into(), json!(platform.name));
        if let Some(type_name) = &platform.type_name {
            reference.insert("type_name".into(), json!(type_name));
        }
        address.insert("syntax_help".into(), Value::Object(reference));
    }
    if let Some(place) = &candidate.place {
        insert_location(db, roots, place, &mut address);
    }

    let mut out = Map::new();
    out.insert("display".into(), json!(candidate.display));
    out.insert("category".into(), json!(candidate.category.as_str()));
    out.insert("match".into(), json!(candidate.match_tier.as_str()));
    out.insert("provider".into(), json!(candidate.provider.as_str()));
    // The original key survives only where it still addresses something. A
    // candidate with no graph node used to carry an `id` an agent would feed
    // back and get nothing for — the defect this list was rebuilt to fix.
    if let Some(id) = &candidate.graph_id {
        out.insert("id".into(), json!(id));
    }
    out.insert("address".into(), Value::Object(address));
    Value::Object(out)
}

fn insert_location(
    db: &ide::RootDatabaseImpl,
    roots: Option<&bsl_search::WorkspaceRoots>,
    place: &ide::NamePlace,
    address: &mut Map<String, Value>,
) {
    insert_resolved_location(&ide::resolve_place(db, place), roots, address);
}

/// Publish a resolved place as the contract's `location`, or the reason there is
/// none. Shared with `references`, whose hits are places without being
/// declarations: one mapping, so the two cannot name the same absence
/// differently.
pub(crate) fn insert_resolved_location(
    resolved: &ide::ResolvedPlace,
    roots: Option<&bsl_search::WorkspaceRoots>,
    address: &mut Map<String, Value>,
) {
    match (&resolved.path, roots) {
        (Some(path), Some(roots)) => {
            match loc::Location::from_path(roots, std::path::Path::new(path)) {
                Ok(location) => {
                    let location = location
                        .with_range(resolved.range.map(loc::PositionRange::from))
                        .with_enclosing_range(
                            resolved.enclosing_range.map(loc::PositionRange::from),
                        );
                    address.insert("location".into(), location.to_value());
                }
                Err(reason) => {
                    address.insert("location_unavailable".into(), json!(reason.code()));
                }
            }
        }
        // Two different facts that must not share a code: no root table at all,
        // versus a table that was there and a path it could not name.
        (Some(_), None) => {
            address.insert(
                "location_unavailable".into(),
                json!(loc::LocationUnavailable::RootsUnavailable.code()),
            );
        }
        (None, _) => {
            address.insert(
                "location_unavailable".into(),
                json!(loc::LocationUnavailable::SourcePathUnavailable.code()),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The section of the tools document that describes the name dictionary.
    ///
    /// The bound is not decoration. `graph` and `platform` appear dozens of
    /// times elsewhere in this file, so a gate reading the whole document would
    /// pass on a vocabulary the dictionary's own section never mentions — and
    /// that section is what a consumer writes its closed list from.
    fn dictionary_section() -> &'static str {
        let document = include_str!("../../../../docs/mcp/TOOLS_AND_EXTENSION.md");
        let after_heading = document
            .split_once("\n## Словарь имён\n")
            .expect("the document describes the name dictionary under its own heading")
            .1;
        match after_heading.split_once("\n## ") {
            Some((section, _)) => section,
            None => after_heading,
        }
    }

    /// The multi-source envelope belongs to the vocabulary the contract
    /// publishes, and the contract's own gate only checks the values it knows
    /// about — a source served but unnamed would slip past it.
    #[test]
    fn the_multi_source_envelope_names_itself() {
        let body = NameAnswer::freshness(loc::Completeness::complete()).to_value();
        assert_eq!(body["source"], "name-dictionary");
        // Nobody's revision is borrowed: five artefacts, no single identity.
        assert!(body["revision"].is_null(), "{body}");
        assert!(body["topology_fingerprint"].is_null(), "{body}");
        assert!(body["stale"].is_null(), "{body}");
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn platform_candidate_with_spaces_has_only_an_accepted_symbol_address() {
        let db = ide::RootDatabaseImpl::new();
        let candidate = ide::NameCandidate::new(
            "Расширение элементов управления, расположенных на панели.УстановитьПривязку",
            ide::NameCategory::PlatformMember,
            ide::NameMatchTier::Exact,
            ide::ProviderId::Platform,
        )
        .with_symbol("Расширение элементов управления, расположенных на панели.УстановитьПривязку");
        let mut candidate = candidate;
        candidate.platform_ref = Some(ide::PlatformRef {
            name: "УстановитьПривязку".to_string(),
            type_name: Some("Расширение элементов управления, расположенных на панели".to_string()),
        });

        let value = candidate_value(&db, None, &candidate);
        assert!(value["address"].get("symbol").is_none(), "{value}");
        assert_eq!(value["address"]["syntax_help"]["name"], "УстановитьПривязку");
        assert_eq!(
            value["address"]["syntax_help"]["type_name"],
            "Расширение элементов управления, расположенных на панели"
        );
        let help = crate::tools::platform::bsl_syntax_help(
            value["address"]["syntax_help"]["name"].as_str().unwrap(),
            value["address"]["syntax_help"]["type_name"].as_str(),
            6_000,
        )
        .unwrap();
        let help = help.structured_content.as_ref().unwrap();
        assert_eq!(help["kind"], "method", "syntax_help returned no method card: {help}");
        assert!(
            help["matches"].as_array().is_some_and(|matches| {
                matches.iter().any(|item| item["name"] == "УстановитьПривязку")
            }),
            "syntax_help did not resolve the published address: {help}"
        );
    }

    /// Four closed vocabularies leave this crate on the wire, and their only
    /// worth over free-form strings is that a consumer may match on them. That
    /// holds while every value names one fact and while the document a consumer
    /// reads carries every value.
    ///
    /// The gates in `location.rs` cannot serve here: they enumerate their own
    /// three vocabularies by name and know nothing of `ide`'s, so leaning on
    /// them would be a check that cannot fail.
    #[test]
    fn every_dictionary_value_is_published_where_the_dictionary_is_described() {
        let section = dictionary_section();

        let vocabularies: [(&str, Vec<&'static str>); 4] = [
            ("NameCategory", ide::NameCategory::ALL.iter().map(|v| v.as_str()).collect()),
            ("ProviderId", ide::ProviderId::ALL.iter().map(|v| v.as_str()).collect()),
            ("ProviderState", ide::ProviderState::ALL.iter().map(|v| v.as_str()).collect()),
            ("NameMatchTier", ide::NameMatchTier::ALL.iter().map(|v| v.as_str()).collect()),
        ];

        for (name, values) in vocabularies {
            let mut seen = std::collections::BTreeSet::new();
            for value in values {
                assert!(seen.insert(value), "two {name} values share the code `{value}`");
                assert!(
                    section.contains(&format!("`{value}`")),
                    "{name}::{value} is served but the dictionary section does not name it",
                );
            }
        }
    }
}
