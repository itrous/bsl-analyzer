mod baseline_runtime;
mod context;
mod document;
mod domain;
mod embedder;
mod engine;
mod error;
mod external_baseline;
mod fingerprint;
mod hybrid;
mod index;
mod key_carriers;
mod lexical;
pub mod lifecycle;
mod local_baseline;
mod merge;
mod point_refresh;
mod ports;
mod publish;
mod resolved_view_search;
mod resolver;
mod store;
mod token_policy;
mod vector_persist;
mod workspace_overlay;
mod workspace_roots;

pub use baseline_runtime::BaselineOverlaySearchService;
pub use code_chunk::{Chunk, ChunkKind, Chunker};

/// blake3 of a file's bytes, as the skip hash the local index keys on (see
/// `Store::file_hash`). Exposed so a fused producer in another crate records the SAME
/// hash the standalone indexer does, keeping an unchanged file reusable across runs.
pub fn content_blake3(bytes: &[u8]) -> Vec<u8> {
    blake3::hash(bytes).as_bytes().to_vec()
}
pub use context::file_path_to_module_path;
pub use document::{
    prepare_file_documents, semantic_key_for_indexed_document, semantic_text_for_indexed_document,
    Document,
};
pub use domain::{
    BaselineRef, BaselineSourceConfig, CorpusId, DocumentPath, ExternalBaselineBackend,
    ExternalBaselineConfig, FileOverlay, IndexedDocument, LexicalHit, OverlayChange, SearchOverlay,
    SemanticHit, Snapshot, SnapshotId, SnapshotPublishMetadata, SnapshotPublishStats, SourceSpan,
};
pub use embedder::{Embedder, EmbedderConfig};
pub use engine::{
    removed_path_keys, workspace_file_key_in, CarrierCapture, CarrierSnapshot, FenceOutcome,
    FtsIngest, IndexProgress, OverlayRetrySignals, ReferenceCollectionReplaceOutcome, SearchConfig,
    SearchEngine, SearchHit, SemanticIndexQualification, ValidatedWorkspaceOverlayPublication,
    ValidatedWorkspaceRootsTransitionPlan, WorkspaceOverlayDebt, WorkspaceRootsTransitionOutcome,
    WorkspaceRootsTransitionPlan, WorkspaceRootsTransitionSeed, WORKSPACE_APPLY_BATCH_ROWS,
};
pub use error::SCHEMA_VERSION_CURRENT;
pub use error::{EmbeddingFailure, EmbeddingFailureCode, SearchError};
pub use external_baseline::ensure_the_roots_mean_the_same_elsewhere;
pub use external_baseline::ExternalBaselineAdapter;
pub use external_baseline::{
    BaselineCollectionRecord, BaselineEmbeddingCoverageRecord, BaselineEmbeddingModelRecord,
    BaselineFileObjectDetails, BaselineFileObjectRecord, BaselineFileObjectReference,
    BaselineGcReport, BaselineSemanticPublication, BaselineSnapshotDetails, BaselineSnapshotRecord,
    SemanticPublishPhase, SemanticPublishProgress,
};
pub use fingerprint::{fingerprint_documents, fingerprint_indexed_documents};
pub use hybrid::{fuse_smart, FusedHit, Modality};
pub use index::{SearchResult, VectorIndex};
pub use local_baseline::LocalStoreBaselineAdapter;
pub use merge::{
    build_merge_context, merge_context_for_collection, merge_lexical, merge_semantic, HitSource,
    MergeContext, MergedHit,
};
pub use point_refresh::{
    BoundedPublication, PointCapture, PointPublish, PreparedPointBatch, C_BUSY_TIMEOUT,
    POINT_BATCH_KEYS, PREPARED_BYTES,
};
pub use ports::{
    BaselineLexicalSearch, BaselineManifestFile, BaselineSemanticSearch, EmbeddingGenerator,
    EmbeddingStore, GraphContextError, GraphContextProvider, LexicalSearchIndex, ModuleSnapshot,
    ModuleSnapshotSource, OverlayBuilder, ResolvedViewService, SnapshotCatalog,
    SnapshotContentStore, SnapshotFetch, SnapshotPublisher, VectorSearchIndex,
    WorkspaceBaselineManifest, WorkspaceBaselineManifestStore,
};
pub use publish::{
    BaselinePublishReport, BaselinePublisher, EmbeddingExecutionPolicy, EmbeddingProgress,
    SharedEmbeddingPublishStats, SharedEmbeddingPublisher,
};
pub use resolved_view_search::lexical_hits as lexical_hits_for_resolved_view;
pub use resolver::{InMemoryResolvedViewResolver, ResolvedView, ResolvedViewCounts};
pub use store::{BaselineManifestRecord, ChunkInfo, Store, TextSearchResult};
pub use token_policy::{TokenPolicy, SEGMENTATION_VERSION};
pub use workspace_overlay::{
    BaselineHashMode, PublicationBaseline, PublishOutcome, RefreshPlan, WorkspaceOverlayStats,
};
pub use workspace_roots::{
    FileKey, RejectedRoot, Rejection, WorkspaceRoots, CONFIGURATION_ROOT_ID,
};

mod progress;
pub use progress::{
    ActivePass, IndexCounters, IndexPassState, IndexPassToken, IndexPhase, IndexProgressSnapshot,
};
