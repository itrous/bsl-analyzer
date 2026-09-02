use super::acquire::{engine_lock_poisoned_error, try_acquire_engine};
use super::gating::{
    ensure_workspace_baseline_runtime_ready, ensure_workspace_search_allowed,
    external_baseline_mcp_error,
};
use super::types::{
    direct_search_initial_window, direct_search_max_window, AcquireFailure, CodeHits,
    DirectResolve, DirectResult, SearchFailure, SemanticUnavailable,
    DIRECT_SEARCH_MAX_REFILL_ROUNDS,
};
use super::wait::{embed_unless_cancelled, Withdrawn};
use crate::baseline::{BaselineCall, ConfiguredBaselineStatus, ExternalBaselineService};
use crate::state::{SemanticRuntimeStatus, WorkspaceSearchMode};
use bsl_search::{
    merge_context_for_collection, merge_semantic, SearchEngine, SearchError, SearchHit, SemanticHit,
};
use rmcp::ErrorData as McpError;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;
use tracing::warn;

/// Produce semantic (pgvector) code hits, separated from presentation. Hard policy/terminal
/// failures are `Err`; a still-warming index is `Pending`; a semantic shortfall that
/// `hybrid_code` can degrade past is `Unavailable`.
#[allow(
    clippy::too_many_arguments,
    reason = "semantic search has the lexical inputs plus its runtime status; a one-use context struct would only rename them"
)]
pub(super) fn semantic_code_hits(
    engine: &Arc<Mutex<Option<SearchEngine>>>,
    cancel: &CancellationToken,
    semantic_runtime: &Arc<Mutex<SemanticRuntimeStatus>>,
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
    let semantic_runtime = semantic_runtime
        .lock()
        .map_err(|e| McpError::internal_error(format!("semantic runtime lock error: {e}"), None))?
        .clone();
    let guard = match try_acquire_engine(engine, cancel) {
        Ok(g) => g,
        Err(AcquireFailure::Poisoned) => return Err(engine_lock_poisoned_error().into()),
        Err(AcquireFailure::Cancelled) => return Err(SearchFailure::Cancelled),
        Err(AcquireFailure::TimedOut) => {
            return Ok(CodeHits::Pending(
                "Semantic search is busy (a long operation is holding the index). Lexical search is available in the meantime."
                    .to_owned(),
            ));
        }
    };
    {
        let Some(engine) = guard.as_ref() else {
            return Ok(CodeHits::Pending(
                "Search index is being built, please try again in a moment.".to_owned(),
            ));
        };

        if let SemanticRuntimeStatus::Failed(_) = semantic_runtime {
            return Ok(CodeHits::Unavailable(SemanticUnavailable::RuntimeFailed));
        }

        // The fused engine is published before its vectors exist. Degrade to lexical until
        // the background pass swaps in a populated index, rather than searching the empty
        // one and reporting a silent zero.
        if let SemanticRuntimeStatus::Indexing = semantic_runtime {
            return Ok(CodeHits::Pending(
                "RAG semantic index is still building; lexical search is available in the meantime."
                    .to_owned(),
            ));
        }

        if !engine.has_semantic() {
            return Ok(CodeHits::Unavailable(SemanticUnavailable::NotConfigured));
        }

        // Best-effort identity gate, kept under the guard and *before* the embed: the reader's
        // query vectors are only comparable against the baseline's stored vectors if both were
        // produced by the same embedding model/dimension. A mismatch means the embed could never
        // match, so checking it here (cheap) avoids paying for a wasted ~1.4s query embed. On a
        // mismatch, name the exact reason (and the knobs to fix it) instead of silently returning
        // lexical-only. A baseline with no recorded identity, or a read error, falls through to the
        // existing behavior rather than hard-failing. A cancelled wait is the one answer that
        // does NOT fall through: the caller is gone, and the guard goes with it.
        if let Some(source) = external_baseline.as_ref() {
            match source.embedding_identity(cancel) {
                Ok(Some((baseline_model, baseline_dim))) => {
                    let reader_model = engine.embedding_model().unwrap_or("unset");
                    let reader_dim = engine.embedding_dimension();
                    if reader_model != baseline_model || reader_dim != Some(baseline_dim) {
                        let reader_dim = reader_dim
                            .map(|dim| dim.to_string())
                            .unwrap_or_else(|| "unset".to_owned());
                        let msg = format!(
                            "semantic skipped: this baseline was indexed with model \
                             '{baseline_model}' (dim {baseline_dim}), but the reader is configured \
                             with model '{reader_model}' (dim {reader_dim}); set \
                             EMBEDDING_MODEL/EMBEDDING_DIM (or [search.baseline.embedding] in \
                             bsl-analyzer.toml) to match and restart"
                        );
                        return Ok(CodeHits::Unavailable(SemanticUnavailable::IdentityMismatch(
                            msg,
                        )));
                    }
                }
                Ok(None) => {}
                Err(BaselineCall::Withdrawn) => return Err(SearchFailure::Cancelled),
                Err(BaselineCall::Failed(error)) => {
                    tracing::debug!(
                        "failed to read baseline embedding identity for validation: {error}"
                    );
                }
            }
        }
    }

    // Capture everything needed from the engine while holding the guard, then drop it so the
    // ~1.4s embed does not serialize every concurrent search on the single engine Mutex.
    // `model_id` and `dim` are captured here so `resolve_direct_semantic` (called lock-free
    // below) can gate baseline readiness without re-acquiring the engine.
    //
    // These captures cannot go stale across the unlocked embed window: the engine's embedding
    // identity (embedder/model/dimension) is fixed for the life of the process — built once from
    // the startup env config and never reconfigured. The only runtime mutation under the engine
    // lock is `set_vector_index` (the background pass swapping in the populated index, built from
    // the same config), which preserves model and dimension. So the captured embedder/model_id/dim
    // stay consistent with the engine the second guard sees. (If a model-reconfiguration path is
    // ever added, re-validate identity under the second guard.)
    let (embedder, roots, model_id, dim) = {
        let engine = guard.as_ref().expect("checked is_none above");
        (
            engine.embedder_clone(),
            engine.workspace_roots().cloned(),
            engine.embedding_model().map(str::to_owned),
            engine.embedding_dimension(),
        )
    };
    drop(guard);

    let Some(embedder) = embedder else {
        return Ok(CodeHits::Unavailable(SemanticUnavailable::NotConfigured));
    };

    // Resolve external baseline readiness BEFORE embedding. The snapshot actor round-trip is
    // cheap and needs no engine lock; the embed (~1.4s) must not fire on a not-ready baseline
    // (it would be wasted, and the caller would receive EmbedderUnavailable instead of the
    // correct BaselineNotReady). `resolve_direct_semantic` uses the model_id/dim captured above.
    let resolved_baseline: Option<DirectResolve> = if let Some(ref source) = external_baseline {
        let resolve_start = std::time::Instant::now();
        let r = resolve_direct_semantic(source, cancel, model_id.as_deref(), dim);
        tracing::debug!(
            elapsed_ms = resolve_start.elapsed().as_millis() as u64,
            "search.code: resolve_direct_semantic"
        );
        match r {
            DirectResolve::Terminal(e) => return Err(external_baseline_mcp_error(&e).into()),
            DirectResolve::Cancelled => return Err(SearchFailure::Cancelled),
            DirectResolve::Unavailable => {
                // Baseline not ready: the PostgresRemoteOverlay mode has no local fallback.
                if matches!(workspace_search_mode, WorkspaceSearchMode::PostgresRemoteOverlay) {
                    return Ok(CodeHits::Unavailable(SemanticUnavailable::BaselineNotReady));
                }
                None // Non-Postgres: continue to the local path without embedding for baseline.
            }
            r @ DirectResolve::Ready { .. } => Some(r),
        }
    } else {
        // No external baseline: PostgresRemoteOverlay requires one.
        if matches!(workspace_search_mode, WorkspaceSearchMode::PostgresRemoteOverlay) {
            return Ok(CodeHits::Unavailable(SemanticUnavailable::BaselineRequired));
        }
        None
    };

    // Embed lock-free now that readiness is confirmed (either baseline Ready or local path).
    let embed_start = std::time::Instant::now();
    let embed_result = match embed_unless_cancelled(embedder, query, cancel) {
        Ok(result) => result,
        Err(Withdrawn) => return Err(SearchFailure::Cancelled),
    };
    tracing::debug!(
        elapsed_ms = embed_start.elapsed().as_millis() as u64,
        query_len = query.len(),
        "search.code: embedder.embed (off-lock)"
    );
    let query_embedding = match embed_result {
        Ok(vector) => vector,
        // The query embed is a request-time call to a remote embedder on the hot path of every
        // search. When it times out or transiently fails, degrade to the lexical hits the caller
        // already has rather than failing the whole tool. A non-embedder error means something
        // structural is broken, which is worth surfacing as a hard error.
        Err(SearchError::Embedder(detail)) => {
            warn!("semantic: query embed failed, degrading to lexical: {detail}");
            return Ok(CodeHits::Unavailable(SemanticUnavailable::EmbedderUnavailable(detail)));
        }
        Err(e) => return Err(McpError::internal_error(format!("search error: {e}"), None).into()),
    };

    // Re-acquire the lock for the now-fast search. The engine may have changed while unlocked, so
    // re-check the readiness conditions that gate a semantic search.
    let guard = match try_acquire_engine(engine, cancel) {
        Ok(g) => g,
        Err(AcquireFailure::Poisoned) => return Err(engine_lock_poisoned_error().into()),
        Err(AcquireFailure::Cancelled) => return Err(SearchFailure::Cancelled),
        Err(AcquireFailure::TimedOut) => {
            return Ok(CodeHits::Pending(
                "Semantic search is busy (a long operation is holding the index). Lexical search is available in the meantime."
                    .to_owned(),
            ));
        }
    };
    let Some(engine) = guard.as_ref() else {
        return Ok(CodeHits::Pending(
            "Search index is being built, please try again in a moment.".to_owned(),
        ));
    };
    if !engine.has_semantic() {
        return Ok(CodeHits::Unavailable(SemanticUnavailable::NotConfigured));
    }

    if let Some(DirectResolve::Ready { snapshot, model_id: ref mid, dim: d }) = resolved_baseline {
        // `external_baseline` is still live (the Arc was not consumed); borrow the service for
        // the search call.
        let source = external_baseline.as_ref().expect("resolved_baseline=Some implies Some");
        let direct_start = std::time::Instant::now();
        let direct =
            run_direct_semantic(engine, source, cancel, &snapshot, mid, d, &query_embedding, limit);
        tracing::debug!(
            elapsed_ms = direct_start.elapsed().as_millis() as u64,
            "search.code: run_direct_semantic (under lock)"
        );
        match direct {
            DirectResult::Found(hits) => {
                return Ok(CodeHits::Ready { hits, roots });
            }
            DirectResult::Terminal(error) => {
                return Err(external_baseline_mcp_error(&error).into());
            }
            DirectResult::Cancelled => return Err(SearchFailure::Cancelled),
            DirectResult::Unavailable => {
                if matches!(workspace_search_mode, WorkspaceSearchMode::PostgresRemoteOverlay) {
                    return Ok(CodeHits::Unavailable(SemanticUnavailable::BaselineNotReady));
                }
                // Non-Postgres: fall through to local search.
            }
        }
    }

    if matches!(workspace_search_mode, WorkspaceSearchMode::PostgresRemoteOverlay) {
        return Ok(CodeHits::Unavailable(SemanticUnavailable::BaselineRequired));
    }

    match engine.search_with_embedding_read_only(&query_embedding, limit, Some("code")) {
        Ok(hits) => Ok(CodeHits::Ready { hits, roots }),
        Err(e) => Err(McpError::internal_error(format!("search error: {e}"), None).into()),
    }
}

/// Check whether the external baseline can serve a semantic search — without embedding the query.
///
/// Called lock-free, between the two engine-guard acquisitions in `semantic_code_hits`, so a
/// not-ready baseline aborts before the ~1.4s query embed fires.
fn resolve_direct_semantic(
    source: &ExternalBaselineService,
    cancel: &CancellationToken,
    model_id: Option<&str>,
    dim: Option<usize>,
) -> DirectResolve {
    let snapshot = match source.resolve_snapshot(cancel) {
        Ok(Some((_, s))) => s,
        Ok(None) => return DirectResolve::Unavailable,
        Err(BaselineCall::Withdrawn) => return DirectResolve::Cancelled,
        Err(BaselineCall::Failed(e)) => {
            if e.is_terminal() {
                warn!("direct semantic: terminal snapshot resolution error: {e}");
                return DirectResolve::Terminal(e);
            }
            warn!("direct semantic: snapshot resolution failed: {e}");
            return DirectResolve::Unavailable;
        }
    };
    let Some(model_id) = model_id else {
        return DirectResolve::Unavailable;
    };
    let Some(dim) = dim else {
        return DirectResolve::Unavailable;
    };
    DirectResolve::Ready { snapshot, model_id: model_id.to_owned(), dim }
}

/// Execute the external baseline semantic search with a precomputed query vector.
///
/// Called under the engine lock after [`resolve_direct_semantic`] confirmed readiness and the
/// embed completed. The snapshot and model identity were resolved in the lock-free phase and are
/// passed in directly; no second `resolve_snapshot` call is made.
#[allow(
    clippy::too_many_arguments,
    reason = "the resolved identity travels as the separate values the actor call takes"
)]
fn run_direct_semantic(
    engine: &SearchEngine,
    source: &ExternalBaselineService,
    cancel: &CancellationToken,
    snapshot: &bsl_search::Snapshot,
    model_id: &str,
    dim: usize,
    query_embedding: &[f32],
    limit: usize,
) -> DirectResult {
    let (overlay_hits, hidden_paths) = match engine
        .workspace_overlay_semantic_hits_with_embedding_read_only(query_embedding, limit)
    {
        Ok(r) => r,
        Err(e) => {
            warn!("direct semantic: overlay query failed: {e}");
            return DirectResult::Unavailable;
        }
    };
    let overlay_semantic: Vec<SemanticHit> =
        overlay_hits.iter().map(SearchHit::to_semantic).collect();
    merge_direct_semantic_with_refill(
        &overlay_semantic,
        &hidden_paths,
        cancel,
        limit,
        |fetch_limit| {
            source.semantic_search(
                cancel,
                snapshot.id.0.as_str(),
                query_embedding,
                model_id,
                dim,
                Some("code"),
                fetch_limit,
            )
        },
    )
}

fn merge_direct_semantic_with_refill<F>(
    overlay_hits: &[SemanticHit],
    hidden_paths: &HashSet<bsl_search::FileKey>,
    cancel: &CancellationToken,
    limit: usize,
    mut fetch_baseline: F,
) -> DirectResult
where
    F: FnMut(usize) -> Result<Vec<SemanticHit>, BaselineCall>,
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
                    warn!("direct semantic: terminal serving query error: {e}");
                    return DirectResult::Terminal(e);
                }
                warn!("direct semantic: serving query failed: {e}");
                return DirectResult::Unavailable;
            }
        };

        best = merge_semantic(&baseline_hits, overlay_hits, &context, limit)
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
    use super::super::test_support::semantic_hit;
    use super::super::types::{CodeHits, DirectResult, SemanticUnavailable};
    use super::{merge_direct_semantic_with_refill, semantic_code_hits};
    use crate::state::{SemanticRuntimeStatus, WorkspaceSearchMode};
    use bsl_search::{EmbedderConfig, SearchConfig, SearchEngine};
    use std::collections::HashSet;
    use std::sync::{Arc, Mutex};
    use tempfile::tempdir;

    #[test]
    fn direct_semantic_refill_recovers_results_hidden_by_overlay() {
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
            semantic_hit("src/hidden1.bsl", "Hidden1", 1.00),
            semantic_hit("src/hidden2.bsl", "Hidden2", 0.99),
            semantic_hit("src/hidden3.bsl", "Hidden3", 0.98),
            semantic_hit("src/hidden4.bsl", "Hidden4", 0.97),
            semantic_hit("src/hidden5.bsl", "Hidden5", 0.96),
            semantic_hit("src/hidden6.bsl", "Hidden6", 0.95),
            semantic_hit("src/hidden7.bsl", "Hidden7", 0.94),
            semantic_hit("src/hidden8.bsl", "Hidden8", 0.93),
            semantic_hit("src/hidden9.bsl", "Hidden9", 0.92),
            semantic_hit("src/visible1.bsl", "Visible1", 0.91),
            semantic_hit("src/visible2.bsl", "Visible2", 0.90),
            semantic_hit("src/visible3.bsl", "Visible3", 0.89),
        ];
        let mut requested_limits = Vec::new();

        let result = merge_direct_semantic_with_refill(
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
            panic!("expected semantic refill to produce hits");
        };
        assert_eq!(hits.len(), 3);
        assert_eq!(
            hits.iter().map(|hit| hit.file_path.as_str()).collect::<Vec<_>>(),
            vec!["src/visible1.bsl", "src/visible2.bsl", "src/visible3.bsl"]
        );
        assert_eq!(requested_limits, vec![9, 18]);
    }

    #[test]
    fn semantic_core_reports_unavailable_when_runtime_failed() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("workspace-search.db");
        let engine = Arc::new(Mutex::new(Some(SearchEngine::fts_only(&db_path).unwrap())));
        let outcome = semantic_code_hits(
            &engine,
            &tokio_util::sync::CancellationToken::new(),
            &Arc::new(Mutex::new(SemanticRuntimeStatus::Failed("overlay sync failed".to_owned()))),
            WorkspaceSearchMode::SqliteLocal,
            None,
            None,
            "обработка проведения документа",
            10,
        )
        .unwrap();

        assert!(matches!(outcome, CodeHits::Unavailable(SemanticUnavailable::RuntimeFailed)));
    }

    #[test]
    fn semantic_core_reports_pending_when_indexing() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("workspace-search.db");
        let engine = Arc::new(Mutex::new(Some(SearchEngine::fts_only(&db_path).unwrap())));
        let outcome = semantic_code_hits(
            &engine,
            &tokio_util::sync::CancellationToken::new(),
            &Arc::new(Mutex::new(SemanticRuntimeStatus::Indexing)),
            WorkspaceSearchMode::SqliteLocal,
            None,
            None,
            "обработка проведения документа",
            10,
        )
        .unwrap();

        assert!(matches!(outcome, CodeHits::Pending(_)));
    }

    #[test]
    fn semantic_core_degrades_to_lexical_when_query_embed_fails() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("workspace-search.db");
        let config = SearchConfig {
            embedder: EmbedderConfig {
                base_url: "http://127.0.0.1:1".to_owned(),
                model: "test-model".to_owned(),
                dim: Some(8),
                api_key: None,
                provider: None,
            },
            ..SearchConfig::default()
        };
        let engine = Arc::new(Mutex::new(Some(SearchEngine::new(&db_path, config).unwrap())));
        let outcome = semantic_code_hits(
            &engine,
            &tokio_util::sync::CancellationToken::new(),
            &Arc::new(Mutex::new(SemanticRuntimeStatus::Ready)),
            WorkspaceSearchMode::SqliteLocal,
            None,
            None,
            "обработка проведения документа",
            10,
        )
        .unwrap();

        assert!(matches!(
            outcome,
            CodeHits::Unavailable(SemanticUnavailable::EmbedderUnavailable(_))
        ));
    }
}
