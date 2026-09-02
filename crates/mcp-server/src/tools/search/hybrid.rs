use super::lexical::lexical_code_hits;
use super::render::{format_code_hits, hits_response, no_hits_response, Envelope};
use super::semantic::semantic_code_hits;
use super::status::search_not_ready;
use super::types::{CodeHits, SearchFailure, HYBRID_FETCH_MULTIPLIER};
use crate::baseline::{ConfiguredBaselineStatus, ExternalBaselineService};
use crate::state::{SemanticRuntimeStatus, WorkspaceSearchMode};
use bsl_search::{fuse_smart, FusedHit, IndexProgress, SearchEngine};
use rmcp::model::CallToolResult;
use rmcp::ErrorData as McpError;
use std::fmt::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

/// The one-line legend above the hits: what `[L]` / `[S]` / `[L+S]` mean.
const MODALITY_LEGEND: &str = "Modality tag per hit: [L] lexical-only · [S] semantic-only · [L+S] found by both (cross-modal agreement).\n";

/// What the hits may spend, once this profile's own wrapping is set aside: the legend above
/// them and the degradation note, which is carried THREE times — as the trailing text line, as
/// the envelope's `degraded`, and as the `modality_degraded` reason's `detail` in the
/// freshness block. (The envelope's fixed keys are charged by the renderer itself; only the
/// note grows with its own length, so it is charged here, where that length is known.) Sizing
/// the hits against the full budget and only then wrapping them in all this is how a response
/// ends up over its ceiling while reporting `budget_exhausted: false`.
fn hits_budget(max_output_tokens: usize, note: Option<&str>) -> usize {
    let reserved =
        MODALITY_LEGEND.len() + note.map_or(0, |note| note.len() * 3 + "-- {} --\n".len());
    max_output_tokens.saturating_sub(reserved.div_ceil(4))
}

/// The unified code search: run lexical and semantic, fuse by `fuse_smart` (exact-symbol tier
/// then semantic tail), and degrade to lexical (with a trailing note) when semantic cannot serve.
/// This is what the `search_code` action dispatches to.
// This is the tool-dispatch boundary: each argument is an independent runtime handle or
// per-request value pulled straight from `SharedState`, with no natural sub-grouping that a
// context struct would not make more obscure than the flat list.
#[allow(clippy::too_many_arguments)]
#[allow(dead_code)] // unmanaged, uncancellable compatibility wrapper for tests
pub fn hybrid_code(
    engine: &Arc<Mutex<Option<SearchEngine>>>,
    semantic_runtime: &Arc<Mutex<SemanticRuntimeStatus>>,
    workspace_search_mode: WorkspaceSearchMode,
    configured_baseline: Option<&ConfiguredBaselineStatus>,
    external_baseline: Option<Arc<ExternalBaselineService>>,
    graph_root: Option<&Path>,
    index_progress: &IndexProgress,
    query: &str,
    limit: usize,
    max_output_tokens: usize,
) -> Result<CallToolResult, McpError> {
    hybrid_code_cancellable(
        engine,
        &CancellationToken::new(),
        semantic_runtime,
        workspace_search_mode,
        configured_baseline,
        external_baseline,
        graph_root,
        index_progress,
        query,
        limit,
        max_output_tokens,
    )
    .map_err(|failure| match failure {
        SearchFailure::Error(error) => error,
        SearchFailure::Cancelled => McpError::internal_error("request cancelled", None),
    })
}

#[allow(clippy::too_many_arguments)]
pub fn hybrid_code_cancellable(
    engine: &Arc<Mutex<Option<SearchEngine>>>,
    cancel: &CancellationToken,
    semantic_runtime: &Arc<Mutex<SemanticRuntimeStatus>>,
    workspace_search_mode: WorkspaceSearchMode,
    configured_baseline: Option<&ConfiguredBaselineStatus>,
    external_baseline: Option<Arc<ExternalBaselineService>>,
    graph_root: Option<&Path>,
    index_progress: &IndexProgress,
    query: &str,
    limit: usize,
    max_output_tokens: usize,
) -> Result<CallToolResult, SearchFailure> {
    // Over-fetch each modality so a hit ranked just outside `limit` in one but boosted by the
    // other can still surface after fusion.
    let fetch = limit.saturating_mul(HYBRID_FETCH_MULTIPLIER).max(limit);

    let lexical = lexical_code_hits(
        engine,
        cancel,
        workspace_search_mode.clone(),
        configured_baseline,
        external_baseline.clone(),
        query,
        fetch,
    )?;
    let (lex_hits, roots) = match lexical {
        CodeHits::Ready { hits, roots } => (hits, roots),
        // Lexical is the floor: if it cannot serve yet, the whole search cannot — return a
        // structured not-ready envelope (machine status + live counters + retry hint),
        // matching the graph tool, rather than a bare sentence a poller must parse.
        CodeHits::Pending(message) => {
            return Ok(search_not_ready(&message, index_progress, "search_code"));
        }
        // Lexical search is always available, so it never reports a semantic shortfall; treat
        // it defensively as "still building".
        CodeHits::Unavailable(_) => {
            return Ok(search_not_ready(
                "Search index is being built, please try again in a moment.",
                index_progress,
                "search_code",
            ));
        }
    };

    // Between the modalities: the lexical answer is worth nothing to a caller that has
    // gone, and the semantic half is the expensive one.
    if cancel.is_cancelled() {
        return Err(SearchFailure::Cancelled);
    }
    let semantic = semantic_code_hits(
        engine,
        cancel,
        semantic_runtime,
        workspace_search_mode,
        configured_baseline,
        external_baseline,
        query,
        fetch,
    )?;

    let (mut hits, note): (Vec<FusedHit>, Option<String>) = match semantic {
        CodeHits::Ready { hits: sem_hits, .. } => {
            (fuse_smart(&lex_hits, &sem_hits, query, limit), None)
        }
        // Semantic could not serve — degrade to lexical-only by fusing against an empty semantic
        // list, so the exact-symbol tier still floats. Surface the precise upstream pending reason
        // (overlay warmup, local RAG indexing, or index build) verbatim rather than collapsing
        // them to one generic note.
        CodeHits::Pending(message) => (fuse_smart(&lex_hits, &[], query, limit), Some(message)),
        CodeHits::Unavailable(reason) => {
            (fuse_smart(&lex_hits, &[], query, limit), Some(reason.note()))
        }
    };
    hits.truncate(limit);

    if hits.is_empty() {
        // The text stays the bare sentence it has always been; the degradation reaches a
        // machine consumer through the envelope, where an empty list plus `degraded` reads as
        // "half the search was down" rather than "there is nothing".
        return Ok(no_hits_response(note.as_deref(), Envelope::Yes, "search_code"));
    }

    Ok(assemble_code_response(
        &hits,
        roots.as_ref(),
        graph_root,
        note.as_deref(),
        max_output_tokens,
    ))
}

/// The served response: the legend, the hits sized against what the wrapping will spend, the
/// degradation note, the envelope.
///
/// Split out from [`hybrid_code`] so a test can choose the note's LENGTH: in production the
/// note is picked by whichever subsystem was down, and the longest of them — the
/// embedding-identity mismatch, some 230 characters — needs a live baseline with a
/// conflicting model to provoke. Since the note is charged to the budget by its length, a
/// ceiling checked only against the short notes is a ceiling checked at one point.
fn assemble_code_response(
    hits: &[FusedHit],
    roots: Option<&bsl_search::WorkspaceRoots>,
    graph_root: Option<&Path>,
    note: Option<&str>,
    max_output_tokens: usize,
) -> CallToolResult {
    // Explain the per-hit modality tag once, up front — a leading line does not shift the
    // per-hit `graph_id:` parsing (which is relative to each `#N` line).
    let mut out = String::from(MODALITY_LEGEND);
    let rendered = format_code_hits(hits, roots, graph_root, hits_budget(max_output_tokens, note));
    out.push_str(&rendered.text);
    if let Some(note) = note {
        // Append AFTER the hit lines — never before — so a client parsing `graph_id:` lines
        // positionally is not shifted.
        let _ = writeln!(out, "-- {note} --");
    }
    hits_response(out, rendered, note, Envelope::Yes, "search_code")
}

#[cfg(test)]
pub(super) mod tests {
    use super::super::lexical::lexical_code_hits;
    use super::super::semantic::semantic_code_hits;
    use super::super::test_support::{code_hit, retryable_postgres_source};
    use super::{assemble_code_response, hybrid_code};
    use crate::baseline::ConfiguredBaselineStatus;
    use crate::state::{SemanticRuntimeStatus, WorkspaceSearchMode};
    use bsl_search::{FileKey, FusedHit, IndexProgress, Modality, ModuleSnapshot, SearchEngine};
    use project_model::{ResolvedWorkspaceBaselineSupport, SearchBaselineSupportState};
    use rmcp::model::ErrorCode;
    use std::fs;
    use std::sync::atomic::Ordering;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};
    use tempfile::tempdir;
    use tokio_util::sync::CancellationToken;

    pub(in crate::tools::search) fn assert_all_search_modes_are_resident_only_under_held_lease() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let file = workspace.join("CommonModule.bsl");
        fs::write(&file, "Процедура Базовая()\nКонецПроцедуры").unwrap();
        let cache = crate::cache::WorkspaceCacheLayout::for_workspace(workspace);
        cache.ensure().unwrap();
        let mut engine = SearchEngine::fts_only(&cache.search_db_path()).unwrap();
        engine.index_directory_fts(workspace).unwrap();
        engine.set_workspace_root(workspace);
        engine.initialize_workspace_overlay_clean().unwrap();
        engine.enable_workspace_watcher_mode();

        let key = FileKey::configuration("CommonModule.bsl");
        fs::write(&file, "Процедура Предыдущая()\nКонецПроцедуры").unwrap();
        assert!(engine.mark_workspace_path_dirty(&file).unwrap());
        let text = fs::read_to_string(&file).unwrap();
        engine
            .reindex_dirty_from_snapshots(&std::collections::HashMap::from([(
                key.clone(),
                ModuleSnapshot { root: parser::parse(&text).syntax_node(), text: text.into() },
            )]))
            .unwrap();
        let before_hash = engine.store().file_hash(&key.root_id, &key.path).unwrap();

        let lease = crate::workspace_lease::WorkspaceLease::claim(workspace);
        let shared = Arc::new(Mutex::new(Some(engine)));
        let failed = Arc::new(Mutex::new(SemanticRuntimeStatus::Failed("test".to_owned())));

        fs::write(&file, "Процедура Новая()\nКонецПроцедуры").unwrap();
        shared.lock().unwrap().as_ref().unwrap().mark_workspace_path_dirty(&file).unwrap();
        let held_lease = lease.hold_file_lock_for_test();
        let started = Instant::now();

        let lexical = lexical_code_hits(
            &shared,
            &CancellationToken::new(),
            WorkspaceSearchMode::SqliteLocal,
            None,
            None,
            "Предыдущая",
            10,
        )
        .unwrap();
        let semantic = semantic_code_hits(
            &shared,
            &CancellationToken::new(),
            &failed,
            WorkspaceSearchMode::SqliteLocal,
            None,
            None,
            "Предыдущая",
            10,
        )
        .unwrap();
        assert!(started.elapsed() < Duration::from_millis(500));
        drop(held_lease);
        let hybrid = hybrid_code(
            &shared,
            &failed,
            WorkspaceSearchMode::SqliteLocal,
            None,
            None,
            None,
            &IndexProgress::new(),
            "Предыдущая",
            10,
            usize::MAX,
        )
        .unwrap();

        let super::super::types::CodeHits::Ready { hits, .. } = lexical else {
            panic!("lexical search must serve the resident snapshot")
        };
        assert!(hits.iter().any(|hit| hit.symbol_name == "Предыдущая"));
        assert!(matches!(semantic, super::super::types::CodeHits::Unavailable(_)));
        let hybrid_text = hybrid.content[0].as_text().unwrap().text.as_str();
        assert!(hybrid_text.contains("Предыдущая"), "{hybrid_text}");
        assert!(!hybrid_text.contains("Новая"), "{hybrid_text}");
        assert!(!lease.is_superseded());

        let guard = shared.lock().unwrap();
        let engine = guard.as_ref().unwrap();
        assert!(engine.workspace_overlay_dirty_paths().unwrap().contains(&key));
        assert_eq!(engine.store().file_hash(&key.root_id, &key.path).unwrap(), before_hash);
        let snapshot = engine.workspace_overlay_snapshot().unwrap();
        assert!(snapshot.lexical_documents.iter().any(|doc| doc.symbol_name == "Предыдущая"));
        assert!(!snapshot.lexical_documents.iter().any(|doc| doc.symbol_name == "Новая"));
    }

    #[test]
    fn hybrid_serves_lexical_while_semantic_indexing() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        fs::write(workspace.join("CommonModule.bsl"), "Процедура ПроверитьИНН()\nКонецПроцедуры")
            .unwrap();
        let db_path = workspace.join("bsl-search.db");
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.index_directory_fts(workspace).unwrap();
        engine.set_workspace_root(workspace);

        let result = hybrid_code(
            &Arc::new(Mutex::new(Some(engine))),
            &Arc::new(Mutex::new(SemanticRuntimeStatus::Indexing)),
            WorkspaceSearchMode::SqliteLocal,
            None,
            None,
            None,
            &IndexProgress::new(),
            "ПроверитьИНН",
            10,
            usize::MAX,
        )
        .unwrap();
        let text = result.content[0].as_text().expect("text content").text.as_str();

        assert!(text.contains("ПроверитьИНН"), "{text}");
        assert!(text.contains("-- RAG semantic index is still building"), "{text}");
    }

    #[test]
    fn hybrid_degrades_to_lexical_with_note_when_semantic_runtime_failed() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        fs::write(workspace.join("CommonModule.bsl"), "Процедура ПроверитьИНН()\nКонецПроцедуры")
            .unwrap();
        let db_path = workspace.join("bsl-search.db");
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.index_directory_fts(workspace).unwrap();
        engine.set_workspace_root(workspace);

        let result = hybrid_code(
            &Arc::new(Mutex::new(Some(engine))),
            &Arc::new(Mutex::new(SemanticRuntimeStatus::Failed("overlay sync failed".to_owned()))),
            WorkspaceSearchMode::SqliteLocal,
            None,
            None,
            None,
            &IndexProgress::new(),
            "ПроверитьИНН",
            10,
            usize::MAX,
        )
        .unwrap();
        let text = result.content[0].as_text().expect("text content").text.as_str();

        assert!(text.contains("ПроверитьИНН"), "{text}");
        assert!(text.contains("-- semantic skipped: runtime initialization failed --"), "{text}");
    }

    #[test]
    fn hybrid_degrade_note_follows_hit_lines_and_empty_results_suppress_it() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        fs::write(workspace.join("CommonModule.bsl"), "Процедура ПроверитьИНН()\nКонецПроцедуры")
            .unwrap();
        let db_path = workspace.join("bsl-search.db");
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.index_directory_fts(workspace).unwrap();
        engine.set_workspace_root(workspace);
        let engine = Arc::new(Mutex::new(Some(engine)));
        let failed = Arc::new(Mutex::new(SemanticRuntimeStatus::Failed("boom".to_owned())));

        let hit_result = hybrid_code(
            &engine,
            &failed,
            WorkspaceSearchMode::SqliteLocal,
            None,
            None,
            None,
            &IndexProgress::new(),
            "ПроверитьИНН",
            10,
            usize::MAX,
        )
        .unwrap();
        let text = hit_result.content[0].as_text().expect("text").text.as_str();
        let hit_pos = text.find("ПроверитьИНН").expect("hit line present");
        let note_pos = text.find("-- semantic skipped").expect("note present");
        assert!(note_pos > hit_pos, "note must trail the hit lines: {text}");

        let empty_result = hybrid_code(
            &engine,
            &failed,
            WorkspaceSearchMode::SqliteLocal,
            None,
            None,
            None,
            &IndexProgress::new(),
            "несуществующийидентификатор",
            10,
            usize::MAX,
        )
        .unwrap();
        let empty_text = empty_result.content[0].as_text().expect("text").text.as_str();
        assert_eq!(empty_text, "No results found.");
        assert!(!empty_text.contains("--"), "no trailing note without hits: {empty_text}");
        // The text says nothing about the degradation, but a machine consumer must not read
        // this as "the configuration has no such code": half the search was down.
        let empty_body = empty_result.structured_content.as_ref().expect("structured envelope");
        assert_eq!(empty_body["hits"], serde_json::json!([]));
        assert_eq!(empty_body["degraded"], "semantic skipped: runtime initialization failed");
    }

    /// The same ceiling, measured where it depends on a value that GROWS: the degradation
    /// note rides the response three times — the trailing text line, the envelope's
    /// `degraded`, and the `modality_degraded` reason's `detail` — so a charge that does not
    /// scale with its length lets a long note push the answer past the ceiling while it still
    /// reports `budget_exhausted: false`. Production notes run from 44 characters (a runtime
    /// that failed to start) to some 230 (the embedding-identity mismatch), and a ceiling
    /// checked only at the short end is a ceiling checked at one point.
    #[test]
    fn a_long_degradation_note_is_charged_to_the_budget_that_carries_it() {
        let hits: Vec<FusedHit> = (1..=6)
            .map(|i| FusedHit {
                hit: code_hit(
                    &format!("CommonModules/Модуль{i}/Ext/Module.bsl"),
                    "ПроверитьИНН",
                    "procedure",
                ),
                modality: Modality::Lexical,
            })
            .collect();

        let mut saw_complete_answer = false;
        for note_length in [44usize, 230, 800] {
            let note = "с".repeat(note_length);
            for budget in (50usize..=2000).step_by(25) {
                let result = assemble_code_response(&hits, None, None, Some(&note), budget);
                let text = result.content[0].as_text().expect("text").text.as_str();
                let body = result.structured_content.as_ref().expect("structured");
                let size = text.len() + serde_json::to_string(body).unwrap().len();

                if body.get("budget_exhausted").is_none() {
                    saw_complete_answer = true;
                    assert!(
                        size <= budget * 4,
                        "note of {note_length} chars, budget {budget}: {size} chars shipped \
                         as a complete answer",
                    );
                }
            }
        }
        assert!(
            saw_complete_answer,
            "the sweep must include answers that fit, or it proves nothing"
        );
    }

    #[test]
    fn the_whole_response_stays_within_the_budget_it_was_given() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        for i in 1..=6 {
            fs::write(
                workspace.join(format!("Модуль{i}.bsl")),
                "Процедура ПроверитьИНН()\n\tВозврат;\nКонецПроцедуры",
            )
            .unwrap();
        }
        let db_path = workspace.join("bsl-search.db");
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.index_directory_fts(workspace).unwrap();
        engine.set_workspace_root(workspace);
        let engine = Arc::new(Mutex::new(Some(engine)));

        // A sweep across the boundary where the hits alone fit but the assembled response does
        // not — the case that overshoots when only the hit blocks are sized.
        let mut saw_complete_answer = false;
        for budget in [200usize, 400, 700, 875, 1000, 1200] {
            let result = hybrid_code(
                &engine,
                &Arc::new(Mutex::new(SemanticRuntimeStatus::Failed("boom".to_owned()))),
                WorkspaceSearchMode::SqliteLocal,
                None,
                None,
                None,
                &IndexProgress::new(),
                "ПроверитьИНН",
                10,
                budget,
            )
            .unwrap();

            let text = result.content[0].as_text().expect("text").text.as_str();
            let body = result.structured_content.as_ref().expect("structured");
            let size = text.len() + serde_json::to_string(body).unwrap().len();

            // The flag's whole contract: absent means "this answer fits the ceiling you set".
            // Everything the response carries counts — legend, hits, degradation note, envelope
            // — not just the hit blocks that were measured while rendering.
            if body.get("budget_exhausted").is_none() {
                saw_complete_answer = true;
                assert!(
                    size <= budget * 4,
                    "budget {budget}: {size} chars shipped as a complete answer: {text}",
                );
            }
        }
        assert!(
            saw_complete_answer,
            "the sweep must include budgets that fit, or it proves nothing"
        );
    }

    #[test]
    fn hybrid_hits_carry_the_structured_listing_beside_the_text() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        fs::write(
            workspace.join("CommonModule.bsl"),
            "Процедура ПроверитьИНН()\n\tВозврат;\nКонецПроцедуры",
        )
        .unwrap();
        let db_path = workspace.join("bsl-search.db");
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.index_directory_fts(workspace).unwrap();
        engine.set_workspace_root(workspace);

        let result = hybrid_code(
            &Arc::new(Mutex::new(Some(engine))),
            &Arc::new(Mutex::new(SemanticRuntimeStatus::Failed("boom".to_owned()))),
            WorkspaceSearchMode::SqliteLocal,
            None,
            None,
            None,
            &IndexProgress::new(),
            "ПроверитьИНН",
            10,
            usize::MAX,
        )
        .unwrap();

        let text = result.content[0].as_text().expect("text mirror").text.as_str();
        assert!(text.starts_with("Modality tag per hit:"), "text listing unchanged: {text}");

        let body = result.structured_content.as_ref().expect("structured listing");
        assert_eq!(body["schema_version"], "4");
        assert_eq!(body["action"], "search_code");
        let hits = body["hits"].as_array().expect("hits array");
        assert_eq!(hits.len(), body["shown"].as_u64().unwrap() as usize);
        assert_eq!(body["total"], serde_json::json!(hits.len()));
        assert!(body.get("budget_exhausted").is_none(), "nothing was cut: {body}");
        assert_eq!(body["degraded"], "semantic skipped: runtime initialization failed");

        let first = &hits[0];
        assert_eq!(first["rank"], 1);
        assert_eq!(first["modality"], "L");
        assert_eq!(first["path"], "CommonModule.bsl");
        assert_eq!(first["symbol"], "ПроверитьИНН");
        // Every structured field is also on screen, so the two views cannot drift apart.
        assert!(text.contains(first["path"].as_str().unwrap()), "{text}");
        assert!(text.contains(first["symbol"].as_str().unwrap()), "{text}");
    }

    #[test]
    fn hybrid_code_not_ready_returns_structured_envelope() {
        let engine: Arc<Mutex<Option<SearchEngine>>> = Arc::new(Mutex::new(None));
        let runtime = Arc::new(Mutex::new(SemanticRuntimeStatus::Ready));
        let progress = Arc::new(IndexProgress::default());
        progress.active.store(true, Ordering::Relaxed);
        progress.total_chunks.store(100, Ordering::Relaxed);
        progress.done_chunks.store(40, Ordering::Relaxed);
        progress.total_batches.store(10, Ordering::Relaxed);
        progress.done_batches.store(4, Ordering::Relaxed);

        let result = hybrid_code(
            &engine,
            &runtime,
            WorkspaceSearchMode::SqliteLocal,
            None,
            None,
            None,
            &progress,
            "ПроверитьИНН",
            10,
            usize::MAX,
        )
        .unwrap();

        let body = result.structured_content.as_ref().expect("structured not-ready envelope");
        assert_eq!(body["status"], "not_ready");
        assert_eq!(body["retry_after_ms"], 1500);
        assert_eq!(body["progress"]["active"], true);
        assert_eq!(body["progress"]["pct"], 40);
        assert_eq!(body["progress"]["chunks"]["done"], 40);
        assert_eq!(body["progress"]["batches"]["total"], 10);

        let text = result.content[0].as_text().expect("text mirror").text.as_str();
        let mirror: serde_json::Value =
            serde_json::from_str(text).expect("text mirror must be valid JSON");
        assert_eq!(&mirror, body, "text mirror must match structuredContent");
    }

    #[test]
    fn hybrid_code_not_ready_omits_counters_when_inactive() {
        let engine: Arc<Mutex<Option<SearchEngine>>> = Arc::new(Mutex::new(None));
        let runtime = Arc::new(Mutex::new(SemanticRuntimeStatus::Ready));
        let progress = Arc::new(IndexProgress::default());
        progress.total_chunks.store(100, Ordering::Relaxed);
        progress.done_chunks.store(100, Ordering::Relaxed);

        let result = hybrid_code(
            &engine,
            &runtime,
            WorkspaceSearchMode::SqliteLocal,
            None,
            None,
            None,
            &progress,
            "ПроверитьИНН",
            10,
            usize::MAX,
        )
        .unwrap();

        let body = result.structured_content.as_ref().expect("structured not-ready envelope");
        assert_eq!(body["status"], "not_ready");
        assert_eq!(body["progress"]["active"], false);
        assert!(body["progress"]["pct"].is_null(), "no stale pct when inactive: {body}");
        assert!(body["progress"]["chunks"].is_null(), "no stale counters when inactive: {body}");
    }

    #[test]
    fn code_search_returns_structured_error_when_workspace_branch_is_expired() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("workspace-search.db");
        let engine = Arc::new(Mutex::new(Some(SearchEngine::fts_only(&db_path).unwrap())));
        let configured = ConfiguredBaselineStatus {
            backend: "postgres",
            selection: "workspace branch feature/demo -> branch develop -> branch vendor".to_owned(),
            issue: None,
            support: Some(ResolvedWorkspaceBaselineSupport {
                state: SearchBaselineSupportState::Expired,
                workspace_branch: Some("feature/demo".to_owned()),
                selected_branch: Some("develop".to_owned()),
                snapshot_age_days: 45,
                stale_after_days: 21,
                expire_after_days: 30,
                reason: "workspace branch 'feature/demo' uses shared baseline branch 'develop' published 45 days ago".to_owned(),
            }),
        };

        let error = hybrid_code(
            &engine,
            &Arc::new(Mutex::new(SemanticRuntimeStatus::Disabled)),
            WorkspaceSearchMode::PostgresRemoteOverlay,
            Some(&configured),
            None,
            None,
            &IndexProgress::new(),
            "Процедура",
            10,
            usize::MAX,
        )
        .unwrap_err();

        assert!(error.message.contains("expired"));
        assert!(error.message.contains("Update the branch from develop"));
    }

    #[test]
    fn code_search_rejects_local_fallback_when_postgres_baseline_is_unavailable() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let file = workspace.join("CommonModule.bsl");
        fs::write(&file, "Процедура ТестоваПроцедура()\nКонецПроцедуры").unwrap();

        let db_path = workspace.join("bsl-search.db");
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.index_directory_fts(workspace).unwrap();
        engine.set_workspace_root(workspace);

        let error = hybrid_code(
            &Arc::new(Mutex::new(Some(engine))),
            &Arc::new(Mutex::new(SemanticRuntimeStatus::Disabled)),
            WorkspaceSearchMode::PostgresRemoteOverlay,
            Some(&ConfiguredBaselineStatus {
                backend: "postgres",
                selection: "branch main".to_owned(),
                issue: Some("failed to resolve PostgreSQL reader credentials".to_owned()),
                support: None,
            }),
            None,
            None,
            &IndexProgress::new(),
            "ТестоваПроцедура",
            10,
            usize::MAX,
        )
        .unwrap_err();

        assert_eq!(error.code, ErrorCode::INVALID_PARAMS);
        assert!(error.message.contains("Shared baseline is unavailable"));
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
    fn code_search_surfaces_retry_exhausted_external_baseline_errors() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let file = workspace.join("CommonModule.bsl");
        fs::write(&file, "Процедура ТестоваПроцедура()\nКонецПроцедуры").unwrap();

        let db_path = workspace.join("bsl-search.db");
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.index_directory_fts(workspace).unwrap();
        engine.set_workspace_root(workspace);

        let error = hybrid_code(
            &Arc::new(Mutex::new(Some(engine))),
            &Arc::new(Mutex::new(SemanticRuntimeStatus::Disabled)),
            WorkspaceSearchMode::PostgresRemoteOverlay,
            Some(&ConfiguredBaselineStatus {
                backend: "postgres",
                selection: "branch main".to_owned(),
                issue: None,
                support: None,
            }),
            Some(retryable_postgres_source()),
            None,
            &IndexProgress::new(),
            "ТестоваПроцедура",
            10,
            usize::MAX,
        )
        .unwrap_err();

        assert_eq!(error.code, ErrorCode::INTERNAL_ERROR);
        assert!(error.message.contains("external baseline error"));
        assert_eq!(
            error
                .data
                .as_ref()
                .and_then(|data| data.get("reasonCode"))
                .and_then(|value| value.as_str()),
            Some("refresh_retry_exhausted")
        );
    }

    #[test]
    fn code_search_surfaces_retry_exhausted_errors_for_empty_queries() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("workspace-search.db");
        let engine = Arc::new(Mutex::new(Some(SearchEngine::fts_only(&db_path).unwrap())));

        let error = hybrid_code(
            &engine,
            &Arc::new(Mutex::new(SemanticRuntimeStatus::Disabled)),
            WorkspaceSearchMode::PostgresRemoteOverlay,
            Some(&ConfiguredBaselineStatus {
                backend: "postgres",
                selection: "branch main".to_owned(),
                issue: None,
                support: None,
            }),
            Some(retryable_postgres_source()),
            None,
            &IndexProgress::new(),
            "НесуществующееСлово",
            10,
            usize::MAX,
        )
        .unwrap_err();

        assert_eq!(error.code, ErrorCode::INTERNAL_ERROR);
        assert_eq!(
            error
                .data
                .as_ref()
                .and_then(|data| data.get("reasonCode"))
                .and_then(|value| value.as_str()),
            Some("refresh_retry_exhausted")
        );
    }
}
