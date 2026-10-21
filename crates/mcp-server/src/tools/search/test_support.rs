use crate::baseline::{
    BaselineRequestKind, ExternalBaselineService, RefreshableExternalBaselineSource,
};
use bsl_search::{BaselineRef, CorpusId, LexicalHit, SearchHit, SemanticHit, Snapshot};
use project_model::{SearchPostgresConfig, SearchPostgresCredentialHelperConfig};
use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};

/// The name a scripted actor records for each request kind it serves.
fn request_name(kind: &BaselineRequestKind) -> &'static str {
    match kind {
        BaselineRequestKind::ResolveSnapshot { .. } => "resolve_snapshot",
        BaselineRequestKind::LexicalSearch { .. } => "lexical",
        BaselineRequestKind::SemanticSearch { .. } => "semantic",
        BaselineRequestKind::LoadReferenceSnapshotDocuments { .. } => "load_reference_documents",
        BaselineRequestKind::LoadBaselineManifest { .. } => "load_manifest",
        BaselineRequestKind::EmbeddingIdentity { .. } => "embedding_identity",
        BaselineRequestKind::Shutdown { .. } => "shutdown",
    }
}

/// The test's hold over a scripted actor: which requests it has begun, which it has
/// finished, and the gate that lets a latched one proceed.
pub(super) struct Latch {
    started: AtomicUsize,
    executed: Mutex<Vec<&'static str>>,
    release: Mutex<mpsc::Sender<()>>,
}

impl Latch {
    /// How many requests the worker has taken off the queue and begun.
    pub(super) fn started(&self) -> usize {
        self.started.load(Ordering::SeqCst)
    }

    /// The requests the worker has answered, in order.
    pub(super) fn executed(&self) -> Vec<&'static str> {
        self.executed.lock().unwrap().clone()
    }

    /// Let one latched request proceed.
    pub(super) fn release_one(&self) {
        self.release.lock().unwrap().send(()).expect("the worker is alive");
    }

    /// Block until the worker has begun at least `count` requests.
    pub(super) fn wait_started(&self, count: usize) {
        crate::test_wait::wait_until("the worker begins its request", || self.started() >= count);
    }
}

/// A baseline service whose actor answers every request with an empty success at once,
/// except the kinds named in `latched`, which it holds until [`Latch::release_one`]. The
/// queue discipline is the production one (see `ExternalBaselineService::serve`).
pub(super) fn latched_service(
    latched: &[&'static str],
) -> (Arc<ExternalBaselineService>, Arc<Latch>) {
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let latch = Arc::new(Latch {
        started: AtomicUsize::new(0),
        executed: Mutex::new(Vec::new()),
        release: Mutex::new(release_tx),
    });
    let latched: HashSet<&'static str> = latched.iter().copied().collect();
    let worker_latch = Arc::clone(&latch);
    let service = ExternalBaselineService::with_worker_for_test(move |kind| {
        let name = request_name(&kind);
        worker_latch.started.fetch_add(1, Ordering::SeqCst);
        if latched.contains(name) {
            let _ = release_rx
                .recv_timeout(test_utils::wait::hang_guard(std::time::Duration::from_secs(60)));
        }
        worker_latch.executed.lock().unwrap().push(name);
        match kind {
            BaselineRequestKind::ResolveSnapshot { reply } => {
                let _ = reply.send(Ok(Some((
                    BaselineRef::for_snapshot(CorpusId::WorkspaceCode, "snap-1"),
                    Snapshot::new("snap-1", CorpusId::WorkspaceCode),
                ))));
            }
            BaselineRequestKind::LexicalSearch { reply, .. } => {
                let _ = reply.send(Ok(vec![]));
            }
            BaselineRequestKind::SemanticSearch { reply, .. } => {
                let _ = reply.send(Ok(vec![]));
            }
            BaselineRequestKind::LoadReferenceSnapshotDocuments { reply, .. } => {
                let _ = reply.send(Ok(None));
            }
            BaselineRequestKind::LoadBaselineManifest { reply, .. } => {
                let _ = reply.send(Err(bsl_search::SearchError::Index("no manifest".to_owned())));
            }
            BaselineRequestKind::EmbeddingIdentity { reply } => {
                let _ = reply.send(Ok(None));
            }
            BaselineRequestKind::Shutdown { reply } => {
                let _ = reply.send(());
                return std::ops::ControlFlow::Break(());
            }
        }
        std::ops::ControlFlow::Continue(())
    });
    (service, latch)
}

pub(super) fn retryable_postgres_source() -> Arc<ExternalBaselineService> {
    let postgres = SearchPostgresConfig {
        host: Some("127.0.0.1".to_owned()),
        port: Some(1),
        dbname: Some("bsl_search".to_owned()),
        schema: Some("bsl_search".to_owned()),
        vault_role_base: Some("prod/search/bsl-analyzer".to_owned()),
        credential_helper: SearchPostgresCredentialHelperConfig {
            program: Some("sh".to_owned()),
            args: vec![
                "-c".to_owned(),
                "cat >/dev/null; printf '%s\\n' '{\"protocol\":\"bsl-analyzer.postgres-helper.v1\",\"ok\":true,\"url\":\"postgres://127.0.0.1:1/bsl_search\"}'".to_owned(),
            ],
        },
    };
    ExternalBaselineService::for_test(
        RefreshableExternalBaselineSource::for_test_with_refresh_context(
            bsl_search::ExternalBaselineConfig::postgres("postgres://127.0.0.1:1/bsl_search"),
            BaselineRef {
                corpus: CorpusId::WorkspaceCode,
                snapshot_id: None,
                branch: Some("main".to_owned()),
                commit: None,
            },
            postgres,
        )
        .unwrap(),
    )
}

pub(super) fn unreachable_workspace_service() -> Arc<ExternalBaselineService> {
    ExternalBaselineService::for_test(
        RefreshableExternalBaselineSource::for_test(
            bsl_search::ExternalBaselineConfig::postgres("postgres://127.0.0.1:1"),
            BaselineRef {
                corpus: CorpusId::WorkspaceCode,
                snapshot_id: None,
                branch: Some("main".to_owned()),
                commit: None,
            },
        )
        .unwrap(),
    )
}

pub(super) fn lexical_hit(path: &str, symbol_name: &str, rank: f32) -> LexicalHit {
    LexicalHit {
        source_span: None,
        collection: "code".to_owned(),
        root_id: bsl_search::CONFIGURATION_ROOT_ID.to_owned(),
        path: path.to_owned(),
        symbol_name: symbol_name.to_owned(),
        kind: "procedure".to_owned(),
        line_start: 1,
        line_end: 10,
        text: format!("procedure {symbol_name}"),
        rank,
    }
}

pub(super) fn semantic_hit(path: &str, symbol_name: &str, score: f32) -> SemanticHit {
    SemanticHit {
        source_span: None,
        collection: "code".to_owned(),
        root_id: bsl_search::CONFIGURATION_ROOT_ID.to_owned(),
        path: path.to_owned(),
        symbol_name: symbol_name.to_owned(),
        kind: "procedure".to_owned(),
        line_start: 1,
        line_end: 10,
        score,
    }
}

pub(super) fn code_hit(file_path: &str, symbol: &str, kind: &str) -> SearchHit {
    SearchHit {
        source_span: None,
        collection: "code".to_owned(),
        root_id: bsl_search::CONFIGURATION_ROOT_ID.to_owned(),
        file_path: file_path.to_owned(),
        symbol_name: symbol.to_owned(),
        kind: kind.to_owned(),
        text: String::new(),
        line_start: 0,
        line_end: 1,
        score: 1.0,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        code_hit, lexical_hit, retryable_postgres_source, semantic_hit,
        unreachable_workspace_service,
    };

    #[test]
    fn fixtures_construct_synthetic_hits_and_baseline_services() {
        assert_eq!(lexical_hit("a.bsl", "A", 1.0).collection, "code");
        assert_eq!(semantic_hit("a.bsl", "A", 1.0).collection, "code");
        assert_eq!(code_hit("a.bsl", "A", "procedure").collection, "code");
        let _ = retryable_postgres_source();
        let _ = unreachable_workspace_service();
    }
}
