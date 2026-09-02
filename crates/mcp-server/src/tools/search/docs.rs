use super::acquire::{engine_lock_poisoned_error, try_acquire_engine};
use super::gating::{
    ensure_reference_baseline_runtime_ready, external_baseline_mcp_error,
    map_reference_baseline_resolution,
};
use super::render::{
    format_doc_hits, format_lexical_doc_hits, format_semantic_doc_hits, no_hits_response, Envelope,
};
use super::status::docs_not_ready;
use super::types::{AcquireFailure, SearchFailure};
use super::wait::{embed_unless_cancelled, Withdrawn};
use crate::baseline::{BaselineCall, ConfiguredBaselineStatus, ExternalBaselineService};
use bsl_search::{lexical_hits_for_resolved_view, SearchEngine, SearchError};
use rmcp::model::CallToolResult;
use rmcp::ErrorData as McpError;
use std::sync::{Arc, Mutex, MutexGuard};
use tokio_util::sync::CancellationToken;
use tracing::warn;

/// The actor's answer with the cancellation taken out: a withdrawn call ends the search
/// here, and only a real failure reaches the terminal/transient classification below.
fn unless_withdrawn<R>(
    call: Result<R, BaselineCall>,
) -> Result<Result<R, SearchError>, SearchFailure> {
    match call {
        Ok(value) => Ok(Ok(value)),
        Err(BaselineCall::Failed(error)) => Ok(Err(error)),
        Err(BaselineCall::Withdrawn) => Err(SearchFailure::Cancelled),
    }
}

/// The engine guard for one docs call, or the answer to give instead. A stalled lock is
/// answered with the retry envelope: the index is there, it is merely held.
fn engine_guard<'a>(
    engine: &'a Arc<Mutex<Option<SearchEngine>>>,
    cancel: &CancellationToken,
    action: &str,
) -> Result<Result<MutexGuard<'a, Option<SearchEngine>>, CallToolResult>, SearchFailure> {
    match try_acquire_engine(engine, cancel) {
        Ok(guard) => Ok(Ok(guard)),
        Err(AcquireFailure::Poisoned) => Err(engine_lock_poisoned_error().into()),
        Err(AcquireFailure::Cancelled) => Err(SearchFailure::Cancelled),
        Err(AcquireFailure::TimedOut) => Ok(Err(docs_not_ready(action))),
    }
}

pub fn find_docs(
    engine: &Arc<Mutex<Option<SearchEngine>>>,
    cancel: &CancellationToken,
    configured_baseline: Option<&ConfiguredBaselineStatus>,
    external_baseline: Option<Arc<ExternalBaselineService>>,
    query: &str,
    limit: usize,
    max_output_tokens: usize,
) -> Result<CallToolResult, SearchFailure> {
    ensure_reference_baseline_runtime_ready(configured_baseline, external_baseline.as_ref())?;
    let guard = match engine_guard(engine, cancel, "find_docs")? {
        Ok(guard) => guard,
        Err(not_ready) => return Ok(not_ready),
    };

    if let Some(source) = external_baseline {
        if let Some((_, snapshot)) = map_reference_baseline_resolution(
            configured_baseline,
            unless_withdrawn(source.resolve_snapshot(cancel))?,
            "failed to resolve external reference baseline snapshot for lexical search",
        )? {
            match unless_withdrawn(source.lexical_search(
                cancel,
                snapshot.id.0.as_str(),
                query,
                Some("platform"),
                limit,
            ))? {
                Ok(hits) if !hits.is_empty() => {
                    return Ok(format_lexical_doc_hits(&hits, max_output_tokens)
                        .into_response("find_docs"));
                }
                Ok(_) => {
                    return Ok(no_hits_response(None, Envelope::No, "find_docs"));
                }
                Err(error) => {
                    if error.is_terminal() {
                        return Err(external_baseline_mcp_error(&error).into());
                    }
                    warn!(
                        snapshot_id = snapshot.id.0.as_str(),
                        %error,
                        "direct lexical search failed for external reference baseline, falling back",
                    );
                }
            }

            if let Some(view) = map_reference_baseline_resolution(
                configured_baseline,
                unless_withdrawn(source.resolve_reference_view(cancel))?,
                "failed to resolve external reference baseline view for lexical search",
            )? {
                let hits = lexical_hits_for_resolved_view(&view, query, limit, Some("platform"));
                if !hits.is_empty() {
                    return Ok(format_doc_hits(&hits, max_output_tokens).into_response("find_docs"));
                }
                return Ok(no_hits_response(None, Envelope::No, "find_docs"));
            }
        }
    }

    let Some(engine) = guard.as_ref() else {
        return Ok(docs_not_ready("find_docs"));
    };
    let hits = engine
        .text_search(query, limit, Some("platform"))
        .map_err(|e| McpError::internal_error(format!("search error: {e}"), None))?;

    if hits.is_empty() {
        return Ok(no_hits_response(None, Envelope::No, "find_docs"));
    }

    Ok(format_doc_hits(&hits, max_output_tokens).into_response("find_docs"))
}

fn semantic_not_available() -> McpError {
    McpError::invalid_params(
        "Semantic search not available. Set EMBEDDING_URL and EMBEDDING_MODEL \
         environment variables and restart. Use find_docs for text search instead.",
        None,
    )
}

pub fn search_docs(
    engine: &Arc<Mutex<Option<SearchEngine>>>,
    cancel: &CancellationToken,
    configured_baseline: Option<&ConfiguredBaselineStatus>,
    external_baseline: Option<Arc<ExternalBaselineService>>,
    query: &str,
    limit: usize,
    max_output_tokens: usize,
) -> Result<CallToolResult, SearchFailure> {
    ensure_reference_baseline_runtime_ready(configured_baseline, external_baseline.as_ref())?;

    // The same shape as the semantic code path: take what the embed needs from the engine,
    // release the lock, embed off it, take the lock again for the now-fast search. The
    // engine's embedding identity is fixed for the life of the process, so the captures
    // cannot go stale across the unlocked window (see `semantic_code_hits`).
    let (embedder, model_id, dim) = {
        let guard = match engine_guard(engine, cancel, "search_docs")? {
            Ok(guard) => guard,
            Err(not_ready) => return Ok(not_ready),
        };
        let Some(engine) = guard.as_ref() else {
            return Ok(docs_not_ready("search_docs"));
        };
        if !engine.has_semantic() {
            return Err(semantic_not_available().into());
        }
        (
            engine.embedder_clone(),
            engine.embedding_model().map(str::to_owned),
            engine.embedding_dimension(),
        )
    };
    let Some(embedder) = embedder else {
        return Err(semantic_not_available().into());
    };

    // Resolve the baseline snapshot BEFORE the embed, off the lock: a terminal baseline
    // failure must not cost a wasted round-trip to the embedder.
    let resolved = match external_baseline.as_ref() {
        Some(source) => map_reference_baseline_resolution(
            configured_baseline,
            unless_withdrawn(source.resolve_snapshot(cancel))?,
            "failed to resolve external reference baseline snapshot for semantic search",
        )?,
        None => None,
    };

    let query_embedding = match embed_unless_cancelled(embedder, query, cancel) {
        Err(Withdrawn) => return Err(SearchFailure::Cancelled),
        Ok(Ok(vector)) => vector,
        Ok(Err(e)) => {
            return Err(McpError::internal_error(format!("search error: {e}"), None).into());
        }
    };

    let guard = match engine_guard(engine, cancel, "search_docs")? {
        Ok(guard) => guard,
        Err(not_ready) => return Ok(not_ready),
    };
    let Some(engine) = guard.as_ref() else {
        return Ok(docs_not_ready("search_docs"));
    };
    if !engine.has_semantic() {
        return Err(semantic_not_available().into());
    }

    if let (Some(source), Some((_, snapshot))) = (external_baseline.as_ref(), resolved) {
        let model_id = model_id.as_deref().ok_or_else(|| {
            McpError::internal_error(
                "search error: semantic model id is unavailable".to_owned(),
                None,
            )
        })?;
        let dim = dim.ok_or_else(|| {
            McpError::internal_error(
                "search error: embedding dimension is unavailable".to_owned(),
                None,
            )
        })?;

        match unless_withdrawn(source.semantic_search(
            cancel,
            snapshot.id.0.as_str(),
            &query_embedding,
            model_id,
            dim,
            Some("platform"),
            limit,
        ))? {
            Ok(hits) if !hits.is_empty() => {
                return Ok(
                    format_semantic_doc_hits(&hits, max_output_tokens).into_response("search_docs")
                );
            }
            Ok(_) => {
                return Ok(no_hits_response(None, Envelope::No, "search_docs"));
            }
            Err(error) => {
                if error.is_terminal() {
                    return Err(external_baseline_mcp_error(&error).into());
                }
                warn!(
                    snapshot_id = snapshot.id.0.as_str(),
                    %error,
                    "direct semantic search failed for external reference baseline, falling back",
                );
            }
        }
    }

    // The local tail searches by the vector already computed: the string form of this
    // search embeds on its own, under the lock, and would make a second round-trip.
    let hits = engine
        .search_with_embedding_read_only(&query_embedding, limit, Some("platform"))
        .map_err(|e| McpError::internal_error(format!("search error: {e}"), None))?;

    if hits.is_empty() {
        return Ok(no_hits_response(None, Envelope::No, "search_docs"));
    }

    Ok(format_doc_hits(&hits, max_output_tokens).into_response("search_docs"))
}

#[cfg(test)]
mod tests {
    use super::{find_docs, search_docs};
    use crate::baseline::{
        ConfiguredBaselineStatus, ExternalBaselineService, RefreshableExternalBaselineSource,
    };
    use bsl_search::{BaselineRef, CorpusId, SearchEngine};
    use rmcp::model::ErrorCode;
    use serde_json::json;
    use std::sync::{Arc, Mutex};
    use tempfile::tempdir;
    use tokio_util::sync::CancellationToken;

    fn never() -> CancellationToken {
        CancellationToken::new()
    }

    #[test]
    fn find_docs_hits_carry_the_structured_listing_beside_the_text() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("reference-search.db");
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        let document = crate::build_reference_documents()
            .into_iter()
            .find(|document| document.kind == "type" && document.title.starts_with("Массив /"))
            .expect("Array reference document");
        engine.index_documents("platform", "platform/Массив", b"v1", &[document], None).unwrap();

        let result = find_docs(
            &Arc::new(Mutex::new(Some(engine))),
            &never(),
            None,
            None,
            "Массив",
            10,
            usize::MAX,
        )
        .unwrap();

        let text = result.content[0].as_text().expect("text mirror").text.as_str();
        assert!(text.starts_with("#1 ["), "text listing unchanged: {text}");

        let body = result.structured_content.as_ref().expect("structured listing");
        assert_eq!(body["schema_version"], "4");
        assert_eq!(body["action"], "find_docs");
        let hits = body["hits"].as_array().expect("hits array");
        assert_eq!(hits[0]["rank"], 1);
        assert_eq!(hits[0]["symbol"], "Массив / Array");
        assert_eq!(hits[0]["name"], "Массив");
        assert_eq!(hits[0]["kind"], "type");
        assert!(hits[0]["reference_id"].as_str().is_some_and(|id| id.starts_with("type::")));
        assert!(hits[0].get("english_name").is_some());
        assert!(hits[0].get("description").is_some());
        assert!(hits[0]["score"].is_number(), "the score the listing prints: {body}");
        assert_eq!(body["shown"], json!(hits.len()));
    }

    #[test]
    fn find_docs_returns_a_real_property_identity() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("reference-search.db");
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        let (document, expected) = crate::build_reference_documents()
            .into_iter()
            .find_map(|document| {
                if document.kind != "property" {
                    return None;
                }
                let reference = crate::tools::platform::platform_reference_for_document(
                    &document.kind,
                    &document.title,
                )?;
                reference.description.as_ref()?;
                Some((document, reference))
            })
            .expect("the platform corpus must contain a documented property");
        let query = expected.name.clone();
        engine.index_documents("platform", "platform/property", b"v1", &[document], None).unwrap();

        let result = find_docs(
            &Arc::new(Mutex::new(Some(engine))),
            &never(),
            None,
            None,
            &query,
            10,
            usize::MAX,
        )
        .unwrap();
        let hit = &result.structured_content.unwrap()["hits"][0];
        assert_eq!(hit["reference_id"], expected.reference_id);
        assert_eq!(hit["kind"], "property");
        assert_eq!(hit["owner"], expected.owner);
        assert_eq!(hit["name"], expected.name);
        assert_eq!(hit["description"], expected.description.unwrap());
    }

    #[test]
    fn search_docs_returns_a_real_constructor_identity_from_semantic_corpus() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("reference-search.db");
        let mock = crate::state::test_support::spawn_mock_embedding_server(vec![1.0, 0.0, 0.0]);
        let mut engine =
            SearchEngine::new(&db_path, crate::state::test_support::mock_semantic_config(&mock))
                .unwrap();
        let (document, expected) = crate::build_reference_documents()
            .into_iter()
            .find_map(|document| {
                if document.kind != "constructor" {
                    return None;
                }
                let reference = crate::tools::platform::platform_reference_for_document(
                    &document.kind,
                    &document.title,
                )?;
                Some((document, reference))
            })
            .expect("the platform corpus must contain a constructor");
        engine
            .index_documents("platform", "platform/constructor", b"v1", &[document], None)
            .unwrap();

        let result = search_docs(
            &Arc::new(Mutex::new(Some(engine))),
            &never(),
            None,
            None,
            &format!("создать значение {}", expected.owner),
            10,
            usize::MAX,
        )
        .unwrap();
        let hit = &result.structured_content.unwrap()["hits"][0];
        assert_eq!(hit["reference_id"], expected.reference_id);
        assert_eq!(hit["kind"], "constructor");
        assert_eq!(hit["owner"], expected.owner);
        assert_eq!(hit["name"], expected.name);
    }

    #[test]
    fn doc_search_not_ready_and_empty_answers_are_structured_too() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("reference-search.db");
        let engine = SearchEngine::fts_only(&db_path).unwrap();

        let building =
            find_docs(&Arc::new(Mutex::new(None)), &never(), None, None, "Массив", 10, usize::MAX)
                .unwrap();
        assert_eq!(
            building.content[0].as_text().expect("text").text,
            "Search index is being built, please try again in a moment.",
        );
        let building_body = building.structured_content.as_ref().expect("structured envelope");
        assert_eq!(building_body["status"], "not_ready");
        assert_eq!(building_body["retry_after_ms"], 1500);

        let empty = find_docs(
            &Arc::new(Mutex::new(Some(engine))),
            &never(),
            None,
            None,
            "Массив",
            10,
            usize::MAX,
        )
        .unwrap();
        assert_eq!(empty.content[0].as_text().expect("text").text, "No results found.");
        // An empty index and an empty result set must not look alike to a machine consumer.
        assert_eq!(
            empty.structured_content.as_ref().expect("structured envelope")["hits"],
            json!([]),
        );
    }

    #[test]
    fn search_docs_with_external_reference_baseline_uses_standard_semantic_validation() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("reference-search.db");
        let engine = SearchEngine::fts_only(&db_path).unwrap();
        let source = ExternalBaselineService::for_test(
            RefreshableExternalBaselineSource::for_test(
                bsl_search::ExternalBaselineConfig::postgres("postgres://127.0.0.1:1"),
                BaselineRef {
                    corpus: CorpusId::Reference,
                    snapshot_id: None,
                    branch: None,
                    commit: None,
                },
            )
            .unwrap(),
        );

        let error = search_docs(
            &Arc::new(Mutex::new(Some(engine))),
            &never(),
            None,
            Some(source),
            "Массив",
            10,
            usize::MAX,
        )
        .unwrap_err()
        .expect_error();

        assert!(error.message.contains("Semantic search not available"));
        assert!(!error.message.contains("centralized reference baseline"));
    }

    #[test]
    fn find_docs_rejects_local_fallback_when_reference_postgres_baseline_is_unavailable() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("reference-search.db");
        let engine = SearchEngine::fts_only(&db_path).unwrap();

        let error = find_docs(
            &Arc::new(Mutex::new(Some(engine))),
            &never(),
            Some(&ConfiguredBaselineStatus {
                backend: "postgres",
                selection: "latest reference".to_owned(),
                issue: Some("failed to resolve PostgreSQL reader credentials".to_owned()),
                support: None,
            }),
            None,
            "Массив",
            10,
            usize::MAX,
        )
        .unwrap_err()
        .expect_error();

        assert_eq!(error.code, ErrorCode::INVALID_PARAMS);
        assert!(error.message.contains("Shared reference baseline is unavailable"));
        assert_eq!(
            error
                .data
                .as_ref()
                .and_then(|data| data.get("reasonCode"))
                .and_then(|value| value.as_str()),
            Some("baseline_unavailable")
        );
    }

    #[test]
    fn search_docs_rejects_reference_postgres_baseline_unavailability_before_semantic_validation() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("reference-search.db");
        let engine = SearchEngine::fts_only(&db_path).unwrap();

        let error = search_docs(
            &Arc::new(Mutex::new(Some(engine))),
            &never(),
            Some(&ConfiguredBaselineStatus {
                backend: "postgres",
                selection: "latest reference".to_owned(),
                issue: Some("failed to resolve PostgreSQL reader credentials".to_owned()),
                support: None,
            }),
            None,
            "Массив",
            10,
            usize::MAX,
        )
        .unwrap_err()
        .expect_error();

        assert_eq!(error.code, ErrorCode::INVALID_PARAMS);
        assert!(error.message.contains("Shared reference baseline is unavailable"));
        assert_eq!(
            error
                .data
                .as_ref()
                .and_then(|data| data.get("reasonCode"))
                .and_then(|value| value.as_str()),
            Some("baseline_unavailable")
        );
    }

    #[test]
    fn search_docs_falls_back_to_local_sqlite_when_external_semantic_fails() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("reference-search.db");
        let engine = SearchEngine::fts_only(&db_path).unwrap();

        let source = ExternalBaselineService::for_test(
            RefreshableExternalBaselineSource::for_test(
                bsl_search::ExternalBaselineConfig::postgres("postgres://127.0.0.1:1"),
                BaselineRef {
                    corpus: CorpusId::Reference,
                    snapshot_id: Some(bsl_search::SnapshotId::new("ref:0.1.0")),
                    branch: None,
                    commit: None,
                },
            )
            .unwrap(),
        );

        let result = search_docs(
            &Arc::new(Mutex::new(Some(engine))),
            &never(),
            None,
            Some(source),
            "Массив",
            10,
            usize::MAX,
        )
        .unwrap_err()
        .expect_error();

        assert!(result.message.contains("Semantic search not available"));
    }
}
