use crate::domain::{
    BaselineRef, IndexedDocument, LexicalHit, SearchOverlay, SemanticHit, Snapshot,
    SnapshotPublishMetadata, SnapshotPublishStats,
};
use crate::error::SearchError;
use crate::external_baseline::BaselineEmbeddingStats;
use crate::resolver::ResolvedView;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

/// A resident-host-backed snapshot of one module's text and its already-computed syntax
/// tree. `text` is the verbatim source (no BOM strip, no newline normalization — matching
/// the disk read the overlay would otherwise perform) and `root` is the shared parse the
/// resident already holds, so the overlay reindex chunks it via
/// [`code_chunk::Chunker::chunk_parsed`] instead of parsing the file a second time.
pub struct ModuleSnapshot {
    pub text: Arc<str>,
    pub root: syntax::SyntaxNode,
}

/// The result of a [`ModuleSnapshotSource`] lookup.
pub enum SnapshotFetch {
    /// The resident served the file: its verbatim text plus the shared parse.
    Fetched(ModuleSnapshot),
    /// The resident could not serve the file (absent, loading, evicted, or a read that
    /// failed twice under drift). The caller falls back to its own disk read, so a missing
    /// resident degrades to today's behavior rather than surfacing an error.
    Unavailable,
}

/// Inverts the dependency from the search index (this crate, a lower layer) up to the
/// resident salsa host: `bsl-search` must not depend on the host crates or `mcp-server`.
/// An implementor resolves an absolute `.bsl` path to the resident file and hands back its
/// text and shared parse, letting the overlay's incremental reindex share the one repository
/// read+parse instead of doing its own.
pub trait ModuleSnapshotSource: Send + Sync {
    /// `path` is the file's ABSOLUTE path. The overlay keys dirty entries relative to its own
    /// (possibly nested) workspace root, which need not match the resident's root, so the
    /// caller resolves the rel to an absolute path before calling — a workspace-relative path
    /// would be re-joined against the resident's root and silently miss. Returns
    /// [`SnapshotFetch::Unavailable`] whenever the resident cannot serve the file.
    fn text_and_parse(&self, path: &str) -> SnapshotFetch;

    /// Reconcile any pending workspace drift before a batch of [`Self::text_and_parse`] calls,
    /// so a file edited just before the query reads fresh instead of the resident's stale
    /// pre-edit text. Default: a no-op for sources with no drift notion. An implementor must
    /// take only its own lock (never a caller's), so the caller can invoke this with no other
    /// lock held.
    fn catch_up(&self) {}
}

pub trait EmbeddingGenerator {
    fn model_id(&self) -> &str;

    /// Claim for the storage layout this input generator requires. Legacy generators need none.
    fn token_layout_claim(&self) -> Option<&str> {
        None
    }

    fn dimension(&self) -> usize;

    fn batch_ranges(
        &self,
        texts: &[&str],
        max_items: usize,
    ) -> Result<Vec<std::ops::Range<usize>>, SearchError> {
        let max_items = max_items.max(1);
        Ok((0..texts.len())
            .step_by(max_items)
            .map(|start| start..start.saturating_add(max_items).min(texts.len()))
            .collect())
    }

    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, SearchError>;
}

/// Inverts the dependency from the search index (this crate) up to the call-graph
/// layer. Clean architecture: `bsl-search` is a lower layer and must not depend on
/// `ide`/`hir`. An implementor that owns the call graph renders a code chunk's
/// OUTBOUND graph context (dispatch, signature, calls, metadata reads); the index
/// folds the returned string into the chunk's embedding text so vectors capture what
/// a method *does*, not just its source. The returned string is opaque to this crate.
///
/// `None` when the chunk is not a resolvable method (module header, unresolved name)
/// or no provider is configured — in which case embedding text is unchanged.
pub trait GraphContextProvider: Send + Sync {
    /// `rel_path` is the chunk's workspace-relative file path, `symbol_name` the
    /// method name, `kind` the chunk kind label (`procedure` / `function` / `header`).
    fn graph_context(&self, rel_path: &str, symbol_name: &str, kind: &str) -> Option<String>;

    /// Fallible variant that distinguishes a transient render FAILURE (e.g. the graph
    /// database could not be read) from a legitimate `None` (the chunk has no graph
    /// presence). [`crate::SearchEngine::refresh_dirty_contexts`] keeps a path's dirty
    /// mark on `Err` so the next publish retries the render, but clears it on `Ok(None)`
    /// (a file with no graph facts must not stay dirty forever). The default treats
    /// every result as successful, preserving the infallible contract for providers
    /// (like the test stubs) that cannot fail.
    fn try_graph_context(
        &self,
        rel_path: &str,
        symbol_name: &str,
        kind: &str,
    ) -> Result<Option<String>, GraphContextError> {
        Ok(self.graph_context(rel_path, symbol_name, kind))
    }

    /// Render graph context for a file pass that already holds its source text. The full key
    /// prevents reusing bytes from another root that happens to have the same relative path.
    /// Providers that can use the supplied source may override this; the default preserves the
    /// existing renderer behavior.
    fn try_graph_context_with_source(
        &self,
        source_key: &crate::workspace_roots::FileKey,
        symbol_name: &str,
        kind: &str,
        _source: &str,
    ) -> Result<Option<String>, GraphContextError> {
        self.try_graph_context(&source_key.path, symbol_name, kind)
    }

    /// Files written without a context this provider failed to render have been marked
    /// context-dirty, the highest of those marks being `mark_high`. The provider's owner decides
    /// when the marks are rendered; the call must not block, since the engine is held while it
    /// runs. The default ignores it, for providers that cannot fail.
    fn context_marks_owed(&self, _mark_high: i64) {}
}

/// A transient failure rendering graph context. The message is opaque to this crate.
#[derive(Debug)]
pub struct GraphContextError(pub String);

impl std::fmt::Display for GraphContextError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for GraphContextError {}

pub trait EmbeddingStore {
    fn load_embeddings(
        &self,
        embedding_keys: &[String],
        model_id: &str,
        dimension: usize,
    ) -> Result<HashMap<String, Vec<f32>>, SearchError>;

    fn store_embeddings(
        &self,
        model_id: &str,
        dimension: usize,
        embeddings: &[(String, Vec<f32>)],
    ) -> Result<BaselineEmbeddingStats, SearchError>;

    /// Pin or validate this baseline's embedding identity (model + dimension).
    /// The first semantic publish records it; later publishes must match, so a
    /// shared baseline can never mix vectors from different models. Default no-op
    /// for stores not shared across writers (the local sqlite index).
    fn ensure_embedding_identity(
        &self,
        _model_id: &str,
        _dimension: usize,
    ) -> Result<(), SearchError> {
        Ok(())
    }

    /// Pin or validate the opt-in source-part layout before any snapshot or vector writes.
    fn ensure_token_layout_claim(
        &self,
        _layout_version: &str,
        _claim: &str,
    ) -> Result<(), SearchError> {
        Err(crate::EmbeddingFailure::new(crate::EmbeddingFailureCode::EmbeddingInvalidConfig)
            .into())
    }
}

pub trait SnapshotCatalog {
    fn resolve_baseline(&self, baseline: &BaselineRef) -> Result<Option<Snapshot>, SearchError>;
}

pub trait SnapshotContentStore {
    fn load_snapshot_documents(
        &self,
        snapshot: &Snapshot,
    ) -> Result<Vec<IndexedDocument>, SearchError>;
}

pub trait BaselineLexicalSearch {
    fn lexical_search_baseline(
        &self,
        snapshot_id: &str,
        query: &str,
        collection: Option<&str>,
        limit: usize,
    ) -> Result<Vec<LexicalHit>, SearchError>;
}

pub trait BaselineSemanticSearch {
    fn semantic_search_baseline(
        &self,
        snapshot_id: &str,
        query_embedding: &[f32],
        model_id: &str,
        dimension: usize,
        collection: Option<&str>,
        limit: usize,
    ) -> Result<Vec<SemanticHit>, SearchError>;
}

pub trait SnapshotPublisher {
    fn publish_snapshot(
        &self,
        snapshot: &Snapshot,
        metadata: &SnapshotPublishMetadata,
        documents: &[IndexedDocument],
    ) -> Result<SnapshotPublishStats, SearchError>;
}

pub trait OverlayBuilder {
    fn build_overlay(
        &self,
        baseline: &BaselineRef,
        workspace_root: &Path,
    ) -> Result<SearchOverlay, SearchError>;
}

pub trait LexicalSearchIndex {
    fn lexical_candidates(
        &self,
        view: &ResolvedView,
        query: &str,
        limit: usize,
    ) -> Result<Vec<IndexedDocument>, SearchError>;
}

pub trait VectorSearchIndex {
    fn semantic_candidates(
        &self,
        view: &ResolvedView,
        query_embedding: &[f32],
        limit: usize,
    ) -> Result<Vec<IndexedDocument>, SearchError>;
}

pub trait ResolvedViewService {
    fn resolve_view(
        &self,
        baseline: BaselineRef,
        baseline_documents: Vec<IndexedDocument>,
        overlay: SearchOverlay,
    ) -> Result<ResolvedView, SearchError>;
}

#[derive(Debug, Clone)]
pub struct BaselineManifestFile {
    pub collection: String,
    /// The source root the file belongs to; the consumer keys by `(root_id, path)`, and a
    /// manifest that flattens every file onto the configuration makes it compare an extension's
    /// file against the configuration's fingerprint.
    pub root_id: String,
    pub path: String,
    pub file_fingerprint: String,
    pub document_count: usize,
    pub file_object_id: String,
}

#[derive(Debug, Clone)]
pub struct WorkspaceBaselineManifest {
    pub snapshot_id: String,
    pub snapshot_fingerprint: Option<String>,
    pub files: Vec<BaselineManifestFile>,
}

pub trait WorkspaceBaselineManifestStore {
    fn load_baseline_manifest(
        &self,
        snapshot_id: &str,
    ) -> Result<WorkspaceBaselineManifest, SearchError>;
}
