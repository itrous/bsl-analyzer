use crate::document::Document;
use crate::embedder::{Embedder, EmbedderConfig};
use crate::error::SearchError;
use crate::index::VectorIndex;
use crate::lifecycle::{self, Batch, Outcome, Reason};
use crate::local_baseline::LocalStoreBaselineAdapter;
use crate::ports::{ModuleSnapshotSource, SnapshotCatalog, SnapshotContentStore};
use crate::publish::EmbeddingExecutionPolicy;
use crate::resolver::{InMemoryResolvedViewResolver, ResolvedView, ResolvedViewCounts};
use crate::store::{
    ContextRefreshMutation, Store, WorkspaceDriftStoreOutcome, WorkspaceStoreTransition,
    WorkspaceTransitionFile,
};
use crate::workspace_overlay::{
    lexical_hits, normalized_file_hash_for_indexed_documents, semantic_hits, BaselineHashMode,
    OverlayPublicationStaging, PublicationBaseline, PublishOutcome, RefreshPlan,
    WorkspaceOverlayCache, WorkspaceOverlayIndex, WorkspaceOverlayStats,
    WorkspaceTransitionOverlayFile,
};
use crate::workspace_roots::{FileKey, WorkspaceRoots, CONFIGURATION_ROOT_ID};
use crate::{
    semantic_key_for_indexed_document, semantic_text_for_indexed_document,
    BaselineOverlaySearchService, BaselineRef, CorpusId,
};
use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use tracing::{debug, info, warn};

#[cfg(test)]
std::thread_local! {
    static CONSTRUCTOR_APPLY_ACTIVE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub use crate::progress::{ActivePass, IndexProgress};
use crate::progress::{IndexPassState, IndexPassToken, IndexPhase};

#[derive(Default)]
pub struct SearchConfig {
    pub embedder: EmbedderConfig,
    pub execution: EmbeddingExecutionPolicy,
}

#[derive(Debug, Clone)]
pub struct SearchHit {
    pub collection: String,
    /// The source root the file belongs to; see [`crate::DocumentPath::root_id`].
    pub root_id: String,
    pub file_path: String,
    pub symbol_name: String,
    pub kind: String,
    pub text: String,
    pub line_start: u32,
    pub line_end: u32,
    pub score: f32,
    pub source_span: Option<crate::SourceSpan>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReferenceCollectionReplaceOutcome {
    pub committed_fingerprint: String,
    pub written: bool,
    pub embedding_failure: Option<crate::EmbeddingFailure>,
}

/// Host-neutral outcome of admitting one shared-store mutation through an ownership fence.
#[derive(Debug, PartialEq, Eq)]
pub enum FenceOutcome<T> {
    Applied(T),
    TransientRefusal,
    Superseded,
    Released,
}

#[doc(hidden)]
pub const WORKSPACE_APPLY_BATCH_ROWS: usize = 64;

impl SearchHit {
    pub fn from_lexical(hit: &crate::domain::LexicalHit) -> Self {
        Self {
            collection: hit.collection.clone(),
            root_id: hit.root_id.clone(),
            file_path: hit.path.clone(),
            symbol_name: hit.symbol_name.clone(),
            kind: hit.kind.clone(),
            text: hit.text.clone(),
            line_start: hit.line_start,
            line_end: hit.line_end,
            score: hit.rank,
            source_span: hit.source_span.clone(),
        }
    }

    pub fn to_lexical(&self) -> crate::domain::LexicalHit {
        crate::domain::LexicalHit {
            collection: self.collection.clone(),
            root_id: self.root_id.clone(),
            path: self.file_path.clone(),
            symbol_name: self.symbol_name.clone(),
            kind: self.kind.clone(),
            line_start: self.line_start,
            line_end: self.line_end,
            text: self.text.clone(),
            rank: self.score,
            source_span: self.source_span.clone(),
        }
    }

    pub fn to_semantic(&self) -> crate::domain::SemanticHit {
        crate::domain::SemanticHit {
            collection: self.collection.clone(),
            root_id: self.root_id.clone(),
            path: self.file_path.clone(),
            symbol_name: self.symbol_name.clone(),
            kind: self.kind.clone(),
            line_start: self.line_start,
            line_end: self.line_end,
            score: self.score,
            source_span: self.source_span.clone(),
        }
    }

    pub fn from_merged(hit: crate::merge::MergedHit) -> Self {
        Self {
            collection: hit.collection,
            root_id: hit.root_id,
            file_path: hit.path,
            symbol_name: hit.symbol_name,
            kind: hit.kind,
            text: hit.text.unwrap_or_default(),
            line_start: hit.line_start,
            line_end: hit.line_end,
            score: hit.score,
            source_span: hit.source_span,
        }
    }
}

// Test seam: force the vector eviction of a removal to count as failed, so a test can assert
// that a removal whose vectors stayed in the live index is not reported as a success — and
// that the store row it is selected by outlives the failure. The live index rejects nothing
// on its own: removing an id it does not hold is a no-op there.
#[cfg(test)]
thread_local! {
    // Thread-local on purpose: tests run in parallel, and a process-wide flag would fail
    // removals in whichever unrelated test happened to be running at the time.
    static FORCE_VECTOR_REMOVE_ERROR: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Cheap state captured under the live engine lock before a root-transition plan scans disk.
pub struct WorkspaceRootsTransitionSeed {
    epoch: u64,
    overlay_epoch: u64,
    old_roots: WorkspaceRoots,
    next_roots: WorkspaceRoots,
    serves_external_baseline: bool,
    manifest: HashMap<FileKey, String>,
    graph_context_provider: Option<Arc<dyn crate::ports::GraphContextProvider>>,
    embedder: Option<Embedder>,
}

#[derive(Clone, PartialEq, Eq)]
struct WorkspaceTransitionFileIdentity {
    abs_path: PathBuf,
    canonical: PathBuf,
    len: u64,
    modified: Option<std::time::SystemTime>,
    read: WorkspaceTransitionReadState,
}

#[derive(Clone, PartialEq, Eq)]
enum WorkspaceTransitionReadState {
    Readable(Vec<u8>),
    InvalidUtf8(Vec<u8>),
    ReadFailed,
}

struct PlannedWorkspaceFile {
    key: FileKey,
    identity: WorkspaceTransitionFileIdentity,
    content_hash: Vec<u8>,
    chunks: Vec<crate::Chunk>,
    graph_contexts: Vec<Option<String>>,
    documents: Vec<crate::IndexedDocument>,
    embedding_inputs: Vec<String>,
    source_spans: Vec<Option<crate::SourceSpan>>,
    manifest_fingerprint: String,
}

struct PlannedUnreadWorkspaceFile {
    key: FileKey,
    identity: WorkspaceTransitionFileIdentity,
}

/// Off-lock result of scanning and preparing the complete next root universe.
pub struct WorkspaceRootsTransitionPlan {
    epoch: u64,
    overlay_epoch: u64,
    old_roots: WorkspaceRoots,
    next_roots: WorkspaceRoots,
    serves_external_baseline: bool,
    manifest: HashMap<FileKey, String>,
    files: Vec<PlannedWorkspaceFile>,
    unread_files: Vec<PlannedUnreadWorkspaceFile>,
}

/// A prepared transition whose filesystem snapshot was checked immediately before apply.
///
/// Construction is intentionally available only through
/// [`WorkspaceRootsTransitionPlan::revalidate`]. This separates the expensive second walk and
/// content reads from [`SearchEngine::apply_validated_workspace_roots_transition`], whose caller
/// may hold the live engine mutex.
pub struct ValidatedWorkspaceRootsTransitionPlan {
    plan: WorkspaceRootsTransitionPlan,
    staging: Option<WorkspaceRootsTransitionStaging>,
}

/// Phase-C value bundle retained across transient ownership refusals.
pub struct ValidatedWorkspaceOverlayPublication {
    staging: Option<OverlayPublicationStaging>,
    expected: PublicationBaseline,
    embedding_identity: Option<(String, usize)>,
}

struct WorkspaceRootsTransitionStaging {
    changed_root_ids: HashSet<String>,
    cleanup: HashSet<FileKey>,
    obsolete_baseline: HashSet<FileKey>,
    /// Every spelling the planned walk reached its files through, ready to be recorded beside
    /// the transition's rows: a root move re-attributes spellings, and the journal has to
    /// follow the world the transition published (github#192).
    spellings: Vec<(String, FileKey)>,
    upserts: Vec<WorkspaceTransitionFile>,
    /// The arguments of the in-place overlay transition, not a rebuilt overlay. Staging must not
    /// hold the engine lock while it scans, and a cache cloned before that scan is a photograph
    /// of a state the commit no longer finds: installing it would erase every mark, entry and
    /// hiding the window admitted. Carrying the arguments instead lets the commit apply the
    /// transition to whatever the live cache has become, so concurrent work is merged rather
    /// than overwritten — and no comparison has to enumerate what "concurrent work" can be.
    unread_present: HashSet<FileKey>,
    overlay_files: Vec<crate::workspace_overlay::WorkspaceTransitionOverlayFile>,
    next_index: VectorIndex,
    embedding_generation: i64,
    removed: usize,
    rebuilt: usize,
    added: usize,
    pending_collection_embeddings: bool,
    pending_overlay_embeddings: bool,
}

/// Observable result of applying a prepared root transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceRootsTransitionOutcome {
    Unchanged,
    Applied {
        removed: usize,
        rebuilt: usize,
        added: usize,
        pending_collection_embeddings: bool,
        pending_overlay_embeddings: bool,
    },
    /// The live roots or filesystem moved past the plan. Nothing was changed.
    Superseded,
}

impl WorkspaceRootsTransitionSeed {
    /// Render newly built documents with the graph artifact paired with the root snapshot.
    /// The live engine may still carry the previous publication's provider while this plan is
    /// prepared off-lock, so the orchestrator supplies the just-published provider explicitly.
    pub fn with_graph_context_provider(
        mut self,
        provider: Arc<dyn crate::ports::GraphContextProvider>,
    ) -> Self {
        self.graph_context_provider = Some(provider);
        self
    }

    /// Scan and prepare all lexical documents without borrowing the live engine.
    pub fn plan(self) -> Result<WorkspaceRootsTransitionPlan, SearchError> {
        let declared: Vec<PathBuf> =
            self.next_roots.entries().map(|(_, path)| path.to_path_buf()).collect();
        let set = project_model::SourceSet::scan_in_scope(
            &declared,
            self.next_roots.excluded(),
            self.next_roots.user_excluded(),
        );
        if !set.clean() {
            return Err(SearchError::Index(format!(
                "workspace root transition scan is incomplete: unreadable={}, canonical_fallbacks={}",
                set.unreadable, set.canonical_fallbacks
            )));
        }

        let mut seen = HashSet::new();
        let mut files = Vec::new();
        let mut unread_files = Vec::new();
        for file in &set.files {
            if file.role != project_model::FileRole::Source {
                continue;
            }
            let Some(key) = self.next_roots.root_of(&file.walked, &file.canonical) else {
                continue;
            };
            if !seen.insert(key.clone()) {
                continue;
            }
            let bytes = std::fs::read(&file.walked);
            let read = match &bytes {
                Ok(bytes) if std::str::from_utf8(bytes).is_ok() => {
                    WorkspaceTransitionReadState::Readable(blake3::hash(bytes).as_bytes().to_vec())
                }
                Ok(bytes) => WorkspaceTransitionReadState::InvalidUtf8(
                    blake3::hash(bytes).as_bytes().to_vec(),
                ),
                Err(_) => WorkspaceTransitionReadState::ReadFailed,
            };
            let identity = WorkspaceTransitionFileIdentity {
                abs_path: file.walked.clone(),
                canonical: file.canonical.clone(),
                len: file.metadata.len(),
                modified: file.metadata.modified().ok(),
                read,
            };
            let Ok(bytes) = bytes else {
                unread_files.push(PlannedUnreadWorkspaceFile { key, identity });
                continue;
            };
            let Ok(content) = std::str::from_utf8(&bytes) else {
                unread_files.push(PlannedUnreadWorkspaceFile { key, identity });
                continue;
            };
            let (documents, context_failed) = crate::document::prepare_indexed_documents(
                &key,
                content,
                self.graph_context_provider.as_deref(),
                self.embedder.as_ref(),
            )?;
            // A transition applied without some contexts would publish files that no mark
            // owes a render; the plan fails instead and its owner retries the transition.
            if context_failed {
                return Err(SearchError::Index(format!(
                    "graph context unavailable for {}; root transition deferred",
                    key.path
                )));
            }
            let (chunks, graph_contexts, source_spans) =
                crate::document::chunks_contexts_and_spans(&documents);
            let embedding_inputs =
                documents.iter().map(crate::document::semantic_text_for_indexed_document).collect();
            files.push(PlannedWorkspaceFile {
                key: key.clone(),
                identity,
                content_hash: blake3::hash(content.as_bytes()).as_bytes().to_vec(),
                chunks,
                graph_contexts,
                documents,
                embedding_inputs,
                source_spans,
                manifest_fingerprint: crate::workspace_overlay::fingerprint_content(
                    content, &key.path,
                ),
            });
        }

        Ok(WorkspaceRootsTransitionPlan {
            epoch: self.epoch,
            overlay_epoch: self.overlay_epoch,
            old_roots: self.old_roots,
            next_roots: self.next_roots,
            serves_external_baseline: self.serves_external_baseline,
            manifest: self.manifest,
            files,
            unread_files,
        })
    }
}

impl WorkspaceRootsTransitionPlan {
    /// Re-scan and re-read the planned universe without borrowing a live engine.
    ///
    /// `Ok(None)` means create/modify/delete/retarget or a read-state change moved the filesystem
    /// past the plan. An incomplete scan is an error so orchestration keeps the last-known-good
    /// roots and its retry obligation. A clean scan may still contain individually unread files;
    /// their identity and read state are compared without pretending their content was rebuilt.
    pub fn revalidate(self) -> Result<Option<ValidatedWorkspaceRootsTransitionPlan>, SearchError> {
        let declared: Vec<PathBuf> =
            self.next_roots.entries().map(|(_, path)| path.to_path_buf()).collect();
        let set = project_model::SourceSet::scan_in_scope(
            &declared,
            self.next_roots.excluded(),
            self.next_roots.user_excluded(),
        );
        if !set.clean() {
            return Err(SearchError::Index(format!(
                "workspace root transition validation scan is incomplete: unreadable={}, canonical_fallbacks={}",
                set.unreadable, set.canonical_fallbacks
            )));
        }
        let mut validation = HashMap::new();
        for file in &set.files {
            if file.role != project_model::FileRole::Source {
                continue;
            }
            let Some(key) = self.next_roots.root_of(&file.walked, &file.canonical) else {
                continue;
            };
            validation.entry(key).or_insert_with(|| {
                let read = match std::fs::read(&file.walked) {
                    Ok(bytes) if std::str::from_utf8(&bytes).is_ok() => {
                        WorkspaceTransitionReadState::Readable(
                            blake3::hash(&bytes).as_bytes().to_vec(),
                        )
                    }
                    Ok(bytes) => WorkspaceTransitionReadState::InvalidUtf8(
                        blake3::hash(&bytes).as_bytes().to_vec(),
                    ),
                    Err(_) => WorkspaceTransitionReadState::ReadFailed,
                };
                WorkspaceTransitionFileIdentity {
                    abs_path: file.walked.clone(),
                    canonical: file.canonical.clone(),
                    len: file.metadata.len(),
                    modified: file.metadata.modified().ok(),
                    read,
                }
            });
        }
        if validation.len() != self.files.len() + self.unread_files.len() {
            return Ok(None);
        }
        let matches = self
            .files
            .iter()
            .map(|file| (&file.key, &file.identity))
            .chain(self.unread_files.iter().map(|file| (&file.key, &file.identity)))
            .all(|(key, identity)| validation.get(key) == Some(identity));
        Ok(matches.then_some(ValidatedWorkspaceRootsTransitionPlan { plan: self, staging: None }))
    }
}

/// In-memory proof only; all observations are captured by existing work, never by polling.
#[derive(Clone, Copy, Debug, Default)]
pub struct SemanticIndexQualification {
    pub identity_verified: bool,
    pub coverage_complete: bool,
    pub pending_work: bool,
    pub context_pending: bool,
    pub epoch: u64,
}

pub struct SearchEngine {
    store: Store,
    embedder: Option<Embedder>,
    index: VectorIndex,
    dim: usize,
    loaded_reference_fingerprint: Option<String>,
    accepted_native_sidecar: bool,
    semantic_qualification: Cell<SemanticIndexQualification>,
    batch_size: usize,
    concurrency: usize,
    workspace_roots: Option<WorkspaceRoots>,
    workspace_roots_epoch: u64,
    workspace_overlay_cache: Mutex<WorkspaceOverlayCache>,
    workspace_baseline_hash_mode: BaselineHashMode,
    /// Whether this engine serves an EXTERNAL (remote) baseline through the persisted
    /// manifest. The manifest is a persistent warm-cache that deliberately survives a mode
    /// switch, so its mere presence proves nothing: every manifest-path dispatch and every
    /// baseline-evidence decision must consult this flag, not the table.
    serves_external_baseline: bool,
    /// Optional graph-context provider (dependency-inverted via
    /// [`crate::ports::GraphContextProvider`]). When set, code chunks are enriched
    /// with their outbound graph context before embedding. `None` keeps embeddings
    /// graph-free.
    graph_context_provider: Option<Arc<dyn crate::ports::GraphContextProvider>>,
    /// Optional resident-host snapshot source (dependency-inverted via
    /// [`crate::ports::ModuleSnapshotSource`]). When set, the overlay's incremental reindex
    /// chunks the resident's shared parse instead of parsing the file itself. `None` keeps the
    /// pure disk read+parse path.
    module_snapshot_source: Option<Arc<dyn ModuleSnapshotSource>>,
}

/// What the workspace overlay still owes a reader, as one consistent read. See
/// [`SearchEngine::workspace_overlay_debt`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkspaceOverlayDebt {
    /// Whether a full pass has ever built the overlay. Before it has, the overlay answers no
    /// query and owes every change on disk — a fact the counts below cannot express.
    pub initialized: bool,
    /// Dirty marks a point refresh could answer now.
    pub pending: usize,
    /// Keys proven present but not read back yet.
    pub unread: usize,
}

/// The overlay retry driver's condition signals, read without side effects by
/// [`SearchEngine::workspace_overlay_retry_signals`]. Any nonzero/true field means the
/// overlay owes another Embed pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OverlayRetrySignals {
    pub initialized: bool,
    pub needs_full_rescan: bool,
    pub pending_dirty_paths: usize,
    pub unembedded_entries: usize,
    pub unread_keys: usize,
}

impl OverlayRetrySignals {
    /// Whether any signal demands a pass: the first pass has not happened, removals were
    /// withheld or a persist failed, entries lack vectors, or proven-present files stayed
    /// unread. Dirty marks alone do not: they are the point refresh's to answer, and the
    /// entries it settles without vectors are what this pass then catches up.
    pub fn demands_a_pass(&self) -> bool {
        !self.initialized
            || self.needs_full_rescan
            || self.unembedded_entries > 0
            || self.unread_keys > 0
    }
}

/// Outcome of [`SearchEngine::refresh_dirty_contexts`]: how many context-dirty paths
/// were processed (marks cleared) and how many chunks had their context re-rendered
/// (and embedding cleared) as a result.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ContextRefreshStats {
    pub paths_marked: usize,
    pub paths_cleared: usize,
    pub chunks_updated: usize,
    /// Chunks whose live embedding was cleared (set NULL) as part of the re-render, so
    /// the caller knows a background re-embed pass is warranted. Equal to
    /// `chunks_updated` today (every re-render clears the embedding), tracked separately
    /// so the "kick the embed pass" decision reads an explicit signal, not a coincidence.
    pub cleared_embeddings: usize,
}

impl SearchEngine {
    pub fn new(db_path: &Path, config: SearchConfig) -> Result<Self, SearchError> {
        match Self::new_fenced(db_path, config, Self::permit_checkpointed_apply)? {
            FenceOutcome::Applied(engine) => Ok(engine),
            FenceOutcome::TransientRefusal | FenceOutcome::Superseded | FenceOutcome::Released => {
                unreachable!("the permit-all constructor fence cannot refuse")
            }
        }
    }

    /// [`Self::new`] with each shared-store mutation delegated to `apply`.
    ///
    /// Opening/migrating the store, persisting a newly built sidecar and rebuilding FTS are
    /// separate groups; loading or building the HNSW remains outside the callback.
    pub fn new_fenced<F>(
        db_path: &Path,
        config: SearchConfig,
        mut apply: F,
    ) -> Result<FenceOutcome<Self>, SearchError>
    where
        F: FnMut(
            &mut dyn FnMut(
                &mut dyn FnMut() -> ControlFlow<()>,
            ) -> ControlFlow<(), Result<(), SearchError>>,
        ) -> FenceOutcome<Result<(), SearchError>>,
    {
        let SearchConfig { embedder: embedder_config, execution } = config;
        embedder_config.validate()?;
        let store = match Self::open_store_fenced(db_path, "semantic", &mut apply)? {
            FenceOutcome::Applied(store) => store,
            FenceOutcome::TransientRefusal => return Ok(FenceOutcome::TransientRefusal),
            FenceOutcome::Superseded => return Ok(FenceOutcome::Superseded),
            FenceOutcome::Released => return Ok(FenceOutcome::Released),
        };
        let dim = embedder_config.dim.unwrap_or(1024);
        let embedder = Embedder::new(embedder_config);
        if let Err(error) = Self::ensure_store_token_layout_claim(&store, &embedder) {
            if Self::is_token_layout_mismatch(&error) {
                drop(store);
                return Self::fts_only_fenced(db_path, &mut apply);
            }
            return Err(error);
        }

        let (index, built_generation, identity_verified) =
            Self::load_or_build_index_unpublished(&store, dim, Some(&embedder))?;
        let loaded_reference_fingerprint = store.reference_collection_fingerprint("platform")?;
        info!(vectors = index.len(), dim, "search index loaded");

        if let Some(generation) = built_generation {
            if let Some(mut prepared) =
                Self::prepare_built(&store, dim, Some(&embedder), &index, generation)
            {
                let persisted = Self::fenced_checkpointed_value(&mut apply, |_| {
                    ControlFlow::Continue(Self::install_prepared_built(&store, &mut prepared))
                })?;
                match persisted {
                    FenceOutcome::Applied(()) => prepared.finish(),
                    FenceOutcome::TransientRefusal => return Ok(FenceOutcome::TransientRefusal),
                    FenceOutcome::Superseded => return Ok(FenceOutcome::Superseded),
                    FenceOutcome::Released => return Ok(FenceOutcome::Released),
                }
            }
        }

        match Self::fenced_checkpointed_value(&mut apply, |checkpoint| {
            Self::ensure_fts_checkpointed(&store, checkpoint)
        })? {
            FenceOutcome::Applied(()) => {}
            FenceOutcome::TransientRefusal => return Ok(FenceOutcome::TransientRefusal),
            FenceOutcome::Superseded => return Ok(FenceOutcome::Superseded),
            FenceOutcome::Released => return Ok(FenceOutcome::Released),
        }

        let context_pending = store.startup_context_pending();
        Ok(FenceOutcome::Applied(Self {
            store,
            embedder: Some(embedder),
            index,
            dim,
            loaded_reference_fingerprint,
            accepted_native_sidecar: built_generation.is_none(),
            semantic_qualification: Cell::new(SemanticIndexQualification {
                identity_verified,
                context_pending,
                ..SemanticIndexQualification::default()
            }),
            batch_size: execution.batch_size(),
            concurrency: execution.concurrency(),
            workspace_roots: None,
            workspace_roots_epoch: 0,
            workspace_overlay_cache: Mutex::new(WorkspaceOverlayCache::default()),
            workspace_baseline_hash_mode: BaselineHashMode::RawFileBytes,
            serves_external_baseline: false,
            graph_context_provider: None,
            module_snapshot_source: None,
        }))
    }

    #[allow(clippy::type_complexity)]
    fn permit_checkpointed_apply(
        operation: &mut dyn FnMut(
            &mut dyn FnMut() -> ControlFlow<()>,
        ) -> ControlFlow<(), Result<(), SearchError>>,
    ) -> FenceOutcome<Result<(), SearchError>> {
        let mut checkpoint = || ControlFlow::Continue(());
        match operation(&mut checkpoint) {
            ControlFlow::Continue(result) => FenceOutcome::Applied(result),
            ControlFlow::Break(()) => unreachable!("permit-all checkpoint cannot cancel"),
        }
    }

    fn open_store_fenced<A>(
        db_path: &Path,
        mode: &str,
        apply: &mut A,
    ) -> Result<FenceOutcome<Store>, SearchError>
    where
        A: FnMut(
            &mut dyn FnMut(
                &mut dyn FnMut() -> ControlFlow<()>,
            ) -> ControlFlow<(), Result<(), SearchError>>,
        ) -> FenceOutcome<Result<(), SearchError>>,
    {
        lifecycle::startup_snapshot(db_path, mode);
        // Opening stays inside the fence so a refused boot leaves no artifact behind: an
        // unadmitted daemon must not create this derived cache. The WAIT does not stay inside
        // it. Two processes may bootstrap the same database, and losing that race costs tens of
        // seconds; holding the fence across the wait would pin the interprocess lock and the
        // lease's own lifecycle mutex for all of it — exactly what splitting work into short
        // admissions exists to prevent, and enough to stall a shutdown or a peer's claim. So
        // every attempt is its own admission and the backoff runs with the fence released; the
        // half-open connection is dropped between attempts rather than carried across a window
        // in which this daemon does not own the workspace.
        enum OpenAttempt {
            Opened,
            Retry(SearchError),
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let mut store: Option<Store> = None;
        loop {
            let attempt = Self::fenced_checkpointed_value(&mut *apply, |checkpoint| {
                let opened = match Store::prepare_open_with_busy_timeout(
                    db_path,
                    crate::store::FENCED_OPEN_BUSY_TIMEOUT,
                ) {
                    Ok(opened) => opened,
                    Err(error) if crate::store::sqlite_bootstrap_retryable(&error) => {
                        return ControlFlow::Continue(Ok(OpenAttempt::Retry(error)));
                    }
                    Err(error) => return ControlFlow::Continue(Err(error)),
                };
                match opened.finish_open_checkpointed(checkpoint) {
                    Ok(ControlFlow::Continue(())) => {
                        store = Some(opened);
                        ControlFlow::Continue(Ok(OpenAttempt::Opened))
                    }
                    Ok(ControlFlow::Break(())) => ControlFlow::Break(()),
                    Err(error) if crate::store::sqlite_bootstrap_retryable(&error) => {
                        ControlFlow::Continue(Ok(OpenAttempt::Retry(error)))
                    }
                    Err(error) => ControlFlow::Continue(Err(error)),
                }
            })?;
            match attempt {
                FenceOutcome::Applied(OpenAttempt::Opened) => {
                    let opened = store.take().expect("an admitted store open produces the store");
                    opened.restore_operational_busy_timeout()?;
                    return Ok(FenceOutcome::Applied(opened));
                }
                FenceOutcome::Applied(OpenAttempt::Retry(error)) => {
                    if std::time::Instant::now() >= deadline {
                        return Err(error);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                FenceOutcome::TransientRefusal => return Ok(FenceOutcome::TransientRefusal),
                FenceOutcome::Superseded => return Ok(FenceOutcome::Superseded),
                FenceOutcome::Released => return Ok(FenceOutcome::Released),
            }
        }
    }

    /// Run one value-producing operation only when `apply` admits it. The erased callback shape
    /// matches `WorkspaceLease::with_ownership` without making this crate depend on the MCP host.
    fn fenced_value<T, F, A>(apply: &mut A, operation: F) -> Result<FenceOutcome<T>, SearchError>
    where
        F: FnOnce() -> Result<T, SearchError>,
        A: FnMut(
            &mut dyn FnMut() -> Result<(), SearchError>,
        ) -> FenceOutcome<Result<(), SearchError>>,
    {
        let mut operation = Some(operation);
        let mut value = None;
        let mut erased = || {
            let operation = operation.take().ok_or_else(|| {
                SearchError::Index("fenced apply operation invoked more than once".to_owned())
            })?;
            value = Some(operation()?);
            Ok(())
        };
        match apply(&mut erased) {
            FenceOutcome::TransientRefusal => Ok(FenceOutcome::TransientRefusal),
            FenceOutcome::Superseded => Ok(FenceOutcome::Superseded),
            FenceOutcome::Released => Ok(FenceOutcome::Released),
            FenceOutcome::Applied(Err(error)) => Err(error),
            FenceOutcome::Applied(Ok(())) => value.map(FenceOutcome::Applied).ok_or_else(|| {
                SearchError::Index(
                    "fenced apply callback admitted without invoking the operation".to_owned(),
                )
            }),
        }
    }

    fn fenced_value_retrying<T, F, A, R>(
        apply: &mut A,
        mut operation: F,
        retry_transient: &mut R,
    ) -> Result<FenceOutcome<T>, SearchError>
    where
        F: FnMut() -> Result<T, SearchError>,
        A: FnMut(
            &mut dyn FnMut() -> Result<(), SearchError>,
        ) -> FenceOutcome<Result<(), SearchError>>,
        R: FnMut() -> bool + ?Sized,
    {
        loop {
            match Self::fenced_value(apply, &mut operation)? {
                FenceOutcome::TransientRefusal if retry_transient() => {}
                outcome => return Ok(outcome),
            }
        }
    }

    /// Erased host adapter for one atomic transaction that must remain fenced but can refresh
    /// the lease heartbeat and observe terminal shutdown at bounded row boundaries.
    ///
    /// `operation` is `FnMut`, not `FnOnce`: a checkpoint releases and re-takes the
    /// interprocess lock, so a refusal can arrive AFTER the callback was admitted. The host
    /// answers that by rolling the transaction back and running the whole thing again, and an
    /// adapter that had handed its operation away would meet the retry with nothing to run.
    fn fenced_checkpointed_value<T, F, A>(
        apply: &mut A,
        mut operation: F,
    ) -> Result<FenceOutcome<T>, SearchError>
    where
        F: FnMut(&mut dyn FnMut() -> ControlFlow<()>) -> ControlFlow<(), Result<T, SearchError>>,
        A: FnMut(
            &mut dyn FnMut(
                &mut dyn FnMut() -> ControlFlow<()>,
            ) -> ControlFlow<(), Result<(), SearchError>>,
        ) -> FenceOutcome<Result<(), SearchError>>,
    {
        let mut value = None;
        let mut erased = |checkpoint: &mut dyn FnMut() -> ControlFlow<()>| {
            // Each attempt starts from nothing: a value left by a rolled-back attempt would
            // otherwise be reported as this one's result.
            value = None;
            match operation(checkpoint) {
                ControlFlow::Break(()) => ControlFlow::Break(()),
                ControlFlow::Continue(Err(error)) => ControlFlow::Continue(Err(error)),
                ControlFlow::Continue(Ok(result)) => {
                    value = Some(result);
                    ControlFlow::Continue(Ok(()))
                }
            }
        };
        match apply(&mut erased) {
            FenceOutcome::TransientRefusal => Ok(FenceOutcome::TransientRefusal),
            FenceOutcome::Superseded => Ok(FenceOutcome::Superseded),
            FenceOutcome::Released => Ok(FenceOutcome::Released),
            FenceOutcome::Applied(Err(error)) => Err(error),
            FenceOutcome::Applied(Ok(())) => value.map(FenceOutcome::Applied).ok_or_else(|| {
                SearchError::Index(
                    "checkpointed fenced apply callback admitted without completing the operation"
                        .to_owned(),
                )
            }),
        }
    }

    /// Load a persisted vector index when it still matches the current embeddings, otherwise
    /// build it from SQLite and persist the result. Rebuilding the HNSW is the dominant cold-
    /// start cost; loading a prebuilt one is ~40x faster (see `examples/bench_vector_index.rs`).
    /// Only a real, model-backed, file-backed engine persists — in-memory and embedder-less
    /// (FTS-only / overlay) engines fall back to a plain build with no sidecar.
    /// Load or build an index without publishing the built sidecar. The fenced workspace
    /// constructor publishes that sidecar in its own apply group.
    fn load_or_build_index_unpublished(
        store: &Store,
        dim: usize,
        embedder: Option<&Embedder>,
    ) -> Result<(VectorIndex, Option<i64>, bool), SearchError> {
        if let Some(key) = Self::persist_key(store, dim, embedder) {
            if let Some(index) = crate::vector_persist::try_load(store, &key) {
                info!(vectors = index.len(), "loaded persisted vector index");
                return Ok((index, None, true));
            }
        }
        let (generation, data, stored) = Self::load_index_snapshot(store, dim, embedder)?;
        #[cfg(test)]
        CONSTRUCTOR_APPLY_ACTIVE.with(|active| {
            assert!(!active.get(), "HNSW build ran inside the constructor apply callback")
        });
        let index = VectorIndex::build(dim, &data)?;
        let mut observed =
            lifecycle::Record::new(store.db_path(), "artifact_rebuilt", Reason::ExplicitRebuild);
        observed.outcome = Outcome::Completed;
        observed.emit(false);
        Ok((index, Some(generation), stored == 0))
    }

    fn load_index_snapshot(
        store: &Store,
        dim: usize,
        embedder: Option<&Embedder>,
    ) -> Result<crate::store::IndexEmbeddingSnapshot, SearchError> {
        let Some(embedder) = embedder else {
            return store.load_index_embedding_snapshot(dim);
        };
        match store.load_index_embedding_snapshot_for_profile_with_token_layout(
            embedder.model_id(),
            dim,
            embedder.token_layout_claim(),
        ) {
            Ok(snapshot) => Ok(snapshot),
            Err(SearchError::Index(message)) if message == "embedding profile mismatch" => {
                tracing::warn!(
                    "semantic vectors are unavailable for the current embedding profile"
                );
                Ok((
                    store.embedding_generation()?,
                    Vec::new(),
                    store.stored_embedding_count()?.max(1),
                ))
            }
            Err(error) => Err(error),
        }
    }

    fn ensure_store_token_layout_claim(
        store: &Store,
        embedder: &Embedder,
    ) -> Result<(), SearchError> {
        if let Some(claim) = embedder.token_layout_claim() {
            store.ensure_token_layout_claim(crate::token_policy::SEGMENTATION_VERSION, claim)?;
        }
        Ok(())
    }

    fn is_token_layout_mismatch(error: &SearchError) -> bool {
        matches!(error, SearchError::Index(message) if message == "token layout mismatch")
    }

    fn load_embeddings_for_profile(
        store: &Store,
        embedder: &Embedder,
        dim: usize,
    ) -> Result<crate::store::EmbeddingsSnapshot, SearchError> {
        if store.embedding_profile_matches(embedder.model_id(), dim)? {
            store.load_all_embeddings_with_profile_and_token_layout(
                embedder.model_id(),
                dim,
                embedder.token_layout_claim(),
            )
        } else {
            Err(SearchError::Index("embedding profile mismatch".to_owned()))
        }
    }

    /// Build the vector index from SQLite and persist it (best-effort) when persistence applies.
    /// The persisted fingerprint is taken from the SAME `data` snapshot the index is built from —
    /// never a fresh DB read — so the sidecar can never describe a different state than the saved
    /// index. An empty index is not persisted (e.g. before the deferred embedding pass runs).
    fn build_persisted_index(
        store: &Store,
        dim: usize,
        embedder: Option<&Embedder>,
    ) -> Result<VectorIndex, SearchError> {
        Self::build_persisted_index_with_progress(store, dim, embedder, None)
    }

    fn prepare_built(
        store: &Store,
        dim: usize,
        embedder: Option<&Embedder>,
        index: &VectorIndex,
        generation: i64,
    ) -> Option<crate::vector_persist::PreparedPersist> {
        Self::prepare_built_with_progress(store, dim, embedder, index, generation, None)
    }

    fn prepare_built_with_progress(
        store: &Store,
        dim: usize,
        embedder: Option<&Embedder>,
        index: &VectorIndex,
        generation: i64,
        progress: Option<&IndexPassToken>,
    ) -> Option<crate::vector_persist::PreparedPersist> {
        if index.is_empty() {
            return None;
        }
        let key = Self::persist_key(store, dim, embedder)?;
        match crate::vector_persist::prepare(index, &key, generation) {
            Ok(prepared) => Some(prepared),
            Err(error) => {
                if let Some(token) = progress {
                    token.finish(IndexPassState::Failed);
                }
                let mut record = lifecycle::Record::new(
                    store.db_path(),
                    "artifact_prepare",
                    Reason::ExplicitRebuild,
                );
                record.outcome = Outcome::Failed;
                record.emit(false);
                warn!("failed to prepare vector index: {error}");
                None
            }
        }
    }

    fn build_persisted_index_with_progress(
        store: &Store,
        dim: usize,
        embedder: Option<&Embedder>,
        progress: Option<&IndexPassToken>,
    ) -> Result<VectorIndex, SearchError> {
        if let Some(token) = progress {
            token.phase(IndexPhase::Persisting);
        }
        let (generation, data) = match embedder {
            Some(embedder) => Self::load_embeddings_for_profile(store, embedder, dim)?,
            None => store.load_all_embeddings_with_generation(dim)?,
        };
        let index = VectorIndex::build(dim, &data)?;
        if let Some(mut prepared) =
            Self::prepare_built_with_progress(store, dim, embedder, &index, generation, progress)
        {
            if let Err(error) =
                Self::install_prepared_built_with_progress(store, &mut prepared, progress)
            {
                if let Some(token) = progress {
                    token.finish(IndexPassState::Failed);
                }
                warn!("failed to publish vector index: {error}");
            }
            prepared.finish();
        }
        let mut observed =
            lifecycle::Record::new(store.db_path(), "artifact_rebuilt", Reason::ExplicitRebuild);
        observed.outcome = Outcome::Completed;
        observed.emit(false);
        Ok(index)
    }

    fn install_prepared_built(
        store: &Store,
        prepared: &mut crate::vector_persist::PreparedPersist,
    ) -> Result<(), SearchError> {
        Self::install_prepared_built_with_progress(store, prepared, None)
    }

    fn install_prepared_built_with_progress(
        store: &Store,
        prepared: &mut crate::vector_persist::PreparedPersist,
        progress: Option<&IndexPassToken>,
    ) -> Result<(), SearchError> {
        let generation = store.embedding_generation().inspect_err(|_| {
            let mut record =
                lifecycle::Record::new(store.db_path(), "artifact_publish", Reason::ReadError);
            record.outcome = Outcome::Failed;
            record.examples.push("generation_read".to_owned());
            record.emit(false);
        })?;
        if generation != prepared.generation() {
            let mut record =
                lifecycle::Record::new(store.db_path(), "artifact_publish", Reason::ArtifactStale);
            record.outcome = Outcome::Refused;
            record.examples.push("generation_mismatch".to_owned());
            record.emit(false);
            return Err(SearchError::Index(
                "vector index changed while its sidecar was prepared".to_owned(),
            ));
        }
        if let Err(error) = prepared.install() {
            if let Some(token) = progress {
                token.finish(IndexPassState::Failed);
            }
            warn!("failed to persist vector index: {error}");
        }
        Ok(())
    }

    /// The persistence identity for this engine's index, or `None` when persistence does not
    /// apply (no embedder, or an in-memory database).
    fn persist_key<'a>(
        store: &'a Store,
        dim: usize,
        embedder: Option<&'a Embedder>,
    ) -> Option<crate::vector_persist::PersistKey<'a>> {
        let model_id = embedder?.model_id();
        if store.db_path() == Path::new(":memory:") {
            return None;
        }
        Some(crate::vector_persist::PersistKey { db_path: store.db_path(), model_id, dim })
    }

    pub fn fts_only(db_path: &Path) -> Result<Self, SearchError> {
        match Self::fts_only_fenced(db_path, Self::permit_checkpointed_apply)? {
            FenceOutcome::Applied(engine) => Ok(engine),
            FenceOutcome::TransientRefusal | FenceOutcome::Superseded | FenceOutcome::Released => {
                unreachable!("the permit-all constructor fence cannot refuse")
            }
        }
    }

    /// [`Self::fts_only`] with store open/migration and FTS rebuild fenced separately.
    pub fn fts_only_fenced<F>(
        db_path: &Path,
        mut apply: F,
    ) -> Result<FenceOutcome<Self>, SearchError>
    where
        F: FnMut(
            &mut dyn FnMut(
                &mut dyn FnMut() -> ControlFlow<()>,
            ) -> ControlFlow<(), Result<(), SearchError>>,
        ) -> FenceOutcome<Result<(), SearchError>>,
    {
        let store = match Self::open_store_fenced(db_path, "fts", &mut apply)? {
            FenceOutcome::Applied(store) => store,
            FenceOutcome::TransientRefusal => return Ok(FenceOutcome::TransientRefusal),
            FenceOutcome::Superseded => return Ok(FenceOutcome::Superseded),
            FenceOutcome::Released => return Ok(FenceOutcome::Released),
        };
        let dim = 1024;
        let index = VectorIndex::new(dim)?;
        let loaded_reference_fingerprint = store.reference_collection_fingerprint("platform")?;

        match Self::fenced_checkpointed_value(&mut apply, |checkpoint| {
            Self::ensure_fts_checkpointed(&store, checkpoint)
        })? {
            FenceOutcome::Applied(()) => {}
            FenceOutcome::TransientRefusal => return Ok(FenceOutcome::TransientRefusal),
            FenceOutcome::Superseded => return Ok(FenceOutcome::Superseded),
            FenceOutcome::Released => return Ok(FenceOutcome::Released),
        }

        Ok(FenceOutcome::Applied(Self {
            store,
            embedder: None,
            index,
            dim,
            loaded_reference_fingerprint,
            accepted_native_sidecar: false,
            semantic_qualification: Cell::new(SemanticIndexQualification::default()),
            batch_size: EmbeddingExecutionPolicy::default().batch_size(),
            concurrency: EmbeddingExecutionPolicy::default().concurrency(),
            workspace_roots: None,
            workspace_roots_epoch: 0,
            workspace_overlay_cache: Mutex::new(WorkspaceOverlayCache::default()),
            workspace_baseline_hash_mode: BaselineHashMode::RawFileBytes,
            serves_external_baseline: false,
            graph_context_provider: None,
            module_snapshot_source: None,
        }))
    }

    pub fn semantic_overlay_only(
        db_path: &Path,
        config: SearchConfig,
    ) -> Result<Self, SearchError> {
        match Self::semantic_overlay_only_fenced(db_path, config, |apply| {
            Self::permit_checkpointed_apply(apply)
        })? {
            FenceOutcome::Applied(engine) => Ok(engine),
            FenceOutcome::TransientRefusal | FenceOutcome::Superseded | FenceOutcome::Released => {
                unreachable!("the permit-all constructor fence cannot refuse")
            }
        }
    }

    /// [`Self::semantic_overlay_only`] with store open/migration and FTS rebuild fenced separately.
    pub fn semantic_overlay_only_fenced<F>(
        db_path: &Path,
        config: SearchConfig,
        mut apply: F,
    ) -> Result<FenceOutcome<Self>, SearchError>
    where
        F: FnMut(
            &mut dyn FnMut(
                &mut dyn FnMut() -> ControlFlow<()>,
            ) -> ControlFlow<(), Result<(), SearchError>>,
        ) -> FenceOutcome<Result<(), SearchError>>,
    {
        let SearchConfig { embedder: embedder_config, execution } = config;
        embedder_config.validate()?;
        let store = match Self::open_store_fenced(db_path, "semantic_overlay", &mut apply)? {
            FenceOutcome::Applied(store) => store,
            FenceOutcome::TransientRefusal => return Ok(FenceOutcome::TransientRefusal),
            FenceOutcome::Superseded => return Ok(FenceOutcome::Superseded),
            FenceOutcome::Released => return Ok(FenceOutcome::Released),
        };
        let dim = embedder_config.dim.unwrap_or(1024);
        let embedder = Embedder::new(embedder_config);
        if let Err(error) = Self::ensure_store_token_layout_claim(&store, &embedder) {
            if Self::is_token_layout_mismatch(&error) {
                drop(store);
                return Self::fts_only_fenced(db_path, &mut apply);
            }
            return Err(error);
        }
        let index = VectorIndex::new(dim)?;
        let loaded_reference_fingerprint = store.reference_collection_fingerprint("platform")?;

        match Self::fenced_checkpointed_value(&mut apply, |checkpoint| {
            Self::ensure_fts_checkpointed(&store, checkpoint)
        })? {
            FenceOutcome::Applied(()) => {}
            FenceOutcome::TransientRefusal => return Ok(FenceOutcome::TransientRefusal),
            FenceOutcome::Superseded => return Ok(FenceOutcome::Superseded),
            FenceOutcome::Released => return Ok(FenceOutcome::Released),
        }

        Ok(FenceOutcome::Applied(Self {
            store,
            embedder: Some(embedder),
            index,
            dim,
            loaded_reference_fingerprint,
            accepted_native_sidecar: false,
            semantic_qualification: Cell::new(SemanticIndexQualification::default()),
            batch_size: execution.batch_size(),
            concurrency: execution.concurrency(),
            workspace_roots: None,
            workspace_roots_epoch: 0,
            workspace_overlay_cache: Mutex::new(WorkspaceOverlayCache::default()),
            workspace_baseline_hash_mode: BaselineHashMode::RawFileBytes,
            serves_external_baseline: false,
            graph_context_provider: None,
            module_snapshot_source: None,
        }))
    }

    fn ensure_fts_checkpointed(
        store: &Store,
        checkpoint: &mut dyn FnMut() -> ControlFlow<()>,
    ) -> ControlFlow<(), Result<(), SearchError>> {
        let chunk_count = match store.chunk_count() {
            Ok(count) => count,
            Err(error) => return ControlFlow::Continue(Err(error)),
        };
        let fts_count = match store.fts_count() {
            Ok(count) => count,
            Err(error) => return ControlFlow::Continue(Err(error)),
        };
        if chunk_count > 0 && fts_count == 0 {
            info!(chunks = chunk_count, "populating FTS index from existing data");
            match store.rebuild_fts_checkpointed(checkpoint) {
                Ok(ControlFlow::Continue(())) => {}
                Ok(ControlFlow::Break(())) => return ControlFlow::Break(()),
                Err(error) => return ControlFlow::Continue(Err(error)),
            }
        }
        ControlFlow::Continue(Ok(()))
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    /// Inject the graph-context provider (dependency-inverted). Once set, code chunks
    /// indexed afterwards are enriched with their outbound graph context before
    /// embedding. Idempotent; pass-through to the indexing paths.
    pub fn set_graph_context_provider(
        &mut self,
        provider: Arc<dyn crate::ports::GraphContextProvider>,
    ) {
        if let Ok(mut cache) = self.workspace_overlay_cache.lock() {
            cache.set_graph_context_provider(provider.clone());
        }
        self.graph_context_provider = Some(provider);
    }

    /// Install the provider of an already-verified graph publication without invalidating stable
    /// lexical overlay entries. The cache raises its wholesale fence so plans prepared with the
    /// previous semantic source cannot publish; keeping both pointers together ensures later
    /// watcher point refreshes do not render against the graph file current at daemon boot.
    pub fn replace_published_graph_context_provider(
        &mut self,
        provider: Arc<dyn crate::ports::GraphContextProvider>,
    ) -> Result<(), SearchError> {
        self.workspace_overlay_cache
            .lock()
            .map_err(|error| {
                SearchError::Index(format!("workspace overlay cache lock error: {error}"))
            })?
            .replace_graph_context_provider(provider.clone());
        self.graph_context_provider = Some(provider);
        Ok(())
    }

    /// Inject the resident-host snapshot source (dependency-inverted). Once set, the overlay's
    /// incremental reindex prefers the resident's shared parse. Does not touch cached entries:
    /// the source changes only HOW a file is read+parsed, never the chunk output.
    pub fn set_module_snapshot_source(&mut self, source: Arc<dyn ModuleSnapshotSource>) {
        self.module_snapshot_source = Some(source);
    }

    /// The injected resident-host snapshot source, cloned so the orchestrator can prefetch
    /// snapshots OFF the engine lock (the resident read must never overlap the engine lock).
    pub fn module_snapshot_source(&self) -> Option<Arc<dyn ModuleSnapshotSource>> {
        self.module_snapshot_source.clone()
    }

    /// The overlay paths currently marked dirty, awaiting a point refresh.
    pub fn workspace_overlay_dirty_paths(&self) -> Result<Vec<FileKey>, SearchError> {
        let cache = self
            .workspace_overlay_cache
            .lock()
            .map_err(|e| SearchError::Index(format!("workspace overlay cache lock error: {e}")))?;
        Ok(cache.dirty_paths_list())
    }

    /// How many overlay entries have been built from a resident-provided shared parse since the
    /// engine's workspace root was set. Observability for the resident-fed reindex (proves the
    /// shared-parse path fired, e.g. in a regression test).
    pub fn workspace_overlay_resident_fed_count(&self) -> Result<usize, SearchError> {
        let cache = self
            .workspace_overlay_cache
            .lock()
            .map_err(|e| SearchError::Index(format!("workspace overlay cache lock error: {e}")))?;
        Ok(cache.resident_fed_count())
    }

    pub fn index_directory(
        &mut self,
        root: &Path,
        progress: Option<&Arc<IndexProgress>>,
    ) -> Result<usize, SearchError> {
        let mut pass = progress.map(IndexProgress::begin_pass);
        let token = pass.as_ref().map(ActivePass::token);
        let progress = token.as_ref();
        let lifecycle_batch = Batch::new(self.store.db_path(), Reason::ExplicitRebuild);
        let lifecycle_result = lifecycle_batch.context().in_scope(|| {
        let bsl_files = self.boot_ingest_files(root);

        info!(total_files = bsl_files.len(), "scanning BSL files");

        let embedder = self.embedder.as_ref().ok_or_else(|| {
            SearchError::Embedder(
                "Cannot generate embeddings: embedder not configured. Set EMBEDDING_URL.".into(),
            )
        })?;
        let model_id = embedder.model_id().to_owned();
        let dimension = embedder.dimension();

        let mut tasks: Vec<FileTask> = Vec::new();
        let mut total_chunks = 0usize;

        for (key, file_path) in &bsl_files {
            let content = match std::fs::read_to_string(file_path) {
                Ok(c) => c,
                Err(e) => {
                    lifecycle::decision(self.store.db_path(), key, Reason::ReadError, None, None);
                    warn!(?file_path, "failed to read file: {e}");
                    if let Some(p) = progress { p.finish(IndexPassState::Failed); }
                    continue;
                }
            };

            let hash = blake3::hash(content.as_bytes());

            let (_, reason) = self.observed_file_hash(key, hash.as_bytes())?;
            if reason == Reason::Unchanged { continue; }

            let (documents, context_owed) = crate::document::prepare_indexed_documents(
                key,
                &content,
                self.graph_context_provider.as_deref(),
                Some(embedder),
            )?;
            let (chunks, graph_contexts, source_spans) =
                crate::document::chunks_contexts_and_spans(&documents);
            if chunks.is_empty() {
                continue;
            }
            let texts: Vec<String> =
                documents.iter().map(crate::document::semantic_text_for_indexed_document).collect();

            let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
            let ranges = embedder.batch_ranges(&refs, self.batch_size)?;
            total_chunks += chunks.len();
            tasks.push(FileTask {
                key: key.clone(),
                reason,
                hash: hash.as_bytes().to_vec(),
                chunks,
                texts,
                ranges,
                graph_contexts,
                context_owed,
                source_spans,
            });
        }

        if tasks.is_empty() {
            info!("no files need reindexing");
            return Ok(0);
        }

        let dim = self.dim;
        let total_batches: usize = tasks.iter().map(|t| t.ranges.len()).sum();


        if let Some(p) = &progress {
            p.set_totals(tasks.len(), total_chunks, total_batches);
        }

        let concurrency = self.concurrency.min(tasks.len());
        info!(
            files = tasks.len(),
            chunks = total_chunks,
            batches = total_batches,
            concurrency,
            "generating embeddings"
        );

        let (task_tx, task_rx) = crossbeam_channel::bounded::<FileTask>(concurrency * 2);
        let (result_tx, result_rx) = crossbeam_channel::bounded::<FileResult>(concurrency * 2);

        let workers: Vec<std::thread::JoinHandle<()>> = (0..concurrency)
            .map(|_| {
                let rx = task_rx.clone();
                let tx = result_tx.clone();
                let emb = embedder.clone();
                let prog = progress.cloned();

                let context = lifecycle_batch.context();
                std::thread::spawn(move || context.in_scope(|| {
                    while let Ok(task) = rx.recv() {
                        let mut embeddings = Vec::with_capacity(task.texts.len());
                        let mut error = None;

                        for range in &task.ranges {
                            let batch = &task.texts[range.clone()];
                            let refs: Vec<&str> = batch.iter().map(|s| s.as_str()).collect();
                            match emb.embed_batch(&refs) {
                                Ok(embs) => {
                                    Self::observe_embedding_dimensions(&embs, dim, prog.as_ref());
                                    embeddings.extend(embs);
                                    if let Some(p) = &prog {
                                        p.advance(batch.len(), 1);
                                    }
                                }
                                Err(e) => {
                                    error = Some(e);
                                    break;
                                }
                            }
                        }

                        let _ = tx.send(FileResult {
                            key: task.key,
                            reason: task.reason,
                            hash: task.hash,
                            chunks: task.chunks,
                            graph_contexts: task.graph_contexts,
                            context_owed: task.context_owed,
                            source_spans: task.source_spans,
                            embeddings: match error {
                                None => Ok(embeddings),
                                Some(e) => Err(e),
                            },
                        });
                    }
                }))
            })
            .collect();

        drop(task_rx);
        drop(result_tx);

        let producer = std::thread::spawn(move || {
            for task in tasks {
                if task_tx.send(task).is_err() {
                    break;
                }
            }
        });

        let mut indexed = 0usize;
        let mut first_error = None;
        while let Ok(result) = result_rx.recv() {
            match result.embeddings {
                Ok(embeddings) => {
                    if let Err(error) = lifecycle::with_reason(result.reason, || self.store.reindex_file_with_identity_and_source_spans(
                        &result.key.root_id,
                        &result.key.path,
                        &result.hash,
                        &result.chunks,
                        Some(&embeddings),
                        Some(&result.graph_contexts),
                        &model_id,
                        dimension,
                        embedder.token_layout_claim(),
                        &result.source_spans,
                    )) {
                        first_error.get_or_insert(error);
                        continue;
                    }
                    if result.context_owed {
                        if let Err(error) = self.owe_graph_contexts([&result.key]) {
                            first_error.get_or_insert(error);
                        }
                    }
                    indexed += 1;
                    debug!(file = %result.key.path, chunks = result.chunks.len(), "file indexed");
                }
                Err(e) => {
                    warn!(file = %result.key.path, "embedding failed after retries: {e}");
                    first_error.get_or_insert(e);
                }
            }
        }

        if producer.join().is_err() {
            first_error.get_or_insert_with(|| crate::EmbeddingFailure::new(crate::EmbeddingFailureCode::EmbeddingFailed).into());
        }
        for w in workers {
            if w.join().is_err() {
                first_error.get_or_insert_with(|| crate::EmbeddingFailure::new(crate::EmbeddingFailureCode::EmbeddingFailed).into());
            }
        }

        if let Some(p) = &progress {
            p.phase(IndexPhase::Persisting);
        }

        if let Some(error) = first_error {
            return Err(error);
        }
        self.index = Self::build_persisted_index_with_progress(&self.store, self.dim, self.embedder.as_ref(), progress)?;
        self.observe_live_index("live_index_replaced", Reason::ExplicitRebuild, Outcome::Completed);

        info!(indexed, total_vectors = self.index.len(), "indexing complete");
        Ok(indexed)
        });
        let lifecycle_outcome =
            if lifecycle_result.is_ok() { Outcome::Completed } else { Outcome::Failed };
        lifecycle_batch.finish(lifecycle_outcome);
        if let Some(pass) = pass.as_mut() {
            pass.finish(if lifecycle_result.is_ok() {
                IndexPassState::Ready
            } else {
                IndexPassState::Failed
            });
        }
        lifecycle_result
    }

    /// Ingest one file's chunks produced by the fused graph pass: writes chunk text,
    /// FTS rows, and the per-chunk graph context with NO embedding (filled later by
    /// [`Self::embed_pending_chunks_standalone`]), and records the file hash so an
    /// unchanged file is skipped next run. Chunks and contexts originate in the graph
    /// build, so no
    /// parsing or graph round-trip happens here — this is purely the storage write.
    pub fn ingest_fused_file(
        &mut self,
        key: &FileKey,
        hash: &[u8],
        chunks: &[crate::Chunk],
        graph_contexts: &[Option<String>],
    ) -> Result<(), SearchError> {
        let mut checkpoint = || ControlFlow::Continue(());
        match self.ingest_fused_file_checkpointed(
            key,
            hash,
            chunks,
            graph_contexts,
            &mut checkpoint,
        ) {
            ControlFlow::Continue(result) => result,
            ControlFlow::Break(()) => unreachable!("the permit-all checkpoint cannot cancel"),
        }
    }

    pub fn ingest_fused_file_checkpointed(
        &mut self,
        key: &FileKey,
        hash: &[u8],
        chunks: &[crate::Chunk],
        graph_contexts: &[Option<String>],
        checkpoint: &mut dyn FnMut() -> ControlFlow<()>,
    ) -> ControlFlow<(), Result<(), SearchError>> {
        self.invalidate_semantic_coverage();
        match self.store.reindex_file_with_context_checkpointed(
            key,
            hash,
            chunks,
            None,
            Some(graph_contexts),
            checkpoint,
        ) {
            Ok(ControlFlow::Continue(_)) => ControlFlow::Continue(Ok(())),
            Ok(ControlFlow::Break(())) => ControlFlow::Break(()),
            Err(error) => ControlFlow::Continue(Err(error)),
        }
    }

    /// Run the fused embedding pass against a database without holding a live
    /// [`SearchEngine`]: opens its own connection (WAL — concurrent readers, single
    /// writer) so the engine's outer mutex stays free for lexical search during the
    /// long HTTP-bound embed. Returns the freshly built [`VectorIndex`] for the caller
    /// to swap into the live engine via [`Self::set_vector_index`].
    pub fn embed_pending_chunks_standalone(
        db_path: &Path,
        config: &SearchConfig,
        progress: Option<&Arc<IndexProgress>>,
        should_continue: Option<&(dyn Fn() -> bool + Sync)>,
    ) -> Result<VectorIndex, SearchError> {
        let store = Store::open(db_path)?;
        let (index, _) = Self::embed_pending_chunks_with_fence(
            &store,
            config,
            progress,
            should_continue,
            |operation| FenceOutcome::Applied(operation()),
            None,
        )?;
        Ok(index)
    }

    pub fn embed_pending_chunks_fenced<F>(
        db_path: &Path,
        config: &SearchConfig,
        progress: Option<&Arc<IndexProgress>>,
        should_continue: Option<&(dyn Fn() -> bool + Sync)>,
        apply: F,
    ) -> Result<FenceOutcome<VectorIndex>, SearchError>
    where
        F: FnMut(
            &mut dyn FnMut() -> Result<(), SearchError>,
        ) -> FenceOutcome<Result<(), SearchError>>,
    {
        Self::embed_pending_chunks_fenced_retrying(
            db_path,
            config,
            progress,
            should_continue,
            apply,
            || false,
        )
    }

    /// Workspace-fenced embedding with host-owned transient retry policy. A paid network batch
    /// stays in memory while only its SQLite commit is retried.
    pub fn embed_pending_chunks_fenced_retrying<F, R>(
        db_path: &Path,
        config: &SearchConfig,
        progress: Option<&Arc<IndexProgress>>,
        should_continue: Option<&(dyn Fn() -> bool + Sync)>,
        apply: F,
        retry_transient: R,
    ) -> Result<FenceOutcome<VectorIndex>, SearchError>
    where
        F: FnMut(
            &mut dyn FnMut() -> Result<(), SearchError>,
        ) -> FenceOutcome<Result<(), SearchError>>,
        R: FnMut() -> bool,
    {
        let mut pass = progress.map(IndexProgress::begin_pass);
        let token = pass.as_ref().map(ActivePass::token);
        let result = Self::embed_pending_chunks_fenced_retrying_owned(
            db_path,
            config,
            token.as_ref(),
            should_continue,
            apply,
            retry_transient,
        );
        if let Some(pass) = pass.as_mut() {
            pass.finish(match &result {
                Ok(FenceOutcome::Applied(_)) => IndexPassState::Ready,
                Ok(FenceOutcome::Superseded) => IndexPassState::Superseded,
                Ok(FenceOutcome::Released) => IndexPassState::Cancelled,
                _ => IndexPassState::Failed,
            });
        }
        result
    }

    pub fn embed_pending_chunks_fenced_retrying_owned<F, R>(
        db_path: &Path,
        config: &SearchConfig,
        progress: Option<&IndexPassToken>,
        should_continue: Option<&(dyn Fn() -> bool + Sync)>,
        mut apply: F,
        mut retry_transient: R,
    ) -> Result<FenceOutcome<VectorIndex>, SearchError>
    where
        F: FnMut(
            &mut dyn FnMut() -> Result<(), SearchError>,
        ) -> FenceOutcome<Result<(), SearchError>>,
        R: FnMut() -> bool,
    {
        let store = Store::open_existing(db_path)?;
        let (index, completed) = Self::run_embedding_pass_owned(
            &store,
            &Embedder::new(config.embedder.clone()),
            config.embedder.dim.unwrap_or(1024),
            config.execution.batch_size(),
            config.execution.concurrency(),
            progress,
            should_continue,
            &mut apply,
            Some(&mut retry_transient),
        )?;
        Ok(match completed {
            FenceOutcome::Applied(()) => FenceOutcome::Applied(index),
            FenceOutcome::TransientRefusal => FenceOutcome::TransientRefusal,
            FenceOutcome::Superseded => FenceOutcome::Superseded,
            FenceOutcome::Released => FenceOutcome::Released,
        })
    }

    fn embed_pending_chunks_with_fence<F>(
        store: &Store,
        config: &SearchConfig,
        progress: Option<&Arc<IndexProgress>>,
        should_continue: Option<&(dyn Fn() -> bool + Sync)>,
        mut apply: F,
        retry_transient: Option<&mut dyn FnMut() -> bool>,
    ) -> Result<(VectorIndex, FenceOutcome<()>), SearchError>
    where
        F: FnMut(
            &mut dyn FnMut() -> Result<(), SearchError>,
        ) -> FenceOutcome<Result<(), SearchError>>,
    {
        let dim = config.embedder.dim.unwrap_or(1024);
        let embedder = Embedder::new(config.embedder.clone());
        // `run_embedding_pass` persists the built index from this background thread (it owns the
        // standalone store), NOT after the caller's `set_vector_index` swap which holds the live
        // engine lock — so the ~1.5s save never blocks concurrent search and the swap is instant.
        Self::run_embedding_pass(
            store,
            &embedder,
            dim,
            config.execution.batch_size(),
            config.execution.concurrency(),
            progress,
            should_continue,
            &mut apply,
            retry_transient,
        )
    }

    /// Atomically swap the in-memory vector index of a live engine. Brief operation
    /// held under the engine's outer mutex (the same lock semantic queries take while
    /// reading `self.index`), so a concurrent reader sees either the old or the new
    /// index, never a torn one.
    pub fn set_vector_index(&mut self, index: VectorIndex) {
        self.index = index;
        self.observe_live_index("live_index_replaced", Reason::Embedding, Outcome::Completed);
    }

    fn observe_live_index(&self, kind: &'static str, reason: Reason, outcome: Outcome) {
        let mut observed = lifecycle::Record::new(self.store.db_path(), kind, reason);
        observed.outcome = outcome;
        observed.emit(kind == "live_index_evicted" && lifecycle::Context::current().is_some());
    }

    #[allow(clippy::too_many_arguments)]
    fn run_fenced_embedding_pass(
        store: &Store,
        embedder: &Embedder,
        dim: usize,
        ranges: &[std::ops::Range<usize>],
        items: &[(i64, String)],
        progress: Option<&IndexPassToken>,
        should_continue: Option<&(dyn Fn() -> bool + Sync)>,
        apply: &mut impl FnMut(
            &mut dyn FnMut() -> Result<(), SearchError>,
        ) -> FenceOutcome<Result<(), SearchError>>,
        retry_transient: &mut dyn FnMut() -> bool,
    ) -> Result<(VectorIndex, FenceOutcome<()>), SearchError> {
        let mut embedded = 0usize;
        let mut first_error = None;
        for range in ranges {
            let batch = &items[range.clone()];
            if should_continue.is_some_and(|keep_going| !keep_going()) {
                let (_, data) = Self::load_embeddings_for_profile(store, embedder, dim)?;
                return Ok((VectorIndex::build(dim, &data)?, FenceOutcome::Released));
            }
            match Self::fenced_value_retrying(apply, || Ok(()), retry_transient)? {
                FenceOutcome::Applied(()) => {}
                FenceOutcome::TransientRefusal => {
                    let (_, data) = Self::load_embeddings_for_profile(store, embedder, dim)?;
                    return Ok((VectorIndex::build(dim, &data)?, FenceOutcome::TransientRefusal));
                }
                FenceOutcome::Superseded => {
                    let (_, data) = Self::load_embeddings_for_profile(store, embedder, dim)?;
                    return Ok((VectorIndex::build(dim, &data)?, FenceOutcome::Superseded));
                }
                FenceOutcome::Released => {
                    let (_, data) = Self::load_embeddings_for_profile(store, embedder, dim)?;
                    return Ok((VectorIndex::build(dim, &data)?, FenceOutcome::Released));
                }
            }

            let refs: Vec<&str> = batch.iter().map(|(_, text)| text.as_str()).collect();
            let embeddings = match embedder.embed_batch(&refs) {
                Ok(embeddings) => embeddings,
                Err(error) => {
                    first_error = Some(error);
                    break;
                }
            };
            if let Some(progress) = progress {
                progress.advance(batch.len(), 1);
            }
            Self::observe_embedding_dimensions(&embeddings, dim, progress);
            let pairs: Vec<_> = batch.iter().map(|(id, _)| *id).zip(embeddings).collect();
            match Self::fenced_value_retrying(
                apply,
                || {
                    store.set_chunk_embeddings_with_token_layout(
                        &pairs,
                        Some((embedder.model_id(), dim)),
                        embedder.token_layout_claim(),
                    )
                },
                retry_transient,
            )? {
                FenceOutcome::Applied(()) => embedded += pairs.len(),
                FenceOutcome::TransientRefusal => {
                    let (_, data) = Self::load_embeddings_for_profile(store, embedder, dim)?;
                    return Ok((VectorIndex::build(dim, &data)?, FenceOutcome::TransientRefusal));
                }
                FenceOutcome::Superseded => {
                    let (_, data) = Self::load_embeddings_for_profile(store, embedder, dim)?;
                    return Ok((VectorIndex::build(dim, &data)?, FenceOutcome::Superseded));
                }
                FenceOutcome::Released => {
                    let (_, data) = Self::load_embeddings_for_profile(store, embedder, dim)?;
                    return Ok((VectorIndex::build(dim, &data)?, FenceOutcome::Released));
                }
            }
        }

        if let Some(progress) = progress {
            progress.phase(IndexPhase::Persisting);
        }
        let (generation, data) = Self::load_embeddings_for_profile(store, embedder, dim)?;
        let index = VectorIndex::build(dim, &data)?;
        if should_continue.is_some_and(|keep_going| !keep_going()) {
            return Ok((index, FenceOutcome::Released));
        }
        if let Some(error) = first_error {
            let outcome = Self::fenced_value_retrying(apply, || Ok(()), retry_transient)?;
            return if matches!(outcome, FenceOutcome::Applied(())) {
                Err(error)
            } else {
                Ok((index, outcome))
            };
        }
        let persisted = if let Some(mut prepared) = Self::prepare_built_with_progress(
            store,
            dim,
            Some(embedder),
            &index,
            generation,
            progress,
        ) {
            let outcome = Self::fenced_value_retrying(
                apply,
                || Self::install_prepared_built_with_progress(store, &mut prepared, progress),
                retry_transient,
            )?;
            if matches!(outcome, FenceOutcome::Applied(())) {
                prepared.finish();
            }
            outcome
        } else {
            FenceOutcome::Applied(())
        };
        if matches!(persisted, FenceOutcome::Applied(())) {
            info!(embedded, total_vectors = index.len(), "fused embedding complete");
        }
        Ok((index, persisted))
    }

    /// Core of the fused embedding phase, free of any borrow on a live engine so it can
    /// run against either `self.store` or a standalone connection. Reads the `code`
    /// chunks still missing an embedding, embeds their semantic text concurrently,
    /// updates each row, then builds and returns the vector index.
    #[allow(
        clippy::too_many_arguments,
        reason = "the pass takes distinct execution controls plus the publish fence; bundling them only renames the same call contract"
    )]
    fn run_embedding_pass(
        store: &Store,
        embedder: &Embedder,
        dim: usize,
        batch_size: usize,
        concurrency: usize,
        progress: Option<&Arc<IndexProgress>>,
        should_continue: Option<&(dyn Fn() -> bool + Sync)>,
        apply: &mut impl FnMut(
            &mut dyn FnMut() -> Result<(), SearchError>,
        ) -> FenceOutcome<Result<(), SearchError>>,
        retry_transient: Option<&mut dyn FnMut() -> bool>,
    ) -> Result<(VectorIndex, FenceOutcome<()>), SearchError> {
        let mut pass = progress.map(IndexProgress::begin_pass);
        let token = pass.as_ref().map(ActivePass::token);
        let result = Self::run_embedding_pass_owned(
            store,
            embedder,
            dim,
            batch_size,
            concurrency,
            token.as_ref(),
            should_continue,
            apply,
            retry_transient,
        );
        if let Some(pass) = pass.as_mut() {
            pass.finish(match &result {
                Ok((_, FenceOutcome::Applied(()))) => IndexPassState::Ready,
                Ok((_, FenceOutcome::Superseded)) => IndexPassState::Superseded,
                Ok((_, FenceOutcome::Released)) => IndexPassState::Cancelled,
                _ => IndexPassState::Failed,
            });
        }
        result
    }

    #[allow(clippy::too_many_arguments)]
    fn run_embedding_pass_owned(
        store: &Store,
        embedder: &Embedder,
        dim: usize,
        batch_size: usize,
        concurrency: usize,
        progress: Option<&IndexPassToken>,
        should_continue: Option<&(dyn Fn() -> bool + Sync)>,
        apply: &mut impl FnMut(
            &mut dyn FnMut() -> Result<(), SearchError>,
        ) -> FenceOutcome<Result<(), SearchError>>,
        mut retry_transient: Option<&mut dyn FnMut() -> bool>,
    ) -> Result<(VectorIndex, FenceOutcome<()>), SearchError> {
        let lifecycle_batch = Batch::new(store.db_path(), Reason::Embedding);
        let mut embedding_skipped = false;
        let lifecycle_result = lifecycle_batch.context().in_scope(|| {
            embedder.config().validate()?;
            Self::ensure_store_token_layout_claim(store, embedder)?;
            if !store.embedding_profile_matches(embedder.model_id(), dim)? {
                return Err(SearchError::Index("embedding profile mismatch".to_owned()));
            }
            let pending = store.load_pending_embedding_documents("code")?;
            lifecycle_batch.pending(pending.len() as u64);
            if pending.is_empty() {
                if let Some(p) = progress {
                    p.phase(IndexPhase::Persisting);
                }
                embedding_skipped = true;
                let (generation, data) = Self::load_embeddings_for_profile(store, embedder, dim)?;
                let index = VectorIndex::build(dim, &data)?;
                // The sidecar is a shared artifact like any other; a caller that may no longer
                // write leaves it to whoever may.
                if should_continue.is_some_and(|keep_going| !keep_going()) {
                    return Ok((index, FenceOutcome::Released));
                }
                let persisted = if let Some(mut prepared) = Self::prepare_built_with_progress(
                    store,
                    dim,
                    Some(embedder),
                    &index,
                    generation,
                    progress,
                ) {
                    let mut persist = || {
                        Self::install_prepared_built_with_progress(store, &mut prepared, progress)
                    };
                    let outcome = match retry_transient.as_deref_mut() {
                        Some(retry) => Self::fenced_value_retrying(apply, &mut persist, retry)?,
                        None => Self::fenced_value(apply, persist)?,
                    };
                    if matches!(outcome, FenceOutcome::Applied(())) {
                        prepared.finish();
                    }
                    outcome
                } else {
                    FenceOutcome::Applied(())
                };
                return Ok((index, persisted));
            }

            let items: Vec<(i64, String)> = pending
                .into_iter()
                .map(|(id, doc)| (id, crate::document::semantic_text_for_indexed_document(&doc)))
                .collect();
            let total = items.len();

            let texts: Vec<&str> = items.iter().map(|(_, text)| text.as_str()).collect();
            let ranges = embedder.batch_ranges(&texts, batch_size)?;
            let total_batches = ranges.len();

            if let Some(p) = &progress {
                p.set_totals(0, total, total_batches);
            }

            let concurrency = concurrency.min(total_batches.max(1));
            info!(chunks = total, batches = total_batches, concurrency, "embedding fused chunks");

            if let Some(retry_transient) = retry_transient {
                return Self::run_fenced_embedding_pass(
                    store,
                    embedder,
                    dim,
                    &ranges,
                    &items,
                    progress,
                    should_continue,
                    apply,
                    retry_transient,
                );
            }

            // Fan batches of (chunk_id, text) out to embedder workers; the main thread
            // applies each batch's vectors (SQLite is single-writer). Nothing larger than
            // one batch is held per worker, so peak RAM stays bounded by the batch size.
            let (task_tx, task_rx) =
                crossbeam_channel::bounded::<Vec<(i64, String)>>(concurrency * 2);
            #[allow(clippy::type_complexity)]
            let (result_tx, result_rx) = crossbeam_channel::bounded::<
                Result<Vec<(i64, Vec<f32>)>, SearchError>,
            >(concurrency * 2);

            let workers: Vec<std::thread::JoinHandle<()>> = (0..concurrency)
                .map(|_| {
                    let rx = task_rx.clone();
                    let tx = result_tx.clone();
                    let emb = embedder.clone();
                    let prog = progress.cloned();
                    let context = lifecycle_batch.context();
                    std::thread::spawn(move || {
                        context.in_scope(|| {
                            while let Ok(batch) = rx.recv() {
                                let refs: Vec<&str> =
                                    batch.iter().map(|(_, t)| t.as_str()).collect();
                                let out = match emb.embed_batch(&refs) {
                                    Ok(embs) => {
                                        Self::observe_embedding_dimensions(
                                            &embs,
                                            dim,
                                            prog.as_ref(),
                                        );
                                        if let Some(p) = &prog {
                                            p.advance(batch.len(), 1);
                                        }
                                        Ok(batch.iter().map(|(id, _)| *id).zip(embs).collect())
                                    }
                                    Err(e) => Err(e),
                                };
                                if tx.send(out).is_err() {
                                    break;
                                }
                            }
                        })
                    })
                })
                .collect();

            drop(task_rx);
            drop(result_tx);

            let producer = {
                std::thread::spawn(move || {
                    for range in ranges {
                        if task_tx.send(items[range].to_vec()).is_err() {
                            break;
                        }
                    }
                })
            };

            let mut embedded = 0usize;
            let mut errors = 0usize;
            let mut first_error = None;
            let mut stopped = None;
            while let Ok(result) = result_rx.recv() {
                // Asked between batches, never inside one: a pass over a large configuration runs
                // for hours, and the caller's right to write may not outlive it.
                if should_continue.is_some_and(|keep_going| !keep_going()) {
                    stopped = Some(FenceOutcome::Released);
                    break;
                }
                match result {
                    Ok(pairs) => {
                        let batch_len = pairs.len();
                        let outcome = match Self::fenced_value(apply, || {
                            store.set_chunk_embeddings_with_token_layout(
                                &pairs,
                                Some((embedder.model_id(), dim)),
                                embedder.token_layout_claim(),
                            )
                        }) {
                            Ok(outcome) => outcome,
                            Err(error) => {
                                first_error.get_or_insert(error);
                                break;
                            }
                        };
                        match outcome {
                            FenceOutcome::Applied(()) => {}
                            FenceOutcome::TransientRefusal => {
                                stopped = Some(FenceOutcome::TransientRefusal);
                                break;
                            }
                            FenceOutcome::Superseded => {
                                stopped = Some(FenceOutcome::Superseded);
                                break;
                            }
                            FenceOutcome::Released => {
                                stopped = Some(FenceOutcome::Released);
                                break;
                            }
                        }
                        embedded += batch_len;
                    }
                    Err(e) => {
                        warn!("embedding batch failed after retries: {e}");
                        errors += 1;
                        first_error.get_or_insert(e);
                    }
                }
            }
            // Closed before the joins in every path: a worker parked on a send into a channel
            // nobody reads any more would never finish, and the stop path leaves exactly that.
            drop(result_rx);

            if producer.join().is_err() {
                first_error.get_or_insert_with(|| {
                    crate::EmbeddingFailure::new(crate::EmbeddingFailureCode::EmbeddingFailed)
                        .into()
                });
            }
            for w in workers {
                if w.join().is_err() {
                    first_error.get_or_insert_with(|| {
                        crate::EmbeddingFailure::new(crate::EmbeddingFailureCode::EmbeddingFailed)
                            .into()
                    });
                }
            }
            if let Some(p) = &progress {
                p.phase(IndexPhase::Persisting);
            }

            let (generation, data) = Self::load_embeddings_for_profile(store, embedder, dim)?;
            let index = VectorIndex::build(dim, &data)?;
            // Asked once more before the sidecar: a takeover landing after the last batch would
            // otherwise still leave this pass's index description behind for the new owner.
            let stopped = stopped.or_else(|| {
                should_continue
                    .is_some_and(|keep_going| !keep_going())
                    .then_some(FenceOutcome::Released)
            });
            if let Some(outcome) = stopped {
                // The vectors already written stay — they were written while the caller still had
                // the right to. What is skipped is the persisted sidecar, the one artifact a
                // stopped pass would leave behind for whoever writes this database next; the index
                // itself is still returned, so this process keeps answering semantic queries from
                // what it has.
                warn!(embedded, errors, "embedding pass stopped early; sidecar not persisted");
                return Ok((index, outcome));
            }
            if let Some(error) = first_error {
                let outcome = Self::fenced_value(apply, || Ok(()))?;
                return if matches!(outcome, FenceOutcome::Applied(())) {
                    Err(error)
                } else {
                    Ok((index, outcome))
                };
            }
            let persisted = if let Some(mut prepared) = Self::prepare_built_with_progress(
                store,
                dim,
                Some(embedder),
                &index,
                generation,
                progress,
            ) {
                let outcome = Self::fenced_value(apply, || {
                    Self::install_prepared_built_with_progress(store, &mut prepared, progress)
                })?;
                if matches!(outcome, FenceOutcome::Applied(())) {
                    prepared.finish();
                }
                outcome
            } else {
                FenceOutcome::Applied(())
            };
            match persisted {
                FenceOutcome::Applied(()) => {}
                FenceOutcome::TransientRefusal => {
                    return Ok((index, FenceOutcome::TransientRefusal));
                }
                FenceOutcome::Superseded => return Ok((index, FenceOutcome::Superseded)),
                FenceOutcome::Released => return Ok((index, FenceOutcome::Released)),
            }

            info!(embedded, errors, total_vectors = index.len(), "fused embedding complete");
            Ok((index, FenceOutcome::Applied(())))
        });
        let lifecycle_outcome = match &lifecycle_result {
            Ok((_, FenceOutcome::Applied(()))) if embedding_skipped => Outcome::Skipped,
            Ok((_, FenceOutcome::Applied(()))) => Outcome::Completed,
            Ok((_, FenceOutcome::TransientRefusal)) => Outcome::Refused,
            Ok((_, FenceOutcome::Superseded | FenceOutcome::Released)) => Outcome::Interrupted,
            Err(_) => Outcome::Failed,
        };
        if let Some(token) = progress {
            match &lifecycle_result {
                Ok((_, FenceOutcome::Applied(()))) => token.phase(IndexPhase::Persisting),
                Ok((_, FenceOutcome::Superseded)) => token.finish(IndexPassState::Superseded),
                Ok((_, FenceOutcome::Released)) => token.finish(IndexPassState::Cancelled),
                Ok((_, FenceOutcome::TransientRefusal)) => token.phase(IndexPhase::Persisting),
                _ => token.finish(IndexPassState::Failed),
            }
        }
        lifecycle_batch.finish(lifecycle_outcome);
        lifecycle_result
    }

    /// The files a boot ingest must write, each under the key the store knows it by.
    ///
    /// With a root table configured, the universe is EVERY registered root, walked once through
    /// the shared source-set walk, and the key is decided by the same attribution every other
    /// path uses — the longest matching prefix, not the root the walk entered through. Keying by
    /// the entered root would give a file under a configuration that some extension contains a
    /// second row under that extension's id. De-duplication by key is what keeps one file one
    /// row when roots nest and the walk reaches it twice.
    ///
    /// Without a table the caller is not a workspace daemon but a one-shot indexer (the baseline
    /// publisher, a reference corpus). The contract it gets is the same one stated by
    /// [`Self::set_workspace_root`] — everything found belongs to the configuration — and it is
    /// honoured by giving that caller a one-root table rather than a second way to walk. A walk
    /// of its own would answer "which file is this" differently from the daemon that later reads
    /// the rows: attribution ranks the CANONICAL spelling first, so two names for one file inside
    /// the root are one key to the reader and would have been two to a writer keying by the name
    /// it walked through.
    fn boot_ingest_files(&self, root: &Path) -> Vec<(FileKey, std::path::PathBuf)> {
        let walked: Option<Vec<std::path::PathBuf>> = self
            .workspace_roots
            .as_ref()
            .map(|roots| roots.entries().map(|(_, declared)| declared.to_path_buf()).collect());
        self.boot_ingest_files_over(root, walked.as_deref())
    }

    /// The same projection over a chosen subset of the roots to WALK. Attribution still consults
    /// the whole table: roots may nest, so a file found while walking one root can belong to
    /// another, and keying it by the root the walk entered through would give it a second row.
    fn boot_ingest_files_over(
        &self,
        root: &Path,
        walk: Option<&[std::path::PathBuf]>,
    ) -> Vec<(FileKey, std::path::PathBuf)> {
        let ephemeral;
        let roots = match self.workspace_roots.as_ref() {
            Some(roots) => roots,
            None => {
                ephemeral = WorkspaceRoots::build(root, root, &[]).0;
                &ephemeral
            }
        };
        let owned: Vec<std::path::PathBuf>;
        let declared: &[std::path::PathBuf] = match walk {
            Some(walk) => walk,
            None => {
                owned = roots.entries().map(|(_, declared)| declared.to_path_buf()).collect();
                &owned
            }
        };
        let set = project_model::SourceSet::scan_in_scope(
            declared,
            roots.excluded(),
            roots.user_excluded(),
        );
        let files = Self::files_from_scan(roots, &set);
        self.record_scanned_spellings(&files);
        files.into_iter().map(|file| (file.key, file.walked)).collect()
    }

    /// Record every scanned file's two spellings, best effort.
    ///
    /// The journal is what a removal has and re-attribution does not; a write that fails
    /// costs the fallback behaviour for those spellings, not the ingest — so the walk is
    /// not failed over it (github#192).
    fn record_scanned_spellings(&self, files: &[ScannedWorkspaceFile]) {
        let mut records: Vec<(String, FileKey)> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for file in files {
            for spelling in [&file.walked, &file.canonical] {
                let rendered = spelling.to_string_lossy().into_owned();
                if !rendered.is_empty() && seen.insert(rendered.clone()) {
                    records.push((rendered, file.key.clone()));
                }
            }
        }
        if let Err(error) = self.store.record_path_spellings("code", &records) {
            tracing::warn!("failed to record the walk's path spellings: {error}");
        }
    }

    /// The corpus a walk describes: every source file it reached, under the key its owning root
    /// gives it. Pure — it reaches no further than the set handed in, so the files it names and
    /// the completeness its caller reads off that same set always describe one traversal.
    ///
    /// De-duplication is by key, and that is what keeps one file one row when roots nest and the
    /// walk arrives twice. It does NOT merge two spellings of one file that attribution keeps
    /// apart: a link leaving every root keeps the name it was walked through, because that name
    /// is the only handle its root has on it.
    fn files_from_scan(
        roots: &WorkspaceRoots,
        set: &project_model::SourceSet,
    ) -> Vec<ScannedWorkspaceFile> {
        let mut seen = HashSet::new();
        let mut files = Vec::new();
        for file in &set.files {
            if file.role != project_model::FileRole::Source {
                continue;
            }
            let Some(key) = roots.root_of(&file.walked, &file.canonical) else {
                continue;
            };
            if !seen.insert(key.clone()) {
                continue;
            }
            files.push(ScannedWorkspaceFile {
                key,
                walked: file.walked.clone(),
                canonical: file.canonical.clone(),
            });
        }
        files
    }

    /// Ingest the corpus of a walk the CALLER performed, for a caller that must also judge that
    /// walk's completeness — the baseline publisher, which may not ship a corpus built from a
    /// tree it could not read whole.
    ///
    /// Taking the scan itself, rather than a ready list of files, is the point: a list could have
    /// come from any traversal, and then the verdict its holder consulted would answer for a
    /// corpus some other traversal produced. Here the files are derived from the very value whose
    /// completeness the caller read.
    pub fn ingest_scanned_fts(
        &mut self,
        set: &project_model::SourceSet,
    ) -> Result<FtsIngest, SearchError> {
        let files = {
            let Some(roots) = self.workspace_roots.as_ref() else {
                return Err(SearchError::Index(
                    "ingesting a scanned source set needs a root table: without one there is no \
                     way to say which root a file belongs to"
                        .into(),
                ));
            };
            Self::files_from_scan(roots, set)
        };
        self.record_scanned_spellings(&files);
        let files: Vec<(FileKey, std::path::PathBuf)> =
            files.into_iter().map(|file| (file.key, file.walked)).collect();
        self.ingest_files_fts(&files)
    }

    fn observed_file_hash(
        &self,
        key: &FileKey,
        hash: &[u8],
    ) -> Result<(Option<Vec<u8>>, Reason), SearchError> {
        let stored = self.store.file_hash(&key.root_id, &key.path);
        let reason = lifecycle::hash_reason(
            stored.as_ref().map(|value| value.as_deref()).map_err(|_| ()),
            hash,
        );
        lifecycle::decision(
            self.store.db_path(),
            key,
            reason,
            stored.as_ref().ok().and_then(|value| value.as_deref()),
            Some(hash),
        );
        stored.map(|value| (value, reason))
    }

    fn prepare_boot_file(
        &self,
        key: &FileKey,
        file_path: &Path,
        with_graph_context: bool,
    ) -> Result<PreparedBootFile, SearchError> {
        let content = match std::fs::read_to_string(file_path) {
            Ok(content) => content,
            Err(error) => {
                lifecycle::decision(self.store.db_path(), key, Reason::ReadError, None, None);
                warn!(?file_path, "failed to read file: {error}");
                return Ok(PreparedBootFile::Unread);
            }
        };
        let hash = blake3::hash(content.as_bytes());
        let (stored_hash, reason) = self.observed_file_hash(key, hash.as_bytes())?;
        let had_prior = match stored_hash {
            Some(stored_hash) if stored_hash == hash.as_bytes() => {
                return Ok(PreparedBootFile::Unchanged);
            }
            Some(_) => true,
            None => false,
        };
        let provider =
            if with_graph_context { self.graph_context_provider.as_deref() } else { None };
        let (documents, context_owed) = crate::document::prepare_indexed_documents(
            key,
            &content,
            provider,
            self.embedder.as_ref(),
        )?;
        let (chunks, rendered_contexts, source_spans) =
            crate::document::chunks_contexts_and_spans(&documents);
        if chunks.is_empty() {
            return Ok(if had_prior {
                PreparedBootFile::Remove(key.clone(), reason)
            } else {
                PreparedBootFile::Unchanged
            });
        }
        let graph_contexts = with_graph_context.then_some(rendered_contexts);
        Ok(PreparedBootFile::Reindex {
            key: key.clone(),
            reason,
            hash: hash.as_bytes().to_vec(),
            chunks,
            graph_contexts,
            context_owed,
            source_spans,
        })
    }

    fn apply_prepared_boot_file_checkpointed(
        &mut self,
        prepared: &PreparedBootFile,
        checkpoint: &mut dyn FnMut() -> ControlFlow<()>,
    ) -> ControlFlow<(), Result<(), SearchError>> {
        if matches!(&prepared, PreparedBootFile::Reindex { .. }) {
            self.invalidate_semantic_coverage();
        }
        match prepared {
            PreparedBootFile::Remove(key, reason) => {
                if checkpoint().is_break() {
                    return ControlFlow::Break(());
                }
                ControlFlow::Continue(lifecycle::with_reason(*reason, || {
                    self.store.remove_file_with_token_layout(
                        &key.root_id,
                        &key.path,
                        "code",
                        self.embedder.as_ref().and_then(Embedder::token_layout_claim),
                    )
                }))
            }
            PreparedBootFile::Reindex {
                key,
                hash,
                chunks,
                reason,
                graph_contexts: Some(contexts),
                context_owed,
                source_spans,
            } => {
                match lifecycle::with_reason(*reason, || {
                    self.store.reindex_file_in_collection_checkpointed_with_source_spans(
                        &key.root_id,
                        &key.path,
                        hash,
                        "code",
                        chunks,
                        None,
                        Some(contexts),
                        self.embedder
                            .as_ref()
                            .map(|embedder| (embedder.model_id(), embedder.dim())),
                        self.embedder.as_ref().and_then(Embedder::token_layout_claim),
                        source_spans,
                        checkpoint,
                    )
                }) {
                    // Marked only once the rows exist: a refresh that ran in between would find
                    // no chunks and clear a mark placed ahead of them.
                    Ok(ControlFlow::Continue(_)) if *context_owed => {
                        ControlFlow::Continue(self.owe_graph_contexts([key]))
                    }
                    Ok(ControlFlow::Continue(_)) => ControlFlow::Continue(Ok(())),
                    Ok(ControlFlow::Break(())) => ControlFlow::Break(()),
                    Err(error) => ControlFlow::Continue(Err(error)),
                }
            }
            PreparedBootFile::Reindex {
                key,
                hash,
                chunks,
                reason,
                graph_contexts: None,
                source_spans,
                ..
            } => {
                match lifecycle::with_reason(*reason, || {
                    self.store.reindex_file_in_collection_checkpointed_with_source_spans(
                        &key.root_id,
                        &key.path,
                        hash,
                        "code",
                        chunks,
                        None,
                        None,
                        self.embedder
                            .as_ref()
                            .map(|embedder| (embedder.model_id(), embedder.dim())),
                        self.embedder.as_ref().and_then(Embedder::token_layout_claim),
                        source_spans,
                        checkpoint,
                    )
                }) {
                    Ok(ControlFlow::Continue(_)) => ControlFlow::Continue(Ok(())),
                    Ok(ControlFlow::Break(())) => ControlFlow::Break(()),
                    Err(error) => ControlFlow::Continue(Err(error)),
                }
            }
            PreparedBootFile::Unchanged | PreparedBootFile::Unread => ControlFlow::Continue(Ok(())),
        }
    }

    fn ingest_boot_files_fenced<F>(
        &mut self,
        files: &[(FileKey, PathBuf)],
        with_graph_context: bool,
        apply: &mut F,
    ) -> Result<FenceOutcome<FtsIngest>, SearchError>
    where
        F: FnMut(
            &mut dyn FnMut(
                &mut dyn FnMut() -> ControlFlow<()>,
            ) -> ControlFlow<(), Result<(), SearchError>>,
        ) -> FenceOutcome<Result<(), SearchError>>,
    {
        let lifecycle_batch = Batch::new(self.store.db_path(), Reason::ExplicitRebuild);
        let lifecycle_result = lifecycle_batch.context().in_scope(|| {
            let mut result = FtsIngest { indexed: 0, unread: 0 };
            for (key, path) in files {
                let prepared = self.prepare_boot_file(key, path, with_graph_context)?;
                match prepared {
                    PreparedBootFile::Unchanged => continue,
                    PreparedBootFile::Unread => {
                        result.unread += 1;
                        continue;
                    }
                    prepared => {
                        match Self::fenced_checkpointed_value(apply, |checkpoint| {
                            self.apply_prepared_boot_file_checkpointed(&prepared, checkpoint)
                        })? {
                            FenceOutcome::Applied(()) => {}
                            FenceOutcome::TransientRefusal => {
                                return Ok(FenceOutcome::TransientRefusal);
                            }
                            FenceOutcome::Superseded => return Ok(FenceOutcome::Superseded),
                            FenceOutcome::Released => return Ok(FenceOutcome::Released),
                        }
                        result.indexed += 1;
                    }
                }
            }
            Ok(FenceOutcome::Applied(result))
        });
        let lifecycle_outcome = match &lifecycle_result {
            Ok(FenceOutcome::Applied(_)) => Outcome::Completed,
            Ok(FenceOutcome::TransientRefusal) => Outcome::Refused,
            Ok(FenceOutcome::Superseded | FenceOutcome::Released) => Outcome::Cancelled,
            Err(_) => Outcome::Failed,
        };
        lifecycle_batch.finish(lifecycle_outcome);
        lifecycle_result
    }

    /// Index workspace files for *deferred* embedding: chunk each changed file, attach
    /// its graph context via the configured provider, and persist chunk + FTS rows with a
    /// NULL embedding. The vectors are filled later by
    /// [`Self::embed_pending_chunks_standalone`], which reads back the stored graph
    /// context. Unlike [`Self::index_directory_fts`] this preserves graph context, so the
    /// deferred embeddings are graph-enriched whenever a provider is set — matching what
    /// the synchronous [`Self::index_directory`] would have produced, without blocking on
    /// the embed. Returns the number of files (re)indexed.
    pub fn index_directory_deferred(&mut self, root: &Path) -> Result<usize, SearchError> {
        match self.index_directory_deferred_fenced(root, Self::permit_checkpointed_apply)? {
            FenceOutcome::Applied(indexed) => Ok(indexed),
            FenceOutcome::TransientRefusal | FenceOutcome::Superseded | FenceOutcome::Released => {
                unreachable!("the permit-all ingest fence cannot refuse")
            }
        }
    }

    pub fn index_directory_deferred_fenced<F>(
        &mut self,
        root: &Path,
        mut apply: F,
    ) -> Result<FenceOutcome<usize>, SearchError>
    where
        F: FnMut(
            &mut dyn FnMut(
                &mut dyn FnMut() -> ControlFlow<()>,
            ) -> ControlFlow<(), Result<(), SearchError>>,
        ) -> FenceOutcome<Result<(), SearchError>>,
    {
        let bsl_files = self.boot_ingest_files(root);
        info!(total_files = bsl_files.len(), "scanning BSL files (deferred embedding)");
        let result = match self.ingest_boot_files_fenced(&bsl_files, true, &mut apply)? {
            FenceOutcome::Applied(result) => result,
            FenceOutcome::TransientRefusal => return Ok(FenceOutcome::TransientRefusal),
            FenceOutcome::Superseded => return Ok(FenceOutcome::Superseded),
            FenceOutcome::Released => return Ok(FenceOutcome::Released),
        };
        info!(
            indexed = result.indexed,
            total_chunks = self.store.chunk_count()?,
            "deferred indexing complete"
        );
        Ok(FenceOutcome::Applied(result.indexed))
    }

    pub fn index_directory_fts(&mut self, root: &Path) -> Result<usize, SearchError> {
        match self.index_directory_fts_fenced(root, Self::permit_checkpointed_apply)? {
            FenceOutcome::Applied(indexed) => Ok(indexed),
            FenceOutcome::TransientRefusal | FenceOutcome::Superseded | FenceOutcome::Released => {
                unreachable!("the permit-all ingest fence cannot refuse")
            }
        }
    }

    pub fn index_directory_fts_fenced<F>(
        &mut self,
        root: &Path,
        mut apply: F,
    ) -> Result<FenceOutcome<usize>, SearchError>
    where
        F: FnMut(
            &mut dyn FnMut(
                &mut dyn FnMut() -> ControlFlow<()>,
            ) -> ControlFlow<(), Result<(), SearchError>>,
        ) -> FenceOutcome<Result<(), SearchError>>,
    {
        let bsl_files = self.boot_ingest_files(root);
        Ok(match self.ingest_boot_files_fenced(&bsl_files, false, &mut apply)? {
            FenceOutcome::Applied(result) => FenceOutcome::Applied(result.indexed),
            FenceOutcome::TransientRefusal => FenceOutcome::TransientRefusal,
            FenceOutcome::Superseded => FenceOutcome::Superseded,
            FenceOutcome::Released => FenceOutcome::Released,
        })
    }

    /// Index only the registered roots that have no rows at all yet.
    ///
    /// A warm store skips re-indexing to keep a restart cheap, but "warm" is a per-ROOT fact: a
    /// root declared while the daemon was down has nothing stored, and skipping it would leave it
    /// out of the index until someone edited a file in it. Roots that already have rows are not
    /// walked, read or hashed here, so the restart stays as cheap as it was.
    pub fn index_unindexed_roots_fts(&mut self) -> Result<usize, SearchError> {
        match self.index_unindexed_roots_fts_fenced(Self::permit_checkpointed_apply)? {
            FenceOutcome::Applied(indexed) => Ok(indexed),
            FenceOutcome::TransientRefusal | FenceOutcome::Superseded | FenceOutcome::Released => {
                unreachable!("the permit-all ingest fence cannot refuse")
            }
        }
    }

    pub fn index_unindexed_roots_fts_fenced<F>(
        &mut self,
        mut apply: F,
    ) -> Result<FenceOutcome<usize>, SearchError>
    where
        F: FnMut(
            &mut dyn FnMut(
                &mut dyn FnMut() -> ControlFlow<()>,
            ) -> ControlFlow<(), Result<(), SearchError>>,
        ) -> FenceOutcome<Result<(), SearchError>>,
    {
        let indexed_roots: HashSet<String> = self
            .store
            .all_files_in_collection("code")?
            .into_iter()
            .map(|(key, _hash)| key.root_id)
            .collect();
        let Some(roots) = self.workspace_roots.as_ref() else {
            return Ok(FenceOutcome::Applied(0));
        };
        // Only the unindexed roots are WALKED: a warm root must not be traversed, canonicalised
        // or stat-ed at all, or the per-root skip would cost exactly what it exists to avoid.
        let cold: Vec<std::path::PathBuf> = roots
            .entries()
            .filter(|(id, _)| !indexed_roots.contains(*id))
            .map(|(_, declared)| declared.to_path_buf())
            .collect();
        if cold.is_empty() {
            return Ok(FenceOutcome::Applied(0));
        }
        let files: Vec<(FileKey, std::path::PathBuf)> = self
            .boot_ingest_files_over(Path::new(""), Some(&cold))
            .into_iter()
            .filter(|(key, _)| !indexed_roots.contains(&key.root_id))
            .collect();
        if files.is_empty() {
            return Ok(FenceOutcome::Applied(0));
        }
        Ok(match self.ingest_boot_files_fenced(&files, false, &mut apply)? {
            FenceOutcome::Applied(result) => FenceOutcome::Applied(result.indexed),
            FenceOutcome::TransientRefusal => FenceOutcome::TransientRefusal,
            FenceOutcome::Superseded => FenceOutcome::Superseded,
            FenceOutcome::Released => FenceOutcome::Released,
        })
    }

    fn ingest_files_fts(
        &mut self,
        bsl_files: &[(FileKey, std::path::PathBuf)],
    ) -> Result<FtsIngest, SearchError> {
        let lifecycle_batch = Batch::new(self.store.db_path(), Reason::ExplicitRebuild);
        let lifecycle_result = lifecycle_batch.context().in_scope(|| {
            info!(total_files = bsl_files.len(), "scanning BSL files (FTS-only)");

            let mut indexed = 0;
            let mut unread = 0;
            for (key, file_path) in bsl_files {
                let content = match std::fs::read_to_string(file_path) {
                    Ok(c) => c,
                    Err(e) => {
                        lifecycle::decision(
                            self.store.db_path(),
                            key,
                            Reason::ReadError,
                            None,
                            None,
                        );
                        warn!(?file_path, "failed to read file: {e}");
                        unread += 1;
                        continue;
                    }
                };

                let hash = blake3::hash(content.as_bytes());
                let rel_path = key.path.clone();

                let (stored_hash, reason) = self.observed_file_hash(key, hash.as_bytes())?;
                let had_prior = match stored_hash {
                    Some(stored_hash) => {
                        if stored_hash == hash.as_bytes() {
                            continue;
                        }
                        true
                    }
                    None => false,
                };

                let (documents, context_owed) = crate::document::prepare_indexed_documents(
                    key,
                    &content,
                    self.graph_context_provider.as_deref(),
                    self.embedder.as_ref(),
                )?;
                let (chunks, graph_contexts, source_spans) =
                    crate::document::chunks_contexts_and_spans(&documents);
                if chunks.is_empty() {
                    // The content changed (its hash mismatched above) but now yields no chunks — the
                    // file was gutted to comments/blank while the daemon was down. Any prior chunks are
                    // now stale; leaving them makes a Clean boot false-clean (the vanished symbol is
                    // served forever), and the deletion reconcile does NOT cover this — the file still
                    // EXISTS on disk, so it is never "gone". Remove the stored rows. A file that was
                    // never indexed has nothing to remove and must not gain a spurious zero-chunk row,
                    // so only prior-stored files are touched.
                    if had_prior {
                        lifecycle::with_reason(reason, || {
                            self.store.remove_file_with_token_layout(
                                &key.root_id,
                                &rel_path,
                                "code",
                                self.embedder.as_ref().and_then(Embedder::token_layout_claim),
                            )
                        })?;
                        indexed += 1;
                    }
                    continue;
                }

                match lifecycle::with_reason(reason, || {
                    let mut checkpoint = || ControlFlow::Continue(());
                    self.store.reindex_file_in_collection_checkpointed_with_source_spans(
                        &key.root_id,
                        &rel_path,
                        hash.as_bytes(),
                        "code",
                        &chunks,
                        None,
                        Some(&graph_contexts),
                        self.embedder
                            .as_ref()
                            .map(|embedder| (embedder.model_id(), embedder.dim())),
                        self.embedder.as_ref().and_then(Embedder::token_layout_claim),
                        &source_spans,
                        &mut checkpoint,
                    )
                })? {
                    ControlFlow::Continue(_) => {}
                    ControlFlow::Break(()) => {
                        unreachable!("permit-all ingest checkpoint cannot cancel")
                    }
                }
                // Marked only once the rows exist, so a refresh in between cannot clear it.
                if context_owed {
                    self.owe_graph_contexts([key])?;
                }
                indexed += 1;
            }

            info!(
                indexed,
                unread,
                total_chunks = self.store.chunk_count()?,
                "FTS indexing complete"
            );
            Ok(FtsIngest { indexed, unread })
        });
        let lifecycle_outcome =
            if lifecycle_result.is_ok() { Outcome::Completed } else { Outcome::Failed };
        lifecycle_batch.finish(lifecycle_outcome);
        lifecycle_result
    }

    pub fn index_documents(
        &mut self,
        collection: &str,
        virtual_path: &str,
        version_hash: &[u8],
        documents: &[Document],
        progress: Option<&Arc<IndexProgress>>,
    ) -> Result<usize, SearchError> {
        let mut pass = progress.map(IndexProgress::begin_pass);
        let token = pass.as_ref().map(ActivePass::token);
        let progress = token.as_ref();
        let lifecycle_batch = Batch::new(self.store.db_path(), Reason::ExplicitRebuild);
        let lifecycle_result = lifecycle_batch.context().in_scope(|| {
            self.invalidate_reference_stamp(collection)?;
            let (stored_hash, reason) =
                self.observed_file_hash(&FileKey::configuration(virtual_path), version_hash)?;
            if let Some(stored_hash) = stored_hash {
                if stored_hash == version_hash {
                    info!(collection, documents = documents.len(), "documents unchanged, skipping");
                    return Ok(0);
                }
            }

            info!(collection, documents = documents.len(), "indexing documents");

            let embeddings = self.embed_documents(documents, progress)?;
            lifecycle::with_reason(reason, || {
                self.store.reindex_documents_with_identity(
                    collection,
                    virtual_path,
                    version_hash,
                    documents,
                    embeddings.as_deref(),
                    self.embedder
                        .as_ref()
                        .map(|embedder| (embedder.model_id(), embedder.dimension())),
                )
            })?;
            if embeddings.is_some() {
                self.index = Self::build_persisted_index_with_progress(
                    &self.store,
                    self.dim,
                    self.embedder.as_ref(),
                    progress,
                )?;
                self.observe_live_index(
                    "live_index_replaced",
                    Reason::ExplicitRebuild,
                    Outcome::Completed,
                );
            }

            let count = documents.len();
            info!(collection, count, "document indexing complete");
            Ok(count)
        });
        let lifecycle_outcome =
            if lifecycle_result.is_ok() { Outcome::Completed } else { Outcome::Failed };
        lifecycle_batch.finish(lifecycle_outcome);
        if let Some(pass) = pass.as_mut() {
            pass.finish(if lifecycle_result.is_ok() {
                IndexPassState::Ready
            } else {
                IndexPassState::Failed
            });
        }
        lifecycle_result
    }

    /// Метка записанного справочного корпуса: сам корпус плюс эмбеддер, которым он
    /// записан.
    ///
    /// Тождество эмбеддера входит в метку, потому что корпус без векторов и корпус,
    /// векторизованный другой моделью или размерностью, для семантического поиска
    /// разные вещи, а текст документов у них один и тот же.
    pub(crate) fn reference_stamp(
        fingerprint: &str,
        model: Option<&str>,
        dim: Option<usize>,
    ) -> String {
        match (model, dim) {
            (Some(model), Some(dim)) => format!("{fingerprint}:{model}:{dim}"),
            (Some(model), None) => format!("{fingerprint}:{model}:default"),
            _ => format!("{fingerprint}:fts"),
        }
    }

    /// Запись в коллекцию мимо [`Self::replace_reference_collection_if_stale`] снимает
    /// метку корпуса: она описывает содержимое, которого после такой записи в коллекции
    /// уже нет, и уцелевшая метка заставила бы следующую публикацию молча не состояться.
    fn invalidate_reference_stamp(&mut self, collection: &str) -> Result<(), SearchError> {
        self.store.clear_reference_collection_fingerprint(collection)?;
        if collection == "platform" {
            self.loaded_reference_fingerprint = None;
        }
        Ok(())
    }

    pub fn replace_reference_collection_if_stale(
        &mut self,
        collection: &str,
        virtual_path: &str,
        fingerprint: &str,
        documents: &[Document],
        progress: Option<&Arc<IndexProgress>>,
    ) -> Result<ReferenceCollectionReplaceOutcome, SearchError> {
        let mut pass = progress.map(IndexProgress::begin_pass);
        let token = pass.as_ref().map(ActivePass::token);
        let progress = token.as_ref();
        let result = (|| {
            let stamp = Self::reference_stamp(
                fingerprint,
                self.embedding_storage_identity(),
                self.embedding_dimension(),
            );
            let committed = self.store.reference_collection_fingerprint(collection)?;
            if committed.as_deref() == Some(stamp.as_str()) {
                if self.loaded_reference_fingerprint != committed && self.embedder.is_some() {
                    self.index = Self::build_persisted_index_with_progress(
                        &self.store,
                        self.dim,
                        self.embedder.as_ref(),
                        progress,
                    )?;
                    self.observe_live_index(
                        "live_index_replaced",
                        Reason::ExplicitRebuild,
                        Outcome::Completed,
                    );
                }
                self.loaded_reference_fingerprint = committed;
                return Ok(ReferenceCollectionReplaceOutcome {
                    committed_fingerprint: stamp,
                    written: false,
                    embedding_failure: None,
                });
            }
            // A corpus that could not be vectorised is still worth publishing lexically: refusing
            // to publish it costs the caller find_docs entirely, and the stamp above records that
            // the published corpus has no vectors — so a later boot with a reachable embedder sees
            // a stale stamp and embeds it then. Without that stamp this degradation would be
            // permanent, which is why it is safe only now.
            let (embeddings, embedding_failure) = match self.embed_documents(documents, progress) {
                Ok(embeddings) => (embeddings, None),
                Err(error) => {
                    warn!(%error, collection, "reference corpus embedding failed, publishing lexical-only");
                    if let Some(token) = progress {
                        token.finish(IndexPassState::Failed);
                    }
                    (
                        None,
                        Some(error.embedding_failure().unwrap_or_else(|| {
                            crate::EmbeddingFailure::new(
                                crate::EmbeddingFailureCode::EmbeddingFailed,
                            )
                        })),
                    )
                }
            };
            let stamp = if embeddings.is_some() {
                stamp
            } else {
                Self::reference_stamp(fingerprint, None, None)
            };
            let outcome = self.store.replace_reference_collection_with_identity(
                collection,
                virtual_path,
                &stamp,
                documents,
                embeddings.as_deref(),
                self.embedder.as_ref().map(|embedder| (embedder.model_id(), embedder.dimension())),
            )?;
            if self.loaded_reference_fingerprint.as_deref()
                != Some(outcome.committed_fingerprint.as_str())
                && self.embedder.is_some()
            {
                self.index = Self::build_persisted_index_with_progress(
                    &self.store,
                    self.dim,
                    self.embedder.as_ref(),
                    progress,
                )?;
                self.observe_live_index(
                    "live_index_replaced",
                    Reason::ExplicitRebuild,
                    Outcome::Completed,
                );
            }
            self.loaded_reference_fingerprint = Some(outcome.committed_fingerprint.clone());
            Ok(ReferenceCollectionReplaceOutcome {
                committed_fingerprint: outcome.committed_fingerprint,
                written: outcome.written,
                embedding_failure,
            })
        })();
        if let Some(pass) = pass.as_mut() {
            pass.finish(if result.is_ok() {
                IndexPassState::Ready
            } else {
                IndexPassState::Failed
            });
        }
        result
    }

    fn observe_embedding_dimensions(
        vectors: &[Vec<f32>],
        dim: usize,
        progress: Option<&IndexPassToken>,
    ) {
        if vectors.iter().any(|vector| vector.len() != dim) {
            if let Some(token) = progress {
                token.finish(IndexPassState::Failed);
            }
        }
    }

    fn embed_documents(
        &self,
        documents: &[Document],
        progress: Option<&IndexPassToken>,
    ) -> Result<Option<Vec<Vec<f32>>>, SearchError> {
        let Some(embedder) = &self.embedder else { return Ok(None) };
        let texts: Vec<String> = documents.iter().map(|d| d.body.clone()).collect();
        let dim = self.dim;
        let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
        let ranges = embedder.batch_ranges(&refs, self.batch_size)?;
        let total_batches = ranges.len();

        if let Some(p) = progress {
            p.set_totals(1, texts.len(), total_batches);
        }

        let concurrency = self.concurrency.min(total_batches.max(1));

        let (task_tx, task_rx) =
            crossbeam_channel::bounded::<(usize, Vec<String>)>(concurrency * 2);
        let (result_tx, result_rx) = crossbeam_channel::bounded::<(
            usize,
            Result<Vec<Vec<f32>>, SearchError>,
        )>(concurrency * 2);

        let workers: Vec<std::thread::JoinHandle<()>> = (0..concurrency)
            .map(|_| {
                let rx = task_rx.clone();
                let tx = result_tx.clone();
                let emb = embedder.clone();
                let prog = progress.cloned();

                std::thread::spawn(move || {
                    while let Ok((idx, batch)) = rx.recv() {
                        let refs: Vec<&str> = batch.iter().map(|s| s.as_str()).collect();
                        let result = emb.embed_batch(&refs);
                        if let Ok(vectors) = &result {
                            Self::observe_embedding_dimensions(vectors, dim, prog.as_ref());
                        }
                        if let (Ok(_), Some(p)) = (&result, &prog) {
                            p.advance(batch.len(), 1);
                        }
                        if tx.send((idx, result)).is_err() {
                            break;
                        }
                    }
                })
            })
            .collect();

        drop(task_rx);
        drop(result_tx);

        let producer = std::thread::spawn(move || {
            for (idx, range) in ranges.into_iter().enumerate() {
                if task_tx.send((idx, texts[range].to_vec())).is_err() {
                    break;
                }
            }
        });

        // Отказ батча не выходит из функции прямо здесь: незакрытый прогресс переживёт
        // вызов и навсегда оставит статус поиска в «идёт индексация», а воркеры останутся
        // неприсоединёнными. Ошибка запоминается, приём результатов прекращается, и
        // уборка ниже — общая для успеха и отказа.
        let mut results: Vec<(usize, Vec<Vec<f32>>)> = Vec::with_capacity(total_batches);
        let mut failure: Option<SearchError> = None;
        while let Ok((idx, result)) = result_rx.recv() {
            match result {
                Ok(embeddings) => results.push((idx, embeddings)),
                Err(error) => {
                    failure.get_or_insert(error);
                    break;
                }
            }
        }
        drop(result_rx);

        if producer.join().is_err() {
            failure.get_or_insert_with(|| {
                crate::EmbeddingFailure::new(crate::EmbeddingFailureCode::EmbeddingFailed).into()
            });
        }
        for w in workers {
            if w.join().is_err() {
                failure.get_or_insert_with(|| {
                    crate::EmbeddingFailure::new(crate::EmbeddingFailureCode::EmbeddingFailed)
                        .into()
                });
            }
        }

        if let Some(p) = progress {
            p.phase(IndexPhase::Persisting);
        }

        if let Some(error) = failure {
            return Err(error);
        }

        if results.len() != total_batches {
            return Err(
                crate::EmbeddingFailure::new(crate::EmbeddingFailureCode::EmbeddingFailed).into()
            );
        }
        results.sort_by_key(|(i, _)| *i);
        let all_embeddings: Vec<Vec<f32>> =
            results.into_iter().flat_map(|(_, embs)| embs).collect();
        Ok(Some(all_embeddings))
    }

    pub fn has_semantic(&self) -> bool {
        self.embedder.is_some()
    }

    pub fn embedding_model(&self) -> Option<&str> {
        self.embedder.as_ref().map(Embedder::model)
    }

    pub fn embedding_storage_identity(&self) -> Option<&str> {
        self.embedder.as_ref().map(Embedder::storage_identity)
    }

    /// Whether persisted local vectors may be used with this engine's embedding profile.
    /// A matching profile claim is required for profile identities; an unclaimed legacy
    /// wire-model id remains accepted by the store's compatibility rule.
    pub fn embedding_profile_matches(&self) -> Result<bool, SearchError> {
        let Some(embedder) = &self.embedder else { return Ok(false) };
        self.store.embedding_profile_matches(embedder.storage_identity(), embedder.dim())
    }

    pub fn embedding_profile(&self) -> Option<(&str, &str, usize)> {
        self.embedder.as_ref().and_then(|embedder| {
            (embedder.storage_identity() != embedder.model()).then_some((
                embedder.model(),
                embedder.storage_identity(),
                embedder.dim(),
            ))
        })
    }

    pub fn embedding_dimension(&self) -> Option<usize> {
        self.embedder.as_ref().map(Embedder::dim)
    }

    pub fn embed_query(&self, query: &str) -> Result<Vec<f32>, SearchError> {
        let embedder = self.embedder.as_ref().ok_or_else(|| {
            SearchError::Index(
                "semantic search not available: configure EMBEDDING_URL to enable embeddings"
                    .to_owned(),
            )
        })?;
        embedder.embed(query)
    }

    /// The CONFIGURATION root: the directory every stored path with the reserved
    /// configuration id is spelled against, and the base a caller resolves a hit's relative path
    /// with. Deliberately not the workspace directory of the root table — a configuration
    /// commonly sits in a subdirectory of the project, and the table's workspace exists to make
    /// root identifiers relative, not to resolve paths.
    pub fn configuration_root(&self) -> Option<&std::path::Path> {
        self.workspace_roots.as_ref().and_then(WorkspaceRoots::configuration)
    }

    /// The engine's root table, for a caller that must scan or resolve keys off
    /// the engine lock (the standalone overlay prime).
    pub fn workspace_roots(&self) -> Option<&WorkspaceRoots> {
        self.workspace_roots.as_ref()
    }

    pub fn workspace_roots_epoch(&self) -> u64 {
        self.workspace_roots_epoch
    }

    /// Point the engine at a workspace whose only source root is the workspace
    /// directory itself. Every file found under it is the configuration's, which
    /// is what a caller with no project model to consult can honestly say.
    pub fn set_workspace_root(&mut self, workspace_root: impl Into<std::path::PathBuf>) {
        let workspace_root = workspace_root.into();
        let (roots, _) = WorkspaceRoots::build(&workspace_root, &workspace_root, &[]);
        self.set_workspace_roots(roots);
    }

    /// Install the complete root table during boot. Live callers must use the transition API;
    /// silently assigning another table would leave every root-keyed carrier behind.
    pub fn initialize_workspace_roots(&mut self, roots: WorkspaceRoots) -> Result<(), SearchError> {
        if self.workspace_roots.is_some() {
            return Err(SearchError::Index(
                "workspace roots are already initialized; use a live transition".to_owned(),
            ));
        }
        self.workspace_roots = Some(roots);
        self.workspace_roots_epoch = 1;
        Ok(())
    }

    /// Compatibility entry point for boot and test callers being migrated to the explicit
    /// two-phase API. A live replacement is best-effort: any incomplete scan, superseded plan or
    /// apply failure leaves the old observable state serving and is logged rather than panicking
    /// the daemon. Production live orchestration uses [`Self::workspace_roots_transition_seed`]
    /// directly so it can retain and retry the obligation and act on embedding signals.
    pub fn set_workspace_roots(&mut self, roots: WorkspaceRoots) {
        if self.workspace_roots.is_none() {
            if let Err(error) = self.initialize_workspace_roots(roots) {
                warn!("failed to initialize compatibility workspace roots: {error}");
            }
            return;
        }
        let plan = match self
            .workspace_roots_transition_seed(roots)
            .and_then(WorkspaceRootsTransitionSeed::plan)
        {
            Ok(plan) => plan,
            Err(error) => {
                warn!("compatibility workspace root transition was not planned: {error}");
                return;
            }
        };
        let validated = match plan.revalidate() {
            Ok(Some(validated)) => validated,
            Ok(None) => {
                warn!("compatibility workspace root transition was superseded; old roots retained");
                return;
            }
            Err(error) => {
                warn!("compatibility workspace root transition validation failed: {error}");
                return;
            }
        };
        match self.apply_validated_workspace_roots_transition(validated) {
            Ok(WorkspaceRootsTransitionOutcome::Applied {
                pending_collection_embeddings,
                pending_overlay_embeddings,
                ..
            }) => {
                if pending_collection_embeddings || pending_overlay_embeddings {
                    warn!(
                        pending_collection_embeddings,
                        pending_overlay_embeddings,
                        "compatibility workspace root transition requires an embedding pass; \
                         live orchestration must consume the explicit transition outcome"
                    );
                }
            }
            Ok(WorkspaceRootsTransitionOutcome::Unchanged) => {}
            Ok(WorkspaceRootsTransitionOutcome::Superseded) => {
                warn!("compatibility workspace root transition was superseded; old roots retained");
            }
            Err(error) => {
                warn!(
                    "compatibility workspace root transition failed; old roots retained: {error}"
                );
            }
        }
    }

    /// Capture the cheap inputs of a two-phase live root transition.
    pub fn workspace_roots_transition_seed(
        &self,
        next_roots: WorkspaceRoots,
    ) -> Result<WorkspaceRootsTransitionSeed, SearchError> {
        let old_roots = self.workspace_roots.clone().ok_or_else(|| {
            SearchError::Index("workspace roots must be initialized before transition".to_owned())
        })?;
        let manifest = if self.serves_external_baseline {
            self.dispatched_manifest_fingerprints()?.unwrap_or_default()
        } else {
            HashMap::new()
        };
        let overlay_epoch = self
            .workspace_overlay_cache
            .lock()
            .map_err(|error| {
                SearchError::Index(format!("workspace overlay cache lock error: {error}"))
            })?
            .transition_epoch();
        Ok(WorkspaceRootsTransitionSeed {
            epoch: self.workspace_roots_epoch,
            overlay_epoch,
            old_roots,
            next_roots,
            serves_external_baseline: self.serves_external_baseline,
            manifest,
            graph_context_provider: self.graph_context_provider.clone(),
            embedder: self.embedder.clone(),
        })
    }

    /// Prepare the cache and HNSW candidates without an ownership fence. `None` means a live
    /// input moved past the validated filesystem snapshot; no candidate is published.
    pub fn stage_validated_workspace_roots_transition(
        &self,
        mut validated: ValidatedWorkspaceRootsTransitionPlan,
    ) -> Result<Option<ValidatedWorkspaceRootsTransitionPlan>, SearchError> {
        let plan = &validated.plan;
        if self.workspace_roots_epoch != plan.epoch
            || self.workspace_roots.as_ref() != Some(&plan.old_roots)
            || self.serves_external_baseline != plan.serves_external_baseline
        {
            return Ok(None);
        }
        if plan.old_roots == plan.next_roots {
            return Ok(Some(validated));
        }
        if plan.serves_external_baseline {
            let live_manifest = self.dispatched_manifest_fingerprints()?.unwrap_or_default();
            if live_manifest != plan.manifest {
                return Ok(None);
            }
        }
        let carriers = self.carrier_keys()?;
        let cache = self.workspace_overlay_cache.lock().map_err(|error| {
            SearchError::Index(format!("workspace overlay cache lock error: {error}"))
        })?;
        if cache.transition_epoch() != plan.overlay_epoch {
            return Ok(None);
        }
        let mut known = carriers.all_keys();
        known.extend(cache.root_keyed_keys());
        drop(cache);

        // A root id is the durable identity. Its declared/canonical directory may move while
        // the `(root_id,path)` key space and its content remain unchanged; that move must not
        // evict chunks or vectors. Only ids entering or leaving the table change a key space.
        let old_ids: HashSet<String> = plan.old_roots.ids().map(str::to_owned).collect();
        let next_ids: HashSet<String> = plan.next_roots.ids().map(str::to_owned).collect();
        let changed_ids: HashSet<String> =
            old_ids.symmetric_difference(&next_ids).cloned().collect();
        // A rebound id keeps rows only for keys whose new bytes were compared with the stored
        // ones. An unread key has no bytes to compare, and under an external baseline the stored
        // hash is the baseline's while a local edit lives in the overlay, so neither may inherit
        // what the old directory put there.
        let rebound_ids = plan.old_roots.rebound_root_ids(&plan.next_roots);
        let readable_keys: HashSet<FileKey> =
            plan.files.iter().map(|file| file.key.clone()).collect();
        let unread_keys: HashSet<FileKey> =
            plan.unread_files.iter().map(|file| file.key.clone()).collect();
        let present_keys: HashSet<FileKey> = readable_keys.union(&unread_keys).cloned().collect();

        let mut cleanup: HashSet<FileKey> = known
            .iter()
            .filter(|key| changed_ids.contains(&key.root_id) || !present_keys.contains(*key))
            .cloned()
            .chain(unread_keys.iter().filter(|key| rebound_ids.contains(&key.root_id)).cloned())
            .collect();
        let mut affected_files = Vec::new();
        let mut rebuilt = 0;
        let mut added = 0;
        for file in &plan.files {
            let stored_hash = self.store.file_hash(&file.key.root_id, &file.key.path)?;
            let content_same =
                stored_hash.as_deref().is_some_and(|hash| hash == file.content_hash.as_slice())
                    && !(plan.serves_external_baseline && rebound_ids.contains(&file.key.root_id));
            if changed_ids.contains(&file.key.root_id)
                || !known.contains(&file.key)
                || !content_same
            {
                cleanup.insert(file.key.clone());
                affected_files.push(file);
                if plan.old_roots.contains_id(&file.key.root_id) && known.contains(&file.key) {
                    rebuilt += 1;
                } else {
                    added += 1;
                }
            }
        }

        let obsolete: HashSet<FileKey> =
            cleanup.iter().filter(|key| !present_keys.contains(*key)).cloned().collect();
        let obsolete_baseline: HashSet<FileKey> =
            obsolete.iter().filter(|key| plan.manifest.contains_key(*key)).cloned().collect();

        let upserts: Vec<WorkspaceTransitionFile> = if plan.serves_external_baseline {
            Vec::new()
        } else {
            affected_files
                .iter()
                .map(|file| WorkspaceTransitionFile {
                    key: file.key.clone(),
                    hash: file.content_hash.clone(),
                    chunks: file.chunks.clone(),
                    graph_contexts: file.graph_contexts.clone(),
                    source_spans: file.source_spans.clone(),
                })
                .collect()
        };
        let overlay_files: Vec<WorkspaceTransitionOverlayFile> = if plan.serves_external_baseline {
            affected_files
                .iter()
                .map(|file| {
                    let baseline = plan.manifest.get(&file.key);
                    WorkspaceTransitionOverlayFile {
                        key: file.key.clone(),
                        len: file.identity.len,
                        modified: file.identity.modified,
                        canonical: file.identity.canonical.clone(),
                        file_hash: normalized_file_hash_for_indexed_documents(&file.documents),
                        lexical_documents: file.documents.clone(),
                        embedding_inputs: file.embedding_inputs.clone(),
                        has_baseline: baseline.is_some(),
                        baseline_equal: baseline
                            .is_some_and(|fingerprint| fingerprint == &file.manifest_fingerprint),
                    }
                })
                .collect()
        } else {
            Vec::new()
        };

        let pending_collection_embeddings = !plan.serves_external_baseline
            && self.embedder.is_some()
            && upserts.iter().any(|file| !file.chunks.is_empty());
        let pending_overlay_embeddings = plan.serves_external_baseline
            && (!unread_keys.is_empty() || (self.embedder.is_some() && !affected_files.is_empty()));

        let mut removed_chunk_ids = HashSet::new();
        for key in &cleanup {
            removed_chunk_ids.extend(self.store.chunk_ids_for_file(
                "code",
                &key.root_id,
                &key.path,
            )?);
        }
        let (embedding_generation, mut embeddings) = match self.embedder.as_ref() {
            Some(embedder) => Self::load_embeddings_for_profile(&self.store, embedder, self.dim)?,
            None => self.store.load_all_embeddings_with_generation(self.dim)?,
        };
        embeddings.retain(|(id, _)| !removed_chunk_ids.contains(id));
        #[cfg(test)]
        if crate::store::FORCE_WORKSPACE_TRANSITION_VECTOR_ERROR.with(std::cell::Cell::get) {
            return Err(SearchError::Index(
                "forced workspace transition vector preparation failure".to_owned(),
            ));
        }
        let next_index = VectorIndex::build(self.dim, &embeddings)?;
        // Both spellings of every planned file, collected here where the walk's identities are
        // still in hand — the apply step writes them beside the rows (github#192).
        let spellings = {
            let mut records: Vec<(String, FileKey)> = Vec::new();
            let mut seen: HashSet<String> = HashSet::new();
            for (key, identity) in plan
                .files
                .iter()
                .map(|file| (&file.key, &file.identity))
                .chain(plan.unread_files.iter().map(|file| (&file.key, &file.identity)))
            {
                for spelling in [&identity.abs_path, &identity.canonical] {
                    let rendered = spelling.to_string_lossy().into_owned();
                    if !rendered.is_empty() && seen.insert(rendered.clone()) {
                        records.push((rendered, key.clone()));
                    }
                }
            }
            records
        };
        validated.staging = Some(WorkspaceRootsTransitionStaging {
            changed_root_ids: changed_ids,
            cleanup,
            obsolete_baseline,
            spellings,
            upserts,
            unread_present: unread_keys,
            overlay_files,
            next_index,
            embedding_generation,
            removed: obsolete.len(),
            rebuilt,
            added,
            pending_collection_embeddings,
            pending_overlay_embeddings,
        });
        Ok(Some(validated))
    }

    /// Commit one staged root transition under the ownership fence. The only fallible persistent
    /// mutation is one cooperatively-cancellable SQLite transaction; cache, vector and root-table
    /// publication happen together after commit while the already-acquired cache guard is held.
    pub fn apply_staged_workspace_roots_transition(
        &mut self,
        validated: &mut ValidatedWorkspaceRootsTransitionPlan,
        checkpoint: &mut dyn FnMut() -> ControlFlow<()>,
    ) -> ControlFlow<(), Result<WorkspaceRootsTransitionOutcome, SearchError>> {
        let plan = &validated.plan;
        if self.workspace_roots_epoch != plan.epoch
            || self.workspace_roots.as_ref() != Some(&plan.old_roots)
            || self.serves_external_baseline != plan.serves_external_baseline
        {
            return ControlFlow::Continue(Ok(WorkspaceRootsTransitionOutcome::Superseded));
        }
        if plan.old_roots == plan.next_roots {
            return ControlFlow::Continue(Ok(WorkspaceRootsTransitionOutcome::Unchanged));
        }
        if plan.serves_external_baseline {
            match self.dispatched_manifest_fingerprints() {
                Ok(Some(manifest)) if manifest == plan.manifest => {}
                Ok(_) => {
                    return ControlFlow::Continue(Ok(WorkspaceRootsTransitionOutcome::Superseded));
                }
                Err(error) => return ControlFlow::Continue(Err(error)),
            }
        }
        let Some(staging) = validated.staging.as_ref() else {
            return ControlFlow::Continue(Err(SearchError::Index(
                "workspace root transition was not staged".to_owned(),
            )));
        };
        match self.store.embedding_generation() {
            Ok(generation) if generation == staging.embedding_generation => {}
            Ok(_) => {
                return ControlFlow::Continue(Ok(WorkspaceRootsTransitionOutcome::Superseded));
            }
            Err(error) => return ControlFlow::Continue(Err(error)),
        }
        let mut cache = match self.workspace_overlay_cache.lock() {
            Ok(cache) if cache.transition_epoch() == plan.overlay_epoch => cache,
            Ok(_) => {
                return ControlFlow::Continue(Ok(WorkspaceRootsTransitionOutcome::Superseded));
            }
            Err(error) => {
                return ControlFlow::Continue(Err(SearchError::Index(format!(
                    "workspace overlay cache lock error: {error}"
                ))));
            }
        };
        match self.store.apply_workspace_roots_transition(
            WorkspaceStoreTransition {
                changed_root_ids: &staging.changed_root_ids,
                cleanup: &staging.cleanup,
                upserts: &staging.upserts,
                token_layout_claim: self.embedder.as_ref().and_then(Embedder::token_layout_claim),
            },
            checkpoint,
        ) {
            Ok(ControlFlow::Break(())) => return ControlFlow::Break(()),
            Err(error) => return ControlFlow::Continue(Err(error)),
            Ok(ControlFlow::Continue(())) => {}
        }
        let staging =
            validated.staging.take().expect("staging checked immediately before the transaction");
        // The transition published a world with these spellings; the journal follows it. Best
        // effort like every journal write: a failure costs the fallback behaviour for those
        // spellings, not the transition that already committed (github#192).
        if let Err(error) = self.store.record_path_spellings("code", &staging.spellings) {
            tracing::warn!("failed to record the transition's path spellings: {error}");
        }
        // Applied to the LIVE cache, never installed over it: whatever the window between
        // staging and this commit admitted — a point mark, a settled refresh, a whole overlay
        // publication — is still there and keeps its meaning. The transition changes only the
        // keys its own root change is about.
        cache.transition_roots(
            &staging.changed_root_ids,
            &staging.cleanup,
            &staging.obsolete_baseline,
            &staging.unread_present,
            staging.overlay_files,
        );
        let prior_coverage = self.semantic_qualification.get().coverage_complete;
        self.invalidate_semantic_coverage();
        if !staging.pending_collection_embeddings && prior_coverage {
            let mut evidence = self.semantic_qualification.get();
            evidence.coverage_complete = true;
            evidence.pending_work = false;
            self.semantic_qualification.set(evidence);
        }
        self.index = staging.next_index;
        self.observe_live_index("live_index_replaced", Reason::RootTransition, Outcome::Completed);
        self.workspace_roots = Some(plan.next_roots.clone());
        self.workspace_roots_epoch += 1;
        ControlFlow::Continue(Ok(WorkspaceRootsTransitionOutcome::Applied {
            removed: staging.removed,
            rebuilt: staging.rebuilt,
            added: staging.added,
            pending_collection_embeddings: staging.pending_collection_embeddings,
            pending_overlay_embeddings: staging.pending_overlay_embeddings,
        }))
    }

    /// Compatibility wrapper for boot and tests that do not own a workspace lease.
    pub fn apply_validated_workspace_roots_transition(
        &mut self,
        validated: ValidatedWorkspaceRootsTransitionPlan,
    ) -> Result<WorkspaceRootsTransitionOutcome, SearchError> {
        let Some(mut staged) = self.stage_validated_workspace_roots_transition(validated)? else {
            return Ok(WorkspaceRootsTransitionOutcome::Superseded);
        };
        let mut checkpoint = || ControlFlow::Continue(());
        match self.apply_staged_workspace_roots_transition(&mut staged, &mut checkpoint) {
            ControlFlow::Continue(result) => result,
            ControlFlow::Break(()) => unreachable!("permit-all checkpoint cannot cancel"),
        }
    }

    pub fn enable_workspace_watcher_mode(&mut self) {
        if let Ok(mut cache) = self.workspace_overlay_cache.lock() {
            cache.enable_watcher_mode();
        }
    }

    pub fn set_workspace_baseline_hash_mode(&mut self, hash_mode: BaselineHashMode) {
        self.workspace_baseline_hash_mode = hash_mode;
        if let Ok(mut cache) = self.workspace_overlay_cache.lock() {
            cache.clear();
        }
    }

    pub fn mark_workspace_path_dirty(
        &self,
        path: impl AsRef<std::path::Path>,
    ) -> Result<bool, SearchError> {
        let path = path.as_ref();
        let Some(key) = self.workspace_file_key(path) else {
            return Ok(false);
        };
        self.record_point_spelling(path, &key);
        self.mark_workspace_key_dirty(key)?;
        Ok(true)
    }

    /// Record a point event's spelling, best effort.
    ///
    /// A point mark is an index WRITE, and the spelling it arrived as is what a later removal
    /// of that spelling looks up — a failure costs that fallback, not the mark (github#192).
    fn record_point_spelling(&self, path: &Path, key: &FileKey) {
        if let Err(error) = self.record_workspace_path_spelling(path, key) {
            tracing::warn!(?path, "failed to record a point event's path spelling: {error}");
        }
    }

    pub fn mark_workspace_key_dirty(&self, key: FileKey) -> Result<(), SearchError> {
        let mut cache = self
            .workspace_overlay_cache
            .lock()
            .map_err(|e| SearchError::Index(format!("workspace overlay cache lock error: {e}")))?;
        // The mark is recorded, the mode is NOT flipped: watcher mode doubles as "skip the full
        // rescan", and a mark alone is not a feeder — a store nothing feeds would stop seeing
        // the disk for good (github#186). The mode is enabled where the feed is: by the sink
        // that consumes the hub, and by nothing else.
        cache.mark_dirty_path(key);
        Ok(())
    }

    /// Apply one already-materialized drift slice. The host advances its cursors only after this
    /// returns `Continue(Ok(_))`; cancellation rolls back every Store mutation in the slice.
    ///
    /// Returns the seq the slice's context-dirty marks were stamped with, `None` when it placed
    /// none: the host hands it to whoever consumes the marks, together with the fact that
    /// caused them.
    pub fn apply_prepared_workspace_drift_batch(
        &mut self,
        dirty_keys: &[FileKey],
        removed_keys: &[FileKey],
        context_keys: &[FileKey],
        checkpoint: &mut dyn FnMut() -> ControlFlow<()>,
    ) -> ControlFlow<(), Result<Option<i64>, SearchError>> {
        let _bounded = crate::point_refresh::BoundedPublication::enter();
        let rows = dirty_keys.len() + removed_keys.len() + context_keys.len();
        if rows > WORKSPACE_APPLY_BATCH_ROWS {
            return ControlFlow::Continue(Err(SearchError::Index(format!(
                "workspace drift batch has {rows} rows; maximum is {WORKSPACE_APPLY_BATCH_ROWS}"
            ))));
        }
        let mut cache = match self.workspace_overlay_cache.lock() {
            Ok(cache) => cache,
            Err(error) => {
                return ControlFlow::Continue(Err(SearchError::Index(format!(
                    "workspace overlay cache lock error: {error}"
                ))));
            }
        };
        let outcome =
            match self.store.apply_workspace_drift_batch(removed_keys, context_keys, checkpoint) {
                Ok(ControlFlow::Continue(outcome)) => outcome,
                Ok(ControlFlow::Break(())) => return ControlFlow::Break(()),
                Err(error) => return ControlFlow::Continue(Err(error)),
            };
        let WorkspaceDriftStoreOutcome { removed_chunk_ids, context_mark_seq } = outcome;
        if let Some(seq) = context_mark_seq {
            self.store.observe_committed_mark_seq(seq);
            let mut evidence = self.semantic_qualification.get();
            evidence.context_pending = true;
            self.semantic_qualification.set(evidence);
        }
        let mut eviction_outcome = Outcome::Completed;
        let evicted_any = !removed_chunk_ids.is_empty();
        for chunk_id in removed_chunk_ids {
            if let Err(error) = self.index.remove(chunk_id) {
                eviction_outcome = Outcome::Failed;
                self.invalidate_semantic_coverage();
                tracing::warn!(chunk_id, "failed to evict a committed drift removal: {error}");
            }
        }
        if evicted_any {
            self.observe_live_index("live_index_evicted", Reason::FileDeleted, eviction_outcome);
        }
        // The feed has just proved itself: this many keys arrived from it. Enabling the mode
        // here — and not on a bare mark or removal — is what keeps "watcher mode" meaning "a
        // feed is attached" (github#186). An empty slice says nothing about a feed.
        if rows > 0 {
            cache.enable_watcher_mode();
        }
        for key in dirty_keys {
            cache.mark_dirty_path(key.clone());
        }
        for key in removed_keys {
            cache.mark_dirty_path(key.clone());
            // A shared baseline may hold a copy, and hiding it conservatively needs no manifest
            // loaded inside the publication. A local store has no copy beyond the rows this
            // batch just deleted: there is nothing to hide.
            cache.remove_known_deleted(key, self.serves_external_baseline);
        }
        ControlFlow::Continue(Ok(context_mark_seq))
    }

    /// The store key of a workspace `.bsl` file, or `None` when it is not a
    /// `.bsl` or lies outside every registered root. Shared by the workspace
    /// point-update entry points.
    ///
    /// A relative path is taken as configuration-relative — that is the only reading
    /// available, and the callers that pass one have already stripped the prefix
    /// themselves. The canonical spelling is what attribution ranks roots by, which
    /// is why [`WorkspaceRoots::root_of`] takes two; both come from the one procedure
    /// [`WorkspaceRoots::spellings_of`], so a `.bsl` and a descriptor cannot be
    /// attributed by different rules.
    pub fn workspace_file_key(&self, path: &Path) -> Option<FileKey> {
        workspace_file_key_in(self.workspace_roots.as_ref()?, path)
    }

    /// Phase one of a carrier snapshot: the in-memory carriers, read under the caller's
    /// engine lock. The store's carriers are read by [`CarrierCapture::complete`] through a
    /// connection of the caller's own, off every lock — reading every stored key under the
    /// engine mutex is what used to park every request behind a whole-store load.
    pub fn capture_carriers(&self) -> Result<CarrierCapture, SearchError> {
        let (overlay_entries, unread) = self
            .workspace_overlay_cache
            .lock()
            .map_err(|e| SearchError::Index(format!("workspace overlay cache lock error: {e}")))?
            .known_keys();
        Ok(CarrierCapture {
            roots: self.workspace_roots.clone(),
            overlay_entries,
            unread,
            serves_external_baseline: self.serves_external_baseline,
            db_path: self.store.db_path().to_path_buf(),
        })
    }
}

/// The in-memory half of a carrier snapshot (see [`SearchEngine::capture_carriers`]).
pub struct CarrierCapture {
    roots: Option<WorkspaceRoots>,
    overlay_entries: HashSet<FileKey>,
    unread: HashSet<FileKey>,
    serves_external_baseline: bool,
    db_path: PathBuf,
}

impl CarrierCapture {
    /// Where the caller opens the reader [`Self::complete`] takes.
    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    /// Phase two: the store's carriers, read through `reader` — never the live connection.
    /// The manifest counts only when this engine serves an external baseline, as in the
    /// engine's own carrier read.
    pub fn complete(self, reader: &Store) -> Result<CarrierSnapshot, SearchError> {
        let carriers = crate::key_carriers::CarrierKeys {
            store_rows: reader
                .all_files_in_collection("code")?
                .into_iter()
                .map(|(key, _hash)| key)
                .collect(),
            overlay_entries: self.overlay_entries,
            unread: self.unread,
            fingerprints: reader.overlay_fingerprint_keys()?,
            manifest: if self.serves_external_baseline {
                reader
                    .load_baseline_manifest_fingerprints("code")?
                    .unwrap_or_default()
                    .into_keys()
                    .collect()
            } else {
                HashSet::new()
            },
        };
        Ok(CarrierSnapshot { roots: self.roots, carriers })
    }
}

/// Every key any carrier knows, with the root table it was read under.
pub struct CarrierSnapshot {
    roots: Option<WorkspaceRoots>,
    carriers: crate::key_carriers::CarrierKeys,
}

impl CarrierSnapshot {
    pub fn known_keys(&self) -> HashSet<FileKey> {
        self.carriers.all_keys()
    }

    /// The keys under `dirs` whose files are proven gone (see
    /// [`SearchEngine::vanished_workspace_keys`]).
    pub fn vanished_keys(&self, dirs: &[PathBuf]) -> Vec<FileKey> {
        match &self.roots {
            Some(roots) => vanished_keys_in(roots, &self.carriers, dirs),
            None => Vec::new(),
        }
    }

    /// The keys a drain's removals name: `removed_paths` are the exact paths it reported
    /// gone, `removed_subtrees` are the paths under which every indexed file is gone.
    /// `reader` is the store half the caller opened — the journal is read through it, off the
    /// engine lock, like the carriers themselves.
    ///
    /// A path the journal holds is answered by the journal ALONE: the spelling names the key
    /// its file was indexed under, and re-attribution now would name a different one — which
    /// is exactly the defect the pair exists to close (github#192). That answer is gated by
    /// the absence proof, so removing a link whose target lives takes nothing with it. A path
    /// the journal does not hold falls back to attribution, with the event-driven meaning it
    /// had before the journal existed.
    pub fn removal_keys(
        &self,
        reader: &Store,
        removed_paths: &[PathBuf],
        removed_subtrees: &[PathBuf],
    ) -> Result<Vec<FileKey>, SearchError> {
        let Some(roots) = &self.roots else {
            return Ok(Vec::new());
        };
        let mut keys = self.vanished_keys(removed_subtrees);
        let mut seen: HashSet<FileKey> = keys.iter().cloned().collect();
        for spelling in removal_spellings(roots, removed_subtrees) {
            for key in reader
                .path_spelling_key(&spelling, "code")?
                .into_iter()
                .chain(reader.path_spellings_under(&spelling, "code")?)
            {
                if !seen.contains(&key) && proven_absent_key(roots, &key) {
                    seen.insert(key.clone());
                    keys.push(key);
                }
            }
        }
        for key in removed_path_keys(roots, reader, removed_paths)? {
            if seen.insert(key.clone()) {
                keys.push(key);
            }
        }
        Ok(keys)
    }
}

/// The store key of a workspace `.bsl` file under `roots`, or `None` when it is not a `.bsl`
/// or lies outside every registered root (see [`SearchEngine::workspace_file_key`]).
pub fn workspace_file_key_in(roots: &WorkspaceRoots, path: &Path) -> Option<FileKey> {
    if !bsl_conventions::has_extension(path, bsl_conventions::BSL_EXTENSION) {
        return None;
    }
    let (walked, canonical) = roots.spellings_of(path);
    // A `.bsl`-spelled link may resolve to a non-source target — by role, or by not being
    // a regular file at all (a directory spelled `.bsl`). A key under such a target's root
    // would be one that is FORBIDDEN to exist (the walk drops such files), so canonical
    // attribution is meaningless there; the walked spelling is the only key the file could
    // ever have been indexed under — the key a removal must reach. A GONE target still
    // attributes canonically: it was a file if it was anything, and the removal path needs
    // the last known spelling.
    let target_is_source = project_model::file_role(&canonical) == project_model::FileRole::Source
        && match std::fs::metadata(&canonical) {
            Ok(metadata) => metadata.is_file(),
            Err(_) => true,
        };
    if target_is_source {
        roots.root_of(&walked, &canonical)
    } else {
        roots.root_of_declared(&walked)
    }
}

impl SearchEngine {
    /// Mark one workspace `.bsl` file's stored graph context stale, so a later
    /// reindex/embed pass re-renders it. Cheap metadata write (a side-table upsert, no
    /// chunk mutation, so the vector sidecar is not invalidated). Returns whether the
    /// path was a workspace `.bsl`.
    pub fn mark_workspace_path_context_dirty(
        &self,
        path: impl AsRef<Path>,
    ) -> Result<bool, SearchError> {
        let path = path.as_ref();
        let Some(key) = self.workspace_file_key(path) else {
            return Ok(false);
        };
        self.mark_workspace_key_context_dirty(&key)?;
        Ok(true)
    }

    /// Mark files just written without a context the provider failed to render, so a context
    /// refresh renders them, and tell the provider's owner which marks are owed.
    fn owe_graph_contexts<'k>(
        &self,
        keys: impl IntoIterator<Item = &'k FileKey>,
    ) -> Result<(), SearchError> {
        let mut high = None;
        for key in keys {
            high = Some(self.store.mark_context_dirty("code", &key.root_id, &key.path)?);
        }
        if let (Some(high), Some(provider)) = (high, self.graph_context_provider.as_deref()) {
            provider.context_marks_owed(high);
        }
        Ok(())
    }

    pub fn mark_workspace_key_context_dirty(&self, key: &FileKey) -> Result<(), SearchError> {
        self.store.mark_context_dirty("code", &key.root_id, &key.path)?;
        let mut evidence = self.semantic_qualification.get();
        evidence.context_pending = true;
        self.semantic_qualification.set(evidence);
        Ok(())
    }

    /// Mark every indexed workspace file context-dirty (a configuration-root descriptor
    /// changed: conservatively assume any module's context could shift). Returns the
    /// number of files marked.
    pub fn mark_workspace_context_dirty(&self) -> Result<usize, SearchError> {
        let count = self.store.mark_collection_context_dirty("code")?.0;
        if count > 0 {
            let mut evidence = self.semantic_qualification.get();
            evidence.context_pending = true;
            self.semantic_qualification.set(evidence);
        }
        Ok(count)
    }

    /// [`Self::mark_workspace_context_dirty`] for a caller that consumes the batch in the same
    /// breath: the rows are stamped at `stamp_seq` — the bound of the build the re-render will
    /// run against — so that same bound clears exactly this batch, and a file carrying a
    /// fresher drift keeps its own mark instead of being swept up by it. Returns the number of
    /// rows written.
    pub fn mark_workspace_context_dirty_at(&self, stamp_seq: i64) -> Result<usize, SearchError> {
        let count = self.store.mark_collection_context_dirty_at("code", stamp_seq)?;
        if count > 0 {
            let mut evidence = self.semantic_qualification.get();
            evidence.context_pending = true;
            self.semantic_qualification.set(evidence);
        }
        Ok(count)
    }

    /// The set of paths currently marked context-dirty in `collection`.
    pub fn context_dirty_paths(&self, collection: &str) -> Result<HashSet<FileKey>, SearchError> {
        self.store.context_dirty_paths(collection)
    }

    /// A handle to the highest context-dirty mark seq this store has observed. The graph
    /// layer reads it at build start to bound which marks that build's publish may consume;
    /// the stamps themselves are allocated by the database. See [`Store::mark_seq_handle`].
    pub fn mark_seq_handle(&self) -> Arc<AtomicI64> {
        self.store.mark_seq_handle()
    }

    /// Remove one workspace `.bsl` file after a local deletion, closing every path a
    /// stale hit could survive:
    /// - drops its `files` row and cascaded `chunks`/FTS rows from the store;
    /// - hides a baseline (Postgres-mode) copy of the same path in the overlay so it
    ///   cannot resurrect the file;
    /// - marks the path dirty in the in-memory overlay cache so a cached entry stops
    ///   serving stale hits on the next refresh (`refresh_dirty_paths` hides a gone file);
    /// - evicts exactly the deleted chunks' vectors from the live index incrementally.
    ///
    /// The store deletion bumps `embedding_generation` (via the delete triggers), so the
    /// persisted vector sidecar already invalidates and a cold start rebuilds — this path
    /// deliberately does NOT reload every embedding or re-persist the sidecar. Returns
    /// whether the path was a workspace `.bsl`.
    pub fn remove_workspace_path(&mut self, path: impl AsRef<Path>) -> Result<bool, SearchError> {
        let path = path.as_ref();
        // The journal speaks for a spelling it holds, alone: it names the key the file was
        // indexed under, while re-attribution now would name a different one — that is the
        // whole reason the pair is kept (github#192). Its answer is gated by the absence
        // proof, so a link removed while its target lives takes nothing with it.
        if let Some(recorded) = self.recorded_spelling_key(path)? {
            let roots = self.workspace_roots.as_ref().expect("a recorded spelling has a table");
            if proven_absent_key(roots, &recorded) {
                self.remove_workspace_key(&recorded)?;
            }
            return Ok(true);
        }
        // No record: the spelling is answered by attribution with its event-driven meaning,
        // exactly as before the journal existed.
        let Some(key) = self.workspace_file_key(path) else {
            return Ok(false);
        };
        self.remove_workspace_key(&key)?;
        Ok(true)
    }

    /// The journal's answer for the spellings `path` can arrive under — the path as given and
    /// its canonical counterpart while one still resolves.
    fn recorded_spelling_key(&self, path: &Path) -> Result<Option<FileKey>, SearchError> {
        let Some(roots) = self.workspace_roots.as_ref() else {
            return Ok(None);
        };
        for spelling in removal_spellings(roots, &[path.to_path_buf()]) {
            if let Some(key) = self.store.path_spelling_key(&spelling, "code")? {
                return Ok(Some(key));
            }
        }
        Ok(None)
    }

    /// Record the spellings a path was indexed under: the one given, and its canonical
    /// counterpart while it resolves.
    ///
    /// Called where an index WRITE happens, not where a query does: the journal describes the
    /// generation this store serves, and a lookup has no business extending it (github#192).
    pub fn record_workspace_path_spelling(
        &self,
        path: &Path,
        key: &FileKey,
    ) -> Result<(), SearchError> {
        let Some(roots) = self.workspace_roots.as_ref() else {
            return Ok(());
        };
        let (walked, canonical) = roots.spellings_of(path);
        self.record_workspace_spellings(key, &[walked.as_path(), canonical.as_path()])
    }

    /// [`Self::record_workspace_path_spelling`] for spellings the caller already holds — a
    /// walk has both sides in hand, and re-deriving the canonical one would read a disk the
    /// walk no longer describes.
    pub fn record_workspace_spellings(
        &self,
        key: &FileKey,
        spellings: &[&Path],
    ) -> Result<(), SearchError> {
        let mut records: Vec<(String, FileKey)> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for spelling in spellings {
            let rendered = spelling.to_string_lossy().into_owned();
            if !rendered.is_empty() && seen.insert(rendered.clone()) {
                records.push((rendered, key.clone()));
            }
        }
        self.store.record_path_spellings("code", &records)
    }

    /// [`Self::remove_workspace_path`] for a caller that already holds the store
    /// key. A key read back from the store must NOT be re-attributed: its path is
    /// relative to its own root, and re-deriving it from the workspace would hand
    /// an extension's file to the configuration and leave the real row in place.
    pub fn remove_workspace_key(&mut self, key: &FileKey) -> Result<(), SearchError> {
        // A manifest this engine cannot read is not evidence of "no baseline copy": guessing
        // `false` would skip the hiding that stops the copy from being served.
        let has_baseline =
            self.dispatched_manifest_fingerprints()?.is_some_and(|m| m.contains_key(key));
        self.remove_workspace_key_with(key, has_baseline)
    }

    /// Drop the indexed files under `dirs` that are no longer on disk, across every carrier.
    ///
    /// A vanished directory arrives as ONE event naming the directory itself, while the files
    /// that went with it are never named — the walk that would have enumerated them is
    /// exactly what is no longer possible. Point removal cannot express this: it keys through
    /// [`Self::workspace_file_key`], which answers `None` for anything that is not a `.bsl`,
    /// so a call on a directory is silently a no-op.
    ///
    /// Each candidate key is then checked against disk INDIVIDUALLY rather than trusting the
    /// event or the state of the directory itself. Both of those are too coarse to be
    /// trusted with a whole subtree: an event says what was true when it fired, and a
    /// directory that is back says nothing about which of its files came back with it. The
    /// key's own file is the only thing that answers the question actually being asked.
    /// Anything that is not a proven absence — a permission error, a race — keeps its key.
    ///
    /// Callers pass every path a batch reported gone, not just the ones that look like
    /// directories: the answer for a path that really was a file is simply an empty
    /// candidate set, since no key lives strictly under it. Taking them together is what
    /// keeps this to ONE reading of the carriers per batch.
    ///
    /// Candidates come from every carrier for the same reason a reconcile does (see
    /// [`Self::carrier_keys`]): against a remote baseline there are no local rows at all.
    /// Returns the number of removed keys.
    pub fn remove_vanished_under(&mut self, dirs: &[PathBuf]) -> Result<usize, SearchError> {
        let candidates = self.vanished_workspace_keys(dirs)?;
        self.remove_workspace_keys(candidates)
    }

    /// Materialize subtree removals, including filesystem absence checks, before a caller enters
    /// its publication barrier.
    pub fn vanished_workspace_keys(&self, dirs: &[PathBuf]) -> Result<Vec<FileKey>, SearchError> {
        let Some(roots) = self.workspace_roots.as_ref() else {
            return Ok(Vec::new());
        };
        let mut candidates = vanished_keys_in(roots, &self.carrier_keys()?, dirs);
        let mut seen: HashSet<FileKey> = candidates.iter().cloned().collect();
        // The journal answers what attribution no longer can — the key a removed spelling was
        // indexed under — and the absence proof gates every journal answer, so a spelling whose
        // file lives on (a link removed while its target stays) takes nothing with it
        // (github#192).
        for spelling in removal_spellings(roots, dirs) {
            let recorded = self
                .store
                .path_spelling_key(&spelling, "code")?
                .into_iter()
                .chain(self.store.path_spellings_under(&spelling, "code")?);
            for key in recorded {
                if !seen.contains(&key) && proven_absent_key(roots, &key) {
                    seen.insert(key.clone());
                    candidates.push(key);
                }
            }
        }
        Ok(candidates)
    }

    /// Apply an already-materialized removal set without further filesystem probes.
    pub fn remove_workspace_keys(
        &mut self,
        candidates: Vec<FileKey>,
    ) -> Result<usize, SearchError> {
        let lifecycle_batch = Batch::new(self.store.db_path(), Reason::FileDeleted);
        let result = lifecycle_batch.context().in_scope(|| {
            let carriers = self.carrier_keys()?;
            let hidden = match self.workspace_overlay_cache.lock() {
                Ok(cache) => cache.hidden_keys(),
                Err(error) => {
                    tracing::warn!("failed to read overlay hidings for a removal batch: {error}");
                    HashSet::new()
                }
            };
            let batch = self.remove_key_batch(candidates, &carriers, &hidden);
            if let Some(error) = batch.first_error {
                return Err(SearchError::Index(format!(
                    "removal batch cleared {} keys and failed on {}; first failure: {error}",
                    batch.removed, batch.failed
                )));
            }
            Ok(batch.removed)
        });
        lifecycle_batch.finish(if result.is_ok() { Outcome::Completed } else { Outcome::Failed });
        result
    }

    /// Snapshot every known workspace key from all carriers for an external reconcile plan.
    pub fn known_workspace_keys(&self) -> Result<HashSet<FileKey>, SearchError> {
        Ok(self.carrier_keys()?.all_keys())
    }

    /// [`Self::remove_workspace_key`] with the baseline evidence already resolved, so a batch
    /// caller loads the manifest once instead of once per key.
    ///
    /// The store row goes LAST, after every step that can fail. That row is what a reconcile
    /// selects the key by, so dropping it first would turn any later failure into a silent
    /// loss: the retry mark reaches the overlay alone, and the chunk ids the vector eviction
    /// needs come from the row itself. Leaving it in place instead makes the very next
    /// reconcile pick the key up exactly where this pass left it.
    ///
    /// One window stays open and is accepted knowingly: if the final row delete fails after
    /// the vectors are out, a file that turns out to still exist (a removal event that lied,
    /// or a delete-and-recreate) keeps its row and loses its vectors until a rebuild — the
    /// point refresh finds it equal to the baseline and reindexes nothing. Closing it needs
    /// atomicity between an in-memory index and SQLite, which does not exist here; every
    /// other order merely moves the window (evicting after the row delete strands vectors
    /// with no row left to find them by).
    fn remove_workspace_key_with(
        &mut self,
        key: &FileKey,
        has_baseline: bool,
    ) -> Result<(), SearchError> {
        // The retry obligation comes FIRST: every operation below can fail, and returning
        // early without the mark would leave no signal for the point path — while the rows
        // still tell the old story. A poisoned lock is a dead process, not a key state, but
        // it does mean the obligation was not recorded, so it is not a success either.
        {
            let mut cache = self.workspace_overlay_cache.lock().map_err(|e| {
                SearchError::Index(format!("workspace overlay cache lock error: {e}"))
            })?;
            // The removal records its mark and its hiding; the MODE is not flipped here, for
            // the same reason as in `mark_workspace_key_dirty` — a removal is not a feeder
            // either, and the boot reconcile runs long before any sink exists (github#186).
            cache.mark_dirty_path(key.clone());
        }
        // Collected before the row goes, because the row is where they live.
        let chunk_ids = self.store.chunk_ids_for_file("code", &key.root_id, &key.path)?;
        // The dead file's fingerprint row must not survive it: the dirty mark dies with the
        // process, and a namesake recreated at the same (len, mtime, canonical) would inherit
        // the "verified" claim across a restart.
        self.store.delete_overlay_fingerprint_entries(std::slice::from_ref(key))?;
        {
            // The deletion is proven, so drop the overlay entry at once — the point refresh
            // would read a root that vanished WITH the file as "unreachable, retry" and leave
            // a ghost entry. The mark still re-checks the disk: if the event lied, the next
            // point pass republishes the live file.
            let mut cache = self.workspace_overlay_cache.lock().map_err(|e| {
                SearchError::Index(format!("workspace overlay cache lock error: {e}"))
            })?;
            cache.remove_known_deleted(key, has_baseline);
        }
        // Second to last, right before the row that names the chunks: evicting earlier would
        // strip a file of its vectors on any LATER failure, and a removal event that turns out
        // to have lied leaves the file unchanged on disk — so the point refresh settles it as
        // equal to the baseline, reindexes nothing, and the vectors never come back.
        for id in chunk_ids {
            #[cfg(test)]
            if FORCE_VECTOR_REMOVE_ERROR.with(std::cell::Cell::get) {
                return Err(SearchError::Index("forced vector removal failure".to_owned()));
            }
            if let Err(error) = self.index.remove(id) {
                self.observe_live_index("live_index_evicted", Reason::FileDeleted, Outcome::Failed);
                return Err(error);
            }
        }
        self.observe_live_index("live_index_evicted", Reason::FileDeleted, Outcome::Completed);
        self.store.remove_file_with_token_layout(
            &key.root_id,
            &key.path,
            "code",
            self.embedder.as_ref().and_then(Embedder::token_layout_claim),
        )?;
        // Housekeeping, not coverage: a failure here leaves at most one stale record, which the
        // next index write at the same spelling overwrites, so the removal is not failed over it
        // (github#192).
        if let Err(error) =
            self.store.forget_path_spellings_for_key(&key.root_id, &key.path, "code")
        {
            tracing::warn!(
                root = ?key.root_id,
                path = %key.path,
                "failed to forget a removed key's path spellings: {error}"
            );
        }
        Ok(())
    }

    /// One reading of every carrier that can still know about a workspace file, taken once
    /// per operation: each carrier costs a load, and asking per key would turn a reconcile
    /// into a query per stored file.
    ///
    /// A carrier that cannot be read is left EMPTY rather than guessed at, and the two cases
    /// mean different things. The manifest is empty whenever this engine does not serve an
    /// external baseline — its rows deliberately survive a mode switch, so a local engine
    /// must not read them as evidence. The overlay is empty only if its lock is poisoned,
    /// which is a dead process rather than a key state.
    fn carrier_keys(&self) -> Result<crate::key_carriers::CarrierKeys, SearchError> {
        let mut carriers = crate::key_carriers::CarrierKeys {
            store_rows: self
                .store
                .all_files_in_collection("code")?
                .into_iter()
                .map(|(key, _hash)| key)
                .collect(),
            ..Default::default()
        };
        {
            // A carrier that could not be read leaves its keys out of the reconcile entirely,
            // so a silent empty set would report a full sweep that never looked here.
            let cache = self.workspace_overlay_cache.lock().map_err(|e| {
                SearchError::Index(format!("workspace overlay cache lock error: {e}"))
            })?;
            let (entries, unread) = cache.known_keys();
            carriers.overlay_entries = entries;
            carriers.unread = unread;
        }
        // Keys only, without a snapshot id: the snapshot-aware load clears the table when the
        // rows belong to another snapshot, and it would need the manifest header — making a
        // reconcile of a LOCAL index fail on a carrier that mode does not even serve.
        carriers.fingerprints = self.store.overlay_fingerprint_keys()?;
        carriers.manifest =
            self.dispatched_manifest_fingerprints()?.unwrap_or_default().into_keys().collect();
        Ok(carriers)
    }

    /// Reconcile the workspace `code` collection against the set of `.bsl` files actually
    /// present on disk (`present_abs`, absolute paths from a fresh walk): every key no longer
    /// present is removed via [`Self::remove_workspace_key_with`] (overlay dirty + baseline
    /// hiding + incremental vector eviction). This closes the gap where a file deleted during
    /// a lost watch window (change-hub overflow or a structural subtree rescan) keeps its rows
    /// and vectors forever, because the ordinary drift path only marks files that still exist.
    ///
    /// Candidates come from EVERY carrier (see [`Self::carrier_keys`]), not from the store
    /// rows alone: those rows are a snapshot of the boot walk, so a file indexed afterwards
    /// has no row at all, and against a remote baseline there are no local rows whatsoever.
    /// Bounded O(known keys) and driven only on the rare rescan branch; the caller walks the
    /// tree OUTSIDE the engine lock and passes the result here. Returns the number of removed
    /// keys.
    pub fn reconcile_workspace_files(
        &mut self,
        present_abs: &HashSet<std::path::PathBuf>,
    ) -> Result<usize, SearchError> {
        match self
            .reconcile_workspace_files_fenced(present_abs, |apply| FenceOutcome::Applied(apply()))?
        {
            FenceOutcome::Applied(removed) => Ok(removed),
            FenceOutcome::TransientRefusal | FenceOutcome::Superseded | FenceOutcome::Released => {
                unreachable!("the permit-all reconcile fence cannot refuse")
            }
        }
    }

    pub fn reconcile_workspace_files_fenced<F>(
        &mut self,
        present_abs: &HashSet<std::path::PathBuf>,
        mut apply: F,
    ) -> Result<FenceOutcome<usize>, SearchError>
    where
        F: FnMut(
            &mut dyn FnMut() -> Result<(), SearchError>,
        ) -> FenceOutcome<Result<(), SearchError>>,
    {
        let lifecycle_batch = Batch::new(self.store.db_path(), Reason::FileDeleted);
        let result = lifecycle_batch.context().in_scope(|| {
            if self.workspace_roots.is_none() {
                return Ok(FenceOutcome::Applied(0));
            }
            // The present files under the same keying the `code` collection uses, so
            // a file of one root never answers for the same relative path in another.
            let present: HashSet<FileKey> =
                present_abs.iter().filter_map(|p| self.workspace_file_key(p)).collect();
            let carriers = self.carrier_keys()?;
            // A manifest-only key survives its own removal — the row belongs to someone else's
            // corpus and only its hiding is ours to write — so without this the next reconcile
            // would select it again, and every pass would report a removal that changes nothing.
            // Read once, and only for that case: hiding elsewhere proves absence from disk, not
            // a settled key (a clean full pass hides a baseline key while its row lives on).
            let hidden = match self.workspace_overlay_cache.lock() {
                Ok(cache) => cache.hidden_keys(),
                Err(error) => {
                    tracing::warn!("failed to read overlay hidings for a reconcile: {error}");
                    HashSet::new()
                }
            };
            let mut candidates: Vec<_> =
                carriers.all_keys().into_iter().filter(|key| !present.contains(key)).collect();
            candidates.sort();
            let mut batch = RemovedBatch::default();
            for key in candidates {
                if carriers.manifest_is_sole_carrier(&key) && hidden.contains(&key) {
                    continue;
                }
                let has_baseline = carriers.manifest.contains(&key);
                let mut removal = None;
                let admitted = Self::fenced_value(&mut apply, || {
                    removal = Some(self.remove_workspace_key_with(&key, has_baseline));
                    Ok(())
                })?;
                match admitted {
                    FenceOutcome::Applied(()) => {}
                    FenceOutcome::TransientRefusal => return Ok(FenceOutcome::TransientRefusal),
                    FenceOutcome::Superseded => return Ok(FenceOutcome::Superseded),
                    FenceOutcome::Released => return Ok(FenceOutcome::Released),
                }
                match removal.expect("an admitted reconcile operation runs once") {
                    Ok(()) => batch.removed += 1,
                    Err(error) => {
                        tracing::warn!(
                            root = %key.root_id,
                            path = %key.path,
                            "failed to remove a deleted file from the index: {error}"
                        );
                        batch.failed += 1;
                        batch.first_error.get_or_insert(error);
                    }
                }
            }
            if let Some(error) = batch.first_error {
                return Err(SearchError::Index(format!(
                    "reconcile removed {} keys and failed on {}; first failure: {error}",
                    batch.removed, batch.failed
                )));
            }
            Ok(FenceOutcome::Applied(batch.removed))
        });
        lifecycle_batch.finish(match &result {
            Ok(FenceOutcome::Applied(_)) => Outcome::Completed,
            Ok(FenceOutcome::TransientRefusal) => Outcome::Refused,
            Ok(FenceOutcome::Superseded | FenceOutcome::Released) => Outcome::Interrupted,
            Err(_) => Outcome::Failed,
        });
        result
    }

    /// Remove a chosen set of keys, with the carrier reading and the hiding reading already
    /// taken once for the whole batch.
    ///
    /// Shared by every batch removal so the rules that make one correct — skip a key whose
    /// removal would be a no-op, resolve baseline evidence per key, survive a single key's
    /// failure — live in one place rather than being restated per caller.
    fn remove_key_batch(
        &mut self,
        candidates: Vec<FileKey>,
        carriers: &crate::key_carriers::CarrierKeys,
        hidden: &HashSet<FileKey>,
    ) -> RemovedBatch {
        // Sorted so a batch removes in a stable order regardless of hash iteration.
        let mut candidates = candidates;
        candidates.sort();
        let mut batch = RemovedBatch::default();
        for key in candidates {
            // A manifest-only key survives its own removal — the row belongs to someone else's
            // corpus and only its hiding is ours to write — so without this the next pass would
            // select it again and report a removal that changes nothing.
            if carriers.manifest_is_sole_carrier(&key) && hidden.contains(&key) {
                continue;
            }
            let has_baseline = carriers.manifest.contains(&key);
            // A key that cannot be removed does not cost the rest of the batch its pass: each
            // key's carriers are independent, and aborting here would strand every key after
            // the first fault until some later rescan happens to run.
            match self.remove_workspace_key_with(&key, has_baseline) {
                Ok(()) => batch.removed += 1,
                Err(error) => {
                    tracing::warn!(
                        root = %key.root_id,
                        path = %key.path,
                        "failed to remove a deleted file from the index: {error}"
                    );
                    batch.failed += 1;
                    batch.first_error.get_or_insert(error);
                }
            }
        }
        batch
    }

    /// Re-render the stored `graph_context` of every chunk whose owning file was marked
    /// context-dirty (a metadata `.xml` it owns changed), using the freshly published
    /// graph. Only chunks whose context actually changed are rewritten, and only those
    /// have their embedding cleared (NULL) so the existing NULL-embedding embed machinery
    /// re-embeds them; an unchanged context clears the mark and touches nothing. The mark
    /// is cleared for a successfully processed path (an orphan mark for a file no longer in
    /// the store clears too), but a path whose render FAILED keeps its mark so the next
    /// publish retries it. Callers pass the provider built from the just-published graph;
    /// with no graph there is no provider and nothing is called, so marks simply persist.
    ///
    /// `seq_bound` is the mark sequence captured when the publishing build STARTED (see
    /// [`Store::mark_seq_handle`]): only marks at or below it are read and cleared. A drift
    /// that landed after the build started carries a higher `seq` and is left untouched —
    /// its mark is not cleared against a graph that predates it, and a re-mark of an
    /// in-flight path survives the bounded clear. Pass [`i64::MAX`] to consume every mark
    /// (an unbounded caller, e.g. a graph with no wired mark-seq source).
    #[allow(
        clippy::type_complexity,
        reason = "the local permit-all adapter mirrors the existing host checkpoint callback"
    )]
    pub fn refresh_dirty_contexts(
        &self,
        provider: &dyn crate::ports::GraphContextProvider,
        seq_bound: i64,
    ) -> Result<ContextRefreshStats, SearchError> {
        let mut apply = |operation: &mut dyn FnMut(
            &mut dyn FnMut() -> ControlFlow<()>,
        )
            -> ControlFlow<(), Result<(), SearchError>>| {
            let mut checkpoint = || ControlFlow::Continue(());
            match operation(&mut checkpoint) {
                ControlFlow::Break(()) => FenceOutcome::Released,
                ControlFlow::Continue(result) => FenceOutcome::Applied(result),
            }
        };
        let (stats, outcome) =
            self.refresh_dirty_contexts_fenced(provider, seq_bound, false, &mut apply)?;
        match outcome {
            FenceOutcome::Applied(()) => Ok(stats),
            FenceOutcome::TransientRefusal | FenceOutcome::Superseded | FenceOutcome::Released => {
                unreachable!("the permit-all context refresh fence cannot refuse")
            }
        }
    }

    /// Prepare graph-context strings before lease admission, then publish no more than 64
    /// mark/chunk/clear mutations per fenced SQLite transaction. A topology refresh marks every
    /// eligible code file at the graph build's existing sequence bound; a fresher mark is never
    /// overwritten or consumed.
    pub fn refresh_dirty_contexts_fenced<A>(
        &self,
        provider: &dyn crate::ports::GraphContextProvider,
        seq_bound: i64,
        topology_changed: bool,
        apply: &mut A,
    ) -> Result<(ContextRefreshStats, FenceOutcome<()>), SearchError>
    where
        A: FnMut(
            &mut dyn FnMut(
                &mut dyn FnMut() -> ControlFlow<()>,
            ) -> ControlFlow<(), Result<(), SearchError>>,
        ) -> FenceOutcome<Result<(), SearchError>>,
    {
        let lifecycle_batch = Batch::new(self.store.db_path(), Reason::ContextChanged);
        let lifecycle_result = lifecycle_batch.context().in_scope(|| {
            let bounded = self.store.context_dirty_paths_bounded("code", seq_bound)?;
            if topology_changed || !bounded.is_empty() {
                let mut evidence = self.semantic_qualification.get();
                evidence.context_pending = true;
                self.semantic_qualification.set(evidence);
            }
            let mut keys = bounded.clone();
            if topology_changed {
                let all_dirty = self.store.context_dirty_paths("code")?;
                for (key, _) in self.store.all_files_in_collection("code")? {
                    if !all_dirty.contains(&key) || bounded.contains(&key) {
                        keys.insert(key);
                    }
                }
            }
            let mut keys: Vec<_> = keys.into_iter().collect();
            keys.sort();

            let mut mutations = Vec::new();
            let token_claim = self
                .embedder
                .as_ref()
                .and_then(Embedder::token_layout_claim)
                .map(str::to_owned);
            let token_policy = self
                .embedder
                .as_ref()
                .and_then(Embedder::token_policy);
            let mut token_replacements = Vec::new();
            let mut retained_context = false;
            if topology_changed {
                mutations.extend(
                    keys.iter()
                        .cloned()
                        .map(|key| ContextRefreshMutation::Mark { key, seq: seq_bound }),
                );
            }
            for key in keys {
                if token_claim.is_some() {
                    let (Some(embedder), Some(policy), Some(roots)) = (
                        self.embedder.as_ref(),
                        token_policy,
                        self.workspace_roots.as_ref(),
                    ) else {
                        retained_context = true;
                        continue;
                    };
                    let Some(path) = roots.resolve(&key) else {
                        retained_context = true;
                        continue;
                    };
                    let content = match std::fs::read_to_string(&path) {
                        Ok(content) => content,
                        Err(error) => {
                            tracing::warn!(root = %key.root_id, path = %key.path, "context refresh source read failed; keeping dirty mark: {error}");
                            retained_context = true;
                            continue;
                        }
                    };
                    let content_hash = blake3::hash(content.as_bytes()).as_bytes().to_vec();
                    if self.store.file_hash(&key.root_id, &key.path)?.as_deref()
                        != Some(content_hash.as_slice())
                    {
                        retained_context = true;
                        continue;
                    }
                    let mut check = |input: &str| embedder.check_singleton_input(input);
                    let documents = crate::document::prepare_file_documents_with_context(
                        &key,
                        &content,
                        |chunk| match chunk.kind {
                            code_chunk::ChunkKind::Procedure | code_chunk::ChunkKind::Function => {
                                provider
                                    .try_graph_context(&key.path, &chunk.name, chunk.kind.label())
                                    .map_err(|error| SearchError::Index(error.to_string()))
                            }
                            code_chunk::ChunkKind::ModuleHeader => Ok(None),
                        },
                        policy,
                        embedder.document_prefix(),
                        &mut check,
                    );
                    let documents = match documents {
                        Ok(documents) => documents,
                        Err(error) => {
                            tracing::warn!(root = %key.root_id, path = %key.path, "context refresh preparation failed; keeping dirty mark: {error}");
                            retained_context = true;
                            continue;
                        }
                    };
                    let (chunks, contexts, spans) =
                        crate::document::chunks_contexts_and_spans(&documents);
                    token_replacements.push((key.clone(), content_hash, chunks, contexts, spans));
                    lifecycle::decision(
                        self.store.db_path(),
                        &key,
                        Reason::ContextChanged,
                        None,
                        None,
                    );
                    continue;
                }
                // A render error for ANY method of this path keeps the mark: the failure is
                // transient (the graph DB could not be read), so the next publish must retry
                // the whole path rather than clearing it against a half-failed render. A
                // legitimate `Ok(None)` (a method with no graph presence, or a file entirely
                // gone from the graph) is not an error and clears normally.
                let mut render_failed = false;
                let mut updates = Vec::new();
                for (id, symbol_name, kind, stored) in
                    self.store.chunks_with_context_for_file("code", &key.root_id, &key.path)?
                {
                    match provider.try_graph_context(&key.path, &symbol_name, &kind) {
                        Ok(rendered) => {
                            if rendered.as_deref() != stored.as_deref() {
                                updates.push(ContextRefreshMutation::Update {
                                    chunk_id: id,
                                    graph_context: rendered,
                                });
                            }
                        }
                        Err(e) => {
                            render_failed = true;
                            tracing::warn!(
                                root = %key.root_id,
                                path = %key.path,
                                method = %symbol_name,
                                "graph context render failed; keeping dirty mark for retry: {e}"
                            );
                        }
                    }
                }
                lifecycle::decision(
                    self.store.db_path(),
                    &key,
                    if render_failed {
                        Reason::ReadError
                    } else if updates.is_empty() {
                        Reason::Unchanged
                    } else {
                        Reason::ContextChanged
                    },
                    None,
                    None,
                );
                if render_failed {
                    retained_context = true;
                    continue;
                }
                mutations.extend(updates);
                mutations.push(ContextRefreshMutation::Clear { key, seq_bound });
            }

            let mut stats = ContextRefreshStats::default();
            for batch in mutations.chunks(WORKSPACE_APPLY_BATCH_ROWS) {
                let outcome = Self::fenced_checkpointed_value(apply, |checkpoint| {
                    lifecycle::with_reason(Reason::ContextChanged, || {
                    self.store.apply_context_refresh_batch_with_token_layout(
                        batch,
                        self.embedder.as_ref().and_then(Embedder::token_layout_claim),
                        checkpoint,
                    )
                    })
                })?;
                match outcome {
                    FenceOutcome::Applied((marked, updated, cleared)) => {
                        if updated > 0 {
                            self.invalidate_semantic_coverage();
                        }
                        stats.paths_marked += marked;
                        stats.paths_cleared += cleared;
                        stats.chunks_updated += updated;
                        stats.cleared_embeddings += updated;
                    }
                    FenceOutcome::TransientRefusal => {
                        return Ok((stats, FenceOutcome::TransientRefusal));
                    }
                    FenceOutcome::Superseded => return Ok((stats, FenceOutcome::Superseded)),
                    FenceOutcome::Released => return Ok((stats, FenceOutcome::Released)),
                }
            }
            if let Some(token_claim) = token_claim.as_deref() {
                for (key, hash, chunks, contexts, spans) in token_replacements {
                    let (model_id, dimension) = self
                        .embedder
                        .as_ref()
                        .map(|embedder| (embedder.model_id().to_owned(), embedder.dim()))
                        .expect("claim requires embedder");
                    let outcome = Self::fenced_checkpointed_value(apply, |checkpoint| {
                        let reindex = lifecycle::with_reason(Reason::ContextChanged, || {
                            self.store.reindex_file_in_collection_checkpointed_with_source_spans(
                                &key.root_id,
                                &key.path,
                                &hash,
                                "code",
                                &chunks,
                                None,
                                Some(&contexts),
                                Some((&model_id, dimension)),
                                Some(token_claim),
                                &spans,
                                checkpoint,
                            )
                        });
                        match reindex {
                            Err(error) => ControlFlow::Continue(Err(error)),
                            Ok(ControlFlow::Break(())) => ControlFlow::Break(()),
                            Ok(ControlFlow::Continue(_)) => {
                                let clear = [ContextRefreshMutation::Clear {
                                    key: key.clone(),
                                    seq_bound,
                                }];
                                match self.store.apply_context_refresh_batch_with_token_layout(
                                    &clear,
                                    Some(token_claim),
                                    checkpoint,
                                ) {
                                    ControlFlow::Break(()) => ControlFlow::Break(()),
                                    ControlFlow::Continue(Err(error)) => {
                                        ControlFlow::Continue(Err(error))
                                    }
                                    ControlFlow::Continue(Ok(_)) => {
                                        ControlFlow::Continue(Ok(()))
                                    }
                                }
                            }
                        }
                    })?;
                    match outcome {
                        FenceOutcome::Applied(()) => {
                            self.invalidate_semantic_coverage();
                            stats.paths_cleared += 1;
                            stats.chunks_updated += chunks.len();
                            stats.cleared_embeddings += chunks.len();
                        }
                        FenceOutcome::TransientRefusal => {
                            return Ok((stats, FenceOutcome::TransientRefusal));
                        }
                        FenceOutcome::Superseded => {
                            return Ok((stats, FenceOutcome::Superseded));
                        }
                        FenceOutcome::Released => {
                            return Ok((stats, FenceOutcome::Released));
                        }
                    }
                }
            }
            let mut evidence = self.semantic_qualification.get();
            evidence.context_pending =
                retained_context || self.store.mark_seq_handle().load(Ordering::SeqCst) > seq_bound;
            self.semantic_qualification.set(evidence);
            Ok((stats, FenceOutcome::Applied(())))
        });
        let lifecycle_outcome = match &lifecycle_result {
            Ok((_, FenceOutcome::Applied(()))) => Outcome::Completed,
            Ok((_, FenceOutcome::TransientRefusal)) => Outcome::Refused,
            Ok((_, FenceOutcome::Superseded | FenceOutcome::Released)) => Outcome::Interrupted,
            Err(_) => Outcome::Failed,
        };
        lifecycle_batch.finish(lifecycle_outcome);
        lifecycle_result
    }

    /// Declare whether this engine serves an external (remote) baseline. `false`
    /// additionally clears the persisted overlay fingerprint rows: a row claims "verified
    /// against the manifest", and the raw local mode can neither re-verify nor honour that
    /// claim — a file changed at the same stat during the local period would be suppressed by
    /// the inherited row after a switch back. Rows live only under the mode that wrote them.
    pub fn set_serves_external_baseline(&mut self, serves: bool) -> Result<(), SearchError> {
        let mut checkpoint = || ControlFlow::Continue(());
        match self.set_serves_external_baseline_checkpointed(serves, &mut checkpoint) {
            ControlFlow::Continue(result) => result,
            ControlFlow::Break(()) => unreachable!("permit-all checkpoint cannot cancel"),
        }
    }

    pub fn set_serves_external_baseline_checkpointed(
        &mut self,
        serves: bool,
        checkpoint: &mut dyn FnMut() -> ControlFlow<()>,
    ) -> ControlFlow<(), Result<(), SearchError>> {
        let mut observation =
            lifecycle::Record::new(self.store.db_path(), "mode_transition", Reason::ModeTransition);
        observation.snapshot = Some(lifecycle::Snapshot {
            state: "configured",
            mode: Some(if serves { "external_baseline" } else { "local" }.to_owned()),
            ..lifecycle::Snapshot::default()
        });
        observation.emit(false);
        let result = (|| {
            if serves && self.serves_external_baseline {
                return ControlFlow::Continue(Ok(()));
            }
            if !serves {
                match self.store.clear_overlay_fingerprint_cache_checkpointed(checkpoint) {
                    Ok(ControlFlow::Continue(())) => {}
                    Ok(ControlFlow::Break(())) => return ControlFlow::Break(()),
                    Err(error) => return ControlFlow::Continue(Err(error)),
                }
            } else if checkpoint().is_break() {
                return ControlFlow::Break(());
            }
            if self.serves_external_baseline != serves {
                self.invalidate_semantic_coverage();
            }
            self.serves_external_baseline = serves;
            ControlFlow::Continue(Ok(()))
        })();
        observation.outcome = match &result {
            ControlFlow::Break(()) => Outcome::Cancelled,
            ControlFlow::Continue(Ok(())) => Outcome::Completed,
            ControlFlow::Continue(Err(_)) => Outcome::Failed,
        };
        observation.emit(false);
        result
    }

    /// The manifest fingerprints IF this engine serves an external baseline, `None` otherwise.
    /// Every manifest-vs-raw dispatch goes through here: the persisted manifest is a
    /// warm-cache that survives a mode switch, and dispatching on its presence would pin a
    /// local engine to another mode's baseline.
    fn dispatched_manifest_fingerprints(
        &self,
    ) -> Result<Option<HashMap<FileKey, String>>, SearchError> {
        crate::point_refresh::forbidden_under_a_bounded_publication("dispatching the manifest");
        if !self.serves_external_baseline {
            return Ok(None);
        }
        self.store.load_baseline_manifest_fingerprints("code")
    }

    /// How many overlay keys are proven present but unread — the durable retry signal that
    /// outlives the bounded point budget (see `WorkspaceOverlayCache::unread_keys_count`).
    pub fn workspace_overlay_unread_count(&self) -> Result<usize, SearchError> {
        let cache = self
            .workspace_overlay_cache
            .lock()
            .map_err(|e| SearchError::Index(format!("workspace overlay cache lock error: {e}")))?;
        Ok(cache.unread_keys_count())
    }

    /// The retry driver's condition signals, read STRICTLY without side effects: unlike
    /// [`Self::workspace_overlay_stats`], no refresh runs — a condition check that drained
    /// marks or touched the store would itself violate the ownership discipline it serves.
    pub fn workspace_overlay_retry_signals(&self) -> Result<OverlayRetrySignals, SearchError> {
        let cache = self
            .workspace_overlay_cache
            .lock()
            .map_err(|e| SearchError::Index(format!("workspace overlay cache lock error: {e}")))?;
        Ok(OverlayRetrySignals {
            initialized: cache.is_initialized(),
            needs_full_rescan: cache.needs_full_rescan(),
            pending_dirty_paths: cache.dirty_paths_count(),
            unembedded_entries: cache.unembedded_entry_count(),
            unread_keys: cache.unread_keys_count(),
        })
    }

    /// Bounded telemetry only: no engine/overlay wait, clone, scan or lazy refresh.
    pub fn try_workspace_overlay_retry_signals(&self) -> Option<OverlayRetrySignals> {
        let cache = self.workspace_overlay_cache.try_lock().ok()?;
        Some(OverlayRetrySignals {
            initialized: cache.is_initialized(),
            needs_full_rescan: cache.needs_full_rescan(),
            pending_dirty_paths: cache.dirty_paths_count(),
            unembedded_entries: cache.unembedded_entry_count(),
            unread_keys: cache.unread_keys_count(),
        })
    }

    /// Identity captured by the published overlay; never reads the Store or refreshes it.
    pub fn try_workspace_overlay_baseline_identity(&self) -> Option<(String, Option<String>)> {
        self.workspace_overlay_cache.try_lock().ok()?.baseline_identity.clone()
    }

    pub fn workspace_overlay_stats(&self) -> Result<Option<WorkspaceOverlayStats>, SearchError> {
        let Some(roots) = &self.workspace_roots else {
            return Ok(None);
        };
        let mut cache = self
            .workspace_overlay_cache
            .lock()
            .map_err(|e| SearchError::Index(format!("workspace overlay cache lock error: {e}")))?;
        // `search status` is a read-only display; it must never trigger the cold full-tree scan,
        // so it uses the same non-cold-scan path as interactive queries.
        if let Some(manifest_fingerprints) = self.dispatched_manifest_fingerprints()? {
            cache.refresh_with_manifest(
                &manifest_fingerprints,
                roots,
                None,
                self.batch_size,
                &self.store,
                false,
            )?;
        } else {
            cache.refresh(
                &self.store,
                roots,
                None,
                self.batch_size,
                self.workspace_baseline_hash_mode,
                false,
            )?;
        }
        Ok(Some(cache.stats()))
    }

    /// Snapshot status counters without consuming dirty marks, refreshing fingerprints, or
    /// touching the Store. MCP status is observational even after workspace supersession.
    pub fn workspace_overlay_stats_read_only(
        &self,
    ) -> Result<Option<WorkspaceOverlayStats>, SearchError> {
        if self.workspace_roots.is_none() {
            return Ok(None);
        }
        let cache = self
            .workspace_overlay_cache
            .lock()
            .map_err(|e| SearchError::Index(format!("workspace overlay cache lock error: {e}")))?;
        Ok(Some(cache.stats()))
    }

    /// Whether the overlay's last full publication ran over an incomplete scan and withheld its
    /// removals, so only a future clean full scan can catch up. Read-only: takes the cache lock,
    /// refreshes nothing.
    pub fn workspace_overlay_needs_full_rescan(&self) -> Result<bool, SearchError> {
        let cache = self
            .workspace_overlay_cache
            .lock()
            .map_err(|e| SearchError::Index(format!("workspace overlay cache lock error: {e}")))?;
        Ok(cache.needs_full_rescan())
    }

    /// What the workspace overlay still owes a reader, read under ONE lock.
    ///
    /// `initialized` is its own fact, never a conclusion drawn from the counts: an overlay that
    /// has never run a full pass owes marks it cannot even count yet, so both counts read zero
    /// there — the same zero a fully caught-up overlay shows. A caller that reads only the
    /// counts calls that answer complete, and in a profile with no embedder it says so for the
    /// whole life of the daemon.
    pub fn workspace_overlay_debt(&self) -> Result<WorkspaceOverlayDebt, SearchError> {
        let cache = self
            .workspace_overlay_cache
            .lock()
            .map_err(|e| SearchError::Index(format!("workspace overlay cache lock error: {e}")))?;
        Ok(WorkspaceOverlayDebt {
            initialized: cache.is_initialized(),
            pending: cache.point_backlog(),
            unread: cache.unread_keys_count(),
        })
    }

    /// How many dirty marks a point refresh could answer now: none before the overlay is
    /// initialized, whose first full refresh answers them instead. O(1).
    pub fn workspace_overlay_point_backlog(&self) -> Result<usize, SearchError> {
        let cache = self
            .workspace_overlay_cache
            .lock()
            .map_err(|e| SearchError::Index(format!("workspace overlay cache lock error: {e}")))?;
        Ok(cache.point_backlog())
    }

    /// Phase A of a point refresh (see [`crate::point_refresh`]): up to `limit` dirty keys, the
    /// oldest marks first, with the fences phase C re-checks and the inputs phase B needs. A
    /// moment under the caller's engine lock; `None` when the overlay has nothing to answer.
    pub fn capture_point_refresh(
        &self,
        limit: usize,
    ) -> Result<Option<crate::point_refresh::PointCapture>, SearchError> {
        let Some(roots) = self.workspace_roots.clone() else {
            return Ok(None);
        };
        let (keys, fence, wholesale_seq, provider) = {
            let cache = self.workspace_overlay_cache.lock().map_err(|e| {
                SearchError::Index(format!("workspace overlay cache lock error: {e}"))
            })?;
            let keys = cache.capture_point_keys(limit);
            let (fence, wholesale_seq) = cache.point_fences();
            (keys, fence, wholesale_seq, cache.graph_context_provider_handle())
        };
        if keys.is_empty() {
            return Ok(None);
        }
        Ok(Some(crate::point_refresh::PointCapture {
            keys,
            fence,
            wholesale_seq,
            roots_epoch: self.workspace_roots_epoch,
            baseline_version: self.store.baseline_version()?,
            roots,
            hash_mode: self.workspace_baseline_hash_mode,
            serves_external_baseline: self.serves_external_baseline,
            provider,
            source: self.module_snapshot_source.clone(),
            batch_size: self.batch_size,
            db_path: self.store.db_path().to_path_buf(),
        }))
    }

    /// Phase C of a point refresh: settle what `batch` prepared, unless the state it was
    /// prepared against moved. Runs under the caller's engine lock and ownership fence, and
    /// does only bounded work there — anything that would load a whole baseline or read a
    /// file panics in a debug build (see [`crate::point_refresh::BoundedPublication`]). On
    /// [`crate::PointPublish::Busy`] the batch is untouched and may be published again.
    pub fn publish_point_refresh(
        &self,
        batch: &mut crate::point_refresh::PreparedPointBatch,
    ) -> Result<crate::point_refresh::PointPublish, SearchError> {
        use crate::point_refresh::PointPublish;
        let _bounded = crate::point_refresh::BoundedPublication::enter();
        if self.workspace_roots_epoch != batch.roots_epoch {
            return Ok(PointPublish::Discarded);
        }
        // The baseline mode is a fence of its own: `set_serves_external_baseline` moves neither
        // `roots_epoch` nor `wholesale_seq` nor `baseline_version`, so without this check a
        // batch classified in one mode could settle in the other. The roots plan already treats
        // a mode change as superseding; the point path was reading a classification made under
        // rules that no longer hold. An in-memory flag, so nothing here reads a file.
        if self.serves_external_baseline != batch.serves_external_baseline {
            return Ok(PointPublish::Discarded);
        }
        let mut cache = self
            .workspace_overlay_cache
            .lock()
            .map_err(|e| SearchError::Index(format!("workspace overlay cache lock error: {e}")))?;
        if !cache.point_batch_stands(batch) {
            return Ok(PointPublish::Discarded);
        }
        let applicable = cache.applicable_point_keys(batch);
        let retract: Vec<FileKey> = batch
            .keys
            .iter()
            .zip(&applicable)
            .filter(|(prepared, applies)| **applies && prepared.retract)
            .map(|(prepared, _)| prepared.captured.key.clone())
            .collect();
        match self.store.commit_point_refresh(
            batch.baseline_version,
            &retract,
            crate::point_refresh::C_BUSY_TIMEOUT,
        )? {
            crate::store::PointCommit::Busy => return Ok(PointPublish::Busy),
            crate::store::PointCommit::VersionMoved => return Ok(PointPublish::Discarded),
            crate::store::PointCommit::Committed => {}
        }
        let stale = applicable.iter().filter(|applies| !**applies).count();
        let (settled, cleared) = cache.settle_point_batch(batch, &applicable);
        Ok(PointPublish::Applied { settled, cleared, stale, remaining: cache.point_backlog() })
    }

    /// In-engine overlay prime that may embed inline (holds the engine lock for its duration).
    /// Reserved for the no-baseline / local paths and tests; the PostgresRemoteOverlay warmup must
    /// NOT use this (it would serialize all search behind a multi-minute embed) and instead drives
    /// the lock-free [`Self::prime_workspace_overlay_standalone`] + [`Self::publish_workspace_overlay`].
    pub fn prime_workspace_overlay(&self) -> Result<(), SearchError> {
        self.refresh_workspace_overlay_snapshot(true)?;
        Ok(())
    }

    /// Prepare the cold overlay refresh off the caller's publication barrier, then swap the
    /// completed in-memory cache while that barrier is held. `Prime` is a local-baseline path;
    /// its clone refresh reads/scans/embeds but writes no shared Store rows.
    ///
    /// An embedding failure does not withhold the overlay: the refresh reached its tail, so the
    /// cache is installed and the typed failure is returned only once the barrier applied it.
    /// The refused keys keep a dirty mark for the next refresh.
    pub fn prime_workspace_overlay_fenced<F>(
        &self,
        mut apply: F,
    ) -> Result<FenceOutcome<()>, SearchError>
    where
        F: FnMut(
            &mut dyn FnMut() -> Result<(), SearchError>,
        ) -> FenceOutcome<Result<(), SearchError>>,
    {
        if self.workspace_roots.is_none() {
            return Ok(FenceOutcome::Applied(()));
        }
        let roots = self.workspace_roots.as_ref().expect("checked above");
        let mut prepared = self
            .workspace_overlay_cache
            .lock()
            .map_err(|e| SearchError::Index(format!("workspace overlay cache lock error: {e}")))?
            .clone();
        let embedding_failure = match prepared.refresh(
            &self.store,
            roots,
            self.embedder.as_ref(),
            self.batch_size,
            self.workspace_baseline_hash_mode,
            true,
        ) {
            Ok(()) => None,
            Err(error) => Some(error.embedding_failure().ok_or(error)?),
        };
        let outcome = Self::fenced_value(&mut apply, || {
            *self.workspace_overlay_cache.lock().map_err(|e| {
                SearchError::Index(format!("workspace overlay cache lock error: {e}"))
            })? = prepared;
            Ok(())
        })?;
        match (outcome, embedding_failure) {
            (FenceOutcome::Applied(()), Some(failure)) => Err(failure.into()),
            (outcome, _) => Ok(outcome),
        }
    }

    /// Mark the workspace overlay initialized with zero entries, WITHOUT a disk scan. The caller
    /// must have proven the SQLite store was just reconciled with disk at boot (a fused parse
    /// ingest, or an `index_directory_deferred`/`index_directory_fts` walk+hash re-ingest), so the
    /// overlay baseline already equals the working tree and a prime would find no diffs. Zero cost,
    /// zero RAM — and, unlike a prime, robust to how the boot hashed files, because it asserts the
    /// reconciled invariant directly rather than re-deriving it. Flips the same `initialized` flag a
    /// prime would, so the resident-fed incremental reindex (inert until initialized) goes live.
    pub fn initialize_workspace_overlay_clean(&self) -> Result<(), SearchError> {
        if self.workspace_roots.is_none() {
            return Ok(());
        }
        // An unreadable manifest leaves the identity unknown, which only withholds readiness.
        let identity = if self.serves_external_baseline {
            self.store
                .load_baseline_manifest()
                .ok()
                .flatten()
                .map(|r| (r.snapshot_id, r.fingerprint))
        } else {
            None
        };
        let mut cache = self
            .workspace_overlay_cache
            .lock()
            .map_err(|e| SearchError::Index(format!("workspace overlay cache lock error: {e}")))?;
        cache.mark_initialized_clean();
        cache.baseline_identity = identity;
        Ok(())
    }

    /// The embedder configuration of this engine, if semantic search is configured. The warmup
    /// thread clones this under a brief lock so it can build a standalone embedder for the
    /// lock-free embedding pass.
    pub fn embedder_config(&self) -> Option<EmbedderConfig> {
        self.embedder.as_ref().map(Embedder::config)
    }

    /// The path of this engine's SQLite database, for reopening a standalone connection off-lock.
    pub fn db_path(&self) -> &Path {
        self.store.db_path()
    }

    /// The injected graph-context provider, cloned for the standalone overlay prime so its
    /// embeddings are graph-enriched exactly like an in-engine refresh.
    pub fn graph_context_provider(&self) -> Option<Arc<dyn crate::ports::GraphContextProvider>> {
        self.graph_context_provider.clone()
    }

    /// A read-only clone of the overlay embedding cache, for the warmup's lock-free Phase B start.
    pub fn workspace_overlay_embedding_cache_snapshot(
        &self,
    ) -> Result<HashMap<String, Vec<f32>>, SearchError> {
        let cache = self
            .workspace_overlay_cache
            .lock()
            .map_err(|e| SearchError::Index(format!("workspace overlay cache lock error: {e}")))?;
        Ok(cache.embedding_cache_snapshot())
    }

    /// Phase A + B of the lock-free overlay warmup: plan the manifest-driven refresh against a
    /// freshly reopened standalone [`Store`] (Phase A, read-only), then embed the missing chunks
    /// via the remote embedder with no engine/inner lock held (Phase B). Returns the plan and the
    /// embeddings it produced for [`Self::publish_workspace_overlay`] (Phase C) to merge in.
    ///
    /// `Store` is `!Sync`, so this opens its own connection from `db_path` rather than borrowing
    /// the live engine's store. Newly embedded vectors are persisted to that standalone store at
    /// the end of Phase B so a crash mid-warmup does not throw away embedding work already paid
    /// for; Phase C persists the merged live cache once more.
    #[allow(
        clippy::too_many_arguments,
        clippy::type_complexity,
        reason = "the standalone pass returns its existing plan/vector pair plus the typed fence outcome; one-use aliases or options would only rename them"
    )]
    pub fn prime_workspace_overlay_standalone<F>(
        db_path: &Path,
        embedder_config: EmbedderConfig,
        roots: &WorkspaceRoots,
        warm_embeddings: HashMap<String, Vec<f32>>,
        graph_provider: Option<Arc<dyn crate::ports::GraphContextProvider>>,
        should_continue: &dyn Fn() -> bool,
        apply: F,
        distrusted: &HashSet<FileKey>,
    ) -> Result<FenceOutcome<(RefreshPlan, HashMap<String, Vec<f32>>)>, SearchError>
    where
        F: FnMut(
            &mut dyn FnMut() -> Result<(), SearchError>,
        ) -> FenceOutcome<Result<(), SearchError>>,
    {
        Self::prime_workspace_overlay_standalone_retrying(
            db_path,
            embedder_config,
            roots,
            warm_embeddings,
            graph_provider,
            should_continue,
            apply,
            distrusted,
            || false,
        )
    }

    #[allow(clippy::too_many_arguments, clippy::type_complexity)]
    pub fn prime_workspace_overlay_standalone_retrying<F, R>(
        db_path: &Path,
        embedder_config: EmbedderConfig,
        roots: &WorkspaceRoots,
        warm_embeddings: HashMap<String, Vec<f32>>,
        graph_provider: Option<Arc<dyn crate::ports::GraphContextProvider>>,
        should_continue: &dyn Fn() -> bool,
        mut apply: F,
        distrusted: &HashSet<FileKey>,
        mut retry_transient: R,
    ) -> Result<FenceOutcome<(RefreshPlan, HashMap<String, Vec<f32>>)>, SearchError>
    where
        F: FnMut(
            &mut dyn FnMut() -> Result<(), SearchError>,
        ) -> FenceOutcome<Result<(), SearchError>>,
        R: FnMut() -> bool,
    {
        embedder_config.validate()?;
        let batch_size = EmbeddingExecutionPolicy::default().batch_size();
        // `open_existing`, not `open`: this standalone pass runs while another daemon may own
        // the workspace, and the migrating constructor could wipe and recreate the owner's
        // tables on a schema mismatch. A pass has no business migrating anything.
        let store = Store::open_existing(db_path)?;
        let embedder = Embedder::new(embedder_config);
        Self::ensure_store_token_layout_claim(&store, &embedder)?;

        // Seed the warm cache from the persisted overlay embedding cache so a restart reuses
        // vectors already paid for instead of re-embedding everything.
        let mut warm_embeddings = warm_embeddings;
        if warm_embeddings.is_empty() {
            match store.load_overlay_embedding_cache(embedder.model_id(), embedder.dim()) {
                Ok(cached) if !cached.is_empty() => {
                    info!(
                        model_id = embedder.model_id(),
                        dim = embedder.dim(),
                        cached_embeddings = cached.len(),
                        "loaded persisted overlay embedding cache for standalone prime"
                    );
                    warm_embeddings = cached;
                }
                _ => {}
            }
        }

        let manifest_fingerprints =
            store.load_baseline_manifest_fingerprints("code")?.unwrap_or_default();

        let plan = WorkspaceOverlayCache::plan_full_refresh_from_manifest_with_embedder(
            &manifest_fingerprints,
            roots,
            &store,
            &warm_embeddings,
            graph_provider.as_deref(),
            distrusted,
            Some(&embedder),
        )?;

        let mut new_embeddings = match Self::embed_missing_overlay_chunks(
            &store,
            &embedder,
            plan.missing_embeddings(),
            batch_size,
            should_continue,
            &mut apply,
            &mut retry_transient,
        )? {
            FenceOutcome::Applied(embeddings) => embeddings,
            FenceOutcome::TransientRefusal => return Ok(FenceOutcome::TransientRefusal),
            FenceOutcome::Superseded => return Ok(FenceOutcome::Superseded),
            FenceOutcome::Released => return Ok(FenceOutcome::Released),
        };

        // Include the warm-reused vectors for the plan's chunks in the published set so Phase C
        // builds complete vectors regardless of the live cache's state (it may be empty on a
        // fresh engine). The embedding key is value stable, so this is a no-op merge for chunks
        // the live cache already holds.
        for embedding_key in plan.planned_embedding_keys() {
            if let std::collections::hash_map::Entry::Vacant(slot) =
                new_embeddings.entry(embedding_key)
            {
                if let Some(embedding) = warm_embeddings.get(slot.key()) {
                    slot.insert(embedding.clone());
                }
            }
        }

        Ok(FenceOutcome::Applied((plan, new_embeddings)))
    }

    /// Phase B: embed the plan's missing `embedding_key -> input` pairs in batches off any lock,
    /// persisting each batch's vectors to the standalone `store` as it lands so a mid-pass crash
    /// keeps the progress already paid for.
    fn embed_missing_overlay_chunks<F>(
        store: &Store,
        embedder: &Embedder,
        missing: &HashMap<String, String>,
        batch_size: usize,
        should_continue: &dyn Fn() -> bool,
        apply: &mut F,
        retry_transient: &mut dyn FnMut() -> bool,
    ) -> Result<FenceOutcome<HashMap<String, Vec<f32>>>, SearchError>
    where
        F: FnMut(
            &mut dyn FnMut() -> Result<(), SearchError>,
        ) -> FenceOutcome<Result<(), SearchError>>,
    {
        if missing.is_empty() {
            return Ok(FenceOutcome::Applied(HashMap::new()));
        }

        let pairs: Vec<(&String, &String)> = missing.iter().collect();
        let inputs: Vec<&str> = pairs.iter().map(|(_, input)| input.as_str()).collect();
        let ranges = embedder.batch_ranges(&inputs, batch_size)?;
        let mut new_embeddings = HashMap::with_capacity(missing.len());

        for range in ranges {
            let batch = &pairs[range.clone()];
            // Checked between batches, like the collection embed pass: each batch persists
            // vectors to the shared store, and a caller that lost the workspace lease must
            // stop writing over the new owner's rows.
            if !should_continue() {
                return Ok(FenceOutcome::Released);
            }
            match Self::fenced_value_retrying(apply, || Ok(()), retry_transient)? {
                FenceOutcome::Applied(()) => {}
                FenceOutcome::TransientRefusal => return Ok(FenceOutcome::TransientRefusal),
                FenceOutcome::Superseded => return Ok(FenceOutcome::Superseded),
                FenceOutcome::Released => return Ok(FenceOutcome::Released),
            }
            let result = embedder.embed_batch_interactive(&inputs[range]);
            if !should_continue() {
                return Ok(FenceOutcome::Released);
            }
            let embeddings = match result {
                Ok(embeddings) => embeddings,
                Err(error) => {
                    return match Self::fenced_value_retrying(apply, || Ok(()), retry_transient)? {
                        FenceOutcome::Applied(()) => Err(error),
                        FenceOutcome::Released => Ok(FenceOutcome::Released),
                        FenceOutcome::Superseded => Ok(FenceOutcome::Superseded),
                        FenceOutcome::TransientRefusal => Ok(FenceOutcome::TransientRefusal),
                    };
                }
            };

            let mut batch_persist = HashMap::with_capacity(batch.len());
            for ((embedding_key, _), embedding) in batch.iter().zip(embeddings) {
                batch_persist.insert((*embedding_key).clone(), embedding.clone());
                new_embeddings.insert((*embedding_key).clone(), embedding);
            }
            // Re-checked AFTER the embed round-trip too: the stop signal may have changed while
            // the request was in flight. The actual SQLite save is separately fenced below.
            if !should_continue() {
                return Ok(FenceOutcome::Released);
            }
            // Persist to the standalone store (NOT the live engine) so partial progress survives
            // a mid-pass failure. The callback holds the host ownership barrier for this one
            // atomic SQLite batch; a refusal stops before touching the shared store.
            let admitted = Self::fenced_value_retrying(
                apply,
                || {
                    store.save_overlay_embedding_cache(
                        embedder.model_id(),
                        embedder.dim(),
                        &batch_persist,
                    )?;
                    Ok(())
                },
                retry_transient,
            )?;
            match admitted {
                FenceOutcome::Applied(()) => {}
                FenceOutcome::TransientRefusal => return Ok(FenceOutcome::TransientRefusal),
                FenceOutcome::Superseded => return Ok(FenceOutcome::Superseded),
                FenceOutcome::Released => return Ok(FenceOutcome::Released),
            }
        }

        Ok(FenceOutcome::Applied(new_embeddings))
    }

    /// Phase C: merge the plan and Phase-B embeddings into the live overlay cache under a brief
    /// inner-cache lock, swapping the entry/hidden-path set atomically so a concurrent reader
    /// never sees a half-embedded file. Never holds the lock across an embed.
    /// Returns how many marked keys the plan's gate skipped unread — see
    /// [`WorkspaceOverlayCache::publish_plan`].
    pub fn publish_workspace_overlay(
        &self,
        plan: RefreshPlan,
        new_embeddings: HashMap<String, Vec<f32>>,
        baseline: &PublicationBaseline,
    ) -> Result<PublishOutcome, SearchError> {
        let mut cache = self
            .workspace_overlay_cache
            .lock()
            .map_err(|e| SearchError::Index(format!("workspace overlay cache lock error: {e}")))?;
        cache.publish_plan(plan, new_embeddings, baseline, self.embedder.as_ref(), &self.store)
    }

    /// Build the complete Phase-C map and persistence bundle outside the ownership fence.
    pub fn stage_workspace_overlay_publication(
        &self,
        plan: RefreshPlan,
        new_embeddings: HashMap<String, Vec<f32>>,
        baseline: &PublicationBaseline,
    ) -> Result<ValidatedWorkspaceOverlayPublication, SearchError> {
        let cache = self
            .workspace_overlay_cache
            .lock()
            .map_err(|e| SearchError::Index(format!("workspace overlay cache lock error: {e}")))?;
        let staging = cache.stage_plan(plan, new_embeddings, baseline)?;
        Ok(ValidatedWorkspaceOverlayPublication {
            staging: Some(staging),
            expected: cache.publication_baseline(),
            embedding_identity: self
                .embedder
                .as_ref()
                .map(|embedder| (embedder.model_id().to_owned(), embedder.dim())),
        })
    }

    /// Commit one staged Phase-C bundle and swap its map only after the Store transaction lands.
    pub fn apply_staged_workspace_overlay_publication(
        &self,
        publication: &mut ValidatedWorkspaceOverlayPublication,
        checkpoint: &mut dyn FnMut() -> ControlFlow<()>,
    ) -> ControlFlow<(), Result<PublishOutcome, SearchError>> {
        let mut cache = match self.workspace_overlay_cache.lock() {
            Ok(cache) => cache,
            Err(error) => {
                return ControlFlow::Continue(Err(SearchError::Index(format!(
                    "workspace overlay cache lock error: {error}"
                ))));
            }
        };
        if cache.publication_baseline() != publication.expected {
            return ControlFlow::Continue(Ok(PublishOutcome::Superseded));
        }
        let Some(staging) = publication.staging.as_ref() else {
            return ControlFlow::Continue(Err(SearchError::Index(
                "workspace overlay publication was already consumed".to_owned(),
            )));
        };
        let embedding_cache = staging.next_cache.embedding_cache_snapshot();
        let embedding = publication
            .embedding_identity
            .as_ref()
            .map(|(model, dim)| (model.as_str(), *dim, &embedding_cache));
        match self.store.apply_overlay_publication_with_token_layout(
            staging.fingerprints.as_ref().map(|(id, rows)| (id.as_str(), rows)),
            embedding,
            self.embedder.as_ref().and_then(Embedder::token_layout_claim),
            checkpoint,
        ) {
            Ok(ControlFlow::Break(())) => return ControlFlow::Break(()),
            Err(error) => return ControlFlow::Continue(Err(error)),
            Ok(ControlFlow::Continue(())) => {}
        }
        let staging =
            publication.staging.take().expect("staging checked immediately before the transaction");
        *cache = staging.next_cache;
        ControlFlow::Continue(Ok(staging.outcome))
    }

    /// The atomic pre-plan snapshot (live marks + freshness fence) a planned publication is
    /// judged against; captured under the cache lock before the lock-free Phase A/B.
    pub fn workspace_overlay_publication_baseline(
        &self,
    ) -> Result<PublicationBaseline, SearchError> {
        let cache = self
            .workspace_overlay_cache
            .lock()
            .map_err(|e| SearchError::Index(format!("workspace overlay cache lock error: {e}")))?;
        Ok(cache.publication_baseline())
    }

    /// Snapshot the overlay dirty-path set (path -> mark sequence). Taken under the cache lock
    /// before the warmup's lock-free embed pass so [`Self::publish_workspace_overlay`] clears only
    /// the flags that pass supersedes, never one the watcher re-marked while the embed was in flight.
    pub fn workspace_overlay_dirty_paths_snapshot(
        &self,
    ) -> Result<HashMap<FileKey, u64>, SearchError> {
        let cache = self
            .workspace_overlay_cache
            .lock()
            .map_err(|e| SearchError::Index(format!("workspace overlay cache lock error: {e}")))?;
        Ok(cache.dirty_paths_snapshot())
    }

    pub fn workspace_overlay_lexical_hits(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<(Vec<SearchHit>, HashSet<FileKey>), SearchError> {
        if self.workspace_roots.is_none() {
            return Ok((Vec::new(), HashSet::new()));
        }
        let overlay = self.refresh_workspace_overlay_snapshot(false)?;
        if overlay.is_empty() {
            return Ok((Vec::new(), HashSet::new()));
        }
        let hits = lexical_hits(&overlay, query, limit);
        Ok((hits, overlay.hidden_paths.clone()))
    }

    pub fn workspace_overlay_lexical_hits_read_only(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<(Vec<SearchHit>, HashSet<FileKey>), SearchError> {
        if self.workspace_roots.is_none() {
            return Ok((Vec::new(), HashSet::new()));
        }
        let overlay = self.workspace_overlay_snapshot()?;
        Ok((lexical_hits(&overlay, query, limit), overlay.hidden_paths))
    }

    pub fn workspace_overlay_semantic_hits(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<(Vec<SearchHit>, HashSet<FileKey>), SearchError> {
        if self.workspace_roots.is_none() {
            return Ok((Vec::new(), HashSet::new()));
        }
        let Some(embedder) = &self.embedder else {
            return Ok((Vec::new(), HashSet::new()));
        };
        // The resident snapshot contains only vectors a prior fenced publication installed.
        // Chunks lacking one remain lexical; the embedder below is used only for the query vector.
        let overlay = self.refresh_workspace_overlay_snapshot(false)?;
        if overlay.is_empty() {
            return Ok((Vec::new(), HashSet::new()));
        }
        let query_embedding = embedder.embed(query)?;
        let hits = semantic_hits(&overlay, &query_embedding, limit);
        Ok((hits, overlay.hidden_paths.clone()))
    }

    /// Overlay semantic hits from a caller-supplied query vector (embedded off the engine lock),
    /// so the direct/Postgres path embeds once instead of re-embedding here.
    pub fn workspace_overlay_semantic_hits_with_embedding(
        &self,
        query_embedding: &[f32],
        limit: usize,
    ) -> Result<(Vec<SearchHit>, HashSet<FileKey>), SearchError> {
        if self.workspace_roots.is_none() {
            return Ok((Vec::new(), HashSet::new()));
        }
        if self.embedder.is_none() {
            return Ok((Vec::new(), HashSet::new()));
        }
        let overlay = self.refresh_workspace_overlay_snapshot(false)?;
        if overlay.is_empty() {
            return Ok((Vec::new(), HashSet::new()));
        }
        let hits = semantic_hits(&overlay, query_embedding, limit);
        Ok((hits, overlay.hidden_paths.clone()))
    }

    pub fn workspace_overlay_semantic_hits_with_embedding_read_only(
        &self,
        query_embedding: &[f32],
        limit: usize,
    ) -> Result<(Vec<SearchHit>, HashSet<FileKey>), SearchError> {
        if self.workspace_roots.is_none() || self.embedder.is_none() {
            return Ok((Vec::new(), HashSet::new()));
        }
        let overlay = self.workspace_overlay_snapshot()?;
        Ok((semantic_hits(&overlay, query_embedding, limit), overlay.hidden_paths))
    }

    pub fn resolve_workspace_code_view(&self) -> Result<Option<ResolvedView>, SearchError> {
        if self.workspace_roots.is_none() {
            return Ok(None);
        }
        let baseline =
            BaselineRef::for_snapshot(CorpusId::WorkspaceCode, "local-workspace-baseline");
        let mut overlay = self.workspace_overlay_snapshot()?.overlay;
        overlay.baseline = baseline.clone();
        BaselineOverlaySearchService::new(
            LocalStoreBaselineAdapter::workspace_code(&self.store).with_token_layout_claim(
                self.embedder.as_ref().and_then(Embedder::token_layout_claim),
            ),
            LocalStoreBaselineAdapter::workspace_code(&self.store).with_token_layout_claim(
                self.embedder.as_ref().and_then(Embedder::token_layout_claim),
            ),
            InMemoryResolvedViewResolver,
        )
        .resolve_view(baseline, overlay)
    }

    /// The counts [`Self::resolve_workspace_code_view`] would report, without the view:
    /// the status line that asks for them needs two numbers, and the view behind them
    /// is the whole indexed corpus loaded into memory.
    pub fn resolve_workspace_code_view_counts(
        &self,
    ) -> Result<Option<ResolvedViewCounts>, SearchError> {
        if self.workspace_roots.is_none() {
            return Ok(None);
        }
        let baseline =
            BaselineRef::for_snapshot(CorpusId::WorkspaceCode, "local-workspace-baseline");
        let mut overlay = self.workspace_overlay_snapshot()?.overlay;
        overlay.baseline = baseline.clone();
        let per_file = self.store.count_indexed_documents_by_file(
            "code",
            self.embedder.as_ref().and_then(Embedder::token_layout_claim),
        )?;
        InMemoryResolvedViewResolver.resolve_counts(baseline, per_file, overlay).map(Some)
    }

    pub fn resolve_workspace_code_view_with<C, S>(
        &self,
        baseline: BaselineRef,
        catalog: C,
        content_store: S,
    ) -> Result<Option<ResolvedView>, SearchError>
    where
        C: SnapshotCatalog,
        S: SnapshotContentStore,
    {
        if self.workspace_roots.is_none() {
            return Ok(None);
        }

        let overlay = self.refresh_workspace_overlay_snapshot(false)?;
        let mut overlay = overlay.overlay;
        overlay.baseline = baseline.clone();
        let service =
            BaselineOverlaySearchService::new(catalog, content_store, InMemoryResolvedViewResolver);

        service.resolve_view(baseline, overlay)
    }

    pub fn resolve_workspace_code_view_from_documents(
        &self,
        baseline: BaselineRef,
        baseline_documents: Vec<crate::IndexedDocument>,
    ) -> Result<Option<ResolvedView>, SearchError> {
        if self.workspace_roots.is_none() {
            return Ok(None);
        }

        let overlay = self.refresh_workspace_overlay_snapshot(false)?;
        let mut overlay = overlay.overlay;
        overlay.baseline = baseline.clone();

        InMemoryResolvedViewResolver.resolve(baseline, baseline_documents, overlay).map(Some)
    }

    pub fn search(
        &self,
        query: &str,
        limit: usize,
        collection: Option<&str>,
    ) -> Result<Vec<SearchHit>, SearchError> {
        if collection == Some("code") {
            if let Some(overlay_hits) = self.search_with_workspace_overlay(query, limit)? {
                return Ok(overlay_hits);
            }
        }

        self.search_persisted(query, limit, collection)
    }

    /// Clone the configured embedder (rebuilds its HTTP agents from config), so the request path
    /// can embed the query *without* holding the engine lock. `None` when semantic is unconfigured.
    pub fn embedder_clone(&self) -> Option<Embedder> {
        self.embedder.clone()
    }

    /// Run a code search from a query vector embedded by the caller (off the engine lock), instead
    /// of embedding inline. Mirrors [`SearchEngine::search`] minus the embed step.
    pub fn search_with_embedding(
        &self,
        query_embedding: &[f32],
        limit: usize,
        collection: Option<&str>,
    ) -> Result<Vec<SearchHit>, SearchError> {
        if collection == Some("code") {
            if let Some(overlay_hits) =
                self.search_with_workspace_overlay_embedding(query_embedding, limit, true)?
            {
                return Ok(overlay_hits);
            }
        }

        self.search_persisted_with_embedding(query_embedding, limit, collection)
    }

    pub fn search_with_embedding_read_only(
        &self,
        query_embedding: &[f32],
        limit: usize,
        collection: Option<&str>,
    ) -> Result<Vec<SearchHit>, SearchError> {
        if collection == Some("code") {
            if let Some(hits) =
                self.search_with_workspace_overlay_embedding(query_embedding, limit, false)?
            {
                return Ok(hits);
            }
        }
        self.search_persisted_with_embedding(query_embedding, limit, collection)
    }

    fn search_persisted(
        &self,
        query: &str,
        limit: usize,
        collection: Option<&str>,
    ) -> Result<Vec<SearchHit>, SearchError> {
        let embedder = self.embedder.as_ref().ok_or_else(|| {
            SearchError::Embedder(
                "Semantic search not configured. Set EMBEDDING_URL to enable.".into(),
            )
        })?;
        let query_embedding = embedder.embed(query)?;
        self.search_persisted_with_embedding(&query_embedding, limit, collection)
    }

    /// The persisted-search body after the query has already been embedded, so callers that
    /// embed once (the overlay merge, the lock-free request path) need not embed again.
    fn search_persisted_with_embedding(
        &self,
        query_embedding: &[f32],
        limit: usize,
        collection: Option<&str>,
    ) -> Result<Vec<SearchHit>, SearchError> {
        let fetch_limit = if collection.is_some() { limit * 3 } else { limit };
        let results = self.index.search(query_embedding, fetch_limit)?;

        let ids: Vec<i64> = results.iter().map(|result| result.chunk_id).collect();
        let infos = self.store.chunks_by_ids(&ids)?;

        let mut hits = Vec::with_capacity(limit);
        for result in results {
            if hits.len() >= limit {
                break;
            }
            if let Some(info) = infos.get(&result.chunk_id).cloned() {
                if let Some(coll) = collection {
                    if info.collection != coll {
                        continue;
                    }
                }
                hits.push(SearchHit {
                    collection: info.collection,
                    root_id: info.root_id,
                    file_path: info.file_path,
                    symbol_name: info.symbol_name,
                    kind: info.kind,
                    text: info.text,
                    line_start: info.line_start,
                    line_end: info.line_end,
                    score: result.score,
                    source_span: info.source_span,
                });
            }
        }

        Ok(hits)
    }

    fn search_with_workspace_overlay(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Option<Vec<SearchHit>>, SearchError> {
        if self.workspace_roots.is_none() {
            return Ok(None);
        }
        let Some(embedder) = &self.embedder else {
            return Ok(None);
        };
        // Snapshot before embedding so an empty overlay returns `None` without paying for a query
        // embed the persisted fallback would only repeat.
        let overlay = self.refresh_workspace_overlay_snapshot(false)?;
        if overlay.is_empty() {
            return Ok(None);
        }
        let query_embedding = embedder.embed(query)?;
        let mut combined =
            self.search_persisted_with_embedding(&query_embedding, limit * 3, Some("code"))?;
        combined.retain(|hit| {
            !overlay.hidden_paths.contains(&FileKey::new(&hit.root_id, &hit.file_path))
        });
        combined.extend(semantic_hits(&overlay, &query_embedding, limit));
        combined.sort_by(|lhs, rhs| rhs.score.total_cmp(&lhs.score));
        combined.truncate(limit);
        Ok(Some(combined))
    }

    /// The overlay-merged code search after the query has already been embedded, so the request
    /// path can embed once off the engine lock and the persisted fetch never re-embeds.
    fn search_with_workspace_overlay_embedding(
        &self,
        query_embedding: &[f32],
        limit: usize,
        refresh: bool,
    ) -> Result<Option<Vec<SearchHit>>, SearchError> {
        if self.workspace_roots.is_none() {
            return Ok(None);
        }

        let overlay = if refresh {
            self.refresh_workspace_overlay_snapshot(false)?
        } else {
            self.workspace_overlay_snapshot()?
        };
        if overlay.is_empty() {
            return Ok(None);
        }

        let mut combined =
            self.search_persisted_with_embedding(query_embedding, limit * 3, Some("code"))?;
        combined.retain(|hit| {
            !overlay.hidden_paths.contains(&FileKey::new(&hit.root_id, &hit.file_path))
        });
        combined.extend(semantic_hits(&overlay, query_embedding, limit));
        combined.sort_by(|lhs, rhs| rhs.score.total_cmp(&lhs.score));
        combined.truncate(limit);
        Ok(Some(combined))
    }

    pub fn text_search(
        &self,
        query: &str,
        limit: usize,
        collection: Option<&str>,
    ) -> Result<Vec<SearchHit>, SearchError> {
        if collection == Some("code") {
            if let Some(overlay_hits) =
                self.text_search_with_workspace_overlay(query, limit, true)?
            {
                return Ok(overlay_hits);
            }
        }

        self.text_search_persisted(query, limit, collection)
    }

    pub fn text_search_read_only(
        &self,
        query: &str,
        limit: usize,
        collection: Option<&str>,
    ) -> Result<Vec<SearchHit>, SearchError> {
        if collection == Some("code") {
            if let Some(hits) = self.text_search_with_workspace_overlay(query, limit, false)? {
                return Ok(hits);
            }
        }
        self.text_search_persisted(query, limit, collection)
    }

    fn text_search_persisted(
        &self,
        query: &str,
        limit: usize,
        collection: Option<&str>,
    ) -> Result<Vec<SearchHit>, SearchError> {
        let results = self.store.text_search(query, limit, collection)?;

        let ids: Vec<i64> = results.iter().map(|result| result.chunk_id).collect();
        let infos = self.store.chunks_by_ids(&ids)?;

        let mut hits = Vec::with_capacity(results.len());
        for result in results {
            if let Some(info) = infos.get(&result.chunk_id).cloned() {
                // FTS5 bm25 `rank` is negative and *smaller is better*. Map it to a [0,1) score
                // that *increases* with relevance so any later descending re-sort (the overlay
                // merge in `text_search_with_workspace_overlay`) keeps the strongest match first
                // rather than inverting it.
                let score = 1.0 - 1.0 / (1.0 - result.rank as f32);
                hits.push(SearchHit {
                    collection: info.collection,
                    root_id: info.root_id,
                    file_path: info.file_path,
                    symbol_name: info.symbol_name,
                    kind: info.kind,
                    text: info.text,
                    line_start: info.line_start,
                    line_end: info.line_end,
                    score,
                    source_span: info.source_span,
                });
            }
        }

        Ok(hits)
    }

    fn text_search_with_workspace_overlay(
        &self,
        query: &str,
        limit: usize,
        refresh: bool,
    ) -> Result<Option<Vec<SearchHit>>, SearchError> {
        if self.workspace_roots.is_none() {
            return Ok(None);
        }

        let overlay = if refresh {
            self.refresh_workspace_overlay_snapshot(false)?
        } else {
            self.workspace_overlay_snapshot()?
        };
        if overlay.is_empty() {
            return Ok(None);
        }

        let mut combined = self.text_search_persisted(query, limit * 3, Some("code"))?;
        combined.retain(|hit| {
            !overlay.hidden_paths.contains(&FileKey::new(&hit.root_id, &hit.file_path))
        });
        combined.extend(lexical_hits(&overlay, query, limit));
        combined.sort_by(|lhs, rhs| rhs.score.total_cmp(&lhs.score));
        combined.truncate(limit);
        Ok(Some(combined))
    }

    /// Clone the currently published workspace overlay without refreshing fingerprints,
    /// manifests, dirty paths, or the Store. Interactive queries use this after the host's
    /// fenced prefetch/apply attempt, so a refused apply serves the last resident publication.
    pub fn workspace_overlay_snapshot(&self) -> Result<WorkspaceOverlayIndex, SearchError> {
        let cache = self
            .workspace_overlay_cache
            .lock()
            .map_err(|e| SearchError::Index(format!("workspace overlay cache lock error: {e}")))?;
        Ok(cache.snapshot())
    }

    /// Refresh and snapshot the workspace overlay.
    ///
    /// `embed_missing` selects whether chunks without a cached vector may be embedded inline.
    fn refresh_workspace_overlay_snapshot(
        &self,
        embed_missing: bool,
    ) -> Result<WorkspaceOverlayIndex, SearchError> {
        let roots = self
            .workspace_roots
            .as_ref()
            .ok_or_else(|| SearchError::Index("workspace root is not configured".to_owned()))?;
        let embedder = if embed_missing { self.embedder.as_ref() } else { None };
        // Only the embedding refresh may pay for a cold full-tree scan under the lock.
        let allow_cold_scan = embed_missing;
        let mut cache = self
            .workspace_overlay_cache
            .lock()
            .map_err(|e| SearchError::Index(format!("workspace overlay cache lock error: {e}")))?;
        if let Some(manifest_fingerprints) = self.dispatched_manifest_fingerprints()? {
            cache.refresh_with_manifest(
                &manifest_fingerprints,
                roots,
                embedder,
                self.batch_size,
                &self.store,
                allow_cold_scan,
            )?;
        } else {
            cache.refresh(
                &self.store,
                roots,
                embedder,
                self.batch_size,
                self.workspace_baseline_hash_mode,
                allow_cold_scan,
            )?;
        }
        Ok(cache.snapshot())
    }

    pub fn sync_indexed_documents_in_collection(
        &mut self,
        collection: &str,
        documents: &[crate::IndexedDocument],
        progress: Option<&Arc<IndexProgress>>,
    ) -> Result<usize, SearchError> {
        self.sync_indexed_documents_in_collection_with_embeddings(
            collection, documents, None, progress,
        )
    }

    pub fn sync_indexed_documents_in_collection_with_embeddings(
        &mut self,
        collection: &str,
        documents: &[crate::IndexedDocument],
        shared_embeddings: Option<&HashMap<String, Vec<f32>>>,
        progress: Option<&Arc<IndexProgress>>,
    ) -> Result<usize, SearchError> {
        let mut pass = progress.map(IndexProgress::begin_pass);
        let token = pass.as_ref().map(ActivePass::token);
        let progress = token.as_ref();
        let lifecycle_batch = Batch::new(self.store.db_path(), Reason::ExplicitRebuild);
        let lifecycle_result = lifecycle_batch.context().in_scope(|| {
            use std::collections::{BTreeMap, HashSet};

            self.invalidate_reference_stamp(collection)?;

            let mut grouped = BTreeMap::<FileKey, Vec<crate::IndexedDocument>>::new();
            for document in documents {
                grouped
                    .entry(FileKey::new(&document.root_id, &document.path))
                    .or_default()
                    .push(document.clone());
            }

            let desired: HashSet<&FileKey> = grouped.keys().collect();
            for (existing, _) in self.store.all_files_in_collection(collection)? {
                if !desired.contains(&existing) {
                    self.store.remove_file_with_token_layout(
                        &existing.root_id,
                        &existing.path,
                        collection,
                        self.embedder.as_ref().and_then(Embedder::token_layout_claim),
                    )?;
                }
            }

            let total_chunks = documents.len();

            if let Some(p) = progress {
                p.set_totals(
                    grouped.len(),
                    total_chunks,
                    grouped
                        .values()
                        .map(|documents| documents.len().div_ceil(self.batch_size.max(1)))
                        .sum(),
                );
            }

            let mut indexed = 0usize;
            for (key, mut file_documents) in grouped {
                file_documents.sort_by(|lhs, rhs| {
                    (lhs.line_start, lhs.line_end, lhs.symbol_name.as_str()).cmp(&(
                        rhs.line_start,
                        rhs.line_end,
                        rhs.symbol_name.as_str(),
                    ))
                });

                let file_hash = normalized_file_hash_for_indexed_documents(&file_documents);
                let (stored_hash, reason) = self.observed_file_hash(&key, &file_hash)?;
                if stored_hash.as_deref() == Some(file_hash.as_slice()) {
                    continue;
                }

                let embeddings = if let Some(embedder) = &self.embedder {
                    let mut vectors = vec![Vec::<f32>::new(); file_documents.len()];
                    let mut missing_indices = Vec::new();
                    let mut missing_texts = Vec::new();

                    for (idx, document) in file_documents.iter().enumerate() {
                        let embedding_key = semantic_key_for_indexed_document(document);
                        if let Some(shared_embedding) =
                            shared_embeddings.and_then(|items| items.get(&embedding_key))
                        {
                            if shared_embedding.len() != self.dim {
                                if let Some(token) = progress {
                                    token.finish(IndexPassState::Failed);
                                }
                            }
                            vectors[idx] = shared_embedding.clone();
                            if let Some(p) = progress {
                                p.advance(1, 0);
                            }
                        } else {
                            missing_indices.push(idx);
                            missing_texts.push(semantic_text_for_indexed_document(document));
                        }
                    }

                    let refs = missing_texts.iter().map(String::as_str).collect::<Vec<_>>();
                    let ranges = embedder.batch_ranges(&refs, self.batch_size)?;
                    // The totals above count requests by document count alone; a byte-bounded
                    // split can need more, and only the overflow is new work.
                    let estimated = file_documents.len().div_ceil(self.batch_size.max(1));
                    if let Some(p) = progress {
                        p.add_batches(ranges.len().saturating_sub(estimated));
                    }
                    for range in ranges {
                        let batch_vectors = embedder.embed_batch(&refs[range.clone()])?;
                        Self::observe_embedding_dimensions(&batch_vectors, self.dim, progress);
                        if let Some(p) = progress {
                            p.advance(range.len(), 1);
                        }

                        for (offset, embedding) in batch_vectors.into_iter().enumerate() {
                            let idx = missing_indices[range.start + offset];
                            vectors[idx] = embedding;
                        }
                    }

                    Some(vectors)
                } else {
                    None
                };

                lifecycle::with_reason(reason, || {
                    self.store.reindex_indexed_documents_in_collection(
                        &key.root_id,
                        &key.path,
                        &file_hash,
                        collection,
                        &file_documents,
                        embeddings.as_deref(),
                    )
                })?;
                indexed += 1;
            }

            if let Some(p) = progress {
                p.phase(IndexPhase::Persisting);
            }

            // Метка снимается и ПОСЛЕ записи, не только до неё: запись идёт файл за файлом,
            // отдельными транзакциями, и другой процесс успевает опубликовать локальный корпус
            // в середине — его метка описывала бы содержимое, которого здесь уже нет.
            self.invalidate_reference_stamp(collection)?;

            self.index = Self::build_persisted_index_with_progress(
                &self.store,
                self.dim,
                self.embedder.as_ref(),
                progress,
            )?;
            self.observe_live_index(
                "live_index_replaced",
                Reason::ExplicitRebuild,
                Outcome::Completed,
            );
            Ok(indexed)
        });
        let lifecycle_outcome =
            if lifecycle_result.is_ok() { Outcome::Completed } else { Outcome::Failed };
        lifecycle_batch.finish(lifecycle_outcome);
        if let Some(pass) = pass.as_mut() {
            pass.finish(if lifecycle_result.is_ok() {
                IndexPassState::Ready
            } else {
                IndexPassState::Failed
            });
        }
        lifecycle_result
    }

    pub fn chunk_count(&self) -> Result<usize, SearchError> {
        self.store.chunk_count()
    }

    pub fn file_count(&self) -> Result<usize, SearchError> {
        self.store.file_count()
    }

    /// Startup identity proof only: a SQLite BLOB rebuild does not prove model provenance.
    pub fn semantic_index_qualification(&self) -> SemanticIndexQualification {
        self.semantic_qualification.get()
    }

    /// Capture the existing boot count results. Failed reads cannot prove an empty scope.
    pub fn observe_semantic_boot_coverage(&self, chunks: Option<usize>, embeddings: Option<usize>) {
        let mut evidence = self.semantic_qualification.get();
        evidence.coverage_complete = matches!((chunks, embeddings), (Some(chunks), Some(embeddings)) if chunks == embeddings && self.index.len() >= chunks);
        evidence.pending_work =
            matches!((chunks, embeddings), (Some(chunks), Some(embeddings)) if chunks > embeddings);
        self.semantic_qualification.set(evidence);
    }

    fn invalidate_semantic_coverage(&self) {
        let mut evidence = self.semantic_qualification.get();
        if let Some(epoch) = evidence.epoch.checked_add(1) {
            evidence.epoch = epoch;
        } else {
            evidence.identity_verified = false;
        }
        evidence.coverage_complete = false;
        evidence.pending_work = true;
        self.semantic_qualification.set(evidence);
    }

    /// Called after a fully successful native pass and live installation, for its captured epoch.
    pub fn observe_semantic_publication(&self, epoch: u64) {
        let mut evidence = self.semantic_qualification.get();
        if evidence.epoch != epoch {
            return;
        }
        if !evidence.identity_verified {
            let Some(embedder) = self.embedder.as_ref() else { return };
            let Some(claim) = embedder.token_layout_claim() else { return };
            let qualifies = self
                .store
                .embedding_profile_matches(embedder.storage_identity(), self.dim)
                .unwrap_or(false)
                && self.store.token_layout_claim_matches(claim).unwrap_or(false)
                && self
                    .store
                    .chunk_count()
                    .ok()
                    .zip(self.store.embedding_count_by_collection("code").ok())
                    .is_some_and(|(chunks, embeddings)| {
                        chunks == embeddings && chunks == self.index.len()
                    });
            if !qualifies {
                return;
            }
            evidence.identity_verified = true;
        }
        if evidence.identity_verified {
            evidence.coverage_complete = true;
            evidence.pending_work = false;
            self.semantic_qualification.set(evidence);
        }
    }

    pub fn accepted_native_sidecar(&self) -> bool {
        self.accepted_native_sidecar
    }

    pub fn vector_count(&self) -> usize {
        self.index.len()
    }

    pub fn embedding_count_by_collection(&self, collection: &str) -> Result<usize, SearchError> {
        self.store.embedding_count_by_collection(collection)
    }

    pub fn load_indexed_documents(
        &self,
        collection: Option<&str>,
    ) -> Result<Vec<crate::IndexedDocument>, SearchError> {
        self.store.load_indexed_documents(collection)
    }

    pub fn clear_file_hashes(&self, collection: &str) -> Result<usize, SearchError> {
        self.store.clear_file_hashes(collection)
    }

    pub fn clear_file_hashes_without_embeddings(
        &self,
        collection: &str,
    ) -> Result<usize, SearchError> {
        self.store.clear_file_hashes_without_embeddings(collection)
    }

    /// [`Self::remove_file`] when the file is indexed; `false` (and no index
    /// rebuild) when there is nothing to remove.
    pub fn remove_file_if_present(
        &mut self,
        rel_path: &str,
        collection: &str,
    ) -> Result<bool, SearchError> {
        if self.store.file_hash(CONFIGURATION_ROOT_ID, rel_path)?.is_none() {
            return Ok(false);
        }
        self.remove_file(rel_path, collection)?;
        Ok(true)
    }

    pub fn remove_file(&mut self, rel_path: &str, collection: &str) -> Result<(), SearchError> {
        self.store.remove_file_with_token_layout(
            CONFIGURATION_ROOT_ID,
            rel_path,
            collection,
            self.embedder.as_ref().and_then(Embedder::token_layout_claim),
        )?;
        self.invalidate_reference_stamp(collection)?;
        self.index = Self::build_persisted_index(&self.store, self.dim, self.embedder.as_ref())?;
        self.observe_live_index("live_index_replaced", Reason::ExplicitRebuild, Outcome::Completed);
        Ok(())
    }
}

/// Whether the indexed file at this path is PROVEN to be gone, as opposed to merely unreadable.
///
/// Following links, because a link whose target is deleted is deleted as far as anything
/// reading the file is concerned. `NotADirectory` counts too: a path whose parent is a file
/// cannot exist. So does anything that is not a regular file — a directory now standing at
/// the path is not the file the walk indexed, and the walk would never index it. Everything
/// else — a permission error, a momentary race — is an unanswered question, and an
/// unanswered question is not evidence of deletion.
fn proven_absent(path: &Path) -> bool {
    match std::fs::metadata(path) {
        Ok(metadata) => !metadata.is_file(),
        Err(err) => {
            matches!(err.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory)
        }
    }
}

/// The spellings a removal of one of `paths` can arrive under: the path as given on disk,
/// and its canonical counterpart while one still resolves. Both are recorded at index time
/// (the walk has both sides), so both are tried when a removal asks who the spelling was.
fn removal_spellings(roots: &WorkspaceRoots, paths: &[PathBuf]) -> Vec<String> {
    let mut spellings = Vec::new();
    let mut seen = HashSet::new();
    for path in paths {
        let (walked, canonical) = roots.spellings_of(path);
        for spelling in [walked, canonical] {
            let rendered = spelling.to_string_lossy().into_owned();
            if !rendered.is_empty() && seen.insert(rendered.clone()) {
                spellings.push(rendered);
            }
        }
    }
    spellings
}

/// The keys a drain's removed PATHS name, with the journal answering for a spelling it holds
/// and attribution for a spelling it does not.
///
/// A path the journal holds is answered by the journal alone: the spelling names the key its
/// file was indexed under, and re-attribution now would name a different one — which is the
/// defect the pair exists to close (github#192). The journal answer is gated by the absence
/// proof, so removing a link whose target lives takes nothing with it. A spelling the journal
/// does not hold keeps the event-driven attribution it had before the journal existed: a store
/// whose journal has not been populated yet behaves exactly as it used to.
pub fn removed_path_keys(
    roots: &WorkspaceRoots,
    reader: &Store,
    removed_paths: &[PathBuf],
) -> Result<Vec<FileKey>, SearchError> {
    let mut keys = Vec::new();
    let mut seen: HashSet<FileKey> = HashSet::new();
    for path in removed_paths {
        let mut recorded = None;
        for spelling in removal_spellings(roots, std::slice::from_ref(path)) {
            if let Some(key) = reader.path_spelling_key(&spelling, "code")? {
                recorded = Some(key);
                break;
            }
        }
        match recorded {
            Some(key) => {
                if proven_absent_key(roots, &key) && seen.insert(key.clone()) {
                    keys.push(key);
                }
            }
            None => {
                if let Some(key) = workspace_file_key_in(roots, path) {
                    if seen.insert(key.clone()) {
                        keys.push(key);
                    }
                }
            }
        }
    }
    Ok(keys)
}

/// Whether the file `key` names is proven gone, through the spelling a reader opens it by.
fn proven_absent_key(roots: &WorkspaceRoots, key: &FileKey) -> bool {
    roots.resolve(key).is_some_and(|path| proven_absent(&path))
}

/// The keys under `dirs` whose files are proven gone, attributed by the DECLARED spellings
/// alone and chosen from every carrier (see [`SearchEngine::vanished_workspace_keys`]).
fn vanished_keys_in(
    roots: &WorkspaceRoots,
    carriers: &crate::key_carriers::CarrierKeys,
    dirs: &[PathBuf],
) -> Vec<FileKey> {
    // Attributed by the DECLARED spellings alone: the directory is gone, so there is
    // nothing left on disk to canonicalise, and its keys were spelled the way the walk
    // reached it.
    //
    // Two kinds of key go with a directory, and attribution alone finds only the first.
    // It answers "which root owns this file", and a root owns no file at its own path,
    // so a directory that IS a root — an extension deleted whole, a configuration that
    // is the workspace — attributes to nothing at all. Its keys are its root's.
    let mut prefixes: Vec<FileKey> = Vec::new();
    let mut swallowed_roots: HashSet<String> = HashSet::new();
    for dir in dirs {
        let walked = if dir.is_absolute() {
            dir.clone()
        } else {
            roots.configuration().unwrap_or_else(|| roots.workspace()).join(dir)
        };
        // Both spellings, for the same reason point removal canonicalises: a root
        // declared through a link owns the files physically under its target, and the
        // declared path alone would hand the subtree to the enclosing alias root —
        // whose keys are not the ones in the store.
        let canonical = crate::workspace_roots::canonical_spelling(&walked);
        prefixes.extend(roots.root_of_declared(&walked));
        prefixes.extend(roots.root_of(&walked, &canonical));
        swallowed_roots.extend(
            roots
                .entries()
                .filter(|(_, declared)| {
                    declared.starts_with(&walked) || declared.starts_with(&canonical)
                })
                .map(|(id, _)| id.to_owned()),
        );
        // A root declared THROUGH a link keeps its declared spelling, while its files —
        // and the roots registered under its target — are spelled from the target, which
        // the table captured when it was built. When the target is what got deleted, the
        // removed path is the root's declared spelling (or a path inside it), and the
        // nested roots match neither spelling of it: their keys would be left behind,
        // spelled as the walk saw them through the link that is now gone (github#190).
        // The nested roots are read from the table for exactly that reason — the disk can
        // no longer say where the link led. Candidates only: every key is still proven
        // gone file by file below.
        for (_, declared, target) in roots.entries_with_canonical() {
            if declared != walked && !crate::workspace_roots::starts_at(&walked, declared) {
                continue;
            }
            swallowed_roots.extend(
                roots
                    .entries_with_canonical()
                    .filter(|(_, nested_declared, nested_canonical)| {
                        crate::workspace_roots::starts_at(nested_declared, target)
                            || crate::workspace_roots::starts_at(nested_canonical, target)
                    })
                    .map(|(nested_id, _, _)| nested_id.to_owned()),
            );
        }
    }
    if prefixes.is_empty() && swallowed_roots.is_empty() {
        return Vec::new();
    }
    carriers
        .all_keys()
        .into_iter()
        .filter(|key| {
            swallowed_roots.contains(&key.root_id)
                || prefixes.iter().any(|prefix| key.is_under(prefix))
        })
        .filter(|key| roots.resolve(key).is_some_and(|path| proven_absent(&path)))
        .collect()
}

/// One file a walk reached, with both spellings attribution had: the walked path is what a
/// caller reads and what a removal arrives as; the canonical one is its counterpart under
/// the target (github#192).
#[derive(Debug, Clone)]
pub(crate) struct ScannedWorkspaceFile {
    pub(crate) key: FileKey,
    pub(crate) walked: PathBuf,
    pub(crate) canonical: PathBuf,
}

/// What a batch removal did: the count each caller reports, and the first fault, kept so a
/// batch can finish every independent key and still fail as a whole.
#[derive(Default)]
struct RemovedBatch {
    removed: usize,
    failed: usize,
    first_error: Option<SearchError>,
}

/// What one FTS ingest did, and what it could not do.
///
/// `unread` counts files the WALK reached and classified as source but whose bytes could not be
/// read — a permission change, a file removed between the walk and the read, bytes that are not
/// UTF-8. The walk's own counters cannot see this: `stat` needs no read permission, so such a
/// file is enumerated and counted as healthy. A caller that must not stand behind an incomplete
/// corpus has to ask separately, which is why the count travels instead of only being logged.
pub struct FtsIngest {
    pub indexed: usize,
    pub unread: usize,
}

enum PreparedBootFile {
    Unchanged,
    Unread,
    Remove(FileKey, Reason),
    Reindex {
        key: FileKey,
        reason: Reason,
        hash: Vec<u8>,
        chunks: Vec<code_chunk::Chunk>,
        graph_contexts: Option<Vec<Option<String>>>,
        /// Some context failed to render; the file owes a re-render once written.
        context_owed: bool,
        source_spans: Vec<Option<crate::SourceSpan>>,
    },
}

struct FileTask {
    key: FileKey,
    reason: Reason,
    hash: Vec<u8>,
    chunks: Vec<code_chunk::Chunk>,
    texts: Vec<String>,
    ranges: Vec<std::ops::Range<usize>>,
    /// Per-chunk graph context (parallel to `chunks`), persisted so a later
    /// reconstruction-from-storage re-embeds with the same enriched text.
    graph_contexts: Vec<Option<String>>,
    /// Some context failed to render; the file owes a re-render once written.
    context_owed: bool,
    source_spans: Vec<Option<crate::SourceSpan>>,
}

struct FileResult {
    key: FileKey,
    reason: Reason,
    hash: Vec<u8>,
    chunks: Vec<code_chunk::Chunk>,
    graph_contexts: Vec<Option<String>>,
    context_owed: bool,
    source_spans: Vec<Option<crate::SourceSpan>>,
    embeddings: Result<Vec<Vec<f32>>, SearchError>,
}

#[cfg(test)]
mod walk_ownership {
    /// The engine must not walk the tree itself. The walk policy — which links are followed,
    /// how an error is classified, which spelling a file is keyed by — lives in `project-model`
    /// and `WorkspaceRoots`, and a private traversal here diverges from it silently: the corpus
    /// it produces answers to a completeness verdict some other traversal computed.
    ///
    /// The ban names the traversal APIs rather than the word, so a mention in prose cannot fail
    /// the gate, and it covers `read_dir` as well as the walk crate: a hand-rolled recursion is
    /// the same divergence wearing different letters.
    #[test]
    fn the_engine_does_not_carry_its_own_tree_walk() {
        let source = include_str!("engine.rs");
        // A CRLF checkout (core.autocrlf on Windows, no .gitattributes pinning LF) gives this
        // file "\r\n" endings, and a needle anchored on "\n" would then match nothing — the
        // gate would fail on the line endings rather than on the code, in the very CI step that
        // runs it by name. Normalised first, so the gate is about the source and not the
        // checkout.
        let source = &source.replace("\r\n", "\n");
        // Test code walks legitimately (a stand has to build and probe trees), so the ban
        // covers production only. The cut is asserted below: a marker that stopped matching
        // would shrink the scanned region to nothing and quietly pass everything.
        let production = source.split("\n#[cfg(test)]\nmod tests {").next().unwrap_or(source);
        assert!(
            production.contains("fn ingest_scanned_fts"),
            "the production/test cut moved; this gate scans only what it can prove it scanned"
        );
        for needle in [["walk", "dir::Walk", "Dir"].concat(), ["read", "_dir("].concat()] {
            assert!(
                !production.contains(&needle),
                "engine.rs must reach the filesystem through project_model::SourceSet::scan, \
                 not through its own traversal ({needle})"
            );
        }
    }
}

#[cfg(test)]
mod retry_signal_ownership {
    use super::OverlayRetrySignals;

    /// Dirty marks belong to the point path and its own owner. A mark alone is no work for the
    /// embedding driver, whose pass is a whole plan: counting marks as its work would run one
    /// per edit and race the point path for the same keys.
    #[test]
    fn marks_alone_demand_no_embedding_pass() {
        let clean = OverlayRetrySignals {
            initialized: true,
            needs_full_rescan: false,
            pending_dirty_paths: 0,
            unembedded_entries: 0,
            unread_keys: 0,
        };
        let marked = OverlayRetrySignals { pending_dirty_paths: 3, ..clean };
        assert!(!marked.demands_a_pass(), "a dirty mark started a whole embedding pass");
        assert!(OverlayRetrySignals { unembedded_entries: 1, ..clean }.demands_a_pass());
        assert!(OverlayRetrySignals { initialized: false, ..clean }.demands_a_pass());
    }
}

#[cfg(test)]
mod lifecycle_tests;

#[cfg(test)]
mod tests {

    /// A pause is reversible, and that is all it is. Held past the end of the pass it paused —
    /// an ordering nothing reaches today and nothing forbade either — a guard that raised the
    /// flag unconditionally left it raised for good, pinning the resident process the flag
    /// keeps alive.
    #[test]
    fn a_pause_outliving_its_pass_leaves_the_flag_down() {
        let progress = std::sync::Arc::new(crate::IndexProgress::default());
        let paused = {
            let _active = progress.begin_pass();
            assert!(progress.is_active(), "the pass is working");
            let paused = progress.pause_pass();
            assert!(!progress.is_active(), "a pause is not work");
            paused
        };
        assert!(
            !progress.is_active(),
            "the pass ended, so the flag is down however the guards were dropped",
        );
        drop(paused);
        assert!(
            !progress.is_active(),
            "a pause that outlived its pass raised the flag nothing will ever lower",
        );
    }
    use super::{
        FenceOutcome, IndexProgress, SearchEngine, CONSTRUCTOR_APPLY_ACTIVE,
        FORCE_VECTOR_REMOVE_ERROR, WORKSPACE_APPLY_BATCH_ROWS,
    };
    use crate::key_carriers::KeyCarrier;
    use crate::ports::{SnapshotCatalog, SnapshotContentStore};
    use crate::workspace_roots::{FileKey, CONFIGURATION_ROOT_ID};
    use crate::{BaselineRef, CorpusId, Document, IndexedDocument, SearchError, Snapshot};
    use crate::{
        Chunk, ChunkKind, Embedder, EmbedderConfig, EmbeddingExecutionPolicy, SearchConfig,
        SourceSpan, Store, TokenPolicy,
    };
    use std::collections::HashMap;
    use std::collections::HashSet;
    use std::fs;
    use std::ops::ControlFlow;
    use std::path::{Path, PathBuf};

    #[test]
    fn token_layout_corruption_falls_back_to_fts_without_mutating_rows() {
        let Ok(tokenizer_path) = std::env::var("USER2_TOKENIZER_JSON") else { return };
        const TOKENIZER_SHA256: &str =
            "80d0433a2cfc55a4561b0e98b6f822decc48c9d457db498837223f9385ef3aff";
        let token_policy =
            TokenPolicy::load(Path::new(&tokenizer_path), TOKENIZER_SHA256, 8192).unwrap();
        let config = || SearchConfig {
            embedder: EmbedderConfig {
                dim: Some(2),
                token_policy: Some(token_policy.clone()),
                ..EmbedderConfig::default()
            },
            execution: EmbeddingExecutionPolicy::default(),
        };
        let embedder = Embedder::new(config().embedder);
        let claim = embedder.token_layout_claim().unwrap().to_owned();
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("claimed-corrupt.db");
        let mut store = Store::open(&db_path).unwrap();
        let text = "Procedure Original()\n// DistinctiveLexicalNeedle\nEndProcedure";
        let chunk = Chunk {
            kind: ChunkKind::Procedure,
            name: "Original".to_owned(),
            is_export: false,
            annotations: Vec::new(),
            line_start: 1,
            line_end: 3,
            text: text.to_owned(),
        };
        store
            .reindex_file_with_identity_and_source_spans(
                "root",
                "Module.bsl",
                b"stable-hash",
                &[chunk],
                Some(&[vec![1.0, 0.0]]),
                None,
                embedder.model_id(),
                2,
                Some(&claim),
                &[Some(SourceSpan {
                    parent_symbol: "Original".to_owned(),
                    byte_start: 0,
                    byte_end: text.len() as u32,
                    parent_byte_start: 0,
                    parent_byte_end: text.len() as u32,
                    part_index: 1,
                    part_count: 1,
                })],
            )
            .unwrap();
        drop(store);
        let connection = rusqlite::Connection::open(&db_path).unwrap();
        connection.execute("UPDATE chunk_source_spans SET part_index = 0", []).unwrap();
        drop(connection);

        let before = Store::open_existing(&db_path).unwrap();
        let before_counts = (before.file_count().unwrap(), before.chunk_count().unwrap());
        drop(before);
        let outcome =
            SearchEngine::new_fenced(&db_path, config(), SearchEngine::permit_checkpointed_apply)
                .unwrap();
        let FenceOutcome::Applied(engine) = outcome else {
            panic!("permit-all startup should return the FTS fallback")
        };
        assert!(!engine.has_semantic());
        assert_eq!(engine.index.len(), 0, "fallback must not read vectors into HNSW");
        assert_eq!(
            engine.text_search("DistinctiveLexicalNeedle", 10, Some("code")).unwrap().len(),
            1
        );
        assert_eq!(
            (engine.store().file_count().unwrap(), engine.store().chunk_count().unwrap()),
            before_counts,
            "claim validation must keep pre-existing rows intact"
        );

        let overlay = SearchEngine::semantic_overlay_only_fenced(
            &db_path,
            config(),
            SearchEngine::permit_checkpointed_apply,
        )
        .unwrap();
        let FenceOutcome::Applied(engine) = overlay else {
            panic!("permit-all startup should return the FTS fallback")
        };
        assert!(!engine.has_semantic());
        assert_eq!(engine.index.len(), 0, "fallback must not read vectors into HNSW");
        assert_eq!(
            engine.text_search("DistinctiveLexicalNeedle", 10, Some("code")).unwrap().len(),
            1
        );
        assert_eq!(
            (engine.store().file_count().unwrap(), engine.store().chunk_count().unwrap()),
            before_counts
        );
    }

    /// Opt-in proof over a caller-prepared, already-indexed corpus. The test is inert unless all
    /// required paths are explicitly provided; it opens the Store read-only and never rebuilds it.
    #[test]
    fn optional_token_layout_corpus_store_source_coverage() {
        let (Ok(workspace), Ok(configuration), Ok(store_path)) = (
            std::env::var("BSL_TOKEN_LAYOUT_COVERAGE_WORKSPACE"),
            std::env::var("BSL_TOKEN_LAYOUT_COVERAGE_CONFIGURATION"),
            std::env::var("BSL_TOKEN_LAYOUT_COVERAGE_STORE"),
        ) else {
            return;
        };
        let extensions = std::env::var_os("BSL_TOKEN_LAYOUT_COVERAGE_EXTENSIONS")
            .as_deref()
            .map(std::env::split_paths)
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        let workspace = PathBuf::from(workspace);
        let configuration = PathBuf::from(configuration);
        let (roots, rejected) =
            crate::WorkspaceRoots::build(&workspace, &configuration, &extensions);
        assert!(rejected.is_empty(), "coverage roots must match production root ownership");
        let declared = roots.entries().map(|(_, path)| path.to_path_buf()).collect::<Vec<_>>();
        let scan = project_model::SourceSet::scan_excluding(&declared, roots.excluded());
        assert!(scan.clean(), "source coverage requires a complete source walk");
        let files = SearchEngine::files_from_scan(&roots, &scan);
        let store = Store::open_reader(Path::new(&store_path)).unwrap();
        let documents = store.load_indexed_documents(Some("code")).unwrap();
        let mut documents_by_file = HashMap::<FileKey, Vec<IndexedDocument>>::new();
        for document in documents {
            documents_by_file
                .entry(FileKey::new(&document.root_id, &document.path))
                .or_default()
                .push(document);
        }

        let mut expected = HashSet::new();
        for file in &files {
            let key = &file.key;
            let source = fs::read_to_string(&file.walked).unwrap();
            for parent in crate::Chunker::source_chunks(&source) {
                let identity = (
                    key.clone(),
                    parent.chunk.name,
                    parent.source_byte_start,
                    parent.source_byte_end,
                );
                assert!(expected.insert(identity), "AST parent identity must be unique");
            }
        }

        let mut actual = HashSet::new();
        for file in &files {
            let key = &file.key;
            let source = fs::read_to_string(&file.walked).unwrap();
            let mut groups = HashMap::<(String, u32, u32), Vec<IndexedDocument>>::new();
            for document in documents_by_file.remove(key).unwrap_or_default() {
                let span = document.source_span.as_ref().expect("token rows need provenance");
                groups
                    .entry((
                        span.parent_symbol.clone(),
                        span.parent_byte_start,
                        span.parent_byte_end,
                    ))
                    .or_default()
                    .push(document);
            }

            for ((parent_symbol, parent_start, parent_end), mut parts) in groups {
                let identity = (key.clone(), parent_symbol.clone(), parent_start, parent_end);
                assert!(actual.insert(identity), "a parent may be persisted only once");
                parts.sort_by_key(|document| document.source_span.as_ref().unwrap().byte_start);
                let part_count = parts[0].source_span.as_ref().unwrap().part_count;
                assert_eq!(parts.len(), part_count as usize);
                assert_eq!(parts[0].source_span.as_ref().unwrap().byte_start, parent_start);
                assert_eq!(
                    parts.last().unwrap().source_span.as_ref().unwrap().byte_end,
                    parent_end
                );
                for (index, document) in parts.iter().enumerate() {
                    let span = document.source_span.as_ref().unwrap();
                    assert_eq!(span.parent_symbol, parent_symbol);
                    assert_eq!(
                        (span.parent_byte_start, span.parent_byte_end),
                        (parent_start, parent_end)
                    );
                    assert_eq!(span.part_index, index as u32 + 1);
                    assert_eq!(span.part_count, part_count);
                    assert_eq!(
                        document.text,
                        source[span.byte_start as usize..span.byte_end as usize],
                        "persisted source text must be the exact original byte slice"
                    );
                    if let Some(next) = parts.get(index + 1) {
                        assert_eq!(span.byte_end, next.source_span.as_ref().unwrap().byte_start);
                    }
                }
            }
        }
        assert_eq!(actual, expected, "every AST parent, including module headers, is covered once");
        assert!(
            documents_by_file.is_empty(),
            "store contains source files outside the configured roots"
        );
    }
    use std::sync::Arc;

    fn baseline_manifest(snapshot: &str, rows: usize) -> crate::WorkspaceBaselineManifest {
        crate::WorkspaceBaselineManifest {
            snapshot_id: snapshot.to_owned(),
            snapshot_fingerprint: Some(format!("{snapshot}-fingerprint")),
            files: (0..rows)
                .map(|index| crate::BaselineManifestFile {
                    collection: "code".to_owned(),
                    root_id: CONFIGURATION_ROOT_ID.to_owned(),
                    path: format!("File{index}.bsl"),
                    file_fingerprint: format!("fingerprint-{index}"),
                    document_count: 1,
                    file_object_id: format!("object-{index}"),
                })
                .collect(),
        }
    }

    #[test]
    fn manifest_and_fingerprint_transitions_checkpoint_and_rollback() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("search.db");
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        let original = baseline_manifest("original", 1);
        engine.store().save_baseline_manifest(&original).unwrap();

        let replacement = baseline_manifest("replacement", WORKSPACE_APPLY_BATCH_ROWS + 1);
        let mut checkpoints = 0;
        let interrupted = engine
            .store()
            .save_baseline_manifest_checkpointed(&replacement, &mut || {
                checkpoints += 1;
                if checkpoints == 2 {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                }
            })
            .unwrap();
        assert!(interrupted.is_break());
        assert_eq!(
            engine.store().load_baseline_manifest().unwrap().unwrap().snapshot_id,
            "original"
        );

        engine.store().save_baseline_manifest(&replacement).unwrap();
        checkpoints = 0;
        let interrupted = engine
            .store()
            .clear_baseline_manifest_checkpointed(&mut || {
                checkpoints += 1;
                if checkpoints == 2 {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                }
            })
            .unwrap();
        assert!(interrupted.is_break());
        assert_eq!(
            engine.store().load_coherent_baseline_manifest().unwrap().unwrap().snapshot_id,
            "replacement"
        );

        engine.set_serves_external_baseline(true).unwrap();
        let fingerprints = (0..=WORKSPACE_APPLY_BATCH_ROWS)
            .map(|index| {
                (
                    FileKey::configuration(format!("File{index}.bsl")),
                    crate::store::PersistedFingerprint {
                        file_size: index as u64,
                        file_mtime_secs: index as i64,
                        file_mtime_nanos: 0,
                        content_fingerprint: format!("content-{index}"),
                        canonical: format!("canonical-{index}"),
                    },
                )
            })
            .collect();
        engine.store().save_overlay_fingerprint_cache("replacement", &fingerprints).unwrap();
        checkpoints = 0;
        let interrupted = engine.set_serves_external_baseline_checkpointed(false, &mut || {
            checkpoints += 1;
            if checkpoints == 2 {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        });
        assert!(interrupted.is_break());
        assert!(engine.serves_external_baseline);
        assert_eq!(engine.store().overlay_fingerprint_keys().unwrap().len(), fingerprints.len());
    }

    /// Clean initialization only records which baseline the overlay matches; an unreadable
    /// manifest leaves that unknown instead of keeping the resident-fed reindex inert.
    #[test]
    fn a_clean_start_with_an_unreadable_manifest_still_initializes() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("search.db");
        let mut engine = SearchEngine::fts_only(&db).unwrap();
        engine.set_workspace_root(dir.path().to_path_buf());
        engine.set_serves_external_baseline(true).unwrap();
        rusqlite::Connection::open(&db)
            .unwrap()
            .execute_batch("ALTER TABLE baseline_manifest RENAME TO hidden_manifest;")
            .unwrap();
        engine.initialize_workspace_overlay_clean().unwrap();
        assert!(engine.try_workspace_overlay_retry_signals().unwrap().initialized);
        assert_eq!(engine.try_workspace_overlay_baseline_identity(), None);
    }

    /// A local store has no baseline copy beyond its own rows, which the batch deletes: a removed
    /// file is not hidden, and is not counted among deleted baseline files for ever.
    #[test]
    fn a_local_drift_removal_hides_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut engine = SearchEngine::fts_only(&dir.path().join("search.db")).unwrap();
        engine.set_workspace_root(dir.path().to_path_buf());
        engine.initialize_workspace_overlay_clean().unwrap();
        let removed = FileKey::configuration("Removed.bsl");
        let chunks = code_chunk::Chunker::chunk("Процедура Тест()\nКонецПроцедуры");
        engine.ingest_fused_file(&removed, b"hash", &chunks, &vec![None; chunks.len()]).unwrap();
        assert!(!engine.serves_external_baseline);
        let outcome = engine.apply_prepared_workspace_drift_batch(
            &[],
            std::slice::from_ref(&removed),
            &[],
            &mut || ControlFlow::Continue(()),
        );
        assert!(matches!(outcome, ControlFlow::Continue(Ok(_))));
        let stats = engine.workspace_overlay_stats_read_only().unwrap().unwrap();
        assert_eq!(stats.deleted_files, 0, "a local removal was counted as a hidden baseline file");
    }

    #[test]
    fn prepared_drift_batch_rolls_back_owner_rows() {
        let dir = tempfile::tempdir().unwrap();
        let mut engine = SearchEngine::fts_only(&dir.path().join("search.db")).unwrap();
        let removed = FileKey::configuration("Removed.bsl");
        let context = FileKey::configuration("Context.bsl");
        let chunks = code_chunk::Chunker::chunk("Процедура Тест()\nКонецПроцедуры");
        engine.ingest_fused_file(&removed, b"hash", &chunks, &vec![None; chunks.len()]).unwrap();
        let mut checkpoints = 0;
        let outcome = engine.apply_prepared_workspace_drift_batch(
            &[],
            std::slice::from_ref(&removed),
            std::slice::from_ref(&context),
            &mut || {
                checkpoints += 1;
                if checkpoints == 2 {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                }
            },
        );
        assert!(outcome.is_break());
        assert!(engine.store().file_hash(&removed.root_id, &removed.path).unwrap().is_some());
        assert!(!engine.context_dirty_paths("code").unwrap().contains(&context));
        assert!(engine.workspace_overlay_dirty_paths().unwrap().is_empty());
    }

    /// The seq a slice reports is the one its marks carry, so a consumer bounded by it clears
    /// exactly those marks; a slice without context rows reports none.
    #[test]
    fn a_drift_slice_reports_the_seq_of_the_marks_it_placed() {
        let dir = tempfile::tempdir().unwrap();
        let mut engine = SearchEngine::fts_only(&dir.path().join("search.db")).unwrap();
        let context = FileKey::configuration("Context.bsl");
        let mut checkpoint = || ControlFlow::Continue(());
        let none = engine.apply_prepared_workspace_drift_batch(&[], &[], &[], &mut checkpoint);
        assert!(matches!(none, ControlFlow::Continue(Ok(None))));
        let placed = match engine.apply_prepared_workspace_drift_batch(
            &[],
            &[],
            std::slice::from_ref(&context),
            &mut checkpoint,
        ) {
            ControlFlow::Continue(Ok(Some(seq))) => seq,
            other => panic!("context rows placed no seq: {other:?}"),
        };
        assert_eq!(placed, engine.mark_seq_handle().load(std::sync::atomic::Ordering::SeqCst));
        assert!(engine.context_dirty_paths("code").unwrap().contains(&context));
        struct NoContext;
        impl crate::ports::GraphContextProvider for NoContext {
            fn graph_context(&self, _: &str, _: &str, _: &str) -> Option<String> {
                None
            }
        }
        engine.refresh_dirty_contexts(&NoContext, placed - 1).unwrap();
        assert!(engine.context_dirty_paths("code").unwrap().contains(&context), "cleared early");
        engine.refresh_dirty_contexts(&NoContext, placed).unwrap();
        assert!(!engine.context_dirty_paths("code").unwrap().contains(&context));
    }
    use tempfile::tempdir;

    #[test]
    fn workspace_apply_n_plus_one_rows_use_two_fenced_transactions() {
        let connection = rusqlite::Connection::open_in_memory().unwrap();
        connection.execute("CREATE TABLE applied (value INTEGER NOT NULL)", []).unwrap();
        let rows: Vec<usize> = (0..=WORKSPACE_APPLY_BATCH_ROWS).collect();
        let mut admissions = 0;
        let mut apply = |operation: &mut dyn FnMut() -> Result<(), SearchError>| {
            admissions += 1;
            FenceOutcome::Applied(operation())
        };

        for batch in rows.chunks(WORKSPACE_APPLY_BATCH_ROWS) {
            let outcome = SearchEngine::fenced_value(&mut apply, || {
                let transaction = connection.unchecked_transaction()?;
                for value in batch {
                    transaction.execute(
                        "INSERT INTO applied (value) VALUES (?1)",
                        rusqlite::params![*value as i64],
                    )?;
                }
                transaction.commit()?;
                Ok(())
            })
            .unwrap();
            assert!(matches!(outcome, FenceOutcome::Applied(())));
        }

        assert_eq!(admissions, 2);
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM applied", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            (WORKSPACE_APPLY_BATCH_ROWS + 1) as i64
        );
    }

    #[test]
    #[allow(clippy::type_complexity)]
    fn checkpointed_atomic_transaction_rolls_back_at_batch_boundary() {
        let connection = rusqlite::Connection::open_in_memory().unwrap();
        connection.execute("CREATE TABLE applied (value INTEGER NOT NULL)", []).unwrap();
        let mut admissions = 0;
        let mut checkpoints = 0;
        let mut apply = |operation: &mut dyn FnMut(
            &mut dyn FnMut() -> ControlFlow<()>,
        )
            -> ControlFlow<(), Result<(), SearchError>>| {
            admissions += 1;
            let mut checkpoint = || {
                checkpoints += 1;
                ControlFlow::Break(())
            };
            match operation(&mut checkpoint) {
                ControlFlow::Break(()) => FenceOutcome::Released,
                ControlFlow::Continue(result) => FenceOutcome::Applied(result),
            }
        };

        let outcome = SearchEngine::fenced_checkpointed_value(&mut apply, |checkpoint| {
            let transaction = connection.unchecked_transaction().unwrap();
            for value in 0..=WORKSPACE_APPLY_BATCH_ROWS {
                transaction
                    .execute(
                        "INSERT INTO applied (value) VALUES (?1)",
                        rusqlite::params![value as i64],
                    )
                    .unwrap();
                if (value + 1) % WORKSPACE_APPLY_BATCH_ROWS == 0 && checkpoint().is_break() {
                    return ControlFlow::Break(());
                }
            }
            transaction.commit().unwrap();
            ControlFlow::Continue(Ok(()))
        })
        .unwrap();

        assert!(matches!(outcome, FenceOutcome::Released));
        assert_eq!(admissions, 1);
        assert_eq!(checkpoints, 1);
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM applied", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            0,
            "dropping the interrupted transaction rolls back every row"
        );
    }

    #[test]
    fn fused_file_rolls_back_hash_and_all_chunks_at_the_64_chunk_checkpoint() {
        let dir = tempdir().unwrap();
        let mut engine = SearchEngine::fts_only(&dir.path().join("search.db")).unwrap();
        let key = FileKey::configuration("Module.bsl");
        let old = crate::Chunk {
            kind: code_chunk::ChunkKind::Procedure,
            name: "Старая".to_owned(),
            is_export: true,
            annotations: Vec::new(),
            line_start: 1,
            line_end: 2,
            text: "Процедура Старая() Экспорт\nКонецПроцедуры".to_owned(),
        };
        engine.ingest_fused_file(&key, b"old", &[old], &[None]).unwrap();

        let chunks: Vec<_> = (0..=WORKSPACE_APPLY_BATCH_ROWS)
            .map(|index| crate::Chunk {
                kind: code_chunk::ChunkKind::Procedure,
                name: format!("Новая{index}"),
                is_export: true,
                annotations: Vec::new(),
                line_start: index as u32 * 2 + 1,
                line_end: index as u32 * 2 + 2,
                text: format!("Процедура Новая{index}() Экспорт\nКонецПроцедуры"),
            })
            .collect();
        let contexts = vec![None; chunks.len()];
        let mut checkpoints = 0;
        let cancelled =
            engine.ingest_fused_file_checkpointed(&key, b"new", &chunks, &contexts, &mut || {
                checkpoints += 1;
                if checkpoints == 2 {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                }
            });
        assert!(cancelled.is_break());
        assert_eq!(checkpoints, 2, "initial admission plus the 64-chunk heartbeat");
        assert_eq!(
            engine.store().file_hash(&key.root_id, &key.path).unwrap(),
            Some(b"old".to_vec())
        );
        assert_eq!(engine.chunk_count().unwrap(), 1);
        assert_eq!(engine.text_search("Старая", 10, Some("code")).unwrap().len(), 1);
        assert!(engine.text_search("Новая64", 10, Some("code")).unwrap().is_empty());

        let mut permit = || ControlFlow::Continue(());
        assert!(matches!(
            engine.ingest_fused_file_checkpointed(&key, b"new", &chunks, &contexts, &mut permit,),
            ControlFlow::Continue(Ok(()))
        ));
        assert_eq!(
            engine.store().file_hash(&key.root_id, &key.path).unwrap(),
            Some(b"new".to_vec())
        );
        assert_eq!(engine.chunk_count().unwrap(), WORKSPACE_APPLY_BATCH_ROWS + 1);
        assert!(engine.text_search("Старая", 10, Some("code")).unwrap().is_empty());
        assert_eq!(engine.text_search("Новая64", 10, Some("code")).unwrap().len(), 1);
    }

    #[derive(Default)]
    struct TestCatalog {
        snapshots: HashMap<String, Snapshot>,
    }

    impl SnapshotCatalog for TestCatalog {
        fn resolve_baseline(
            &self,
            baseline: &BaselineRef,
        ) -> Result<Option<Snapshot>, SearchError> {
            let id = baseline.snapshot_id.as_ref().map(|id| id.0.as_str()).unwrap_or_default();
            Ok(self.snapshots.get(id).cloned())
        }
    }

    #[derive(Default)]
    struct TestContentStore {
        documents: HashMap<String, Vec<IndexedDocument>>,
    }

    impl SnapshotContentStore for TestContentStore {
        fn load_snapshot_documents(
            &self,
            snapshot: &Snapshot,
        ) -> Result<Vec<IndexedDocument>, SearchError> {
            Ok(self.documents.get(&snapshot.id.0).cloned().unwrap_or_default())
        }
    }

    #[test]
    fn workspace_constructor_publish_fence() {
        use crate::{
            Chunk, ChunkKind, EmbedderConfig, EmbeddingExecutionPolicy, SearchConfig, Store,
        };
        use std::cell::Cell;

        fn config(dim: usize) -> SearchConfig {
            SearchConfig {
                embedder: EmbedderConfig { dim: Some(dim), ..EmbedderConfig::default() },
                execution: EmbeddingExecutionPolicy::default(),
            }
        }

        fn seed(db_path: &std::path::Path, dim: usize) {
            let mut store = Store::open(db_path).unwrap();
            let chunk = Chunk {
                kind: ChunkKind::Procedure,
                name: "Процедура".to_owned(),
                is_export: true,
                annotations: Vec::new(),
                line_start: 1,
                line_end: 2,
                text: "Процедура Процедура()\nКонецПроцедуры".to_owned(),
            };
            store
                .reindex_file(
                    crate::workspace_roots::CONFIGURATION_ROOT_ID,
                    "Module.bsl",
                    b"hash",
                    &[chunk],
                    Some(&[vec![1.0; dim]]),
                )
                .unwrap();
        }

        fn sibling(path: &std::path::Path, suffix: &str) -> std::path::PathBuf {
            let mut value = path.as_os_str().to_os_string();
            value.push(suffix);
            value.into()
        }

        fn prepared_temps(path: &std::path::Path) -> Vec<std::path::PathBuf> {
            let name = path.file_name().unwrap().to_string_lossy();
            std::fs::read_dir(path.parent().unwrap())
                .unwrap()
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|candidate| {
                    let candidate = candidate.file_name().unwrap().to_string_lossy();
                    candidate.starts_with(name.as_ref()) && candidate.contains(".tmp-")
                })
                .collect()
        }

        #[allow(clippy::type_complexity)]
        fn run_apply(
            apply: &mut dyn FnMut(
                &mut dyn FnMut() -> ControlFlow<()>,
            ) -> ControlFlow<(), Result<(), SearchError>>,
        ) -> FenceOutcome<Result<(), SearchError>> {
            CONSTRUCTOR_APPLY_ACTIVE.with(|active| {
                assert!(!active.replace(true));
                let mut checkpoint = || ControlFlow::Continue(());
                let result = match apply(&mut checkpoint) {
                    ControlFlow::Continue(result) => result,
                    ControlFlow::Break(()) => unreachable!("permit checkpoint cannot cancel"),
                };
                active.set(false);
                FenceOutcome::Applied(result)
            })
        }

        let dir = tempdir().unwrap();

        // Refusing the first group neither creates nor migrates a store.
        let refused_open = dir.path().join("refused-open.db");
        assert!(matches!(
            SearchEngine::fts_only_fenced(&refused_open, |_| FenceOutcome::TransientRefusal)
                .unwrap(),
            FenceOutcome::TransientRefusal
        ));
        assert!(!refused_open.exists());

        // FTS is its own group in the overlay constructor. The refused rebuild leaves the
        // deliberately emptied FTS table untouched.
        let refused_fts = dir.path().join("refused-fts.db");
        seed(&refused_fts, 8);
        rusqlite::Connection::open(&refused_fts)
            .unwrap()
            .execute("DELETE FROM chunks_fts", [])
            .unwrap();
        let calls = Cell::new(0usize);
        let overlay =
            SearchEngine::semantic_overlay_only_fenced(&refused_fts, config(8), |apply| {
                calls.set(calls.get() + 1);
                if calls.get() == 1 {
                    run_apply(apply)
                } else {
                    FenceOutcome::Superseded
                }
            })
            .unwrap();
        assert!(matches!(overlay, FenceOutcome::Superseded));
        assert_eq!(calls.get(), 2);
        assert_eq!(Store::open_existing(&refused_fts).unwrap().fts_count().unwrap(), 0);

        // A semantic constructor builds HNSW between callbacks. The thread-local assertion in
        // `load_or_build_index_unpublished` proves the build does not run while `run_apply` marks
        // the callback active; refusing the following sidecar group publishes neither artifact.
        let refused_sidecar = dir.path().join("refused-sidecar.db");
        seed(&refused_sidecar, 8);
        let calls = Cell::new(0usize);
        let semantic = SearchEngine::new_fenced(&refused_sidecar, config(8), |apply| {
            calls.set(calls.get() + 1);
            if calls.get() == 2 {
                assert_eq!(prepared_temps(&refused_sidecar).len(), 2);
                FenceOutcome::TransientRefusal
            } else {
                run_apply(apply)
            }
        })
        .unwrap();
        assert!(matches!(semantic, FenceOutcome::TransientRefusal));
        assert_eq!(calls.get(), 2);
        assert!(!sibling(&refused_sidecar, ".usearch").exists());
        assert!(!sibling(&refused_sidecar, ".usearch.json").exists());
        assert!(prepared_temps(&refused_sidecar).is_empty());

        // A database change after prepare is an operation error, not a lease refusal or a stale
        // sidecar publication.
        let changed_baseline = dir.path().join("changed-baseline.db");
        seed(&changed_baseline, 8);
        let calls = Cell::new(0usize);
        let changed = SearchEngine::new_fenced(&changed_baseline, config(8), |apply| {
            calls.set(calls.get() + 1);
            if calls.get() == 2 {
                assert_eq!(prepared_temps(&changed_baseline).len(), 2);
                rusqlite::Connection::open(&changed_baseline)
                    .unwrap()
                    .execute(
                        "UPDATE meta SET value = CAST(value AS INTEGER) + 1 WHERE key = 'embedding_generation'",
                        [],
                    )
                    .unwrap();
            }
            run_apply(apply)
        });
        assert!(changed.is_err());
        assert!(!sibling(&changed_baseline, ".usearch.json").exists());

        // All three modes admit successfully with the same callback contract.
        assert!(matches!(
            SearchEngine::fts_only_fenced(&dir.path().join("fts.db"), run_apply).unwrap(),
            FenceOutcome::Applied(_)
        ));
        assert!(matches!(
            SearchEngine::semantic_overlay_only_fenced(
                &dir.path().join("overlay.db"),
                config(8),
                run_apply,
            )
            .unwrap(),
            FenceOutcome::Applied(_)
        ));
        assert!(matches!(
            SearchEngine::new_fenced(&dir.path().join("semantic.db"), config(8), run_apply)
                .unwrap(),
            FenceOutcome::Applied(_)
        ));

        // An admitted operation error remains `Err`, never a callback refusal.
        let missing_parent = dir.path().join("missing").join("error.db");
        assert!(SearchEngine::fts_only_fenced(&missing_parent, run_apply).is_err());
    }

    #[test]
    #[allow(clippy::type_complexity)]
    fn embedding_publish_fence() {
        use crate::{
            Chunk, ChunkKind, EmbedderConfig, EmbeddingExecutionPolicy, SearchConfig, Store,
        };
        use std::io::{Read, Write};

        fn server(
            calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
            max_active: Option<std::sync::Arc<std::sync::atomic::AtomicUsize>>,
        ) -> String {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let active = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { continue };
                    let calls = std::sync::Arc::clone(&calls);
                    let active = std::sync::Arc::clone(&active);
                    let max_active = max_active.clone();
                    std::thread::spawn(move || {
                        calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        let mut bytes = Vec::new();
                        let mut buffer = [0u8; 2048];
                        loop {
                            let read = stream.read(&mut buffer).unwrap_or(0);
                            if read == 0 {
                                break;
                            }
                            bytes.extend_from_slice(&buffer[..read]);
                            let Some(split) =
                                bytes.windows(4).position(|window| window == b"\r\n\r\n")
                            else {
                                continue;
                            };
                            let headers = String::from_utf8_lossy(&bytes[..split]).to_lowercase();
                            let length = headers
                                .lines()
                                .find_map(|line| line.strip_prefix("content-length:"))
                                .and_then(|value| value.trim().parse::<usize>().ok())
                                .unwrap_or(0);
                            if bytes.len() >= split + 4 + length {
                                break;
                            }
                        }
                        let split =
                            bytes.windows(4).position(|window| window == b"\r\n\r\n").unwrap();
                        let input_count =
                            serde_json::from_slice::<serde_json::Value>(&bytes[split + 4..])
                                .unwrap()["input"]
                                .as_array()
                                .unwrap()
                                .len();
                        if let Some(max_active) = max_active.as_ref() {
                            let now = active.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                            max_active.fetch_max(now, std::sync::atomic::Ordering::SeqCst);
                            std::thread::sleep(std::time::Duration::from_millis(50));
                        }
                        let data: Vec<_> = (0..input_count)
                        .map(|index| serde_json::json!({"index": index, "embedding": [1.0, 0.0, 0.0]}))
                        .collect();
                        let body = serde_json::json!({"data": data}).to_string();
                        write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    )
                    .unwrap();
                        if max_active.is_some() {
                            active.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                        }
                    });
                }
            });
            format!("http://{address}")
        }

        fn seed(path: &Path, files: usize) {
            let mut store = Store::open(path).unwrap();
            for index in 0..files {
                let name = format!("П{index}");
                let chunk = Chunk {
                    kind: ChunkKind::Procedure,
                    name: name.clone(),
                    is_export: false,
                    annotations: Vec::new(),
                    line_start: 1,
                    line_end: 2,
                    text: format!("Процедура {name}()\nКонецПроцедуры"),
                };
                store
                    .reindex_file(
                        CONFIGURATION_ROOT_ID,
                        &format!("M{index}.bsl"),
                        name.as_bytes(),
                        &[chunk],
                        None,
                    )
                    .unwrap();
            }
        }

        fn sidecar(path: &Path) -> PathBuf {
            let mut value = path.as_os_str().to_os_string();
            value.push(".usearch.json");
            value.into()
        }

        fn prepared_temps(path: &Path) -> Vec<PathBuf> {
            let name = path.file_name().unwrap().to_string_lossy();
            let mut paths: Vec<_> = fs::read_dir(path.parent().unwrap())
                .unwrap()
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|candidate| {
                    let candidate = candidate.file_name().unwrap().to_string_lossy();
                    candidate.starts_with(name.as_ref()) && candidate.contains(".tmp-")
                })
                .collect();
            paths.sort();
            paths
        }

        let network_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let config = SearchConfig {
            embedder: EmbedderConfig {
                base_url: server(std::sync::Arc::clone(&network_calls), None),
                model: "test".to_owned(),
                dim: Some(3),
                api_key: None,
                provider: None,
                ..Default::default()
            },
            execution: EmbeddingExecutionPolicy {
                batch_size: 2,
                concurrency: 1,
                progress_interval: 1,
            },
        };
        let dir = tempdir().unwrap();

        let created = dir.path().join("created-by-wrapper.db");
        assert!(
            !created.exists()
                && SearchEngine::embed_pending_chunks_standalone(&created, &config, None, None)
                    .unwrap()
                    .is_empty()
                && created.exists(),
            "the direct compatibility wrapper still creates and initializes its database"
        );

        let wrapper = dir.path().join("wrapper.db");
        seed(&wrapper, 2);
        assert_eq!(
            SearchEngine::embed_pending_chunks_standalone(&wrapper, &config, None, None)
                .unwrap()
                .len(),
            2
        );
        assert!(Store::open_existing(&wrapper)
            .unwrap()
            .load_pending_embedding_documents("code")
            .unwrap()
            .is_empty());

        let parallel_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let max_active = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let parallel_config = SearchConfig {
            embedder: EmbedderConfig {
                base_url: server(
                    std::sync::Arc::clone(&parallel_calls),
                    Some(std::sync::Arc::clone(&max_active)),
                ),
                model: "parallel-test".to_owned(),
                dim: Some(3),
                api_key: None,
                provider: None,
                ..Default::default()
            },
            execution: EmbeddingExecutionPolicy {
                batch_size: 1,
                concurrency: 2,
                progress_interval: 1,
            },
        };
        let parallel = dir.path().join("parallel-direct.db");
        seed(&parallel, 2);
        SearchEngine::embed_pending_chunks_standalone(&parallel, &parallel_config, None, None)
            .unwrap();
        assert_eq!(parallel_calls.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert!(
            max_active.load(std::sync::atomic::Ordering::SeqCst) > 1,
            "the direct non-fenced path preserves configured concurrency"
        );

        let stop = || false;
        assert!(matches!(
            SearchEngine::embed_pending_chunks_fenced(
                &wrapper,
                &config,
                None,
                Some(&stop),
                |operation| FenceOutcome::Applied(operation()),
            )
            .unwrap(),
            FenceOutcome::Released
        ));
        assert_eq!(
            SearchEngine::embed_pending_chunks_standalone(&wrapper, &config, None, Some(&stop),)
                .unwrap()
                .len(),
            2,
            "the direct wrapper preserves its partial-index return contract on stop"
        );

        let preflight_refused = dir.path().join("preflight-refused.db");
        seed(&preflight_refused, 2);
        let before = network_calls.load(std::sync::atomic::Ordering::SeqCst);
        assert!(matches!(
            SearchEngine::embed_pending_chunks_fenced(
                &preflight_refused,
                &config,
                None,
                None,
                |_| FenceOutcome::TransientRefusal,
            )
            .unwrap(),
            FenceOutcome::TransientRefusal
        ));
        assert_eq!(
            network_calls.load(std::sync::atomic::Ordering::SeqCst),
            before,
            "a refused fresh collection preflight performs no network call"
        );

        let refused_batch = dir.path().join("refused-batch.db");
        seed(&refused_batch, 4);
        let mut calls = 0;
        // The progress flag outlives its status line: a broker backend stays alive while it
        // is raised, so a refused batch has to lower it again. `raised` is the positive
        // control — without it "lowered afterwards" would hold on a flag nobody ever raised.
        let progress = IndexProgress::new();
        let mut raised = false;
        let result = SearchEngine::embed_pending_chunks_fenced(
            &refused_batch,
            &config,
            Some(&progress),
            None,
            |operation| {
                calls += 1;
                raised |= progress.is_active();
                if calls == 1 {
                    FenceOutcome::Applied(operation())
                } else {
                    FenceOutcome::TransientRefusal
                }
            },
        )
        .unwrap();
        assert!(matches!(result, FenceOutcome::TransientRefusal));
        assert_eq!(calls, 2);
        assert!(raised, "the pass never marked itself active; the check below proves nothing");
        assert!(!progress.is_active(), "a refused batch left the pass marked active");
        assert_eq!(
            Store::open_existing(&refused_batch)
                .unwrap()
                .load_pending_embedding_documents("code")
                .unwrap()
                .len(),
            4,
            "the paid batch remains pending when its first commit is refused"
        );

        let retried_batch = dir.path().join("retried-batch.db");
        seed(&retried_batch, 2);
        let before = network_calls.load(std::sync::atomic::Ordering::SeqCst);
        let mut admissions = 0;
        let mut retries = 0;
        let result = SearchEngine::embed_pending_chunks_fenced_retrying(
            &retried_batch,
            &config,
            None,
            None,
            |operation| {
                admissions += 1;
                if admissions == 2 {
                    FenceOutcome::TransientRefusal
                } else {
                    FenceOutcome::Applied(operation())
                }
            },
            || {
                retries += 1;
                true
            },
        )
        .unwrap();
        assert!(matches!(result, FenceOutcome::Applied(_)));
        assert_eq!(retries, 1);
        assert_eq!(
            network_calls.load(std::sync::atomic::Ordering::SeqCst) - before,
            1,
            "retrying a refused SQLite commit must not repeat its paid network batch"
        );

        let retried_sidecar = dir.path().join("retried-sidecar.db");
        seed(&retried_sidecar, 2);
        let before = network_calls.load(std::sync::atomic::Ordering::SeqCst);
        let mut admissions = 0;
        let mut retries = 0;
        let mut refused_temps = Vec::new();
        let result = SearchEngine::embed_pending_chunks_fenced_retrying(
            &retried_sidecar,
            &config,
            None,
            None,
            |operation| {
                admissions += 1;
                if admissions == 3 {
                    refused_temps = prepared_temps(&retried_sidecar);
                    assert_eq!(refused_temps.len(), 2);
                    FenceOutcome::TransientRefusal
                } else {
                    if admissions == 4 {
                        assert_eq!(prepared_temps(&retried_sidecar), refused_temps);
                    }
                    FenceOutcome::Applied(operation())
                }
            },
            || {
                retries += 1;
                true
            },
        )
        .unwrap();
        assert!(matches!(result, FenceOutcome::Applied(_)));
        assert_eq!(retries, 1);
        assert_eq!(
            network_calls.load(std::sync::atomic::Ordering::SeqCst) - before,
            1,
            "retrying the prepared final bundle must not repeat network work"
        );
        assert!(sidecar(&retried_sidecar).exists());
        assert!(prepared_temps(&retried_sidecar).is_empty());

        let refused_sidecar = dir.path().join("refused-sidecar.db");
        seed(&refused_sidecar, 4);
        let mut calls = 0;
        let result = SearchEngine::embed_pending_chunks_fenced(
            &refused_sidecar,
            &config,
            None,
            None,
            |operation| {
                calls += 1;
                if calls < 5 {
                    FenceOutcome::Applied(operation())
                } else {
                    FenceOutcome::Superseded
                }
            },
        )
        .unwrap();
        assert!(matches!(result, FenceOutcome::Superseded));
        assert_eq!(calls, 5);
        assert!(Store::open_existing(&refused_sidecar)
            .unwrap()
            .load_pending_embedding_documents("code")
            .unwrap()
            .is_empty());
        assert!(!sidecar(&refused_sidecar).exists());

        let refused_overlay = dir.path().join("refused-overlay");
        fs::create_dir(&refused_overlay).unwrap();
        fs::write(refused_overlay.join("Changed.bsl"), "Процедура Измененная()\nКонецПроцедуры")
            .unwrap();
        let overlay_db = refused_overlay.join("search.db");
        Store::open(&overlay_db)
            .unwrap()
            .save_baseline_manifest(&crate::WorkspaceBaselineManifest {
                snapshot_id: "baseline".to_owned(),
                snapshot_fingerprint: None,
                files: Vec::new(),
            })
            .unwrap();
        let roots = crate::WorkspaceRoots::build(&refused_overlay, &refused_overlay, &[]).0;
        let before = network_calls.load(std::sync::atomic::Ordering::SeqCst);
        let result = SearchEngine::prime_workspace_overlay_standalone(
            &overlay_db,
            config.embedder.clone(),
            &roots,
            std::collections::HashMap::new(),
            None,
            &|| true,
            |_| FenceOutcome::TransientRefusal,
            &std::collections::HashSet::new(),
        );
        assert!(matches!(result.unwrap(), FenceOutcome::TransientRefusal));
        assert_eq!(
            network_calls.load(std::sync::atomic::Ordering::SeqCst),
            before,
            "a refused fresh overlay preflight performs no network call"
        );
        assert!(Store::open_existing(&overlay_db)
            .unwrap()
            .load_overlay_embedding_cache("test", 3)
            .unwrap()
            .is_empty());

        let before = network_calls.load(std::sync::atomic::Ordering::SeqCst);
        let mut admissions = 0;
        let mut retries = 0;
        let primed = SearchEngine::prime_workspace_overlay_standalone_retrying(
            &overlay_db,
            config.embedder.clone(),
            &roots,
            std::collections::HashMap::new(),
            None,
            &|| true,
            |operation| {
                admissions += 1;
                if admissions == 2 {
                    FenceOutcome::TransientRefusal
                } else {
                    FenceOutcome::Applied(operation())
                }
            },
            &std::collections::HashSet::new(),
            || {
                retries += 1;
                true
            },
        )
        .unwrap();
        let FenceOutcome::Applied((plan, embeddings)) = primed else {
            panic!("the retained overlay batch must commit on retry")
        };
        assert_eq!(retries, 1);
        assert_eq!(
            network_calls.load(std::sync::atomic::Ordering::SeqCst) - before,
            1,
            "retrying the refused overlay cache commit retains its paid batch"
        );

        let mut overlay_engine = SearchEngine::fts_only(&overlay_db).unwrap();
        overlay_engine.initialize_workspace_roots(roots).unwrap();
        overlay_engine.set_serves_external_baseline(true).unwrap();
        let baseline = overlay_engine.workspace_overlay_publication_baseline().unwrap();
        let mut staged = overlay_engine
            .stage_workspace_overlay_publication(plan, embeddings, &baseline)
            .unwrap();
        let after_phase_b = network_calls.load(std::sync::atomic::Ordering::SeqCst);
        let mut refuse_once = true;
        let mut publish =
            |operation: &mut dyn FnMut(
                &mut dyn FnMut() -> ControlFlow<()>,
            ) -> ControlFlow<(), Result<(), SearchError>>| {
                if std::mem::take(&mut refuse_once) {
                    FenceOutcome::TransientRefusal
                } else {
                    SearchEngine::permit_checkpointed_apply(operation)
                }
            };
        assert!(matches!(
            SearchEngine::fenced_checkpointed_value(&mut publish, |checkpoint| {
                overlay_engine.apply_staged_workspace_overlay_publication(&mut staged, checkpoint)
            })
            .unwrap(),
            FenceOutcome::TransientRefusal
        ));
        assert!(matches!(
            SearchEngine::fenced_checkpointed_value(&mut publish, |checkpoint| {
                overlay_engine.apply_staged_workspace_overlay_publication(&mut staged, checkpoint)
            })
            .unwrap(),
            FenceOutcome::Applied(super::PublishOutcome::Applied { .. })
        ));
        assert_eq!(
            network_calls.load(std::sync::atomic::Ordering::SeqCst),
            after_phase_b,
            "retrying the staged Phase C bundle performs no network call"
        );
    }

    #[test]
    fn text_search_sees_workspace_overlay_without_reindex() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let file = workspace.join("CommonModule.bsl");
        fs::write(&file, "Процедура СтараяПроцедура()\nКонецПроцедуры").unwrap();

        let db_path = workspace.join("bsl-search.db");
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.index_directory_fts(workspace).unwrap();
        engine.set_workspace_root(workspace);

        fs::write(&file, "Процедура НоваяПроцедура()\nКонецПроцедуры").unwrap();

        // The warmup (Embed) builds the overlay; interactive queries (ReuseOnly) never cold-scan.
        engine.prime_workspace_overlay().unwrap();

        let hits = engine.text_search("НоваяПроцедура", 10, Some("code")).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].symbol_name, "НоваяПроцедура");
    }

    #[test]
    fn index_directory_deferred_preserves_graph_context_without_embedding() {
        struct StubProvider;
        impl crate::ports::GraphContextProvider for StubProvider {
            fn graph_context(
                &self,
                _rel_path: &str,
                symbol_name: &str,
                _kind: &str,
            ) -> Option<String> {
                Some(format!("calls: {symbol_name}_helper"))
            }
        }

        let dir = tempdir().unwrap();
        let workspace = dir.path();
        fs::write(workspace.join("CommonModule.bsl"), "Процедура Тест()\nКонецПроцедуры").unwrap();

        let db_path = workspace.join("bsl-search.db");
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.set_graph_context_provider(std::sync::Arc::new(StubProvider));

        let indexed = engine.index_directory_deferred(workspace).unwrap();
        assert_eq!(indexed, 1);

        // Chunks are written with graph context but no vectors yet — the deferred
        // background pass embeds the stored, already-enriched text.
        assert_eq!(engine.vector_count(), 0);
        let pending = engine.store().load_pending_embedding_documents("code").unwrap();
        let method = pending
            .iter()
            .find(|(_, doc)| doc.symbol_name == "Тест")
            .expect("method chunk should be pending embedding");
        assert_eq!(method.1.graph_context.as_deref(), Some("calls: Тест_helper"));
    }

    /// A context the provider failed to render is indexed as owed, not as "no context": the
    /// file is marked for the next context refresh and the provider's owner is told the mark.
    #[test]
    fn index_directory_deferred_owes_a_context_the_provider_failed_to_render() {
        use std::sync::atomic::{AtomicI64, Ordering};

        #[derive(Default)]
        struct Unreadable {
            owed: AtomicI64,
        }
        impl crate::ports::GraphContextProvider for Unreadable {
            fn graph_context(&self, _: &str, _: &str, _: &str) -> Option<String> {
                None
            }
            fn try_graph_context(
                &self,
                _: &str,
                _: &str,
                _: &str,
            ) -> Result<Option<String>, crate::GraphContextError> {
                Err(crate::GraphContextError("graph unavailable".to_owned()))
            }
            fn context_marks_owed(&self, mark_high: i64) {
                self.owed.fetch_max(mark_high, Ordering::SeqCst);
            }
        }

        let dir = tempdir().unwrap();
        let workspace = dir.path();
        fs::write(workspace.join("CommonModule.bsl"), "Процедура Тест()\nКонецПроцедуры").unwrap();

        let db_path = workspace.join("bsl-search.db");
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        let provider = std::sync::Arc::new(Unreadable::default());
        engine.set_graph_context_provider(provider.clone());

        assert_eq!(engine.index_directory_deferred(workspace).unwrap(), 1, "the file is indexed");
        let key = FileKey::configuration("CommonModule.bsl");
        assert!(
            engine.context_dirty_paths("code").unwrap().contains(&key),
            "the file owes the render it could not get"
        );
        let owed = provider.owed.load(Ordering::SeqCst);
        assert!(owed > 0, "the provider's owner is told which mark is owed");
        assert_eq!(
            engine.store().context_dirty_paths_bounded("code", owed).unwrap().len(),
            1,
            "the reported mark covers the file"
        );
    }

    #[test]
    fn text_search_hides_deleted_baseline_file() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let file = workspace.join("CommonModule.bsl");
        fs::write(&file, "Процедура УдаляемаяПроцедура()\nКонецПроцедуры").unwrap();

        let db_path = workspace.join("bsl-search.db");
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.index_directory_fts(workspace).unwrap();
        engine.set_workspace_root(workspace);

        fs::remove_file(&file).unwrap();

        // The warmup (Embed) builds the overlay; interactive queries (ReuseOnly) never cold-scan.
        engine.prime_workspace_overlay().unwrap();

        let hits = engine.text_search("УдаляемаяПроцедура", 10, Some("code")).unwrap();
        assert!(hits.is_empty());
    }

    /// The debt of an overlay that has never been built is not zero, and the counts cannot say
    /// so: they answer for marks the overlay could serve, and an uninitialized one serves none.
    /// A reader that took the counts alone would call such an answer complete — permanently so
    /// in a profile whose overlay is never built at all.
    #[test]
    fn an_uninitialized_overlay_owes_what_its_counts_cannot_say() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let file = workspace.join("CommonModule.bsl");
        fs::write(&file, "Процедура СтараяПроцедура()\nКонецПроцедуры").unwrap();

        let db_path = workspace.join("bsl-search.db");
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.index_directory_fts(workspace).unwrap();
        engine.set_workspace_root(workspace);
        engine.enable_workspace_watcher_mode();

        fs::write(&file, "Процедура НоваяПроцедура()\nКонецПроцедуры").unwrap();
        assert!(engine.mark_workspace_path_dirty(&file).unwrap());

        let owed = engine.workspace_overlay_debt().unwrap();
        assert!(!owed.initialized, "no pass has built this overlay yet");
        assert_eq!(
            owed.pending, 0,
            "the count an uninitialized overlay reports is the same zero a caught-up one \
             reports — which is why `initialized` has to be a fact of its own"
        );
        assert_eq!(owed.unread, 0);

        engine.prime_workspace_overlay().unwrap();

        let owed = engine.workspace_overlay_debt().unwrap();
        assert!(owed.initialized, "the priming pass built the overlay");
        assert_eq!(owed.pending, 0, "the pass answered the mark it was given");

        fs::write(&file, "Процедура ТретьяПроцедура()\nКонецПроцедуры").unwrap();
        assert!(engine.mark_workspace_path_dirty(&file).unwrap());
        let owed = engine.workspace_overlay_debt().unwrap();
        assert!(owed.initialized);
        assert_eq!(owed.pending, 1, "an initialized overlay counts what it still owes");
    }

    #[test]
    fn workspace_overlay_stats_report_changed_files() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let file = workspace.join("CommonModule.bsl");
        fs::write(&file, "Процедура СтараяПроцедура()\nКонецПроцедуры").unwrap();

        let db_path = workspace.join("bsl-search.db");
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.index_directory_fts(workspace).unwrap();
        engine.set_workspace_root(workspace);

        fs::write(&file, "Процедура НоваяПроцедура()\nКонецПроцедуры").unwrap();

        // The warmup (Embed) builds the overlay; `search status` (ReuseOnly) never cold-scans.
        engine.prime_workspace_overlay().unwrap();

        let stats = engine.workspace_overlay_stats().unwrap().unwrap();
        assert_eq!(stats.overlay_files, 1);
        assert_eq!(stats.deleted_files, 0);
        assert_eq!(stats.hidden_paths, 1);
        assert_eq!(stats.lexical_chunks, 1);
    }

    #[test]
    fn resolved_workspace_view_combines_local_baseline_with_overlay() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let changed = workspace.join("ChangedModule.bsl");
        let stable = workspace.join("StableModule.bsl");
        fs::write(&changed, "Процедура СтараяПроцедура()\nКонецПроцедуры").unwrap();
        fs::write(&stable, "Процедура СтабильнаяПроцедура()\nКонецПроцедуры").unwrap();

        let db_path = workspace.join("bsl-search.db");
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.index_directory_fts(workspace).unwrap();
        engine.set_workspace_root(workspace);

        fs::write(&changed, "Процедура НоваяПроцедура()\nКонецПроцедуры").unwrap();

        // The warmup (Embed) builds the overlay; the resolved view reads it via ReuseOnly.
        engine.prime_workspace_overlay().unwrap();

        let view = engine.resolve_workspace_code_view().unwrap().unwrap();
        let symbols: HashSet<&str> =
            view.documents().iter().map(|document| document.symbol_name.as_str()).collect();

        assert!(symbols.contains("НоваяПроцедура"));
        assert!(symbols.contains("СтабильнаяПроцедура"));
        assert!(!symbols.contains("СтараяПроцедура"));
    }

    #[test]
    fn resolved_workspace_view_can_target_explicit_baseline_snapshot() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let changed = workspace.join("ChangedModule.bsl");
        fs::write(&changed, "Процедура ЛокальнаяВерсия()\nКонецПроцедуры").unwrap();

        let db_path = workspace.join("bsl-search.db");
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.index_directory_fts(workspace).unwrap();
        engine.set_workspace_root(workspace);

        fs::write(&changed, "Процедура OverlayВерсия()\nКонецПроцедуры").unwrap();

        // The warmup (Embed) builds the overlay; the resolved view reads it via ReuseOnly.
        engine.prime_workspace_overlay().unwrap();

        let baseline = BaselineRef::for_snapshot(CorpusId::WorkspaceCode, "external-main");
        let snapshot = Snapshot::new("external-main", CorpusId::WorkspaceCode);
        let mut catalog = TestCatalog::default();
        catalog.snapshots.insert(snapshot.id.0.clone(), snapshot.clone());

        let mut content_store = TestContentStore::default();
        content_store.documents.insert(
            snapshot.id.0.clone(),
            vec![
                IndexedDocument {
                    collection: "code".to_owned(),
                    root_id: crate::CONFIGURATION_ROOT_ID.to_owned(),
                    path: "ChangedModule.bsl".to_owned(),
                    symbol_name: "БазоваяВерсия".to_owned(),
                    kind: "procedure".to_owned(),
                    line_start: 1,
                    line_end: 2,
                    text: "базовая".to_owned(),
                    content_hash: "base-changed".to_owned(),
                    graph_context: None,
                    source_span: None,
                },
                IndexedDocument {
                    collection: "code".to_owned(),
                    root_id: crate::CONFIGURATION_ROOT_ID.to_owned(),
                    path: "StableModule.bsl".to_owned(),
                    symbol_name: "СтабильноИзBaseline".to_owned(),
                    kind: "procedure".to_owned(),
                    line_start: 1,
                    line_end: 2,
                    text: "stable".to_owned(),
                    content_hash: "base-stable".to_owned(),
                    graph_context: None,
                    source_span: None,
                },
            ],
        );

        let view = engine
            .resolve_workspace_code_view_with(baseline, catalog, content_store)
            .unwrap()
            .unwrap();
        let symbols: HashSet<&str> =
            view.documents().iter().map(|document| document.symbol_name.as_str()).collect();

        assert!(symbols.contains("OverlayВерсия"));
        assert!(symbols.contains("СтабильноИзBaseline"));
        assert!(!symbols.contains("БазоваяВерсия"));
    }

    #[test]
    fn workspace_overlay_stats_use_persisted_manifest_without_hiding_unchanged_files() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let file = workspace.join("CommonModule.bsl");
        fs::write(&file, "Процедура БазоваяПроцедура()\nКонецПроцедуры").unwrap();

        let db_path = workspace.join("bsl-search.db");
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.set_workspace_root(workspace);
        engine.set_serves_external_baseline(true).unwrap();
        engine
            .store()
            .save_baseline_manifest(&crate::WorkspaceBaselineManifest {
                snapshot_id: "snap-1".to_owned(),
                snapshot_fingerprint: Some("fp-1".to_owned()),
                files: vec![crate::BaselineManifestFile {
                    root_id: crate::CONFIGURATION_ROOT_ID.to_owned(),
                    collection: "code".to_owned(),
                    path: "CommonModule.bsl".to_owned(),
                    file_fingerprint: crate::workspace_overlay::fingerprint_content(
                        "Процедура БазоваяПроцедура()\nКонецПроцедуры",
                        "CommonModule.bsl",
                    ),
                    document_count: 1,
                    file_object_id: "obj-1".to_owned(),
                }],
            })
            .unwrap();

        let stats = engine.workspace_overlay_stats().unwrap().unwrap();
        assert_eq!(stats.overlay_files, 0);
        assert_eq!(stats.deleted_files, 0);
        assert_eq!(stats.hidden_paths, 0);
    }

    #[test]
    fn workspace_overlay_lexical_hits_use_persisted_manifest_for_modified_file() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let file = workspace.join("CommonModule.bsl");
        fs::write(&file, "Процедура ЛокальнаяПроцедура()\nКонецПроцедуры").unwrap();

        let db_path = workspace.join("bsl-search.db");
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.set_workspace_root(workspace);
        engine.set_serves_external_baseline(true).unwrap();
        engine
            .store()
            .save_baseline_manifest(&crate::WorkspaceBaselineManifest {
                snapshot_id: "snap-1".to_owned(),
                snapshot_fingerprint: Some("fp-1".to_owned()),
                files: vec![crate::BaselineManifestFile {
                    root_id: crate::CONFIGURATION_ROOT_ID.to_owned(),
                    collection: "code".to_owned(),
                    path: "CommonModule.bsl".to_owned(),
                    file_fingerprint: "different-fingerprint".to_owned(),
                    document_count: 1,
                    file_object_id: "obj-1".to_owned(),
                }],
            })
            .unwrap();

        // The warmup (Embed) builds the overlay; interactive queries (ReuseOnly) never cold-scan.
        engine.prime_workspace_overlay().unwrap();

        let (hits, hidden_paths) =
            engine.workspace_overlay_lexical_hits("ЛокальнаяПроцедура", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].symbol_name, "ЛокальнаяПроцедура");
        assert!(hidden_paths.contains(&FileKey::configuration("CommonModule.bsl")));
    }

    #[test]
    fn watcher_mode_applies_dirty_file_updates() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let file = workspace.join("CommonModule.bsl");
        fs::write(&file, "Процедура СтараяПроцедура()\nКонецПроцедуры").unwrap();

        let db_path = workspace.join("bsl-search.db");
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.index_directory_fts(workspace).unwrap();
        engine.set_workspace_root(workspace);
        engine.enable_workspace_watcher_mode();
        // The warmup (Embed) initializes the overlay; afterwards the watcher's dirty-path marks are
        // applied incrementally on the next ReuseOnly query without any cold full-tree scan.
        engine.prime_workspace_overlay().unwrap();

        let initial = engine.workspace_overlay_stats().unwrap().unwrap();
        assert!(initial.watcher_mode);
        assert_eq!(initial.overlay_files, 0);

        fs::write(&file, "Процедура ОбновленаЧерезWatcher()\nКонецПроцедуры").unwrap();
        assert!(engine.mark_workspace_path_dirty(&file).unwrap());

        let hits = engine.text_search("ОбновленаЧерезWatcher", 10, Some("code")).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].symbol_name, "ОбновленаЧерезWatcher");
    }

    /// A dirty mark is not a feeder: on a store nothing feeds, marking a path must not flip the
    /// overlay into watcher mode. The mode doubles as "skip the full rescan", so a mark alone
    /// would stop the corpus from ever following the disk (github#186).
    #[test]
    fn a_dirty_mark_alone_does_not_enable_watcher_mode() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let file = workspace.join("A.bsl");
        fs::write(&file, "Процедура П()\nКонецПроцедуры").unwrap();

        let mut engine = SearchEngine::fts_only(&workspace.join("bsl-search.db")).unwrap();
        let (roots, _) = crate::WorkspaceRoots::build(workspace, workspace, &[]);
        engine.initialize_workspace_roots(roots).unwrap();
        engine.initialize_workspace_overlay_clean().unwrap();

        assert!(engine.mark_workspace_path_dirty(&file).unwrap());
        assert!(
            engine
                .workspace_overlay_dirty_paths()
                .unwrap()
                .contains(&FileKey::configuration("A.bsl")),
            "the mark itself is recorded",
        );
        assert!(
            !engine.workspace_overlay_stats().unwrap().unwrap().watcher_mode,
            "a mark is not a feeder and must not skip the full rescan",
        );
    }

    /// The same rule for a removal: it records its mark and its hiding, but the mode stays off
    /// until a feeder asks for it — the boot reconcile removes before any sink exists, and
    /// flipping the mode there would leave a hub-less daemon following nothing (github#186).
    #[test]
    fn a_removal_alone_does_not_enable_watcher_mode() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let file = workspace.join("A.bsl");
        fs::write(&file, "Процедура П()\nКонецПроцедуры").unwrap();

        let mut engine = SearchEngine::fts_only(&workspace.join("bsl-search.db")).unwrap();
        let (roots, _) = crate::WorkspaceRoots::build(workspace, workspace, &[]);
        engine.initialize_workspace_roots(roots).unwrap();
        engine.initialize_workspace_overlay_clean().unwrap();
        engine.store().upsert_file(crate::CONFIGURATION_ROOT_ID, "A.bsl", b"h", "code").unwrap();

        fs::remove_file(&file).unwrap();
        assert!(engine.remove_workspace_path(&file).unwrap());
        assert!(
            !engine.workspace_overlay_stats().unwrap().unwrap().watcher_mode,
            "a removal is not a feeder either",
        );
    }

    #[test]
    fn search_with_embedding_uses_precomputed_vector_without_network() {
        use crate::embedder::EmbedderConfig;
        use crate::{Chunk, ChunkKind, Store};

        // Populate a file-backed store with two chunks carrying distinct stored vectors, so the
        // engine builds a real vector index from them. The embedder points at an unreachable URL:
        // the embedding-free search paths must never call it, so the query resolves offline.
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("bsl-search.db");

        let chunk = |name: &str| Chunk {
            kind: ChunkKind::Procedure,
            name: name.to_owned(),
            is_export: true,
            annotations: vec![],
            line_start: 0,
            line_end: 1,
            text: format!("Процедура {name}()\nКонецПроцедуры"),
        };
        let vec_a = vec![1.0f32, 0.0, 0.0];
        let vec_b = vec![0.0f32, 1.0, 0.0];
        {
            let mut store = Store::open(&db_path).unwrap();
            store
                .reindex_file(
                    CONFIGURATION_ROOT_ID,
                    "a.bsl",
                    b"ha",
                    &[chunk("Альфа")],
                    Some(std::slice::from_ref(&vec_a)),
                )
                .unwrap();
            store
                .reindex_file(
                    CONFIGURATION_ROOT_ID,
                    "b.bsl",
                    b"hb",
                    &[chunk("Бета")],
                    Some(std::slice::from_ref(&vec_b)),
                )
                .unwrap();
        }

        let config = crate::SearchConfig {
            embedder: EmbedderConfig {
                base_url: "http://127.0.0.1:1".to_owned(),
                model: "test-model".to_owned(),
                dim: Some(3),
                api_key: None,
                provider: None,
                ..Default::default()
            },
            execution: crate::EmbeddingExecutionPolicy::default(),
        };
        let engine = SearchEngine::new(&db_path, config).unwrap();

        // Querying with chunk A's own vector ranks A first; with B's vector, B first. This
        // exercises `search_with_embedding` -> `search_persisted_with_embedding` (the batched
        // `chunks_by_ids` lookup) end to end with no embed round-trip.
        let hits_a = engine.search_with_embedding(&vec_a, 5, None).unwrap();
        assert_eq!(hits_a.first().map(|h| h.symbol_name.as_str()), Some("Альфа"));

        let hits_b = engine.search_with_embedding(&vec_b, 5, None).unwrap();
        assert_eq!(hits_b.first().map(|h| h.symbol_name.as_str()), Some("Бета"));
    }

    #[test]
    fn cancelled_overlay_phase_c_rolls_back_and_retries_the_same_bundle() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let mut manifest = HashMap::new();
        let mut manifest_files = Vec::new();
        for index in 0..=WORKSPACE_APPLY_BATCH_ROWS {
            let path = format!("M{index}.bsl");
            fs::write(
                workspace.join(&path),
                format!("Процедура Локальная{index}()\nКонецПроцедуры"),
            )
            .unwrap();
            let key = FileKey::configuration(&path);
            manifest.insert(key, "remote-version".to_owned());
            manifest_files.push(crate::BaselineManifestFile {
                root_id: CONFIGURATION_ROOT_ID.to_owned(),
                collection: "code".to_owned(),
                path,
                file_fingerprint: "remote-version".to_owned(),
                document_count: 1,
                file_object_id: format!("object-{index}"),
            });
        }
        let mut engine = SearchEngine::fts_only(&workspace.join("search.db")).unwrap();
        let roots = crate::WorkspaceRoots::build(workspace, workspace, &[]).0;
        engine.initialize_workspace_roots(roots.clone()).unwrap();
        engine
            .store()
            .save_baseline_manifest(&crate::WorkspaceBaselineManifest {
                snapshot_id: "snap".to_owned(),
                snapshot_fingerprint: Some("fingerprint".to_owned()),
                files: manifest_files,
            })
            .unwrap();
        engine.set_serves_external_baseline(true).unwrap();
        let baseline = engine.workspace_overlay_publication_baseline().unwrap();
        let plan =
            crate::workspace_overlay::WorkspaceOverlayCache::plan_full_refresh_from_manifest(
                &manifest,
                &roots,
                engine.store(),
                &HashMap::new(),
                None,
                baseline.distrusted(),
            )
            .unwrap();
        let cache_before = engine.workspace_overlay_stats().unwrap();
        let mut prepared =
            engine.stage_workspace_overlay_publication(plan, HashMap::new(), &baseline).unwrap();

        let mut checkpoints = 0;
        assert!(engine
            .apply_staged_workspace_overlay_publication(&mut prepared, &mut || {
                checkpoints += 1;
                if checkpoints == 2 {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                }
            })
            .is_break());
        assert_eq!(checkpoints, 2);
        assert_eq!(engine.workspace_overlay_stats().unwrap(), cache_before);
        assert!(engine
            .store()
            .load_overlay_fingerprint_cache("snap")
            .unwrap()
            .unwrap_or_default()
            .is_empty());

        let mut permit = || ControlFlow::Continue(());
        assert!(matches!(
            engine.apply_staged_workspace_overlay_publication(&mut prepared, &mut permit),
            ControlFlow::Continue(Ok(super::PublishOutcome::Applied { overlay_files, .. }))
                if overlay_files == WORKSPACE_APPLY_BATCH_ROWS + 1
        ));
        assert_eq!(
            engine.store().load_overlay_fingerprint_cache("snap").unwrap().unwrap().len(),
            WORKSPACE_APPLY_BATCH_ROWS + 1
        );
    }

    #[test]
    fn interactive_overlay_semantic_does_not_embed_overlay_chunks_when_vectors_absent() {
        use crate::embedder::EmbedderConfig;
        use std::time::{Duration, Instant};

        // An overlay engine wired to an unreachable embedder. The interactive overlay refresh is
        // ReuseOnly: with no cached vectors it must NOT embed the changed file's chunks inline
        // (that would hit the dead embedder and stall the lock-held query). The overlay still
        // refreshes lexically, and the call returns promptly rather than blocking on an embed.
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let file = workspace.join("CommonModule.bsl");
        fs::write(&file, "Процедура ЛокальнаяПравка()\nКонецПроцедуры").unwrap();

        let db_path = workspace.join("bsl-search.db");
        let config = crate::SearchConfig {
            embedder: EmbedderConfig {
                base_url: "http://127.0.0.1:1".to_owned(),
                model: "test-model".to_owned(),
                dim: Some(3),
                api_key: None,
                provider: None,
                ..Default::default()
            },
            execution: crate::EmbeddingExecutionPolicy::default(),
        };
        let mut engine = SearchEngine::semantic_overlay_only(&db_path, config).unwrap();
        engine.set_workspace_root(workspace);
        engine.set_serves_external_baseline(true).unwrap();
        engine
            .store()
            .save_baseline_manifest(&crate::WorkspaceBaselineManifest {
                snapshot_id: "snap-1".to_owned(),
                snapshot_fingerprint: Some("fp-1".to_owned()),
                files: vec![crate::BaselineManifestFile {
                    root_id: crate::CONFIGURATION_ROOT_ID.to_owned(),
                    collection: "code".to_owned(),
                    path: "CommonModule.bsl".to_owned(),
                    file_fingerprint: "different-fingerprint".to_owned(),
                    document_count: 1,
                    file_object_id: "obj-1".to_owned(),
                }],
            })
            .unwrap();

        // Populate the overlay lexically the way the lock-free warmup does — plan the refresh and
        // publish it with NO embeddings (the embed step failed against the dead endpoint). This is
        // the exact failed-semantic-warmup state the fix targets: the overlay carries lexical docs
        // but no vectors, and interactive queries must answer from it without ever cold-scanning or
        // embedding inline.
        let manifest =
            engine.store().load_baseline_manifest_fingerprints("code").unwrap().unwrap_or_default();
        let plan =
            crate::workspace_overlay::WorkspaceOverlayCache::plan_full_refresh_from_manifest(
                &manifest,
                &crate::WorkspaceRoots::build(workspace, workspace, &[]).0,
                engine.store(),
                &std::collections::HashMap::new(),
                None,
                &std::collections::HashSet::new(),
            )
            .unwrap();
        engine
            .publish_workspace_overlay(
                plan,
                std::collections::HashMap::new(),
                &engine.workspace_overlay_publication_baseline().unwrap(),
            )
            .unwrap();

        // Lexical overlay still sees the change without any embedding round-trip.
        let (lexical, _hidden) =
            engine.workspace_overlay_lexical_hits("ЛокальнаяПравка", 10).unwrap();
        assert_eq!(lexical.len(), 1);

        // The semantic overlay query embeds only the QUERY (fast connection-refused on a dead
        // endpoint), never the overlay chunks. Either way it returns quickly; it must not stall
        // trying to embed the uncached chunk. The result is allowed to be an error (query embed
        // failed), but it must come back fast.
        let started = Instant::now();
        let _ = engine.workspace_overlay_semantic_hits("ЛокальнаяПравка", 10);
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "ReuseOnly query must not block on inline overlay embedding"
        );
    }

    /// The consumer for the context-dirty marks: `refresh_dirty_contexts` re-renders each
    /// dirty file's chunks against the provider, rewrites only those whose context
    /// changed (clearing their embedding so the embed machinery re-embeds), leaves
    /// unchanged ones alone, and clears every processed mark. Without this consumer the
    /// marks are write-only and `.xml` edits re-render nothing.
    #[test]
    fn refresh_dirty_contexts_rerenders_changed_context_and_clears_marks() {
        use crate::{Chunk, ChunkKind, Store};

        struct Stub;
        impl crate::ports::GraphContextProvider for Stub {
            fn graph_context(&self, _rel: &str, symbol_name: &str, _kind: &str) -> Option<String> {
                match symbol_name {
                    "Изменённая" => Some("новый контекст".to_owned()),
                    "Стабильная" => Some("тот же контекст".to_owned()),
                    _ => None,
                }
            }
        }

        let dir = tempdir().unwrap();
        let db_path = dir.path().join("bsl-search.db");
        let chunk = |name: &str| Chunk {
            kind: ChunkKind::Procedure,
            name: name.to_owned(),
            is_export: true,
            annotations: vec![],
            line_start: 0,
            line_end: 1,
            text: format!("Процедура {name}()\nКонецПроцедуры"),
        };
        let vec = vec![1.0f32, 0.0, 0.0];
        {
            let mut store = Store::open(&db_path).unwrap();
            store
                .reindex_file_with_context(
                    CONFIGURATION_ROOT_ID,
                    "Owned.bsl",
                    b"h1",
                    &[chunk("Изменённая")],
                    Some(std::slice::from_ref(&vec)),
                    Some(&[Some("старый контекст".to_owned())]),
                )
                .unwrap();
            store
                .reindex_file_with_context(
                    CONFIGURATION_ROOT_ID,
                    "Stable.bsl",
                    b"h2",
                    &[chunk("Стабильная")],
                    Some(std::slice::from_ref(&vec)),
                    Some(&[Some("тот же контекст".to_owned())]),
                )
                .unwrap();
        }

        let engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.store().mark_context_dirty("code", CONFIGURATION_ROOT_ID, "Owned.bsl").unwrap();
        engine.store().mark_context_dirty("code", CONFIGURATION_ROOT_ID, "Stable.bsl").unwrap();
        let gen_before = engine.store().embedding_generation().unwrap();

        let stats = engine.refresh_dirty_contexts(&Stub, i64::MAX).unwrap();
        assert_eq!(stats.paths_cleared, 2, "both marked paths are processed");
        assert_eq!(stats.chunks_updated, 1, "only the file whose context changed is rewritten");
        assert_eq!(stats.cleared_embeddings, 1, "the one rewritten chunk had its embedding NULLed");

        // Every mark is cleared.
        assert!(engine.context_dirty_paths("code").unwrap().is_empty());

        // The changed context is rewritten; the stable one is untouched.
        let docs = engine.store().load_indexed_documents(Some("code")).unwrap();
        let changed = docs.iter().find(|d| d.symbol_name == "Изменённая").unwrap();
        assert_eq!(changed.graph_context.as_deref(), Some("новый контекст"));
        let stable = docs.iter().find(|d| d.symbol_name == "Стабильная").unwrap();
        assert_eq!(stable.graph_context.as_deref(), Some("тот же контекст"));

        // Only the changed chunk had its embedding cleared (→ pending re-embed), which
        // bumped the vector generation; the stable chunk kept its vector.
        assert!(engine.store().embedding_generation().unwrap() > gen_before);
        let pending = engine.store().load_pending_embedding_documents("code").unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].1.symbol_name, "Изменённая");
    }

    #[test]
    #[allow(clippy::type_complexity)]
    fn fenced_context_refresh_preserves_completed_batch_and_clears_mark_only_after_retry() {
        use crate::{Chunk, ChunkKind, Store};

        struct NewContext;
        impl crate::ports::GraphContextProvider for NewContext {
            fn graph_context(&self, _rel: &str, _symbol: &str, _kind: &str) -> Option<String> {
                Some("new context".to_owned())
            }
        }

        let dir = tempdir().unwrap();
        let db_path = dir.path().join("bsl-search.db");
        let chunks: Vec<_> = (0..=WORKSPACE_APPLY_BATCH_ROWS)
            .map(|index| Chunk {
                kind: ChunkKind::Procedure,
                name: format!("P{index}"),
                is_export: true,
                annotations: vec![],
                line_start: index as u32,
                line_end: index as u32 + 1,
                text: format!("Procedure P{index}()\nEndProcedure"),
            })
            .collect();
        let embeddings = vec![vec![1.0f32, 0.0, 0.0]; chunks.len()];
        let contexts = vec![Some("old context".to_owned()); chunks.len()];
        {
            let mut store = Store::open(&db_path).unwrap();
            store
                .reindex_file_with_context(
                    CONFIGURATION_ROOT_ID,
                    "Owned.bsl",
                    b"hash",
                    &chunks,
                    Some(&embeddings),
                    Some(&contexts),
                )
                .unwrap();
        }
        let engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.store().mark_context_dirty("code", CONFIGURATION_ROOT_ID, "Owned.bsl").unwrap();

        let mut admissions = 0;
        let mut checkpoints = 0;
        let mut virtual_now = 0u64;
        let mut heartbeat = 0u64;
        const VIRTUAL_STALE_AFTER: u64 = 60;
        let mut stop_before_second =
            |operation: &mut dyn FnMut(
                &mut dyn FnMut() -> ControlFlow<()>,
            ) -> ControlFlow<(), Result<(), SearchError>>| {
                admissions += 1;
                assert!(
                    virtual_now - heartbeat <= VIRTUAL_STALE_AFTER,
                    "the refreshed heartbeat keeps the live owner admissible"
                );
                if admissions == 2 {
                    return FenceOutcome::Released;
                }
                let mut checkpoint = || {
                    checkpoints += 1;
                    virtual_now += 31;
                    heartbeat = virtual_now;
                    ControlFlow::Continue(())
                };
                match operation(&mut checkpoint) {
                    ControlFlow::Break(()) => FenceOutcome::Released,
                    ControlFlow::Continue(result) => FenceOutcome::Applied(result),
                }
            };
        let (partial, outcome) = engine
            .refresh_dirty_contexts_fenced(&NewContext, i64::MAX, false, &mut stop_before_second)
            .unwrap();
        assert!(matches!(outcome, FenceOutcome::Released));
        assert_eq!(admissions, 2, "65 chunk writes require a second fence");
        assert_eq!(checkpoints, 2, "the committed batch heartbeats before and after its write");
        assert!(virtual_now > VIRTUAL_STALE_AFTER, "the virtual refresh exceeds stale interval");
        assert_eq!(partial.chunks_updated, WORKSPACE_APPLY_BATCH_ROWS);
        assert_eq!(partial.paths_cleared, 0);
        assert_eq!(
            engine.store().load_pending_embedding_documents("code").unwrap().len(),
            WORKSPACE_APPLY_BATCH_ROWS,
            "the completed batch remains visible"
        );
        assert!(
            engine
                .context_dirty_paths("code")
                .unwrap()
                .contains(&FileKey::configuration("Owned.bsl")),
            "the path mark survives until its last chunk is committed"
        );

        let mut retry_admissions = 0;
        let mut retry = |operation: &mut dyn FnMut(
            &mut dyn FnMut() -> ControlFlow<()>,
        )
            -> ControlFlow<(), Result<(), SearchError>>| {
            retry_admissions += 1;
            let mut checkpoint = || ControlFlow::Continue(());
            match operation(&mut checkpoint) {
                ControlFlow::Break(()) => FenceOutcome::Released,
                ControlFlow::Continue(result) => FenceOutcome::Applied(result),
            }
        };
        let (completed, outcome) =
            engine.refresh_dirty_contexts_fenced(&NewContext, i64::MAX, false, &mut retry).unwrap();
        assert!(matches!(outcome, FenceOutcome::Applied(())));
        assert_eq!(retry_admissions, 1, "the retry writes only the unfinished tail and clear");
        assert_eq!(completed.chunks_updated, 1);
        assert_eq!(completed.paths_cleared, 1);
        assert!(engine.context_dirty_paths("code").unwrap().is_empty());
        assert_eq!(
            engine.store().load_pending_embedding_documents("code").unwrap().len(),
            WORKSPACE_APPLY_BATCH_ROWS + 1
        );
    }

    #[test]
    fn token_context_refresh_refusal_keeps_source_vectors_and_dirty_mark() {
        let Ok(tokenizer_path) = std::env::var("USER2_TOKENIZER_JSON") else { return };
        const TOKENIZER_SHA256: &str =
            "80d0433a2cfc55a4561b0e98b6f822decc48c9d457db498837223f9385ef3aff";
        use std::io::Write;
        use std::net::TcpListener;
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use std::sync::Arc;
        use std::time::Duration;

        struct OversizedContext(String);
        impl crate::ports::GraphContextProvider for OversizedContext {
            fn graph_context(&self, _rel: &str, _symbol: &str, _kind: &str) -> Option<String> {
                Some(self.0.clone())
            }
        }

        let token_policy =
            TokenPolicy::load(Path::new(&tokenizer_path), TOKENIZER_SHA256, 8192).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let running = Arc::new(AtomicBool::new(true));
        let requests = Arc::new(AtomicUsize::new(0));
        let server_running = Arc::clone(&running);
        let server_requests = Arc::clone(&requests);
        let server = std::thread::spawn(move || loop {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    server_requests.fetch_add(1, Ordering::SeqCst);
                    let _ = stream.write_all(
                            b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        );
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if !server_running.load(Ordering::SeqCst) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(error) => panic!("fake embedding endpoint failed: {error}"),
            }
        });

        let dir = tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let source = "Процедура Owned()\n    Возврат;\nКонецПроцедуры\n";
        std::fs::write(workspace.join("Owned.bsl"), source).unwrap();
        let config = SearchConfig {
            embedder: EmbedderConfig {
                base_url: endpoint,
                dim: Some(2),
                token_policy: Some(token_policy.clone()),
                ..EmbedderConfig::default()
            },
            execution: EmbeddingExecutionPolicy::default(),
        };
        let embedder = Embedder::new(config.embedder.clone());
        let key = FileKey::configuration("Owned.bsl");
        let mut check_singleton = |input: &str| embedder.check_singleton_input(input);
        let documents = crate::document::prepare_file_documents(
            &key,
            source,
            None,
            &token_policy,
            embedder.document_prefix(),
            &mut check_singleton,
        )
        .unwrap();
        assert!(documents.iter().any(|document| document.kind == "procedure"));
        let (chunks, contexts, spans) = crate::document::chunks_contexts_and_spans(&documents);
        let vectors = vec![vec![0.5_f32, 0.25]; chunks.len()];
        let claim = embedder.token_layout_claim().unwrap();
        let db_path = dir.path().join("bsl-search.db");
        {
            let mut store = Store::open(&db_path).unwrap();
            store
                .reindex_file_with_identity_and_source_spans(
                    CONFIGURATION_ROOT_ID,
                    "Owned.bsl",
                    blake3::hash(source.as_bytes()).as_bytes(),
                    &chunks,
                    Some(&vectors),
                    Some(&contexts),
                    embedder.model_id(),
                    2,
                    Some(claim),
                    &spans,
                )
                .unwrap();
        }
        let mut engine = SearchEngine::new(&db_path, config).unwrap();
        engine.set_workspace_root(&workspace);
        engine.store().mark_context_dirty("code", CONFIGURATION_ROOT_ID, "Owned.bsl").unwrap();
        let before_documents = engine
            .store()
            .load_indexed_documents_with_token_layout_claim(Some("code"), Some(claim))
            .unwrap();
        let before_embeddings = engine
            .store()
            .load_all_embeddings_with_profile_and_token_layout(claim, 2, Some(claim))
            .unwrap();
        let before_generation = engine.store().embedding_generation().unwrap();

        let stats = engine
            .refresh_dirty_contexts(&OversizedContext("контекст ".repeat(20_000)), i64::MAX)
            .unwrap();
        assert_eq!(stats.paths_cleared, 0);
        assert_eq!(stats.chunks_updated, 0);
        assert!(
            engine.context_dirty_paths("code").unwrap().contains(&key),
            "context preparation refusal must retain its retry obligation"
        );
        assert_eq!(
            engine
                .store()
                .load_indexed_documents_with_token_layout_claim(Some("code"), Some(claim))
                .unwrap(),
            before_documents,
            "refused context must preserve exact source text and spans"
        );
        assert_eq!(
            engine
                .store()
                .load_all_embeddings_with_profile_and_token_layout(claim, 2, Some(claim))
                .unwrap(),
            before_embeddings,
            "refused context must preserve persisted vector rows"
        );
        assert_eq!(engine.store().embedding_generation().unwrap(), before_generation);
        std::thread::sleep(Duration::from_millis(20));
        running.store(false, Ordering::SeqCst);
        server.join().unwrap();
        assert_eq!(
            requests.load(Ordering::SeqCst),
            0,
            "context refusal must not request embeddings"
        );
    }

    /// A render FAILURE (transient — the graph DB could not be read) must NOT clear the
    /// path's dirty mark: the next graph publish has to retry it. A legitimate `Ok(None)`
    /// still clears. Without keeping the mark, a one-off graph-read error would silently
    /// drop the `.xml` edit's re-render forever.
    #[test]
    fn refresh_dirty_contexts_keeps_the_mark_when_render_fails() {
        use crate::{Chunk, ChunkKind, Store};

        struct Failing;
        impl crate::ports::GraphContextProvider for Failing {
            fn graph_context(&self, _rel: &str, _sym: &str, _kind: &str) -> Option<String> {
                None
            }
            fn try_graph_context(
                &self,
                _rel: &str,
                _sym: &str,
                _kind: &str,
            ) -> Result<Option<String>, crate::GraphContextError> {
                Err(crate::GraphContextError("graph db unreadable".to_owned()))
            }
        }

        let dir = tempdir().unwrap();
        let db_path = dir.path().join("bsl-search.db");
        {
            let mut store = Store::open(&db_path).unwrap();
            store
                .reindex_file_with_context(
                    CONFIGURATION_ROOT_ID,
                    "Owned.bsl",
                    b"h1",
                    &[Chunk {
                        kind: ChunkKind::Procedure,
                        name: "П".to_owned(),
                        is_export: true,
                        annotations: vec![],
                        line_start: 0,
                        line_end: 1,
                        text: "Процедура П()\nКонецПроцедуры".to_owned(),
                    }],
                    None,
                    Some(&[Some("старый контекст".to_owned())]),
                )
                .unwrap();
        }

        let engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.store().mark_context_dirty("code", CONFIGURATION_ROOT_ID, "Owned.bsl").unwrap();

        let stats = engine.refresh_dirty_contexts(&Failing, i64::MAX).unwrap();
        assert_eq!(stats.paths_cleared, 0, "a failed render clears no path");
        assert_eq!(stats.chunks_updated, 0);
        assert_eq!(stats.cleared_embeddings, 0);
        assert!(
            engine
                .context_dirty_paths("code")
                .unwrap()
                .contains(&FileKey::configuration("Owned.bsl")),
            "the mark survives a render failure so the next publish retries it",
        );
    }

    /// A mark stamped AFTER a build captured its start-seq is not consumed by that build's
    /// publish (its seq exceeds the bound) and IS consumed by the next build (whose start-seq
    /// covers it). This is the race a stale `.xml` drift lands in: it must not be cleared
    /// against a graph that predates it. Reverting the `seq <= seq_bound` bound on the read
    /// (consuming every mark) makes the later mark vanish in the first round and this fails.
    #[test]
    fn refresh_bounded_by_start_seq_excludes_later_marks_and_consumes_them_next_round() {
        struct NoContext;
        impl crate::ports::GraphContextProvider for NoContext {
            fn graph_context(&self, _rel: &str, _sym: &str, _kind: &str) -> Option<String> {
                None
            }
        }

        let dir = tempdir().unwrap();
        let db_path = dir.path().join("bsl-search.db");
        let engine = SearchEngine::fts_only(&db_path).unwrap();

        // A build captures its start-seq AFTER the first drift marked A, but BEFORE a second
        // drift marks B (as if B's `.xml` landed while this build was already reading disk).
        engine.store().mark_context_dirty("code", CONFIGURATION_ROOT_ID, "A.bsl").unwrap();
        let build_start_seq = engine.mark_seq_handle().load(std::sync::atomic::Ordering::SeqCst);
        engine.store().mark_context_dirty("code", CONFIGURATION_ROOT_ID, "B.bsl").unwrap();
        let next_build_seq = engine.mark_seq_handle().load(std::sync::atomic::Ordering::SeqCst);
        assert!(next_build_seq > build_start_seq, "the later mark got a higher seq");

        // The build's publish consumes only A (seq <= its start-seq); B is left for a later
        // build so it is never cleared against this pre-drift graph.
        let stats = engine.refresh_dirty_contexts(&NoContext, build_start_seq).unwrap();
        assert_eq!(stats.paths_cleared, 1, "only the mark at or below the bound is consumed");
        let dirty = engine.context_dirty_paths("code").unwrap();
        assert!(
            !dirty.contains(&FileKey::configuration("A.bsl")),
            "A was within the bound and is cleared"
        );
        assert!(
            dirty.contains(&FileKey::configuration("B.bsl")),
            "B was stamped after build start and survives"
        );

        // The next build's start-seq covers B, so its publish consumes it.
        let stats = engine.refresh_dirty_contexts(&NoContext, next_build_seq).unwrap();
        assert_eq!(stats.paths_cleared, 1, "the follow-up build consumes the deferred mark");
        assert!(
            engine.context_dirty_paths("code").unwrap().is_empty(),
            "every mark is consumed once a build's start-seq covers it",
        );
    }

    /// A structural rescan reconciles the store against disk: a file deleted during a lost
    /// watch window (hub overflow / subtree removal) — absent from the freshly walked set —
    /// is removed (FTS rows dropped, live vector evicted, baseline copy hidden); a file still
    /// present is untouched. Without this, a deleted file lingers in the index forever.
    #[test]
    fn reconcile_workspace_files_removes_stored_but_gone_files() {
        use crate::embedder::EmbedderConfig;
        use crate::{Chunk, ChunkKind, Store};

        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let db_path = workspace.join("bsl-search.db");
        let chunk = |name: &str| Chunk {
            kind: ChunkKind::Procedure,
            name: name.to_owned(),
            is_export: true,
            annotations: vec![],
            line_start: 0,
            line_end: 1,
            text: format!("Процедура {name}()\nКонецПроцедуры"),
        };
        let vec_a = vec![1.0f32, 0.0, 0.0];
        let vec_b = vec![0.0f32, 1.0, 0.0];
        {
            let mut store = Store::open(&db_path).unwrap();
            store
                .reindex_file(
                    CONFIGURATION_ROOT_ID,
                    "Gone.bsl",
                    b"ha",
                    &[chunk("Ушедшая")],
                    Some(std::slice::from_ref(&vec_a)),
                )
                .unwrap();
            store
                .reindex_file(
                    CONFIGURATION_ROOT_ID,
                    "Kept.bsl",
                    b"hb",
                    &[chunk("Оставшаяся")],
                    Some(std::slice::from_ref(&vec_b)),
                )
                .unwrap();
        }

        let config = crate::SearchConfig {
            embedder: EmbedderConfig {
                base_url: "http://127.0.0.1:1".to_owned(),
                model: "test-model".to_owned(),
                dim: Some(3),
                api_key: None,
                provider: None,
                ..Default::default()
            },
            execution: crate::EmbeddingExecutionPolicy::default(),
        };
        let mut engine = SearchEngine::new(&db_path, config).unwrap();
        engine.set_workspace_root(workspace);
        assert_eq!(engine.file_count().unwrap(), 2, "both files indexed");

        // The rescan walked only the surviving file; `Gone.bsl` is absent from disk.
        let mut present = std::collections::HashSet::new();
        present.insert(workspace.join("Kept.bsl"));

        let removed = engine.reconcile_workspace_files(&present).unwrap();
        assert_eq!(removed, 1, "exactly the stored-but-gone file is reconciled out");

        assert_eq!(engine.file_count().unwrap(), 1, "only the surviving file remains");
        assert!(
            engine.text_search("Ушедшая", 10, Some("code")).unwrap().is_empty(),
            "the gone file no longer appears in FTS results",
        );
        assert!(
            !engine.text_search("Оставшаяся", 10, Some("code")).unwrap().is_empty(),
            "the surviving file is intact",
        );
        // The gone file's vector answers nothing; the survivor's still does.
        let hits = engine.search_with_embedding(&vec_a, 5, None).unwrap();
        assert!(
            hits.iter().all(|h| h.symbol_name != "Ушедшая"),
            "the reconciled file's vector is evicted from the live index: {hits:?}",
        );
    }

    /// Indexed files with their store rows, as a boot walk would have left them. Seeded in
    /// ONE call: the collection sync is a whole-collection operation, so a second call would
    /// evict what the first one wrote.
    fn seed_rows(engine: &mut SearchEngine, paths: &[&str]) {
        let documents: Vec<IndexedDocument> = paths
            .iter()
            .map(|path| IndexedDocument {
                collection: "code".to_owned(),
                root_id: crate::CONFIGURATION_ROOT_ID.to_owned(),
                path: (*path).to_owned(),
                symbol_name: "П".to_owned(),
                kind: "procedure".to_owned(),
                line_start: 0,
                line_end: 1,
                text: "Процедура П()\nКонецПроцедуры".to_owned(),
                content_hash: "h".to_owned(),
                graph_context: None,
                source_span: None,
            })
            .collect();
        engine.sync_indexed_documents_in_collection("code", &documents, None).unwrap();
    }

    /// Retracting the fingerprint row used to be best effort: its failure was logged and the
    /// removal reported success, leaving a row that claims the file was verified — enough for
    /// a namesake at the same size and mtime to inherit the claim across a restart.
    #[test]
    fn a_denied_fingerprint_retraction_fails_the_removal() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let db_path = workspace.join("bsl-search.db");
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.set_workspace_root(workspace);
        seed_rows(&mut engine, &["Removed.bsl"]);
        engine
            .store()
            .save_overlay_fingerprint_cache(
                "",
                &HashMap::from([(
                    FileKey::configuration("Removed.bsl"),
                    crate::store::PersistedFingerprint {
                        file_size: 7,
                        file_mtime_secs: 1,
                        file_mtime_nanos: 0,
                        content_fingerprint: "fp".to_owned(),
                        canonical: workspace.join("Removed.bsl").display().to_string(),
                    },
                )]),
            )
            .unwrap();

        let saboteur = rusqlite::Connection::open(&db_path).unwrap();
        saboteur
            .execute_batch(
                "CREATE TRIGGER deny_fp_delete BEFORE DELETE ON overlay_fingerprint_cache \
                 BEGIN SELECT RAISE(FAIL, 'deny'); END;",
            )
            .unwrap();

        assert!(
            engine.reconcile_workspace_files(&HashSet::new()).is_err(),
            "a carrier left populated is not a successful removal",
        );
        assert_eq!(engine.file_count().unwrap(), 1, "the row survives as evidence for a retry");
    }

    /// Evicting the dead file's vectors is the one step whose retry dies with the store row:
    /// the chunk ids come from that row, and the dirty mark only ever reaches the overlay.
    /// The seam fires BEFORE the eviction, so the test states what a real failure leaves —
    /// the vector still answering — instead of asserting it after the vectors are already out.
    #[test]
    fn a_failed_vector_eviction_fails_the_removal_and_keeps_the_row() {
        use crate::embedder::EmbedderConfig;
        use crate::{Chunk, ChunkKind, Store};

        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let db_path = workspace.join("bsl-search.db");
        let vector = vec![1.0f32, 0.0, 0.0];
        {
            let mut store = Store::open(&db_path).unwrap();
            store
                .reindex_file(
                    CONFIGURATION_ROOT_ID,
                    "Removed.bsl",
                    b"h",
                    &[Chunk {
                        kind: ChunkKind::Procedure,
                        name: "Ушедшая".to_owned(),
                        is_export: true,
                        annotations: vec![],
                        line_start: 0,
                        line_end: 1,
                        text: "Процедура Ушедшая()\nКонецПроцедуры".to_owned(),
                    }],
                    Some(std::slice::from_ref(&vector)),
                )
                .unwrap();
        }
        let config = crate::SearchConfig {
            embedder: EmbedderConfig {
                base_url: "http://127.0.0.1:1".to_owned(),
                model: "test-model".to_owned(),
                dim: Some(3),
                api_key: None,
                provider: None,
                ..Default::default()
            },
            execution: crate::EmbeddingExecutionPolicy::default(),
        };
        let mut engine = SearchEngine::new(&db_path, config).unwrap();
        engine.set_workspace_root(workspace);

        FORCE_VECTOR_REMOVE_ERROR.with(|flag| flag.set(true));
        let outcome = engine.reconcile_workspace_files(&HashSet::new());
        FORCE_VECTOR_REMOVE_ERROR.with(|flag| flag.set(false));

        assert!(outcome.is_err(), "a vector left in the live index is not a successful removal");
        assert_eq!(engine.file_count().unwrap(), 1, "the row survives as evidence for a retry");
        assert!(
            engine
                .search_with_embedding(&vector, 5, None)
                .unwrap()
                .iter()
                .any(|hit| hit.symbol_name == "Ушедшая"),
            "the failure left the vector in the live index — which is what makes it a failure",
        );
        assert_eq!(
            engine.reconcile_workspace_files(&HashSet::new()).unwrap(),
            1,
            "the retry finds the key exactly where the failed pass left it",
        );
        assert!(
            engine
                .search_with_embedding(&vector, 5, None)
                .unwrap()
                .iter()
                .all(|hit| hit.symbol_name != "Ушедшая"),
            "and the successful retry evicts it",
        );
    }

    /// A carrier that could not be READ leaves its keys out of the sweep entirely, so the
    /// reading must fail loudly: reporting a clean reconcile after skipping a carrier is worse
    /// than reporting nothing.
    #[test]
    fn an_unreadable_overlay_fails_the_reconcile_instead_of_sweeping_without_it() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let file = workspace.join("AfterBoot.bsl");
        fs::write(&file, "Процедура ПослеСтарта()\nКонецПроцедуры").unwrap();
        let db_path = workspace.join("bsl-search.db");
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.set_workspace_root(workspace);
        engine.initialize_workspace_overlay_clean().unwrap();
        assert!(engine.mark_workspace_path_dirty(&file).unwrap());
        engine.refresh_workspace_overlay_snapshot(false).unwrap();

        let hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = engine.workspace_overlay_cache.lock().unwrap();
            panic!("poison the overlay lock");
        }));
        std::panic::set_hook(hook);

        fs::remove_file(&file).unwrap();
        assert!(
            engine.reconcile_workspace_files(&HashSet::new()).is_err(),
            "a sweep that could not read a carrier is not a successful sweep",
        );
    }

    /// The manifest is a carrier only where it is served, so a local reconcile must not
    /// depend on it: an inherited header left broken on disk would otherwise block the sweep
    /// of the carriers this mode does have.
    #[test]
    fn a_local_reconcile_ignores_an_unreadable_inactive_manifest() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let db_path = workspace.join("bsl-search.db");
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.set_workspace_root(workspace);
        seed_rows(&mut engine, &["Gone.bsl"]);

        let saboteur = rusqlite::Connection::open(&db_path).unwrap();
        saboteur.execute_batch("DROP TABLE baseline_manifest").unwrap();

        assert_eq!(
            engine.reconcile_workspace_files(&HashSet::new()).unwrap(),
            1,
            "the local carriers are swept whatever state an unserved manifest is in",
        );
    }

    /// A manifest header this engine cannot read is not an absent header. Reading the
    /// fingerprint rows under the empty id that an absent header yields would treat every
    /// existing row as another snapshot's and CLEAR the cache — the reconcile would destroy
    /// the evidence a retry needs while reporting a failure.
    #[test]
    fn an_unreadable_manifest_header_leaves_the_fingerprint_rows_alone() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let db_path = workspace.join("bsl-search.db");
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.set_workspace_root(workspace);
        engine
            .store()
            .save_baseline_manifest(&crate::WorkspaceBaselineManifest {
                snapshot_id: "snap".to_owned(),
                snapshot_fingerprint: Some("fp".to_owned()),
                files: vec![crate::BaselineManifestFile {
                    root_id: crate::CONFIGURATION_ROOT_ID.to_owned(),
                    collection: "code".to_owned(),
                    path: "A.bsl".to_owned(),
                    file_fingerprint: "fp-file".to_owned(),
                    document_count: 1,
                    file_object_id: "obj-1".to_owned(),
                }],
            })
            .unwrap();
        engine
            .store()
            .save_overlay_fingerprint_cache(
                "snap",
                &HashMap::from([(
                    FileKey::configuration("A.bsl"),
                    crate::store::PersistedFingerprint {
                        file_size: 7,
                        file_mtime_secs: 1,
                        file_mtime_nanos: 0,
                        content_fingerprint: "fp".to_owned(),
                        canonical: workspace.join("A.bsl").display().to_string(),
                    },
                )]),
            )
            .unwrap();
        engine.set_serves_external_baseline(true).unwrap();

        let saboteur = rusqlite::Connection::open(&db_path).unwrap();
        saboteur.execute_batch("DROP TABLE baseline_manifest").unwrap();

        assert!(engine.reconcile_workspace_files(&HashSet::new()).is_err(), "the failure is told");
        let surviving: i64 = saboteur
            .query_row("SELECT COUNT(*) FROM overlay_fingerprint_cache", [], |row| row.get(0))
            .unwrap();
        assert_eq!(surviving, 1, "the fingerprint row a retry needs was not swept away");
    }

    /// A manifest that cannot be read is not evidence that there is no baseline copy. Taken
    /// as one, the removal would skip the hiding and the copy would keep being served — while
    /// the caller was told the file is gone.
    #[test]
    fn an_unreadable_manifest_fails_the_removal() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let file = workspace.join("Removed.bsl");
        fs::write(&file, "Процедура П()\nКонецПроцедуры").unwrap();
        let db_path = workspace.join("bsl-search.db");
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.set_workspace_root(workspace);
        seed_rows(&mut engine, &["Removed.bsl"]);
        engine
            .store()
            .save_baseline_manifest(&crate::WorkspaceBaselineManifest {
                snapshot_id: "snap".to_owned(),
                snapshot_fingerprint: Some("fp".to_owned()),
                files: vec![crate::BaselineManifestFile {
                    root_id: crate::CONFIGURATION_ROOT_ID.to_owned(),
                    collection: "code".to_owned(),
                    path: "Removed.bsl".to_owned(),
                    file_fingerprint: "fp-file".to_owned(),
                    document_count: 1,
                    file_object_id: "obj-1".to_owned(),
                }],
            })
            .unwrap();
        engine.set_serves_external_baseline(true).unwrap();

        let saboteur = rusqlite::Connection::open(&db_path).unwrap();
        saboteur.execute_batch("DROP TABLE baseline_manifest_files").unwrap();

        fs::remove_file(&file).unwrap();
        assert!(
            engine.remove_workspace_path(&file).is_err(),
            "a removal that could not weigh the baseline is not a success",
        );
    }

    /// A reconcile is a batch: one key it cannot remove must not cost the others their pass.
    /// The failure is still reported — it just no longer strands the tail.
    #[test]
    fn a_reconcile_batch_outlives_a_failing_key() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let db_path = workspace.join("bsl-search.db");
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.set_workspace_root(workspace);
        seed_rows(&mut engine, &["AFailing.bsl", "BHealthy.bsl"]);
        // The fallible store step before the row goes needs a row to fail on: both keys get a
        // fingerprint entry, so the denial below lands on a step the removal really takes.
        engine
            .store()
            .save_overlay_fingerprint_cache(
                "snap",
                &HashMap::from([
                    (
                        FileKey::configuration("AFailing.bsl"),
                        crate::store::PersistedFingerprint {
                            file_size: 1,
                            file_mtime_secs: 1,
                            file_mtime_nanos: 0,
                            content_fingerprint: "fp".to_owned(),
                            canonical: "AFailing.bsl".to_owned(),
                        },
                    ),
                    (
                        FileKey::configuration("BHealthy.bsl"),
                        crate::store::PersistedFingerprint {
                            file_size: 1,
                            file_mtime_secs: 1,
                            file_mtime_nanos: 0,
                            content_fingerprint: "fp".to_owned(),
                            canonical: "BHealthy.bsl".to_owned(),
                        },
                    ),
                ]),
            )
            .unwrap();

        // Denied for exactly one key, so the batch has both a failing and a healthy member.
        let saboteur = rusqlite::Connection::open(&db_path).unwrap();
        saboteur
            .execute_batch(
                "CREATE TRIGGER deny_one_fingerprint BEFORE DELETE ON overlay_fingerprint_cache \
                 WHEN OLD.path = 'AFailing.bsl' BEGIN SELECT RAISE(FAIL, 'deny'); END;",
            )
            .unwrap();

        assert!(engine.reconcile_workspace_files(&HashSet::new()).is_err(), "the failure is told");
        assert_eq!(
            engine.file_count().unwrap(),
            1,
            "the healthy key was removed despite the earlier failure",
        );
        assert!(
            engine
                .store()
                .all_files_in_collection("code")
                .unwrap()
                .iter()
                .any(|(key, _)| key.path == "AFailing.bsl"),
            "and the failing one is the one left behind",
        );
    }

    /// A file indexed AFTER boot lives in the overlay alone — the store rows stop growing
    /// once the daemon is up — so a reconcile that walks the rows cannot see it, and its
    /// entry keeps serving a file that is gone from disk.
    #[test]
    fn a_reconcile_removes_a_key_known_only_to_the_overlay() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let file = workspace.join("AfterBoot.bsl");
        fs::write(&file, "Процедура ПослеСтарта()\nКонецПроцедуры").unwrap();
        let db_path = workspace.join("bsl-search.db");
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.set_workspace_root(workspace);
        engine.initialize_workspace_overlay_clean().unwrap();
        assert!(engine.mark_workspace_path_dirty(&file).unwrap());
        engine.refresh_workspace_overlay_snapshot(false).unwrap();
        let key = FileKey::configuration("AfterBoot.bsl");
        assert_eq!(engine.file_count().unwrap(), 0, "no boot walk ever wrote a row");
        assert!(
            engine.carrier_keys().unwrap().carriers_of(&key).contains(&KeyCarrier::OverlayEntry),
            "the overlay is the only carrier that knows this file",
        );

        fs::remove_file(&file).unwrap();
        let removed = engine.reconcile_workspace_files(&HashSet::new()).unwrap();

        assert_eq!(removed, 1, "the overlay-only key is reconciled out");
        assert!(
            engine.carrier_keys().unwrap().carriers_of(&key).is_empty(),
            "no carrier still knows it",
        );
    }

    /// The fingerprint row outlives its entry: an entry that matched the baseline is dropped
    /// while its row stays behind asserting the file was verified. Left alone, that row lets
    /// a namesake recreated at the same size and mtime inherit the claim across a restart.
    #[test]
    fn a_reconcile_removes_a_key_known_only_to_the_fingerprint_row() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let db_path = workspace.join("bsl-search.db");
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.set_workspace_root(workspace);
        let key = FileKey::configuration("OnlyFingerprint.bsl");
        engine
            .store()
            .save_overlay_fingerprint_cache(
                "",
                &HashMap::from([(
                    key.clone(),
                    crate::store::PersistedFingerprint {
                        file_size: 7,
                        file_mtime_secs: 1,
                        file_mtime_nanos: 0,
                        content_fingerprint: "fp".to_owned(),
                        canonical: workspace.join("OnlyFingerprint.bsl").display().to_string(),
                    },
                )]),
            )
            .unwrap();
        assert_eq!(
            engine.carrier_keys().unwrap().carriers_of(&key),
            vec![KeyCarrier::FingerprintRow],
            "the fingerprint row is the only carrier",
        );

        let removed = engine.reconcile_workspace_files(&HashSet::new()).unwrap();

        assert_eq!(removed, 1, "the fingerprint-only key is reconciled out");
        assert!(
            engine.carrier_keys().unwrap().carriers_of(&key).is_empty(),
            "no carrier still knows it",
        );
    }

    /// Against a remote baseline the local rows are cleared on boot, so the manifest is the
    /// only carrier there is. A reconcile blind to it removes nothing at all, and a file
    /// deleted locally keeps arriving from the baseline.
    #[test]
    fn a_reconcile_hides_a_key_known_only_to_the_served_manifest() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let db_path = workspace.join("bsl-search.db");
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.set_workspace_root(workspace);
        engine
            .store()
            .save_baseline_manifest(&crate::WorkspaceBaselineManifest {
                snapshot_id: "snap".to_owned(),
                snapshot_fingerprint: Some("fp".to_owned()),
                files: vec![crate::BaselineManifestFile {
                    root_id: crate::CONFIGURATION_ROOT_ID.to_owned(),
                    collection: "code".to_owned(),
                    path: "Deleted.bsl".to_owned(),
                    file_fingerprint: "fp-file".to_owned(),
                    document_count: 1,
                    file_object_id: "obj-1".to_owned(),
                }],
            })
            .unwrap();
        engine.set_serves_external_baseline(true).unwrap();
        engine.initialize_workspace_overlay_clean().unwrap();
        let key = FileKey::configuration("Deleted.bsl");

        let removed = engine.reconcile_workspace_files(&HashSet::new()).unwrap();

        assert_eq!(removed, 1, "the manifest-only key is reconciled out");
        assert!(
            engine.workspace_overlay_cache.lock().unwrap().hidden_keys().contains(&key),
            "removing a manifest-only key is expressed by hiding its baseline copy",
        );
    }

    /// A vanished directory takes its files with it, whichever carrier happens to know
    /// them — and takes nothing that merely starts with the same text. `Dir2` is not
    /// inside `Dir`, so a removal comparing paths as text instead of components would
    /// erase a directory nobody touched.
    #[test]
    fn removing_a_subtree_clears_its_carriers_and_spares_a_namesake_directory() {
        use crate::{Chunk, ChunkKind, Store};

        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let db_path = workspace.join("bsl-search.db");
        fs::create_dir(workspace.join("Dir")).unwrap();
        fs::create_dir(workspace.join("Dir2")).unwrap();
        let chunk = |name: &str| Chunk {
            kind: ChunkKind::Procedure,
            name: name.to_owned(),
            is_export: true,
            annotations: vec![],
            line_start: 0,
            line_end: 1,
            text: format!("Процедура {name}()\nКонецПроцедуры"),
        };
        {
            let mut store = Store::open(&db_path).unwrap();
            store
                .reindex_file(CONFIGURATION_ROOT_ID, "Dir/Row.bsl", b"ha", &[chunk("Строка")], None)
                .unwrap();
            store
                .reindex_file(
                    CONFIGURATION_ROOT_ID,
                    "Dir2/Keep.bsl",
                    b"hb",
                    &[chunk("Соседка")],
                    None,
                )
                .unwrap();
        }
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.set_workspace_root(workspace);
        engine.initialize_workspace_overlay_clean().unwrap();

        // One key per carrier, so a removal that walks a single carrier cannot pass.
        let live = workspace.join("Dir/Live.bsl");
        fs::write(&live, "Процедура Живая()\nКонецПроцедуры").unwrap();
        assert!(engine.mark_workspace_path_dirty(&live).unwrap());
        engine.refresh_workspace_overlay_snapshot(false).unwrap();
        engine
            .store()
            .save_overlay_fingerprint_cache(
                "",
                &HashMap::from([(
                    FileKey::configuration("Dir/Print.bsl"),
                    crate::store::PersistedFingerprint {
                        file_size: 7,
                        file_mtime_secs: 1,
                        file_mtime_nanos: 0,
                        content_fingerprint: "fp".to_owned(),
                        canonical: workspace.join("Dir/Print.bsl").display().to_string(),
                    },
                )]),
            )
            .unwrap();

        let row = FileKey::configuration("Dir/Row.bsl");
        let overlay = FileKey::configuration("Dir/Live.bsl");
        let fingerprint = FileKey::configuration("Dir/Print.bsl");
        let beside = FileKey::configuration("Dir2/Keep.bsl");
        let carriers = engine.carrier_keys().unwrap();
        assert!(carriers.carriers_of(&row).contains(&KeyCarrier::StoreRow));
        assert!(carriers.carriers_of(&overlay).contains(&KeyCarrier::OverlayEntry));
        assert!(carriers.carriers_of(&fingerprint).contains(&KeyCarrier::FingerprintRow));

        // The directory is what vanished; the removal answers per key, so its one file that
        // existed has to go with it.
        fs::remove_dir_all(workspace.join("Dir")).unwrap();
        let removed = engine.remove_vanished_under(&[workspace.join("Dir")]).unwrap();

        assert_eq!(removed, 3, "every carrier under the directory gave up its key");
        let carriers = engine.carrier_keys().unwrap();
        for key in [&row, &overlay, &fingerprint] {
            assert!(carriers.carriers_of(key).is_empty(), "{key:?} is still known to a carrier");
        }
        assert!(!carriers.carriers_of(&beside).is_empty(), "the namesake directory is untouched");
        assert!(
            !engine.text_search("Соседка", 10, Some("code")).unwrap().is_empty(),
            "and its file still answers searches",
        );
    }

    /// A root can vanish as a whole — an extension directory deleted, a configuration that
    /// IS the workspace. Attribution answers "which root owns this file", and a root owns no
    /// file at its own path, so asking it about the root itself yields nothing: the removal
    /// has to take the root's own keys, not just the keys under some enclosing root.
    #[test]
    fn removing_a_registered_root_clears_the_files_it_held() {
        use crate::{Chunk, ChunkKind, Store};

        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let configuration = workspace.join("src/cf");
        let extension = workspace.join("ext");
        fs::create_dir_all(&configuration).unwrap();
        fs::create_dir_all(&extension).unwrap();
        let db_path = workspace.join("bsl-search.db");

        let (roots, rejected) = crate::WorkspaceRoots::build(
            workspace,
            &configuration,
            std::slice::from_ref(&extension),
        );
        assert!(rejected.is_empty(), "both roots are registered");
        let extension_id = roots
            .entries()
            .find(|(_, declared)| *declared == extension)
            .map(|(id, _)| id.to_owned())
            .expect("the extension root is registered");
        {
            let mut store = Store::open(&db_path).unwrap();
            let mut index = |root_id: &str, path: &str, name: &str| {
                store
                    .reindex_file(
                        root_id,
                        path,
                        b"h",
                        &[Chunk {
                            kind: ChunkKind::Procedure,
                            name: name.to_owned(),
                            is_export: true,
                            annotations: vec![],
                            line_start: 0,
                            line_end: 1,
                            text: format!("Процедура {name}()\nКонецПроцедуры"),
                        }],
                        None,
                    )
                    .unwrap();
            };
            index(&extension_id, "A.bsl", "ИзРасширения");
            index(CONFIGURATION_ROOT_ID, "B.bsl", "ИзКонфигурации");
        }
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.set_workspace_roots(roots);

        fs::remove_dir_all(&extension).unwrap();
        let removed = engine.remove_vanished_under(std::slice::from_ref(&extension)).unwrap();

        assert_eq!(removed, 1, "the vanished root gave up its file");
        assert!(
            engine.text_search("ИзРасширения", 10, Some("code")).unwrap().is_empty(),
            "a root that vanished stops answering searches",
        );
        assert!(
            !engine.text_search("ИзКонфигурации", 10, Some("code")).unwrap().is_empty(),
            "the other root is untouched",
        );
    }

    /// A file proven present but unreadable leaves only an obligation to re-read it. If the
    /// directory it lived in is gone, that obligation is for a file that no longer exists —
    /// and nothing else records the key at all.
    #[cfg(unix)]
    #[test]
    fn removing_a_subtree_clears_an_obligation_to_re_read_a_file_under_it() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        let workspace = dir.path();
        fs::create_dir(workspace.join("Dir")).unwrap();
        let unread = workspace.join("Dir/Unread.bsl");
        fs::write(&unread, "Процедура Непрочтённая()\nКонецПроцедуры").unwrap();

        let mut engine = SearchEngine::fts_only(&workspace.join("bsl-search.db")).unwrap();
        engine.set_workspace_root(workspace);
        engine.initialize_workspace_overlay_clean().unwrap();
        assert!(engine.mark_workspace_path_dirty(&unread).unwrap());
        fs::set_permissions(&unread, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read(&unread).is_ok() {
            // Running as root: permissions cannot make the file unreadable.
            fs::set_permissions(&unread, fs::Permissions::from_mode(0o644)).unwrap();
            return;
        }
        engine.refresh_workspace_overlay_snapshot(false).unwrap();
        fs::set_permissions(&unread, fs::Permissions::from_mode(0o644)).unwrap();

        let key = FileKey::configuration("Dir/Unread.bsl");
        assert_eq!(
            engine.carrier_keys().unwrap().carriers_of(&key),
            vec![KeyCarrier::UnreadObligation],
            "the standing obligation is the only carrier",
        );

        // The obligation outlives the file: the directory goes, and with it any hope of
        // ever re-reading what was owed.
        fs::remove_dir_all(workspace.join("Dir")).unwrap();
        let removed = engine.remove_vanished_under(&[workspace.join("Dir")]).unwrap();

        assert_eq!(removed, 1, "the obligation-only key is removed with its directory");
        assert!(
            engine.carrier_keys().unwrap().carriers_of(&key).is_empty(),
            "no carrier still knows it",
        );
    }

    /// Against a remote baseline the manifest is the only carrier there is, and a removal
    /// there is expressed by hiding rather than by deleting someone else's row.
    #[test]
    fn removing_a_subtree_hides_the_baseline_copies_under_it() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let mut engine = SearchEngine::fts_only(&workspace.join("bsl-search.db")).unwrap();
        engine.set_workspace_root(workspace);
        let baseline_file = |path: &str, id: &str| crate::BaselineManifestFile {
            root_id: crate::CONFIGURATION_ROOT_ID.to_owned(),
            collection: "code".to_owned(),
            path: path.to_owned(),
            file_fingerprint: "fp-file".to_owned(),
            document_count: 1,
            file_object_id: id.to_owned(),
        };
        engine
            .store()
            .save_baseline_manifest(&crate::WorkspaceBaselineManifest {
                snapshot_id: "snap".to_owned(),
                snapshot_fingerprint: Some("fp".to_owned()),
                files: vec![
                    baseline_file("Dir/Deleted.bsl", "obj-1"),
                    baseline_file("Dir2/Kept.bsl", "obj-2"),
                ],
            })
            .unwrap();
        engine.set_serves_external_baseline(true).unwrap();
        engine.initialize_workspace_overlay_clean().unwrap();

        let removed = engine.remove_vanished_under(&[workspace.join("Dir")]).unwrap();

        assert_eq!(removed, 1, "only the baseline copy under the directory is removed");
        let hidden = engine.workspace_overlay_cache.lock().unwrap().hidden_keys();
        assert!(
            hidden.contains(&FileKey::configuration("Dir/Deleted.bsl")),
            "the baseline copy under the gone directory stops being served",
        );
        assert!(
            !hidden.contains(&FileKey::configuration("Dir2/Kept.bsl")),
            "the namesake directory's baseline copy is untouched",
        );
    }

    /// The manifest deliberately survives a mode switch, so its rows prove nothing to an
    /// engine that does not serve it. Reading them anyway would make a local engine remove
    /// ghosts of another mode's corpus.
    #[test]
    fn an_inherited_manifest_yields_no_candidates_in_the_local_mode() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let db_path = workspace.join("bsl-search.db");
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.set_workspace_root(workspace);
        engine
            .store()
            .save_baseline_manifest(&crate::WorkspaceBaselineManifest {
                snapshot_id: "stale-snap".to_owned(),
                snapshot_fingerprint: Some("fp".to_owned()),
                files: vec![crate::BaselineManifestFile {
                    root_id: crate::CONFIGURATION_ROOT_ID.to_owned(),
                    collection: "code".to_owned(),
                    path: "Ghost.bsl".to_owned(),
                    file_fingerprint: "fp-file".to_owned(),
                    document_count: 1,
                    file_object_id: "obj-1".to_owned(),
                }],
            })
            .unwrap();
        engine.initialize_workspace_overlay_clean().unwrap();

        assert_eq!(
            engine.reconcile_workspace_files(&HashSet::new()).unwrap(),
            0,
            "an inherited manifest is not evidence in the local mode",
        );
        // The positive control: the very same rows DO yield a candidate once served, so the
        // assertion above cannot be satisfied by never reading the manifest at all.
        engine.set_serves_external_baseline(true).unwrap();
        assert_eq!(
            engine.reconcile_workspace_files(&HashSet::new()).unwrap(),
            1,
            "served, the same manifest yields its key",
        );
    }

    /// Removing a manifest-only key cannot delete its row — the manifest is a snapshot of
    /// someone else's corpus — so the key survives its own removal. Re-selecting it every
    /// time would grow the removal count and the retry obligations without a single change
    /// to what search serves.
    #[test]
    fn a_second_reconcile_over_an_unchanged_tree_removes_nothing() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let db_path = workspace.join("bsl-search.db");
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.set_workspace_root(workspace);
        engine
            .store()
            .save_baseline_manifest(&crate::WorkspaceBaselineManifest {
                snapshot_id: "snap".to_owned(),
                snapshot_fingerprint: Some("fp".to_owned()),
                files: vec![crate::BaselineManifestFile {
                    root_id: crate::CONFIGURATION_ROOT_ID.to_owned(),
                    collection: "code".to_owned(),
                    path: "Deleted.bsl".to_owned(),
                    file_fingerprint: "fp-file".to_owned(),
                    document_count: 1,
                    file_object_id: "obj-1".to_owned(),
                }],
            })
            .unwrap();
        engine.set_serves_external_baseline(true).unwrap();
        engine.initialize_workspace_overlay_clean().unwrap();

        assert_eq!(engine.reconcile_workspace_files(&HashSet::new()).unwrap(), 1);
        assert_eq!(
            engine.reconcile_workspace_files(&HashSet::new()).unwrap(),
            0,
            "the second pass has nothing left to do",
        );
    }

    /// Hiding proves a file is absent from disk, NOT that its carriers were cleared: a clean
    /// full pass hides a baseline key it did not see without touching the store row. Treating
    /// a hidden key as already settled would leave that row, its chunks and its vectors alive
    /// for good.
    #[test]
    fn a_hidden_key_whose_row_survives_is_still_a_candidate() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let file = workspace.join("Hidden.bsl");
        fs::write(&file, "Процедура Спрятанная()\nКонецПроцедуры").unwrap();
        let db_path = workspace.join("bsl-search.db");
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.index_directory_fts(workspace).unwrap();
        engine.set_workspace_root(workspace);
        let key = FileKey::configuration("Hidden.bsl");
        assert_eq!(engine.file_count().unwrap(), 1, "the boot walk wrote its row");

        // A clean full pass over a tree the file has left: it hides the baseline key and
        // leaves the row exactly as it was.
        fs::remove_file(&file).unwrap();
        engine.refresh_workspace_overlay_snapshot(true).unwrap();
        assert!(
            engine.workspace_overlay_cache.lock().unwrap().hidden_keys().contains(&key),
            "the clean pass hid the vanished baseline key",
        );
        assert_eq!(engine.file_count().unwrap(), 1, "the row survived the pass untouched");

        assert_eq!(
            engine.reconcile_workspace_files(&HashSet::new()).unwrap(),
            1,
            "hiding does not excuse the row from reconciliation",
        );
        assert_eq!(engine.file_count().unwrap(), 0, "the row is gone");
    }

    /// The reconcile grew its candidate set, so what it must NOT do grew with it: a file
    /// present on disk stays, however many carriers know about it.
    #[test]
    fn a_reconcile_keeps_an_overlay_only_file_that_is_still_on_disk() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let file = workspace.join("Alive.bsl");
        fs::write(&file, "Процедура Живая()\nКонецПроцедуры").unwrap();
        let db_path = workspace.join("bsl-search.db");
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.set_workspace_root(workspace);
        engine.initialize_workspace_overlay_clean().unwrap();
        assert!(engine.mark_workspace_path_dirty(&file).unwrap());
        engine.refresh_workspace_overlay_snapshot(false).unwrap();
        let key = FileKey::configuration("Alive.bsl");

        let present = HashSet::from([file.clone()]);
        assert_eq!(
            engine.reconcile_workspace_files(&present).unwrap(),
            0,
            "a file the walk found is never removed",
        );
        assert!(
            !engine.carrier_keys().unwrap().carriers_of(&key).is_empty(),
            "its overlay entry is intact",
        );
    }

    /// A workspace removal HIDES a baseline (Postgres-mode) copy of the same path so it
    /// cannot resurrect the locally-deleted file. The hiding lives in the overlay — the
    /// negative carrier the store used to keep beside it was written and never read
    /// (github#233) — so the promise is pinned where it is actually delivered.
    #[test]
    fn remove_workspace_path_hides_a_baseline_hit_so_it_cannot_resurrect() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let db_path = workspace.join("bsl-search.db");
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.set_workspace_root(workspace);
        // A baseline copy is what makes the hiding mean anything: a key outside the manifest
        // has nothing to hide unless a local entry of its own is being replaced.
        engine.set_serves_external_baseline(true).unwrap();
        engine
            .store()
            .save_baseline_manifest(&crate::WorkspaceBaselineManifest {
                snapshot_id: "snap".to_owned(),
                snapshot_fingerprint: None,
                files: vec![crate::BaselineManifestFile {
                    root_id: crate::CONFIGURATION_ROOT_ID.to_owned(),
                    collection: "code".to_owned(),
                    path: "Removed.bsl".to_owned(),
                    file_fingerprint: "fp-file".to_owned(),
                    document_count: 1,
                    file_object_id: "obj-1".to_owned(),
                }],
            })
            .unwrap();
        engine
            .sync_indexed_documents_in_collection(
                "code",
                &[IndexedDocument {
                    collection: "code".to_owned(),
                    root_id: crate::CONFIGURATION_ROOT_ID.to_owned(),
                    path: "Removed.bsl".to_owned(),
                    symbol_name: "П".to_owned(),
                    kind: "procedure".to_owned(),
                    line_start: 0,
                    line_end: 1,
                    text: "Процедура П()\nКонецПроцедуры".to_owned(),
                    content_hash: "h".to_owned(),
                    graph_context: None,
                    source_span: None,
                }],
                None,
            )
            .unwrap();

        assert!(engine.remove_workspace_path(workspace.join("Removed.bsl")).unwrap());

        let stats = engine.workspace_overlay_stats().unwrap().unwrap();
        assert_eq!(
            stats.hidden_paths, 1,
            "the deleted path's baseline copy stays hidden so it cannot resurrect",
        );
        assert_eq!(
            stats.deleted_files, 1,
            "and the hiding is what carries the deletion, since no local entry is left",
        );
    }

    /// Two roots whose declared nesting is the reverse of their canonical one: an
    /// outer root reached through an alias, and an inner root registered under the
    /// alias's real path. A file deleted there cannot be canonicalized, and ranking
    /// the roots by their declared spellings alone would pick the outer one — so the
    /// removal would drop a key nobody ever wrote and leave the real row serving
    /// a dead hit.
    #[cfg(unix)]
    #[test]
    fn a_deletion_reached_through_an_alias_removes_the_row_the_file_lived_under() {
        let dir = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let workspace = dir.path();
        let configuration = workspace.join("cf");
        fs::create_dir_all(&configuration).unwrap();
        let outer = outside.path().join("outer");
        let inner = outer.join("inner");
        fs::create_dir_all(&inner).unwrap();
        let alias = workspace.join("alias");
        std::os::unix::fs::symlink(&outer, &alias).unwrap();

        let file = inner.join("X.bsl");
        fs::write(&file, "Процедура П()\nКонецПроцедуры").unwrap();

        let mut engine = SearchEngine::fts_only(&workspace.join("bsl-search.db")).unwrap();
        let (roots, rejected) = crate::WorkspaceRoots::build(
            workspace,
            &configuration,
            &[alias.clone(), inner.clone()],
        );
        assert!(rejected.is_empty(), "both roots register: {rejected:?}");
        engine.set_workspace_roots(roots);

        // The file is stored under the root it physically lives in, which is what
        // the walk would have attributed it to.
        let lived_under =
            engine.workspace_file_key(&file).expect("a live file attributes to its own root");
        engine.store().upsert_file(&lived_under.root_id, &lived_under.path, b"h", "code").unwrap();
        assert_eq!(engine.file_count().unwrap(), 1);

        fs::remove_file(&file).unwrap();
        assert!(engine.remove_workspace_path(alias.join("inner/X.bsl")).unwrap());

        assert_eq!(
            engine.file_count().unwrap(),
            0,
            "the row of the root the file lived under is the one removed",
        );
    }

    /// The same argument one level up: a DIRECTORY removed through an alias must reach the
    /// keys of the root its files physically lived under. Declared spellings alone hand the
    /// subtree to the alias root, whose keys are not the ones in the store.
    #[cfg(unix)]
    #[test]
    fn a_subtree_removed_through_an_alias_clears_the_root_it_lived_under() {
        let dir = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let workspace = dir.path();
        let configuration = workspace.join("cf");
        fs::create_dir_all(&configuration).unwrap();
        let outer = outside.path().join("outer");
        let inner = outer.join("inner");
        let gone = inner.join("Gone");
        fs::create_dir_all(&gone).unwrap();
        let alias = workspace.join("alias");
        std::os::unix::fs::symlink(&outer, &alias).unwrap();

        let file = gone.join("A.bsl");
        fs::write(&file, "Процедура ЧерезАлиас()\nКонецПроцедуры").unwrap();

        let mut engine = SearchEngine::fts_only(&workspace.join("bsl-search.db")).unwrap();
        let (roots, rejected) = crate::WorkspaceRoots::build(
            workspace,
            &configuration,
            &[alias.clone(), inner.clone()],
        );
        assert!(rejected.is_empty(), "both roots register: {rejected:?}");
        engine.set_workspace_roots(roots);

        let lived_under =
            engine.workspace_file_key(&file).expect("a live file attributes to its own root");
        engine.store().upsert_file(&lived_under.root_id, &lived_under.path, b"h", "code").unwrap();
        assert_eq!(engine.file_count().unwrap(), 1);

        fs::remove_dir_all(&gone).unwrap();
        let removed = engine.remove_vanished_under(&[alias.join("inner/Gone")]).unwrap();

        assert_eq!(removed, 1, "the subtree is found through the alias");
        assert_eq!(
            engine.file_count().unwrap(),
            0,
            "the row of the root the files lived under is the one cleared",
        );
    }

    /// A workspace removal marks the path dirty in the in-memory overlay cache, so a
    /// cached overlay entry for the deleted file stops serving stale hits on the next
    /// query's refresh.
    #[test]
    fn remove_workspace_path_drops_the_cached_overlay_entry() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let file = workspace.join("Ext.bsl");
        fs::write(&file, "Процедура Живая()\nКонецПроцедуры").unwrap();

        let db_path = workspace.join("bsl-search.db");
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.index_directory_fts(workspace).unwrap();
        engine.set_workspace_root(workspace);
        engine.enable_workspace_watcher_mode();
        engine.prime_workspace_overlay().unwrap();

        // Edit the file so the overlay caches an entry for it, then confirm it serves.
        fs::write(&file, "Процедура ЖиваяПравка()\nКонецПроцедуры").unwrap();
        assert!(engine.mark_workspace_path_dirty(&file).unwrap());
        assert_eq!(
            engine.text_search("ЖиваяПравка", 10, Some("code")).unwrap().len(),
            1,
            "the edited file is served from the overlay cache",
        );

        // Delete the file and drive the removal branch.
        fs::remove_file(&file).unwrap();
        assert!(engine.remove_workspace_path(&file).unwrap());

        // The removal marked the path dirty; the next query's overlay refresh sees it gone
        // and drops the cached entry, so the stale hit disappears.
        assert!(
            engine.text_search("ЖиваяПравка", 10, Some("code")).unwrap().is_empty(),
            "the removed file no longer serves a stale overlay hit",
        );
    }

    /// A workspace removal evicts exactly the deleted chunks' vectors from the live index
    /// incrementally — it does NOT reload every embedding and rebuild the index. The live
    /// index keeps its count (a tombstone); a full reload would have shrunk it to the one
    /// surviving vector.
    #[test]
    fn remove_workspace_path_evicts_vectors_incrementally_without_full_reload() {
        use crate::embedder::EmbedderConfig;
        use crate::{Chunk, ChunkKind, Store};

        let dir = tempdir().unwrap();
        let db_path = dir.path().join("bsl-search.db");
        let chunk = |name: &str| Chunk {
            kind: ChunkKind::Procedure,
            name: name.to_owned(),
            is_export: true,
            annotations: vec![],
            line_start: 0,
            line_end: 1,
            text: format!("Процедура {name}()\nКонецПроцедуры"),
        };
        let vec_a = vec![1.0f32, 0.0, 0.0];
        let vec_b = vec![0.0f32, 1.0, 0.0];
        {
            let mut store = Store::open(&db_path).unwrap();
            store
                .reindex_file(
                    CONFIGURATION_ROOT_ID,
                    "a.bsl",
                    b"ha",
                    &[chunk("Альфа")],
                    Some(std::slice::from_ref(&vec_a)),
                )
                .unwrap();
            store
                .reindex_file(
                    CONFIGURATION_ROOT_ID,
                    "b.bsl",
                    b"hb",
                    &[chunk("Бета")],
                    Some(std::slice::from_ref(&vec_b)),
                )
                .unwrap();
        }

        let config = crate::SearchConfig {
            embedder: EmbedderConfig {
                base_url: "http://127.0.0.1:1".to_owned(),
                model: "test-model".to_owned(),
                dim: Some(3),
                api_key: None,
                provider: None,
                ..Default::default()
            },
            execution: crate::EmbeddingExecutionPolicy::default(),
        };
        let mut engine = SearchEngine::new(&db_path, config).unwrap();
        engine.set_workspace_root(dir.path());
        assert_eq!(engine.index.len(), 2, "both vectors load at construction");

        assert!(engine.remove_workspace_path(dir.path().join("a.bsl")).unwrap());

        // Incremental eviction keeps the live count (tombstone); a full reload would have
        // rebuilt the index to exactly the one surviving vector.
        assert_eq!(
            engine.index.len(),
            2,
            "removal evicts incrementally; it does not reload every embedding",
        );
        // The evicted vector no longer answers even its own query; the survivor still does.
        let hits_a = engine.search_with_embedding(&vec_a, 5, None).unwrap();
        assert!(
            hits_a.iter().all(|h| h.symbol_name != "Альфа"),
            "the removed file's vector is gone from the live index: {hits_a:?}",
        );
        let hits_b = engine.search_with_embedding(&vec_b, 5, None).unwrap();
        assert_eq!(hits_b.first().map(|h| h.symbol_name.as_str()), Some("Бета"));
    }

    /// A `.bsl`-spelled link resolving to a non-source target keeps its key under the WALKED
    /// spelling: a key under the target's root is forbidden to exist (the walk drops such
    /// files), and the walked key is the only one the file could have been indexed under — the
    /// one a removal must reach.
    #[cfg(unix)]
    #[test]
    fn a_link_to_a_non_source_target_keys_by_its_walked_spelling() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let configuration = workspace.join("cf");
        let extension = workspace.join("cfe");
        fs::create_dir_all(&configuration).unwrap();
        fs::create_dir_all(&extension).unwrap();
        let target = extension.join("Target.txt");
        fs::write(&target, "не исходник").unwrap();
        let alias = configuration.join("Alias.bsl");
        std::os::unix::fs::symlink(&target, &alias).unwrap();

        let mut engine = SearchEngine::fts_only(&workspace.join("bsl-search.db")).unwrap();
        let (roots, _) = crate::WorkspaceRoots::build(
            workspace,
            &configuration,
            std::slice::from_ref(&extension),
        );
        engine.set_workspace_roots(roots);
        let key = engine.workspace_file_key(&alias).expect("the walked spelling is a .bsl");
        assert_eq!(
            (key.root_id.as_str(), key.path.as_str()),
            (crate::CONFIGURATION_ROOT_ID, "Alias.bsl"),
            "attribution must not follow the non-source target into its root"
        );
    }

    /// Walked-spelling attribution must rank the DECLARED roots: for a root declared through a
    /// link, the walked path also lies under the enclosing root's canonical spelling, and a
    /// canonical-ranked lookup would hand the key to the wrong root — missing the row the
    /// removal must reach.
    #[cfg(unix)]
    #[test]
    fn a_non_source_target_under_a_linked_root_keys_by_the_declared_root() {
        let dir = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let workspace = dir.path();
        let configuration = workspace.join("cf");
        fs::create_dir_all(&configuration).unwrap();
        let real_ext = outside.path().join("ext");
        fs::create_dir_all(&real_ext).unwrap();
        let ext_link = configuration.join("ext");
        std::os::unix::fs::symlink(&real_ext, &ext_link).unwrap();
        let source = outside.path().join("Source.bsl");
        fs::write(&source, "Процедура Настоящая()\nКонецПроцедуры").unwrap();
        let alias = real_ext.join("Alias.bsl");
        std::os::unix::fs::symlink(&source, &alias).unwrap();

        let mut engine = SearchEngine::fts_only(&workspace.join("bsl-search.db")).unwrap();
        let (roots, _) = crate::WorkspaceRoots::build(
            workspace,
            &configuration,
            std::slice::from_ref(&ext_link),
        );
        engine.set_workspace_roots(roots);
        let walked_alias = ext_link.join("Alias.bsl");
        let old_key = engine.workspace_file_key(&walked_alias).unwrap();
        engine.store().upsert_file(&old_key.root_id, &old_key.path, b"h", "code").unwrap();

        let foreign = outside.path().join("Foreign.txt");
        fs::write(&foreign, "не исходник").unwrap();
        fs::remove_file(&alias).unwrap();
        std::os::unix::fs::symlink(&foreign, &alias).unwrap();
        assert!(engine.remove_workspace_path(&walked_alias).unwrap());
        assert_eq!(
            engine.file_count().unwrap(),
            0,
            "the removal must reach the key the file was indexed under"
        );
    }

    /// A directory spelled `.bsl` must not pass for a source target: extension-only role
    /// classification would attribute the key to the DIRECTORY'S root, and the stale row under
    /// the walked key would never be reached.
    #[cfg(unix)]
    #[test]
    fn a_directory_target_spelled_bsl_keys_by_the_walked_spelling() {
        let dir = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let workspace = dir.path();
        let configuration = workspace.join("cf");
        let extension = workspace.join("cfe");
        fs::create_dir_all(&configuration).unwrap();
        fs::create_dir_all(&extension).unwrap();
        let source = outside.path().join("Source.bsl");
        fs::write(&source, "Процедура Настоящая()\nКонецПроцедуры").unwrap();
        let alias = configuration.join("Alias.bsl");
        std::os::unix::fs::symlink(&source, &alias).unwrap();

        let mut engine = SearchEngine::fts_only(&workspace.join("bsl-search.db")).unwrap();
        let (roots, _) = crate::WorkspaceRoots::build(
            workspace,
            &configuration,
            std::slice::from_ref(&extension),
        );
        engine.set_workspace_roots(roots);
        let old_key = engine.workspace_file_key(&alias).unwrap();
        engine.store().upsert_file(&old_key.root_id, &old_key.path, b"h", "code").unwrap();

        fs::create_dir(extension.join("Target.bsl")).unwrap();
        fs::remove_file(&alias).unwrap();
        std::os::unix::fs::symlink(extension.join("Target.bsl"), &alias).unwrap();
        assert!(engine.remove_workspace_path(&alias).unwrap());
        assert_eq!(
            engine.file_count().unwrap(),
            0,
            "a live directory target is not a source; the walked key owns the row"
        );
    }

    /// A removed directory can be the DECLARED spelling of a root while every key the root
    /// holds is spelled from its canonical target — and the roots registered under that
    /// target carry spellings the removed path never matches. Attribution alone answers
    /// nothing there: the walk spelled those keys through the link that is now gone, so the
    /// roots under the target are read from the table, which captured it while it still
    /// resolved (github#190). The candidates stay gated by the absence proof.
    #[test]
    fn a_removed_alias_spelling_reaches_roots_under_its_vanished_target() {
        let base = tempdir().unwrap();
        let workspace = base.path().join("w");
        let alias = workspace.join("alias");
        let target = base.path().join("outside").join("outer");
        let inner = target.join("inner");
        fs::create_dir_all(&inner).unwrap();
        let module = inner.join("A.bsl");
        fs::write(&module, "Процедура Внутренняя()\nКонецПроцедуры").unwrap();

        let roots = crate::WorkspaceRoots::with_stated_spellings(
            &workspace,
            vec![
                ("cfe/alias".to_owned(), alias.clone(), target.clone()),
                ("cfe/inner".to_owned(), inner.clone(), inner.clone()),
            ],
        );
        let key = FileKey::new("cfe/inner", "A.bsl");
        let carriers = crate::key_carriers::CarrierKeys {
            store_rows: [key.clone()].into_iter().collect(),
            ..Default::default()
        };

        assert!(
            super::vanished_keys_in(&roots, &carriers, std::slice::from_ref(&alias)).is_empty(),
            "a live file is not a removal, however the candidates were widened",
        );
        fs::remove_file(&module).unwrap();
        assert_eq!(
            super::vanished_keys_in(&roots, &carriers, std::slice::from_ref(&alias)),
            vec![key],
            "the nested root's key belongs to the removed tree",
        );
    }

    /// The same repair when the removal names a path INSIDE the alias root rather than the
    /// root itself: the spelling is still followed to the target, and the roots registered
    /// under it are the ones whose keys the removal has to reach (github#190).
    #[test]
    fn a_removal_inside_an_alias_spelling_still_reaches_its_target() {
        let base = tempdir().unwrap();
        let workspace = base.path().join("w");
        let alias = workspace.join("alias");
        let target = base.path().join("outside").join("outer");
        let inner = target.join("inner");
        fs::create_dir_all(&inner).unwrap();
        let module = inner.join("A.bsl");
        fs::write(&module, "Процедура Внутренняя()\nКонецПроцедуры").unwrap();

        let roots = crate::WorkspaceRoots::with_stated_spellings(
            &workspace,
            vec![
                ("cfe/alias".to_owned(), alias.clone(), target.clone()),
                ("cfe/inner".to_owned(), inner.clone(), inner.clone()),
            ],
        );
        let key = FileKey::new("cfe/inner", "A.bsl");
        let carriers = crate::key_carriers::CarrierKeys {
            store_rows: [key.clone()].into_iter().collect(),
            ..Default::default()
        };

        fs::remove_file(&module).unwrap();
        assert_eq!(
            super::vanished_keys_in(&roots, &carriers, &[alias.join("inner")]),
            vec![key],
            "a removed path inside the declared spelling still names the target's roots",
        );
    }

    /// The journal fixture: a configuration root and an extension root, and a file the walk
    /// reached through a link spelled inside the configuration. `link` is the spelling a drain
    /// delivers; `target` is where the file physically lives.
    fn link_spelling_journal_fixture(
    ) -> (tempfile::TempDir, tempfile::TempDir, SearchEngine, PathBuf, PathBuf) {
        let workspace_dir = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let workspace = workspace_dir.path().to_path_buf();
        let configuration = workspace.join("cf");
        let extension = outside.path().join("ext");
        fs::create_dir_all(&configuration).unwrap();
        fs::create_dir_all(&extension).unwrap();
        let target = extension.join("Module.bsl");
        fs::write(&target, "Процедура Ссылочная()\nКонецПроцедуры").unwrap();
        let link = configuration.join("ExtLink").join("Module.bsl");

        let mut engine = SearchEngine::fts_only(&workspace.join("bsl-search.db")).unwrap();
        let roots = crate::WorkspaceRoots::with_stated_spellings(
            &workspace,
            vec![
                (crate::CONFIGURATION_ROOT_ID.to_owned(), configuration.clone(), configuration),
                ("ext".to_owned(), extension.clone(), extension),
            ],
        );
        engine.initialize_workspace_roots(roots).unwrap();
        (workspace_dir, outside, engine, link, target)
    }

    /// Deleting a file that was reached through a link spelled inside the configuration must
    /// take the EXTENSION's key — the spelling names it — and leave the configuration's key
    /// space untouched: no wrong-key hiding, no wrong-key mark, no row (github#192, topology A).
    #[test]
    fn a_removed_link_spelling_takes_the_key_the_walk_recorded() {
        let (_workspace_dir, _outside, mut engine, link, target) = link_spelling_journal_fixture();
        let key = FileKey::new("ext", "Module.bsl");
        engine.store().upsert_file(&key.root_id, &key.path, b"h", "code").unwrap();
        engine.record_workspace_spellings(&key, &[link.as_path(), target.as_path()]).unwrap();

        // The removal arrives as the link spelling, with the target already gone.
        fs::remove_file(&target).unwrap();
        assert!(engine.remove_workspace_path(&link).unwrap());

        assert_eq!(
            engine.store().all_files_in_collection("code").unwrap().len(),
            0,
            "the extension's row is what the spelling named",
        );
        assert!(
            engine
                .workspace_overlay_dirty_paths()
                .unwrap()
                .iter()
                .all(|dirty| dirty.root_id == "ext"),
            "attribution rerun would have marked a configuration key nobody indexed",
        );
        assert_eq!(engine.workspace_overlay_stats().unwrap().unwrap().hidden_paths, 0);
    }

    /// Removing only the link while the file behind it lives must take NOTHING with it: the
    /// journal answer is gated by the absence proof, so neither the point channel nor the
    /// subtree channel may strand the live file's row (github#192, topology B).
    #[test]
    fn a_removed_link_whose_target_lives_takes_nothing() {
        let (_workspace_dir, _outside, mut engine, link, target) = link_spelling_journal_fixture();
        let key = FileKey::new("ext", "Module.bsl");
        engine.store().upsert_file(&key.root_id, &key.path, b"h", "code").unwrap();
        engine.record_workspace_spellings(&key, &[link.as_path(), target.as_path()]).unwrap();

        assert!(engine.remove_workspace_path(&link).unwrap());
        assert!(
            engine.store().file_hash(&key.root_id, &key.path).unwrap().is_some(),
            "a live target keeps answering search",
        );
        assert_eq!(
            engine.remove_vanished_under(std::slice::from_ref(&link)).unwrap(),
            0,
            "and the subtree channel takes it no more than the point one",
        );
        assert!(engine.workspace_overlay_dirty_paths().unwrap().is_empty());
    }

    /// The batch materializer's journal answer [`CarrierSnapshot::removal_keys`]: a removed
    /// link spelling names the extension's key once the target is gone, and the same answer is
    /// withheld while the target lives (github#192).
    #[test]
    fn snapshot_removal_keys_answer_from_the_journal() {
        let (_workspace_dir, _outside, engine, link, target) = link_spelling_journal_fixture();
        let key = FileKey::new("ext", "Module.bsl");
        engine.store().upsert_file(&key.root_id, &key.path, b"h", "code").unwrap();
        engine.record_workspace_spellings(&key, &[link.as_path(), target.as_path()]).unwrap();
        let reader = crate::Store::open_reader(engine.store().db_path()).unwrap();
        let snapshot = engine.capture_carriers().unwrap().complete(&reader).unwrap();

        assert!(
            snapshot.removal_keys(&reader, std::slice::from_ref(&link), &[]).unwrap().is_empty(),
            "a live target is not a removal, whatever the spelling says",
        );
        fs::remove_file(&target).unwrap();
        assert_eq!(
            snapshot.removal_keys(&reader, std::slice::from_ref(&link), &[]).unwrap(),
            vec![key],
            "the spelling names the key the walk recorded",
        );
    }

    /// The journal is durable: an engine reopened on the same store without ever walking
    /// resolves the same removal (github#192, invariant 4).
    #[test]
    fn the_journal_resolves_a_removal_after_a_restart_without_a_walk() {
        let workspace_dir = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let workspace = workspace_dir.path().to_path_buf();
        let configuration = workspace.join("cf");
        let extension = outside.path().join("ext");
        fs::create_dir_all(&configuration).unwrap();
        fs::create_dir_all(&extension).unwrap();
        let target = extension.join("Module.bsl");
        fs::write(&target, "Процедура Ссылочная()\nКонецПроцедуры").unwrap();
        let link = configuration.join("ExtLink").join("Module.bsl");
        let db = workspace.join("bsl-search.db");
        let key = FileKey::new("ext", "Module.bsl");
        {
            let engine = SearchEngine::fts_only(&db).unwrap();
            engine.store().upsert_file(&key.root_id, &key.path, b"h", "code").unwrap();
            engine.record_workspace_spellings(&key, &[link.as_path(), target.as_path()]).unwrap();
        }

        fs::remove_file(&target).unwrap();
        let mut engine = SearchEngine::fts_only(&db).unwrap();
        let roots = crate::WorkspaceRoots::with_stated_spellings(
            &workspace,
            vec![
                (crate::CONFIGURATION_ROOT_ID.to_owned(), configuration.clone(), configuration),
                ("ext".to_owned(), extension.clone(), extension),
            ],
        );
        engine.initialize_workspace_roots(roots).unwrap();
        assert!(engine.remove_workspace_path(&link).unwrap());
        assert_eq!(
            engine.store().all_files_in_collection("code").unwrap().len(),
            0,
            "the pair outlives the process that recorded it",
        );
    }

    /// A store created before the journal existed is populated again by the ordinary write
    /// paths after the update — the table being recreated empty is not the end of it
    /// (github#192, invariant 5).
    #[test]
    fn a_store_that_predates_the_journal_is_populated_again_by_the_next_write() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let file = workspace.join("A.bsl");
        fs::write(&file, "Процедура Точка()\nКонецПроцедуры").unwrap();
        let db = workspace.join("bsl-search.db");
        {
            let engine = SearchEngine::fts_only(&db).unwrap();
            let saboteur = rusqlite::Connection::open(&db).unwrap();
            saboteur.execute_batch("DROP TABLE path_spellings").unwrap();
            drop(saboteur);
            drop(engine);
        }

        let mut engine = SearchEngine::fts_only(&db).unwrap();
        let (roots, _) = crate::WorkspaceRoots::build(workspace, workspace, &[]);
        engine.initialize_workspace_roots(roots).unwrap();
        assert!(engine.mark_workspace_path_dirty(&file).unwrap());
        assert!(
            engine.store().path_spelling_key(&file.to_string_lossy(), "code").unwrap().is_some(),
            "the update must not leave the journal empty",
        );
    }

    /// The boot ingest is one of the index-write paths and records what it walked: a removal
    /// arriving as the file's own spelling finds its key (github#192, population).
    #[test]
    fn the_boot_ingest_records_the_files_spellings() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let file = workspace.join("A.bsl");
        fs::write(&file, "Процедура Первая()\nКонецПроцедуры").unwrap();
        let mut engine = SearchEngine::fts_only(&workspace.join("bsl-search.db")).unwrap();
        let (roots, _) = crate::WorkspaceRoots::build(workspace, workspace, &[]);
        engine.initialize_workspace_roots(roots).unwrap();

        engine.index_directory_fts(workspace).unwrap();

        assert_eq!(
            engine.store().path_spelling_key(&file.to_string_lossy(), "code").unwrap(),
            Some(FileKey::configuration("A.bsl")),
        );
    }

    /// A point mark is an index write too, so the spelling it arrived as is recorded beside
    /// the key (github#192, population).
    #[test]
    fn a_point_mark_records_the_spelling_it_arrived_as() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let file = workspace.join("A.bsl");
        fs::write(&file, "Процедура Точка()\nКонецПроцедуры").unwrap();
        let engine = SearchEngine::fts_only(&workspace.join("bsl-search.db")).unwrap();
        let (roots, _) = crate::WorkspaceRoots::build(workspace, workspace, &[]);
        let mut engine = engine;
        engine.initialize_workspace_roots(roots).unwrap();

        assert!(engine.mark_workspace_path_dirty(&file).unwrap());
        assert_eq!(
            engine.store().path_spelling_key(&file.to_string_lossy(), "code").unwrap(),
            Some(FileKey::configuration("A.bsl")),
        );
    }

    /// A cold overlay refresh scans the tree, and that scan populates the journal like every
    /// other walk (github#192, population).
    #[test]
    fn a_cold_overlay_refresh_records_the_scanned_spellings() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let file = workspace.join("A.bsl");
        fs::write(&file, "Процедура Оверлейная()\nКонецПроцедуры").unwrap();
        let engine = SearchEngine::fts_only(&workspace.join("bsl-search.db")).unwrap();
        let (roots, _) = crate::WorkspaceRoots::build(workspace, workspace, &[]);
        let mut engine = engine;
        engine.initialize_workspace_roots(roots).unwrap();
        engine.initialize_workspace_overlay_clean().unwrap();

        engine.refresh_workspace_overlay_snapshot(true).unwrap();

        assert_eq!(
            engine.store().path_spelling_key(&file.to_string_lossy(), "code").unwrap(),
            Some(FileKey::configuration("A.bsl")),
        );
    }

    /// A deletion PROVEN by the removal channel must drop the cached overlay entry even when
    /// the whole root vanished with the file: the point refresh would read the dead root as
    /// "unreachable, retry" and leave a ghost entry serving hits forever.
    #[test]
    fn removing_a_file_under_a_vanished_root_drops_its_overlay_entry() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let configuration = workspace.join("cf");
        fs::create_dir_all(&configuration).unwrap();
        fs::write(configuration.join("A.bsl"), "Процедура Локальная()\nКонецПроцедуры").unwrap();

        let mut engine = SearchEngine::fts_only(&workspace.join("bsl-search.db")).unwrap();
        let (roots, _) = crate::WorkspaceRoots::build(workspace, &configuration, &[]);
        engine.set_workspace_roots(roots);
        engine.prime_workspace_overlay().unwrap();
        assert_eq!(engine.text_search("Локальная", 10, Some("code")).unwrap().len(), 1);

        fs::rename(&configuration, workspace.join("cf.saved")).unwrap();
        engine.remove_workspace_path(configuration.join("A.bsl")).unwrap();
        let hits = engine.text_search("Локальная", 10, Some("code")).unwrap();
        assert!(hits.is_empty(), "a proven removal must not leave a ghost entry: {hits:?}");
    }

    /// The proven-removal channel must retract the persisted fingerprint row too: the dirty
    /// mark dies with the process, and a namesake recreated at the same `(len, mtime,
    /// canonical)` would inherit the dead file's "verified" claim across a restart.
    #[test]
    fn a_proven_removal_retracts_the_fingerprint_row() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let file = workspace.join("A.bsl");
        fs::write(&file, "Процедура Локальная()\nКонецПроцедуры").unwrap();

        let mut engine = SearchEngine::fts_only(&workspace.join("bsl-search.db")).unwrap();
        let (roots, _) = crate::WorkspaceRoots::build(workspace, workspace, &[]);
        engine.set_workspace_roots(roots);
        engine.prime_workspace_overlay().unwrap();
        let key = engine.workspace_file_key(&file).unwrap();
        engine
            .store()
            .save_overlay_fingerprint_cache(
                "",
                &HashMap::from([(
                    key.clone(),
                    crate::store::PersistedFingerprint {
                        file_size: 1,
                        file_mtime_secs: 2,
                        file_mtime_nanos: 3,
                        content_fingerprint: "fp".to_owned(),
                        canonical: "/spelled".to_owned(),
                    },
                )]),
            )
            .unwrap();

        fs::remove_file(&file).unwrap();
        engine.remove_workspace_path(&file).unwrap();
        assert!(
            !engine
                .store()
                .load_overlay_fingerprint_cache("")
                .unwrap_or(None)
                .unwrap_or_default()
                .contains_key(&key),
            "the dead file's row must not vouch for a future namesake"
        );
    }

    /// A symlink spelled `.bsl` whose target is not a BSL source is not a source
    /// file: the graph walk drops it because the roles of the two spellings
    /// disagree, and the overlay must agree with that universe.
    #[cfg(unix)]
    #[test]
    fn probe_a_symlink_to_a_non_bsl_target_is_not_indexed() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let configuration = workspace.join("cf");
        let extension = workspace.join("cfe");
        fs::create_dir_all(&configuration).unwrap();
        fs::create_dir_all(&extension).unwrap();
        let target = extension.join("Target.txt");
        fs::write(&target, "Процедура ТолькоЧерезСсылку()\nКонецПроцедуры").unwrap();
        let alias = configuration.join("Alias.bsl");
        std::os::unix::fs::symlink(&target, &alias).unwrap();

        let mut engine = SearchEngine::fts_only(&workspace.join("bsl-search.db")).unwrap();
        let (roots, _) = crate::WorkspaceRoots::build(
            workspace,
            &configuration,
            std::slice::from_ref(&extension),
        );
        engine.set_workspace_roots(roots);
        engine.prime_workspace_overlay().unwrap();
        let before = engine.text_search("ТолькоЧерезСсылку", 10, Some("code")).unwrap();
        assert!(before.is_empty(), "a .txt is not a BSL source file: {before:?}");
    }

    /// End-to-end through a REAL walk: a subtree that loses read permission
    /// makes the scan unclean, and the indexed file inside it survives instead
    /// of being read as deleted. The policy and the walker are each tested on
    /// their own; this leg catches the adapter between them dropping a counter.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_subtree_does_not_erase_its_indexed_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let closed = workspace.join("closed");
        fs::create_dir(&closed).unwrap();
        fs::write(closed.join("Hidden.bsl"), "Процедура ЗаЗакрытымКаталогом()\nКонецПроцедуры")
            .unwrap();

        let mut engine = SearchEngine::fts_only(&workspace.join("bsl-search.db")).unwrap();
        let (roots, _) = crate::WorkspaceRoots::build(workspace, workspace, &[]);
        engine.set_workspace_roots(roots);
        engine.prime_workspace_overlay().unwrap();
        let hits = engine.text_search("ЗаЗакрытымКаталогом", 10, Some("code")).unwrap();
        assert_eq!(hits.len(), 1, "the file is indexed while readable");

        fs::set_permissions(&closed, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read_dir(&closed).is_ok() {
            // Running as root: permissions cannot make the subtree unreadable.
            fs::set_permissions(&closed, fs::Permissions::from_mode(0o755)).unwrap();
            return;
        }
        let rescan = engine.prime_workspace_overlay();
        fs::set_permissions(&closed, fs::Permissions::from_mode(0o755)).unwrap();
        rescan.unwrap();

        let hits = engine.text_search("ЗаЗакрытымКаталогом", 10, Some("code")).unwrap();
        assert_eq!(hits.len(), 1, "an unreadable subtree is not evidence of deletion");
        assert!(
            engine.workspace_overlay_needs_full_rescan().unwrap(),
            "the unclean prime leaves the overlay waiting for a clean rescan"
        );
    }

    /// A cold overlay prime is exactly one walk of the workspace, and an
    /// initialized watcher-mode engine performs none: the walk count is the
    /// observable proving every overlay pass shares the one common scan instead
    /// of a private traversal with its own symlink and error policy.
    #[test]
    fn a_cold_prime_walks_the_workspace_exactly_once() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        fs::write(workspace.join("M.bsl"), "Процедура Раз()\nКонецПроцедуры").unwrap();

        let mut engine = SearchEngine::fts_only(&workspace.join("bsl-search.db")).unwrap();
        let (roots, _) = crate::WorkspaceRoots::build(workspace, workspace, &[]);
        engine.set_workspace_roots(roots);

        let before = project_model::source_set::scans_performed_on_thread();
        engine.prime_workspace_overlay().unwrap();
        let walked = project_model::source_set::scans_performed_on_thread() - before;
        assert_eq!(walked, 1, "one cold prime is one walk");

        engine.enable_workspace_watcher_mode();
        let before = project_model::source_set::scans_performed_on_thread();
        engine.prime_workspace_overlay().unwrap();
        let walked = project_model::source_set::scans_performed_on_thread() - before;
        assert_eq!(walked, 0, "an initialized watcher-mode cache must not rescan");
    }

    /// The removal's retry obligation (the dirty mark) must be set BEFORE the fallible store
    /// operations: an early failure would otherwise leave no signal anywhere while the rows
    /// still tell the old story.
    #[test]
    fn a_failed_removal_still_leaves_the_retry_mark() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let db_path = workspace.join("bsl-search.db");
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.set_workspace_root(workspace);
        engine
            .sync_indexed_documents_in_collection(
                "code",
                &[IndexedDocument {
                    collection: "code".to_owned(),
                    root_id: crate::CONFIGURATION_ROOT_ID.to_owned(),
                    path: "Removed.bsl".to_owned(),
                    symbol_name: "П".to_owned(),
                    kind: "procedure".to_owned(),
                    line_start: 0,
                    line_end: 1,
                    text: "Процедура П()\nКонецПроцедуры".to_owned(),
                    content_hash: "h".to_owned(),
                    graph_context: None,
                    source_span: None,
                }],
                None,
            )
            .unwrap();

        let saboteur = rusqlite::Connection::open(&db_path).unwrap();
        // The fallible store step before the row goes needs a row to fail on.
        engine
            .store()
            .save_overlay_fingerprint_cache(
                "snap",
                &HashMap::from([(
                    FileKey::configuration("Removed.bsl"),
                    crate::store::PersistedFingerprint {
                        file_size: 1,
                        file_mtime_secs: 1,
                        file_mtime_nanos: 0,
                        content_fingerprint: "fp".to_owned(),
                        canonical: "Removed.bsl".to_owned(),
                    },
                )]),
            )
            .unwrap();
        saboteur
            .execute_batch(
                "CREATE TRIGGER deny_fingerprint BEFORE DELETE ON overlay_fingerprint_cache \
                 BEGIN SELECT RAISE(FAIL, 'deny'); END;",
            )
            .unwrap();
        let result = engine.remove_workspace_path(workspace.join("Removed.bsl"));
        assert!(result.is_err(), "the denied retraction surfaces as an error");
        assert!(
            engine
                .workspace_overlay_dirty_paths_snapshot()
                .unwrap()
                .contains_key(&FileKey::configuration("Removed.bsl")),
            "the retry mark was set before the store failed"
        );
    }

    /// An INHERITED manifest (a warm-cache left by a Postgres period) is not baseline
    /// evidence for a LOCAL engine: a removal must not hide anything on its account.
    #[test]
    fn a_local_removal_ignores_an_inherited_manifest() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let file = workspace.join("Removed.bsl");
        let content = "Процедура П()\nКонецПроцедуры";
        fs::write(&file, content).unwrap();
        let db_path = workspace.join("bsl-search.db");
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.index_directory_fts(workspace).unwrap();
        engine.set_workspace_root(workspace);
        engine
            .store()
            .save_baseline_manifest(&crate::WorkspaceBaselineManifest {
                snapshot_id: "stale-snap".to_owned(),
                snapshot_fingerprint: Some("fp".to_owned()),
                files: vec![crate::BaselineManifestFile {
                    root_id: crate::CONFIGURATION_ROOT_ID.to_owned(),
                    collection: "code".to_owned(),
                    path: "Removed.bsl".to_owned(),
                    file_fingerprint: crate::workspace_overlay::fingerprint_content(
                        content,
                        "Removed.bsl",
                    ),
                    document_count: 1,
                    file_object_id: "obj-1".to_owned(),
                }],
            })
            .unwrap();

        fs::remove_file(&file).unwrap();
        assert!(engine.remove_workspace_path(workspace.join("Removed.bsl")).unwrap());
        let stats = engine.workspace_overlay_stats().unwrap().unwrap();
        assert_eq!(stats.deleted_files, 0, "an inherited manifest proves no baseline copy to hide");
    }

    /// A LOCAL engine's dirty-path refresh reads its edits against the local store rows, not
    /// against an inherited manifest: an edit that happens to equal the STALE manifest
    /// fingerprint must still become an overlay entry.
    #[test]
    fn a_local_point_refresh_ignores_an_inherited_manifest() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let file = workspace.join("CommonModule.bsl");
        let local = "Процедура Локальная()\nКонецПроцедуры";
        let edited = "Процедура Правка()\nКонецПроцедуры";
        fs::write(&file, local).unwrap();
        let db_path = workspace.join("bsl-search.db");
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.index_directory_fts(workspace).unwrap();
        engine.set_workspace_root(workspace);
        engine.enable_workspace_watcher_mode();
        engine.prime_workspace_overlay().unwrap();
        engine
            .store()
            .save_baseline_manifest(&crate::WorkspaceBaselineManifest {
                snapshot_id: "stale-snap".to_owned(),
                snapshot_fingerprint: Some("fp".to_owned()),
                files: vec![crate::BaselineManifestFile {
                    root_id: crate::CONFIGURATION_ROOT_ID.to_owned(),
                    collection: "code".to_owned(),
                    path: "CommonModule.bsl".to_owned(),
                    file_fingerprint: crate::workspace_overlay::fingerprint_content(
                        edited,
                        "CommonModule.bsl",
                    ),
                    document_count: 1,
                    file_object_id: "obj-1".to_owned(),
                }],
            })
            .unwrap();

        fs::write(&file, edited).unwrap();
        assert!(engine.mark_workspace_path_dirty(&file).unwrap());
        let stats = engine.workspace_overlay_stats().unwrap().unwrap();
        assert_eq!(
            stats.overlay_files, 1,
            "the edit differs from the LOCAL baseline and must serve as an overlay entry \
             even though it equals the stale manifest fingerprint"
        );
    }

    /// Declaring the local mode clears inherited fingerprint rows: they claim "verified
    /// against the manifest", and the raw mode can neither honour nor refresh that claim —
    /// a same-stat edit during the local period would be suppressed by the row after a
    /// switch back to the remote mode.
    #[test]
    fn declaring_the_local_mode_clears_inherited_fingerprint_rows() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let db_path = workspace.join("bsl-search.db");
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine
            .store()
            .save_overlay_fingerprint_cache(
                "snap-1",
                &HashMap::from([(
                    FileKey::configuration("A.bsl"),
                    crate::store::PersistedFingerprint {
                        file_size: 1,
                        file_mtime_secs: 1,
                        file_mtime_nanos: 0,
                        content_fingerprint: "fp".to_owned(),
                        canonical: String::new(),
                    },
                )]),
            )
            .unwrap();

        engine.set_serves_external_baseline(false).unwrap();
        let rows = engine
            .store()
            .load_overlay_fingerprint_cache("snap-1")
            .unwrap_or(None)
            .unwrap_or_default();
        assert!(rows.is_empty(), "the local mode owns no manifest-verified rows");
    }
    /// A warm root must not be walked when only the cold ones need ingesting: the per-root skip
    /// exists to keep a restart cheap, and walking everything and filtering afterwards spends
    /// exactly what it was meant to save. Attribution still consults the whole table, so a file
    /// found under one root but owned by another keeps its owner's key.
    #[test]
    fn a_subset_walk_visits_only_the_roots_it_was_given() {
        let dir = tempdir().unwrap();
        let workspace = dir.path().join("ws");
        let configuration = workspace.join("cf");
        let extension = dir.path().join("outside-ext");
        fs::create_dir_all(&configuration).unwrap();
        fs::create_dir_all(&extension).unwrap();
        fs::write(configuration.join("Тёплый.bsl"), "Процедура Первая()\nКонецПроцедуры").unwrap();
        fs::write(extension.join("Холодный.bsl"), "Процедура Вторая()\nКонецПроцедуры").unwrap();

        let mut engine = SearchEngine::fts_only(&dir.path().join("search.db")).unwrap();
        let (roots, _) = crate::WorkspaceRoots::build(
            &workspace,
            &configuration,
            std::slice::from_ref(&extension),
        );
        engine.set_workspace_roots(roots);

        let all = engine.boot_ingest_files(&configuration);
        assert_eq!(all.len(), 2, "the full walk covers both roots: {all:?}");

        let cold_only = engine.boot_ingest_files_over(
            std::path::Path::new(""),
            Some(std::slice::from_ref(&extension)),
        );
        let names: Vec<String> = cold_only.iter().map(|(key, _)| key.path.clone()).collect();
        assert_eq!(names, vec!["Холодный.bsl".to_owned()], "only the given root is walked");
    }
    /// A relative path handed to the engine is spelled against the CONFIGURATION root — that is
    /// how every stored path with the reserved id is spelled, and what callers strip before
    /// handing one over (the graph bridge strips the configuration prefix). Resolving it against
    /// the table's workspace instead points one directory too high whenever the configuration
    /// sits in a subdirectory, and the key is then silently not found: the mark is dropped and
    /// the stale graph context is served on.
    #[test]
    fn a_relative_path_is_spelled_against_the_configuration_root() {
        let dir = tempdir().unwrap();
        let workspace = dir.path().join("ws");
        let configuration = workspace.join("src").join("cf");
        let module = configuration.join("CommonModules").join("Б").join("Ext");
        fs::create_dir_all(&module).unwrap();
        fs::write(module.join("Module.bsl"), "Процедура Первая()\nКонецПроцедуры").unwrap();

        let mut engine = SearchEngine::fts_only(&dir.path().join("search.db")).unwrap();
        let (roots, _) = crate::WorkspaceRoots::build(&workspace, &configuration, &[]);
        engine.set_workspace_roots(roots);
        engine.index_directory_fts(&configuration).unwrap();
        assert_eq!(engine.file_count().unwrap(), 1, "the fixture indexes the module");

        let marked = engine
            .mark_workspace_path_context_dirty("CommonModules/Б/Ext/Module.bsl")
            .expect("marking a workspace path is not an error");
        assert!(marked, "a configuration-relative path resolves to its stored key");
    }

    /// A context mark re-renders a stored context and indexes nothing, so it must not move a
    /// spelling's journal record to whatever the spelling attributes to now: the record names
    /// the key the file was indexed under, and a later removal of that spelling has to find it.
    #[test]
    fn a_context_mark_leaves_the_spelling_journal_alone() {
        let dir = tempdir().unwrap();
        let workspace = dir.path().join("ws");
        let configuration = workspace.join("cf");
        let module = configuration.join("CommonModules").join("Б").join("Ext").join("Module.bsl");
        fs::create_dir_all(module.parent().unwrap()).unwrap();
        fs::write(&module, "Процедура Первая()\nКонецПроцедуры").unwrap();

        let mut engine = SearchEngine::fts_only(&dir.path().join("search.db")).unwrap();
        let (roots, _) = crate::WorkspaceRoots::build(&workspace, &configuration, &[]);
        engine.initialize_workspace_roots(roots).unwrap();
        let indexed = FileKey::new("ext", "CommonModules/Б/Ext/Module.bsl");
        let spelling = module.to_string_lossy().into_owned();
        engine.store().record_path_spelling(&spelling, &indexed, "code").unwrap();

        assert!(engine.mark_workspace_path_context_dirty(&module).unwrap());
        assert_eq!(
            engine.store().path_spelling_key(&spelling, "code").unwrap(),
            Some(indexed),
            "a context mark is not an index write and keeps the recorded key",
        );
    }

    /// A key names a regular file, so a directory standing at its path is no evidence the
    /// file lives: the walk would never index it, and the journal's answer must not be
    /// withheld over it.
    #[test]
    fn a_directory_in_place_of_an_indexed_file_proves_the_file_gone() {
        let dir = tempdir().unwrap();
        let workspace = dir.path().join("ws");
        let configuration = workspace.join("cf");
        let module = configuration.join("CommonModules").join("Б").join("Ext").join("Module.bsl");
        fs::create_dir_all(module.parent().unwrap()).unwrap();
        fs::write(&module, "Процедура Первая()\nКонецПроцедуры").unwrap();

        let mut engine = SearchEngine::fts_only(&dir.path().join("search.db")).unwrap();
        let (roots, _) = crate::WorkspaceRoots::build(&workspace, &configuration, &[]);
        engine.initialize_workspace_roots(roots).unwrap();
        let key = FileKey::new(crate::CONFIGURATION_ROOT_ID, "CommonModules/Б/Ext/Module.bsl");
        engine.store().upsert_file(&key.root_id, &key.path, b"h", "code").unwrap();
        engine.record_workspace_spellings(&key, &[module.as_path()]).unwrap();

        fs::remove_file(&module).unwrap();
        fs::create_dir(&module).unwrap();
        assert!(engine.remove_workspace_path(&module).unwrap());
        assert_eq!(
            engine.store().file_hash(&key.root_id, &key.path).unwrap(),
            None,
            "a directory where the file stood does not keep the file's row alive",
        );
    }

    fn write_transition_module(root: &std::path::Path, procedure: &str) {
        let path = root.join("CommonModules/Один/Ext/Module.bsl");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, format!("Процедура {procedure}() Экспорт\nКонецПроцедуры")).unwrap();
    }

    #[test]
    fn live_root_transition_adds_and_removes_an_extension_with_a_namesake_path() {
        let dir = tempdir().unwrap();
        let workspace = dir.path().join("ws");
        let configuration = workspace.join("cf");
        let extension = workspace.join("cfe/one");
        write_transition_module(&configuration, "Конфигурация");
        write_transition_module(&extension, "Расширение");

        let mut engine = SearchEngine::fts_only(&dir.path().join("search.db")).unwrap();
        let configuration_only = crate::WorkspaceRoots::build(&workspace, &configuration, &[]).0;
        engine.initialize_workspace_roots(configuration_only.clone()).unwrap();
        engine.index_directory_fts(&configuration).unwrap();

        let both = crate::WorkspaceRoots::build(
            &workspace,
            &configuration,
            std::slice::from_ref(&extension),
        )
        .0;
        let plan = engine.workspace_roots_transition_seed(both).unwrap().plan().unwrap();
        let outcome = engine
            .apply_validated_workspace_roots_transition(plan.revalidate().unwrap().unwrap())
            .unwrap();
        assert!(matches!(
            outcome,
            super::WorkspaceRootsTransitionOutcome::Applied { added: 1, .. }
        ));
        let extension_hits = engine.text_search("Расширение", 10, Some("code")).unwrap();
        assert_eq!(extension_hits.len(), 1);
        assert_eq!(extension_hits[0].root_id, "cfe/one");
        assert_eq!(
            engine.text_search("Конфигурация", 10, Some("code")).unwrap().len(),
            1,
            "the same relative path in the stable configuration survives"
        );

        let plan =
            engine.workspace_roots_transition_seed(configuration_only).unwrap().plan().unwrap();
        let outcome = engine
            .apply_validated_workspace_roots_transition(plan.revalidate().unwrap().unwrap())
            .unwrap();
        assert!(matches!(
            outcome,
            super::WorkspaceRootsTransitionOutcome::Applied { removed: 1, .. }
        ));
        assert!(engine.text_search("Расширение", 10, Some("code")).unwrap().is_empty());
        assert_eq!(engine.file_count().unwrap(), 1);
    }

    #[test]
    fn invalid_utf8_in_a_stable_root_does_not_block_adding_an_extension() {
        let dir = tempdir().unwrap();
        let workspace = dir.path().join("ws");
        let configuration = workspace.join("cf");
        let extension = workspace.join("cfe/one");
        write_transition_module(&configuration, "Сохранена");
        write_transition_module(&extension, "Расширение");
        let stable_file = configuration.join("CommonModules/Один/Ext/Module.bsl");

        let mut engine = SearchEngine::fts_only(&dir.path().join("search.db")).unwrap();
        let initial = crate::WorkspaceRoots::build(&workspace, &configuration, &[]).0;
        engine.initialize_workspace_roots(initial).unwrap();
        engine.index_directory_fts(&configuration).unwrap();
        engine.initialize_workspace_overlay_clean().unwrap();
        fs::write(&stable_file, [0xcf, 0xf0, 0xee, 0xf6]).unwrap();

        let both = crate::WorkspaceRoots::build(
            &workspace,
            &configuration,
            std::slice::from_ref(&extension),
        )
        .0;
        let plan = engine.workspace_roots_transition_seed(both).unwrap().plan().unwrap();
        let outcome = engine
            .apply_validated_workspace_roots_transition(plan.revalidate().unwrap().unwrap())
            .unwrap();

        assert!(matches!(
            outcome,
            super::WorkspaceRootsTransitionOutcome::Applied { rebuilt: 0, added: 1, .. }
        ));
        assert_eq!(engine.text_search("Сохранена", 10, Some("code")).unwrap().len(), 1);
        assert_eq!(engine.text_search("Расширение", 10, Some("code")).unwrap().len(), 1);
        assert_eq!(engine.workspace_overlay_unread_count().unwrap(), 1);
        assert!(engine
            .workspace_overlay_dirty_paths()
            .unwrap()
            .contains(&FileKey::configuration("CommonModules/Один/Ext/Module.bsl")));
    }

    #[test]
    fn unread_surviving_remote_overlay_entry_is_preserved() {
        let dir = tempdir().unwrap();
        let workspace = dir.path().join("ws");
        let configuration = workspace.join("cf");
        let extension = workspace.join("cfe/one");
        write_transition_module(&configuration, "ЛокальнаяПравка");
        write_transition_module(&extension, "Расширение");
        let stable_file = configuration.join("CommonModules/Один/Ext/Module.bsl");
        let relative = "CommonModules/Один/Ext/Module.bsl";

        let mut engine = SearchEngine::fts_only(&dir.path().join("search.db")).unwrap();
        engine.set_serves_external_baseline(true).unwrap();
        engine
            .store()
            .save_baseline_manifest(&crate::WorkspaceBaselineManifest {
                snapshot_id: "snap".to_owned(),
                snapshot_fingerprint: None,
                files: vec![crate::BaselineManifestFile {
                    root_id: CONFIGURATION_ROOT_ID.to_owned(),
                    collection: "code".to_owned(),
                    path: relative.to_owned(),
                    file_fingerprint: "remote-version".to_owned(),
                    document_count: 1,
                    file_object_id: "obj".to_owned(),
                }],
            })
            .unwrap();
        let initial = crate::WorkspaceRoots::build(&workspace, &configuration, &[]).0;
        engine.initialize_workspace_roots(initial).unwrap();
        engine.prime_workspace_overlay().unwrap();
        assert_eq!(engine.text_search("ЛокальнаяПравка", 10, Some("code")).unwrap().len(), 1);
        fs::write(&stable_file, [0xcf, 0xf0, 0xee, 0xf6]).unwrap();

        let both = crate::WorkspaceRoots::build(
            &workspace,
            &configuration,
            std::slice::from_ref(&extension),
        )
        .0;
        let plan = engine.workspace_roots_transition_seed(both).unwrap().plan().unwrap();
        engine
            .apply_validated_workspace_roots_transition(plan.revalidate().unwrap().unwrap())
            .unwrap();

        assert_eq!(engine.text_search("ЛокальнаяПравка", 10, Some("code")).unwrap().len(), 1);
        assert_eq!(engine.workspace_overlay_stats().unwrap().unwrap().overlay_files, 2);
        assert!(
            engine
                .workspace_overlay_cache
                .lock()
                .unwrap()
                .hidden_keys()
                .contains(&FileKey::configuration(relative)),
            "the preserved local entry must keep hiding its remote baseline twin"
        );
        assert_eq!(engine.workspace_overlay_unread_count().unwrap(), 1);
    }

    #[test]
    fn invalid_utf8_in_a_new_root_is_unread_not_deleted_and_later_heals() {
        let dir = tempdir().unwrap();
        let workspace = dir.path().join("ws");
        let configuration = workspace.join("cf");
        let extension = workspace.join("cfe/one");
        write_transition_module(&configuration, "Конфигурация");
        let extension_file = extension.join("Broken.bsl");
        fs::create_dir_all(&extension).unwrap();
        fs::write(&extension_file, [0xcf, 0xf0, 0xee, 0xf6]).unwrap();

        let mut engine = SearchEngine::fts_only(&dir.path().join("search.db")).unwrap();
        let initial = crate::WorkspaceRoots::build(&workspace, &configuration, &[]).0;
        engine.initialize_workspace_roots(initial).unwrap();
        engine.index_directory_fts(&configuration).unwrap();
        engine.initialize_workspace_overlay_clean().unwrap();
        let both = crate::WorkspaceRoots::build(
            &workspace,
            &configuration,
            std::slice::from_ref(&extension),
        )
        .0;
        let plan = engine.workspace_roots_transition_seed(both).unwrap().plan().unwrap();
        let outcome = engine
            .apply_validated_workspace_roots_transition(plan.revalidate().unwrap().unwrap())
            .unwrap();

        assert!(matches!(
            outcome,
            super::WorkspaceRootsTransitionOutcome::Applied {
                added: 0,
                pending_overlay_embeddings: false,
                ..
            }
        ));
        let key = FileKey::new("cfe/one", "Broken.bsl");
        assert_eq!(engine.workspace_overlay_unread_count().unwrap(), 1);
        assert!(engine.workspace_overlay_dirty_paths().unwrap().contains(&key));

        fs::write(&extension_file, "Процедура Исцелена()\nКонецПроцедуры").unwrap();
        crate::point_refresh::point_refresh_for_test(&engine).expect("a dirty key to refresh");
        assert_eq!(engine.workspace_overlay_unread_count().unwrap(), 0);
        assert!(!engine.workspace_overlay_dirty_paths().unwrap().contains(&key));
        assert_eq!(engine.text_search("Исцелена", 10, Some("code")).unwrap().len(), 1);
    }

    #[test]
    fn unread_present_remote_baseline_is_not_hidden() {
        let dir = tempdir().unwrap();
        let workspace = dir.path().join("ws");
        let configuration = workspace.join("cf");
        let extension = workspace.join("cfe/one");
        write_transition_module(&configuration, "Конфигурация");
        fs::create_dir_all(&extension).unwrap();
        fs::write(extension.join("Broken.bsl"), [0xcf, 0xf0, 0xee, 0xf6]).unwrap();

        let mut engine = SearchEngine::fts_only(&dir.path().join("search.db")).unwrap();
        engine.set_serves_external_baseline(true).unwrap();
        engine
            .store()
            .save_baseline_manifest(&crate::WorkspaceBaselineManifest {
                snapshot_id: "snap".to_owned(),
                snapshot_fingerprint: None,
                files: vec![crate::BaselineManifestFile {
                    root_id: "cfe/one".to_owned(),
                    collection: "code".to_owned(),
                    path: "Broken.bsl".to_owned(),
                    file_fingerprint: "baseline".to_owned(),
                    document_count: 1,
                    file_object_id: "obj".to_owned(),
                }],
            })
            .unwrap();
        let initial = crate::WorkspaceRoots::build(&workspace, &configuration, &[]).0;
        engine.initialize_workspace_roots(initial).unwrap();
        engine.initialize_workspace_overlay_clean().unwrap();
        let both = crate::WorkspaceRoots::build(
            &workspace,
            &configuration,
            std::slice::from_ref(&extension),
        )
        .0;
        let plan = engine.workspace_roots_transition_seed(both).unwrap().plan().unwrap();
        engine
            .apply_validated_workspace_roots_transition(plan.revalidate().unwrap().unwrap())
            .unwrap();

        let stats = engine.workspace_overlay_stats().unwrap().unwrap();
        assert_eq!(stats.hidden_paths, 0);
        assert_eq!(stats.deleted_files, 0);
        assert_eq!(engine.workspace_overlay_unread_count().unwrap(), 1);
    }

    #[test]
    fn unread_file_becoming_readable_during_validation_supersedes_the_plan() {
        let dir = tempdir().unwrap();
        let workspace = dir.path().join("ws");
        let configuration = workspace.join("cf");
        let extension = workspace.join("cfe/one");
        write_transition_module(&configuration, "Конфигурация");
        fs::create_dir_all(&extension).unwrap();
        let broken = extension.join("Broken.bsl");
        fs::write(&broken, [0xcf, 0xf0, 0xee, 0xf6]).unwrap();
        let engine = SearchEngine::fts_only(&dir.path().join("search.db")).unwrap();
        let initial = crate::WorkspaceRoots::build(&workspace, &configuration, &[]).0;
        let mut engine = engine;
        engine.initialize_workspace_roots(initial).unwrap();
        let both = crate::WorkspaceRoots::build(
            &workspace,
            &configuration,
            std::slice::from_ref(&extension),
        )
        .0;
        let plan = engine.workspace_roots_transition_seed(both).unwrap().plan().unwrap();
        fs::write(&broken, "Процедура Исцелена()\nКонецПроцедуры").unwrap();
        assert!(plan.revalidate().unwrap().is_none());
    }

    #[test]
    fn changing_the_configuration_directory_rebuilds_the_stable_empty_root_id() {
        let dir = tempdir().unwrap();
        let workspace = dir.path().join("ws");
        let old_configuration = workspace.join("old-cf");
        let new_configuration = workspace.join("new-cf");
        write_transition_module(&old_configuration, "Старая");
        write_transition_module(&new_configuration, "Новая");

        let mut engine = SearchEngine::fts_only(&dir.path().join("search.db")).unwrap();
        let old = crate::WorkspaceRoots::build(&workspace, &old_configuration, &[]).0;
        engine.initialize_workspace_roots(old).unwrap();
        engine.index_directory_fts(&old_configuration).unwrap();
        let next = crate::WorkspaceRoots::build(&workspace, &new_configuration, &[]).0;
        let plan = engine.workspace_roots_transition_seed(next).unwrap().plan().unwrap();
        engine
            .apply_validated_workspace_roots_transition(plan.revalidate().unwrap().unwrap())
            .unwrap();

        assert!(engine.text_search("Старая", 10, Some("code")).unwrap().is_empty());
        let hits = engine.text_search("Новая", 10, Some("code")).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].root_id, CONFIGURATION_ROOT_ID);
        assert_eq!(engine.configuration_root(), Some(new_configuration.as_path()));
    }

    #[test]
    fn moving_workspace_roots_preserves_matching_chunks_and_vectors() {
        let dir = tempdir().unwrap();
        let old_workspace = dir.path().join("old");
        let new_workspace = dir.path().join("new");
        let old_configuration = old_workspace.join("cf");
        let new_configuration = new_workspace.join("cf");
        write_transition_module(&old_configuration, "Перенесена");
        write_transition_module(&new_configuration, "Перенесена");

        let mut engine = SearchEngine::fts_only(&dir.path().join("search.db")).unwrap();
        let old_roots = crate::WorkspaceRoots::build(&old_workspace, &old_configuration, &[]).0;
        engine.initialize_workspace_roots(old_roots).unwrap();
        engine.index_directory_fts(&old_configuration).unwrap();

        let key = FileKey::configuration("CommonModules/Один/Ext/Module.bsl");
        let chunk_ids = engine.store().chunk_ids_for_file("code", &key.root_id, &key.path).unwrap();
        assert_eq!(chunk_ids.len(), 1);
        let embedding = vec![0.25_f32; 1024];
        engine.store().set_chunk_embeddings(&[(chunk_ids[0], embedding)]).unwrap();
        let (_, before) = engine.store().load_all_embeddings_with_generation(1024).unwrap();

        let new_roots = crate::WorkspaceRoots::build(&new_workspace, &new_configuration, &[]).0;
        let plan = engine.workspace_roots_transition_seed(new_roots).unwrap().plan().unwrap();
        let outcome = engine
            .apply_validated_workspace_roots_transition(plan.revalidate().unwrap().unwrap())
            .unwrap();

        assert!(matches!(
            outcome,
            super::WorkspaceRootsTransitionOutcome::Applied {
                removed: 0,
                rebuilt: 0,
                added: 0,
                pending_collection_embeddings: false,
                ..
            }
        ));
        let (_, after) = engine.store().load_all_embeddings_with_generation(1024).unwrap();
        assert_eq!(after, before, "a physical move must keep the stored vectors");
        assert_eq!(engine.vector_count(), 1, "the live vector index keeps the matching vector");
        assert_eq!(engine.configuration_root(), Some(new_configuration.as_path()));
        assert_eq!(engine.text_search("Перенесена", 10, Some("code")).unwrap().len(), 1);
    }

    /// Rebinding a root id to another directory keeps a readable file whose bytes match, but a
    /// file the new directory cannot read has no bytes to match: the old directory's rows for
    /// that key must not survive as its content.
    #[test]
    fn an_unreadable_file_in_a_rebound_root_drops_the_old_directory_rows() {
        let dir = tempdir().unwrap();
        let workspace = dir.path().join("ws");
        let old_configuration = workspace.join("old-cf");
        let new_configuration = workspace.join("new-cf");
        write_transition_module(&old_configuration, "СтараяВерсия");
        let relative = "CommonModules/Один/Ext/Module.bsl";
        fs::create_dir_all(new_configuration.join(relative).parent().unwrap()).unwrap();
        fs::write(new_configuration.join(relative), [0xcf, 0xf0, 0xee, 0xf6]).unwrap();

        let mut engine = SearchEngine::fts_only(&dir.path().join("search.db")).unwrap();
        let old = crate::WorkspaceRoots::build(&workspace, &old_configuration, &[]).0;
        engine.initialize_workspace_roots(old).unwrap();
        engine.index_directory_fts(&old_configuration).unwrap();
        assert_eq!(engine.text_search("СтараяВерсия", 10, Some("code")).unwrap().len(), 1);

        let next = crate::WorkspaceRoots::build(&workspace, &new_configuration, &[]).0;
        let plan = engine.workspace_roots_transition_seed(next).unwrap().plan().unwrap();
        engine
            .apply_validated_workspace_roots_transition(plan.revalidate().unwrap().unwrap())
            .unwrap();

        assert!(
            engine.text_search("СтараяВерсия", 10, Some("code")).unwrap().is_empty(),
            "the old directory's text must not stand in for an unreadable file",
        );
        assert_eq!(engine.configuration_root(), Some(new_configuration.as_path()));
    }

    /// Under an external baseline the stored file hash is the baseline's, not the overlay's. A
    /// local edit carried from the old directory must not survive a rebind to a directory whose
    /// file equals the baseline just because the bytes match the baseline hash.
    #[test]
    fn a_rebound_root_does_not_keep_an_overlay_edit_from_the_old_directory() {
        let dir = tempdir().unwrap();
        let workspace = dir.path().join("ws");
        let edited_configuration = workspace.join("edited-cf");
        let baseline_configuration = workspace.join("baseline-cf");
        write_transition_module(&edited_configuration, "ЛокальнаяПравка");
        write_transition_module(&baseline_configuration, "Базовая");
        let relative = "CommonModules/Один/Ext/Module.bsl";
        let baseline_bytes = fs::read(baseline_configuration.join(relative)).unwrap();
        let baseline_text = String::from_utf8(baseline_bytes.clone()).unwrap();

        let db = dir.path().join("search.db");
        let mut engine = SearchEngine::fts_only(&db).unwrap();
        engine.set_serves_external_baseline(true).unwrap();
        engine
            .store()
            .save_baseline_manifest(&crate::WorkspaceBaselineManifest {
                snapshot_id: "snap-1".to_owned(),
                snapshot_fingerprint: Some("fp".to_owned()),
                files: vec![crate::BaselineManifestFile {
                    root_id: CONFIGURATION_ROOT_ID.to_owned(),
                    collection: "code".to_owned(),
                    path: relative.to_owned(),
                    file_fingerprint: crate::workspace_overlay::fingerprint_content(
                        &baseline_text,
                        relative,
                    ),
                    document_count: 1,
                    file_object_id: "obj-cf".to_owned(),
                }],
            })
            .unwrap();
        crate::Store::open(&db)
            .unwrap()
            .reindex_file_with_context(
                CONFIGURATION_ROOT_ID,
                relative,
                &crate::content_blake3(&baseline_bytes),
                &crate::Chunker::chunk(&baseline_text),
                None,
                None,
            )
            .unwrap();
        let edited = crate::WorkspaceRoots::build(&workspace, &edited_configuration, &[]).0;
        engine.initialize_workspace_roots(edited).unwrap();
        engine.prime_workspace_overlay().unwrap();
        let stats = engine.workspace_overlay_stats().unwrap().unwrap();
        assert_eq!(stats.overlay_files, 1, "the edit is carried by the overlay");
        assert_eq!(stats.hidden_paths, 1, "the edit hides its baseline twin");
        assert!(
            engine.store().file_hash(CONFIGURATION_ROOT_ID, relative).unwrap().is_some(),
            "the stored hash is still the baseline's",
        );

        let next = crate::WorkspaceRoots::build(&workspace, &baseline_configuration, &[]).0;
        let plan = engine.workspace_roots_transition_seed(next).unwrap().plan().unwrap();
        engine
            .apply_validated_workspace_roots_transition(plan.revalidate().unwrap().unwrap())
            .unwrap();
        let stats = engine.workspace_overlay_stats().unwrap().unwrap();
        assert_eq!(stats.overlay_files, 0, "the new directory's file equals the baseline");
        assert_eq!(stats.hidden_paths, 0, "the baseline is visible again");
    }

    #[test]
    fn an_incomplete_transition_scan_keeps_the_last_known_good_roots() {
        let dir = tempdir().unwrap();
        let workspace = dir.path().join("ws");
        let configuration = workspace.join("cf");
        let missing_extension = workspace.join("missing-extension");
        write_transition_module(&configuration, "Конфигурация");

        let mut engine = SearchEngine::fts_only(&dir.path().join("search.db")).unwrap();
        let initial = crate::WorkspaceRoots::build(&workspace, &configuration, &[]).0;
        engine.initialize_workspace_roots(initial.clone()).unwrap();
        let next = crate::WorkspaceRoots::build(
            &workspace,
            &configuration,
            std::slice::from_ref(&missing_extension),
        )
        .0;

        assert!(engine.workspace_roots_transition_seed(next).unwrap().plan().is_err());
        assert_eq!(engine.workspace_roots(), Some(&initial));
    }

    #[test]
    fn a_file_change_between_plan_and_apply_supersedes_the_transition() {
        let dir = tempdir().unwrap();
        let workspace = dir.path().join("ws");
        let configuration = workspace.join("cf");
        let extension = workspace.join("cfe/one");
        write_transition_module(&configuration, "Конфигурация");
        write_transition_module(&extension, "До");
        let extension_file = extension.join("CommonModules/Один/Ext/Module.bsl");

        let mut engine = SearchEngine::fts_only(&dir.path().join("search.db")).unwrap();
        let initial = crate::WorkspaceRoots::build(&workspace, &configuration, &[]).0;
        engine.initialize_workspace_roots(initial.clone()).unwrap();
        engine.index_directory_fts(&configuration).unwrap();
        let next = crate::WorkspaceRoots::build(
            &workspace,
            &configuration,
            std::slice::from_ref(&extension),
        )
        .0;
        let plan = engine.workspace_roots_transition_seed(next).unwrap().plan().unwrap();
        fs::write(&extension_file, "Процедура После()\nКонецПроцедуры").unwrap();

        assert!(
            plan.revalidate().unwrap().is_none(),
            "the changed bytes supersede the plan before the engine is borrowed"
        );
        assert_eq!(engine.workspace_roots(), Some(&initial));
        assert!(engine.text_search("После", 10, Some("code")).unwrap().is_empty());
    }

    #[test]
    fn remote_overlay_transition_is_lexical_without_embedding_and_hides_removed_baseline() {
        let dir = tempdir().unwrap();
        let workspace = dir.path().join("ws");
        let configuration = workspace.join("cf");
        let extension = workspace.join("cfe/one");
        write_transition_module(&configuration, "Конфигурация");
        write_transition_module(&extension, "ЛокальнаяПравка");
        let relative = "CommonModules/Один/Ext/Module.bsl";

        let mut engine = SearchEngine::fts_only(&dir.path().join("search.db")).unwrap();
        engine.set_serves_external_baseline(true).unwrap();
        engine
            .store()
            .save_baseline_manifest(&crate::WorkspaceBaselineManifest {
                snapshot_id: "snap-1".to_owned(),
                snapshot_fingerprint: Some("fp".to_owned()),
                files: vec![
                    crate::BaselineManifestFile {
                        root_id: CONFIGURATION_ROOT_ID.to_owned(),
                        collection: "code".to_owned(),
                        path: relative.to_owned(),
                        file_fingerprint: crate::workspace_overlay::fingerprint_content(
                            &fs::read_to_string(configuration.join(relative)).unwrap(),
                            relative,
                        ),
                        document_count: 1,
                        file_object_id: "obj-cf".to_owned(),
                    },
                    crate::BaselineManifestFile {
                        root_id: "cfe/one".to_owned(),
                        collection: "code".to_owned(),
                        path: relative.to_owned(),
                        file_fingerprint: "remote-version".to_owned(),
                        document_count: 1,
                        file_object_id: "obj-1".to_owned(),
                    },
                ],
            })
            .unwrap();
        let initial = crate::WorkspaceRoots::build(&workspace, &configuration, &[]).0;
        engine.initialize_workspace_roots(initial.clone()).unwrap();
        let both = crate::WorkspaceRoots::build(
            &workspace,
            &configuration,
            std::slice::from_ref(&extension),
        )
        .0;
        let plan = engine.workspace_roots_transition_seed(both).unwrap().plan().unwrap();
        let outcome = engine
            .apply_validated_workspace_roots_transition(plan.revalidate().unwrap().unwrap())
            .unwrap();
        assert!(matches!(
            outcome,
            super::WorkspaceRootsTransitionOutcome::Applied {
                pending_overlay_embeddings: false,
                ..
            }
        ));
        let stats = engine.workspace_overlay_stats().unwrap().unwrap();
        assert_eq!(stats.overlay_files, 1);
        assert_eq!(stats.hidden_paths, 1, "the local replacement hides its baseline twin");

        let plan = engine.workspace_roots_transition_seed(initial).unwrap().plan().unwrap();
        engine
            .apply_validated_workspace_roots_transition(plan.revalidate().unwrap().unwrap())
            .unwrap();
        let stats = engine.workspace_overlay_stats().unwrap().unwrap();
        assert_eq!(stats.overlay_files, 0);
        assert_eq!(stats.deleted_files, 1, "the removed baseline key remains hidden");
    }

    #[test]
    fn a_store_failure_keeps_the_old_root_table_and_rows() {
        let dir = tempdir().unwrap();
        let workspace = dir.path().join("ws");
        let configuration = workspace.join("cf");
        let extension = workspace.join("cfe/one");
        write_transition_module(&configuration, "Конфигурация");
        write_transition_module(&extension, "Расширение");

        let mut engine = SearchEngine::fts_only(&dir.path().join("search.db")).unwrap();
        let initial = crate::WorkspaceRoots::build(&workspace, &configuration, &[]).0;
        engine.initialize_workspace_roots(initial.clone()).unwrap();
        engine.index_directory_fts(&configuration).unwrap();
        let next = crate::WorkspaceRoots::build(
            &workspace,
            &configuration,
            std::slice::from_ref(&extension),
        )
        .0;
        let plan = engine.workspace_roots_transition_seed(next).unwrap().plan().unwrap();
        let saboteur = rusqlite::Connection::open(engine.db_path()).unwrap();
        saboteur
            .execute_batch(
                "CREATE TRIGGER deny_transition BEFORE INSERT ON files
                 WHEN NEW.root_id <> '' BEGIN SELECT RAISE(ABORT, 'denied'); END;",
            )
            .unwrap();

        assert!(engine
            .apply_validated_workspace_roots_transition(plan.revalidate().unwrap().unwrap())
            .is_err());
        assert_eq!(engine.workspace_roots(), Some(&initial));
        assert_eq!(engine.file_count().unwrap(), 1);
        assert_eq!(engine.text_search("Конфигурация", 10, Some("code")).unwrap().len(), 1);
    }

    #[test]
    fn a_file_created_between_plan_and_apply_supersedes_the_transition() {
        let dir = tempdir().unwrap();
        let workspace = dir.path().join("ws");
        let configuration = workspace.join("cf");
        let extension = workspace.join("cfe/one");
        write_transition_module(&configuration, "Конфигурация");
        write_transition_module(&extension, "Расширение");

        let mut engine = SearchEngine::fts_only(&dir.path().join("search.db")).unwrap();
        let initial = crate::WorkspaceRoots::build(&workspace, &configuration, &[]).0;
        engine.initialize_workspace_roots(initial.clone()).unwrap();
        engine.index_directory_fts(&configuration).unwrap();
        let next = crate::WorkspaceRoots::build(
            &workspace,
            &configuration,
            std::slice::from_ref(&extension),
        )
        .0;
        let plan = engine.workspace_roots_transition_seed(next).unwrap().plan().unwrap();
        fs::write(extension.join("Created.bsl"), "Процедура Создана()\nКонецПроцедуры").unwrap();

        assert!(
            plan.revalidate().unwrap().is_none(),
            "the created key supersedes the plan before the engine is borrowed"
        );
        assert_eq!(engine.workspace_roots(), Some(&initial));
    }

    #[test]
    fn a_file_deleted_between_plan_and_apply_supersedes_the_transition() {
        let dir = tempdir().unwrap();
        let workspace = dir.path().join("ws");
        let configuration = workspace.join("cf");
        let extension = workspace.join("cfe/one");
        write_transition_module(&configuration, "Конфигурация");
        write_transition_module(&extension, "Расширение");
        let extension_file = extension.join("CommonModules/Один/Ext/Module.bsl");

        let mut engine = SearchEngine::fts_only(&dir.path().join("search.db")).unwrap();
        let initial = crate::WorkspaceRoots::build(&workspace, &configuration, &[]).0;
        engine.initialize_workspace_roots(initial.clone()).unwrap();
        let next = crate::WorkspaceRoots::build(
            &workspace,
            &configuration,
            std::slice::from_ref(&extension),
        )
        .0;
        let plan = engine.workspace_roots_transition_seed(next).unwrap().plan().unwrap();
        fs::remove_file(extension_file).unwrap();

        assert!(
            plan.revalidate().unwrap().is_none(),
            "the deleted key supersedes the plan before the engine is borrowed"
        );
        assert_eq!(engine.workspace_roots(), Some(&initial));
    }

    #[test]
    fn a_removed_root_drops_a_dirty_only_key() {
        let dir = tempdir().unwrap();
        let workspace = dir.path().join("ws");
        let configuration = workspace.join("cf");
        let extension = workspace.join("cfe/one");
        write_transition_module(&configuration, "Конфигурация");
        write_transition_module(&extension, "Расширение");
        let extension_file = extension.join("CommonModules/Один/Ext/Module.bsl");

        let mut engine = SearchEngine::fts_only(&dir.path().join("search.db")).unwrap();
        let both = crate::WorkspaceRoots::build(
            &workspace,
            &configuration,
            std::slice::from_ref(&extension),
        )
        .0;
        engine.initialize_workspace_roots(both).unwrap();
        engine.mark_workspace_path_dirty(&extension_file).unwrap();
        let obsolete = FileKey::new("cfe/one", "CommonModules/Один/Ext/Module.bsl");
        assert!(engine.workspace_overlay_dirty_paths().unwrap().contains(&obsolete));

        let configuration_only = crate::WorkspaceRoots::build(&workspace, &configuration, &[]).0;
        let plan =
            engine.workspace_roots_transition_seed(configuration_only).unwrap().plan().unwrap();
        engine
            .apply_validated_workspace_roots_transition(plan.revalidate().unwrap().unwrap())
            .unwrap();

        assert!(!engine.workspace_overlay_dirty_paths().unwrap().contains(&obsolete));
    }

    #[test]
    fn a_vector_candidate_failure_rolls_back_sql_and_keeps_the_live_index() {
        let dir = tempdir().unwrap();
        let workspace = dir.path().join("ws");
        let configuration = workspace.join("cf");
        let extension = workspace.join("cfe/one");
        write_transition_module(&configuration, "Конфигурация");
        write_transition_module(&extension, "Расширение");

        let mut engine = SearchEngine::fts_only(&dir.path().join("search.db")).unwrap();
        let initial = crate::WorkspaceRoots::build(&workspace, &configuration, &[]).0;
        engine.initialize_workspace_roots(initial.clone()).unwrap();
        engine.index_directory_fts(&configuration).unwrap();
        let vectors_before = engine.vector_count();
        let next = crate::WorkspaceRoots::build(
            &workspace,
            &configuration,
            std::slice::from_ref(&extension),
        )
        .0;
        let plan = engine.workspace_roots_transition_seed(next).unwrap().plan().unwrap();

        crate::store::FORCE_WORKSPACE_TRANSITION_VECTOR_ERROR.with(|flag| flag.set(true));
        let result =
            engine.apply_validated_workspace_roots_transition(plan.revalidate().unwrap().unwrap());
        crate::store::FORCE_WORKSPACE_TRANSITION_VECTOR_ERROR.with(|flag| flag.set(false));

        assert!(result.is_err());
        assert_eq!(engine.workspace_roots(), Some(&initial));
        assert_eq!(engine.vector_count(), vectors_before);
        assert_eq!(engine.file_count().unwrap(), 1, "the inserted extension row was rolled back");
        assert!(engine.text_search("Расширение", 10, Some("code")).unwrap().is_empty());
    }

    /// The erased operation an `apply` receives. Named because the nested `dyn FnMut` is
    /// past the inline-complexity limit, and an `allow` would only silence the measure.
    type FencedOperation<'a> = &'a mut dyn FnMut(
        &mut dyn FnMut() -> ControlFlow<()>,
    ) -> ControlFlow<(), Result<(), SearchError>>;

    /// The short budget belongs to the OPEN, not to the store the open returns. That store is
    /// the daemon's for the rest of its life and shares its database with the embedding pass,
    /// which holds the WAL writer far longer than an admission may wait — a live write left on
    /// the admission budget would start failing where it used to wait and succeed.
    ///
    /// The refusal on the short budget, measured in the same held window, is the control: it
    /// proves the peer's lock really does block writes and that 100 ms really is too little, so
    /// the engine's success below is a property of its budget and not of an idle database.
    #[test]
    fn a_fenced_open_leaves_the_engine_on_the_operational_budget() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("live.db");
        let root = dir.path().join("cf");
        fs::create_dir_all(root.join("CommonModules/А/Ext")).unwrap();
        fs::write(
            root.join("CommonModules/А/Ext/Module.bsl"),
            "Процедура А() Экспорт\nКонецПроцедуры\n",
        )
        .unwrap();
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();

        let holder_path = db_path.clone();
        let (locked, wait_for_lock) = std::sync::mpsc::channel();
        let (control_done, wait_for_control) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            let conn = rusqlite::Connection::open(&holder_path).unwrap();
            conn.busy_timeout(std::time::Duration::from_millis(0)).unwrap();
            conn.execute_batch("BEGIN IMMEDIATE").unwrap();
            locked.send(()).unwrap();
            // Held until the control has run — otherwise a release that slipped in first would
            // let the control succeed and the test would pass on an idle database.
            wait_for_control.recv().unwrap();
            std::thread::sleep(std::time::Duration::from_millis(300));
            conn.execute_batch("COMMIT").unwrap();
        });
        wait_for_lock.recv().unwrap();

        let impatient = rusqlite::Connection::open(&db_path).unwrap();
        impatient.busy_timeout(crate::store::FENCED_OPEN_BUSY_TIMEOUT).unwrap();
        assert!(
            impatient.execute_batch("BEGIN IMMEDIATE").is_err(),
            "control: the peer's write lock must actually refuse the admission budget"
        );
        control_done.send(()).unwrap();

        let indexed = engine.index_directory_fts(&root);
        holder.join().unwrap();
        assert!(
            indexed.is_ok(),
            "a live write waits the peer out on the operational budget: {indexed:?}"
        );
    }

    /// The budget an admission may spend waiting on SQLite is the point of the split. Under the
    /// fence a wait is an interprocess lock and the lease's lifecycle mutex held for its whole
    /// length, so a peer's write transaction must be waited out ACROSS admissions, never inside
    /// one. Whether it was is a COUNT, not a duration: the peer holds until the open comes back
    /// for a second admission, so an open that waited inside the first one never arrives and the
    /// count stays at one. Nothing here compares wall-clock readings, which on a loaded machine
    /// measure the scheduler as much as the code.
    ///
    /// The peer is pre-created and already migrated so the contention is the writer reservation
    /// `migrate_structural_schema` takes on every open — a `journal_mode` switch is refused
    /// outright instead of waited out, which would measure the retry rather than the wait.
    #[test]
    fn a_contended_fenced_open_never_spans_the_contention_in_one_admission() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("contended.db");
        drop(crate::store::Store::open(&db_path).unwrap());

        let admissions = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = std::sync::Arc::clone(&admissions);
        let holder_path = db_path.clone();
        let (locked, wait_for_lock) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            let conn = rusqlite::Connection::open(&holder_path).unwrap();
            conn.busy_timeout(std::time::Duration::from_millis(0)).unwrap();
            conn.execute_batch("BEGIN IMMEDIATE").unwrap();
            locked.send(()).unwrap();
            // Held until the open has been admitted twice, so the release is caused by the
            // behaviour under test rather than by a clock. The deadline only keeps a failing
            // build from hanging: an open that never comes back releases on it and fails below.
            let give_up = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while observed.load(std::sync::atomic::Ordering::SeqCst) < 2
                && std::time::Instant::now() < give_up
            {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            conn.execute_batch("COMMIT").unwrap();
        });
        wait_for_lock.recv().unwrap();

        let opened = {
            let mut apply = |operation: FencedOperation<'_>| {
                admissions.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut checkpoint = || ControlFlow::Continue(());
                match operation(&mut checkpoint) {
                    ControlFlow::Continue(value) => FenceOutcome::Applied(value),
                    ControlFlow::Break(()) => FenceOutcome::Released,
                }
            };
            SearchEngine::open_store_fenced(&db_path, "test", &mut apply).unwrap()
        };
        holder.join().unwrap();

        assert!(
            matches!(opened, FenceOutcome::Applied(_)),
            "the open still succeeds once the peer commits"
        );
        assert!(
            admissions.load(std::sync::atomic::Ordering::SeqCst) >= 2,
            "the peer was waited out inside a single admission: it took {} of them",
            admissions.load(std::sync::atomic::Ordering::SeqCst)
        );
    }

    /// Losing the bootstrap race must cost admissions, not one long-held admission. The fence
    /// carries both the interprocess lock and the lease's lifecycle mutex, so a retry loop that
    /// slept inside a single admission would stall shutdown and every peer claim for the whole
    /// backoff. Counting admissions is what tells the two apart: waiting inside the fence shows
    /// up as one, waiting outside it as one per attempt.
    ///
    /// The second half is the control. Without it the assertion would also hold for a fence that
    /// refused to admit anything at all, and for an open that never retried.
    #[test]
    fn each_bootstrap_retry_is_its_own_admission() {
        fn count_admissions(db_path: &Path, forced_retries: u32) -> (usize, bool) {
            crate::store::FORCE_BOOTSTRAP_RETRIES.with(|left| left.set(forced_retries));
            let mut admissions = 0usize;
            let opened = {
                let mut apply = |operation: FencedOperation<'_>| {
                    admissions += 1;
                    let mut checkpoint = || ControlFlow::Continue(());
                    match operation(&mut checkpoint) {
                        ControlFlow::Continue(result) => FenceOutcome::Applied(result),
                        ControlFlow::Break(()) => FenceOutcome::Released,
                    }
                };
                SearchEngine::open_store_fenced(db_path, "test", &mut apply).unwrap()
            };
            crate::store::FORCE_BOOTSTRAP_RETRIES.with(|left| left.set(0));
            (admissions, matches!(opened, FenceOutcome::Applied(_)))
        }

        let dir = tempdir().unwrap();

        let (admissions, opened) = count_admissions(&dir.path().join("raced.db"), 2);
        assert!(opened, "the open still succeeds once the race is over");
        assert_eq!(
            admissions, 3,
            "two lost races and the winning open are three admissions, not one held across both \
             backoffs"
        );

        let (admissions, opened) = count_admissions(&dir.path().join("clean.db"), 0);
        assert!(opened, "an unraced open succeeds");
        assert_eq!(admissions, 1, "an unraced open costs exactly one admission");
    }

    /// Staging releases the engine lock so its scan does not hold the lease, so the commit runs
    /// against a cache other threads have been free to change. The transition must MERGE into
    /// what it finds, never install a picture taken before the window — and the class of things
    /// the window can admit is open, so the test names two members that fail differently: a
    /// point mark, which lives in the dirty map, and a settled overlay entry, which lives in
    /// `entries` and answers searches. A commit that replaced the cache wholesale would lose
    /// each of them, and a guard that only compared dirty marks would still lose the second.
    ///
    /// The transition itself applying is the control: an assertion about what SURVIVES a
    /// commit is satisfied trivially by a commit that never happens.
    #[test]
    fn work_admitted_in_the_commit_window_survives_the_root_transition() {
        let dir = tempdir().unwrap();
        let workspace = dir.path().join("ws");
        let configuration = workspace.join("cf");
        let extension = workspace.join("cfe/one");
        write_transition_module(&configuration, "Конфигурация");
        let extension_file = extension.join("CommonModules/Один/Ext/Module.bsl");
        fs::create_dir_all(extension_file.parent().unwrap()).unwrap();
        fs::write(&extension_file, "Процедура Расширение() Экспорт\nКонецПроцедуры\n").unwrap();

        let mut engine = SearchEngine::fts_only(&dir.path().join("search.db")).unwrap();
        let initial = crate::WorkspaceRoots::build(&workspace, &configuration, &[]).0;
        engine.initialize_workspace_roots(initial).unwrap();
        engine.index_directory_fts(&configuration).unwrap();
        // The overlay is inert until initialized, and a settled entry is the point of the second
        // half of this test.
        engine.initialize_workspace_overlay_clean().unwrap();

        let next = crate::WorkspaceRoots::build(
            &workspace,
            &configuration,
            std::slice::from_ref(&extension),
        )
        .0;
        let validated = engine
            .workspace_roots_transition_seed(next)
            .unwrap()
            .plan()
            .unwrap()
            .revalidate()
            .unwrap()
            .unwrap();
        let mut staged =
            engine.stage_validated_workspace_roots_transition(validated).unwrap().unwrap();

        // The window. Both edits are to the CONFIGURATION root, which this transition does not
        // touch: it only adds an extension root.
        let settled = configuration.join("Улаженный.bsl");
        fs::write(&settled, "Процедура Улажена()\nКонецПроцедуры\n").unwrap();
        assert!(engine.mark_workspace_path_dirty(&settled).unwrap());
        crate::point_refresh::point_refresh_for_test(&engine).expect("a dirty key to refresh");
        assert_eq!(
            engine.text_search("Улажена", 10, Some("code")).unwrap().len(),
            1,
            "the settled entry answers before the commit"
        );

        // Marked AFTER the settle: a refresh reads every dirty path it finds, not only the ones
        // it was handed snapshots for, so a mark made earlier would be settled by the call above
        // and the first half of this test would prove nothing.
        let marked = configuration.join("Помеченный.bsl");
        fs::write(&marked, "Процедура Помечена()\nКонецПроцедуры\n").unwrap();
        assert!(engine.mark_workspace_path_dirty(&marked).unwrap());
        let marked_key = FileKey::configuration("Помеченный.bsl");

        let mut permit = || ControlFlow::Continue(());
        let outcome = engine.apply_staged_workspace_roots_transition(&mut staged, &mut permit);
        assert!(
            matches!(
                outcome,
                ControlFlow::Continue(Ok(super::WorkspaceRootsTransitionOutcome::Applied { .. }))
            ),
            "the transition still commits: {outcome:?}"
        );
        assert_eq!(
            engine.workspace_roots().map(|roots| roots.ids().count()),
            Some(2),
            "the control: the extension root really was installed"
        );
        // The transition is one of the index-write paths: the spellings it published are
        // recorded beside their keys, so a removal arriving as that path finds it
        // (github#192).
        let extension_key = engine
            .workspace_file_key(&extension_file)
            .expect("the transition published the extension's module");
        let declared_spelling = engine
            .workspace_roots()
            .expect("the transition installed its roots")
            .resolve(&extension_key)
            .expect("the key names a file");
        assert_eq!(
            engine.store().path_spelling_key(&declared_spelling.to_string_lossy(), "code").unwrap(),
            Some(extension_key),
            "the transition records the spellings of the files it published",
        );

        assert!(
            engine.workspace_overlay_dirty_paths().unwrap().contains(&marked_key),
            "a mark admitted after staging survives the commit"
        );
        assert_eq!(
            engine.text_search("Улажена", 10, Some("code")).unwrap().len(),
            1,
            "an overlay entry settled after staging survives the commit"
        );
    }

    #[test]
    fn cancelled_root_transition_publishes_nothing_and_retries_the_same_staging() {
        let dir = tempdir().unwrap();
        let workspace = dir.path().join("ws");
        let configuration = workspace.join("cf");
        let extension = workspace.join("cfe/one");
        write_transition_module(&configuration, "Конфигурация");
        let extension_file = extension.join("CommonModules/Один/Ext/Module.bsl");
        fs::create_dir_all(extension_file.parent().unwrap()).unwrap();
        let content = (0..=WORKSPACE_APPLY_BATCH_ROWS)
            .map(|index| format!("Процедура Расширение{index}() Экспорт\nКонецПроцедуры\n"))
            .collect::<String>();
        fs::write(extension_file, content).unwrap();

        let mut engine = SearchEngine::fts_only(&dir.path().join("search.db")).unwrap();
        let initial = crate::WorkspaceRoots::build(&workspace, &configuration, &[]).0;
        engine.initialize_workspace_roots(initial.clone()).unwrap();
        engine.index_directory_fts(&configuration).unwrap();
        let roots_before = engine.workspace_roots().cloned();
        let files_before = engine.file_count().unwrap();
        let vectors_before = engine.vector_count();
        let cache_before = engine.workspace_overlay_stats().unwrap();
        let next = crate::WorkspaceRoots::build(
            &workspace,
            &configuration,
            std::slice::from_ref(&extension),
        )
        .0;
        let validated = engine
            .workspace_roots_transition_seed(next)
            .unwrap()
            .plan()
            .unwrap()
            .revalidate()
            .unwrap()
            .unwrap();
        let mut staged =
            engine.stage_validated_workspace_roots_transition(validated).unwrap().unwrap();

        let mut checkpoints = 0;
        let outcome = engine.apply_staged_workspace_roots_transition(&mut staged, &mut || {
            checkpoints += 1;
            if checkpoints == 2 {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        });
        assert!(outcome.is_break());
        assert_eq!(checkpoints, 2, "the transaction checks again after one 64-row slice");
        assert_eq!(engine.workspace_roots(), roots_before.as_ref());
        assert_eq!(engine.file_count().unwrap(), files_before);
        assert_eq!(engine.vector_count(), vectors_before);
        assert_eq!(engine.workspace_overlay_stats().unwrap(), cache_before);
        assert!(engine.text_search("Расширение64", 10, Some("code")).unwrap().is_empty());

        let mut permit = || ControlFlow::Continue(());
        assert!(matches!(
            engine.apply_staged_workspace_roots_transition(&mut staged, &mut permit),
            ControlFlow::Continue(Ok(super::WorkspaceRootsTransitionOutcome::Applied { .. }))
        ));
        assert_eq!(engine.file_count().unwrap(), files_before + 1);
        assert_eq!(engine.text_search("Расширение64", 10, Some("code")).unwrap().len(), 1);
    }

    #[test]
    fn compatibility_root_replacement_keeps_serving_when_the_scan_is_incomplete() {
        let dir = tempdir().unwrap();
        let workspace = dir.path().join("ws");
        let configuration = workspace.join("cf");
        write_transition_module(&configuration, "Конфигурация");

        let mut engine = SearchEngine::fts_only(&dir.path().join("search.db")).unwrap();
        let initial = crate::WorkspaceRoots::build(&workspace, &configuration, &[]).0;
        engine.initialize_workspace_roots(initial.clone()).unwrap();
        engine.index_directory_fts(&configuration).unwrap();
        let missing = workspace.join("missing-extension");
        let next = crate::WorkspaceRoots::build(
            &workspace,
            &configuration,
            std::slice::from_ref(&missing),
        )
        .0;

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            engine.set_workspace_roots(next);
        }));

        assert!(result.is_ok(), "the compatibility path must not panic on a live failure");
        assert_eq!(engine.workspace_roots(), Some(&initial));
        assert_eq!(engine.text_search("Конфигурация", 10, Some("code")).unwrap().len(), 1);
    }

    #[test]
    fn a_baseline_mode_change_supersedes_a_prepared_root_plan() {
        let dir = tempdir().unwrap();
        let workspace = dir.path().join("ws");
        let configuration = workspace.join("cf");
        let extension = workspace.join("cfe/one");
        write_transition_module(&configuration, "Конфигурация");
        write_transition_module(&extension, "Расширение");

        let mut engine = SearchEngine::fts_only(&dir.path().join("search.db")).unwrap();
        engine.set_serves_external_baseline(true).unwrap();
        let initial = crate::WorkspaceRoots::build(&workspace, &configuration, &[]).0;
        engine.initialize_workspace_roots(initial.clone()).unwrap();
        let next = crate::WorkspaceRoots::build(
            &workspace,
            &configuration,
            std::slice::from_ref(&extension),
        )
        .0;
        let plan = engine.workspace_roots_transition_seed(next).unwrap().plan().unwrap();
        engine.set_serves_external_baseline(false).unwrap();

        assert_eq!(
            engine
                .apply_validated_workspace_roots_transition(plan.revalidate().unwrap().unwrap())
                .unwrap(),
            super::WorkspaceRootsTransitionOutcome::Superseded
        );
        assert_eq!(engine.workspace_roots(), Some(&initial));
        assert!(engine.text_search("Расширение", 10, Some("code")).unwrap().is_empty());
    }

    #[test]
    fn a_manifest_change_supersedes_a_prepared_root_plan() {
        let dir = tempdir().unwrap();
        let workspace = dir.path().join("ws");
        let configuration = workspace.join("cf");
        let extension = workspace.join("cfe/one");
        write_transition_module(&configuration, "Конфигурация");
        write_transition_module(&extension, "Расширение");
        let relative = "CommonModules/Один/Ext/Module.bsl";

        let mut engine = SearchEngine::fts_only(&dir.path().join("search.db")).unwrap();
        engine.set_serves_external_baseline(true).unwrap();
        engine
            .store()
            .save_baseline_manifest(&crate::WorkspaceBaselineManifest {
                snapshot_id: "snap-before".to_owned(),
                snapshot_fingerprint: Some("before".to_owned()),
                files: Vec::new(),
            })
            .unwrap();
        let initial = crate::WorkspaceRoots::build(&workspace, &configuration, &[]).0;
        engine.initialize_workspace_roots(initial.clone()).unwrap();
        let next = crate::WorkspaceRoots::build(
            &workspace,
            &configuration,
            std::slice::from_ref(&extension),
        )
        .0;
        let plan = engine.workspace_roots_transition_seed(next).unwrap().plan().unwrap();

        engine
            .store()
            .save_baseline_manifest(&crate::WorkspaceBaselineManifest {
                snapshot_id: "snap-after".to_owned(),
                snapshot_fingerprint: Some("after".to_owned()),
                files: vec![crate::BaselineManifestFile {
                    root_id: "cfe/one".to_owned(),
                    collection: "code".to_owned(),
                    path: relative.to_owned(),
                    file_fingerprint: "remote-version".to_owned(),
                    document_count: 1,
                    file_object_id: "obj-extension".to_owned(),
                }],
            })
            .unwrap();

        assert_eq!(
            engine
                .apply_validated_workspace_roots_transition(plan.revalidate().unwrap().unwrap())
                .unwrap(),
            super::WorkspaceRootsTransitionOutcome::Superseded
        );
        assert_eq!(engine.workspace_roots(), Some(&initial));
        assert_eq!(engine.workspace_overlay_stats().unwrap().unwrap().overlay_files, 0);
    }

    #[test]
    fn a_semantic_source_change_supersedes_a_prepared_root_plan() {
        struct Provider;
        impl crate::ports::GraphContextProvider for Provider {
            fn graph_context(
                &self,
                _rel_path: &str,
                _symbol_name: &str,
                _kind: &str,
            ) -> Option<String> {
                Some("new graph".to_owned())
            }
        }

        let dir = tempdir().unwrap();
        let workspace = dir.path().join("ws");
        let configuration = workspace.join("cf");
        let extension = workspace.join("cfe/one");
        write_transition_module(&configuration, "Конфигурация");
        write_transition_module(&extension, "Расширение");
        let mut engine = SearchEngine::fts_only(&dir.path().join("search.db")).unwrap();
        let initial = crate::WorkspaceRoots::build(&workspace, &configuration, &[]).0;
        engine.initialize_workspace_roots(initial.clone()).unwrap();
        let next = crate::WorkspaceRoots::build(
            &workspace,
            &configuration,
            std::slice::from_ref(&extension),
        )
        .0;
        let plan = engine.workspace_roots_transition_seed(next).unwrap().plan().unwrap();
        engine.set_graph_context_provider(Arc::new(Provider));

        assert_eq!(
            engine
                .apply_validated_workspace_roots_transition(plan.revalidate().unwrap().unwrap())
                .unwrap(),
            super::WorkspaceRootsTransitionOutcome::Superseded
        );
        assert_eq!(engine.workspace_roots(), Some(&initial));
    }

    #[test]
    fn reference_collection_replace_is_atomic_stamped_and_idempotent() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("reference-search.db");
        let old = [Document {
            title: "Old".to_owned(),
            body: "olduniquemarker".to_owned(),
            kind: "type".to_owned(),
        }];
        let new = [
            Document {
                title: "New".to_owned(),
                body: "newuniquemarker".to_owned(),
                kind: "type".to_owned(),
            },
            Document {
                title: "Property".to_owned(),
                body: "property marker".to_owned(),
                kind: "property".to_owned(),
            },
        ];
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        assert!(
            engine
                .replace_reference_collection_if_stale(
                    "platform",
                    "platform://docs",
                    "fp-old",
                    &old,
                    None
                )
                .unwrap()
                .written
        );
        assert!(
            engine
                .replace_reference_collection_if_stale(
                    "platform",
                    "platform://docs",
                    "fp-new",
                    &new,
                    None
                )
                .unwrap()
                .written
        );
        assert!(
            !engine
                .replace_reference_collection_if_stale(
                    "platform",
                    "platform://docs",
                    "fp-new",
                    &new,
                    None
                )
                .unwrap()
                .written
        );
        assert!(engine.text_search("olduniquemarker", 10, Some("platform")).unwrap().is_empty());
        assert_eq!(engine.text_search("newuniquemarker", 10, Some("platform")).unwrap().len(), 1);
        assert_eq!(engine.load_indexed_documents(Some("platform")).unwrap().len(), 2);
        let integrity: String = rusqlite::Connection::open(&db_path)
            .unwrap()
            .query_row("PRAGMA integrity_check", [], |row| row.get(0))
            .unwrap();
        assert_eq!(integrity, "ok");
    }

    /// Метка корпуса называет эмбеддер, которым он записан.
    ///
    /// Иначе корпус, записанный демоном без эмбеддера, совпадает по метке с тем,
    /// что нужен демону с эмбеддером: ранний выход считает его свежим, векторов у
    /// коллекции не появляется никогда, а семантический поиск по справке молча пуст.
    #[test]
    fn a_corpus_written_without_an_embedder_is_stamped_as_such() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("reference-search.db");
        let documents = [Document {
            title: "Массив".to_owned(),
            body: "uniquemarker".to_owned(),
            kind: "type".to_owned(),
        }];

        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        assert!(
            engine
                .replace_reference_collection_if_stale(
                    "platform",
                    "platform://docs",
                    "fp",
                    &documents,
                    None
                )
                .unwrap()
                .written
        );
        assert!(
            !engine
                .replace_reference_collection_if_stale(
                    "platform",
                    "platform://docs",
                    "fp",
                    &documents,
                    None
                )
                .unwrap()
                .written,
            "тот же движок и тот же корпус второй раз не переписываются"
        );

        assert_eq!(
            engine.store.reference_collection_fingerprint("platform").unwrap().as_deref(),
            Some(SearchEngine::reference_stamp("fp", None, None).as_str()),
            "записанная метка обязана называть отсутствие эмбеддера"
        );
        assert_ne!(
            SearchEngine::reference_stamp("fp", None, None),
            SearchEngine::reference_stamp("fp", Some("bge-m3"), Some(1024)),
            "корпус без векторов не годится движку, который умеет векторы"
        );
        assert_ne!(
            SearchEngine::reference_stamp("fp", Some("bge-m3"), Some(1024)),
            SearchEngine::reference_stamp("fp", Some("e5-large"), Some(1024)),
            "векторы чужой модели не годятся"
        );
        assert_ne!(
            SearchEngine::reference_stamp("fp", Some("bge-m3"), Some(1024)),
            SearchEngine::reference_stamp("fp", Some("bge-m3"), Some(512)),
            "векторы другой размерности не годятся"
        );
    }

    /// Недоступный эмбеддер отнимает у справки семантику, но не лексику.
    ///
    /// Отмечен `ignore`: живого места для внедрения отказа нет, а настоящий недоступный
    /// эмбеддер отрабатывает десять попыток с нарастающей паузой — около трёх минут.
    /// Запускать точечно: `cargo test -p bsl-search --lib -- --ignored lexical`.
    #[test]
    #[ignore = "недоступный эмбеддер ретраится ~160 с; запускать точечно"]
    fn an_unreachable_embedder_still_publishes_the_corpus_lexically() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("reference-search.db");
        let documents = [Document {
            title: "Массив".to_owned(),
            body: "uniquemarker".to_owned(),
            kind: "type".to_owned(),
        }];

        let config = crate::SearchConfig {
            embedder: crate::EmbedderConfig {
                // Порт 1 закрыт: соединение отвергается сразу, отказ настоящий.
                base_url: "http://127.0.0.1:1".to_owned(),
                model: "unreachable-model".to_owned(),
                dim: Some(8),
                ..Default::default()
            },
            ..Default::default()
        };
        let mut engine = SearchEngine::new(&db_path, config).unwrap();

        let outcome = engine
            .replace_reference_collection_if_stale(
                "platform",
                "platform://docs",
                "fp",
                &documents,
                None,
            )
            .expect("отказ эмбеддера не отменяет публикацию корпуса");

        assert!(outcome.written);
        assert_eq!(engine.text_search("uniquemarker", 10, Some("platform")).unwrap().len(), 1);
        assert_eq!(
            engine.store.reference_collection_fingerprint("platform").unwrap().as_deref(),
            Some(SearchEngine::reference_stamp("fp", None, None).as_str()),
            "корпус без векторов обязан быть помечен как таковой — иначе рабочий эмбеддер \
             никогда его не переберёт"
        );
    }

    /// Запись в коллекцию мимо штампующего пути метку снимает.
    ///
    /// В коллекцию `platform` пишет не только публикация локального корпуса: ветка
    /// внешнего справочного базиса синхронизирует туда чужой снимок и удаляет
    /// локальный документ. Уцелевшая метка после такой записи говорит о корпусе,
    /// которого в коллекции уже нет, и следующая публикация молча не состоится.
    #[test]
    fn a_write_past_the_stamping_path_drops_the_stamp() {
        let dir = tempdir().unwrap();
        let documents = [Document {
            title: "Массив".to_owned(),
            body: "uniquemarker".to_owned(),
            kind: "type".to_owned(),
        }];

        for foreign in ["sync", "remove"] {
            let db_path = dir.path().join(format!("reference-{foreign}.db"));
            let mut engine = SearchEngine::fts_only(&db_path).unwrap();
            assert!(
                engine
                    .replace_reference_collection_if_stale(
                        "platform",
                        "platform://docs",
                        "fp",
                        &documents,
                        None
                    )
                    .unwrap()
                    .written
            );

            match foreign {
                "sync" => {
                    engine.sync_indexed_documents_in_collection("platform", &[], None).unwrap();
                }
                _ => engine.remove_file("platform://docs", "platform").unwrap(),
            }

            assert_eq!(
                engine.store.reference_collection_fingerprint("platform").unwrap(),
                None,
                "после записи мимо штампующего пути ({foreign}) метка обязана исчезнуть"
            );
            assert!(
                engine
                    .replace_reference_collection_if_stale(
                        "platform",
                        "platform://docs",
                        "fp",
                        &documents,
                        None
                    )
                    .unwrap()
                    .written,
                "тот же корпус обязан быть записан заново ({foreign})"
            );
            assert_eq!(engine.text_search("uniquemarker", 10, Some("platform")).unwrap().len(), 1);
        }
    }

    #[test]
    fn concurrent_reference_writers_commit_one_generation() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("reference-search.db");
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let mut handles = Vec::new();
        let engines =
            [SearchEngine::fts_only(&db_path).unwrap(), SearchEngine::fts_only(&db_path).unwrap()];
        for mut engine in engines {
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                let documents = [Document {
                    title: "Shared".to_owned(),
                    body: "shared generation marker".to_owned(),
                    kind: "type".to_owned(),
                }];
                barrier.wait();
                let outcome = engine
                    .replace_reference_collection_if_stale(
                        "platform",
                        "platform://docs",
                        "fp-shared",
                        &documents,
                        None,
                    )
                    .unwrap();
                let hits =
                    engine.text_search("shared generation marker", 10, Some("platform")).unwrap();
                (outcome, hits.len(), engine.loaded_reference_fingerprint.clone())
            }));
        }
        let outcomes: Vec<_> = handles.into_iter().map(|handle| handle.join().unwrap()).collect();
        let stamp = SearchEngine::reference_stamp("fp-shared", None, None);
        assert_eq!(outcomes.iter().filter(|(outcome, _, _)| outcome.written).count(), 1);
        assert!(outcomes.iter().all(|(outcome, hits, loaded)| {
            outcome.committed_fingerprint == stamp
                && *hits == 1
                && loaded.as_deref() == Some(stamp.as_str())
        }));
    }

    #[test]
    fn reference_collection_publish_is_process_safe_for_absent_and_stale_db() {
        const CHILD: &str = "BSL_SEARCH_REFERENCE_WRITER_CHILD";
        if let Ok(worker) = std::env::var(CHILD) {
            let root = std::path::PathBuf::from(
                std::env::var("BSL_SEARCH_REFERENCE_WRITER_ROOT").unwrap(),
            );
            let ready = root.join(format!("ready-{worker}"));
            fs::write(&ready, b"ready").unwrap();
            let start = root.join("start");
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while !start.exists() {
                assert!(std::time::Instant::now() < deadline, "parent never released barrier");
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            let documents = [Document {
                title: "ProcessSafe".to_owned(),
                body: "processsafemarker".to_owned(),
                kind: "type".to_owned(),
            }];
            let mut engine = SearchEngine::fts_only(&root.join("reference-search.db")).unwrap();
            let outcome = engine
                .replace_reference_collection_if_stale(
                    "platform",
                    "platform://docs",
                    "fp-process-safe",
                    &documents,
                    None,
                )
                .unwrap();
            let count = engine.load_indexed_documents(Some("platform")).unwrap().len();
            fs::write(
                root.join(format!("result-{worker}")),
                format!("{}:{count}:{}", outcome.written, outcome.committed_fingerprint),
            )
            .unwrap();
            return;
        }

        for stale in [false, true] {
            let dir = tempdir().unwrap();
            let root = dir.path();
            if stale {
                let mut engine = SearchEngine::fts_only(&root.join("reference-search.db")).unwrap();
                engine
                    .replace_reference_collection_if_stale(
                        "platform",
                        "platform://docs",
                        "fp-stale",
                        &[Document {
                            title: "Stale".to_owned(),
                            body: "stalemarker".to_owned(),
                            kind: "type".to_owned(),
                        }],
                        None,
                    )
                    .unwrap();
            }
            let exe = std::env::current_exe().unwrap();
            let mut children = Vec::new();
            for worker in ["a", "b"] {
                children.push(
                    std::process::Command::new(&exe)
                        .args([
                            "--exact",
                            "engine::tests::reference_collection_publish_is_process_safe_for_absent_and_stale_db",
                        ])
                        .env(CHILD, worker)
                        .env("BSL_SEARCH_REFERENCE_WRITER_ROOT", root)
                        .spawn()
                        .unwrap(),
                );
            }
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while !(root.join("ready-a").exists() && root.join("ready-b").exists()) {
                assert!(std::time::Instant::now() < deadline, "writers never reached barrier");
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            fs::write(root.join("start"), b"start").unwrap();
            for child in &mut children {
                assert!(child.wait().unwrap().success());
            }
            let results: Vec<_> = ["a", "b"]
                .map(|worker| fs::read_to_string(root.join(format!("result-{worker}"))).unwrap())
                .into_iter()
                .collect();
            assert_eq!(results.iter().filter(|result| result.starts_with("true:")).count(), 1);
            let expected =
                format!(":1:{}", SearchEngine::reference_stamp("fp-process-safe", None, None));
            assert!(results.iter().all(|result| result.ends_with(&expected)));

            let connection = rusqlite::Connection::open(root.join("reference-search.db")).unwrap();
            assert_eq!(
                connection
                    .query_row::<String, _, _>("PRAGMA integrity_check", [], |row| row.get(0))
                    .unwrap(),
                "ok"
            );
            let chunks: i64 =
                connection.query_row("SELECT COUNT(*) FROM chunks", [], |row| row.get(0)).unwrap();
            let fts: i64 = connection
                .query_row("SELECT COUNT(*) FROM chunks_fts", [], |row| row.get(0))
                .unwrap();
            assert_eq!((chunks, fts), (1, 1));
        }
    }
}

#[cfg(test)]
mod indexing_pass_lifecycle {
    use super::*;

    #[test]
    fn empty_owned_work_waits_for_publication_and_keeps_typed_terminal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.db");
        let store = Store::open(&path).unwrap();
        let config = SearchConfig::default();
        let progress = IndexProgress::new();
        let mut owner = progress.begin_pass();
        let token = owner.token();
        let result = SearchEngine::embed_pending_chunks_fenced_retrying_owned(
            &path,
            &config,
            Some(&token),
            None,
            |op| FenceOutcome::Applied(op()),
            || false,
        )
        .unwrap();
        assert!(matches!(result, FenceOutcome::Applied(_)));
        let snapshot = progress.snapshot().unwrap();
        assert_eq!(snapshot.state, IndexPassState::Running);
        assert_eq!(snapshot.phase, Some(IndexPhase::Persisting));
        assert!(snapshot.counters.is_none());
        owner.finish(IndexPassState::Ready);
        assert_eq!(progress.snapshot().unwrap().state, IndexPassState::Ready);
        let mut owner = progress.begin_pass();
        let token = owner.token();
        let embedder = Embedder::new(config.embedder);
        let result = SearchEngine::run_embedding_pass_owned(
            &store,
            &embedder,
            3,
            1,
            1,
            Some(&token),
            Some(&|| false),
            &mut |op| FenceOutcome::Applied(op()),
            None,
        )
        .unwrap();
        assert!(matches!(result.1, FenceOutcome::Released));
        owner.finish(IndexPassState::Ready);
        assert_eq!(progress.snapshot().unwrap().state, IndexPassState::Cancelled);
    }

    #[test]
    fn best_effort_persistence_keeps_serving_but_never_reports_ready() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("search.db");
        let mut store = Store::open(&path).unwrap();
        let chunk = crate::Chunk {
            kind: crate::ChunkKind::Procedure,
            name: "One".into(),
            is_export: false,
            annotations: Vec::new(),
            line_start: 1,
            line_end: 2,
            text: "Процедура One()\nКонецПроцедуры".into(),
        };
        store.reindex_file("", "M.bsl", b"hash", &[chunk], Some(&[vec![1.0, 0.0, 0.0]])).unwrap();
        // A directory at the publication destination deterministically refuses the rename.
        std::fs::create_dir(path.with_file_name("search.db.usearch")).unwrap();
        std::fs::write(path.with_file_name("search.db.usearch").join("occupied"), b"fixture")
            .unwrap();
        let progress = IndexProgress::new();
        let mut owner = progress.begin_pass();
        let embedder = Embedder::new(EmbedderConfig { dim: Some(3), ..EmbedderConfig::default() });
        let index = SearchEngine::build_persisted_index_with_progress(
            &store,
            3,
            Some(&embedder),
            Some(&owner.token()),
        )
        .unwrap();
        assert_eq!(index.len(), 1);
        owner.finish(IndexPassState::Ready);
        assert_eq!(progress.snapshot().unwrap().state, IndexPassState::Failed);
        assert!(!progress.is_active());
    }

    #[test]
    fn failed_native_store_read_is_retained_after_guard_cleanup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.db");
        let progress = IndexProgress::new();
        let owner = progress.begin_pass();
        let token = owner.token();
        assert!(SearchEngine::embed_pending_chunks_fenced_retrying_owned(
            &path,
            &SearchConfig::default(),
            Some(&token),
            None,
            |op| FenceOutcome::Applied(op()),
            || false,
        )
        .is_err());
        drop(owner);
        assert_eq!(progress.snapshot().unwrap().state, IndexPassState::Failed);
    }
}

#[cfg(test)]
mod indexing_local_qualification {
    use super::*;
    use sha2::Digest;
    use std::io::{Read, Write};

    fn config(dim: usize) -> SearchConfig {
        SearchConfig {
            embedder: EmbedderConfig {
                dim: Some(dim),
                base_url: "http://127.0.0.1:0".into(),
                ..EmbedderConfig::default()
            },
            ..SearchConfig::default()
        }
    }
    fn seed(store: &mut Store, vector: Option<Vec<f32>>) {
        let chunk = crate::Chunk {
            kind: crate::ChunkKind::Procedure,
            name: "One".into(),
            is_export: false,
            annotations: Vec::new(),
            line_start: 1,
            line_end: 2,
            text: "Процедура One()\nКонецПроцедуры".into(),
        };
        let vectors = vector.map(|vector| vec![vector]);
        store.reindex_file("", "M.bsl", b"hash", &[chunk], vectors.as_deref()).unwrap();
    }
    fn observe(engine: &SearchEngine) {
        engine.observe_semantic_boot_coverage(
            engine.chunk_count().ok(),
            engine.embedding_count_by_collection("code").ok(),
        );
    }
    #[test]
    fn cold_empty_failed_counts_and_generation_qualified_publication() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("search.db");
        let mut engine = SearchEngine::new(&path, config(3)).unwrap();
        observe(&engine);
        assert!(engine.semantic_index_qualification().identity_verified);
        assert!(engine.semantic_index_qualification().coverage_complete);
        engine.observe_semantic_boot_coverage(None, Some(0));
        assert!(!engine.semantic_index_qualification().coverage_complete);
        assert!(!engine.semantic_index_qualification().pending_work);
        seed(&mut engine.store, None);
        observe(&engine);
        assert!(engine.semantic_index_qualification().pending_work);
        let epoch = engine.semantic_index_qualification().epoch;
        engine.invalidate_semantic_coverage();
        engine.observe_semantic_publication(epoch);
        assert!(!engine.semantic_index_qualification().coverage_complete);
        seed(&mut engine.store, Some(vec![1.0, 0.0, 0.0]));
        let owner = IndexProgress::new();
        let mut pass = owner.begin_pass();
        let result = SearchEngine::embed_pending_chunks_fenced_retrying_owned(
            &path,
            &config(3),
            Some(&pass.token()),
            None,
            |op| FenceOutcome::Applied(op()),
            || false,
        )
        .unwrap();
        let FenceOutcome::Applied(index) = result else { panic!("publication failed") };
        engine.set_vector_index(index);
        engine.observe_semantic_publication(engine.semantic_index_qualification().epoch);
        pass.finish(IndexPassState::Ready);
        assert!(engine.semantic_index_qualification().coverage_complete);
        assert!(!engine.semantic_index_qualification().pending_work);
    }

    #[test]
    fn blob_fallback_wrong_dimension_and_accepted_sidecar_have_distinct_identity() {
        let dir = tempfile::tempdir().unwrap();
        for (name, vector_dim) in [("valid", 3), ("wrong", 2)] {
            let path = dir.path().join(format!("{name}.db"));
            let mut store = Store::open(&path).unwrap();
            seed(&mut store, Some(vec![1.0; vector_dim]));
            drop(store);
            let engine = SearchEngine::new(&path, config(3)).unwrap();
            observe(&engine);
            assert!(
                !engine.semantic_index_qualification().identity_verified,
                "BLOB provenance is unknown"
            );
            let epoch = engine.semantic_index_qualification().epoch;
            engine.observe_semantic_publication(epoch);
            assert!(
                !engine.semantic_index_qualification().identity_verified,
                "an unclaimed BLOB cannot be qualified by a publication observation"
            );
            if vector_dim == 2 {
                assert!(!engine.semantic_index_qualification().coverage_complete);
            }
            if vector_dim == 3 {
                drop(engine);
                let warm = SearchEngine::new(&path, config(3)).unwrap();
                observe(&warm);
                assert!(warm.accepted_native_sidecar());
                assert!(warm.semantic_index_qualification().identity_verified);
                assert!(warm.semantic_index_qualification().coverage_complete);
                assert!(warm.search("query", 1, None).is_err());
                assert!(
                    warm.semantic_index_qualification().coverage_complete,
                    "query failure does not invalidate the index"
                );
                warm.store.mark_context_dirty("code", "", "M.bsl").unwrap();
                drop(warm);
                let reopened = SearchEngine::new(&path, config(3)).unwrap();
                observe(&reopened);
                assert!(
                    reopened.semantic_index_qualification().context_pending,
                    "persisted context debt survives restart"
                );
            }
        }
    }

    #[test]
    fn successful_token_profile_publication_qualifies_a_partial_matching_cache() {
        let dir = tempfile::tempdir().unwrap();
        let tokenizer = br#"{"version":"1.0","truncation":null,"padding":null,"added_tokens":[],"normalizer":null,"pre_tokenizer":{"type":"Whitespace"},"post_processor":null,"decoder":null,"model":{"type":"WordLevel","vocab":{"[UNK]":0},"unk_token":"[UNK]"}}"#;
        let tokenizer_path = dir.path().join("tokenizer.json");
        std::fs::write(&tokenizer_path, tokenizer).unwrap();
        let tokenizer_hash = sha2::Sha256::digest(tokenizer)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let token_policy = crate::TokenPolicy::load(&tokenizer_path, &tokenizer_hash, 32).unwrap();

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
            let mut bytes = Vec::new();
            let mut buffer = [0; 2048];
            let input_count = loop {
                let read = stream.read(&mut buffer).unwrap();
                assert_ne!(read, 0);
                bytes.extend_from_slice(&buffer[..read]);
                let Some(split) = bytes.windows(4).position(|window| window == b"\r\n\r\n") else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&bytes[..split]).to_lowercase();
                let length = headers
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:"))
                    .unwrap()
                    .trim()
                    .parse::<usize>()
                    .unwrap();
                if bytes.len() >= split + 4 + length {
                    let request: serde_json::Value =
                        serde_json::from_slice(&bytes[split + 4..]).unwrap();
                    break request["input"].as_array().unwrap().len();
                }
            };
            let data: Vec<_> = (0..input_count)
                .map(|index| serde_json::json!({"index": index, "embedding": [0.0, 1.0, 0.0]}))
                .collect();
            let response = serde_json::json!({"data": data}).to_string();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response.len(),
                response
            )
            .unwrap();
        });

        let config = || SearchConfig {
            embedder: EmbedderConfig {
                base_url: format!("http://{address}"),
                model: "fixture".into(),
                dim: Some(3),
                token_policy: Some(token_policy.clone()),
                ..EmbedderConfig::default()
            },
            ..SearchConfig::default()
        };
        let embedder = Embedder::new(config().embedder);
        let path = dir.path().join("partial.db");
        let mut store = Store::open(&path).unwrap();
        let texts = ["Процедура One()\nКонецПроцедуры", "Процедура Two()\nКонецПроцедуры"];
        let chunks = [
            crate::Chunk {
                kind: crate::ChunkKind::Procedure,
                name: "One".into(),
                is_export: false,
                annotations: Vec::new(),
                line_start: 1,
                line_end: 2,
                text: texts[0].into(),
            },
            crate::Chunk {
                kind: crate::ChunkKind::Procedure,
                name: "Two".into(),
                is_export: false,
                annotations: Vec::new(),
                line_start: 3,
                line_end: 4,
                text: texts[1].into(),
            },
        ];
        let first_end = texts[0].len() as u32;
        let second_end = first_end + texts[1].len() as u32;
        let spans = [
            Some(crate::SourceSpan {
                parent_symbol: "One".into(),
                byte_start: 0,
                byte_end: first_end,
                parent_byte_start: 0,
                parent_byte_end: first_end,
                part_index: 1,
                part_count: 1,
            }),
            Some(crate::SourceSpan {
                parent_symbol: "Two".into(),
                byte_start: first_end,
                byte_end: second_end,
                parent_byte_start: first_end,
                parent_byte_end: second_end,
                part_index: 1,
                part_count: 1,
            }),
        ];
        store
            .reindex_file_with_identity_and_source_spans(
                "root",
                "Module.bsl",
                b"partial-cache",
                &chunks,
                Some(&[vec![1.0, 0.0, 0.0]]),
                None,
                embedder.storage_identity(),
                3,
                embedder.token_layout_claim(),
                &spans,
            )
            .unwrap();
        drop(store);

        let mut engine = SearchEngine::new(&path, config()).unwrap();
        assert!(!engine.semantic_index_qualification().identity_verified);
        engine.observe_semantic_boot_coverage(
            engine.chunk_count().ok(),
            engine.embedding_count_by_collection("code").ok(),
        );
        assert!(engine.semantic_index_qualification().pending_work);
        let epoch = engine.semantic_index_qualification().epoch;
        engine.observe_semantic_publication(epoch.wrapping_add(1));
        assert!(!engine.semantic_index_qualification().identity_verified);
        engine.observe_semantic_publication(epoch);
        assert!(
            !engine.semantic_index_qualification().identity_verified,
            "a partial matching cache is not complete evidence"
        );
        let embedder = Embedder::new(config().embedder);
        let mut apply = |operation: &mut dyn FnMut() -> Result<(), SearchError>| {
            FenceOutcome::Applied(operation())
        };
        let mut retry = || false;
        let (index, outcome) = SearchEngine::run_embedding_pass_owned(
            &engine.store,
            &embedder,
            3,
            32,
            1,
            None,
            None,
            &mut apply,
            Some(&mut retry),
        )
        .unwrap();
        assert!(matches!(outcome, FenceOutcome::Applied(())));
        server.join().unwrap();
        assert_eq!(index.len(), 2);
        engine.set_vector_index(index);
        engine.observe_semantic_publication(epoch);
        let qualification = engine.semantic_index_qualification();
        assert!(qualification.identity_verified);
        assert!(qualification.coverage_complete);
        assert!(!qualification.pending_work);
    }

    #[test]
    fn malformed_native_vectors_and_mode_changes_cannot_restore_ready() {
        let progress = IndexProgress::new();
        let mut pass = progress.begin_pass();
        SearchEngine::observe_embedding_dimensions(&[vec![1.0, 0.0]], 3, Some(&pass.token()));
        pass.finish(IndexPassState::Ready);
        assert_eq!(progress.snapshot().unwrap().state, IndexPassState::Failed);
        let dir = tempfile::tempdir().unwrap();
        let mut engine = SearchEngine::new(&dir.path().join("search.db"), config(3)).unwrap();
        observe(&engine);
        let epoch = engine.semantic_index_qualification().epoch;
        engine.set_serves_external_baseline(true).unwrap();
        engine.set_serves_external_baseline(false).unwrap();
        engine.observe_semantic_publication(epoch);
        assert!(!engine.semantic_index_qualification().coverage_complete);
    }

    #[test]
    fn committed_drift_context_marks_revoke_readiness_without_clearing_base_proof() {
        let dir = tempfile::tempdir().unwrap();
        let mut engine = SearchEngine::new(&dir.path().join("search.db"), config(3)).unwrap();
        seed(&mut engine.store, Some(vec![1.0, 0.0, 0.0]));
        engine.index =
            SearchEngine::build_persisted_index(&engine.store, 3, engine.embedder.as_ref())
                .unwrap();
        observe(&engine);
        let key = FileKey::new("", "M.bsl");
        let outcome = engine.apply_prepared_workspace_drift_batch(&[], &[], &[key], &mut || {
            ControlFlow::Continue(())
        });
        assert!(matches!(outcome, ControlFlow::Continue(Ok(_))));
        assert!(engine.semantic_index_qualification().coverage_complete);
        assert!(engine.semantic_index_qualification().context_pending);
    }
}
