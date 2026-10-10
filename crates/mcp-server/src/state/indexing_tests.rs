use super::*;
use crate::{McpProfile, McpServer};
use bsl_search::{IndexPassState, SearchEngine};
use rmcp::handler::server::wrapper::Parameters;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

fn targets(state: &SharedState) -> Value {
    serde_json::to_value(state.workspace_indexing()).unwrap()["targets"].clone()
}

#[test]
fn indexing_owner_lifecycles_reference() {
    let state = SharedState::shared();
    for (lifecycle, expected) in [
        (ReferenceSearchLifecycle::Uninitialized, "waiting"),
        (ReferenceSearchLifecycle::Loading, "running"),
        (ReferenceSearchLifecycle::Ready, "ready"),
        (
            ReferenceSearchLifecycle::Failed {
                message: "private path/token must not escape".into(),
                reason_code: "arbitrary".into(),
            },
            "failed",
        ),
    ] {
        *state.reference_search.lifecycle.lock().unwrap() = lifecycle;
        let wire = serde_json::to_value(state.reference_indexing()).unwrap();
        let target = &wire["targets"][0];
        assert_eq!(target["state"], expected);
        for field in ["phase", "progress", "pass_id"] {
            assert!(target[field].is_null());
        }
        assert!(!wire.to_string().contains("private"));
    }
    let held = state.reference_search.lifecycle.lock().unwrap();
    assert_eq!(
        serde_json::to_value(state.reference_indexing()).unwrap()["targets"][0]["state"],
        "unknown"
    );
    drop(held);
    state.reference_search.stopped.store(true, Ordering::Release);
    assert_eq!(
        serde_json::to_value(state.reference_indexing()).unwrap()["targets"][0]["state"],
        "cancelled"
    );
}

#[test]
fn indexing_local_qualification_projection() {
    let dir = tempfile::tempdir().unwrap();
    let state = SharedState::shared();
    *state.search_engine.lock().unwrap() =
        Some(SearchEngine::fts_only(&dir.path().join("search.db")).unwrap());
    assert_eq!(targets(&state)[0]["state"], "ready");
    assert_eq!(targets(&state)[1]["state"], "disabled");
    *state.semantic_runtime.lock().unwrap() = SemanticRuntimeStatus::Ready;
    assert_eq!(targets(&state)[1]["reason_code"], "pending_work");
    state.search_engine.lock().unwrap().as_mut().unwrap().set_workspace_root(dir.path());
    state
        .search_engine
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .initialize_workspace_overlay_clean()
        .unwrap();
    *state.semantic_runtime.lock().unwrap() = SemanticRuntimeStatus::Ready;
    assert_eq!(targets(&state)[1]["reason_code"], "identity_unverified");
    let mut pass = state.index_progress.begin_pass();
    pass.token().set_totals(1, 5, 1);
    pass.token().advance(3, 0);
    let id = targets(&state)[1]["pass_id"].clone();
    assert_eq!(targets(&state)[1]["state"], "running");
    assert_eq!(targets(&state)[1]["progress"]["completed"], 3);
    pass.finish(IndexPassState::Failed);
    assert_eq!(targets(&state)[1]["state"], "failed");
    assert_eq!(targets(&state)[1]["pass_id"], id);
    assert!(targets(&state)[1]["progress"].is_null());
    let mut next = state.index_progress.begin_pass();
    next.finish(IndexPassState::Superseded);
    assert_eq!(targets(&state)[1]["state"], "superseded");
    assert_ne!(targets(&state)[1]["pass_id"], id);
    let mut ready = state.index_progress.begin_pass();
    ready.finish(IndexPassState::Ready);
    let ready_id = targets(&state)[1]["pass_id"].clone();
    let held = state.search_engine.lock().unwrap();
    assert_eq!(targets(&state)[1]["state"], "unknown");
    assert_eq!(
        targets(&state)[1]["pass_id"],
        ready_id,
        "engine contention cannot erase a known attempt"
    );
    drop(held);
    *state.semantic_runtime.lock().unwrap() = SemanticRuntimeStatus::EmbeddingFailed(
        bsl_search::EmbeddingFailure::new(bsl_search::EmbeddingFailureCode::EmbeddingTimeout),
    );
    assert_eq!(targets(&state)[1]["state"], "failed", "a typed embedding failure is a failure");
    assert_eq!(targets(&state)[1]["reason_code"], "native_failure");
}

#[tokio::test]
async fn indexing_workspace_responses() {
    let state = SharedState::shared();
    *state.semantic_runtime.lock().unwrap() = SemanticRuntimeStatus::Indexing;
    let server = McpServer::new(McpProfile::Workspace, state.clone());
    for action in ["status", "search_code"] {
        let response = server
            .workspace_search(
                Parameters(
                    serde_json::from_value(json!({"action":action,"query":"needle"})).unwrap(),
                ),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let body = response.structured_content.unwrap();
        assert_eq!(body["indexing"]["targets"][0]["kind"], "lexical");
        assert_eq!(body["indexing"]["targets"][1]["kind"], "semantic");
        assert_eq!(body["indexing"]["targets"].as_array().unwrap().len(), 2);
    }
    let dir = tempfile::tempdir().unwrap();
    *state.search_engine.lock().unwrap() =
        Some(SearchEngine::fts_only(&dir.path().join("search.db")).unwrap());
    let mut pass = state.index_progress.begin_pass();
    pass.token().set_totals(1, 10, 1);
    pass.token().advance(4, 0);
    let response = server
        .workspace_search(
            Parameters(
                serde_json::from_value(json!({"action":"search_code","query":"needle"})).unwrap(),
            ),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let body = response.structured_content.unwrap();
    assert_eq!(body["indexing"]["targets"][0]["state"], "ready");
    assert_eq!(body["indexing"]["targets"][1]["state"], "running");
    assert_eq!(body["indexing"]["targets"][1]["progress"]["completed"], 4);
    pass.finish(IndexPassState::Ready);
    let error = server
        .workspace_search(
            Parameters(
                serde_json::from_value(
                    json!({"action":"search_code","query":"needle","max_output_tokens":0}),
                )
                .unwrap(),
            ),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.message, "budget_too_small");
    let listed = server
        .workspace_search(
            Parameters(
                serde_json::from_value(
                    json!({"action":"list_platform","name":"NoSuchPlatformName"}),
                )
                .unwrap(),
            ),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(listed.structured_content.unwrap().get("indexing").is_none());
}

#[tokio::test]
async fn indexing_reference_responses() {
    let dir = tempfile::tempdir().unwrap();
    for profile in [McpProfile::Workspace, McpProfile::Reference] {
        let mut state = SharedState::shared();
        *state.reference_search.lifecycle.lock().unwrap() = ReferenceSearchLifecycle::Loading;
        state.search_engine = state.reference_search.engine.clone();
        let server = McpServer::new(profile, state.clone());
        for ready in [false, true] {
            if ready {
                *state.reference_search.engine.lock().unwrap() = Some(
                    SearchEngine::fts_only(&dir.path().join(format!("{}.db", profile.as_str())))
                        .unwrap(),
                );
                *state.reference_search.lifecycle.lock().unwrap() = ReferenceSearchLifecycle::Ready;
            }
            for action in ["find_docs", "search_docs", "status"] {
                if profile == McpProfile::Workspace && action == "status" {
                    continue;
                }
                let params = json!({"action":action,"query":"missing"});
                let result = if profile == McpProfile::Workspace {
                    server
                        .workspace_search(
                            Parameters(serde_json::from_value(params).unwrap()),
                            CancellationToken::new(),
                        )
                        .await
                } else {
                    server
                        .reference_search(
                            Parameters(serde_json::from_value(params).unwrap()),
                            CancellationToken::new(),
                        )
                        .await
                };
                if ready && action == "search_docs" {
                    assert!(result.is_err(), "no semantic provider remains a hard error");
                    continue;
                }
                let body = result.unwrap().structured_content.unwrap();
                let targets = body["indexing"]["targets"].as_array().unwrap();
                assert_eq!(targets.len(), 1);
                assert_eq!(targets[0]["kind"], "reference");
                assert_eq!(targets[0]["state"], if ready { "ready" } else { "running" });
            }
        }
    }
}

async fn dispatch(server: &McpServer, profile: McpProfile, action: &str, query: &str) -> Value {
    let params = json!({"action": action, "query": query});
    let response = if profile == McpProfile::Workspace {
        server
            .workspace_search(
                Parameters(serde_json::from_value(params).unwrap()),
                CancellationToken::new(),
            )
            .await
    } else {
        server
            .reference_search(
                Parameters(serde_json::from_value(params).unwrap()),
                CancellationToken::new(),
            )
            .await
    }
    .unwrap();
    let body = response.structured_content.expect("dispatcher publishes structured response");
    let tool = McpServer::profile_router(profile)
        .list_all()
        .into_iter()
        .find(|tool| tool.name == "search")
        .unwrap();
    let schema = Value::Object(tool.output_schema.unwrap().as_ref().clone());
    let validator = jsonschema::validator_for(&schema).unwrap();
    assert!(validator.is_valid(&body), "actual dispatcher output must match tools/list: {body}");
    let mut missing = body.clone();
    missing.as_object_mut().unwrap().remove("indexing");
    assert!(!validator.is_valid(&missing), "covered response cannot bypass indexing");
    body
}

fn assert_scope(body: &Value, expected: &[&str]) {
    let indexing = &body["indexing"];
    assert_eq!(indexing["schema_version"], "1");
    let targets = indexing["targets"].as_array().unwrap();
    assert_eq!(targets.len(), expected.len());
    for (target, kind) in targets.iter().zip(expected) {
        assert_eq!(target["kind"], *kind);
        for key in ["kind", "state", "phase", "progress", "pass_id", "reason_code"] {
            assert!(target.get(key).is_some(), "missing {key}: {body}");
        }
        assert_eq!(target.as_object().unwrap().len(), 6);
    }
}

#[tokio::test]
async fn indexing_workspace_dispatch_warming_and_degraded_hits_and_empty() {
    let mut warming = SharedState::shared();
    warming.workspace_search_mode = WorkspaceSearchMode::PostgresRemoteOverlay;
    warming.baseline = crate::baseline::DeferredBaselineRuntime::pending_for_test();
    let server = McpServer::new(McpProfile::Workspace, warming);
    let body = dispatch(&server, McpProfile::Workspace, "search_code", "needle").await;
    assert_eq!(body["status"], "not_ready");
    assert!(body["detail"].as_str().unwrap().contains("PostgreSQL"));
    assert_scope(&body, &["lexical", "semantic"]);

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("Module.bsl"), "Процедура Needle()\nКонецПроцедуры").unwrap();
    let state = SharedState::shared();
    let mut engine = SearchEngine::fts_only(&dir.path().join("search.db")).unwrap();
    engine.index_directory_fts(dir.path()).unwrap();
    engine.set_workspace_root(dir.path());
    engine.initialize_workspace_overlay_clean().unwrap();
    *state.search_engine.lock().unwrap() = Some(engine);
    let server = McpServer::new(McpProfile::Workspace, state.clone());
    for runtime in [
        SemanticRuntimeStatus::Indexing,
        SemanticRuntimeStatus::Failed("fixture failure".into()),
        SemanticRuntimeStatus::Disabled,
    ] {
        *state.semantic_runtime.lock().unwrap() = runtime;
        for (query, expected_hits) in [("Needle", true), ("AbsentTerm", false)] {
            let body = dispatch(&server, McpProfile::Workspace, "search_code", query).await;
            assert_scope(&body, &["lexical", "semantic"]);
            assert_eq!(
                !body["hits"]
                    .as_array()
                    .unwrap_or_else(|| panic!("missing hits: {body}"))
                    .is_empty(),
                expected_hits,
                "{body}"
            );
            assert!(body["degraded"].is_string(), "fallback preserves degradation: {body}");
        }
    }
    let mut pass = state.index_progress.begin_pass();
    pass.finish(IndexPassState::Superseded);
    *state.semantic_runtime.lock().unwrap() = SemanticRuntimeStatus::Ready;
    let body = dispatch(&server, McpProfile::Workspace, "search_code", "Needle").await;
    assert_scope(&body, &["lexical", "semantic"]);
    assert_eq!(body["indexing"]["targets"][1]["state"], "superseded");
}

fn reference_actor(
    hit: bool,
    fallback: bool,
    calls: Arc<std::sync::atomic::AtomicUsize>,
) -> Arc<crate::baseline::ExternalBaselineService> {
    use crate::baseline::{
        BaselineRequestKind, BaselineSnapshotDocuments, ExternalBaselineService,
    };
    use bsl_search::{BaselineRef, CorpusId, IndexedDocument, LexicalHit, SemanticHit, Snapshot};
    ExternalBaselineService::with_worker_for_test(move |kind| {
        match kind {
            BaselineRequestKind::ResolveSnapshot { reply } => {
                let _ = reply.send(Ok(Some((
                    BaselineRef::for_snapshot(CorpusId::Reference, "reference-fixture"),
                    Snapshot::new("reference-fixture", CorpusId::Reference),
                ))));
            }
            BaselineRequestKind::LexicalSearch { reply, .. } => {
                calls.fetch_or(1, Ordering::SeqCst);
                let answer = if fallback {
                    Err(bsl_search::SearchError::Io(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "fixture direct-search failure",
                    )))
                } else {
                    Ok(if hit {
                        vec![LexicalHit {
                            source_span: None,
                            collection: "platform".into(),
                            root_id: "".into(),
                            path: "platform://fixture".into(),
                            symbol_name: "Needle".into(),
                            kind: "method".into(),
                            line_start: 1,
                            line_end: 1,
                            text: "Needle documentation".into(),
                            rank: 1.0,
                        }]
                    } else {
                        vec![]
                    })
                };
                let _ = reply.send(answer);
            }
            BaselineRequestKind::SemanticSearch { reply, .. } => {
                calls.fetch_or(2, Ordering::SeqCst);
                let answer = if fallback {
                    Err(bsl_search::SearchError::Io(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "fixture direct-search failure",
                    )))
                } else {
                    Ok(if hit {
                        vec![SemanticHit {
                            source_span: None,
                            collection: "platform".into(),
                            root_id: "".into(),
                            path: "platform://fixture".into(),
                            symbol_name: "Needle".into(),
                            kind: "method".into(),
                            line_start: 1,
                            line_end: 1,
                            score: 1.0,
                        }]
                    } else {
                        vec![]
                    })
                };
                let _ = reply.send(answer);
            }
            BaselineRequestKind::LoadReferenceSnapshotDocuments { reply, .. } => {
                calls.fetch_or(4, Ordering::SeqCst);
                let documents = if hit {
                    vec![IndexedDocument {
                        collection: "platform".into(),
                        root_id: "".into(),
                        path: "platform://fixture".into(),
                        symbol_name: "Needle".into(),
                        kind: "method".into(),
                        line_start: 1,
                        line_end: 1,
                        text: "Needle documentation".into(),
                        content_hash: "fixture".into(),
                        graph_context: None,
                        source_span: None,
                    }]
                } else {
                    vec![]
                };
                let _ = reply.send(Ok(Some(BaselineSnapshotDocuments {
                    snapshot_id: "reference-fixture".into(),
                    fingerprint: None,
                    documents,
                    shared_embeddings: Default::default(),
                })));
            }
            BaselineRequestKind::EmbeddingIdentity { reply } => {
                let _ = reply.send(Ok(Some(("test-model".into(), 3))));
            }
            BaselineRequestKind::LoadBaselineManifest { reply, .. } => {
                let _ = reply
                    .send(Err(bsl_search::SearchError::Index("unused fixture request".into())));
            }
            BaselineRequestKind::Shutdown { reply } => {
                let _ = reply.send(());
                return std::ops::ControlFlow::Break(());
            }
        }
        std::ops::ControlFlow::Continue(())
    })
}

#[tokio::test]
async fn indexing_reference_dispatch_profiles_local_remote_fallback_hits_and_empty() {
    use crate::baseline::{BaselineRuntime, ConfiguredBaselineStatus, DeferredBaselineRuntime};
    let dir = tempfile::tempdir().unwrap();
    let url = super::test_support::spawn_mock_embedding_server(vec![1.0, 0.0, 0.0]);
    for profile in [McpProfile::Workspace, McpProfile::Reference] {
        for mode in ["local", "remote", "fallback"] {
            for hit in [false, true] {
                let state = SharedState::shared();
                let path = dir.path().join(format!("{}-{mode}-{hit}.db", profile.as_str()));
                let mut engine =
                    SearchEngine::new(&path, super::test_support::mock_semantic_config(&url))
                        .unwrap();
                if hit {
                    engine
                        .index_documents(
                            "platform",
                            "platform://fixture",
                            b"fixture",
                            &[bsl_search::Document {
                                title: "Needle".into(),
                                body: "Needle documentation".into(),
                                kind: "method".into(),
                            }],
                            None,
                        )
                        .unwrap();
                }
                *state.reference_search.engine.lock().unwrap() = Some(engine);
                *state.reference_search.lifecycle.lock().unwrap() = ReferenceSearchLifecycle::Ready;
                let mut state = state;
                let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
                if mode != "local" {
                    state.reference_search.baseline =
                        DeferredBaselineRuntime::ready(BaselineRuntime {
                            configured_baseline: ConfiguredBaselineStatus {
                                backend: "postgres",
                                selection: "fixture".into(),
                                issue: None,
                                support: None,
                            },
                            external_baseline: Some(reference_actor(
                                hit,
                                mode == "fallback",
                                Arc::clone(&calls),
                            )),
                        });
                }
                if profile == McpProfile::Reference {
                    // Mirror SharedState's production reference-profile constructor aliases.
                    state.search_engine = Arc::clone(&state.reference_search.engine);
                    state.baseline = state.reference_search.baseline.clone();
                    state.index_progress = Arc::clone(&state.reference_search.progress);
                    state.semantic_runtime = Arc::clone(&state.reference_search.semantic_runtime);
                }
                let server = McpServer::new(profile, state);
                for action in ["find_docs", "search_docs"] {
                    let body = dispatch(&server, profile, action, "Needle").await;
                    assert_scope(&body, &["reference"]);
                    assert_eq!(body["indexing"]["targets"][0]["state"], "ready");
                    assert_eq!(
                        !body["hits"]
                            .as_array()
                            .unwrap_or_else(|| panic!("missing hits: {body}"))
                            .is_empty(),
                        hit,
                        "{mode}/{action}: {body}"
                    );
                }
                assert_eq!(
                    calls.load(Ordering::SeqCst),
                    match mode {
                        "local" => 0,
                        "remote" => 3,
                        _ => 7,
                    },
                    "the requested serving branch must actually execute"
                );
            }
        }
    }
}

#[tokio::test]
async fn indexing_workspace_dispatch_qualified_semantic_hits_and_empty() {
    let dir = tempfile::tempdir().unwrap();
    let url = super::test_support::spawn_mock_embedding_server(vec![1.0, 0.0, 0.0]);
    for populated in [false, true] {
        let root = dir.path().join(format!("workspace-{populated}"));
        std::fs::create_dir(&root).unwrap();
        if populated {
            std::fs::write(root.join("Module.bsl"), "Процедура Needle()\nКонецПроцедуры").unwrap();
        }
        let mut engine = SearchEngine::new(
            &root.join("search.db"),
            super::test_support::mock_semantic_config(&url),
        )
        .unwrap();
        engine.index_directory(&root, None).unwrap();
        engine.set_workspace_root(&root);
        engine.initialize_workspace_overlay_clean().unwrap();
        engine.observe_semantic_boot_coverage(
            engine.chunk_count().ok(),
            engine.embedding_count_by_collection("code").ok(),
        );
        let state = SharedState::shared();
        *state.search_engine.lock().unwrap() = Some(engine);
        *state.semantic_runtime.lock().unwrap() = SemanticRuntimeStatus::Ready;
        let server = McpServer::new(McpProfile::Workspace, state);
        for action in ["search_code", "status"] {
            let body = dispatch(&server, McpProfile::Workspace, action, "Needle").await;
            assert_scope(&body, &["lexical", "semantic"]);
            assert_eq!(body["indexing"]["targets"][1]["state"], "ready", "{body}");
            if action == "search_code" {
                assert_eq!(!body["hits"].as_array().unwrap().is_empty(), populated);
                assert!(body.get("degraded").is_none() || body["degraded"].is_null());
            }
        }
    }
}

#[test]
fn indexing_remote_qualification_combines_publication_and_overlay() {
    use crate::baseline::{
        BaselineRuntime, BaselineSemanticDetails, ConfiguredBaselineStatus,
        DeferredBaselineRuntime, ExternalBaselineState, ExternalBaselineStatus,
    };
    let dir = tempfile::tempdir().unwrap();
    let mut engine = SearchEngine::new(
        &dir.path().join("search.db"),
        super::test_support::mock_semantic_config("http://127.0.0.1:0"),
    )
    .unwrap();
    engine.set_workspace_root(dir.path());
    engine.set_serves_external_baseline(true).unwrap();
    engine
        .store()
        .save_baseline_manifest(&bsl_search::WorkspaceBaselineManifest {
            snapshot_id: "qualified".into(),
            snapshot_fingerprint: Some("fingerprint".into()),
            files: vec![],
        })
        .unwrap();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let baseline = reference_actor(false, false, Arc::clone(&calls));
    let mut state = SharedState::shared();
    state.workspace_search_mode = WorkspaceSearchMode::PostgresRemoteOverlay;
    *state.search_engine.lock().unwrap() = Some(engine);
    *state.semantic_runtime.lock().unwrap() = SemanticRuntimeStatus::Ready;
    state.baseline = DeferredBaselineRuntime::ready(BaselineRuntime {
        configured_baseline: ConfiguredBaselineStatus {
            backend: "postgres",
            selection: "fixture".into(),
            issue: None,
            support: None,
        },
        external_baseline: Some(Arc::clone(&baseline)),
    });
    let qualified = ExternalBaselineStatus {
        backend: "postgres",
        schema: "fixture".into(),
        selection: "fixture".into(),
        resolved: Some("qualified".into()),
        state: ExternalBaselineState::Ready {
            snapshot_id: "qualified".into(),
            fingerprint: Some("fingerprint".into()),
            documents: 0,
            files: 0,
        },
        semantic_details: Some(BaselineSemanticDetails {
            snapshot_id: "qualified".into(),
            fingerprint: Some("fingerprint".into()),
            publication: Some(bsl_search::BaselineSemanticPublication {
                model_id: "test-model".into(),
                dimension: 3,
                complete: true,
            }),
        }),
    };
    baseline.seed_status_cache_for_test(qualified.clone(), std::time::Duration::ZERO);
    *state.overlay_warmup.lock().unwrap() = OverlayWarmupState::NoLocalDiffs;
    assert_eq!(
        targets(&state)[1]["reason_code"],
        "overlay_pending",
        "warmup verdict alone cannot initialize overlay evidence"
    );
    state
        .search_engine
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .initialize_workspace_overlay_clean()
        .unwrap();

    for warmup in [
        OverlayWarmupState::NoLocalDiffs,
        OverlayWarmupState::Synced { overlay_files: 0, embedded: 0 },
    ] {
        *state.overlay_warmup.lock().unwrap() = warmup;
        assert_eq!(
            targets(&state)[1]["state"],
            "ready",
            "publisher-qualified empty baseline is valid"
        );
    }
    for (snapshot_id, fingerprint) in [("next", "fingerprint"), ("next", "changed")] {
        let mut changed = qualified.clone();
        if let ExternalBaselineState::Ready { snapshot_id: id, fingerprint: fp, .. } =
            &mut changed.state
        {
            *id = snapshot_id.into();
            *fp = Some(fingerprint.into());
        }
        let details = changed.semantic_details.as_mut().unwrap();
        details.snapshot_id = snapshot_id.into();
        details.fingerprint = Some(fingerprint.into());
        baseline.seed_status_cache_for_test(changed, std::time::Duration::ZERO);
        for target in targets(&state).as_array().unwrap() {
            assert_ne!(
                target["state"], "ready",
                "new baseline cannot qualify the old overlay: {target}"
            );
        }
        {
            let guard = state.search_engine.lock().unwrap();
            let engine = guard.as_ref().unwrap();
            engine
                .store()
                .save_baseline_manifest(&bsl_search::WorkspaceBaselineManifest {
                    snapshot_id: snapshot_id.into(),
                    snapshot_fingerprint: Some(fingerprint.into()),
                    files: vec![],
                })
                .unwrap();
            engine.initialize_workspace_overlay_clean().unwrap();
        }
        assert_eq!(targets(&state)[0]["state"], "ready");
        assert_eq!(targets(&state)[1]["state"], "ready");
    }
    baseline.seed_status_cache_for_test(qualified.clone(), std::time::Duration::ZERO);
    state
        .search_engine
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .store()
        .save_baseline_manifest(&bsl_search::WorkspaceBaselineManifest {
            snapshot_id: "qualified".into(),
            snapshot_fingerprint: Some("fingerprint".into()),
            files: vec![],
        })
        .unwrap();
    state
        .search_engine
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .initialize_workspace_overlay_clean()
        .unwrap();
    state
        .search_engine
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .mark_workspace_key_dirty(bsl_search::FileKey::configuration("Pending.bsl"))
        .unwrap();
    assert_eq!(targets(&state)[1]["state"], "waiting");
    assert_eq!(targets(&state)[1]["reason_code"], "overlay_pending");
    assert_eq!(targets(&state)[0]["state"], "waiting");
    assert_eq!(
        targets(&state)[0]["reason_code"],
        "overlay_pending",
        "a present baseline with overlay debt is pending overlay work, not an absent baseline"
    );
    *state.overlay_warmup.lock().unwrap() = OverlayWarmupState::Failed("private diagnostic".into());
    assert_eq!(targets(&state)[1]["state"], "failed", "known failure wins over dirty overlay");
    assert_eq!(targets(&state)[1]["reason_code"], "native_failure");
    *state.overlay_warmup.lock().unwrap() = OverlayWarmupState::EmbeddingFailed(
        bsl_search::EmbeddingFailure::new(bsl_search::EmbeddingFailureCode::EmbeddingTimeout),
    );
    assert_eq!(targets(&state)[1]["state"], "failed", "a typed overlay failure is not pending");
    assert_eq!(targets(&state)[1]["reason_code"], "native_failure");
    *state.overlay_warmup.lock().unwrap() = OverlayWarmupState::Superseded;
    assert_eq!(targets(&state)[1]["state"], "superseded");
    *state.overlay_warmup.lock().unwrap() = OverlayWarmupState::NoLocalDiffs;
    state
        .search_engine
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .initialize_workspace_overlay_clean()
        .unwrap();
    *state.overlay_warmup.lock().unwrap() = OverlayWarmupState::Pending;
    assert_eq!(targets(&state)[1]["reason_code"], "overlay_pending");
    *state.overlay_warmup.lock().unwrap() = OverlayWarmupState::NoLocalDiffs;
    baseline.seed_status_cache_for_test(qualified.clone(), std::time::Duration::from_secs(60));
    assert_eq!(targets(&state)[1]["state"], "unknown");
    assert_eq!(targets(&state)[1]["reason_code"], "stale_generation");
    assert_eq!(
        targets(&state)[0]["state"],
        "unknown",
        "expired evidence is not an absent baseline"
    );
    assert_eq!(targets(&state)[0]["reason_code"], "stale_generation");
    let mut unverified = qualified.clone();
    unverified.semantic_details.as_mut().unwrap().publication.as_mut().unwrap().complete = false;
    baseline.seed_status_cache_for_test(unverified, std::time::Duration::ZERO);
    assert_eq!(targets(&state)[1]["reason_code"], "coverage_unverified");
    let mut mismatch = qualified;
    mismatch.semantic_details.as_mut().unwrap().publication.as_mut().unwrap().dimension = 8;
    baseline.seed_status_cache_for_test(mismatch, std::time::Duration::ZERO);
    assert_eq!(targets(&state)[1]["reason_code"], "identity_unverified");
    assert_eq!(calls.load(Ordering::SeqCst), 0, "qualification does not submit actor work");
    assert!(!baseline.status_probe_refreshing_for_test(), "qualification does not start a probe");
}

#[tokio::test]
async fn indexing_query_provider_failure_preserves_qualified_index() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("search.db");
    let url = super::test_support::spawn_mock_embedding_server(vec![1.0, 0.0, 0.0]);
    std::fs::write(dir.path().join("Module.bsl"), "Процедура Needle()\nКонецПроцедуры").unwrap();
    let mut engine =
        SearchEngine::new(&path, super::test_support::mock_semantic_config(&url)).unwrap();
    engine.index_directory(dir.path(), None).unwrap();
    drop(engine);
    let mut engine =
        SearchEngine::new(&path, super::test_support::mock_semantic_config("http://127.0.0.1:0"))
            .unwrap();
    engine.set_workspace_root(dir.path());
    engine.initialize_workspace_overlay_clean().unwrap();
    engine.observe_semantic_boot_coverage(
        engine.chunk_count().ok(),
        engine.embedding_count_by_collection("code").ok(),
    );
    let state = SharedState::shared();
    *state.search_engine.lock().unwrap() = Some(engine);
    *state.semantic_runtime.lock().unwrap() = SemanticRuntimeStatus::Ready;
    let server = McpServer::new(McpProfile::Workspace, state);
    let body = dispatch(&server, McpProfile::Workspace, "search_code", "Needle").await;
    assert_scope(&body, &["lexical", "semantic"]);
    assert!(!body["hits"].as_array().unwrap().is_empty());
    assert!(body["degraded"].is_string());
    assert_eq!(body["indexing"]["targets"][1]["state"], "ready");
}

#[test]
fn indexing_workspace_responses_defensive_retry() {
    let state = SharedState::shared();
    *state.semantic_runtime.lock().unwrap() = SemanticRuntimeStatus::Indexing;
    let pass = state.index_progress.begin_pass();
    pass.token().set_totals(1, 8, 2);
    pass.token().advance(4, 1);
    // search_call currently never emits Superseded; retain proof for the shared defensive funnel.
    let retry = crate::cancellable_answer(
        crate::diagnostics_state::CallOutcome::Superseded,
        "search",
        std::time::Instant::now(),
        || crate::tools::search::search_not_ready("retry", &state.index_progress, "search_code"),
    )
    .unwrap();
    let response =
        crate::finish_indexed_search(retry, state.workspace_indexing(), Some(6000), false).unwrap();
    let body = response.structured_content.unwrap();
    assert_scope(&body, &["lexical", "semantic"]);
    assert_eq!(body["status"], "not_ready");
    assert_eq!(
        body["progress"]["chunks"]["done"],
        body["indexing"]["targets"][1]["progress"]["completed"]
    );
    assert_eq!(body["progress"]["pct"], 50);
}
