use super::acquire::{engine_lock_poisoned_error, try_acquire_engine};
use super::gating::{
    ensure_workspace_baseline_runtime_ready, ensure_workspace_search_allowed,
    external_baseline_mcp_error,
};
use super::types::{
    direct_search_initial_window, direct_search_max_window, AcquireFailure, CodeHits, DirectResult,
    SearchFailure, DIRECT_SEARCH_MAX_REFILL_ROUNDS,
};
use crate::baseline::{BaselineCall, ConfiguredBaselineStatus, ExternalBaselineService};
use crate::state::WorkspaceSearchMode;
use bsl_search::{
    merge_context_for_collection, merge_lexical, LexicalHit, SearchEngine, SearchHit,
};
use rmcp::ErrorData as McpError;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;
use tracing::warn;

#[allow(
    clippy::too_many_arguments,
    reason = "the lexical modality takes the tool-dispatch inputs plus the request's cancellation; a one-use context struct would only rename them"
)]
pub(super) fn lexical_code_hits(
    engine: &Arc<Mutex<Option<SearchEngine>>>,
    cancel: &CancellationToken,
    workspace_search_mode: WorkspaceSearchMode,
    configured_baseline: Option<&ConfiguredBaselineStatus>,
    external_baseline: Option<Arc<ExternalBaselineService>>,
    query: &str,
    limit: usize,
) -> Result<CodeHits, SearchFailure> {
    ensure_workspace_search_allowed(configured_baseline)?;
    ensure_workspace_baseline_runtime_ready(
        workspace_search_mode.clone(),
        configured_baseline,
        external_baseline.as_ref(),
    )?;
    let guard = match try_acquire_engine(engine, cancel) {
        Ok(g) => g,
        Err(AcquireFailure::Poisoned) => return Err(engine_lock_poisoned_error().into()),
        Err(AcquireFailure::Cancelled) => return Err(SearchFailure::Cancelled),
        Err(AcquireFailure::TimedOut) => {
            if let Some(source) = external_baseline {
                match try_direct_lexical_code_no_overlay(&source, cancel, query, limit) {
                    DirectResult::Found(hits) => {
                        if hits.is_empty() {
                            return Ok(CodeHits::Pending(
                                "No results found (overlay is warming up, only baseline search available)."
                                    .to_owned(),
                            ));
                        }
                        // Engine lock is busy, so these are external-baseline direct hits
                        // with no reachable workspace root. Module-keyed methods still get a
                        // graph_id (root-independent); a path-fallback hit would be dropped,
                        // which is fine here — baseline paths are relative, not absolute.
                        return Ok(CodeHits::Ready { hits, roots: None });
                    }
                    DirectResult::Terminal(error) => {
                        return Err(external_baseline_mcp_error(&error).into());
                    }
                    DirectResult::Cancelled => return Err(SearchFailure::Cancelled),
                    DirectResult::Unavailable => {}
                }
            }
            return Ok(CodeHits::Pending(
                "Search index is busy (a long operation is holding it); please try again in a moment."
                    .to_owned(),
            ));
        }
    };

    let hits = if let Some(source) = external_baseline {
        match guard.as_ref() {
            Some(engine) => {
                let direct_start = std::time::Instant::now();
                let direct = try_direct_lexical_code(engine, &source, cancel, query, limit);
                tracing::debug!(
                    elapsed_ms = direct_start.elapsed().as_millis() as u64,
                    query_len = query.len(),
                    "search.code: try_direct_lexical_code"
                );
                match direct {
                    DirectResult::Found(hits) => hits,
                    DirectResult::Terminal(error) => {
                        return Err(external_baseline_mcp_error(&error).into());
                    }
                    DirectResult::Cancelled => return Err(SearchFailure::Cancelled),
                    // Direct baseline serving is unavailable (snapshot, overlay, or a transient
                    // serving-table absence). Do NOT fall back to `resolve_workspace_view`:
                    // that loads the whole baseline corpus under the engine lock and stalls
                    // search past the client timeout on a large remote overlay.
                    //
                    // In PostgresRemoteOverlay mode the local store has no baseline rows, so
                    // local `text_search` would silently return overlay-only or empty results
                    // while the real corpus is unreachable — a misleading "no matches found"
                    // instead of an honest transient state. Surface it as Pending so the caller
                    // retries.
                    //
                    // In SqliteLocal mode the local store IS the full corpus, so `text_search`
                    // is the correct bounded answer.
                    DirectResult::Unavailable => {
                        if matches!(
                            workspace_search_mode,
                            WorkspaceSearchMode::PostgresRemoteOverlay
                        ) {
                            return Ok(CodeHits::Pending(
                                "Baseline lexical serving is temporarily unavailable; \
                                 please retry shortly."
                                    .to_owned(),
                            ));
                        }
                        let fallback_start = std::time::Instant::now();
                        let hits =
                            engine.text_search_read_only(query, limit, Some("code")).map_err(
                                |e| McpError::internal_error(format!("search error: {e}"), None),
                            )?;
                        tracing::debug!(
                            elapsed_ms = fallback_start.elapsed().as_millis() as u64,
                            query_len = query.len(),
                            "search.code: lexical fallback text_search (baseline unavailable)"
                        );
                        hits
                    }
                }
            }
            None => match try_direct_lexical_code_no_overlay(&source, cancel, query, limit) {
                DirectResult::Found(hits) => hits,
                DirectResult::Terminal(error) => {
                    return Err(external_baseline_mcp_error(&error).into());
                }
                DirectResult::Cancelled => return Err(SearchFailure::Cancelled),
                DirectResult::Unavailable => {
                    return Ok(CodeHits::Pending(
                        "Search index is being built, please try again in a moment.".to_owned(),
                    ));
                }
            },
        }
    } else {
        let Some(engine) = guard.as_ref() else {
            return Ok(CodeHits::Pending(
                "Search index is being built, please try again in a moment.".to_owned(),
            ));
        };
        engine
            .text_search_read_only(query, limit, Some("code"))
            .map_err(|e| McpError::internal_error(format!("search error: {e}"), None))?
    };

    let roots = guard.as_ref().and_then(|engine| engine.workspace_roots().cloned());
    Ok(CodeHits::Ready { hits, roots })
}

/// The actor's snapshot answer as a direct-serving outcome; `Ok` carries the snapshot.
fn resolved_snapshot(
    resolution: Result<Option<(bsl_search::BaselineRef, bsl_search::Snapshot)>, BaselineCall>,
    what: &str,
) -> Result<bsl_search::Snapshot, DirectResult> {
    match resolution {
        Ok(Some((_, snapshot))) => Ok(snapshot),
        Ok(None) => Err(DirectResult::Unavailable),
        Err(BaselineCall::Withdrawn) => Err(DirectResult::Cancelled),
        Err(BaselineCall::Failed(e)) => {
            if e.is_terminal() {
                warn!("{what}: terminal snapshot resolution error: {e}");
                return Err(DirectResult::Terminal(e));
            }
            warn!("{what}: snapshot resolution failed: {e}");
            Err(DirectResult::Unavailable)
        }
    }
}

fn try_direct_lexical_code_no_overlay(
    source: &ExternalBaselineService,
    cancel: &CancellationToken,
    query: &str,
    limit: usize,
) -> DirectResult {
    let snapshot =
        match resolved_snapshot(source.resolve_snapshot(cancel), "direct lexical (no overlay)") {
            Ok(snapshot) => snapshot,
            Err(outcome) => return outcome,
        };
    match source.lexical_search(cancel, snapshot.id.0.as_str(), query, Some("code"), limit) {
        Ok(hits) => DirectResult::Found(hits.iter().map(SearchHit::from_lexical).collect()),
        Err(BaselineCall::Withdrawn) => DirectResult::Cancelled,
        Err(BaselineCall::Failed(e)) => {
            if e.is_terminal() {
                warn!("direct lexical (no overlay): terminal serving query error: {e}");
                return DirectResult::Terminal(e);
            }
            warn!("direct lexical (no overlay): serving query failed: {e}");
            DirectResult::Unavailable
        }
    }
}

fn try_direct_lexical_code(
    engine: &SearchEngine,
    source: &ExternalBaselineService,
    cancel: &CancellationToken,
    query: &str,
    limit: usize,
) -> DirectResult {
    let snapshot = match resolved_snapshot(source.resolve_snapshot(cancel), "direct lexical") {
        Ok(snapshot) => snapshot,
        Err(outcome) => return outcome,
    };
    let (overlay_hits, hidden_paths) =
        match engine.workspace_overlay_lexical_hits_read_only(query, limit) {
            Ok(r) => r,
            Err(e) => {
                warn!("direct lexical: overlay query failed: {e}");
                return DirectResult::Unavailable;
            }
        };
    let overlay_lexical: Vec<LexicalHit> = overlay_hits.iter().map(SearchHit::to_lexical).collect();
    merge_direct_lexical_with_refill(
        &overlay_lexical,
        &hidden_paths,
        cancel,
        limit,
        |fetch_limit| {
            source.lexical_search(cancel, snapshot.id.0.as_str(), query, Some("code"), fetch_limit)
        },
    )
}

fn merge_direct_lexical_with_refill<F>(
    overlay_hits: &[LexicalHit],
    hidden_paths: &HashSet<bsl_search::FileKey>,
    cancel: &CancellationToken,
    limit: usize,
    mut fetch_baseline: F,
) -> DirectResult
where
    F: FnMut(usize) -> Result<Vec<LexicalHit>, BaselineCall>,
{
    let context = merge_context_for_collection(hidden_paths, "code");
    let mut fetch_limit = direct_search_initial_window(limit);
    let max_fetch_limit = direct_search_max_window(limit);
    let mut previous_baseline_count = 0usize;
    let mut best = Vec::new();

    for _ in 0..DIRECT_SEARCH_MAX_REFILL_ROUNDS {
        // Between rounds: the round itself waits on the actor under this token, so this
        // only covers a cancellation that landed while the last answer was being merged.
        if cancel.is_cancelled() {
            return DirectResult::Cancelled;
        }
        let baseline_hits = match fetch_baseline(fetch_limit) {
            Ok(hits) => hits,
            Err(BaselineCall::Withdrawn) => return DirectResult::Cancelled,
            Err(BaselineCall::Failed(e)) => {
                if e.is_terminal() {
                    warn!("direct lexical: terminal serving query error: {e}");
                    return DirectResult::Terminal(e);
                }
                warn!("direct lexical: serving query failed: {e}");
                return DirectResult::Unavailable;
            }
        };

        best = merge_lexical(&baseline_hits, overlay_hits, &context, limit)
            .into_iter()
            .map(SearchHit::from_merged)
            .collect();

        if best.len() >= limit {
            return DirectResult::Found(best);
        }

        let baseline_count = baseline_hits.len();
        if baseline_count < fetch_limit || baseline_count <= previous_baseline_count {
            return DirectResult::Found(best);
        }

        previous_baseline_count = baseline_count;
        if fetch_limit >= max_fetch_limit {
            return DirectResult::Found(best);
        }

        let next_fetch_limit = fetch_limit.saturating_mul(2).min(max_fetch_limit);
        if next_fetch_limit == fetch_limit {
            return DirectResult::Found(best);
        }
        fetch_limit = next_fetch_limit;
    }

    DirectResult::Found(best)
}

#[cfg(test)]
mod tests {
    use super::super::test_support::lexical_hit;
    use super::super::types::DirectResult;
    use super::merge_direct_lexical_with_refill;
    use bsl_search::{
        lexical_hits_for_resolved_view, BaselineRef, CorpusId, IndexedDocument, ResolvedView,
    };
    use std::collections::HashSet;

    #[test]
    fn direct_lexical_refill_recovers_results_hidden_by_overlay() {
        let hidden_paths = HashSet::from([
            bsl_search::FileKey::configuration("src/hidden1.bsl"),
            bsl_search::FileKey::configuration("src/hidden2.bsl"),
            bsl_search::FileKey::configuration("src/hidden3.bsl"),
            bsl_search::FileKey::configuration("src/hidden4.bsl"),
            bsl_search::FileKey::configuration("src/hidden5.bsl"),
            bsl_search::FileKey::configuration("src/hidden6.bsl"),
            bsl_search::FileKey::configuration("src/hidden7.bsl"),
            bsl_search::FileKey::configuration("src/hidden8.bsl"),
            bsl_search::FileKey::configuration("src/hidden9.bsl"),
        ]);
        let baseline = vec![
            lexical_hit("src/hidden1.bsl", "Hidden1", 100.0),
            lexical_hit("src/hidden2.bsl", "Hidden2", 99.0),
            lexical_hit("src/hidden3.bsl", "Hidden3", 98.0),
            lexical_hit("src/hidden4.bsl", "Hidden4", 97.0),
            lexical_hit("src/hidden5.bsl", "Hidden5", 96.0),
            lexical_hit("src/hidden6.bsl", "Hidden6", 95.0),
            lexical_hit("src/hidden7.bsl", "Hidden7", 94.0),
            lexical_hit("src/hidden8.bsl", "Hidden8", 93.0),
            lexical_hit("src/hidden9.bsl", "Hidden9", 92.0),
            lexical_hit("src/visible1.bsl", "Visible1", 91.0),
            lexical_hit("src/visible2.bsl", "Visible2", 90.0),
            lexical_hit("src/visible3.bsl", "Visible3", 89.0),
        ];
        let mut requested_limits = Vec::new();

        let result = merge_direct_lexical_with_refill(
            &[],
            &hidden_paths,
            &tokio_util::sync::CancellationToken::new(),
            3,
            |fetch_limit| {
                requested_limits.push(fetch_limit);
                Ok(baseline.iter().take(fetch_limit).cloned().collect())
            },
        );

        let DirectResult::Found(hits) = result else {
            panic!("expected lexical refill to produce hits");
        };
        assert_eq!(hits.len(), 3);
        assert_eq!(
            hits.iter().map(|hit| hit.file_path.as_str()).collect::<Vec<_>>(),
            vec!["src/visible1.bsl", "src/visible2.bsl", "src/visible3.bsl"]
        );
        assert_eq!(requested_limits, vec![9, 18]);
    }

    #[test]
    fn resolved_view_lexical_search_returns_exact_match_first() {
        let view = ResolvedView::new(
            BaselineRef::for_snapshot(CorpusId::WorkspaceCode, "snapshot-1"),
            vec![
                IndexedDocument {
                    collection: "code".to_owned(),
                    root_id: bsl_search::CONFIGURATION_ROOT_ID.to_owned(),
                    path: "A.bsl".to_owned(),
                    symbol_name: "НайтиПроцедуру".to_owned(),
                    kind: "procedure".to_owned(),
                    line_start: 1,
                    line_end: 2,
                    text: "body".to_owned(),
                    content_hash: "a".to_owned(),
                    graph_context: None,
                },
                IndexedDocument {
                    collection: "code".to_owned(),
                    root_id: bsl_search::CONFIGURATION_ROOT_ID.to_owned(),
                    path: "B.bsl".to_owned(),
                    symbol_name: "Другая".to_owned(),
                    kind: "procedure".to_owned(),
                    line_start: 1,
                    line_end: 2,
                    text: "внутри НайтиПроцедуру".to_owned(),
                    content_hash: "b".to_owned(),
                    graph_context: None,
                },
            ],
        );

        let hits = lexical_hits_for_resolved_view(&view, "НайтиПроцедуру", 10, Some("code"));

        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].file_path, "A.bsl");
        assert!(hits[0].score > hits[1].score);
    }
}
