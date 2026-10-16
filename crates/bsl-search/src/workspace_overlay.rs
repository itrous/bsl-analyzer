use crate::domain::{BaselineRef, CorpusId, DocumentPath, IndexedDocument, SearchOverlay};
use crate::embedder::Embedder;
use crate::error::SearchError;
use crate::lexical::lexical_hits_for_documents;
use crate::ports::{GraphContextProvider, ModuleSnapshot};
use crate::store::Store;
use crate::workspace_roots::{FileKey, WorkspaceRoots};
use code_chunk::Chunker;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BaselineHashMode {
    RawFileBytes,
    NormalizedChunks,
}

#[derive(Debug, Clone)]
pub struct OverlayVectorDocument {
    pub document: IndexedDocument,
    pub embedding: Vec<f32>,
}

#[derive(Debug, Clone)]
pub struct WorkspaceOverlayIndex {
    pub overlay: SearchOverlay,
    pub hidden_paths: HashSet<FileKey>,
    pub lexical_documents: Vec<IndexedDocument>,
    pub vector_documents: Vec<OverlayVectorDocument>,
}

impl WorkspaceOverlayIndex {
    pub fn is_empty(&self) -> bool {
        self.overlay.changes.is_empty()
    }
}

/// A read-only plan for a manifest-driven overlay refresh, produced off any lock against a
/// standalone store (Phase A) and applied later under the inner cache lock (Phase C).
///
/// Splitting plan/embed/publish keeps the slow remote embed (Phase B) entirely off the engine
/// and inner-cache locks: Phase A only reads files and the warm embedding cache, Phase B embeds
/// the `missing_embeddings` inputs with no lock held, and Phase C merges everything atomically.
#[derive(Debug, Clone)]
pub struct RefreshPlan {
    snapshot_id: String,
    baseline_identity: Option<(String, Option<String>)>,
    /// External fingerprint rows are meaningful only for the manifest authority that created
    /// them. Local refreshes compare SQLite file hashes directly and must not publish them.
    manifest_backed: bool,
    /// Overlay file entries with lexical docs + embedding inputs but no vectors yet; vectors are
    /// assembled in Phase C from the merged embedding cache.
    entries: Vec<(FileKey, PlannedEntry)>,
    hidden_paths: HashSet<FileKey>,
    updated_persisted: HashMap<FileKey, crate::store::PersistedFingerprint>,
    /// Distinct `embedding_key -> embedding input` pairs that have no warm-cache vector; these are
    /// the inputs Phase B embeds. The key is the hash of the embedding input (the semantic key).
    missing_embeddings: HashMap<String, String>,
    /// Exact-key baseline vectors hydrated only for changed local documents in Phase A.
    reused_embeddings: HashMap<String, Vec<f32>>,
    /// See [`ScannedFiles::unreadable`]: how much of the tree the planning scan could not read.
    scan_unreadable: usize,
    /// See [`ScannedFiles::canonical_fallbacks`]: files whose physical spelling is unknown.
    scan_canonical_fallbacks: usize,
    /// Every key the planning scan saw, whether or not it produced a planned entry. Publication
    /// needs the distinction: a seen key without an entry was PROVEN baseline-equal, an unseen key
    /// was proven nothing.
    seen_keys: HashSet<FileKey>,
    /// Seen files whose read failed during planning: proven present, contents unknown.
    read_failures: HashSet<FileKey>,
    /// Seen files the planning phase skipped by a persisted-row hit — never read. The publish
    /// must not consume a live dirty mark for such a key: planning runs off-lock and cannot
    /// see the marks, and a mark is positive evidence the row must not have been trusted.
    gate_skipped: HashSet<FileKey>,
}

impl RefreshPlan {
    /// Whether the planning scan may speak for the whole tree — same verdict as
    /// [`ScannedFiles::clean`]. A caller reporting the warmup outcome must not call an unclean
    /// plan's emptiness "no local diffs": the scan may simply not have seen the diffs.
    pub fn scan_is_clean(&self) -> bool {
        self.scan_unreadable == 0 && self.scan_canonical_fallbacks == 0
    }

    /// How much of the tree the planning scan could not read (see
    /// [`project_model::SourceSet::unreadable`]).
    pub fn scan_unreadable(&self) -> usize {
        self.scan_unreadable
    }

    /// How many files the planning scan walked without a physical spelling (see
    /// [`project_model::SourceSet::canonical_fallbacks`]).
    pub fn scan_canonical_fallbacks(&self) -> usize {
        self.scan_canonical_fallbacks
    }

    /// How many seen files the planning phase failed to read: proven present, contents unknown.
    pub fn read_failure_count(&self) -> usize {
        self.read_failures.len()
    }

    /// The distinct `(embedding_key, embedding_input)` pairs Phase B must embed.
    pub fn missing_embeddings(&self) -> &HashMap<String, String> {
        &self.missing_embeddings
    }

    pub(crate) fn reused_embeddings(&self) -> &HashMap<String, Vec<f32>> {
        &self.reused_embeddings
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty() && self.hidden_paths.is_empty()
    }

    /// Number of locally-changed files this plan re-embeds. Reported in `search status` so an
    /// agent can see how much of the overlay differed from the baseline.
    pub fn overlay_file_count(&self) -> usize {
        self.entries.len()
    }

    /// Every overlay embedding key referenced by the planned entries. The caller uses this to pull
    /// warm-reused vectors into the published embedding set so Phase C builds complete vectors. The
    /// key is the hash of each chunk's embedding input (the semantic key), matching how the cache
    /// is keyed in [`build_overlay_vectors`].
    pub fn planned_embedding_keys(&self) -> impl Iterator<Item = String> + '_ {
        self.entries.iter().flat_map(|(_, entry)| {
            entry.embedding_inputs.iter().map(|input| overlay_embedding_key(input))
        })
    }
}

#[derive(Debug, Clone)]
struct PlannedEntry {
    fingerprint: FileFingerprint,
    file_hash: Vec<u8>,
    lexical_documents: Vec<IndexedDocument>,
    embedding_inputs: Vec<String>,
}

/// One file prepared for the in-memory part of a workspace-root transition.
pub(crate) struct WorkspaceTransitionOverlayFile {
    pub(crate) key: FileKey,
    pub(crate) len: u64,
    pub(crate) modified: Option<SystemTime>,
    pub(crate) canonical: PathBuf,
    pub(crate) file_hash: Vec<u8>,
    pub(crate) lexical_documents: Vec<IndexedDocument>,
    pub(crate) embedding_inputs: Vec<String>,
    pub(crate) has_baseline: bool,
    pub(crate) baseline_equal: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceOverlayStats {
    pub overlay_files: usize,
    pub deleted_files: usize,
    pub hidden_paths: usize,
    pub lexical_chunks: usize,
    pub semantic_chunks: usize,
    pub cached_embeddings: usize,
    pub watcher_mode: bool,
    pub pending_dirty_paths: usize,
}

#[derive(Clone, Default)]
pub struct WorkspaceOverlayCache {
    pub(crate) baseline_identity: Option<(String, Option<String>)>,
    entries: HashMap<FileKey, OverlayFileEntry>,
    unembedded_entries: usize,
    hidden_paths: HashSet<FileKey>,
    embedding_cache: HashMap<String, Vec<f32>>,
    /// Watcher-marked paths awaiting re-embed, each tagged with the sequence at which it was last
    /// marked. The sequence lets [`publish_plan`] tell a path superseded by its refresh from one
    /// the watcher re-marked while the lock-free embed was in flight (same path, newer sequence).
    dirty_paths: HashMap<FileKey, u64>,
    dirty_seq: u64,
    /// Consecutive refresh-failure count per retained dirty path. A path whose stat/read fails is
    /// re-marked dirty (so the next refresh retries) with its count bumped; after
    /// [`MAX_DIRTY_REFRESH_FAILURES`] it is dropped from the dirty set with a warning rather than
    /// retried forever. A fresh [`Self::mark_dirty_path`] (a new watcher event) clears the count,
    /// and a successful refresh drops the entry, so the count is strictly consecutive.
    dirty_failures: HashMap<FileKey, u32>,
    /// Keys proven PRESENT whose contents could not be read (by either the point path or a
    /// full pass) and whose settlement therefore never happened. A durable signal distinct
    /// from the dirty marks: the bounded point budget may drop a mark, but the obligation to
    /// re-read the file stands until some pass proves the key settled or gone.
    unread_keys: HashSet<FileKey>,
    /// The freshness fence: per key, the `dirty_seq` value of its latest POINT settlement
    /// (any [`PointAction`], plus [`Self::remove_known_deleted`]). A planned publication
    /// whose Phase A started at fence F must not overwrite a key with `settled_seq > F` on
    /// any carrier — the point outcome is fresher than the plan.
    settled_seq: HashMap<FileKey, u64>,
    /// The `dirty_seq` value of the latest WHOLESALE invalidation (an in-place full
    /// publication, `clear`, `mark_initialized_clean`, `set_graph_context_provider`). A plan
    /// whose fence is older than this may not publish at all: the entire state it planned
    /// against has been replaced.
    wholesale_seq: u64,
    /// One-way, and doubles as "a feed is attached": the feeder that owns a change stream
    /// enables it (a hub whose watch is up or whose own poll is running, or the first applied
    /// drift batch that proves the feed), and every refresh allowed to skip the full tree
    /// trusts it. A bare mark or removal records its trace and never flips this — a store
    /// nothing feeds must keep taking full scans instead of freezing at its last publication
    /// (github#186).
    watcher_mode: bool,
    initialized: bool,
    /// The last full publication ran over a scan that could not vouch for the whole tree, so its
    /// removals were withheld and only a future CLEAN full scan can catch up. The dispatchers
    /// honour it by taking the full-scan arm even in watcher mode; a clean full publication
    /// clears it. (A read failure of a seen file is NOT this: the file list was complete, and the
    /// per-key dirty mark already drives the retry.)
    full_rescan_pending: bool,
    /// How many overlay entries have been (re)built from a resident-provided shared parse, rather
    /// than a self-parsed disk read. A cumulative observability counter — proves the resident-fed
    /// path actually fires — reset only by [`Self::clear`].
    resident_fed_count: usize,
    /// Optional graph-context provider (dependency-inverted). When set, overlay
    /// (uncommitted-edit) chunks are enriched with their call-graph context before
    /// embedding, matching the local index.
    graph_context_provider: Option<Arc<dyn GraphContextProvider>>,
}

/// The stored raw-bytes baseline for a dirty-path refresh: per-path stored hashes plus the recipe
/// to recompute a file's hash. Bundled so [`WorkspaceOverlayCache::refresh_dirty_paths`] stays
/// within the argument-count lint.
struct RawBaseline<'a> {
    files: &'a HashMap<FileKey, Vec<u8>>,
    hash_mode: BaselineHashMode,
}

/// The manifest baseline for a dirty-path refresh: the published fingerprints plus the store whose
/// persisted fingerprint rows the refresh must keep truthful. Bundled so
/// [`WorkspaceOverlayCache::refresh_dirty_paths_from_manifest`] stays within the argument-count
/// lint.
struct ManifestBaseline<'a> {
    fingerprints: &'a HashMap<FileKey, String>,
    store: &'a Store,
}

/// One key's point-path result: what the pass PROVED (or failed to prove) about the file,
/// paired with an orthogonal store flag — a failed row retraction does not erase the file
/// outcome, it only changes how the mark is retained. [`WorkspaceOverlayCache::settle_point`]
/// is the single place these are applied to the carriers, so no branch can forget one.
pub(crate) struct PointSettlement {
    pub(crate) action: PointAction,
    /// The key's fingerprint-row obligation did not land (retraction denied); the mark must
    /// survive WITHOUT charging the file budget, whatever the action was.
    store_fault: bool,
}

pub(crate) enum PointAction {
    /// Read and different from the baseline: a fresh overlay entry.
    Reindexed { entry: OverlayFileEntry, has_baseline: bool },
    /// Read and equal to the baseline: the local entry (and any hiding) is lifted.
    BaselineEqual,
    /// Proven gone: `NotFound` under a reachable root, a non-file or non-source target, or a
    /// root no longer registered.
    ProvenGone { has_baseline: bool },
    /// stat/read failed: proven nothing, retried on the bounded file budget.
    FileFault { reason: &'static str },
    /// The file was read, but building the entry (chunking/embedding) failed: an
    /// infrastructure fault, not the file's — retried without charging the file budget.
    BuildFault,
}

/// The pre-plan snapshot a planned publication is judged against in Phase C: the live marks
/// as of Phase A (key -> sequence) and the freshness fence. Opaque to the caller — captured
/// under the cache lock by [`WorkspaceOverlayCache::publication_baseline`] and handed back to
/// [`WorkspaceOverlayCache::publish_plan`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicationBaseline {
    dirty: HashMap<FileKey, u64>,
    fence: u64,
    /// Keys whose live dirty or unread debt makes a persisted fingerprint untrustworthy.
    unread: HashSet<FileKey>,
}

impl PublicationBaseline {
    /// The keys whose fingerprint rows Phase A must not trust (see the field doc).
    pub fn distrusted(&self) -> &HashSet<FileKey> {
        &self.unread
    }

    /// Planning input for a retry-capable host: live marks also distrust persisted rows, which
    /// may be the unchanged table left by an atomic Phase-C rollback.
    pub fn retry_distrusted(&self) -> HashSet<FileKey> {
        self.unread.iter().chain(self.dirty.keys()).cloned().collect()
    }
}

/// The outcome of a planned publication. `Superseded` means a wholesale invalidation happened
/// between Phase A and Phase C: nothing of the plan was applied (only the value-stable
/// embeddings were merged) and the caller owes a fresh pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishOutcome {
    Applied {
        gate_deferred: usize,
        persist_ok: bool,
        /// The overlay size AFTER the publish — carried and fenced keys make the applied
        /// state diverge from the plan's own counts, and the reported status must describe
        /// what actually serves.
        overlay_files: usize,
        /// Baseline files locally DELETED as of the applied state (hidden with no entry):
        /// a local diff even with zero entries — "no local diffs" must not claim otherwise.
        deleted_files: usize,
        /// The unread debts as of the applied state: what the pass genuinely failed to
        /// verify, with the stale plan's out-fenced failures already discounted.
        unread_keys: usize,
    },
    Superseded,
}

type OverlayFingerprintPublication = (String, HashMap<FileKey, crate::store::PersistedFingerprint>);

/// Fully prepared Phase-C state. It is value-only until the host admits the matching Store
/// transaction; a refusal can therefore retry this exact bundle without rebuilding or embedding.
pub(crate) struct OverlayPublicationStaging {
    pub(crate) next_cache: WorkspaceOverlayCache,
    pub(crate) outcome: PublishOutcome,
    pub(crate) fingerprints: Option<OverlayFingerprintPublication>,
}

/// What one FULL publication proved, for the shared tail ([`WorkspaceOverlayCache::finish_publication`]):
/// which keys it saw, which it failed to read, which entry builds faulted, which keys the
/// planning gate skipped unread, and which mark snapshot it supersedes. The three publication
/// paths only fill this in; every obligation is applied by the tail.
struct PublicationVerdict<'a> {
    scan_is_clean: bool,
    seen_keys: &'a HashSet<FileKey>,
    read_failures: &'a HashSet<FileKey>,
    build_faults: &'a HashSet<FileKey>,
    gate_skipped: &'a HashSet<FileKey>,
    /// Keys with a point settlement FRESHER than the plan's fence: the publication may not
    /// touch them on any carrier. Empty for the in-place paths (they run under the lock).
    fenced: &'a HashSet<FileKey>,
    /// The planned path's pre-plan mark snapshot (key -> seq); a mark re-bumped since is not
    /// superseded. `None` for the in-place paths, which run under the lock and supersede
    /// every live mark they settled.
    superseded: Option<&'a HashMap<FileKey, u64>>,
}

/// Consecutive stat/read failures tolerated for a retained dirty path before it is dropped from
/// the dirty set (with a warning). Bounds the per-query retry of a permanently-unreadable path
/// (a deleted file, a path shaped like a `.bsl` that is really a directory) to a fixed budget;
/// strictly better than the pre-S2 behaviour, which silently dropped a path on its FIRST failure.
/// A later watcher event for the same path re-marks it fresh and resets the count.
const MAX_DIRTY_REFRESH_FAILURES: u32 = 3;

impl std::fmt::Debug for WorkspaceOverlayCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkspaceOverlayCache")
            .field("entries", &self.entries)
            .field("hidden_paths", &self.hidden_paths)
            .field("embedding_cache_len", &self.embedding_cache.len())
            .field("dirty_paths", &self.dirty_paths)
            .field("dirty_failures", &self.dirty_failures)
            .field("watcher_mode", &self.watcher_mode)
            .field("initialized", &self.initialized)
            .field("resident_fed_count", &self.resident_fed_count)
            .field("graph_context", &self.graph_context_provider.is_some())
            .finish()
    }
}

impl WorkspaceOverlayCache {
    fn insert_entry(&mut self, key: FileKey, entry: OverlayFileEntry) -> Option<OverlayFileEntry> {
        self.unembedded_entries +=
            usize::from(entry.vector_documents.len() < entry.embedding_inputs.len());
        let previous = self.entries.insert(key, entry);
        if let Some(previous) = &previous {
            self.unembedded_entries -=
                usize::from(previous.vector_documents.len() < previous.embedding_inputs.len());
        }
        previous
    }

    fn remove_entry(&mut self, key: &FileKey) -> Option<OverlayFileEntry> {
        let previous = self.entries.remove(key);
        if let Some(previous) = &previous {
            self.unembedded_entries -=
                usize::from(previous.vector_documents.len() < previous.embedding_inputs.len());
        }
        previous
    }

    fn retain_entries(&mut self, mut keep: impl FnMut(&FileKey, &mut OverlayFileEntry) -> bool) {
        let count = &mut self.unembedded_entries;
        self.entries.retain(|key, entry| {
            let retained = keep(key, entry);
            if !retained {
                *count -= usize::from(entry.vector_documents.len() < entry.embedding_inputs.len());
            }
            retained
        });
    }

    /// Number of watcher marks, without copying the key set.
    pub fn dirty_paths_count(&self) -> usize {
        self.dirty_paths.len()
    }

    pub fn clear(&mut self) {
        self.baseline_identity = None;
        self.entries.clear();
        self.unembedded_entries = 0;
        self.hidden_paths.clear();
        self.dirty_paths.clear();
        self.dirty_failures.clear();
        self.unread_keys.clear();
        self.resident_fed_count = 0;
        self.initialized = false;
        // The withheld removals belonged to the state just discarded; the next build starts
        // from nothing and owes no catch-up scan for it.
        self.full_rescan_pending = false;
        self.bump_wholesale();
    }

    /// Mark the overlay initialized with no entries: the caller has proven the store this overlay
    /// fronts was just reconciled with disk, so nothing differs from the baseline and a full disk
    /// scan (a prime) would build zero entries anyway. This is the zero-scan, zero-RAM equivalent
    /// of that prime. Until the overlay is initialized the incremental reindex is inert
    /// ([`Self::capture_point_keys`] captures nothing), so this is what unblocks the resident-fed
    /// path; from here the watcher marks and point refreshes serve fresh edits.
    pub fn mark_initialized_clean(&mut self) {
        self.baseline_identity = None;
        self.entries.clear();
        self.unembedded_entries = 0;
        self.hidden_paths.clear();
        self.dirty_paths.clear();
        self.dirty_failures.clear();
        self.unread_keys.clear();
        self.initialized = true;
        // The caller has PROVEN the store equals the disk, which is exactly what the pending
        // rescan existed to re-establish.
        self.full_rescan_pending = false;
        self.bump_wholesale();
    }

    /// Inject the graph-context provider so overlay chunks are enriched like the
    /// local index. Clears cached entries so they rebuild with context.
    pub fn set_graph_context_provider(&mut self, provider: Arc<dyn GraphContextProvider>) {
        self.graph_context_provider = Some(provider);
        self.entries.clear();
        self.unembedded_entries = 0;
        self.embedding_cache.clear();
        self.initialized = false;
        // A changed semantic source invalidates everything a plan built without it: an older
        // plan applied after this would publish documents with the OLD context.
        self.bump_wholesale();
    }

    /// Replace only the source used by future point refreshes and fence every plan prepared with
    /// the previous source. Stable lexical entries remain valid: graph context changes semantic
    /// text, not lexical documents. Unlike [`Self::set_graph_context_provider`], this does not
    /// discard them or the value-stable embedding cache, but it still raises the wholesale epoch
    /// so an older semantic plan cannot publish after the provider swap.
    pub(crate) fn replace_graph_context_provider(
        &mut self,
        provider: Arc<dyn GraphContextProvider>,
    ) {
        self.graph_context_provider = Some(provider);
        self.dirty_seq += 1;
        self.wholesale_seq = self.dirty_seq;
    }

    pub fn enable_watcher_mode(&mut self) {
        self.watcher_mode = true;
    }

    /// Selectively move the cache to a new root keyspace while preserving stable edits and warm
    /// embeddings. `unread_present` contains keys a complete walk saw but could not read; stable
    /// carriers for them survive and every such key receives a durable dirty/unread retry debt.
    /// The wholesale fence invalidates every lock-free plan made against the old topology, but
    /// settlements for unaffected keys remain valid. This is not a full overlay publication:
    /// stable roots are not rebuilt, so `initialized` and `full_rescan_pending` keep their prior
    /// truth rather than claiming the transition proved the whole overlay complete.
    pub(crate) fn transition_roots(
        &mut self,
        changed_root_ids: &HashSet<String>,
        cleanup: &HashSet<FileKey>,
        obsolete_baseline: &HashSet<FileKey>,
        unread_present: &HashSet<FileKey>,
        files: Vec<WorkspaceTransitionOverlayFile>,
    ) {
        self.dirty_seq += 1;
        self.wholesale_seq = self.dirty_seq;

        // A stable local overlay entry may already be hiding its remote baseline twin. If its
        // bytes become unreadable during an otherwise unrelated root transition, the entry and
        // its hiding are one coherent carrier pair: keeping the entry but lifting the hiding
        // would expose both local and baseline versions. New, rebound or cleaned-up unread keys
        // have no trusted entry and therefore inherit no hiding.
        let unread_hidings_to_preserve: HashSet<FileKey> = unread_present
            .iter()
            .filter(|key| {
                !changed_root_ids.contains(&key.root_id)
                    && !cleanup.contains(*key)
                    && self.entries.contains_key(*key)
                    && self.hidden_paths.contains(*key)
            })
            .cloned()
            .collect();

        let binding_changed = |key: &FileKey| changed_root_ids.contains(&key.root_id);
        self.retain_entries(|key, _| !binding_changed(key));
        self.hidden_paths.retain(|key| !binding_changed(key));
        self.dirty_paths.retain(|key, _| !binding_changed(key));
        self.dirty_failures.retain(|key, _| !binding_changed(key));
        self.unread_keys.retain(|key| !binding_changed(key));
        self.settled_seq.retain(|key, _| !binding_changed(key) && !cleanup.contains(key));

        for key in cleanup {
            self.remove_entry(key);
            self.dirty_paths.remove(key);
            self.dirty_failures.remove(key);
            self.unread_keys.remove(key);
            if obsolete_baseline.contains(key) {
                self.hidden_paths.insert(key.clone());
            } else {
                self.hidden_paths.remove(key);
            }
        }

        // A clean walk proved these keys present, but their bytes were unavailable. Preserve any
        // stable-root carrier left above; for a new or rebound key, publish no guessed content.
        // Either way the dirty+unread pair is the durable obligation that lets watcher/overlay
        // retry heal the file. Presence is specifically not absence evidence, so it creates no
        // new hiding; only the coherent local-entry hiding captured above survives.
        for key in unread_present {
            if unread_hidings_to_preserve.contains(key) {
                self.hidden_paths.insert(key.clone());
            } else {
                self.hidden_paths.remove(key);
            }
            self.unread_keys.insert(key.clone());
            self.retain_dirty_uncharged(key.clone(), 0);
        }

        for file in files {
            if file.baseline_equal {
                self.remove_entry(&file.key);
                self.hidden_paths.remove(&file.key);
                continue;
            }
            let vector_documents = file
                .lexical_documents
                .iter()
                .zip(&file.embedding_inputs)
                .filter_map(|(document, input)| {
                    self.embedding_cache.get(&overlay_embedding_key(input)).map(|embedding| {
                        OverlayVectorDocument {
                            document: document.clone(),
                            embedding: embedding.clone(),
                        }
                    })
                })
                .collect();
            let key = file.key;
            self.insert_entry(
                key.clone(),
                OverlayFileEntry {
                    fingerprint: FileFingerprint {
                        len: file.len,
                        modified: file.modified,
                        canonical: file.canonical,
                    },
                    file_hash: file.file_hash,
                    lexical_documents: file.lexical_documents,
                    vector_documents,
                    embedding_inputs: file.embedding_inputs,
                },
            );
            if file.has_baseline {
                self.hidden_paths.insert(key);
            } else {
                self.hidden_paths.remove(&key);
            }
        }
    }

    /// Every root-keyed in-memory witness, including negative/freshness-only state that is not a
    /// positive [`crate::key_carriers::KeyCarrier`]. A root transition uses this snapshot while
    /// holding the cache lock so dirty-only and hidden-only obsolete keys cannot survive it.
    pub(crate) fn root_keyed_keys(&self) -> HashSet<FileKey> {
        let mut keys = HashSet::new();
        keys.extend(self.entries.keys().cloned());
        keys.extend(self.hidden_paths.iter().cloned());
        keys.extend(self.dirty_paths.keys().cloned());
        keys.extend(self.dirty_failures.keys().cloned());
        keys.extend(self.unread_keys.iter().cloned());
        keys.extend(self.settled_seq.keys().cloned());
        keys
    }

    /// Fence captured with a root plan. Provider/hash-mode/full-publication invalidations move it,
    /// preventing documents prepared under an old semantic source from being installed later.
    pub(crate) fn transition_epoch(&self) -> u64 {
        self.wholesale_seq
    }

    /// Drop a key's overlay entry for a deletion PROVEN by the caller (its store rows are
    /// already gone). The point refresh cannot be trusted to settle this one: when the whole
    /// root vanished together with the file, it reads the dead root as "unreachable, retry"
    /// and would leave a ghost entry serving hits forever. The caller keeps the dirty mark, so
    /// if the deletion event lied and the file is alive, the next point pass republishes it.
    pub fn remove_known_deleted(&mut self, key: &FileKey, has_baseline: bool) {
        self.record_settlement(key);
        self.remove_entry(key);
        self.unread_keys.remove(key);
        // The baseline copy is HIDDEN, not unhidden: for a remote baseline this set is the
        // only filter, and the deleted file would otherwise resurface as a baseline hit the
        // moment its local entry is gone. Hiding takes EVIDENCE of a baseline copy: a key
        // outside the baseline has nothing to hide, and under a dead root the parasitic
        // hiding would be unremovable and inflate the deleted-files stat forever.
        if has_baseline {
            self.hidden_paths.insert(key.clone());
        } else {
            self.hidden_paths.remove(key);
        }
    }

    /// Whether the last full publication ran over an incomplete scan and withheld its removals,
    /// leaving the overlay waiting for a clean full scan to catch up. The dispatchers already
    /// honour this on the next cold-scan-allowed refresh; the accessor is for a caller deciding
    /// whether to drive one.
    pub fn needs_full_rescan(&self) -> bool {
        self.full_rescan_pending
    }

    /// Record a wholesale invalidation on the SAME scale as the marks and settlements: an
    /// independent counter would be incomparable with a plan's fence, and the supersede check
    /// would lie whenever `dirty_seq` had already advanced past it.
    fn bump_wholesale(&mut self) {
        self.dirty_seq += 1;
        self.wholesale_seq = self.dirty_seq;
        self.settled_seq.clear();
    }

    /// Record a point settlement for the freshness fence (see the `settled_seq` field). Every
    /// settled action writes it, faults included: after the point budget drops a mark, the
    /// fence (and `unread_keys`) may be the ONLY witnesses a stale plan must not erase.
    fn record_settlement(&mut self, key: &FileKey) {
        self.dirty_seq += 1;
        self.settled_seq.insert(key.clone(), self.dirty_seq);
    }

    /// The atomic pre-plan snapshot for a planned publication: the live marks (whose exact
    /// sequences tell a superseded mark from a re-marked one) and the freshness fence.
    pub fn publication_baseline(&self) -> PublicationBaseline {
        PublicationBaseline {
            dirty: self.dirty_paths.clone(),
            fence: self.dirty_seq,
            unread: self.unread_keys.clone(),
        }
    }

    pub fn mark_dirty_path(&mut self, key: FileKey) {
        // A fresh watcher event is a clean slate: clear any consecutive-failure count so a path
        // that failed to refresh before, then genuinely changed, gets the full retry budget again.
        self.dirty_failures.remove(&key);
        self.dirty_seq += 1;
        self.dirty_paths.insert(key, self.dirty_seq);
    }

    /// Whether this key carries a live overlay RECORD — the entry a point refresh rebuilds.
    /// The context refresh asks this before it clears a key's context mark: a key the store
    /// holds no chunks for is carried by its record alone, and only the point path can rebuild
    /// that record from the graph now current.
    pub(crate) fn holds_entry(&self, key: &FileKey) -> bool {
        self.entries.contains_key(key)
    }

    /// Re-mark a path whose refresh failed (stat/read error), carrying its consecutive-failure
    /// count. Past [`MAX_DIRTY_REFRESH_FAILURES`] the path is dropped from the dirty set with a
    /// warning instead of retried forever; a later [`Self::mark_dirty_path`] resets it.
    fn retain_dirty_after_failure(&mut self, key: FileKey, prior_failures: u32, reason: &str) {
        let failures = prior_failures + 1;
        if failures >= MAX_DIRTY_REFRESH_FAILURES {
            tracing::warn!(
                root = %key.root_id,
                path = %key.path,
                reason,
                failures,
                "dropping overlay dirty path after repeated refresh failures; a later change \
                 re-marks it fresh"
            );
            return;
        }
        self.dirty_failures.insert(key.clone(), failures);
        self.dirty_seq += 1;
        self.dirty_paths.insert(key, self.dirty_seq);
    }

    /// Re-mark a path after a fault that is NOT the path's own — a failed fingerprint-row
    /// retraction (store fault) or a failed entry build (embedder fault): the bounded
    /// stat/read budget is NOT charged — the streak count goes back untouched and the mark
    /// simply survives for the retry. Charging the budget would let three infrastructure
    /// hiccups drop the mark and leave the obligation with nothing to retry it.
    fn retain_dirty_uncharged(&mut self, key: FileKey, prior_failures: u32) {
        if prior_failures > 0 {
            self.dirty_failures.insert(key.clone(), prior_failures);
        }
        self.dirty_seq += 1;
        self.dirty_paths.insert(key, self.dirty_seq);
    }

    /// Up to `limit` dirty keys for a point refresh, the oldest marks first, each with the
    /// sequence of its mark and its failure streak. Nothing before the overlay is initialized:
    /// a key marked then is left for the first full refresh, as every other point path does.
    pub(crate) fn capture_point_keys(
        &self,
        limit: usize,
    ) -> Vec<crate::point_refresh::CapturedKey> {
        if !self.initialized {
            return Vec::new();
        }
        let mut marks: Vec<(&FileKey, u64)> =
            self.dirty_paths.iter().map(|(key, seq)| (key, *seq)).collect();
        marks.sort_by(|left, right| left.1.cmp(&right.1).then_with(|| left.0.cmp(right.0)));
        marks
            .into_iter()
            .take(limit)
            .map(|(key, seq)| crate::point_refresh::CapturedKey {
                key: key.clone(),
                seq,
                prior_failures: self.dirty_failures.get(key).copied().unwrap_or(0),
            })
            .collect()
    }

    /// Marks a point refresh may answer: all of them once initialized, none before.
    pub(crate) fn point_backlog(&self) -> usize {
        if self.initialized {
            self.dirty_paths.len()
        } else {
            0
        }
    }

    /// The fence a point batch captures (the mark sequence) and the wholesale invalidation it
    /// must not outlive.
    pub(crate) fn point_fences(&self) -> (u64, u64) {
        (self.dirty_seq, self.wholesale_seq)
    }

    pub(crate) fn graph_context_provider_handle(&self) -> Option<Arc<dyn GraphContextProvider>> {
        self.graph_context_provider.clone()
    }

    /// Whether a batch prepared against this cache may still publish at all: nothing replaced
    /// the whole state it was prepared against.
    pub(crate) fn point_batch_stands(
        &self,
        batch: &crate::point_refresh::PreparedPointBatch,
    ) -> bool {
        self.initialized && self.wholesale_seq == batch.wholesale_seq
    }

    /// Per prepared key, whether it may settle: its mark is still the one captured, and no
    /// other point settlement answered it since.
    pub(crate) fn applicable_point_keys(
        &self,
        batch: &crate::point_refresh::PreparedPointBatch,
    ) -> Vec<bool> {
        batch
            .keys
            .iter()
            .map(|prepared| {
                let key = &prepared.captured.key;
                self.dirty_paths.get(key) == Some(&prepared.captured.seq)
                    && self.settled_seq.get(key).is_none_or(|settled| *settled <= batch.fence)
            })
            .collect()
    }

    /// Settle the applicable keys of `batch`, taking its prepared entries over. A key left out
    /// keeps its mark for the next batch. Returns how many settled, and how many of those lost
    /// their marks for good rather than being marked again by a fault.
    pub(crate) fn settle_point_batch(
        &mut self,
        batch: &mut crate::point_refresh::PreparedPointBatch,
        applicable: &[bool],
    ) -> (usize, usize) {
        let mut settled = 0;
        let mut cleared = 0;
        for (prepared, applies) in std::mem::take(&mut batch.keys).into_iter().zip(applicable) {
            if !applies {
                continue;
            }
            let key = prepared.captured.key;
            self.dirty_paths.remove(&key);
            self.dirty_failures.remove(&key);
            let action = match prepared.action {
                PointAction::Reindexed { mut entry, has_baseline } => {
                    // Vectors the live cache already holds for these exact inputs; the rest
                    // wait for the embedding pass, as every interactive point path leaves them.
                    entry.vector_documents = build_overlay_vectors(
                        None,
                        1,
                        &entry.lexical_documents,
                        &entry.embedding_inputs,
                        &mut self.embedding_cache,
                    )
                    .unwrap_or_default();
                    PointAction::Reindexed { entry, has_baseline }
                }
                other => other,
            };
            if prepared.resident_fed {
                self.resident_fed_count += 1;
            }
            let settlement = PointSettlement { action, store_fault: false };
            self.settle_point(key.clone(), settlement, prepared.captured.prior_failures);
            settled += 1;
            if !self.dirty_paths.contains_key(&key) {
                cleared += 1;
            }
        }
        (settled, cleared)
    }

    /// The paths currently marked dirty, awaiting a point refresh.
    pub fn dirty_paths_list(&self) -> Vec<FileKey> {
        self.dirty_paths.keys().cloned().collect()
    }

    /// An embedding failure is reported only after the pass reached its tail: every key it could
    /// build is published, and a key whose build was refused keeps its previous version and a
    /// dirty mark, exactly as a build fault always has. The cache therefore counts as built; any
    /// other error leaves it unbuilt for the next pass.
    fn settle_cold_refresh(
        &mut self,
        refreshed: Result<(), SearchError>,
    ) -> Result<(), SearchError> {
        if refreshed.as_ref().err().is_none_or(|error| error.embedding_failure().is_some()) {
            self.initialized = true;
        }
        refreshed
    }

    /// `allow_cold_scan` gates the only expensive operation here: a cold full-tree scan + read +
    /// chunk of every workspace file (`full_refresh_from_manifest`). The background warmup
    /// The embedding refresh passes `true`; status paths pass `false`
    /// so it stays O(cached) under the engine lock and answers from the Postgres baseline until the
    /// warmup (or the watcher's incremental path) populates the overlay. Without this gate a single
    /// query on an unwarmed overlay would block for minutes walking the whole tree.
    pub fn refresh_with_manifest(
        &mut self,
        manifest_fingerprints: &HashMap<FileKey, String>,
        roots: &WorkspaceRoots,
        embedder: Option<&Embedder>,
        batch_size: usize,
        store: &Store,
        allow_cold_scan: bool,
    ) -> Result<(), SearchError> {
        if allow_cold_scan {
            let refreshed = if !self.initialized || !self.watcher_mode || self.full_rescan_pending {
                self.full_refresh_from_manifest(
                    manifest_fingerprints,
                    roots,
                    embedder,
                    batch_size,
                    store,
                )
            } else if !self.dirty_paths.is_empty() {
                let dirty: Vec<FileKey> = self.dirty_paths.drain().map(|(key, _)| key).collect();
                self.refresh_dirty_paths_from_manifest(
                    dirty,
                    ManifestBaseline { fingerprints: manifest_fingerprints, store },
                    roots,
                    embedder,
                    batch_size,
                    &HashMap::new(),
                )
            } else {
                Ok(())
            };
            return self.settle_cold_refresh(refreshed);
        } else if self.initialized && !self.dirty_paths.is_empty() {
            // ReuseOnly: never cold-scan. An already-populated cache still applies the cheap
            // watcher-marked dirty-path refresh, but a `!watcher_mode` (polling) cache must NOT
            // re-run the full scan. An uninitialized cache stays empty (and `initialized` stays
            // false) so the next warmup/watcher pass still builds it.
            let dirty: Vec<FileKey> = self.dirty_paths.drain().map(|(key, _)| key).collect();
            self.refresh_dirty_paths_from_manifest(
                dirty,
                ManifestBaseline { fingerprints: manifest_fingerprints, store },
                roots,
                embedder,
                batch_size,
                &HashMap::new(),
            )?;
        }
        Ok(())
    }

    /// `allow_cold_scan` gates the cold full-tree scan + read + chunk (`full_refresh`). See
    /// [`Self::refresh_with_manifest`] for the rationale: only the background warmup
    /// An embedding refresh may pay that cost; status paths pass `false` and
    /// stay O(cached).
    pub fn refresh(
        &mut self,
        store: &Store,
        roots: &WorkspaceRoots,
        embedder: Option<&Embedder>,
        batch_size: usize,
        hash_mode: BaselineHashMode,
        allow_cold_scan: bool,
    ) -> Result<(), SearchError> {
        if allow_cold_scan {
            let baseline_files: HashMap<FileKey, Vec<u8>> =
                store.all_files_in_collection("code")?.into_iter().collect();
            let refreshed = if !self.initialized || !self.watcher_mode || self.full_rescan_pending {
                self.full_refresh(&baseline_files, roots, embedder, batch_size, hash_mode, store)
            } else if !self.dirty_paths.is_empty() {
                let dirty: Vec<FileKey> = self.dirty_paths.drain().map(|(key, _)| key).collect();
                self.refresh_dirty_paths(
                    dirty,
                    RawBaseline { files: &baseline_files, hash_mode },
                    roots,
                    embedder,
                    batch_size,
                    &HashMap::new(),
                )
            } else {
                Ok(())
            };
            return self.settle_cold_refresh(refreshed);
        } else if self.initialized && !self.dirty_paths.is_empty() {
            // ReuseOnly: never cold-scan. Only the cheap dirty-path refresh on an already-populated
            // cache; a `!watcher_mode` (polling) cache must NOT re-run the full scan, and an
            // uninitialized cache stays empty for the warmup/watcher to build later.
            let baseline_files: HashMap<FileKey, Vec<u8>> =
                store.all_files_in_collection("code")?.into_iter().collect();
            let dirty: Vec<FileKey> = self.dirty_paths.drain().map(|(key, _)| key).collect();
            self.refresh_dirty_paths(
                dirty,
                RawBaseline { files: &baseline_files, hash_mode },
                roots,
                embedder,
                batch_size,
                &HashMap::new(),
            )?;
        }
        Ok(())
    }

    fn full_refresh(
        &mut self,
        baseline_files: &HashMap<FileKey, Vec<u8>>,
        roots: &WorkspaceRoots,
        embedder: Option<&Embedder>,
        batch_size: usize,
        hash_mode: BaselineHashMode,
        store: &Store,
    ) -> Result<(), SearchError> {
        let scanned = scan_workspace_files(roots);
        record_scanned_spellings(store, &scanned);
        self.full_refresh_scanned(baseline_files, scanned, embedder, batch_size, hash_mode)
    }

    fn full_refresh_scanned(
        &mut self,
        baseline_files: &HashMap<FileKey, Vec<u8>>,
        scanned: ScannedFiles,
        embedder: Option<&Embedder>,
        batch_size: usize,
        hash_mode: BaselineHashMode,
    ) -> Result<(), SearchError> {
        let mut embedding_failure = None;
        let scan_is_clean = scanned.clean();
        let workspace_files = scanned.files;
        let mut seen_keys = HashSet::new();
        let mut hidden_paths = HashSet::new();
        let mut read_failures: HashSet<FileKey> = HashSet::new();
        let mut build_faults: HashSet<FileKey> = HashSet::new();

        for file in workspace_files {
            seen_keys.insert(file.key.clone());
            let baseline_hash = baseline_files.get(&file.key);

            let mut should_remove_cached_entry = false;
            // A live mark OR an unread debt distrusts the fingerprint (see the manifest twin).
            let key_is_marked =
                self.dirty_paths.contains_key(&file.key) || self.unread_keys.contains(&file.key);
            if let Some(entry) = self.entries.get_mut(&file.key) {
                // A marked key skips the equal-fingerprint gate: the mark is positive evidence
                // the fingerprint must not be trusted (an edit can leave (len, mtime,
                // canonical) unchanged), so the file is re-read below.
                if !key_is_marked && entry.fingerprint == file.fingerprint {
                    if baseline_hash.is_some_and(|stored_hash| stored_hash == &entry.file_hash) {
                        should_remove_cached_entry = true;
                    } else {
                        if baseline_hash.is_some() {
                            hidden_paths.insert(file.key.clone());
                        }
                        if entry.vector_documents.is_empty() {
                            // ReuseOnly passes `embedder = None`: this attaches any cached
                            // vectors and leaves the rest lexical-only. Embed (warmup) fills
                            // the gaps via the remote embedder. A failed attach leaves the
                            // entry lexical-only — the content is known and serving, so the
                            // unembedded-entries signal (not a mark) drives the catch-up.
                            match build_overlay_vectors(
                                embedder,
                                batch_size,
                                &entry.lexical_documents,
                                &entry.embedding_inputs,
                                &mut self.embedding_cache,
                            ) {
                                Ok(vectors) => {
                                    self.unembedded_entries -= usize::from(
                                        entry.vector_documents.len() < entry.embedding_inputs.len(),
                                    );
                                    entry.vector_documents = vectors;
                                    self.unembedded_entries += usize::from(
                                        entry.vector_documents.len() < entry.embedding_inputs.len(),
                                    );
                                }
                                Err(error) => {
                                    embedding_failure =
                                        embedding_failure.or(error.embedding_failure());
                                    tracing::warn!(
                                        "failed to attach overlay vectors; keeping the entry \
                                         lexical-only: {error}"
                                    );
                                }
                            }
                        }
                        continue;
                    }
                }
            }
            if should_remove_cached_entry {
                self.remove_entry(&file.key);
                continue;
            }

            let content = match std::fs::read_to_string(&file.abs_path) {
                Ok(content) => content,
                Err(_) => {
                    // Proven present, contents unknown: the key must stay dirty so a later
                    // refresh retries it.
                    read_failures.insert(file.key.clone());
                    continue;
                }
            };
            let file_hash = compute_file_hash(&content, hash_mode);
            if baseline_hash.is_some_and(|stored_hash| stored_hash == &file_hash) {
                self.remove_entry(&file.key);
                continue;
            }

            let provider = self.graph_context_provider.clone();
            match build_overlay_entry(
                &file.key,
                &content,
                file.fingerprint,
                file_hash,
                embedder,
                batch_size,
                &mut self.embedding_cache,
                provider.as_deref(),
                None,
            ) {
                Ok(entry) => {
                    if baseline_hash.is_some() {
                        hidden_paths.insert(file.key.clone());
                    }
                    self.insert_entry(file.key, entry);
                }
                Err(error) => {
                    embedding_failure = embedding_failure.or(error.embedding_failure());
                    // The key's prior entry and hiding survive (like a failed read): the
                    // fault is the builder's, and the pass must still reach its tail.
                    tracing::warn!(
                        root = %file.key.root_id,
                        path = %file.key.path,
                        "failed to build an overlay entry; keeping the previous version: {error}"
                    );
                    build_faults.insert(file.key);
                }
            }
        }

        if scan_is_clean {
            self.retain_entries(|key, _| seen_keys.contains(key));
            for key in baseline_files.keys() {
                if !seen_keys.contains(key) {
                    hidden_paths.insert(key.clone());
                }
            }
            // A failed read or build proves nothing about the baseline, so the key's prior
            // hiding survives the whole-replace — changing it would serve two versions at
            // once, or neither.
            for key in read_failures.iter().chain(&build_faults) {
                if self.hidden_paths.contains(key) {
                    hidden_paths.insert(key.clone());
                }
            }
            self.hidden_paths = hidden_paths;
        } else {
            self.merge_partial_hidden(&seen_keys, &read_failures, &build_faults, &hidden_paths);
        }
        let empty_gate = HashSet::new();
        let verdict = PublicationVerdict {
            scan_is_clean,
            seen_keys: &seen_keys,
            read_failures: &read_failures,
            build_faults: &build_faults,
            gate_skipped: &empty_gate,
            fenced: &empty_gate,
            superseded: None,
        };
        let to_consume = self.publication_consumption(&verdict);
        self.finish_publication(&verdict, &to_consume, true);
        self.baseline_identity = None;
        // An in-place full publication replaces the whole state: any plan whose Phase A
        // started before this moment must not publish over it.
        self.bump_wholesale();
        embedding_failure.map_or(Ok(()), |failure| Err(failure.into()))
    }

    /// The hidden-path merge of an unclean full publication: only a SEEN key may change its
    /// hiding — the scan proved something about it — while an unseen key keeps its prior state,
    /// because absence from an incomplete scan proves nothing. A seen-but-unread or
    /// build-faulted key is skipped too: changing its hiding would serve the baseline and the
    /// stale overlay both, or neither.
    fn merge_partial_hidden(
        &mut self,
        seen_keys: &HashSet<FileKey>,
        read_failures: &HashSet<FileKey>,
        build_faults: &HashSet<FileKey>,
        fresh_hidden: &HashSet<FileKey>,
    ) {
        for key in seen_keys {
            if read_failures.contains(key) || build_faults.contains(key) {
                continue;
            }
            if fresh_hidden.contains(key) {
                self.hidden_paths.insert(key.clone());
            } else {
                self.hidden_paths.remove(key);
            }
        }
    }

    /// Step 1 of the shared publication tail: which live marks this publication SUPERSEDES —
    /// and may consume once the persist lands. A key is superseded only if the pass settled
    /// it (read it, or a clean scan proved it absent) and, on the planned path, only if its
    /// mark has not been re-bumped since the pre-plan snapshot.
    fn publication_consumption(&self, verdict: &PublicationVerdict<'_>) -> HashSet<FileKey> {
        let settled = |key: &FileKey| {
            !verdict.read_failures.contains(key)
                && !verdict.build_faults.contains(key)
                && !verdict.gate_skipped.contains(key)
                && !verdict.fenced.contains(key)
                && (verdict.scan_is_clean || verdict.seen_keys.contains(key))
        };
        match verdict.superseded {
            Some(dirty_before) => dirty_before
                .iter()
                .filter(|(key, seq)| self.dirty_paths.get(*key) == Some(*seq) && settled(key))
                .map(|(key, _)| key.clone())
                .collect(),
            None => self.dirty_paths.keys().filter(|key| settled(key)).cloned().collect(),
        }
    }

    /// The plan's rows minus every key whose mark will remain LIVE after this publication: a
    /// live mark is positive evidence the row must not be trusted, and the unconditional
    /// replace-save then drops the key's old row simply by not writing a new one.
    fn split_rows_by_live_marks(
        &self,
        updated: HashMap<FileKey, crate::store::PersistedFingerprint>,
        to_consume: &HashSet<FileKey>,
        fenced: &HashSet<FileKey>,
    ) -> HashMap<FileKey, crate::store::PersistedFingerprint> {
        updated
            .into_iter()
            .filter(|(key, _)| {
                (!self.dirty_paths.contains_key(key) || to_consume.contains(key))
                    && !fenced.contains(key)
            })
            .collect()
    }

    /// Step 4 of the shared publication tail — the ONLY place a full publication touches the
    /// marks, the budget and `unread_keys`, for all three paths (raw, manifest, planned).
    /// Marks are consumed strictly AFTER the persist outcome is known and only on success:
    /// a consumed mark promises the knowledge reached every carrier including the disk.
    /// The full tail never charges the bounded point budget — full-pass retries are paced by
    /// the retry driver's backoff, not by the per-query budget — so a store or read fault
    /// here can never exhaust a mark.
    fn finish_publication(
        &mut self,
        verdict: &PublicationVerdict<'_>,
        to_consume: &HashSet<FileKey>,
        persist_ok: bool,
    ) {
        if persist_ok {
            for key in to_consume {
                self.dirty_paths.remove(key);
            }
        }
        // A settled key ends its failure streak: the budget counts CONSECUTIVE failures, and
        // without this reset unrelated failures spread over time would add up to a drop. A
        // FENCED key's streak belongs to a FRESHER settlement — the stale plan's old success
        // must not erase it.
        for key in verdict.seen_keys {
            if !verdict.read_failures.contains(key)
                && !verdict.build_faults.contains(key)
                && !verdict.fenced.contains(key)
            {
                self.dirty_failures.remove(key);
            }
        }
        // A FENCED key's faults are the stale plan's, not the state's: a fresher point
        // settlement already resolved the key, and re-marking it (or re-opening its unread
        // debt) would make yesterday's failure current again.
        for key in verdict.read_failures {
            if verdict.fenced.contains(key) {
                continue;
            }
            tracing::warn!(
                root = %key.root_id,
                path = %key.path,
                "full overlay refresh could not read a seen file; keeping its previous version \
                 and retrying"
            );
            let prior_failures = self.dirty_failures.remove(key).unwrap_or(0);
            self.retain_dirty_uncharged(key.clone(), prior_failures);
            self.unread_keys.insert(key.clone());
        }
        for key in verdict.build_faults {
            if verdict.fenced.contains(key) {
                continue;
            }
            let prior_failures = self.dirty_failures.remove(key).unwrap_or(0);
            self.retain_dirty_uncharged(key.clone(), prior_failures);
        }
        // The unread obligation is settled by proof, either way: a read this pass settles it,
        // and absence from a CLEAN scan proves the file deleted — without the latter a
        // deleted-after-fault file would keep the retry condition true forever.
        if verdict.scan_is_clean {
            self.unread_keys
                .retain(|key| verdict.read_failures.contains(key) || verdict.fenced.contains(key));
        } else {
            for key in verdict.seen_keys {
                if !verdict.read_failures.contains(key) && !verdict.fenced.contains(key) {
                    self.unread_keys.remove(key);
                }
            }
        }
        self.full_rescan_pending = !verdict.scan_is_clean || !persist_ok;
    }

    /// Remove a point-refresh key that is PROVABLY gone: the entry goes, and the baseline copy
    /// (when there is one) is hidden, exactly as a clean full scan would settle it.
    fn remove_point_entry(&mut self, key: FileKey, has_baseline: bool) {
        self.remove_entry(&key);
        if has_baseline {
            self.hidden_paths.insert(key);
        } else {
            self.hidden_paths.remove(&key);
        }
    }

    /// The single place a point-path settlement touches the carriers. The classification
    /// branches in the two dirty loops only DECIDE; every mutation of the entries, the hiding,
    /// the marks, the budget and `unread_keys` happens here, in one total `match` — so a new
    /// branch cannot forget an obligation.
    fn settle_point(&mut self, key: FileKey, settlement: PointSettlement, prior_failures: u32) {
        self.record_settlement(&key);
        let PointSettlement { action, store_fault } = settlement;
        match action {
            PointAction::Reindexed { entry, has_baseline } => {
                if has_baseline {
                    self.hidden_paths.insert(key.clone());
                } else {
                    self.hidden_paths.remove(&key);
                }
                self.unread_keys.remove(&key);
                self.insert_entry(key.clone(), entry);
                if store_fault {
                    self.retain_dirty_uncharged(key, prior_failures);
                }
            }
            PointAction::BaselineEqual => {
                self.remove_entry(&key);
                self.hidden_paths.remove(&key);
                self.unread_keys.remove(&key);
                if store_fault {
                    self.retain_dirty_uncharged(key, prior_failures);
                }
            }
            PointAction::ProvenGone { has_baseline } => {
                self.unread_keys.remove(&key);
                if store_fault {
                    self.retain_dirty_uncharged(key.clone(), prior_failures);
                }
                self.remove_point_entry(key, has_baseline);
            }
            PointAction::FileFault { reason } => {
                // Proven present, contents unknown: the durable signal outlives even a
                // budget-exhausted mark, so a later full pass still re-reads the file.
                self.unread_keys.insert(key.clone());
                if store_fault {
                    self.retain_dirty_uncharged(key, prior_failures);
                } else {
                    self.retain_dirty_after_failure(key, prior_failures, reason);
                }
            }
            PointAction::BuildFault => {
                self.retain_dirty_uncharged(key, prior_failures);
            }
        }
    }

    /// Retract one key's persisted fingerprint row: its "verified against the manifest" claim
    /// did not survive whatever the caller just observed. Returns whether the retraction
    /// LANDED — a caller about to consume the dirty mark must keep it on failure, or the stale
    /// row outlives the process with nothing left to retry it.
    fn retract_fingerprint_row(store: &Store, key: &FileKey) -> bool {
        match store.delete_overlay_fingerprint_entries(std::slice::from_ref(key)) {
            Ok(()) => true,
            Err(error) => {
                tracing::warn!("failed to retract overlay fingerprint row: {error}");
                false
            }
        }
    }

    /// Persist the fingerprint rows a full publication verified. A row asserts "this file was
    /// verified against the manifest", so the table must end up holding exactly what THIS pass
    /// proved — the replace-save runs UNCONDITIONALLY, and an empty result legitimately
    /// empties the table (the row of an unseen deleted file has no other retraction channel).
    /// On a failed save the transactional store kept the OLD table, whose superseded rows are
    /// then retracted point-wise, best effort: the surviving marks die with the process, while
    /// a row of a same-stat edit would live to suppress the re-read after a restart. A
    /// read-failed key's row is retracted explicitly either way.
    /// Returns whether every persistence step LANDED; the caller must not consume any mark
    /// otherwise.
    fn persist_fingerprint_rows(
        store: &Store,
        snapshot_id: &str,
        updated: &HashMap<FileKey, crate::store::PersistedFingerprint>,
        read_failures: &HashSet<FileKey>,
        consumed: &HashSet<FileKey>,
    ) -> bool {
        let mut ok = true;
        if let Err(error) = store.save_overlay_fingerprint_cache(snapshot_id, updated) {
            tracing::warn!("failed to persist overlay fingerprint cache: {error}");
            ok = false;
            let mut doomed: Vec<FileKey> = consumed.iter().cloned().collect();
            match store.load_overlay_fingerprint_cache(snapshot_id) {
                Ok(prior) => doomed.extend(
                    prior.unwrap_or_default().into_keys().filter(|key| !updated.contains_key(key)),
                ),
                Err(error) => {
                    tracing::warn!("failed to enumerate stale overlay fingerprint rows: {error}");
                }
            }
            if !doomed.is_empty() {
                if let Err(error) = store.delete_overlay_fingerprint_entries(&doomed) {
                    tracing::warn!("failed to retract overlay fingerprint rows: {error}");
                }
            }
        }
        if !read_failures.is_empty() {
            let keys: Vec<FileKey> = read_failures.iter().cloned().collect();
            if let Err(error) = store.delete_overlay_fingerprint_entries(&keys) {
                tracing::warn!("failed to retract overlay fingerprint rows: {error}");
                ok = false;
            }
        }
        ok
    }

    fn refresh_dirty_paths(
        &mut self,
        dirty_keys: Vec<FileKey>,
        baseline: RawBaseline<'_>,
        roots: &WorkspaceRoots,
        embedder: Option<&Embedder>,
        batch_size: usize,
        snapshots: &HashMap<FileKey, ModuleSnapshot>,
    ) -> Result<(), SearchError> {
        let mut embedding_failure = None;
        let RawBaseline { files: baseline_files, hash_mode } = baseline;
        // Each drained key CLASSIFIES into a [`PointSettlement`] and settles at once through
        // [`Self::settle_point`]: a later fault in the same batch never disturbs an earlier
        // key's obligations, and no per-key fault aborts the loop — the caller has already
        // drained the dirty set, so an abort would strand every unprocessed mark.
        for key in dirty_keys {
            // Removing the count here clears it on success (the common path) and hands the
            // prior value to the settlement, keeping the failure streak consecutive.
            let prior_failures = self.dirty_failures.remove(&key).unwrap_or(0);
            let baseline =
                PointBaseline::Raw { hash: baseline_files.get(&key).cloned(), mode: hash_mode };
            let provider = self.graph_context_provider.clone();
            let read = classify_point(
                &key,
                &baseline,
                roots,
                snapshots,
                provider.as_deref(),
                embedder,
                batch_size,
                &mut self.embedding_cache,
            );
            embedding_failure = embedding_failure.or(read.embedding_failure);
            if read.resident_fed {
                self.resident_fed_count += 1;
            }
            let settlement = PointSettlement { action: read.action, store_fault: false };
            self.settle_point(key, settlement, prior_failures);
        }
        embedding_failure.map_or(Ok(()), |failure| Err(failure.into()))
    }

    fn full_refresh_from_manifest(
        &mut self,
        manifest_fingerprints: &HashMap<FileKey, String>,
        roots: &WorkspaceRoots,
        embedder: Option<&Embedder>,
        batch_size: usize,
        store: &Store,
    ) -> Result<(), SearchError> {
        let scanned = scan_workspace_files(roots);
        record_scanned_spellings(store, &scanned);
        self.full_refresh_from_manifest_scanned(
            manifest_fingerprints,
            scanned,
            embedder,
            batch_size,
            store,
        )
    }

    fn full_refresh_from_manifest_scanned(
        &mut self,
        manifest_fingerprints: &HashMap<FileKey, String>,
        scanned: ScannedFiles,
        embedder: Option<&Embedder>,
        batch_size: usize,
        store: &Store,
    ) -> Result<(), SearchError> {
        let mut embedding_failure = None;
        let baseline_identity =
            store.load_baseline_manifest().ok().flatten().map(|r| (r.snapshot_id, r.fingerprint));
        let manifest_snapshot_id = baseline_identity.as_ref().map(|r| r.0.as_str()).unwrap_or("");
        let persisted = store
            .load_overlay_fingerprint_cache(manifest_snapshot_id)
            .unwrap_or(None)
            .unwrap_or_default();

        if self.embedding_cache.is_empty() {
            if let Some(embedder) = embedder {
                let model_id = embedder.model_id();
                let dim = embedder.dim();
                match store.load_overlay_embedding_cache(model_id, dim) {
                    Ok(cached) if !cached.is_empty() => {
                        tracing::info!(
                            model_id,
                            dim,
                            cached_embeddings = cached.len(),
                            "loaded persisted overlay embedding cache"
                        );
                        self.embedding_cache = cached;
                    }
                    _ => {}
                }
            }
        }

        let scan_is_clean = scanned.clean();
        let workspace_files = scanned.files;
        let mut seen_keys = HashSet::new();
        let mut hidden_paths = HashSet::new();
        let mut updated_persisted = HashMap::new();
        let mut read_failures: HashSet<FileKey> = HashSet::new();
        let mut build_faults: HashSet<FileKey> = HashSet::new();

        for file in &workspace_files {
            seen_keys.insert(file.key.clone());
            let baseline_fingerprint = manifest_fingerprints.get(&file.key);

            let mut should_remove_cached_entry = false;
            // A live mark OR an unread debt distrusts every fingerprint: both mean the
            // content behind the stat is unverified, and after the point budget drops the
            // mark the debt is the only veto left against a same-stat gate hit.
            let key_is_marked =
                self.dirty_paths.contains_key(&file.key) || self.unread_keys.contains(&file.key);
            // The row a pass proves must reach `updated_persisted` on EVERY settled branch:
            // the unconditional replace-save writes exactly that map, and a branch that
            // settles a key without collecting its still-valid row would wipe it.
            let row_still_matches = |cached: &crate::store::PersistedFingerprint| {
                cached.file_size == file.fingerprint.len
                    && fingerprint_mtime_matches(file.fingerprint.modified, cached)
                    && fingerprint_canonical_matches(&file.fingerprint, cached)
            };
            if let Some(entry) = self.entries.get_mut(&file.key) {
                // A marked key skips the equal-fingerprint gate: the mark is positive evidence
                // the fingerprint must not be trusted (an edit can leave (len, mtime,
                // canonical) unchanged), so the file is re-read below.
                if !key_is_marked && entry.fingerprint == file.fingerprint {
                    if let Some(cached) = persisted.get(&file.key) {
                        if row_still_matches(cached) {
                            updated_persisted.insert(file.key.clone(), cached.clone());
                        }
                    }
                    let local_fp =
                        fingerprint_overlay_documents(&entry.lexical_documents, &file.key.path);
                    if baseline_fingerprint.is_some_and(|stored| stored == &local_fp) {
                        should_remove_cached_entry = true;
                    } else {
                        if baseline_fingerprint.is_some() {
                            hidden_paths.insert(file.key.clone());
                        }
                        if entry.vector_documents.is_empty() {
                            // ReuseOnly passes `embedder = None`: this attaches any cached
                            // vectors and leaves the rest lexical-only. Embed (warmup) fills
                            // the gaps via the remote embedder. A failed attach leaves the
                            // entry lexical-only — the content is known and serving, so the
                            // unembedded-entries signal (not a mark) drives the catch-up.
                            match build_overlay_vectors(
                                embedder,
                                batch_size,
                                &entry.lexical_documents,
                                &entry.embedding_inputs,
                                &mut self.embedding_cache,
                            ) {
                                Ok(vectors) => {
                                    self.unembedded_entries -= usize::from(
                                        entry.vector_documents.len() < entry.embedding_inputs.len(),
                                    );
                                    entry.vector_documents = vectors;
                                    self.unembedded_entries += usize::from(
                                        entry.vector_documents.len() < entry.embedding_inputs.len(),
                                    );
                                }
                                Err(error) => {
                                    embedding_failure =
                                        embedding_failure.or(error.embedding_failure());
                                    tracing::warn!(
                                        "failed to attach overlay vectors; keeping the entry \
                                         lexical-only: {error}"
                                    );
                                }
                            }
                        }
                        continue;
                    }
                }
            }
            if should_remove_cached_entry {
                self.remove_entry(&file.key);
                continue;
            }

            if let Some(cached) = persisted.get(&file.key) {
                if !key_is_marked && row_still_matches(cached) {
                    updated_persisted.insert(file.key.clone(), cached.clone());

                    if baseline_fingerprint
                        .is_some_and(|stored| stored == &cached.content_fingerprint)
                    {
                        self.remove_entry(&file.key);
                        continue;
                    }
                }
            }

            let content = match std::fs::read_to_string(&file.abs_path) {
                Ok(content) => content,
                Err(_) => {
                    // Proven present, contents unknown: the key must stay dirty so a later
                    // refresh retries it.
                    read_failures.insert(file.key.clone());
                    continue;
                }
            };
            let file_hash = normalized_file_hash_for_content(&content);
            let local_fp = fingerprint_content(&content, &file.key.path);

            if let Some((secs, nanos)) = mtime_to_secs_nanos(file.fingerprint.modified) {
                updated_persisted.insert(
                    file.key.clone(),
                    crate::store::PersistedFingerprint {
                        file_size: file.fingerprint.len,
                        file_mtime_secs: secs,
                        file_mtime_nanos: nanos,
                        content_fingerprint: local_fp.clone(),
                        canonical: file.fingerprint.canonical.to_string_lossy().into_owned(),
                    },
                );
            }

            if baseline_fingerprint.is_some_and(|stored| stored == &local_fp) {
                self.remove_entry(&file.key);
                continue;
            }

            let provider = self.graph_context_provider.clone();
            match build_overlay_entry(
                &file.key,
                &content,
                file.fingerprint.clone(),
                file_hash,
                embedder,
                batch_size,
                &mut self.embedding_cache,
                provider.as_deref(),
                None,
            ) {
                Ok(entry) => {
                    if baseline_fingerprint.is_some() {
                        hidden_paths.insert(file.key.clone());
                    }
                    self.insert_entry(file.key.clone(), entry);
                }
                Err(error) => {
                    embedding_failure = embedding_failure.or(error.embedding_failure());
                    // The key's prior entry and hiding survive (like a failed read): the
                    // fault is the builder's, and the pass must still reach its tail.
                    tracing::warn!(
                        root = %file.key.root_id,
                        path = %file.key.path,
                        "failed to build an overlay entry; keeping the previous version: {error}"
                    );
                    build_faults.insert(file.key.clone());
                }
            }
        }

        if scan_is_clean {
            self.retain_entries(|key, _| seen_keys.contains(key));
            for key in manifest_fingerprints.keys() {
                if !seen_keys.contains(key) {
                    hidden_paths.insert(key.clone());
                }
            }
            // A failed read or build proves nothing about the baseline, so the key's prior
            // hiding survives the whole-replace — changing it would serve two versions at
            // once, or neither.
            for key in read_failures.iter().chain(&build_faults) {
                if self.hidden_paths.contains(key) {
                    hidden_paths.insert(key.clone());
                }
            }
            self.hidden_paths = hidden_paths;
        } else {
            self.merge_partial_hidden(&seen_keys, &read_failures, &build_faults, &hidden_paths);
        }
        let empty_gate = HashSet::new();
        let verdict = PublicationVerdict {
            scan_is_clean,
            seen_keys: &seen_keys,
            read_failures: &read_failures,
            build_faults: &build_faults,
            gate_skipped: &empty_gate,
            fenced: &empty_gate,
            superseded: None,
        };
        let to_consume = self.publication_consumption(&verdict);
        let rows = self.split_rows_by_live_marks(updated_persisted, &to_consume, &empty_gate);
        let persist_ok = Self::persist_fingerprint_rows(
            store,
            manifest_snapshot_id,
            &rows,
            &read_failures,
            &to_consume,
        );
        self.finish_publication(&verdict, &to_consume, persist_ok);
        self.baseline_identity = baseline_identity;
        // An in-place full publication replaces the whole state: any plan whose Phase A
        // started before this moment must not publish over it.
        self.bump_wholesale();

        if let Some(embedder) = embedder {
            if !self.embedding_cache.is_empty() {
                if let Err(error) = store.save_overlay_embedding_cache(
                    embedder.model_id(),
                    embedder.dim(),
                    &self.embedding_cache,
                ) {
                    tracing::warn!("failed to persist overlay embedding cache: {error}");
                }
            }
        }

        embedding_failure.map_or(Ok(()), |failure| Err(failure.into()))
    }

    /// Phase A: plan a manifest-driven full refresh without holding any live lock.
    ///
    /// Reads workspace files and the persisted overlay caches through `store` (a standalone
    /// connection) and the supplied read-only `warm_embeddings` clone, decides which files belong
    /// in the overlay, builds their lexical docs and embedding inputs, and collects the distinct
    /// `content_hash -> input` pairs that lack a warm vector. Mutates nothing shared: the result
    /// is a [`RefreshPlan`] applied later by [`Self::publish_plan`].
    #[cfg(test)]
    pub fn plan_full_refresh_from_manifest(
        manifest_fingerprints: &HashMap<FileKey, String>,
        roots: &WorkspaceRoots,
        store: &Store,
        warm_embeddings: &HashMap<String, Vec<f32>>,
        graph_context: Option<&dyn GraphContextProvider>,
        distrusted: &HashSet<FileKey>,
    ) -> Result<RefreshPlan, SearchError> {
        Self::plan_full_refresh_from_manifest_with_embedder(
            manifest_fingerprints,
            roots,
            store,
            warm_embeddings,
            graph_context,
            distrusted,
            None,
        )
    }

    pub(crate) fn plan_full_refresh_from_manifest_with_embedder(
        manifest_fingerprints: &HashMap<FileKey, String>,
        roots: &WorkspaceRoots,
        store: &Store,
        warm_embeddings: &HashMap<String, Vec<f32>>,
        graph_context: Option<&dyn GraphContextProvider>,
        distrusted: &HashSet<FileKey>,
        embedder: Option<&Embedder>,
    ) -> Result<RefreshPlan, SearchError> {
        let scanned = scan_workspace_files(roots);
        record_scanned_spellings(store, &scanned);
        Self::plan_full_refresh_from_manifest_scanned_with_embedder(
            manifest_fingerprints,
            scanned,
            store,
            warm_embeddings,
            graph_context,
            distrusted,
            embedder,
        )
    }

    #[cfg(test)]
    fn plan_full_refresh_from_manifest_scanned(
        manifest_fingerprints: &HashMap<FileKey, String>,
        scanned: ScannedFiles,
        store: &Store,
        warm_embeddings: &HashMap<String, Vec<f32>>,
        graph_context: Option<&dyn GraphContextProvider>,
        distrusted: &HashSet<FileKey>,
    ) -> Result<RefreshPlan, SearchError> {
        Self::plan_full_refresh_from_manifest_scanned_with_embedder(
            manifest_fingerprints,
            scanned,
            store,
            warm_embeddings,
            graph_context,
            distrusted,
            None,
        )
    }

    fn plan_full_refresh_from_manifest_scanned_with_embedder(
        manifest_fingerprints: &HashMap<FileKey, String>,
        scanned: ScannedFiles,
        store: &Store,
        warm_embeddings: &HashMap<String, Vec<f32>>,
        graph_context: Option<&dyn GraphContextProvider>,
        distrusted: &HashSet<FileKey>,
        embedder: Option<&Embedder>,
    ) -> Result<RefreshPlan, SearchError> {
        let baseline_identity =
            store.load_baseline_manifest().ok().flatten().map(|r| (r.snapshot_id, r.fingerprint));
        let snapshot_id = baseline_identity.as_ref().map(|r| r.0.clone()).unwrap_or_default();
        let persisted =
            store.load_overlay_fingerprint_cache(&snapshot_id).unwrap_or(None).unwrap_or_default();

        let scan_unreadable = scanned.unreadable;
        let scan_canonical_fallbacks = scanned.canonical_fallbacks;
        let workspace_files = scanned.files;
        let mut seen_keys = HashSet::new();
        let mut hidden_paths = HashSet::new();
        let mut updated_persisted = HashMap::new();
        let mut entries: Vec<(FileKey, PlannedEntry)> = Vec::new();
        let mut missing_embeddings: HashMap<String, String> = HashMap::new();
        let reused_embeddings = HashMap::new();
        let mut read_failures: HashSet<FileKey> = HashSet::new();
        let mut gate_skipped: HashSet<FileKey> = HashSet::new();

        for file in &workspace_files {
            seen_keys.insert(file.key.clone());
            let baseline_fingerprint = manifest_fingerprints.get(&file.key);

            if let Some(cached) = persisted.get(&file.key) {
                // The unread debt distrusts the row even here: the caller's own retraction
                // landed at fault time, but the SHARED table may have been repopulated by a
                // neighbouring daemon whose knowledge predates the local fault.
                if !distrusted.contains(&file.key)
                    && cached.file_size == file.fingerprint.len
                    && fingerprint_mtime_matches(file.fingerprint.modified, cached)
                    && fingerprint_canonical_matches(&file.fingerprint, cached)
                {
                    updated_persisted.insert(file.key.clone(), cached.clone());

                    if baseline_fingerprint
                        .is_some_and(|stored| stored == &cached.content_fingerprint)
                    {
                        gate_skipped.insert(file.key.clone());
                        continue;
                    }
                }
            }

            let content = match std::fs::read_to_string(&file.abs_path) {
                Ok(content) => content,
                Err(_) => {
                    // A seen-but-unread file is proven present with unknown contents; the
                    // publication keeps its prior state and retries, so the plan must carry the
                    // failure rather than silently plan nothing for the key.
                    read_failures.insert(file.key.clone());
                    continue;
                }
            };
            let file_hash = normalized_file_hash_for_content(&content);
            let local_fp = fingerprint_content(&content, &file.key.path);

            if let Some((secs, nanos)) = mtime_to_secs_nanos(file.fingerprint.modified) {
                updated_persisted.insert(
                    file.key.clone(),
                    crate::store::PersistedFingerprint {
                        file_size: file.fingerprint.len,
                        file_mtime_secs: secs,
                        file_mtime_nanos: nanos,
                        content_fingerprint: local_fp.clone(),
                        canonical: file.fingerprint.canonical.to_string_lossy().into_owned(),
                    },
                );
            }

            if baseline_fingerprint.is_some_and(|stored| stored == &local_fp) {
                continue;
            }

            let (lexical_documents, embedding_inputs) = match build_overlay_documents_prepared(
                &file.key,
                &content,
                graph_context,
                None,
                embedder,
            ) {
                Ok(documents) => documents,
                // An embedding refusal stops the pass; anything else is a context that could
                // not be rendered.
                Err(error) if error.embedding_failure().is_some() => return Err(error),
                Err(_) => {
                    // The file was read but its context was not: an entry without it would read
                    // as complete, so the pass counts it with the failures it cannot vouch for.
                    read_failures.insert(file.key.clone());
                    continue;
                }
            };
            for input in &embedding_inputs {
                let key = overlay_embedding_key(input);
                if !warm_embeddings.contains_key(&key) {
                    missing_embeddings.entry(key).or_insert_with(|| input.clone());
                }
            }

            if baseline_fingerprint.is_some() {
                hidden_paths.insert(file.key.clone());
            }
            entries.push((
                file.key.clone(),
                PlannedEntry {
                    fingerprint: file.fingerprint.clone(),
                    file_hash,
                    lexical_documents,
                    embedding_inputs,
                },
            ));
        }

        // Absence from an incomplete scan proves nothing, so only a clean scan may read "not
        // seen" as "deleted from disk" and hide the baseline copy.
        if scan_unreadable == 0 && scan_canonical_fallbacks == 0 {
            for key in manifest_fingerprints.keys() {
                if !seen_keys.contains(key) {
                    hidden_paths.insert(key.clone());
                }
            }
        }

        Ok(RefreshPlan {
            snapshot_id,
            baseline_identity,
            manifest_backed: true,
            entries,
            hidden_paths,
            updated_persisted,
            missing_embeddings,
            reused_embeddings,
            scan_unreadable,
            scan_canonical_fallbacks,
            seen_keys,
            read_failures,
            gate_skipped,
        })
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "the scan, baseline, authority, and embed inputs are independent; a wrapper would only rename them"
    )]
    fn plan_full_refresh_from_local_scanned_with_embedder(
        baseline_files: &HashMap<FileKey, Vec<u8>>,
        scanned: ScannedFiles,
        store: &Store,
        warm_embeddings: &HashMap<String, Vec<f32>>,
        graph_context: Option<&dyn GraphContextProvider>,
        _distrusted: &HashSet<FileKey>,
        embedder: &Embedder,
        hash_mode: BaselineHashMode,
    ) -> Result<RefreshPlan, SearchError> {
        let scan_unreadable = scanned.unreadable;
        let scan_canonical_fallbacks = scanned.canonical_fallbacks;
        let workspace_files = scanned.files;
        let mut seen_keys = HashSet::new();
        let mut hidden_paths = HashSet::new();
        let mut entries = Vec::new();
        let mut missing_embeddings = HashMap::new();
        let mut reused_embeddings = HashMap::new();
        let mut read_failures = HashSet::new();
        let gate_skipped = HashSet::new();

        for file in &workspace_files {
            seen_keys.insert(file.key.clone());
            let content = match std::fs::read_to_string(&file.abs_path) {
                Ok(content) => content,
                Err(_) => {
                    read_failures.insert(file.key.clone());
                    continue;
                }
            };
            let file_hash = compute_file_hash(&content, hash_mode);
            if baseline_files.get(&file.key).is_some_and(|stored| stored == &file_hash) {
                continue;
            }

            let (lexical_documents, embedding_inputs) = match build_overlay_documents_prepared(
                &file.key,
                &content,
                graph_context,
                None,
                Some(embedder),
            ) {
                Ok(documents) => documents,
                Err(error) if error.embedding_failure().is_some() => return Err(error),
                Err(_) => {
                    read_failures.insert(file.key.clone());
                    continue;
                }
            };

            // Local baseline vectors are eligible only for planned documents from a changed
            // file. The Store first matches semantic payload/profile metadata, then reads BLOBs
            // for those exact hits; never clone the complete base vector corpus into memory.
            let reuse_plan = store.plan_reusable_embeddings_for_documents(
                &file.key.root_id,
                &file.key.path,
                "code",
                &lexical_documents,
                embedder.model_id(),
                embedder.dim(),
                embedder.token_layout_claim(),
            )?;
            let hydrated = store.hydrate_reusable_embedding_plan(
                &file.key.root_id,
                &file.key.path,
                "code",
                &reuse_plan,
                embedder.model_id(),
                embedder.dim(),
                embedder.token_layout_claim(),
            )?;
            for candidate in hydrated.candidates.into_iter().flatten() {
                // Hydration revalidates the exact payload and profile in its own transaction.
                // This map is scoped to this planned refresh, not persisted as a cache.
                if !warm_embeddings.contains_key(&candidate.semantic_key) {
                    reused_embeddings.insert(candidate.semantic_key, candidate.vector);
                }
            }

            for input in &embedding_inputs {
                let key = overlay_embedding_key(input);
                if !warm_embeddings.contains_key(&key) && !reused_embeddings.contains_key(&key) {
                    missing_embeddings.entry(key).or_insert_with(|| input.clone());
                }
            }
            if baseline_files.contains_key(&file.key) {
                hidden_paths.insert(file.key.clone());
            }
            entries.push((
                file.key.clone(),
                PlannedEntry {
                    fingerprint: file.fingerprint.clone(),
                    file_hash,
                    lexical_documents,
                    embedding_inputs,
                },
            ));
        }

        if scan_unreadable == 0 && scan_canonical_fallbacks == 0 {
            for key in baseline_files.keys() {
                if !seen_keys.contains(key) {
                    hidden_paths.insert(key.clone());
                }
            }
        }

        Ok(RefreshPlan {
            snapshot_id: String::new(),
            baseline_identity: None,
            manifest_backed: false,
            entries,
            hidden_paths,
            updated_persisted: HashMap::new(),
            missing_embeddings,
            reused_embeddings,
            scan_unreadable,
            scan_canonical_fallbacks,
            seen_keys,
            read_failures,
            gate_skipped,
        })
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "the roots, baseline, store, authority, and embed inputs are independent; a wrapper would only rename them"
    )]
    pub(crate) fn plan_full_refresh_from_local_with_embedder(
        baseline_files: &HashMap<FileKey, Vec<u8>>,
        roots: &WorkspaceRoots,
        store: &Store,
        warm_embeddings: &HashMap<String, Vec<f32>>,
        graph_context: Option<&dyn GraphContextProvider>,
        distrusted: &HashSet<FileKey>,
        embedder: &Embedder,
        hash_mode: BaselineHashMode,
    ) -> Result<RefreshPlan, SearchError> {
        let scanned = scan_workspace_files(roots);
        record_scanned_spellings(store, &scanned);
        Self::plan_full_refresh_from_local_scanned_with_embedder(
            baseline_files,
            scanned,
            store,
            warm_embeddings,
            graph_context,
            distrusted,
            embedder,
            hash_mode,
        )
    }

    /// Phase C: apply a [`RefreshPlan`] atomically under the inner cache lock.
    ///
    /// Merges `new_embeddings` (Phase B output) into `embedding_cache`, assembles each planned
    /// entry's vectors from the merged cache, swaps `entries`/`hidden_paths` in one shot (so a
    /// concurrent reader never sees a half-embedded file), then persists the fingerprint and
    /// embedding caches once. The merge is last-writer-wins on the embedding key, which is value
    /// stable because identical embedding input yields an identical embedding.
    /// Returns how many marked keys the plan's gate skipped UNREAD: their marks survive this
    /// publish, and the caller's outcome must count them as unread files — a plan that is
    /// empty only because its gate trusted a stale row proves nothing about the tree.
    pub(crate) fn stage_plan(
        &self,
        plan: RefreshPlan,
        new_embeddings: HashMap<String, Vec<f32>>,
        baseline: &PublicationBaseline,
    ) -> Result<OverlayPublicationStaging, SearchError> {
        let mut next_cache = self.clone();
        let (outcome, fingerprints) =
            next_cache.apply_plan_in_memory(plan, new_embeddings, baseline)?;
        Ok(OverlayPublicationStaging { next_cache, outcome, fingerprints })
    }

    /// Compatibility path for callers without a workspace lease. Production stages first and
    /// supplies the lease checkpoint to the Store transaction through `SearchEngine`.
    pub fn publish_plan(
        &mut self,
        plan: RefreshPlan,
        new_embeddings: HashMap<String, Vec<f32>>,
        baseline: &PublicationBaseline,
        embedder: Option<&Embedder>,
        store: &Store,
    ) -> Result<PublishOutcome, SearchError> {
        let staging = self.stage_plan(plan, new_embeddings, baseline)?;
        let embedding = embedder.map(|embedder| {
            (embedder.model_id(), embedder.dim(), staging.next_cache.embedding_cache_snapshot())
        });
        let mut checkpoint = || std::ops::ControlFlow::Continue(());
        if store
            .apply_overlay_publication_with_token_layout(
                staging.fingerprints.as_ref().map(|(id, rows)| (id.as_str(), rows)),
                embedding.as_ref().map(|(model, dim, rows)| (*model, *dim, rows)),
                embedder.and_then(Embedder::token_layout_claim),
                &mut checkpoint,
            )?
            .is_break()
        {
            unreachable!("permit-all checkpoint cannot cancel");
        }
        *self = staging.next_cache;
        Ok(staging.outcome)
    }

    fn apply_plan_in_memory(
        &mut self,
        plan: RefreshPlan,
        new_embeddings: HashMap<String, Vec<f32>>,
        baseline: &PublicationBaseline,
    ) -> Result<(PublishOutcome, Option<OverlayFingerprintPublication>), SearchError> {
        let scan_is_clean = plan.scan_is_clean();
        for (embedding_key, embedding) in new_embeddings {
            self.embedding_cache.insert(embedding_key, embedding);
        }
        // A wholesale invalidation between Phase A and Phase C replaced the entire state the
        // plan was built against: nothing of the plan may publish. The embeddings above are
        // value-stable (identical input, identical vector), so merging them is the one thing
        // an outdated plan is still good for.
        if baseline.fence < self.wholesale_seq {
            return Ok((PublishOutcome::Superseded, None));
        }
        // Keys with a POINT settlement fresher than the plan's fence: the plan may not touch
        // them on any carrier, whatever it read — the point outcome saw a later disk.
        let fenced: HashSet<FileKey> = self
            .settled_seq
            .iter()
            .filter(|(_, seq)| **seq > baseline.fence)
            .map(|(key, _)| key.clone())
            .collect();
        // Gate-skipped keys with a LIVE mark: the fence cannot protect them (their settlement
        // may predate it, its row retraction refused), but the live mark is positive evidence
        // the row the gate trusted was stale — their current state survives too (И-а4 class:
        // a clean replace would resurrect the row's story over the point result).
        let deferred: HashSet<FileKey> = plan
            .gate_skipped
            .iter()
            .filter(|key| self.dirty_paths.contains_key(*key))
            .cloned()
            .collect();

        let mut entries = HashMap::with_capacity(plan.entries.len());
        for (key, planned) in plan.entries {
            // No embedder is passed, so vectors come purely from the merged cache and the batch
            // size is unused; `1` keeps the chunking math well-defined.
            let vector_documents = build_overlay_vectors(
                None,
                1,
                &planned.lexical_documents,
                &planned.embedding_inputs,
                &mut self.embedding_cache,
            )?;
            entries.insert(
                key,
                OverlayFileEntry {
                    fingerprint: planned.fingerprint,
                    file_hash: planned.file_hash,
                    lexical_documents: planned.lexical_documents,
                    vector_documents,
                    embedding_inputs: planned.embedding_inputs,
                },
            );
        }

        // The keys whose CURRENT state must survive this publication: unread (the plan proved
        // nothing), fenced (a fresher point settlement), and gate-deferred (a live mark
        // distrusts the row the gate believed).
        let carried: HashSet<FileKey> =
            plan.read_failures.iter().chain(&fenced).chain(&deferred).cloned().collect();
        if scan_is_clean {
            // The whole-replace must still carry over what the plan may not overwrite: the
            // prior entry (or its absence) keeps serving and the prior hiding stands — the
            // plan's version of these keys is either unproven or stale.
            for key in &carried {
                entries.remove(key);
                if let Some(prior) = self.remove_entry(key) {
                    entries.insert(key.clone(), prior);
                }
            }
            let mut hidden_paths = plan.hidden_paths;
            for key in &carried {
                hidden_paths.remove(key);
                if self.hidden_paths.contains(key) {
                    hidden_paths.insert(key.clone());
                }
            }
            self.unembedded_entries = entries
                .values()
                .filter(|entry| entry.vector_documents.len() < entry.embedding_inputs.len())
                .count();
            self.entries = entries;
            self.hidden_paths = hidden_paths;
        } else {
            // The plan may not speak for what its scan did not see, so it publishes as a merge:
            // planned entries land, a seen key the plan proved baseline-equal (no entry, no read
            // failure) is removed and un-hidden, and everything unseen keeps its prior state.
            let planned_keys: HashSet<FileKey> = entries.keys().cloned().collect();
            for (key, entry) in entries {
                if carried.contains(&key) {
                    continue;
                }
                self.insert_entry(key, entry);
            }
            for key in &plan.seen_keys {
                if carried.contains(key) {
                    continue;
                }
                if !planned_keys.contains(key) {
                    self.remove_entry(key);
                }
                if plan.hidden_paths.contains(key) {
                    self.hidden_paths.insert(key.clone());
                } else {
                    self.hidden_paths.remove(key);
                }
            }
        }
        self.initialized = true;

        // The shared publication tail, in the mandatory order: decide which marks this
        // publish supersedes, filter the rows a live mark distrusts, persist, and only then
        // consume — a consumed mark promises the knowledge reached the disk. A key whose mark
        // was re-bumped since the pre-plan snapshot (a watcher edit mid-embed) is not
        // superseded; a gate-skipped key was never read, so its mark (set before the plan was
        // even built) is evidence the trusted row was stale and survives too.
        let build_faults = HashSet::new();
        let verdict = PublicationVerdict {
            scan_is_clean,
            seen_keys: &plan.seen_keys,
            read_failures: &plan.read_failures,
            build_faults: &build_faults,
            gate_skipped: &plan.gate_skipped,
            fenced: &fenced,
            superseded: Some(&baseline.dirty),
        };
        let to_consume = self.publication_consumption(&verdict);
        // The plan's row snapshot is as old as its phase A: a key whose mark stays live (or
        // whose point settlement out-fenced the plan) has fresher knowledge than its row, so
        // the row is dropped — the unconditional replace-save then removes its old copy from
        // the table as well.
        let rows = self.split_rows_by_live_marks(plan.updated_persisted, &to_consume, &fenced);
        self.finish_publication(&verdict, &to_consume, true);
        self.baseline_identity = plan.baseline_identity;
        // Settlements whose key still CARRIES state (an entry, a hiding, a mark, an unread
        // debt) are kept even below the fence: another plan with an older fence may still be
        // in flight (the library does not enforce the driver's single-flight), and pruning
        // them would hand that plan the very keys the fence protects. A CARRIER-LESS
        // settlement at or below the fence is pruned — without this, a workspace rotating
        // unique paths would grow the map with its history, not its size. Residual, named in
        // full: the traceless PROOFS — a deletion with no baseline copy AND a BaselineEqual
        // that lifted the entry — leave no carrier to keep them alive, so an OLDER plan
        // overlapping the publication that pruned them could resurrect the superseded
        // content. Both legs require two planned publications in flight at once, which the
        // driver's single-flight rules out; keeping them selectively would need a marker
        // indistinguishable from unbounded growth.
        let fence = baseline.fence;
        self.settled_seq.retain(|key, seq| {
            *seq > fence
                || self.entries.contains_key(key)
                || self.hidden_paths.contains(key)
                || self.dirty_paths.contains_key(key)
                || self.unread_keys.contains(key)
        });
        // Marked keys the gate skipped unread: the tail deliberately kept their marks, and
        // the caller reports them as unread files — counting only the keys the applied
        // unread set does not already cover, or one unverified file would show up twice.
        let gate_deferred = deferred.iter().filter(|key| !self.unread_keys.contains(*key)).count();

        Ok((
            PublishOutcome::Applied {
                gate_deferred,
                persist_ok: true,
                overlay_files: self.entries.len(),
                deleted_files: self
                    .hidden_paths
                    .iter()
                    .filter(|key| !self.entries.contains_key(*key))
                    .count(),
                unread_keys: self.unread_keys.len(),
            },
            plan.manifest_backed.then_some((plan.snapshot_id, rows)),
        ))
    }

    /// A read-only clone of the embedding cache for the warmup's lock-free Phase B start.
    pub fn embedding_cache_snapshot(&self) -> HashMap<String, Vec<f32>> {
        self.embedding_cache.clone()
    }

    /// The dirty-path set (path -> mark sequence) as of this call. Captured before a lock-free embed
    /// pass so [`publish_plan`] can clear exactly the flags that pass supersedes, leaving any
    /// re-marked mid-pass intact (their sequence will have advanced).
    pub fn dirty_paths_snapshot(&self) -> HashMap<FileKey, u64> {
        self.dirty_paths.clone()
    }

    /// How many keys are proven present but unread (see the `unread_keys` field): a durable
    /// retry signal for the driver that outlives the bounded point budget.
    pub fn unread_keys_count(&self) -> usize {
        self.unread_keys.len()
    }

    /// The keys this cache still knows about, as the two kinds of POSITIVE knowledge it
    /// holds: entries it serves, and files it owes a re-read. Hidden paths are not among
    /// them — hiding proves a file is absent from disk, not that its carriers were cleared,
    /// and the same state is reached by a clean full pass that never touched a store row.
    pub(crate) fn known_keys(&self) -> (HashSet<FileKey>, HashSet<FileKey>) {
        (self.entries.keys().cloned().collect(), self.unread_keys.clone())
    }

    /// The keys whose baseline copy is currently hidden from results.
    ///
    /// Hiding proves a file is ABSENT from disk, not that its carriers were cleared: a clean
    /// full pass hides a baseline key it did not see without touching that key's store row.
    /// The one thing it does settle is a key the manifest alone carries, whose removal cannot
    /// delete a row of someone else's corpus and is expressed by this hiding instead.
    pub(crate) fn hidden_keys(&self) -> HashSet<FileKey> {
        self.hidden_paths.clone()
    }

    /// How many overlay entries are missing at least one vector. A PARTIALLY vectorized entry
    /// counts too: `build_overlay_vectors` legitimately returns vectors only for the chunks
    /// with a warm cache hit, so emptiness alone would hide a half-embedded file from the
    /// retry driver.
    pub fn unembedded_entry_count(&self) -> usize {
        self.unembedded_entries
    }

    /// Whether the overlay has been initialized by some full pass (or an explicit clean
    /// init). Read-only: the retry driver's "first pass happened at all" signal.
    pub fn is_initialized(&self) -> bool {
        self.initialized
    }

    fn refresh_dirty_paths_from_manifest(
        &mut self,
        dirty_keys: Vec<FileKey>,
        baseline: ManifestBaseline<'_>,
        roots: &WorkspaceRoots,
        embedder: Option<&Embedder>,
        batch_size: usize,
        snapshots: &HashMap<FileKey, ModuleSnapshot>,
    ) -> Result<(), SearchError> {
        let mut embedding_failure = None;
        let ManifestBaseline { fingerprints: manifest_fingerprints, store } = baseline;
        // Each drained key CLASSIFIES into a [`PointSettlement`] and settles at once through
        // [`Self::settle_point`]. The key's fingerprint-row obligation (the row claims
        // "verified"; the mark dies with the process, the row would survive the restart) is
        // executed with the settlement, and its failure becomes the settlement's store flag —
        // the mark then survives without the file budget being charged, whatever the file
        // outcome was. No per-key fault aborts the loop: the caller has already drained the
        // dirty set, and an abort would strand every unprocessed mark.
        for key in dirty_keys {
            // Removing the count here clears it on success (the common path) and hands the
            // prior value to the settlement, keeping the failure streak consecutive.
            let prior_failures = self.dirty_failures.remove(&key).unwrap_or(0);
            let baseline =
                PointBaseline::Manifest { fingerprint: manifest_fingerprints.get(&key).cloned() };
            let provider = self.graph_context_provider.clone();
            let read = classify_point(
                &key,
                &baseline,
                roots,
                snapshots,
                provider.as_deref(),
                embedder,
                batch_size,
                &mut self.embedding_cache,
            );
            embedding_failure = embedding_failure.or(read.embedding_failure);
            if read.resident_fed {
                self.resident_fed_count += 1;
            }
            // Whatever the classification decided, the row's "verified" claim did not
            // survive: a failed stat or read proved nothing, a proven absence ended the
            // file, and a successful read is FRESHER knowledge than the row — at an unchanged
            // (len, mtime, canonical) the old row would suppress this very result after a
            // restart. A failed retraction keeps the mark for a retried one.
            let store_fault = !Self::retract_fingerprint_row(store, &key);
            self.settle_point(
                key,
                PointSettlement { action: read.action, store_fault },
                prior_failures,
            );
        }

        embedding_failure.map_or(Ok(()), |failure| Err(failure.into()))
    }

    /// How many overlay entries have been built from a resident-provided shared parse (rather than
    /// a self-parsed disk read) since the last [`Self::clear`]. Observability for the resident-fed
    /// incremental reindex — a nonzero value proves the shared-parse path actually fired.
    pub fn resident_fed_count(&self) -> usize {
        self.resident_fed_count
    }

    /// The consecutive-failure count recorded for a dirty path, or `0` when none is tracked. Lets a
    /// test assert that a store-wide error left a path's retry budget untouched.
    #[cfg(test)]
    fn dirty_failure_count(&self, key: &FileKey) -> u32 {
        self.dirty_failures.get(key).copied().unwrap_or(0)
    }

    pub fn snapshot(&self) -> WorkspaceOverlayIndex {
        let baseline =
            BaselineRef::for_snapshot(CorpusId::WorkspaceCode, "local-workspace-baseline");
        let mut overlay = SearchOverlay::new(baseline);
        let mut lexical_documents = Vec::new();
        let mut vector_documents = Vec::new();

        let mut entry_keys: Vec<&FileKey> = self.entries.keys().collect();
        entry_keys.sort();
        for key in entry_keys {
            let entry = self.entries.get(key).expect("key collected from map keys");
            overlay.replace_file(
                DocumentPath::new("code", &key.root_id, &key.path),
                entry.lexical_documents.clone(),
            );
            lexical_documents.extend(entry.lexical_documents.clone());
            vector_documents.extend(entry.vector_documents.clone());
        }

        let mut deleted_keys: Vec<&FileKey> =
            self.hidden_paths.iter().filter(|key| !self.entries.contains_key(*key)).collect();
        deleted_keys.sort();
        for key in deleted_keys {
            overlay.delete_file(DocumentPath::new("code", &key.root_id, &key.path));
        }

        WorkspaceOverlayIndex {
            overlay,
            hidden_paths: self.hidden_paths.clone(),
            lexical_documents,
            vector_documents,
        }
    }

    pub fn stats(&self) -> WorkspaceOverlayStats {
        let overlay_files = self.entries.len();
        let hidden_paths = self.hidden_paths.len();
        let deleted_files =
            self.hidden_paths.iter().filter(|key| !self.entries.contains_key(*key)).count();
        let lexical_chunks =
            self.entries.values().map(|entry| entry.lexical_documents.len()).sum::<usize>();
        let semantic_chunks =
            self.entries.values().map(|entry| entry.vector_documents.len()).sum::<usize>();

        WorkspaceOverlayStats {
            overlay_files,
            deleted_files,
            hidden_paths,
            lexical_chunks,
            semantic_chunks,
            cached_embeddings: self.embedding_cache.len(),
            watcher_mode: self.watcher_mode,
            pending_dirty_paths: self.dirty_paths.len(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileFingerprint {
    len: u64,
    modified: Option<SystemTime>,
    /// The physical spelling of what was actually read. Without it a link retargeted onto a
    /// file with the same `(len, mtime)` keeps passing for unchanged, and the old target is
    /// served forever; only the identical target may stay invisible.
    canonical: PathBuf,
}

/// Whether a persisted row's physical spelling matches the file just stat'ed. An empty stored
/// spelling is a row from before the column existed: never a match, so the file is re-read and
/// the re-save fills the spelling in — old rows heal themselves. Both sides compare through the
/// same lossy conversion, so a non-UTF-8 path stays equal to itself.
fn fingerprint_canonical_matches(
    fingerprint: &FileFingerprint,
    cached: &crate::store::PersistedFingerprint,
) -> bool {
    if cached.canonical.is_empty() || cached.canonical.contains('\u{FFFD}') {
        // Empty: a row from before the column existed. Replacement characters: the lossy
        // conversion collapses every invalid byte onto one char, so two DIFFERENT non-UTF-8
        // targets could pass for each other — such a row is never trusted, only re-read.
        return false;
    }
    *cached.canonical == *fingerprint.canonical.to_string_lossy()
}

fn fingerprint_mtime_matches(
    mtime: Option<SystemTime>,
    cached: &crate::store::PersistedFingerprint,
) -> bool {
    let Some((secs, nanos)) = mtime_to_secs_nanos(mtime) else {
        return false;
    };
    secs == cached.file_mtime_secs && nanos == cached.file_mtime_nanos
}

fn mtime_to_secs_nanos(mtime: Option<SystemTime>) -> Option<(i64, u32)> {
    let duration = mtime?.duration_since(SystemTime::UNIX_EPOCH).ok()?;
    Some((duration.as_secs() as i64, duration.subsec_nanos()))
}

#[derive(Debug, Clone)]
pub(crate) struct OverlayFileEntry {
    fingerprint: FileFingerprint,
    file_hash: Vec<u8>,
    lexical_documents: Vec<IndexedDocument>,
    vector_documents: Vec<OverlayVectorDocument>,
    embedding_inputs: Vec<String>,
}

#[derive(Debug, Clone)]
struct WorkspaceFileState {
    key: FileKey,
    abs_path: PathBuf,
    fingerprint: FileFingerprint,
}

/// Everything one full scan of the registered roots saw: the files under their
/// store keys, plus how much of the tree the walk could NOT vouch for. The
/// counters travel with the files because they qualify them — a consumer that
/// reconciles a store against `files` must know whether the list may speak for
/// the whole tree.
#[derive(Debug, Default)]
struct ScannedFiles {
    files: Vec<WorkspaceFileState>,
    /// See [`project_model::SourceSet::unreadable`]: coverage is incomplete.
    unreadable: usize,
    /// See [`project_model::SourceSet::loops`]: benign, coverage complete.
    loops: usize,
    /// See [`project_model::SourceSet::dangling`]: benign, coverage complete.
    dangling: usize,
    /// See [`project_model::SourceSet::canonical_fallbacks`]: identity is degraded.
    canonical_fallbacks: usize,
}

impl ScannedFiles {
    /// Whether `files` may speak for the whole tree — the same verdict as
    /// [`project_model::SourceSet::clean`]: loops and dangling links leave
    /// coverage complete, so they deliberately do not count.
    fn clean(&self) -> bool {
        self.unreadable == 0 && self.canonical_fallbacks == 0
    }
}

/// Every source file of every registered root, each under the key it is stored
/// by, from the one shared workspace walk.
fn scan_workspace_files(roots: &WorkspaceRoots) -> ScannedFiles {
    let declared: Vec<PathBuf> = roots.entries().map(|(_, root)| root.to_path_buf()).collect();
    let set =
        project_model::SourceSet::scan_in_scope(&declared, roots.excluded(), roots.user_excluded());
    let scanned = scanned_files_from(roots, &set);
    if !scanned.clean() {
        tracing::warn!(
            unreadable = scanned.unreadable,
            canonical_fallbacks = scanned.canonical_fallbacks,
            "workspace overlay scan did not cover the whole tree"
        );
    }
    tracing::debug!(
        files = scanned.files.len(),
        unreadable = scanned.unreadable,
        loops = scanned.loops,
        dangling = scanned.dangling,
        canonical_fallbacks = scanned.canonical_fallbacks,
        "workspace overlay scan"
    );
    scanned
}

/// Whether a failed `metadata` call PROVES the point-refresh target is gone. `NotFound` and
/// `NotADirectory` (a parent directory replaced by a file) are the kernel saying so outright; a
/// symlink cycle anywhere in the path is proven by walking the chain — the shared walk
/// classifies such a file as a benign loop and never yields it, so the point answer must agree
/// with the full scan. Anything else — permissions, I/O, or a merely LONG link chain behind
/// which a live file sits (a bare `ELOOP` proves nothing) — reads as a live file the caller
/// keeps and retries.
fn point_target_is_absent(error: &std::io::Error, path: &Path) -> bool {
    matches!(error.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory)
        || project_model::path_crosses_a_link_cycle(path)
}

/// Whether the registered root a key belongs to exists at all. The full walk classifies a
/// failed ROOT as incomplete coverage, never as deletions of its files — so a point refresh
/// that got `NotFound` under an unreachable root must not read it as the file's removal either.
fn root_is_reachable(roots: &WorkspaceRoots, key: &FileKey) -> bool {
    roots
        .entries()
        .find(|(id, _)| *id == key.root_id)
        // Follow the root if it is a link: the inode of the link itself proves nothing about
        // the tree behind it, and a dangling or cycled root link is the walk's "unreadable
        // root", not the file's deletion.
        .is_some_and(|(_, root)| std::fs::metadata(root).is_ok())
}

/// Record every scanned file's two spellings, best effort.
///
/// The journal is what a removal has and re-attribution does not; a write that fails costs
/// the fallback behaviour for those spellings, not the refresh (github#192).
fn record_scanned_spellings(store: &Store, scanned: &ScannedFiles) {
    let mut records: Vec<(String, FileKey)> = Vec::new();
    let mut seen = HashSet::new();
    for file in &scanned.files {
        for spelling in [&file.abs_path, &file.fingerprint.canonical] {
            let rendered = spelling.to_string_lossy().into_owned();
            if !rendered.is_empty() && seen.insert(rendered.clone()) {
                records.push((rendered, file.key.clone()));
            }
        }
    }
    if let Err(error) = store.record_path_spellings("code", &records) {
        tracing::warn!("failed to record a scan's path spellings: {error}");
    }
}

/// The pure projection of a walk result into overlay terms. Roots may nest, so
/// a file reached from two of them is enumerated twice and attributed to the
/// same owner both times; de-duplicating by key is what keeps one file one
/// entry. A file whose key cannot be built (a root that is itself a file) is
/// dropped, as it always was.
fn scanned_files_from(roots: &WorkspaceRoots, set: &project_model::SourceSet) -> ScannedFiles {
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
        files.push(WorkspaceFileState {
            key,
            abs_path: file.walked.clone(),
            fingerprint: FileFingerprint {
                len: file.metadata.len(),
                modified: file.metadata.modified().ok(),
                canonical: file.canonical.clone(),
            },
        });
    }
    ScannedFiles {
        files,
        unreadable: set.unreadable,
        loops: set.loops,
        dangling: set.dangling,
        canonical_fallbacks: set.canonical_fallbacks,
    }
}

/// A dirty key's baseline, as a point classification reads it: the stored raw hash with the
/// recipe that recomputes it, or the manifest's fingerprint.
pub(crate) enum PointBaseline {
    Raw { hash: Option<Vec<u8>>, mode: BaselineHashMode },
    Manifest { fingerprint: Option<String> },
}

impl PointBaseline {
    fn is_present(&self) -> bool {
        match self {
            Self::Raw { hash, .. } => hash.is_some(),
            Self::Manifest { fingerprint } => fingerprint.is_some(),
        }
    }
}

/// What classifying one dirty key read and decided.
pub(crate) struct PointRead {
    pub(crate) embedding_failure: Option<crate::EmbeddingFailure>,
    pub(crate) action: PointAction,
    /// The entry was chunked from the resident's shared parse instead of a parse of its own.
    pub(crate) resident_fed: bool,
    /// Bytes read from disk for the decision.
    pub(crate) bytes: u64,
}

/// Classify one dirty key against its baseline: the ONE decision every point path makes,
/// under a lock or off every lock alike. Reads the file and builds its entry; touches no
/// carrier and no store — the caller settles the result and owns the row obligations.
///
/// No equal-fingerprint fast path: every key classified here carries a dirty mark, and the
/// mark is positive evidence the fingerprint must not be trusted — an edit at unchanged
/// (len, mtime, canonical) would otherwise be consumed silently. The price is one read per
/// honest no-op watcher ping.
#[allow(clippy::too_many_arguments)]
pub(crate) fn classify_point(
    key: &FileKey,
    baseline: &PointBaseline,
    roots: &WorkspaceRoots,
    snapshots: &HashMap<FileKey, ModuleSnapshot>,
    provider: Option<&dyn GraphContextProvider>,
    embedder: Option<&Embedder>,
    batch_size: usize,
    embedding_cache: &mut HashMap<String, Vec<f32>>,
) -> PointRead {
    crate::point_refresh::forbidden_under_a_bounded_publication("reading and parsing a file");
    let has_baseline = baseline.is_present();
    let settled =
        |action| PointRead { action, resident_fed: false, bytes: 0, embedding_failure: None };
    // A key whose root is no longer registered resolves to nothing; that is a change of
    // composition, not a filesystem error, and it settles like a deletion.
    let Some(abs_path) = roots.resolve(key) else {
        return settled(PointAction::ProvenGone { has_baseline });
    };
    let metadata = match std::fs::metadata(&abs_path) {
        Ok(metadata) => metadata,
        Err(error) => {
            return settled(
                if point_target_is_absent(&error, &abs_path) && root_is_reachable(roots, key) {
                    PointAction::ProvenGone { has_baseline }
                } else {
                    PointAction::FileFault { reason: "stat failed" }
                },
            );
        }
    };
    let fingerprint = FileFingerprint {
        len: metadata.len(),
        modified: metadata.modified().ok(),
        canonical: crate::workspace_roots::canonical_spelling(&abs_path),
    };
    // A live target that is not a source file is positive evidence the SOURCE file is gone:
    // the walk never yields a file whose two spellings disagree on role, so a clean full scan
    // would remove this entry — the point path settles it the same way. The same goes for a
    // non-regular target spelled `.bsl` (a directory, a FIFO): the walk yields regular files
    // only, and reading a FIFO would even block.
    if !metadata.is_file()
        || project_model::file_role(&fingerprint.canonical) != project_model::FileRole::Source
    {
        return settled(PointAction::ProvenGone { has_baseline });
    }
    let Ok(content) = std::fs::read_to_string(&abs_path) else {
        return settled(PointAction::FileFault { reason: "read failed" });
    };
    let bytes = content.len() as u64;
    let (file_hash, equal) = match baseline {
        PointBaseline::Raw { hash, mode } => {
            let file_hash = compute_file_hash(&content, *mode);
            let equal = hash.as_ref().is_some_and(|stored| stored == &file_hash);
            (file_hash, equal)
        }
        PointBaseline::Manifest { fingerprint } => {
            let local = fingerprint_content(&content, &key.path);
            let equal = fingerprint.as_ref().is_some_and(|stored| stored == &local);
            (normalized_file_hash_for_content(&content), equal)
        }
    };
    if equal {
        return PointRead {
            action: PointAction::BaselineEqual,
            resident_fed: false,
            bytes,
            embedding_failure: None,
        };
    }
    let parse_root = resident_parse_root(snapshots, key, &content);
    let resident_fed = parse_root.is_some();
    let mut embedding_failure = None;
    let action = match build_overlay_entry(
        key,
        &content,
        fingerprint,
        file_hash,
        embedder,
        batch_size,
        embedding_cache,
        provider,
        parse_root,
    ) {
        Ok(entry) => PointAction::Reindexed { entry, has_baseline },
        Err(error) => {
            embedding_failure = error.embedding_failure();
            tracing::warn!(
                root = %key.root_id,
                path = %key.path,
                "failed to build an overlay entry; keeping the mark for a retry: {error}"
            );
            PointAction::BuildFault
        }
    };
    PointRead { action, resident_fed, bytes, embedding_failure }
}

#[allow(clippy::too_many_arguments)]
fn build_overlay_entry(
    key: &FileKey,
    content: &str,
    fingerprint: FileFingerprint,
    file_hash: Vec<u8>,
    embedder: Option<&Embedder>,
    batch_size: usize,
    embedding_cache: &mut HashMap<String, Vec<f32>>,
    graph_context: Option<&dyn GraphContextProvider>,
    parse_root: Option<&syntax::SyntaxNode>,
) -> Result<OverlayFileEntry, SearchError> {
    let (lexical_documents, embedding_inputs) =
        build_overlay_documents_prepared(key, content, graph_context, parse_root, embedder)?;
    let vector_documents = build_overlay_vectors(
        embedder,
        batch_size,
        &lexical_documents,
        &embedding_inputs,
        embedding_cache,
    )?;

    Ok(OverlayFileEntry {
        fingerprint,
        file_hash,
        lexical_documents,
        vector_documents,
        embedding_inputs,
    })
}

fn compute_file_hash(content: &str, hash_mode: BaselineHashMode) -> Vec<u8> {
    match hash_mode {
        BaselineHashMode::RawFileBytes => blake3::hash(content.as_bytes()).as_bytes().to_vec(),
        BaselineHashMode::NormalizedChunks => normalized_file_hash_for_content(content),
    }
}

pub(crate) fn normalized_file_hash_for_content(content: &str) -> Vec<u8> {
    let chunks = Chunker::chunk(content);
    normalized_file_hash_for_chunks(chunks.iter().map(|chunk| {
        (
            chunk.kind.label(),
            chunk.name.as_str(),
            chunk.line_start,
            chunk.line_end,
            chunk.text.as_str(),
        )
    }))
}

pub(crate) fn normalized_file_hash_for_indexed_documents(documents: &[IndexedDocument]) -> Vec<u8> {
    normalized_file_hash_for_chunks(documents.iter().map(|document| {
        (
            document.kind.as_str(),
            document.symbol_name.as_str(),
            document.line_start,
            document.line_end,
            document.text.as_str(),
        )
    }))
}

fn normalized_file_hash_for_chunks<'a>(
    chunks: impl Iterator<Item = (&'a str, &'a str, u32, u32, &'a str)>,
) -> Vec<u8> {
    let mut hasher = blake3::Hasher::new();
    for (kind, name, line_start, line_end, text) in chunks {
        hasher.update(kind.as_bytes());
        hasher.update(&[0]);
        hasher.update(name.as_bytes());
        hasher.update(&[0]);
        hasher.update(&line_start.to_le_bytes());
        hasher.update(&line_end.to_le_bytes());
        hasher.update(text.as_bytes());
        hasher.update(&[0xff]);
    }
    hasher.finalize().as_bytes().to_vec()
}

/// Both recipes below reproduce the publisher's per-file fingerprint byte for
/// byte, because the only thing they are ever compared against is the value a
/// published manifest carries. A recipe of their own would mean no file in the
/// working tree ever matches the baseline, and the whole corpus would live as
/// an overlay delta.
///
/// Where the publisher folds in each document's graph context, these write its
/// "absent" marker unconditionally. That is not an omission: the published
/// corpus is indexed with no graph context provider at all, so a local document
/// enriched with context describes the same text as the context-free one the
/// snapshot holds. Hashing the enrichment here could only ever report a file
/// whose text matches the snapshot as locally changed.
pub(crate) fn fingerprint_content(content: &str, rel_path: &str) -> String {
    let chunks = Chunker::chunk(content);
    let mut documents: Vec<(u32, u32, &str, &str, String, &str)> = chunks
        .iter()
        .map(|chunk| {
            let kind = match chunk.kind {
                code_chunk::ChunkKind::ModuleHeader => "header",
                code_chunk::ChunkKind::Procedure => "procedure",
                code_chunk::ChunkKind::Function => "function",
            };
            let content_hash = blake3::hash(chunk.text.as_bytes()).to_hex().to_string();
            (
                chunk.line_start,
                chunk.line_end,
                chunk.name.as_str(),
                kind,
                content_hash,
                chunk.text.as_str(),
            )
        })
        .collect();
    sort_like_the_publisher(&mut documents, |lhs, rhs| {
        (lhs.0, lhs.1, lhs.2, lhs.3, lhs.4.as_str()).cmp(&(
            rhs.0,
            rhs.1,
            rhs.2,
            rhs.3,
            rhs.4.as_str(),
        ))
    });

    let mut hasher = blake3::Hasher::new();
    for (line_start, line_end, name, kind, content_hash, text) in &documents {
        hasher.update("code".as_bytes());
        hasher.update(&[0]);
        hasher.update(rel_path.as_bytes());
        hasher.update(&[0]);
        hasher.update(name.as_bytes());
        hasher.update(&[0]);
        hasher.update(kind.as_bytes());
        hasher.update(&line_start.to_le_bytes());
        hasher.update(&line_end.to_le_bytes());
        hasher.update(content_hash.as_bytes());
        hasher.update(&[0]);
        hasher.update(text.as_bytes());
        hasher.update(&[0]);
        hasher.update(&[0]);
        hasher.update(&[0xff]);
    }
    hasher.finalize().to_hex().to_string()
}

/// The publisher hashes a file's chunks in sorted order, while a chunker returns
/// them in source order. For ordinary code the two coincide — line numbers rise
/// with position — and they part ways as soon as several chunks share a line
/// span, as two one-line methods written on one physical line do. Ordering
/// locally by the publisher's key is what makes the recipes agree for those
/// files too.
fn sort_like_the_publisher<T>(documents: &mut [T], compare: impl Fn(&T, &T) -> std::cmp::Ordering) {
    documents.sort_by(compare);
}

pub(crate) fn fingerprint_overlay_documents(
    documents: &[IndexedDocument],
    rel_path: &str,
) -> String {
    let mut ordered: Vec<&IndexedDocument> = documents.iter().collect();
    sort_like_the_publisher(&mut ordered, |lhs, rhs| {
        (
            lhs.line_start,
            lhs.line_end,
            lhs.symbol_name.as_str(),
            lhs.kind.as_str(),
            lhs.content_hash.as_str(),
        )
            .cmp(&(
                rhs.line_start,
                rhs.line_end,
                rhs.symbol_name.as_str(),
                rhs.kind.as_str(),
                rhs.content_hash.as_str(),
            ))
    });

    let mut hasher = blake3::Hasher::new();
    for document in ordered {
        hasher.update(document.collection.as_bytes());
        hasher.update(&[0]);
        hasher.update(rel_path.as_bytes());
        hasher.update(&[0]);
        hasher.update(document.symbol_name.as_bytes());
        hasher.update(&[0]);
        hasher.update(document.kind.as_bytes());
        hasher.update(&document.line_start.to_le_bytes());
        hasher.update(&document.line_end.to_le_bytes());
        hasher.update(document.content_hash.as_bytes());
        hasher.update(&[0]);
        hasher.update(document.text.as_bytes());
        hasher.update(&[0]);
        hasher.update(&[0]);
        hasher.update(&[0xff]);
    }
    hasher.finalize().to_hex().to_string()
}

/// The overlay embedding cache key: the blake3 hash of the exact text that gets embedded. This is
/// the same value as [`crate::document::semantic_key_for_indexed_document`] computed from the
/// document's embedding input, so the overlay reuses vectors by the same identity the baseline and
/// the main chunk index use, rather than by raw chunk text.
fn overlay_embedding_key(embedding_input: &str) -> String {
    blake3::hash(embedding_input.as_bytes()).to_hex().to_string()
}

/// The shared syntax tree to chunk `content` with, when the resident snapshot for `rel_path`
/// holds byte-identical text. A mismatch (the file changed on disk after the resident parsed
/// it) falls back to `None` so the caller parses `content` itself, keeping chunk output and
/// the stored hash pinned to the exact bytes on disk.
fn resident_parse_root<'a>(
    snapshots: &'a HashMap<FileKey, ModuleSnapshot>,
    key: &FileKey,
    content: &str,
) -> Option<&'a syntax::SyntaxNode> {
    snapshots
        .get(key)
        .filter(|snapshot| snapshot.text.as_ref() == content)
        .map(|snapshot| &snapshot.root)
}

/// An error when the provider failed to render some chunk's graph context: an overlay entry
/// built without it would be kept as the answer, so the entry is not built and its file keeps
/// the mark that asked for it.
fn build_overlay_documents(
    key: &FileKey,
    content: &str,
    graph_context: Option<&dyn GraphContextProvider>,
    parse_root: Option<&syntax::SyntaxNode>,
) -> Result<(Vec<IndexedDocument>, Vec<String>), SearchError> {
    // When the resident host already parsed this exact text, chunk its shared syntax tree
    // instead of parsing `content` again (`chunk_parsed` is byte-parity-tested against
    // `chunk`). `content` still drives every text/offset/hash decision, so the chunk output
    // and the stored hash are identical to the pure-disk path.
    let chunks = match parse_root {
        Some(root) => Chunker::chunk_parsed(root, content),
        None => Chunker::chunk(content),
    };
    let mut lexical_documents = Vec::with_capacity(chunks.len());
    let mut embedding_inputs = Vec::with_capacity(chunks.len());

    for chunk in &chunks {
        let (document, context_failed) =
            crate::document::indexed_document_for_chunk(key, chunk, content, graph_context);
        if context_failed {
            return Err(SearchError::Index(format!("graph context unavailable for {}", key.path)));
        }
        embedding_inputs.push(crate::document::semantic_text_for_indexed_document(&document));
        lexical_documents.push(document);
    }

    Ok((lexical_documents, embedding_inputs))
}

fn build_overlay_documents_prepared(
    key: &FileKey,
    content: &str,
    graph_context: Option<&dyn GraphContextProvider>,
    parse_root: Option<&syntax::SyntaxNode>,
    embedder: Option<&Embedder>,
) -> Result<(Vec<IndexedDocument>, Vec<String>), SearchError> {
    if embedder.is_some_and(|embedder| embedder.token_policy().is_some()) {
        let (documents, context_failed) =
            crate::document::prepare_indexed_documents(key, content, graph_context, embedder)?;
        if context_failed {
            return Err(SearchError::Index(format!("graph context unavailable for {}", key.path)));
        }
        let inputs =
            documents.iter().map(crate::document::semantic_text_for_indexed_document).collect();
        Ok((documents, inputs))
    } else {
        build_overlay_documents(key, content, graph_context, parse_root)
    }
}

/// Build overlay vectors for a file's chunks.
///
/// With no `embedder`, only cached vectors are attached and chunks without one remain lexical.
/// With an embedder, missing vectors are embedded inline. Newly embedded vectors are written
/// back to `embedding_cache`.
fn build_overlay_vectors(
    embedder: Option<&Embedder>,
    batch_size: usize,
    documents: &[IndexedDocument],
    embedding_inputs: &[String],
    embedding_cache: &mut HashMap<String, Vec<f32>>,
) -> Result<Vec<OverlayVectorDocument>, SearchError> {
    if documents.is_empty() {
        return Ok(Vec::new());
    }

    let mut vectors: Vec<Option<Vec<f32>>> = vec![None; documents.len()];
    let mut missing_indexes = Vec::new();
    let mut missing_inputs = Vec::new();

    // Key the embedding cache by the hash of the exact text that is embedded (the semantic
    // embedding input), not by the raw chunk-text `content_hash`. Two chunks with identical bodies
    // but different module / symbol / kind / graph context produce different embedding inputs, so
    // they must map to different vectors; keying by `content_hash` would collapse them onto one
    // (and serve a stale vector when only the graph context changed).
    for (idx, _document) in documents.iter().enumerate() {
        let key = overlay_embedding_key(&embedding_inputs[idx]);
        if let Some(embedding) = embedding_cache.get(&key) {
            vectors[idx] = Some(embedding.clone());
        } else {
            missing_indexes.push(idx);
            missing_inputs.push(embedding_inputs[idx].as_str());
        }
    }

    if let Some(embedder) = embedder {
        for range in embedder.batch_ranges(&missing_inputs, batch_size)? {
            let batch_indexes = &missing_indexes[range.clone()];
            let batch_inputs = &missing_inputs[range];
            // The background warmup runs this off any lock, so it can afford the interactive
            // embed; the hot interactive query path never reaches here (it passes `None`).
            let embeddings = embedder.embed_batch_interactive(batch_inputs)?;
            for (idx, embedding) in batch_indexes.iter().copied().zip(embeddings) {
                let key = overlay_embedding_key(&embedding_inputs[idx]);
                embedding_cache.insert(key, embedding.clone());
                vectors[idx] = Some(embedding);
            }
        }
    }

    // ReuseOnly (or a missing inline embed) leaves a chunk without a vector. Such chunks are not
    // emitted as vector documents so they never appear as zero-similarity hits; they still serve
    // lexically through `lexical_documents`.
    Ok(documents
        .iter()
        .cloned()
        .zip(vectors)
        .filter_map(|(document, embedding)| {
            embedding.map(|embedding| OverlayVectorDocument { document, embedding })
        })
        .collect())
}

pub fn lexical_hits(
    overlay: &WorkspaceOverlayIndex,
    query: &str,
    limit: usize,
) -> Vec<crate::engine::SearchHit> {
    lexical_hits_for_documents(overlay.lexical_documents.iter(), query, limit)
}

pub fn semantic_hits(
    overlay: &WorkspaceOverlayIndex,
    query_embedding: &[f32],
    limit: usize,
) -> Vec<crate::engine::SearchHit> {
    let mut hits: Vec<crate::engine::SearchHit> = overlay
        .vector_documents
        .iter()
        .map(|document| crate::engine::SearchHit {
            collection: document.document.collection.clone(),
            root_id: document.document.root_id.clone(),
            file_path: document.document.path.clone(),
            symbol_name: document.document.symbol_name.clone(),
            kind: document.document.kind.clone(),
            text: document.document.text.clone(),
            line_start: document.document.line_start,
            line_end: document.document.line_end,
            score: cosine_similarity(query_embedding, &document.embedding),
            source_span: document.document.source_span.clone(),
        })
        .collect();

    hits.sort_by(|lhs, rhs| rhs.score.total_cmp(&lhs.score));
    hits.truncate(limit);
    hits
}

fn cosine_similarity(lhs: &[f32], rhs: &[f32]) -> f32 {
    if lhs.len() != rhs.len() || lhs.is_empty() {
        return 0.0;
    }

    let mut dot = 0.0f32;
    let mut lhs_norm = 0.0f32;
    let mut rhs_norm = 0.0f32;

    for (&left, &right) in lhs.iter().zip(rhs.iter()) {
        dot += left * right;
        lhs_norm += left * left;
        rhs_norm += right * right;
    }

    let denom = lhs_norm.sqrt() * rhs_norm.sqrt();
    if denom == 0.0 {
        0.0
    } else {
        dot / denom
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn payload_overlay_builder_maps_cached_gaps_and_retains_partial_work() {
        use crate::embedder::payload_tests::{success, vector, PayloadServer};
        let content = (0..4)
            .map(|i| format!("Процедура Метод{i}()\n    Сообщить(\"{i}\");\nКонецПроцедуры\n\n"))
            .collect::<String>();
        let (docs, inputs) = super::build_overlay_documents(
            &crate::FileKey::configuration("Module.bsl"),
            &content,
            None,
            None,
        )
        .unwrap();
        assert_eq!(docs.len(), 4);
        let limit = inputs
            .iter()
            .map(|t| {
                serde_json::json!({"model":"fixture","input":[t],"dimensions":3}).to_string().len()
            })
            .max()
            .unwrap();
        let server =
            PayloadServer::new(
                |i, body| if i == 1 { (413, "refused".into()) } else { success(i, body) },
            );
        let embedder = crate::Embedder::new(server.config(limit));
        let mut cache = [1, 3]
            .into_iter()
            .map(|i| (super::overlay_embedding_key(&inputs[i]), vector(&inputs[i])))
            .collect();
        let error = super::build_overlay_vectors(Some(&embedder), 32, &docs, &inputs, &mut cache)
            .unwrap_err();
        assert_eq!(error.to_string(), "embedding_request_too_large");
        assert_eq!(cache.len(), 3);
        assert_eq!(server.requests().len(), 2);
        let result =
            super::build_overlay_vectors(Some(&embedder), 32, &docs, &inputs, &mut cache).unwrap();
        assert_eq!(result.len(), 4);
        assert_eq!(server.requests().len(), 3);
        for (i, actual) in result.iter().enumerate() {
            assert_eq!(actual.document.symbol_name, docs[i].symbol_name);
            assert_eq!(actual.embedding, vector(&inputs[i]));
        }
        assert!(server.requests().iter().all(|body| body.len() <= limit));
    }

    #[test]
    fn payload_overlay_refresh_settles_every_key_before_exposing_embedding_failure() {
        use crate::embedder::payload_tests::{success, PayloadServer};
        for manifest in [false, true] {
            for dirty in [false, true] {
                let dir = tempfile::tempdir().unwrap();
                let content = "Процедура Тест()\nКонецПроцедуры";
                for file in ["A.bsl", "B.bsl"] {
                    std::fs::write(dir.path().join(file), content).unwrap();
                }
                let roots = single_root(dir.path());
                let store = Store::open(&dir.path().join("search.db")).unwrap();
                let mut cache = WorkspaceOverlayCache::default();
                cache.enable_watcher_mode();
                let server = PayloadServer::new(|i, body| {
                    if i == 0 {
                        (413, "refused".into())
                    } else {
                        success(i, body)
                    }
                });
                let embedder = crate::Embedder::new(server.config(4096));
                if dirty {
                    cache
                        .refresh(
                            &store,
                            &roots,
                            None,
                            32,
                            super::BaselineHashMode::RawFileBytes,
                            true,
                        )
                        .unwrap();
                    for file in ["A.bsl", "B.bsl"] {
                        cache.mark_dirty_path(key(file));
                    }
                }
                let result = if manifest {
                    cache.refresh_with_manifest(
                        &HashMap::new(),
                        &roots,
                        Some(&embedder),
                        32,
                        &store,
                        true,
                    )
                } else {
                    cache.refresh(
                        &store,
                        &roots,
                        Some(&embedder),
                        32,
                        super::BaselineHashMode::RawFileBytes,
                        true,
                    )
                };
                assert_eq!(result.unwrap_err().to_string(), "embedding_request_too_large");
                assert_eq!(server.requests().len(), 2, "the second drained key must still settle");
                assert_eq!(cache.stats().semantic_chunks, 1);
                assert_eq!(cache.dirty_paths_snapshot().len(), 1);
            }
        }
    }

    use super::{
        build_overlay_documents, fingerprint_content, lexical_hits, BaselineHashMode,
        PublishOutcome, WorkspaceOverlayCache, WorkspaceOverlayStats, MAX_DIRTY_REFRESH_FAILURES,
    };
    use crate::store::Store;
    use crate::workspace_roots::{FileKey, WorkspaceRoots, CONFIGURATION_ROOT_ID};
    use std::collections::{HashMap, HashSet};
    use std::fs;
    use std::path::{Path, PathBuf};
    use tempfile::tempdir;

    /// A workspace whose only source root is the workspace directory itself —
    /// the shape every test here works in unless it says otherwise.
    fn single_root(workspace: &Path) -> WorkspaceRoots {
        WorkspaceRoots::build(workspace, workspace, &[]).0
    }

    /// The store key of a configuration file at `path`.
    fn key(path: &str) -> FileKey {
        FileKey::configuration(path)
    }

    /// Chunk output through the resident-shared parse must be byte-identical to the pure
    /// disk-read+parse path — for a UTF-8 BOM, CRLF line endings, and a method large enough
    /// to cross the 32 KiB chunk-split threshold.
    #[test]
    fn snapshot_chunking_matches_disk_for_edge_cases() {
        let large_body: String = (0..4000).map(|i| format!("    П = П + {i};\n")).collect();
        let large = format!("Процедура Большая() Экспорт\n{large_body}КонецПроцедуры\n");
        let cases = [
            "\u{feff}Процедура СБом()\nКонецПроцедуры\n".to_owned(),
            "Процедура СRLF()\r\nВозврат;\r\nКонецПроцедуры\r\n".to_owned(),
            large,
        ];
        for content in &cases {
            let root = parser::parse(content).syntax_node();
            let (disk_docs, disk_inputs) =
                build_overlay_documents(&key("M.bsl"), content, None, None).unwrap();
            let (snap_docs, snap_inputs) =
                build_overlay_documents(&key("M.bsl"), content, None, Some(&root)).unwrap();

            assert!(!disk_docs.is_empty(), "fixture must produce at least one chunk");
            assert_eq!(disk_docs.len(), snap_docs.len(), "chunk count must match");
            for (disk, snap) in disk_docs.iter().zip(&snap_docs) {
                assert_eq!(disk.symbol_name, snap.symbol_name);
                assert_eq!(disk.kind, snap.kind);
                assert_eq!(disk.line_start, snap.line_start);
                assert_eq!(disk.line_end, snap.line_end);
                assert_eq!(disk.text, snap.text);
                assert_eq!(disk.content_hash, snap.content_hash);
            }
            assert_eq!(disk_inputs, snap_inputs, "embedding inputs must match");
        }
        // The large fixture genuinely crosses the split threshold, so parity is checked with
        // more than one chunk in play.
        assert!(
            build_overlay_documents(&key("M.bsl"), &cases[2], None, None).unwrap().0.len() > 1,
            "the large fixture must exercise the 32 KiB split"
        );
    }

    /// A dirty path whose read transiently fails (here invalid UTF-8 in a regular `.bsl`, so
    /// `metadata` succeeds but `read_to_string` errors) must stay in the dirty set for the next
    /// refresh, rather than being silently dropped. Restoring the pre-fix `continue`-drop in
    /// `refresh_dirty_paths_from_manifest` makes this assertion fail.
    #[test]
    fn dirty_path_survives_a_read_failure() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let store = Store::open(&workspace.join("search.db")).unwrap();

        let mut cache = WorkspaceOverlayCache::default();
        cache.enable_watcher_mode();
        let manifest: HashMap<FileKey, String> = HashMap::new();
        // A full refresh initializes the cache so the next refresh takes the incremental branch.
        cache
            .refresh_with_manifest(&manifest, &single_root(workspace), None, 32, &store, true)
            .unwrap();

        fs::write(workspace.join("Broken.bsl"), [0xff, 0xfe]).unwrap();
        cache.mark_dirty_path(key("Broken.bsl"));
        cache
            .refresh_with_manifest(&manifest, &single_root(workspace), None, 32, &store, false)
            .unwrap();

        assert_eq!(
            cache.stats().pending_dirty_paths,
            1,
            "a read-failed dirty path must be retained for the next refresh"
        );
    }

    /// A path that fails to refresh on every attempt is retained for exactly
    /// [`MAX_DIRTY_REFRESH_FAILURES`] attempts, then dropped from the dirty set (with a warning)
    /// so it stops being retried forever. A fresh `mark_dirty_path` clears the streak, giving the
    /// path the full retry budget again. Removing the bookkeeping (unconditionally re-marking)
    /// makes the drop never happen; removing the reset makes the fresh mark not restore it.
    #[test]
    fn dirty_path_dropped_after_max_consecutive_failures_and_reset_by_fresh_mark() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let store = Store::open(&workspace.join("search.db")).unwrap();

        let mut cache = WorkspaceOverlayCache::default();
        cache.enable_watcher_mode();
        let manifest: HashMap<FileKey, String> = HashMap::new();
        cache
            .refresh_with_manifest(&manifest, &single_root(workspace), None, 32, &store, true)
            .unwrap();

        // Invalid UTF-8 in a regular `.bsl`: `metadata` succeeds, `read_to_string` always fails.
        fs::write(workspace.join("Broken.bsl"), [0xff, 0xfe]).unwrap();
        cache.mark_dirty_path(key("Broken.bsl"));

        // The first K-1 refreshes keep retrying: the path stays dirty.
        for _ in 0..(MAX_DIRTY_REFRESH_FAILURES - 1) {
            cache
                .refresh_with_manifest(&manifest, &single_root(workspace), None, 32, &store, false)
                .unwrap();
            assert_eq!(
                cache.stats().pending_dirty_paths,
                1,
                "the path is retained while under the failure budget"
            );
        }
        // The K-th consecutive failure drops it.
        cache
            .refresh_with_manifest(&manifest, &single_root(workspace), None, 32, &store, false)
            .unwrap();
        assert_eq!(
            cache.stats().pending_dirty_paths,
            0,
            "the path is dropped after exactly MAX_DIRTY_REFRESH_FAILURES failures"
        );

        // A fresh watcher event resets the streak: the path survives the budget again.
        cache.mark_dirty_path(key("Broken.bsl"));
        for _ in 0..(MAX_DIRTY_REFRESH_FAILURES - 1) {
            cache
                .refresh_with_manifest(&manifest, &single_root(workspace), None, 32, &store, false)
                .unwrap();
            assert_eq!(
                cache.stats().pending_dirty_paths,
                1,
                "a fresh mark reset the consecutive-failure count"
            );
        }
    }

    #[test]
    fn reuse_only_refresh_attaches_no_vectors_when_cache_is_empty() {
        // A refresh with `embedder = None` is the interactive ReuseOnly path: a changed file is
        // lexically searchable immediately, but with nothing cached it gets NO semantic vector
        // this turn (and crucially never calls an embedder). The background warmup is what fills
        // vectors later.
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let file = workspace.join("A.bsl");
        fs::write(&file, "Процедура ТолькоЛексика()\nКонецПроцедуры").unwrap();

        let store = Store::open(&workspace.join("search.db")).unwrap();
        let mut manifest = HashMap::new();
        manifest.insert(key("A.bsl"), "different-fingerprint".to_owned());

        let mut cache = WorkspaceOverlayCache::default();
        cache
            .refresh_with_manifest(&manifest, &single_root(workspace), None, 32, &store, true)
            .unwrap();

        let overlay = cache.snapshot();
        assert_eq!(overlay.lexical_documents.len(), 1);
        assert!(overlay.vector_documents.is_empty(), "ReuseOnly must not embed overlay chunks");
        let hits = lexical_hits(&overlay, "ТолькоЛексика", 10);
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn plan_and_publish_attach_only_embedded_vectors() {
        // Phase A plans the refresh and reports the chunk that needs embedding; Phase C publishes
        // a (test-supplied) embedding for it and the snapshot then carries exactly one vector.
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let file = workspace.join("A.bsl");
        fs::write(&file, "Процедура НужноВложить()\nКонецПроцедуры").unwrap();

        let store = Store::open(&workspace.join("search.db")).unwrap();
        let mut manifest = HashMap::new();
        manifest.insert(key("A.bsl"), "different-fingerprint".to_owned());

        let warm = HashMap::new();
        let plan = WorkspaceOverlayCache::plan_full_refresh_from_manifest(
            &manifest,
            &single_root(workspace),
            &store,
            &warm,
            None,
            &HashSet::new(),
        )
        .unwrap();

        let missing = plan.missing_embeddings();
        assert_eq!(missing.len(), 1, "the one changed chunk needs embedding");
        let embedding_key = missing.keys().next().unwrap().clone();

        let mut new_embeddings = HashMap::new();
        new_embeddings.insert(embedding_key, vec![0.1_f32, 0.2, 0.3]);

        let mut cache = WorkspaceOverlayCache::default();
        cache
            .publish_plan(
                plan,
                new_embeddings,
                &super::PublicationBaseline {
                    dirty: HashMap::new(),
                    fence: u64::MAX,
                    unread: HashSet::new(),
                },
                None,
                &store,
            )
            .unwrap();

        let overlay = cache.snapshot();
        assert_eq!(overlay.lexical_documents.len(), 1);
        assert_eq!(overlay.vector_documents.len(), 1, "the embedded chunk now has a vector");
        assert_eq!(overlay.vector_documents[0].embedding, vec![0.1, 0.2, 0.3]);
    }

    #[test]
    fn chunks_with_same_text_but_different_module_get_distinct_vectors() {
        // Two files at different module paths hold a byte-identical procedure body. Their raw chunk
        // text (and thus the legacy `content_hash`) is the same, but the embedded text differs (it
        // folds in the module path), so the overlay must key the embedding cache by the embedding
        // input. Keying by raw-text identity would collapse them onto one shared vector.
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let body = "Процедура Делать()\nКонецПроцедуры";
        let dir_a = workspace.join("CommonModules").join("МодульА").join("Ext");
        let dir_b = workspace.join("CommonModules").join("МодульБ").join("Ext");
        fs::create_dir_all(&dir_a).unwrap();
        fs::create_dir_all(&dir_b).unwrap();
        fs::write(dir_a.join("Module.bsl"), body).unwrap();
        fs::write(dir_b.join("Module.bsl"), body).unwrap();

        let store = Store::open(&workspace.join("search.db")).unwrap();
        let mut manifest = HashMap::new();
        manifest.insert(
            key("CommonModules/МодульА/Ext/Module.bsl"),
            "different-fingerprint".to_owned(),
        );
        manifest.insert(
            key("CommonModules/МодульБ/Ext/Module.bsl"),
            "different-fingerprint".to_owned(),
        );

        let warm = HashMap::new();
        let plan = WorkspaceOverlayCache::plan_full_refresh_from_manifest(
            &manifest,
            &single_root(workspace),
            &store,
            &warm,
            None,
            &HashSet::new(),
        )
        .unwrap();

        // The two chunks share raw text but have distinct embedding inputs, so the plan reports two
        // distinct embedding keys (the bug would report a single collapsed key).
        let missing = plan.missing_embeddings();
        assert_eq!(missing.len(), 2, "same-text chunks in different modules must not collapse");

        // Give each key its own vector; publishing must attach the right vector to each chunk.
        let mut new_embeddings = HashMap::new();
        let mut keys: Vec<String> = missing.keys().cloned().collect();
        keys.sort();
        new_embeddings.insert(keys[0].clone(), vec![1.0_f32, 0.0, 0.0]);
        new_embeddings.insert(keys[1].clone(), vec![0.0_f32, 1.0, 0.0]);

        let mut cache = WorkspaceOverlayCache::default();
        cache
            .publish_plan(
                plan,
                new_embeddings,
                &super::PublicationBaseline {
                    dirty: HashMap::new(),
                    fence: u64::MAX,
                    unread: HashSet::new(),
                },
                None,
                &store,
            )
            .unwrap();

        let overlay = cache.snapshot();
        assert_eq!(overlay.vector_documents.len(), 2, "each chunk keeps its own vector");
        let mut embeddings: Vec<Vec<f32>> =
            overlay.vector_documents.iter().map(|doc| doc.embedding.clone()).collect();
        embeddings.sort_by(|a, b| a.partial_cmp(b).unwrap());
        assert_eq!(embeddings, vec![vec![0.0, 1.0, 0.0], vec![1.0, 0.0, 0.0]]);
    }

    #[test]
    fn publish_clears_only_dirty_paths_superseded_by_the_refresh() {
        // The dirty snapshot is taken before the lock-free embed pass. Publish must clear only
        // those paths; a path the watcher marks DURING the embed window is absent from the
        // snapshot and must survive so a later refresh re-embeds it (a blanket clear would drop it).
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let store = Store::open(&workspace.join("search.db")).unwrap();
        let manifest = HashMap::new();
        let warm = HashMap::new();
        let plan = WorkspaceOverlayCache::plan_full_refresh_from_manifest(
            &manifest,
            &single_root(workspace),
            &store,
            &warm,
            None,
            &HashSet::new(),
        )
        .unwrap();

        let mut cache = WorkspaceOverlayCache::default();
        cache.mark_dirty_path(key("before.bsl"));
        cache.mark_dirty_path(key("reedited.bsl"));
        let dirty_before = cache.publication_baseline();
        // Watcher activity during the lock-free embed window: a brand-new path, plus a re-edit of a
        // path that was already in the snapshot (its sequence advances).
        cache.mark_dirty_path(key("during.bsl"));
        cache.mark_dirty_path(key("reedited.bsl"));

        cache.publish_plan(plan, HashMap::new(), &dirty_before, None, &store).unwrap();

        // before.bsl was superseded and cleared; during.bsl (new) and reedited.bsl (re-marked after
        // the snapshot) both survive so a later refresh re-embeds them.
        assert_eq!(cache.stats().pending_dirty_paths, 2);
        let remaining = cache.dirty_paths_snapshot();
        assert!(remaining.contains_key(&key("during.bsl")));
        assert!(remaining.contains_key(&key("reedited.bsl")));
        assert!(!remaining.contains_key(&key("before.bsl")));
    }

    #[test]
    fn overlay_detects_changed_and_deleted_files() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let file_a = workspace.join("A.bsl");
        let file_b = workspace.join("B.bsl");
        fs::write(&file_a, "Процедура Старая()\nКонецПроцедуры").unwrap();
        fs::write(&file_b, "Процедура Удаляемая()\nКонецПроцедуры").unwrap();

        let db_path = workspace.join("search.db");
        let mut store = Store::open(&db_path).unwrap();
        let chunks_a = crate::Chunker::chunk(&fs::read_to_string(&file_a).unwrap());
        let chunks_b = crate::Chunker::chunk(&fs::read_to_string(&file_b).unwrap());
        let hash_a = blake3::hash(fs::read(&file_a).unwrap().as_slice());
        let hash_b = blake3::hash(fs::read(&file_b).unwrap().as_slice());
        store
            .reindex_file(CONFIGURATION_ROOT_ID, "A.bsl", hash_a.as_bytes(), &chunks_a, None)
            .unwrap();
        store
            .reindex_file(CONFIGURATION_ROOT_ID, "B.bsl", hash_b.as_bytes(), &chunks_b, None)
            .unwrap();

        fs::write(&file_a, "Процедура НовоеИмя()\nКонецПроцедуры").unwrap();
        fs::remove_file(&file_b).unwrap();

        let mut cache = WorkspaceOverlayCache::default();
        cache
            .refresh(
                &store,
                &single_root(workspace),
                None,
                32,
                BaselineHashMode::RawFileBytes,
                true,
            )
            .unwrap();
        let overlay = cache.snapshot();

        assert!(overlay.hidden_paths.contains(&key("A.bsl")));
        assert!(overlay.hidden_paths.contains(&key("B.bsl")));
        assert_eq!(overlay.lexical_documents.len(), 1);
        assert_eq!(overlay.lexical_documents[0].symbol_name, "НовоеИмя");
    }

    /// A `cfe` extension repeats the configuration's layout, so the same relative
    /// path exists under both roots at once. Each copy must reach the overlay as
    /// its own entry, and each hit must say which root it came from — a
    /// path-keyed overlay collapses them into one and silently loses a file.
    #[test]
    fn the_same_relative_path_under_two_roots_stays_two_overlay_entries() {
        const MODULE: &str = "CommonModules/М/Ext/Module.bsl";
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let configuration = workspace.join("cf");
        let extension = workspace.join("cfe/one");
        for (root, symbol) in [(&configuration, "ИзКонфигурации"), (&extension, "ИзРасширения")]
        {
            let file = root.join(MODULE);
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(&file, format!("Процедура {symbol}()\nКонецПроцедуры")).unwrap();
        }
        let (roots, rejected) =
            WorkspaceRoots::build(workspace, &configuration, std::slice::from_ref(&extension));
        assert!(rejected.is_empty(), "the extension sits beside the configuration");

        let store = Store::open(&workspace.join("search.db")).unwrap();
        let mut cache = WorkspaceOverlayCache::default();
        cache.refresh(&store, &roots, None, 32, BaselineHashMode::RawFileBytes, true).unwrap();
        let overlay = cache.snapshot();

        assert_eq!(
            overlay.overlay.changes.len(),
            2,
            "one relative path under two roots is two files, not one"
        );
        let mut owners: Vec<(String, String)> = overlay
            .lexical_documents
            .iter()
            .map(|d| (d.root_id.clone(), d.symbol_name.clone()))
            .collect();
        owners.sort();
        assert_eq!(
            owners,
            vec![
                (CONFIGURATION_ROOT_ID.to_owned(), "ИзКонфигурации".to_owned()),
                ("cfe/one".to_owned(), "ИзРасширения".to_owned()),
            ],
            "each document carries the root it was found under"
        );
    }

    /// Both copies must survive the merge too: the fusion key and the dedup key
    /// are independent, and either one keyed by path alone drops a hit.
    #[test]
    fn two_roots_with_one_relative_path_give_two_hits() {
        const MODULE: &str = "CommonModules/М/Ext/Module.bsl";
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let configuration = workspace.join("cf");
        let extension = workspace.join("cfe/one");
        for root in [&configuration, &extension] {
            let file = root.join(MODULE);
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(&file, "Процедура ОбщееИмя()\nКонецПроцедуры").unwrap();
        }
        let (roots, _) =
            WorkspaceRoots::build(workspace, &configuration, std::slice::from_ref(&extension));

        let store = Store::open(&workspace.join("search.db")).unwrap();
        let mut cache = WorkspaceOverlayCache::default();
        cache.refresh(&store, &roots, None, 32, BaselineHashMode::RawFileBytes, true).unwrap();
        let overlay = cache.snapshot();

        let hits = lexical_hits(&overlay, "ОбщееИмя", 10);
        assert_eq!(hits.len(), 2, "identical symbol at one relative path in two roots");
        let mut roots_of_hits: Vec<&str> = hits.iter().map(|h| h.root_id.as_str()).collect();
        roots_of_hits.sort();
        assert_eq!(roots_of_hits, vec![CONFIGURATION_ROOT_ID, "cfe/one"]);
        for hit in &hits {
            assert_eq!(hit.file_path, MODULE, "the path stays relative to its own root");
        }
    }

    /// A root reached through an alias inside the configuration: the walk arrives
    /// by the declared spelling, but the files belong to the extension the alias
    /// points at. Attributing by the walked spelling alone would hand them to the
    /// configuration, whose subtree the alias sits in.
    #[cfg(unix)]
    #[test]
    fn a_root_declared_through_an_alias_keeps_its_own_files() {
        const MODULE: &str = "CommonModules/М/Ext/Module.bsl";
        let dir = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let workspace = dir.path();
        let configuration = workspace.join("cf");
        fs::create_dir_all(&configuration).unwrap();
        let real = outside.path().join("ext");
        let file = real.join(MODULE);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, "Процедура ЗаСсылкой()\nКонецПроцедуры").unwrap();
        let alias = configuration.join("Linked");
        std::os::unix::fs::symlink(&real, &alias).unwrap();

        let (roots, rejected) =
            WorkspaceRoots::build(workspace, &configuration, std::slice::from_ref(&alias));
        assert!(rejected.is_empty(), "only the alias is inside the configuration, not the root");

        let store = Store::open(&workspace.join("search.db")).unwrap();
        let mut cache = WorkspaceOverlayCache::default();
        cache.refresh(&store, &roots, None, 32, BaselineHashMode::RawFileBytes, true).unwrap();
        let overlay = cache.snapshot();

        assert_eq!(overlay.lexical_documents.len(), 1, "the aliased root's file is indexed");
        let document = &overlay.lexical_documents[0];
        assert_eq!(document.symbol_name, "ЗаСсылкой");
        assert_ne!(
            document.root_id, CONFIGURATION_ROOT_ID,
            "the file belongs to the extension, not to the configuration the alias sits in"
        );
        assert_eq!(document.path, MODULE, "keyed relative to its own root");
    }

    /// A `.bsl` that is a symlink into another root belongs to the root it
    /// physically lives in. Attributing it by the walked spelling would give one
    /// file two entries and put the walk at odds with the point-update path,
    /// which resolves the link in full.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_file_belongs_to_the_root_it_lives_in() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let configuration = workspace.join("cf");
        let extension = workspace.join("cfe/one");
        fs::create_dir_all(&configuration).unwrap();
        fs::create_dir_all(&extension).unwrap();
        let target = extension.join("Target.bsl");
        fs::write(&target, "Процедура ЖивётВРасширении()\nКонецПроцедуры").unwrap();
        std::os::unix::fs::symlink(&target, configuration.join("Alias.bsl")).unwrap();

        let (roots, _) =
            WorkspaceRoots::build(workspace, &configuration, std::slice::from_ref(&extension));
        let store = Store::open(&workspace.join("search.db")).unwrap();
        let mut cache = WorkspaceOverlayCache::default();
        cache.refresh(&store, &roots, None, 32, BaselineHashMode::RawFileBytes, true).unwrap();
        let documents = cache.snapshot().lexical_documents;

        assert_eq!(documents.len(), 1, "one file is one entry: {documents:?}");
        assert_eq!(
            (documents[0].root_id.as_str(), documents[0].path.as_str()),
            ("cfe/one", "Target.bsl"),
            "the root it lives in owns it, not the one holding the alias"
        );
    }

    /// The fingerprint of a symlinked `.bsl` must describe the file whose bytes
    /// are read — the target. Stat'ing the link instead reports the link's own
    /// length and mtime, which do not move when the target is edited, so the
    /// edit would be invisible to every later refresh.
    #[cfg(unix)]
    #[test]
    fn editing_a_symlink_target_is_seen_through_the_link() {
        let dir = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let workspace = dir.path();
        let target = outside.path().join("Настоящий.bsl");
        fs::write(&target, "Процедура Старая()\nКонецПроцедуры").unwrap();
        std::os::unix::fs::symlink(&target, workspace.join("Ссылка.bsl")).unwrap();

        let store = Store::open(&workspace.join("search.db")).unwrap();
        let roots = single_root(workspace);
        let mut cache = WorkspaceOverlayCache::default();
        cache.refresh(&store, &roots, None, 32, BaselineHashMode::RawFileBytes, true).unwrap();
        assert_eq!(cache.snapshot().lexical_documents[0].symbol_name, "Старая");

        fs::write(&target, "Процедура Новая()\nКонецПроцедуры").unwrap();
        cache.refresh(&store, &roots, None, 32, BaselineHashMode::RawFileBytes, true).unwrap();
        let documents = cache.snapshot().lexical_documents;

        assert_eq!(documents.len(), 1);
        assert_eq!(
            documents[0].symbol_name, "Новая",
            "an edit behind the link must move the fingerprint"
        );
    }

    #[test]
    fn lexical_hits_rank_overlay_matches() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let file = workspace.join("A.bsl");
        fs::write(&file, "Процедура НоваяПроцедура123()\nКонецПроцедуры").unwrap();

        let db_path = workspace.join("search.db");
        let store = Store::open(&db_path).unwrap();
        let mut cache = WorkspaceOverlayCache::default();
        cache
            .refresh(
                &store,
                &single_root(workspace),
                None,
                32,
                BaselineHashMode::RawFileBytes,
                true,
            )
            .unwrap();
        let overlay = cache.snapshot();

        let hits = lexical_hits(&overlay, "НоваяПроцедура123", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].symbol_name, "НоваяПроцедура123");
    }

    #[test]
    fn refresh_updates_only_changed_overlay_state() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let file = workspace.join("A.bsl");
        fs::write(&file, "Процедура ВерсияОдин111()\nКонецПроцедуры").unwrap();

        let db_path = workspace.join("search.db");
        let store = Store::open(&db_path).unwrap();
        let mut cache = WorkspaceOverlayCache::default();
        cache
            .refresh(
                &store,
                &single_root(workspace),
                None,
                32,
                BaselineHashMode::RawFileBytes,
                true,
            )
            .unwrap();
        let first = cache.snapshot();
        assert_eq!(first.lexical_documents[0].symbol_name, "ВерсияОдин111");

        cache
            .refresh(
                &store,
                &single_root(workspace),
                None,
                32,
                BaselineHashMode::RawFileBytes,
                true,
            )
            .unwrap();
        let second = cache.snapshot();
        assert_eq!(second.lexical_documents[0].symbol_name, "ВерсияОдин111");

        fs::write(&file, "Процедура ВерсияДва222222()\nКонецПроцедуры").unwrap();
        cache
            .refresh(
                &store,
                &single_root(workspace),
                None,
                32,
                BaselineHashMode::RawFileBytes,
                true,
            )
            .unwrap();
        let third = cache.snapshot();
        assert_eq!(third.lexical_documents[0].symbol_name, "ВерсияДва222222");
    }

    #[test]
    fn stats_report_overlay_shape() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let file_a = workspace.join("A.bsl");
        let file_b = workspace.join("B.bsl");
        fs::write(&file_a, "Процедура Первая()\nКонецПроцедуры").unwrap();
        fs::write(&file_b, "Процедура Вторая()\nКонецПроцедуры").unwrap();

        let db_path = workspace.join("search.db");
        let mut store = Store::open(&db_path).unwrap();
        let chunks_a = crate::Chunker::chunk(&fs::read_to_string(&file_a).unwrap());
        let chunks_b = crate::Chunker::chunk(&fs::read_to_string(&file_b).unwrap());
        let hash_a = blake3::hash(fs::read(&file_a).unwrap().as_slice());
        let hash_b = blake3::hash(fs::read(&file_b).unwrap().as_slice());
        store
            .reindex_file(CONFIGURATION_ROOT_ID, "A.bsl", hash_a.as_bytes(), &chunks_a, None)
            .unwrap();
        store
            .reindex_file(CONFIGURATION_ROOT_ID, "B.bsl", hash_b.as_bytes(), &chunks_b, None)
            .unwrap();

        fs::write(&file_a, "Процедура Измененная()\nКонецПроцедуры").unwrap();
        fs::remove_file(&file_b).unwrap();

        let mut cache = WorkspaceOverlayCache::default();
        cache
            .refresh(
                &store,
                &single_root(workspace),
                None,
                32,
                BaselineHashMode::RawFileBytes,
                true,
            )
            .unwrap();

        assert_eq!(
            cache.stats(),
            WorkspaceOverlayStats {
                overlay_files: 1,
                deleted_files: 1,
                hidden_paths: 2,
                lexical_chunks: 1,
                semantic_chunks: 0,
                cached_embeddings: 0,
                watcher_mode: false,
                pending_dirty_paths: 0,
            }
        );
    }

    #[test]
    fn watcher_mode_refreshes_only_marked_paths() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let file = workspace.join("A.bsl");
        fs::write(&file, "Процедура Базовая()\nКонецПроцедуры").unwrap();

        let db_path = workspace.join("search.db");
        let mut store = Store::open(&db_path).unwrap();
        let chunks = crate::Chunker::chunk(&fs::read_to_string(&file).unwrap());
        let hash = blake3::hash(fs::read(&file).unwrap().as_slice());
        store.reindex_file(CONFIGURATION_ROOT_ID, "A.bsl", hash.as_bytes(), &chunks, None).unwrap();

        let mut cache = WorkspaceOverlayCache::default();
        cache.enable_watcher_mode();
        cache
            .refresh(
                &store,
                &single_root(workspace),
                None,
                32,
                BaselineHashMode::RawFileBytes,
                true,
            )
            .unwrap();
        assert_eq!(cache.stats().overlay_files, 0);

        fs::write(&file, "Процедура ИзWatcher()\nКонецПроцедуры").unwrap();
        cache.mark_dirty_path(key("A.bsl"));
        cache
            .refresh(
                &store,
                &single_root(workspace),
                None,
                32,
                BaselineHashMode::RawFileBytes,
                true,
            )
            .unwrap();

        let overlay = cache.snapshot();
        assert_eq!(overlay.lexical_documents.len(), 1);
        assert_eq!(overlay.lexical_documents[0].symbol_name, "ИзWatcher");
        assert_eq!(cache.stats().pending_dirty_paths, 0);
        assert!(cache.stats().watcher_mode);
    }

    #[test]
    fn manifest_refresh_treats_all_files_as_new_without_manifest() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let file = workspace.join("A.bsl");
        fs::write(&file, "Процедура Новая()\nКонецПроцедуры").unwrap();

        let store = Store::open(&workspace.join("search.db")).unwrap();
        let mut cache = WorkspaceOverlayCache::default();
        let manifest: HashMap<FileKey, String> = HashMap::new();
        cache
            .refresh_with_manifest(&manifest, &single_root(workspace), None, 32, &store, true)
            .unwrap();

        let overlay = cache.snapshot();
        assert_eq!(overlay.lexical_documents.len(), 1);
        assert_eq!(overlay.lexical_documents[0].symbol_name, "Новая");
        assert!(overlay.hidden_paths.is_empty());
    }

    #[test]
    fn manifest_refresh_detects_unchanged_file() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let content = "Процедура Базовая()\nКонецПроцедуры";
        let file = workspace.join("A.bsl");
        fs::write(&file, content).unwrap();

        let fp = fingerprint_content(content, "A.bsl");
        let mut manifest = HashMap::new();
        manifest.insert(key("A.bsl"), fp);

        let store = Store::open(&workspace.join("search.db")).unwrap();
        let mut cache = WorkspaceOverlayCache::default();
        cache
            .refresh_with_manifest(&manifest, &single_root(workspace), None, 32, &store, true)
            .unwrap();

        let overlay = cache.snapshot();
        assert_eq!(overlay.lexical_documents.len(), 0);
        assert!(!overlay.hidden_paths.contains(&key("A.bsl")));
    }

    #[test]
    fn manifest_refresh_detects_modified_file() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let file = workspace.join("A.bsl");
        fs::write(&file, "Процедура Старая()\nКонецПроцедуры").unwrap();

        let mut manifest = HashMap::new();
        manifest.insert(key("A.bsl"), "different-fingerprint".to_owned());

        let store = Store::open(&workspace.join("search.db")).unwrap();
        let mut cache = WorkspaceOverlayCache::default();
        cache
            .refresh_with_manifest(&manifest, &single_root(workspace), None, 32, &store, true)
            .unwrap();

        let overlay = cache.snapshot();
        assert_eq!(overlay.lexical_documents.len(), 1);
        assert_eq!(overlay.lexical_documents[0].symbol_name, "Старая");
        assert!(overlay.hidden_paths.contains(&key("A.bsl")));
    }

    #[test]
    fn manifest_refresh_detects_deleted_baseline_file() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();

        let mut manifest = HashMap::new();
        manifest.insert(key("A.bsl"), "some-fp".to_owned());
        manifest.insert(key("B.bsl"), "other-fp".to_owned());

        let store = Store::open(&workspace.join("search.db")).unwrap();
        let mut cache = WorkspaceOverlayCache::default();
        cache
            .refresh_with_manifest(&manifest, &single_root(workspace), None, 32, &store, true)
            .unwrap();

        let overlay = cache.snapshot();
        assert_eq!(overlay.lexical_documents.len(), 0);
        assert_eq!(overlay.hidden_paths.len(), 2);
        assert!(overlay.hidden_paths.contains(&key("A.bsl")));
        assert!(overlay.hidden_paths.contains(&key("B.bsl")));
    }

    #[test]
    fn reuse_only_never_cold_scans_an_uninitialized_cache() {
        // A fresh cache (initialized=false, watcher_mode=false) holding files that DIVERGE from the
        // baseline. A `ReuseOnly` (allow_cold_scan=false) refresh must NOT walk the tree: if it did,
        // the divergent file would surface as an overlay entry. So the snapshot stays empty and the
        // cache stays uninitialized — the warmup/watcher is what builds it. The SAME cache with
        // allow_cold_scan=true then DOES scan and populate, proving the gate is the only difference.
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let file = workspace.join("A.bsl");
        fs::write(&file, "Процедура Локальная()\nКонецПроцедуры").unwrap();

        let store = Store::open(&workspace.join("search.db")).unwrap();
        let mut manifest = HashMap::new();
        manifest.insert(key("A.bsl"), "different-fingerprint".to_owned());

        let mut cache = WorkspaceOverlayCache::default();
        cache
            .refresh_with_manifest(&manifest, &single_root(workspace), None, 32, &store, false)
            .unwrap();

        let overlay = cache.snapshot();
        assert!(
            overlay.lexical_documents.is_empty(),
            "ReuseOnly over an uninitialized cache must not cold-scan present files"
        );
        assert_eq!(cache.stats().overlay_files, 0);

        // The gate is the only difference: a cold-scan-allowed refresh of the same cache populates.
        cache
            .refresh_with_manifest(&manifest, &single_root(workspace), None, 32, &store, true)
            .unwrap();
        let overlay = cache.snapshot();
        assert_eq!(overlay.lexical_documents.len(), 1);
        assert_eq!(overlay.lexical_documents[0].symbol_name, "Локальная");
    }

    #[test]
    fn reuse_only_skips_full_scan_but_applies_dirty_paths_in_polling_mode() {
        // An already-initialized cache in polling mode (watcher_mode=false). A ReuseOnly refresh
        // must NOT re-run the full scan just because it is polling: with no dirty paths the overlay
        // is unchanged, even after a new on-disk file appears that a cold scan would have picked up.
        // A marked dirty path IS still applied (the cheap incremental arm).
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let file_a = workspace.join("A.bsl");
        fs::write(&file_a, "Процедура ИзменённаяА()\nКонецПроцедуры").unwrap();

        let store = Store::open(&workspace.join("search.db")).unwrap();
        let mut manifest = HashMap::new();
        manifest.insert(key("A.bsl"), "different-fingerprint".to_owned());
        manifest.insert(key("B.bsl"), "different-fingerprint".to_owned());

        // Populate the cache once via the cold-scan path so it is initialized.
        let mut cache = WorkspaceOverlayCache::default();
        cache
            .refresh_with_manifest(&manifest, &single_root(workspace), None, 32, &store, true)
            .unwrap();
        assert_eq!(cache.snapshot().lexical_documents.len(), 1, "A.bsl is the only overlay entry");
        assert!(!cache.stats().watcher_mode, "polling mode for this scenario");

        // A new baseline-divergent file appears on disk. A ReuseOnly refresh with NO dirty paths
        // must leave the overlay untouched (no full rescan) — B.bsl stays absent.
        let file_b = workspace.join("B.bsl");
        fs::write(&file_b, "Процедура НоваяБ()\nКонецПроцедуры").unwrap();
        cache
            .refresh_with_manifest(&manifest, &single_root(workspace), None, 32, &store, false)
            .unwrap();
        let overlay = cache.snapshot();
        assert_eq!(
            overlay.lexical_documents.len(),
            1,
            "polling ReuseOnly must not re-scan the tree"
        );
        assert_eq!(overlay.lexical_documents[0].symbol_name, "ИзменённаяА");

        // A marked dirty path IS still picked up by the cheap incremental arm.
        cache.mark_dirty_path(key("B.bsl"));
        cache
            .refresh_with_manifest(&manifest, &single_root(workspace), None, 32, &store, false)
            .unwrap();
        let overlay = cache.snapshot();
        assert_eq!(overlay.lexical_documents.len(), 2, "the dirty path is applied incrementally");
        let mut names: Vec<String> =
            overlay.lexical_documents.iter().map(|doc| doc.symbol_name.clone()).collect();
        names.sort();
        assert_eq!(names, vec!["ИзменённаяА".to_owned(), "НоваяБ".to_owned()]);
    }

    /// With no feed attached (watcher_mode=false), a refresh allowed to cold-scan is FULL: it
    /// has to see files no mark ever named, or a store nothing feeds would freeze at its last
    /// publication — the very hazard the mode may not be flipped on a bare mark for
    /// (github#186). This test pins the invariant rather than reproducing the defect: it is
    /// expected to pass both before and after the fix.
    #[test]
    fn a_cold_refresh_without_a_feed_rescans_the_tree() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path();
        fs::write(workspace.join("A.bsl"), "Процедура А()\nКонецПроцедуры").unwrap();
        let store = Store::open(&workspace.join("search.db")).unwrap();
        let mut cache = WorkspaceOverlayCache::default();
        cache
            .refresh(
                &store,
                &single_root(workspace),
                None,
                32,
                BaselineHashMode::RawFileBytes,
                true,
            )
            .unwrap();
        assert!(!cache.stats().watcher_mode, "nothing feeds this cache");
        assert_eq!(cache.snapshot().lexical_documents.len(), 1);

        // A file appears on disk and no mark names it.
        fs::write(workspace.join("B.bsl"), "Процедура Б()\nКонецПроцедуры").unwrap();
        cache
            .refresh(
                &store,
                &single_root(workspace),
                None,
                32,
                BaselineHashMode::RawFileBytes,
                true,
            )
            .unwrap();
        assert_eq!(
            cache.snapshot().lexical_documents.len(),
            2,
            "an unfed store keeps seeing the whole tree instead of freezing",
        );
    }

    /// A directory symlink inside a root is part of the workspace universe: the
    /// graph walk follows directory links, and an overlay scan that does not
    /// silently serves a different set of files than every other consumer.
    #[cfg(unix)]
    #[test]
    fn a_module_behind_a_directory_link_reaches_the_overlay() {
        let dir = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let workspace = dir.path();
        let real = outside.path().join("shared");
        fs::create_dir_all(&real).unwrap();
        fs::write(real.join("M.bsl"), "Процедура ЗаКаталожнойСсылкой()\nКонецПроцедуры").unwrap();
        std::os::unix::fs::symlink(&real, workspace.join("linked")).unwrap();

        let store = Store::open(&workspace.join("search.db")).unwrap();
        let roots = single_root(workspace);
        let mut cache = WorkspaceOverlayCache::default();
        cache.refresh(&store, &roots, None, 32, BaselineHashMode::RawFileBytes, true).unwrap();
        let documents = cache.snapshot().lexical_documents;

        assert_eq!(documents.len(), 1, "the linked subtree belongs to the universe");
        assert_eq!(documents[0].symbol_name, "ЗаКаталожнойСсылкой");
    }

    /// A hand-built scan result: the files the walk "saw", each stat'ed from
    /// disk, plus the completeness counters under test.
    fn scanned_with(
        seen: &[(&FileKey, &Path)],
        unreadable: usize,
        canonical_fallbacks: usize,
    ) -> super::ScannedFiles {
        let files = seen
            .iter()
            .map(|(key, path)| {
                let metadata = fs::metadata(path).unwrap();
                super::WorkspaceFileState {
                    key: (*key).clone(),
                    abs_path: path.to_path_buf(),
                    fingerprint: super::FileFingerprint {
                        len: metadata.len(),
                        modified: metadata.modified().ok(),
                        canonical: crate::workspace_roots::canonical_spelling(path),
                    },
                }
            })
            .collect();
        super::ScannedFiles { files, unreadable, loops: 0, dangling: 0, canonical_fallbacks }
    }

    /// A workspace with one baseline-divergent file (an overlay entry hiding its
    /// baseline) and one baseline-equal file (no entry, nothing hidden), fully
    /// refreshed once so the cache holds that state.
    fn cache_with_edited_and_clean(
        workspace: &Path,
    ) -> (WorkspaceOverlayCache, HashMap<FileKey, Vec<u8>>) {
        fs::write(workspace.join("Edited.bsl"), "Процедура Изменённая()\nКонецПроцедуры").unwrap();
        let clean_content = "Процедура Прежняя()\nКонецПроцедуры";
        fs::write(workspace.join("Clean.bsl"), clean_content).unwrap();
        let baseline = HashMap::from([
            (key("Edited.bsl"), b"baseline-differs".to_vec()),
            (
                key("Clean.bsl"),
                super::compute_file_hash(clean_content, BaselineHashMode::RawFileBytes),
            ),
        ]);
        let mut cache = WorkspaceOverlayCache::default();
        cache
            .full_refresh(
                &baseline,
                &single_root(workspace),
                None,
                32,
                BaselineHashMode::RawFileBytes,
                &Store::in_memory().unwrap(),
            )
            .unwrap();
        let overlay = cache.snapshot();
        assert_eq!(overlay.lexical_documents.len(), 1, "only the edited file diverges");
        assert!(overlay.hidden_paths.contains(&key("Edited.bsl")));
        assert!(!overlay.hidden_paths.contains(&key("Clean.bsl")));
        (cache, baseline)
    }

    /// An unclean scan proves nothing about what it did not see: the unseen
    /// entry survives, its baseline stays hidden, and the unseen baseline-equal
    /// file does not become hidden — absence is not evidence of deletion.
    #[test]
    fn an_unclean_scan_keeps_the_unseen_entry_and_hides_nothing_new() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let (mut cache, baseline) = cache_with_edited_and_clean(workspace);

        cache
            .full_refresh_scanned(
                &baseline,
                scanned_with(&[], 1, 0),
                None,
                32,
                BaselineHashMode::RawFileBytes,
            )
            .unwrap();
        let overlay = cache.snapshot();
        assert_eq!(overlay.lexical_documents.len(), 1, "the unseen entry survives");
        assert!(overlay.hidden_paths.contains(&key("Edited.bsl")), "its baseline stays hidden");
        assert!(
            !overlay.hidden_paths.contains(&key("Clean.bsl")),
            "an unseen live file must not become hidden"
        );
        assert!(cache.needs_full_rescan(), "withheld removals demand a clean rescan");
    }

    /// The same protection on the manifest-driven full refresh — an independent
    /// implementation of the same reconciliation.
    #[test]
    fn an_unclean_scan_keeps_the_unseen_entry_on_the_manifest_path() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let store = Store::open(&workspace.join("search.db")).unwrap();
        fs::write(workspace.join("Edited.bsl"), "Процедура Изменённая()\nКонецПроцедуры").unwrap();
        let clean_content = "Процедура Прежняя()\nКонецПроцедуры";
        fs::write(workspace.join("Clean.bsl"), clean_content).unwrap();
        let manifest = HashMap::from([
            (key("Edited.bsl"), "manifest-differs".to_owned()),
            (key("Clean.bsl"), super::fingerprint_content(clean_content, "Clean.bsl")),
        ]);
        let mut cache = WorkspaceOverlayCache::default();
        cache
            .full_refresh_from_manifest(&manifest, &single_root(workspace), None, 32, &store)
            .unwrap();
        assert_eq!(cache.snapshot().lexical_documents.len(), 1);
        assert!(cache.snapshot().hidden_paths.contains(&key("Edited.bsl")));

        cache
            .full_refresh_from_manifest_scanned(
                &manifest,
                scanned_with(&[], 1, 0),
                None,
                32,
                &store,
            )
            .unwrap();
        let overlay = cache.snapshot();
        assert_eq!(overlay.lexical_documents.len(), 1, "the unseen entry survives");
        assert!(overlay.hidden_paths.contains(&key("Edited.bsl")), "its baseline stays hidden");
        assert!(
            !overlay.hidden_paths.contains(&key("Clean.bsl")),
            "an unseen live file must not become hidden"
        );
        assert!(cache.needs_full_rescan());
    }

    /// The planned path publishes through a merge when the scan was unclean: the
    /// unmatched prior entry and the prior hidings survive, while a seen key the
    /// plan proved baseline-equal is removed and un-hidden.
    #[test]
    fn an_unclean_plan_merges_instead_of_replacing() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let store = Store::open(&workspace.join("search.db")).unwrap();
        fs::write(workspace.join("Edited.bsl"), "Процедура Изменённая()\nКонецПроцедуры").unwrap();
        let returned = workspace.join("Returned.bsl");
        fs::write(&returned, "Процедура Вернулась()\nКонецПроцедуры").unwrap();
        let manifest = HashMap::from([
            (key("Edited.bsl"), "manifest-differs".to_owned()),
            (key("Returned.bsl"), "manifest-differs-too".to_owned()),
        ]);
        let roots = single_root(workspace);
        let mut cache = WorkspaceOverlayCache::default();
        let plan = WorkspaceOverlayCache::plan_full_refresh_from_manifest(
            &manifest,
            &roots,
            &store,
            &HashMap::new(),
            None,
            &HashSet::new(),
        )
        .unwrap();
        cache
            .publish_plan(
                plan,
                HashMap::new(),
                &super::PublicationBaseline {
                    dirty: HashMap::new(),
                    fence: u64::MAX,
                    unread: HashSet::new(),
                },
                None,
                &store,
            )
            .unwrap();
        assert_eq!(cache.snapshot().lexical_documents.len(), 2, "both files diverge");

        // `Returned.bsl` goes back to its baseline; the unclean scan sees ONLY it.
        fs::write(&returned, "Процедура ПоБазлайну()\nКонецПроцедуры").unwrap();
        let manifest = HashMap::from([
            (key("Edited.bsl"), "manifest-differs".to_owned()),
            (
                key("Returned.bsl"),
                super::fingerprint_content(
                    "Процедура ПоБазлайну()\nКонецПроцедуры",
                    "Returned.bsl",
                ),
            ),
        ]);
        let plan = WorkspaceOverlayCache::plan_full_refresh_from_manifest_scanned(
            &manifest,
            scanned_with(&[(&key("Returned.bsl"), &returned)], 1, 0),
            &store,
            &HashMap::new(),
            None,
            &HashSet::new(),
        )
        .unwrap();
        cache
            .publish_plan(
                plan,
                HashMap::new(),
                &super::PublicationBaseline {
                    dirty: HashMap::new(),
                    fence: u64::MAX,
                    unread: HashSet::new(),
                },
                None,
                &store,
            )
            .unwrap();
        let overlay = cache.snapshot();
        let names: Vec<&str> =
            overlay.lexical_documents.iter().map(|d| d.symbol_name.as_str()).collect();
        assert_eq!(names, vec!["Изменённая"], "unseen entry kept, baseline-equal seen key removed");
        assert!(
            overlay.hidden_paths.contains(&key("Edited.bsl")),
            "the unseen hiding survives the merge"
        );
        assert!(
            !overlay.hidden_paths.contains(&key("Returned.bsl")),
            "a seen key proven baseline-equal is un-hidden"
        );
        assert!(cache.needs_full_rescan());
    }

    /// A degraded identity (`canonical_fallbacks`) is the same "may not speak
    /// for the tree" verdict as an unreadable subtree.
    #[test]
    fn canonical_fallbacks_also_withhold_removals() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let (mut cache, baseline) = cache_with_edited_and_clean(workspace);

        cache
            .full_refresh_scanned(
                &baseline,
                scanned_with(&[], 0, 1),
                None,
                32,
                BaselineHashMode::RawFileBytes,
            )
            .unwrap();
        assert_eq!(cache.snapshot().lexical_documents.len(), 1, "the unseen entry survives");
        assert!(cache.needs_full_rescan());
    }

    /// Loops and dangling links leave coverage complete: a scan with only those
    /// is clean, so a genuinely-deleted file is removed and its baseline hidden,
    /// exactly as before.
    #[test]
    fn loops_and_dangling_links_do_not_withhold_removals() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let (mut cache, baseline) = cache_with_edited_and_clean(workspace);

        let benign = super::ScannedFiles {
            files: Vec::new(),
            unreadable: 0,
            loops: 2,
            dangling: 3,
            canonical_fallbacks: 0,
        };
        cache
            .full_refresh_scanned(&baseline, benign, None, 32, BaselineHashMode::RawFileBytes)
            .unwrap();
        let overlay = cache.snapshot();
        assert_eq!(overlay.lexical_documents.len(), 0, "a clean scan removes the deleted entry");
        assert!(
            overlay.hidden_paths.contains(&key("Edited.bsl")),
            "the deleted file's baseline is hidden"
        );
        assert!(
            overlay.hidden_paths.contains(&key("Clean.bsl")),
            "every baseline key absent from a clean scan is hidden"
        );
        assert!(!cache.needs_full_rescan(), "a clean scan leaves nothing to catch up");
    }

    /// Positive evidence still acts during an unclean publication: a seen file
    /// back at its baseline is removed and un-hidden, a seen changed file is
    /// re-chunked.
    #[test]
    fn an_unclean_scan_still_applies_what_it_saw() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let returned = workspace.join("Returned.bsl");
        let changed = workspace.join("Changed.bsl");
        fs::write(&returned, "Процедура Вернулась()\nКонецПроцедуры").unwrap();
        fs::write(&changed, "Процедура Старая()\nКонецПроцедуры").unwrap();
        let baseline_content = "Процедура ПоБазлайну()\nКонецПроцедуры";
        let baseline = HashMap::from([
            (
                key("Returned.bsl"),
                super::compute_file_hash(baseline_content, BaselineHashMode::RawFileBytes),
            ),
            (key("Changed.bsl"), b"baseline-differs".to_vec()),
        ]);
        let mut cache = WorkspaceOverlayCache::default();
        cache
            .full_refresh(
                &baseline,
                &single_root(workspace),
                None,
                32,
                BaselineHashMode::RawFileBytes,
                &Store::in_memory().unwrap(),
            )
            .unwrap();
        assert_eq!(cache.snapshot().lexical_documents.len(), 2, "both diverge at first");

        fs::write(&returned, baseline_content).unwrap();
        fs::write(&changed, "Процедура Новая()\nКонецПроцедуры").unwrap();
        cache
            .full_refresh_scanned(
                &baseline,
                scanned_with(
                    &[(&key("Returned.bsl"), &returned), (&key("Changed.bsl"), &changed)],
                    1,
                    0,
                ),
                None,
                32,
                BaselineHashMode::RawFileBytes,
            )
            .unwrap();
        let overlay = cache.snapshot();
        let names: Vec<&str> =
            overlay.lexical_documents.iter().map(|d| d.symbol_name.as_str()).collect();
        assert_eq!(
            names,
            vec!["Новая"],
            "the returned file is removed, the changed one re-chunked"
        );
        assert!(
            !overlay.hidden_paths.contains(&key("Returned.bsl")),
            "returning to baseline un-hides it even on an unclean scan"
        );
        assert!(overlay.hidden_paths.contains(&key("Changed.bsl")));
    }

    /// After an unclean publication the dirty set is EXACTLY the unprocessed
    /// keys: a successfully-refreshed seen key is cleared, a seen-but-unread key
    /// stays, an unseen key stays. All three sides matter: keeping everything
    /// would re-process healthy keys forever, clearing everything loses the
    /// stale ones.
    #[test]
    fn an_unclean_full_refresh_clears_exactly_the_processed_dirty_keys() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let processed = workspace.join("Processed.bsl");
        fs::write(&processed, "Процедура Обработана()\nКонецПроцедуры").unwrap();
        // A directory shaped like a `.bsl`: stat succeeds, reading fails.
        let broken = workspace.join("Broken.bsl");
        fs::create_dir(&broken).unwrap();

        let mut cache = WorkspaceOverlayCache::default();
        cache.mark_dirty_path(key("Processed.bsl"));
        cache.mark_dirty_path(key("Broken.bsl"));
        cache.mark_dirty_path(key("Unseen.bsl"));
        cache
            .full_refresh_scanned(
                &HashMap::new(),
                scanned_with(
                    &[(&key("Processed.bsl"), &processed), (&key("Broken.bsl"), &broken)],
                    1,
                    0,
                ),
                None,
                32,
                BaselineHashMode::RawFileBytes,
            )
            .unwrap();
        let mut dirty: Vec<String> =
            cache.dirty_paths_snapshot().keys().map(|k| k.path.clone()).collect();
        dirty.sort();
        assert_eq!(
            dirty,
            vec!["Broken.bsl".to_owned(), "Unseen.bsl".to_owned()],
            "exactly the unread and the unseen keys stay dirty"
        );
    }

    /// The same exactness on the manifest-driven full refresh.
    #[test]
    fn an_unclean_manifest_refresh_clears_exactly_the_processed_dirty_keys() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let store = Store::open(&workspace.join("search.db")).unwrap();
        let processed = workspace.join("Processed.bsl");
        fs::write(&processed, "Процедура Обработана()\nКонецПроцедуры").unwrap();
        let broken = workspace.join("Broken.bsl");
        fs::create_dir(&broken).unwrap();

        let mut cache = WorkspaceOverlayCache::default();
        cache.mark_dirty_path(key("Processed.bsl"));
        cache.mark_dirty_path(key("Broken.bsl"));
        cache.mark_dirty_path(key("Unseen.bsl"));
        cache
            .full_refresh_from_manifest_scanned(
                &HashMap::new(),
                scanned_with(
                    &[(&key("Processed.bsl"), &processed), (&key("Broken.bsl"), &broken)],
                    1,
                    0,
                ),
                None,
                32,
                &store,
            )
            .unwrap();
        let mut dirty: Vec<String> =
            cache.dirty_paths_snapshot().keys().map(|k| k.path.clone()).collect();
        dirty.sort();
        assert_eq!(dirty, vec!["Broken.bsl".to_owned(), "Unseen.bsl".to_owned()]);
    }

    /// And on the planned path: `publish_plan` clears a superseded dirty flag
    /// only for keys the plan actually processed.
    #[test]
    fn an_unclean_published_plan_clears_exactly_the_processed_dirty_keys() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let store = Store::open(&workspace.join("search.db")).unwrap();
        let processed = workspace.join("Processed.bsl");
        fs::write(&processed, "Процедура Обработана()\nКонецПроцедуры").unwrap();
        let broken = workspace.join("Broken.bsl");
        fs::create_dir(&broken).unwrap();

        let mut cache = WorkspaceOverlayCache::default();
        cache.mark_dirty_path(key("Processed.bsl"));
        cache.mark_dirty_path(key("Broken.bsl"));
        cache.mark_dirty_path(key("Unseen.bsl"));
        let dirty_before = cache.publication_baseline();
        let plan = WorkspaceOverlayCache::plan_full_refresh_from_manifest_scanned(
            &HashMap::new(),
            scanned_with(
                &[(&key("Processed.bsl"), &processed), (&key("Broken.bsl"), &broken)],
                1,
                0,
            ),
            &store,
            &HashMap::new(),
            None,
            &HashSet::new(),
        )
        .unwrap();
        cache.publish_plan(plan, HashMap::new(), &dirty_before, None, &store).unwrap();
        let mut dirty: Vec<String> =
            cache.dirty_paths_snapshot().keys().map(|k| k.path.clone()).collect();
        dirty.sort();
        assert_eq!(dirty, vec!["Broken.bsl".to_owned(), "Unseen.bsl".to_owned()]);
    }

    /// Field-by-field transfer of every walk counter through the adapter: an
    /// end-to-end stand can only make `unreadable` non-zero (permissions), so a
    /// dropped counter would otherwise be invisible.
    #[test]
    fn the_mapping_carries_every_walk_counter() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("M.bsl");
        fs::write(&file, "Процедура П()\nКонецПроцедуры").unwrap();
        let set = project_model::SourceSet {
            files: vec![project_model::WalkedFile {
                root: dir.path().to_path_buf(),
                role: project_model::FileRole::Source,
                walked: file.clone(),
                canonical: file.clone(),
                metadata: fs::metadata(&file).unwrap(),
            }],
            unreadable: 2,
            loops: 3,
            dangling: 4,
            canonicalizations: 6,
            canonical_fallbacks: 5,
        };
        let scanned = super::scanned_files_from(&single_root(dir.path()), &set);
        assert_eq!(scanned.files.len(), 1);
        assert_eq!(
            (scanned.unreadable, scanned.loops, scanned.dangling, scanned.canonical_fallbacks),
            (2, 3, 4, 5),
            "each counter crosses the adapter unchanged"
        );
    }

    /// A pending rescan forces the full-scan arm through BOTH dispatchers even
    /// in initialized watcher mode, and a clean full scan clears it; with the
    /// flag down, watcher mode keeps not rescanning.
    #[test]
    fn a_pending_rescan_forces_the_full_arm_of_the_raw_dispatcher() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        fs::write(workspace.join("A.bsl"), "Процедура П()\nКонецПроцедуры").unwrap();
        let store = Store::open(&workspace.join("search.db")).unwrap();
        let roots = single_root(workspace);
        let mut cache = WorkspaceOverlayCache::default();
        cache.enable_watcher_mode();
        cache.refresh(&store, &roots, None, 32, BaselineHashMode::RawFileBytes, true).unwrap();

        cache
            .full_refresh_scanned(
                &HashMap::new(),
                scanned_with(&[], 1, 0),
                None,
                32,
                BaselineHashMode::RawFileBytes,
            )
            .unwrap();
        assert!(cache.needs_full_rescan());
        let before = project_model::source_set::scans_performed_on_thread();
        cache.refresh(&store, &roots, None, 32, BaselineHashMode::RawFileBytes, true).unwrap();
        assert_eq!(
            project_model::source_set::scans_performed_on_thread() - before,
            1,
            "the pending flag forces the full arm"
        );
        assert!(!cache.needs_full_rescan(), "the clean full scan caught up");
        let before = project_model::source_set::scans_performed_on_thread();
        cache.refresh(&store, &roots, None, 32, BaselineHashMode::RawFileBytes, true).unwrap();
        assert_eq!(
            project_model::source_set::scans_performed_on_thread() - before,
            0,
            "flag down, watcher mode: no rescan"
        );
    }

    /// The manifest dispatcher is an independent condition; the flag must force
    /// it too.
    #[test]
    fn a_pending_rescan_forces_the_full_arm_of_the_manifest_dispatcher() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        fs::write(workspace.join("A.bsl"), "Процедура П()\nКонецПроцедуры").unwrap();
        let store = Store::open(&workspace.join("search.db")).unwrap();
        let roots = single_root(workspace);
        let manifest: HashMap<FileKey, String> = HashMap::new();
        let mut cache = WorkspaceOverlayCache::default();
        cache.enable_watcher_mode();
        cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, true).unwrap();

        cache
            .full_refresh_from_manifest_scanned(
                &manifest,
                scanned_with(&[], 1, 0),
                None,
                32,
                &store,
            )
            .unwrap();
        assert!(cache.needs_full_rescan());
        let before = project_model::source_set::scans_performed_on_thread();
        cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, true).unwrap();
        assert_eq!(
            project_model::source_set::scans_performed_on_thread() - before,
            1,
            "the pending flag forces the full arm"
        );
        assert!(!cache.needs_full_rescan());
        let before = project_model::source_set::scans_performed_on_thread();
        cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, true).unwrap();
        assert_eq!(project_model::source_set::scans_performed_on_thread() - before, 0);
    }

    /// The flag is raised by an unclean publication of EVERY full path and
    /// cleared by a clean one — a flag wired to one path of three would leave
    /// its callers blind.
    #[test]
    fn every_unclean_full_publication_raises_the_rescan_flag() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let store = Store::open(&workspace.join("search.db")).unwrap();
        let roots = single_root(workspace);
        let manifest: HashMap<FileKey, String> = HashMap::new();
        let mut cache = WorkspaceOverlayCache::default();

        cache
            .full_refresh_scanned(
                &HashMap::new(),
                scanned_with(&[], 1, 0),
                None,
                32,
                BaselineHashMode::RawFileBytes,
            )
            .unwrap();
        assert!(cache.needs_full_rescan(), "raw full refresh");
        cache
            .full_refresh(
                &HashMap::new(),
                &roots,
                None,
                32,
                BaselineHashMode::RawFileBytes,
                &Store::in_memory().unwrap(),
            )
            .unwrap();
        assert!(!cache.needs_full_rescan(), "a clean raw refresh clears it");

        cache
            .full_refresh_from_manifest_scanned(
                &manifest,
                scanned_with(&[], 0, 1),
                None,
                32,
                &store,
            )
            .unwrap();
        assert!(cache.needs_full_rescan(), "manifest full refresh");
        cache.full_refresh_from_manifest(&manifest, &roots, None, 32, &store).unwrap();
        assert!(!cache.needs_full_rescan(), "a clean manifest refresh clears it");

        let plan = WorkspaceOverlayCache::plan_full_refresh_from_manifest_scanned(
            &manifest,
            scanned_with(&[], 1, 0),
            &store,
            &HashMap::new(),
            None,
            &HashSet::new(),
        )
        .unwrap();
        cache
            .publish_plan(
                plan,
                HashMap::new(),
                &super::PublicationBaseline {
                    dirty: HashMap::new(),
                    fence: u64::MAX,
                    unread: HashSet::new(),
                },
                None,
                &store,
            )
            .unwrap();
        assert!(cache.needs_full_rescan(), "published unclean plan");
        let plan = WorkspaceOverlayCache::plan_full_refresh_from_manifest(
            &manifest,
            &roots,
            &store,
            &HashMap::new(),
            None,
            &HashSet::new(),
        )
        .unwrap();
        cache
            .publish_plan(
                plan,
                HashMap::new(),
                &super::PublicationBaseline {
                    dirty: HashMap::new(),
                    fence: u64::MAX,
                    unread: HashSet::new(),
                },
                None,
                &store,
            )
            .unwrap();
        assert!(!cache.needs_full_rescan(), "a published clean plan clears it");
    }

    /// Runs `f` under a thread-local subscriber capturing WARN-and-up output; returns the
    /// closure's result and the captured lines.
    #[cfg(unix)]
    fn warns_during<T>(f: impl FnOnce() -> T) -> (T, Vec<String>) {
        use std::sync::{Arc, Mutex};
        #[derive(Clone, Default)]
        struct Buf(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Buf {
            fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(data);
                Ok(data.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Buf {
            type Writer = Buf;
            fn make_writer(&'a self) -> Buf {
                self.clone()
            }
        }
        let buf = Buf::default();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::WARN)
            .with_writer(buf.clone())
            .without_time()
            .finish();
        let result = crate::lifecycle::test_with_subscriber(subscriber, f);
        let bytes = buf.0.lock().unwrap().clone();
        (result, String::from_utf8_lossy(&bytes).lines().map(str::to_owned).collect())
    }

    /// Closes `path` with mode 000 and reports whether that actually revokes access —
    /// under root it cannot, and the caller skips the scenario.
    #[cfg(unix)]
    fn deny_access(path: &Path) -> bool {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o000)).unwrap();
        if path.is_dir() {
            fs::read_dir(path).is_err()
        } else {
            fs::read(path).is_err()
        }
    }

    #[cfg(unix)]
    fn restore_access(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        let mode = if path.is_dir() { 0o755 } else { 0o644 };
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    /// A seen file whose read fails keeps its previous entry and its hiding, gets a retryable
    /// dirty mark, and the publication says so out loud — the old silent `continue` stranded
    /// the stale entry with no trace and no retry.
    #[cfg(unix)]
    #[test]
    fn a_read_failure_during_full_refresh_keeps_the_entry_and_warns() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let edited = workspace.join("Edited.bsl");
        fs::write(&edited, "Процедура Изменённая()\nКонецПроцедуры").unwrap();
        let baseline = HashMap::from([(key("Edited.bsl"), b"baseline-differs".to_vec())]);
        let roots = single_root(workspace);
        let mut cache = WorkspaceOverlayCache::default();
        cache
            .full_refresh(
                &baseline,
                &roots,
                None,
                32,
                BaselineHashMode::RawFileBytes,
                &Store::in_memory().unwrap(),
            )
            .unwrap();
        assert_eq!(cache.snapshot().lexical_documents[0].symbol_name, "Изменённая");
        assert!(cache.snapshot().hidden_paths.contains(&key("Edited.bsl")));

        // Two steps: first MOVE the fingerprint (a bare chmod leaves `(len, mtime)` alone and
        // the equal-fingerprint branch would skip the read entirely), then revoke access.
        fs::write(&edited, "Процедура ИзменённаяЕщёРаз()\nКонецПроцедуры").unwrap();
        if !deny_access(&edited) {
            return;
        }
        let store = Store::in_memory().unwrap();
        let (result, warns) = warns_during(|| {
            cache.full_refresh(&baseline, &roots, None, 32, BaselineHashMode::RawFileBytes, &store)
        });
        restore_access(&edited);
        result.unwrap();

        let overlay = cache.snapshot();
        assert_eq!(
            overlay.lexical_documents[0].symbol_name, "Изменённая",
            "the previous version survives the failed read"
        );
        assert!(overlay.hidden_paths.contains(&key("Edited.bsl")), "its hiding survives");
        assert!(
            cache.dirty_paths_snapshot().contains_key(&key("Edited.bsl")),
            "the key is marked for a bounded retry"
        );
        assert!(warns.iter().any(|l| l.contains("Edited.bsl")), "the publication warns: {warns:?}");
    }

    /// The same protection on the manifest-driven full refresh.
    #[cfg(unix)]
    #[test]
    fn a_read_failure_during_manifest_refresh_keeps_the_entry_and_warns() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let edited = workspace.join("Edited.bsl");
        fs::write(&edited, "Процедура Изменённая()\nКонецПроцедуры").unwrap();
        let manifest = HashMap::from([(key("Edited.bsl"), "manifest-differs".to_owned())]);
        let roots = single_root(workspace);
        let store = Store::open(&workspace.join("search.db")).unwrap();
        let mut cache = WorkspaceOverlayCache::default();
        cache.full_refresh_from_manifest(&manifest, &roots, None, 32, &store).unwrap();
        assert_eq!(cache.snapshot().lexical_documents[0].symbol_name, "Изменённая");

        fs::write(&edited, "Процедура ИзменённаяЕщёРаз()\nКонецПроцедуры").unwrap();
        if !deny_access(&edited) {
            return;
        }
        let (result, warns) =
            warns_during(|| cache.full_refresh_from_manifest(&manifest, &roots, None, 32, &store));
        restore_access(&edited);
        result.unwrap();

        let overlay = cache.snapshot();
        assert_eq!(overlay.lexical_documents[0].symbol_name, "Изменённая");
        assert!(overlay.hidden_paths.contains(&key("Edited.bsl")));
        assert!(cache.dirty_paths_snapshot().contains_key(&key("Edited.bsl")));
        assert!(warns.iter().any(|l| l.contains("Edited.bsl")), "{warns:?}");
    }

    /// On the planned path the failure happens in phase A, which publishes nothing — so phase A
    /// stays silent and `publish_plan` emits exactly one warn per failed key, next to the fact
    /// that an incomplete result went live. The prior entry and its hiding survive the
    /// whole-replace publication.
    #[cfg(unix)]
    #[test]
    fn a_read_failure_survives_a_published_plan_with_one_warn() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let edited = workspace.join("Edited.bsl");
        fs::write(&edited, "Процедура Изменённая()\nКонецПроцедуры").unwrap();
        let manifest = HashMap::from([(key("Edited.bsl"), "manifest-differs".to_owned())]);
        let roots = single_root(workspace);
        let store = Store::open(&workspace.join("search.db")).unwrap();
        let mut cache = WorkspaceOverlayCache::default();
        let plan = WorkspaceOverlayCache::plan_full_refresh_from_manifest(
            &manifest,
            &roots,
            &store,
            &HashMap::new(),
            None,
            &HashSet::new(),
        )
        .unwrap();
        cache
            .publish_plan(
                plan,
                HashMap::new(),
                &super::PublicationBaseline {
                    dirty: HashMap::new(),
                    fence: u64::MAX,
                    unread: HashSet::new(),
                },
                None,
                &store,
            )
            .unwrap();
        assert_eq!(cache.snapshot().lexical_documents[0].symbol_name, "Изменённая");
        assert!(cache.snapshot().hidden_paths.contains(&key("Edited.bsl")));

        fs::write(&edited, "Процедура ИзменённаяЕщёРаз()\nКонецПроцедуры").unwrap();
        if !deny_access(&edited) {
            return;
        }
        let (plan, plan_warns) = warns_during(|| {
            WorkspaceOverlayCache::plan_full_refresh_from_manifest(
                &manifest,
                &roots,
                &store,
                &HashMap::new(),
                None,
                &HashSet::new(),
            )
            .unwrap()
        });
        assert!(
            !plan_warns.iter().any(|l| l.contains("Edited.bsl")),
            "phase A publishes nothing and stays silent: {plan_warns:?}"
        );
        let (result, publish_warns) = warns_during(|| {
            cache.publish_plan(
                plan,
                HashMap::new(),
                &super::PublicationBaseline {
                    dirty: HashMap::new(),
                    fence: u64::MAX,
                    unread: HashSet::new(),
                },
                None,
                &store,
            )
        });
        restore_access(&edited);
        result.unwrap();

        let overlay = cache.snapshot();
        assert_eq!(
            overlay.lexical_documents[0].symbol_name, "Изменённая",
            "the entry survives the publication"
        );
        assert!(overlay.hidden_paths.contains(&key("Edited.bsl")), "its hiding survives");
        assert!(cache.dirty_paths_snapshot().contains_key(&key("Edited.bsl")));
        assert_eq!(
            publish_warns.iter().filter(|l| l.contains("Edited.bsl")).count(),
            1,
            "exactly one warn, at the moment the incomplete result goes live: {publish_warns:?}"
        );
    }

    /// The persisted fingerprint row of a read-failed file claims "verified against the
    /// manifest" — after a failed read the claim must be retracted even when NOTHING else was
    /// read (an empty update map used to skip the save entirely), and retracting it must not
    /// take the verified neighbour's row with it.
    #[cfg(unix)]
    #[test]
    fn a_read_failure_drops_its_fingerprint_row_on_the_manifest_path() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let broken = workspace.join("Broken.bsl");
        fs::write(&broken, "Процедура Ломкая()\nКонецПроцедуры").unwrap();
        let alive = workspace.join("Alive.bsl");
        fs::write(&alive, "Процедура Живая()\nКонецПроцедуры").unwrap();
        let manifest = HashMap::from([
            (key("Broken.bsl"), "manifest-differs".to_owned()),
            (key("Alive.bsl"), "manifest-differs-too".to_owned()),
        ]);
        let roots = single_root(workspace);
        let store = Store::open(&workspace.join("search.db")).unwrap();
        let mut cache = WorkspaceOverlayCache::default();
        cache.full_refresh_from_manifest(&manifest, &roots, None, 32, &store).unwrap();
        let rows = store.load_overlay_fingerprint_cache("").unwrap_or(None).unwrap_or_default();
        assert_eq!(rows.len(), 2, "both reads succeeded, both rows persisted");

        fs::write(&broken, "Процедура ЛомкаяЕщёРаз()\nКонецПроцедуры").unwrap();
        if !deny_access(&broken) {
            return;
        }
        let result = cache.full_refresh_from_manifest(&manifest, &roots, None, 32, &store);
        restore_access(&broken);
        result.unwrap();
        let rows = store.load_overlay_fingerprint_cache("").unwrap_or(None).unwrap_or_default();
        assert!(!rows.contains_key(&key("Broken.bsl")), "the failed read retracts its row");
        assert!(rows.contains_key(&key("Alive.bsl")), "the verified neighbour keeps its row");

        // Single-row leg: with the only file failing, the update map is empty and a guarded
        // save would silently keep the stale row.
        let solo_dir = tempdir().unwrap();
        let solo_ws = solo_dir.path();
        let solo = solo_ws.join("Solo.bsl");
        fs::write(&solo, "Процедура Одна()\nКонецПроцедуры").unwrap();
        let solo_manifest = HashMap::from([(key("Solo.bsl"), "manifest-differs".to_owned())]);
        let solo_roots = single_root(solo_ws);
        let solo_store = Store::open(&solo_ws.join("search.db")).unwrap();
        let mut solo_cache = WorkspaceOverlayCache::default();
        solo_cache
            .full_refresh_from_manifest(&solo_manifest, &solo_roots, None, 32, &solo_store)
            .unwrap();
        assert_eq!(
            solo_store.load_overlay_fingerprint_cache("").unwrap_or(None).unwrap_or_default().len(),
            1
        );
        fs::write(&solo, "Процедура ОднаЕщёРаз()\nКонецПроцедуры").unwrap();
        if !deny_access(&solo) {
            return;
        }
        let result = solo_cache.full_refresh_from_manifest(
            &solo_manifest,
            &solo_roots,
            None,
            32,
            &solo_store,
        );
        restore_access(&solo);
        result.unwrap();
        let rows =
            solo_store.load_overlay_fingerprint_cache("").unwrap_or(None).unwrap_or_default();
        assert!(
            !rows.contains_key(&key("Solo.bsl")),
            "an empty update map must not shield the stale row"
        );
    }

    /// The same 2×2 matrix legs through the planned publication — an independent save site.
    #[cfg(unix)]
    #[test]
    fn a_read_failure_drops_its_fingerprint_row_on_the_planned_path() {
        let plan_and_publish = |manifest: &HashMap<FileKey, String>,
                                roots: &WorkspaceRoots,
                                store: &Store,
                                cache: &mut WorkspaceOverlayCache| {
            let plan = WorkspaceOverlayCache::plan_full_refresh_from_manifest(
                manifest,
                roots,
                store,
                &HashMap::new(),
                None,
                &HashSet::new(),
            )
            .unwrap();
            cache
                .publish_plan(
                    plan,
                    HashMap::new(),
                    &super::PublicationBaseline {
                        dirty: HashMap::new(),
                        fence: u64::MAX,
                        unread: HashSet::new(),
                    },
                    None,
                    store,
                )
                .unwrap();
        };

        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let broken = workspace.join("Broken.bsl");
        fs::write(&broken, "Процедура Ломкая()\nКонецПроцедуры").unwrap();
        let alive = workspace.join("Alive.bsl");
        fs::write(&alive, "Процедура Живая()\nКонецПроцедуры").unwrap();
        let manifest = HashMap::from([
            (key("Broken.bsl"), "manifest-differs".to_owned()),
            (key("Alive.bsl"), "manifest-differs-too".to_owned()),
        ]);
        let roots = single_root(workspace);
        let store = Store::open(&workspace.join("search.db")).unwrap();
        let mut cache = WorkspaceOverlayCache::default();
        plan_and_publish(&manifest, &roots, &store, &mut cache);
        assert_eq!(
            store.load_overlay_fingerprint_cache("").unwrap_or(None).unwrap_or_default().len(),
            2
        );

        fs::write(&broken, "Процедура ЛомкаяЕщёРаз()\nКонецПроцедуры").unwrap();
        if !deny_access(&broken) {
            return;
        }
        plan_and_publish(&manifest, &roots, &store, &mut cache);
        restore_access(&broken);
        let rows = store.load_overlay_fingerprint_cache("").unwrap_or(None).unwrap_or_default();
        assert!(!rows.contains_key(&key("Broken.bsl")), "the failed read retracts its row");
        assert!(rows.contains_key(&key("Alive.bsl")), "the verified neighbour keeps its row");

        let solo_dir = tempdir().unwrap();
        let solo_ws = solo_dir.path();
        let solo = solo_ws.join("Solo.bsl");
        fs::write(&solo, "Процедура Одна()\nКонецПроцедуры").unwrap();
        let solo_manifest = HashMap::from([(key("Solo.bsl"), "manifest-differs".to_owned())]);
        let solo_roots = single_root(solo_ws);
        let solo_store = Store::open(&solo_ws.join("search.db")).unwrap();
        let mut solo_cache = WorkspaceOverlayCache::default();
        plan_and_publish(&solo_manifest, &solo_roots, &solo_store, &mut solo_cache);
        fs::write(&solo, "Процедура ОднаЕщёРаз()\nКонецПроцедуры").unwrap();
        if !deny_access(&solo) {
            return;
        }
        plan_and_publish(&solo_manifest, &solo_roots, &solo_store, &mut solo_cache);
        restore_access(&solo);
        let rows =
            solo_store.load_overlay_fingerprint_cache("").unwrap_or(None).unwrap_or_default();
        assert!(!rows.contains_key(&key("Solo.bsl")), "an empty update map must not shield it");
    }

    /// A POINT refresh that fails to read a file must retract that file's persisted row too:
    /// the dirty mark knows the file changed but dies with the process, while the row would
    /// survive the restart and vouch "verified" for contents nobody read — the next full plan
    /// would then skip the read forever. The verified neighbour's row survives the retraction.
    #[cfg(unix)]
    #[test]
    fn a_point_read_failure_retracts_its_fingerprint_row_across_restart() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let swapped = workspace.join("Swapped.bsl");
        let original = "Процедура Первая()\nКонецПроцедуры";
        fs::write(&swapped, original).unwrap();
        let neighbour = workspace.join("Neighbour.bsl");
        fs::write(&neighbour, "Процедура Соседняя()\nКонецПроцедуры").unwrap();
        // Both files match the manifest: no overlay entries, but both fingerprint rows persist.
        let manifest = HashMap::from([
            (key("Swapped.bsl"), super::fingerprint_content(original, "Swapped.bsl")),
            (
                key("Neighbour.bsl"),
                super::fingerprint_content("Процедура Соседняя()\nКонецПроцедуры", "Neighbour.bsl"),
            ),
        ]);
        let roots = single_root(workspace);
        let store = Store::open(&workspace.join("search.db")).unwrap();
        let mut cache = WorkspaceOverlayCache::default();
        cache.enable_watcher_mode();
        cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, true).unwrap();
        assert_eq!(
            store.load_overlay_fingerprint_cache("").unwrap_or(None).unwrap_or_default().len(),
            2
        );

        // Swap the contents at the SAME (len, mtime): only the retracted row makes the next
        // full plan re-read the file.
        let mtime = fs::metadata(&swapped).unwrap().modified().unwrap();
        fs::write(&swapped, "Процедура Вторая()\nКонецПроцедуры").unwrap();
        fs::File::options().write(true).open(&swapped).unwrap().set_modified(mtime).unwrap();
        if !deny_access(&swapped) {
            return;
        }
        cache.mark_dirty_path(key("Swapped.bsl"));
        let result = cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, false);
        restore_access(&swapped);
        result.unwrap();

        let rows = store.load_overlay_fingerprint_cache("").unwrap_or(None).unwrap_or_default();
        assert!(
            !rows.contains_key(&key("Swapped.bsl")),
            "the unread file's row must not claim it was verified"
        );
        assert!(rows.contains_key(&key("Neighbour.bsl")), "the neighbour's row survives");

        // "Restart": a fresh plan must re-read the swapped file and see the new contents.
        let plan = WorkspaceOverlayCache::plan_full_refresh_from_manifest(
            &manifest,
            &roots,
            &store,
            &HashMap::new(),
            None,
            &HashSet::new(),
        )
        .unwrap();
        assert_eq!(plan.overlay_file_count(), 1, "the swap is visible after the restart");
    }

    /// The stat branch of the point refresh leaves the contents just as unverified as the read
    /// branch, so it must retract the row the same way.
    #[cfg(unix)]
    #[test]
    fn a_point_stat_failure_retracts_its_fingerprint_row_across_restart() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let closed = workspace.join("closed");
        fs::create_dir(&closed).unwrap();
        let swapped = closed.join("Swapped.bsl");
        let original = "Процедура Первая()\nКонецПроцедуры";
        fs::write(&swapped, original).unwrap();
        let manifest = HashMap::from([(
            key("closed/Swapped.bsl"),
            super::fingerprint_content(original, "closed/Swapped.bsl"),
        )]);
        let roots = single_root(workspace);
        let store = Store::open(&workspace.join("search.db")).unwrap();
        let mut cache = WorkspaceOverlayCache::default();
        cache.enable_watcher_mode();
        cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, true).unwrap();
        assert_eq!(
            store.load_overlay_fingerprint_cache("").unwrap_or(None).unwrap_or_default().len(),
            1
        );

        let mtime = fs::metadata(&swapped).unwrap().modified().unwrap();
        fs::write(&swapped, "Процедура Вторая()\nКонецПроцедуры").unwrap();
        fs::File::options().write(true).open(&swapped).unwrap().set_modified(mtime).unwrap();
        if !deny_access(&closed) {
            return;
        }
        cache.mark_dirty_path(key("closed/Swapped.bsl"));
        let result = cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, false);
        restore_access(&closed);
        result.unwrap();

        let rows = store.load_overlay_fingerprint_cache("").unwrap_or(None).unwrap_or_default();
        assert!(
            !rows.contains_key(&key("closed/Swapped.bsl")),
            "a stat failure leaves the contents unverified; the row must go"
        );
        let plan = WorkspaceOverlayCache::plan_full_refresh_from_manifest(
            &manifest,
            &roots,
            &store,
            &HashMap::new(),
            None,
            &HashSet::new(),
        )
        .unwrap();
        assert_eq!(plan.overlay_file_count(), 1, "the swap is visible after the restart");
    }

    /// An unclean FULL publication must not leave unseen keys' rows behind either: the row of a
    /// file the scan never reached would survive a restart and vouch for contents that changed
    /// meanwhile. Losing a row costs one extra read on the next clean pass; a false hit costs a
    /// lost edit — the replace-save keeps only what this pass actually verified.
    #[test]
    fn an_unclean_manifest_publication_drops_unseen_fingerprint_rows() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let unseen = workspace.join("Unseen.bsl");
        let original = "Процедура Первая()\nКонецПроцедуры";
        fs::write(&unseen, original).unwrap();
        let seen = workspace.join("Seen.bsl");
        fs::write(&seen, "Процедура Видимая()\nКонецПроцедуры").unwrap();
        let manifest = HashMap::from([
            (key("Unseen.bsl"), super::fingerprint_content(original, "Unseen.bsl")),
            (
                key("Seen.bsl"),
                super::fingerprint_content("Процедура Видимая()\nКонецПроцедуры", "Seen.bsl"),
            ),
        ]);
        let roots = single_root(workspace);
        let store = Store::open(&workspace.join("search.db")).unwrap();
        let mut cache = WorkspaceOverlayCache::default();
        cache.full_refresh_from_manifest(&manifest, &roots, None, 32, &store).unwrap();
        assert_eq!(
            store.load_overlay_fingerprint_cache("").unwrap_or(None).unwrap_or_default().len(),
            2
        );

        let mtime = fs::metadata(&unseen).unwrap().modified().unwrap();
        fs::write(&unseen, "Процедура Вторая()\nКонецПроцедуры").unwrap();
        fs::File::options().write(true).open(&unseen).unwrap().set_modified(mtime).unwrap();
        cache
            .full_refresh_from_manifest_scanned(
                &manifest,
                scanned_with(&[(&key("Seen.bsl"), &seen)], 1, 0),
                None,
                32,
                &store,
            )
            .unwrap();
        let rows = store.load_overlay_fingerprint_cache("").unwrap_or(None).unwrap_or_default();
        assert!(!rows.contains_key(&key("Unseen.bsl")), "the unverified row is dropped");
        assert!(rows.contains_key(&key("Seen.bsl")), "the verified row survives");

        // Single-row leg: an unclean scan that saw NOTHING leaves an empty update map, and a
        // guarded save would keep the whole stale table.
        let solo_dir = tempdir().unwrap();
        let solo_ws = solo_dir.path();
        let solo = solo_ws.join("Solo.bsl");
        let solo_content = "Процедура Одна()\nКонецПроцедуры";
        fs::write(&solo, solo_content).unwrap();
        let solo_manifest = HashMap::from([(
            key("Solo.bsl"),
            super::fingerprint_content(solo_content, "Solo.bsl"),
        )]);
        let solo_roots = single_root(solo_ws);
        let solo_store = Store::open(&solo_ws.join("search.db")).unwrap();
        let mut solo_cache = WorkspaceOverlayCache::default();
        solo_cache
            .full_refresh_from_manifest(&solo_manifest, &solo_roots, None, 32, &solo_store)
            .unwrap();
        assert_eq!(
            solo_store.load_overlay_fingerprint_cache("").unwrap_or(None).unwrap_or_default().len(),
            1
        );
        solo_cache
            .full_refresh_from_manifest_scanned(
                &solo_manifest,
                scanned_with(&[], 1, 0),
                None,
                32,
                &solo_store,
            )
            .unwrap();
        assert!(
            solo_store
                .load_overlay_fingerprint_cache("")
                .unwrap_or(None)
                .unwrap_or_default()
                .is_empty(),
            "nothing was verified, nothing may stay claimed"
        );
    }

    /// The same unseen-row policy through the planned publication.
    #[test]
    fn an_unclean_published_plan_drops_unseen_fingerprint_rows() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let unseen = workspace.join("Unseen.bsl");
        let original = "Процедура Первая()\nКонецПроцедуры";
        fs::write(&unseen, original).unwrap();
        let seen = workspace.join("Seen.bsl");
        fs::write(&seen, "Процедура Видимая()\nКонецПроцедуры").unwrap();
        let manifest = HashMap::from([
            (key("Unseen.bsl"), super::fingerprint_content(original, "Unseen.bsl")),
            (
                key("Seen.bsl"),
                super::fingerprint_content("Процедура Видимая()\nКонецПроцедуры", "Seen.bsl"),
            ),
        ]);
        let roots = single_root(workspace);
        let store = Store::open(&workspace.join("search.db")).unwrap();
        let mut cache = WorkspaceOverlayCache::default();
        let plan = WorkspaceOverlayCache::plan_full_refresh_from_manifest(
            &manifest,
            &roots,
            &store,
            &HashMap::new(),
            None,
            &HashSet::new(),
        )
        .unwrap();
        cache
            .publish_plan(
                plan,
                HashMap::new(),
                &super::PublicationBaseline {
                    dirty: HashMap::new(),
                    fence: u64::MAX,
                    unread: HashSet::new(),
                },
                None,
                &store,
            )
            .unwrap();
        assert_eq!(
            store.load_overlay_fingerprint_cache("").unwrap_or(None).unwrap_or_default().len(),
            2
        );

        let plan = WorkspaceOverlayCache::plan_full_refresh_from_manifest_scanned(
            &manifest,
            scanned_with(&[(&key("Seen.bsl"), &seen)], 1, 0),
            &store,
            &HashMap::new(),
            None,
            &HashSet::new(),
        )
        .unwrap();
        cache
            .publish_plan(
                plan,
                HashMap::new(),
                &super::PublicationBaseline {
                    dirty: HashMap::new(),
                    fence: u64::MAX,
                    unread: HashSet::new(),
                },
                None,
                &store,
            )
            .unwrap();
        let rows = store.load_overlay_fingerprint_cache("").unwrap_or(None).unwrap_or_default();
        assert!(!rows.contains_key(&key("Unseen.bsl")), "the unverified row is dropped");
        assert!(rows.contains_key(&key("Seen.bsl")), "the verified row survives");

        let solo_dir = tempdir().unwrap();
        let solo_ws = solo_dir.path();
        let solo = solo_ws.join("Solo.bsl");
        let solo_content = "Процедура Одна()\nКонецПроцедуры";
        fs::write(&solo, solo_content).unwrap();
        let solo_manifest = HashMap::from([(
            key("Solo.bsl"),
            super::fingerprint_content(solo_content, "Solo.bsl"),
        )]);
        let solo_roots = single_root(solo_ws);
        let solo_store = Store::open(&solo_ws.join("search.db")).unwrap();
        let mut solo_cache = WorkspaceOverlayCache::default();
        let plan = WorkspaceOverlayCache::plan_full_refresh_from_manifest(
            &solo_manifest,
            &solo_roots,
            &solo_store,
            &HashMap::new(),
            None,
            &HashSet::new(),
        )
        .unwrap();
        solo_cache
            .publish_plan(
                plan,
                HashMap::new(),
                &super::PublicationBaseline {
                    dirty: HashMap::new(),
                    fence: u64::MAX,
                    unread: HashSet::new(),
                },
                None,
                &solo_store,
            )
            .unwrap();
        let plan = WorkspaceOverlayCache::plan_full_refresh_from_manifest_scanned(
            &solo_manifest,
            scanned_with(&[], 1, 0),
            &solo_store,
            &HashMap::new(),
            None,
            &HashSet::new(),
        )
        .unwrap();
        solo_cache
            .publish_plan(
                plan,
                HashMap::new(),
                &super::PublicationBaseline {
                    dirty: HashMap::new(),
                    fence: u64::MAX,
                    unread: HashSet::new(),
                },
                None,
                &solo_store,
            )
            .unwrap();
        assert!(
            solo_store
                .load_overlay_fingerprint_cache("")
                .unwrap_or(None)
                .unwrap_or_default()
                .is_empty(),
            "nothing was verified, nothing may stay claimed"
        );
    }

    /// The point refresh distinguishes "the file is provably gone" (remove the entry) from
    /// "the file is alive but unreachable right now" (keep everything, retry with a budget).
    /// Provably gone: NotFound, a directory replaced by a file, a proven symlink cycle — final
    /// or in an ancestor, agreeing with the walk. Alive: a permission error, and a long but
    /// FINITE link chain (`ELOOP` alone is not proof of a cycle). The raw and the manifest
    /// implementations answer independently, so both are exercised.
    #[cfg(unix)]
    #[test]
    fn a_point_refresh_removes_proven_absence_and_keeps_live_files_raw() {
        point_absence_matrix(false);
    }

    #[cfg(unix)]
    #[test]
    fn a_point_refresh_removes_proven_absence_and_keeps_live_files_manifest() {
        point_absence_matrix(true);
    }

    #[cfg(unix)]
    fn point_absence_matrix(use_manifest: bool) {
        let dir = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let workspace = dir.path();
        let body = |name: &str| format!("Процедура {name}()\nКонецПроцедуры");

        fs::write(workspace.join("Gone.bsl"), body("Удалённая")).unwrap();
        fs::create_dir(workspace.join("dir")).unwrap();
        fs::write(workspace.join("dir/Inside.bsl"), body("ВКаталоге")).unwrap();
        fs::write(workspace.join("SelfLoop.bsl"), body("Закольцованная")).unwrap();
        fs::create_dir(workspace.join("anc")).unwrap();
        fs::write(workspace.join("anc/Under.bsl"), body("ПодЦиклом")).unwrap();
        fs::create_dir(workspace.join("perm")).unwrap();
        fs::write(workspace.join("perm/Kept.bsl"), body("Недоступная")).unwrap();
        fs::write(workspace.join("Chain.bsl"), body("Цепочечная")).unwrap();

        let store = Store::open(&workspace.join("search.db")).unwrap();
        let roots = single_root(workspace);
        let mut cache = WorkspaceOverlayCache::default();
        cache.enable_watcher_mode();
        let manifest: HashMap<FileKey, String> =
            HashMap::from([(key("Gone.bsl"), "manifest-differs".to_owned())]);
        // The raw arm resolves its baseline through the store, so the baseline row lives there.
        let gone = key("Gone.bsl");
        store.upsert_file(&gone.root_id, &gone.path, b"baseline-differs", "code").unwrap();
        if use_manifest {
            cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, true).unwrap();
        } else {
            cache.refresh(&store, &roots, None, 32, BaselineHashMode::RawFileBytes, true).unwrap();
        }
        assert_eq!(cache.snapshot().lexical_documents.len(), 6, "all six files indexed");

        // Provably gone, four shapes.
        fs::remove_file(workspace.join("Gone.bsl")).unwrap();
        fs::remove_dir_all(workspace.join("dir")).unwrap();
        fs::write(workspace.join("dir"), "не каталог").unwrap();
        fs::remove_file(workspace.join("SelfLoop.bsl")).unwrap();
        std::os::unix::fs::symlink("SelfLoop.bsl", workspace.join("SelfLoop.bsl")).unwrap();
        fs::remove_dir_all(workspace.join("anc")).unwrap();
        std::os::unix::fs::symlink("anc", workspace.join("anc")).unwrap();
        // Alive but unreachable, two shapes.
        if !deny_access(&workspace.join("perm")) {
            return;
        }
        fs::remove_file(workspace.join("Chain.bsl")).unwrap();
        let mut target = outside.path().join("Target.bsl");
        fs::write(&target, body("Цепочечная")).unwrap();
        for hop in (0..64).rev() {
            let link = outside.path().join(format!("chain{hop}"));
            std::os::unix::fs::symlink(&target, &link).unwrap();
            target = link;
        }
        std::os::unix::fs::symlink(&target, workspace.join("Chain.bsl")).unwrap();

        for path in [
            "Gone.bsl",
            "dir/Inside.bsl",
            "SelfLoop.bsl",
            "anc/Under.bsl",
            "perm/Kept.bsl",
            "Chain.bsl",
        ] {
            cache.mark_dirty_path(key(path));
        }
        let result = if use_manifest {
            cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, false)
        } else {
            cache.refresh(&store, &roots, None, 32, BaselineHashMode::RawFileBytes, false)
        };
        restore_access(&workspace.join("perm"));
        result.unwrap();

        let overlay = cache.snapshot();
        let mut names: Vec<&str> =
            overlay.lexical_documents.iter().map(|d| d.symbol_name.as_str()).collect();
        names.sort();
        assert_eq!(
            names,
            vec!["Недоступная", "Цепочечная"],
            "proven absence removes, live-but-unreachable keeps"
        );
        assert!(
            overlay.hidden_paths.contains(&key("Gone.bsl")),
            "a provably deleted baseline file is hidden"
        );
        let dirty = cache.dirty_paths_snapshot();
        assert!(dirty.contains_key(&key("perm/Kept.bsl")), "unreachable keys stay marked");
        assert!(dirty.contains_key(&key("Chain.bsl")));
        assert!(!dirty.contains_key(&key("Gone.bsl")), "removals consume their mark");
    }

    /// Operations that reset or fully re-certify the overlay state must drop the pending-rescan
    /// flag with the rest of it: `mark_initialized_clean` PROVES the store equals the disk, and
    /// a surviving flag would force the full walk the caller just proved unnecessary.
    #[test]
    fn a_clean_initialization_resets_the_pending_rescan() {
        let mut cache = WorkspaceOverlayCache::default();
        cache
            .full_refresh_scanned(
                &HashMap::new(),
                scanned_with(&[], 1, 0),
                None,
                32,
                BaselineHashMode::RawFileBytes,
            )
            .unwrap();
        assert!(cache.needs_full_rescan());
        cache.mark_initialized_clean();
        assert!(!cache.needs_full_rescan(), "a proven-clean initialization owes no rescan");

        cache
            .full_refresh_scanned(
                &HashMap::new(),
                scanned_with(&[], 1, 0),
                None,
                32,
                BaselineHashMode::RawFileBytes,
            )
            .unwrap();
        assert!(cache.needs_full_rescan());
        cache.clear();
        assert!(!cache.needs_full_rescan(), "cleared state carries no debts of the old state");
    }

    /// A PROVEN point removal must retract the file's persisted fingerprint row along with the
    /// entry: the row claims "verified", and if the path is later recreated with the same size
    /// and mtime, the surviving row would make the next full plan skip reading it.
    #[test]
    fn a_point_removal_retracts_its_fingerprint_row() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let solo = workspace.join("Solo.bsl");
        let content = "Процедура Одна()\nКонецПроцедуры";
        fs::write(&solo, content).unwrap();
        let manifest =
            HashMap::from([(key("Solo.bsl"), super::fingerprint_content(content, "Solo.bsl"))]);
        let roots = single_root(workspace);
        let store = Store::open(&workspace.join("search.db")).unwrap();
        let mut cache = WorkspaceOverlayCache::default();
        cache.enable_watcher_mode();
        cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, true).unwrap();
        assert_eq!(
            store.load_overlay_fingerprint_cache("").unwrap_or(None).unwrap_or_default().len(),
            1
        );

        fs::remove_file(&solo).unwrap();
        cache.mark_dirty_path(key("Solo.bsl"));
        cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, false).unwrap();
        let rows = store.load_overlay_fingerprint_cache("").unwrap_or(None).unwrap_or_default();
        assert!(
            !rows.contains_key(&key("Solo.bsl")),
            "a removed file's row must not survive to vouch for a future namesake"
        );
    }

    /// A published plan must not resurrect fingerprint rows retracted while its lock-free embed
    /// phase ran: the surviving dirty mark says "not verified", and the row written from the
    /// plan's stale snapshot would contradict it across a restart.
    #[cfg(unix)]
    #[test]
    fn a_stale_plan_does_not_resurrect_retracted_rows() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let swapped = workspace.join("Swapped.bsl");
        let original = "Процедура Первая()\nКонецПроцедуры";
        fs::write(&swapped, original).unwrap();
        let manifest = HashMap::from([(
            key("Swapped.bsl"),
            super::fingerprint_content(original, "Swapped.bsl"),
        )]);
        let roots = single_root(workspace);
        let store = Store::open(&workspace.join("search.db")).unwrap();
        let mut cache = WorkspaceOverlayCache::default();
        cache.enable_watcher_mode();
        cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, true).unwrap();

        // Phase A of a slow warmup snapshots the row while the file is still the original.
        let dirty_before = cache.publication_baseline();
        let plan = WorkspaceOverlayCache::plan_full_refresh_from_manifest(
            &manifest,
            &roots,
            &store,
            &HashMap::new(),
            None,
            &HashSet::new(),
        )
        .unwrap();

        // Mid-embed, the file is swapped at the same (len, mtime) and a point refresh fails to
        // read it — retracting the row and keeping the key dirty.
        let mtime = fs::metadata(&swapped).unwrap().modified().unwrap();
        fs::write(&swapped, "Процедура Вторая()\nКонецПроцедуры").unwrap();
        fs::File::options().write(true).open(&swapped).unwrap().set_modified(mtime).unwrap();
        if !deny_access(&swapped) {
            return;
        }
        cache.mark_dirty_path(key("Swapped.bsl"));
        let point = cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, false);
        restore_access(&swapped);
        point.unwrap();
        assert!(!store
            .load_overlay_fingerprint_cache("")
            .unwrap_or(None)
            .unwrap_or_default()
            .contains_key(&key("Swapped.bsl")));

        cache.publish_plan(plan, HashMap::new(), &dirty_before, None, &store).unwrap();
        let rows = store.load_overlay_fingerprint_cache("").unwrap_or(None).unwrap_or_default();
        assert!(
            !rows.contains_key(&key("Swapped.bsl")),
            "the stale plan must not overrule the retraction"
        );
        let plan = WorkspaceOverlayCache::plan_full_refresh_from_manifest(
            &manifest,
            &roots,
            &store,
            &HashMap::new(),
            None,
            &HashSet::new(),
        )
        .unwrap();
        assert_eq!(plan.overlay_file_count(), 1, "the swap is re-read and visible");
    }

    /// A stat/read failure's obligations — the retracted row and the budgeted retry mark — are
    /// settled AT the failure, and a later build fault in the same batch neither aborts the
    /// loop nor disturbs them: every drained key owns its own outcome.
    #[cfg(unix)]
    #[test]
    fn a_failure_earlier_in_a_batch_survives_a_later_batch_error() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let broken = workspace.join("Broken.bsl");
        let content = "Процедура Ломкая()\nКонецПроцедуры";
        fs::write(&broken, content).unwrap();
        let manifest =
            HashMap::from([(key("Broken.bsl"), super::fingerprint_content(content, "Broken.bsl"))]);
        let roots = single_root(workspace);
        let store = Store::open(&workspace.join("search.db")).unwrap();
        let mut cache = WorkspaceOverlayCache::default();
        cache.enable_watcher_mode();
        cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, true).unwrap();
        assert_eq!(
            store.load_overlay_fingerprint_cache("").unwrap_or(None).unwrap_or_default().len(),
            1
        );

        // A second, new file whose processing needs the embedder — which is unreachable, so the
        // batch errors AFTER the first key's failure.
        fs::write(workspace.join("New.bsl"), "Процедура Новая()\nКонецПроцедуры").unwrap();
        let embedder = super::Embedder::new(crate::EmbedderConfig {
            base_url: "http://127.0.0.1:1".to_owned(),
            model: "test-model".to_owned(),
            dim: Some(3),
            api_key: None,
            provider: None,
            ..Default::default()
        });
        if !deny_access(&broken) {
            return;
        }
        let result = cache.refresh_dirty_paths_from_manifest(
            vec![key("Broken.bsl"), key("New.bsl")],
            super::ManifestBaseline { fingerprints: &manifest, store: &store },
            &roots,
            Some(&embedder),
            32,
            &HashMap::new(),
        );
        restore_access(&broken);
        assert!(
            result.is_err(),
            "the batch settles every key before returning the embedding failure"
        );

        let rows = store.load_overlay_fingerprint_cache("").unwrap_or(None).unwrap_or_default();
        assert!(
            !rows.contains_key(&key("Broken.bsl")),
            "the retraction happened at the first key's failure"
        );
        assert!(
            cache.dirty_paths_snapshot().contains_key(&key("Broken.bsl")),
            "the retry mark happened at the first key's failure"
        );
        assert!(
            cache.dirty_paths_snapshot().contains_key(&key("New.bsl")),
            "the build-faulted key stays marked for the retry"
        );
    }

    /// `NotFound` under a root that is ITSELF unreachable proves nothing about the file: the
    /// full walk classifies a failed root as incomplete coverage, and the point path must not
    /// read the same situation as the file's deletion.
    #[test]
    fn a_missing_root_keeps_the_point_entry() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let configuration = workspace.join("cf");
        fs::create_dir(&configuration).unwrap();
        fs::write(configuration.join("A.bsl"), "Процедура Живая()\nКонецПроцедуры").unwrap();
        let manifest = HashMap::from([(key("A.bsl"), "manifest-differs".to_owned())]);
        let (roots, _) = WorkspaceRoots::build(workspace, &configuration, &[]);
        let store = Store::open(&workspace.join("search.db")).unwrap();
        let mut cache = WorkspaceOverlayCache::default();
        cache.enable_watcher_mode();
        cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, true).unwrap();
        assert_eq!(cache.snapshot().lexical_documents.len(), 1);

        fs::rename(&configuration, workspace.join("cf.saved")).unwrap();
        cache.mark_dirty_path(key("A.bsl"));
        let point = cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, false);
        fs::rename(workspace.join("cf.saved"), &configuration).unwrap();
        point.unwrap();

        assert_eq!(
            cache.snapshot().lexical_documents.len(),
            1,
            "an unreachable root is not evidence about the file"
        );
        assert!(
            cache.dirty_paths_snapshot().contains_key(&key("A.bsl")),
            "the key stays marked for a retry"
        );
    }

    /// The failure budget counts CONSECUTIVE failures: a successful full publication of the key
    /// resets it, so unrelated failures spread over time never add up to a drop.
    #[test]
    fn a_successful_full_publication_resets_the_read_failure_budget() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let broken = workspace.join("Broken.bsl");
        let manifest = HashMap::from([(key("Broken.bsl"), "manifest-differs".to_owned())]);
        let roots = single_root(workspace);
        let store = Store::open(&workspace.join("search.db")).unwrap();
        let mut cache = WorkspaceOverlayCache::default();

        for round in 0..MAX_DIRTY_REFRESH_FAILURES {
            // A read failure (invalid UTF-8), then a successful publication of the same key.
            fs::write(&broken, [0xff, 0xfe, round as u8]).unwrap();
            cache.full_refresh_from_manifest(&manifest, &roots, None, 32, &store).unwrap();
            assert!(
                cache.dirty_paths_snapshot().contains_key(&key("Broken.bsl")),
                "round {round}: every non-consecutive failure has the full budget"
            );
            fs::write(&broken, format!("Процедура Раз{round}()\nКонецПроцедуры")).unwrap();
            cache.full_refresh_from_manifest(&manifest, &roots, None, 32, &store).unwrap();
        }
    }

    /// The plan's completeness accessors carry the exact walk counters and the exact number of
    /// failed reads: a consumer reports "incomplete warmup" from these, and a zeroed field
    /// there would be indistinguishable from a complete pass.
    #[cfg(unix)]
    #[test]
    fn a_plan_reports_its_scan_counters_and_read_failures() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let broken = workspace.join("Broken.bsl");
        fs::write(&broken, "Процедура Ломкая()\nКонецПроцедуры").unwrap();
        let store = Store::open(&workspace.join("search.db")).unwrap();
        let scanned = scanned_with(&[(&key("Broken.bsl"), &broken)], 2, 1);
        if !deny_access(&broken) {
            return;
        }
        let plan = WorkspaceOverlayCache::plan_full_refresh_from_manifest_scanned(
            &HashMap::new(),
            scanned,
            &store,
            &HashMap::new(),
            None,
            &HashSet::new(),
        )
        .unwrap();
        restore_access(&broken);
        assert!(!plan.scan_is_clean());
        assert_eq!(
            (plan.scan_unreadable(), plan.scan_canonical_fallbacks(), plan.read_failure_count()),
            (2, 1, 1),
            "each completeness field crosses the accessor unchanged"
        );
    }

    /// Two link targets of the SAME byte length with the SAME forced mtime — the shape that
    /// makes a `(len, mtime)` fingerprint blind, so only the physical spelling can tell them
    /// apart.
    #[cfg(unix)]
    fn equal_stat_targets(outside: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
        let first = outside.join("Первый.bsl");
        let second = outside.join("Второй.bsl");
        fs::write(&first, "Процедура ПерваяЦель()\nКонецПроцедуры").unwrap();
        fs::write(&second, "Процедура ВтораяЦель()\nКонецПроцедуры").unwrap();
        assert_eq!(
            fs::metadata(&first).unwrap().len(),
            fs::metadata(&second).unwrap().len(),
            "the stand needs equal lengths"
        );
        let mtime = fs::metadata(&first).unwrap().modified().unwrap();
        fs::File::options().write(true).open(&second).unwrap().set_modified(mtime).unwrap();
        (first, second)
    }

    /// Retargeting a link onto a file with the same `(len, mtime)` must be seen by the next
    /// full refresh: a file's identity includes WHERE it physically is, and a fingerprint blind
    /// to the spelling serves the old target forever.
    #[cfg(unix)]
    #[test]
    fn a_retargeted_link_at_equal_stat_is_reread_by_a_full_refresh() {
        let dir = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let workspace = dir.path();
        let (first, second) = equal_stat_targets(outside.path());
        let alias = workspace.join("Alias.bsl");
        std::os::unix::fs::symlink(&first, &alias).unwrap();

        let store = Store::open(&workspace.join("search.db")).unwrap();
        let roots = single_root(workspace);
        let mut cache = WorkspaceOverlayCache::default();
        cache.refresh(&store, &roots, None, 32, BaselineHashMode::RawFileBytes, true).unwrap();
        assert_eq!(cache.snapshot().lexical_documents[0].symbol_name, "ПерваяЦель");

        fs::remove_file(&alias).unwrap();
        std::os::unix::fs::symlink(&second, &alias).unwrap();
        cache.refresh(&store, &roots, None, 32, BaselineHashMode::RawFileBytes, true).unwrap();
        assert_eq!(
            cache.snapshot().lexical_documents[0].symbol_name,
            "ВтораяЦель",
            "the retarget moves the fingerprint even at equal (len, mtime)"
        );
    }

    /// The same blindness through both persisted-fingerprint gates: a cache row taken before
    /// the retarget must not pass for the new target, on the in-place manifest refresh and on
    /// the planned one alike.
    #[cfg(unix)]
    #[test]
    fn a_retargeted_link_at_equal_stat_misses_both_fingerprint_gates() {
        let dir = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let workspace = dir.path();
        let (first, second) = equal_stat_targets(outside.path());
        let alias = workspace.join("Alias.bsl");
        std::os::unix::fs::symlink(&first, &alias).unwrap();
        // The file EQUALS the manifest, so the first pass leaves no overlay entry — only the
        // cache row; the second pass must miss that row by spelling, re-read and diverge.
        let manifest = HashMap::from([(
            key("Alias.bsl"),
            super::fingerprint_content(&fs::read_to_string(&first).unwrap(), "Alias.bsl"),
        )]);
        let roots = single_root(workspace);

        let store = Store::open(&workspace.join("search.db")).unwrap();
        let mut cache = WorkspaceOverlayCache::default();
        cache.full_refresh_from_manifest(&manifest, &roots, None, 32, &store).unwrap();
        assert_eq!(cache.snapshot().lexical_documents.len(), 0, "baseline-equal: row only");
        assert_eq!(
            store.load_overlay_fingerprint_cache("").unwrap_or(None).unwrap_or_default().len(),
            1
        );

        fs::remove_file(&alias).unwrap();
        std::os::unix::fs::symlink(&second, &alias).unwrap();
        cache.full_refresh_from_manifest(&manifest, &roots, None, 32, &store).unwrap();
        assert_eq!(
            cache.snapshot().lexical_documents.len(),
            1,
            "the stale row must not vouch for the retargeted link"
        );
        assert_eq!(cache.snapshot().lexical_documents[0].symbol_name, "ВтораяЦель");

        // The planned path re-reads through its own, independent gate.
        let plan_store = Store::open(&workspace.join("search-plan.db")).unwrap();
        let mut plan_cache = WorkspaceOverlayCache::default();
        fs::remove_file(&alias).unwrap();
        std::os::unix::fs::symlink(&first, &alias).unwrap();
        let plan = WorkspaceOverlayCache::plan_full_refresh_from_manifest(
            &manifest,
            &roots,
            &plan_store,
            &HashMap::new(),
            None,
            &HashSet::new(),
        )
        .unwrap();
        plan_cache
            .publish_plan(
                plan,
                HashMap::new(),
                &super::PublicationBaseline {
                    dirty: HashMap::new(),
                    fence: u64::MAX,
                    unread: HashSet::new(),
                },
                None,
                &plan_store,
            )
            .unwrap();
        fs::remove_file(&alias).unwrap();
        std::os::unix::fs::symlink(&second, &alias).unwrap();
        let plan = WorkspaceOverlayCache::plan_full_refresh_from_manifest(
            &manifest,
            &roots,
            &plan_store,
            &HashMap::new(),
            None,
            &HashSet::new(),
        )
        .unwrap();
        assert_eq!(
            plan.overlay_file_count(),
            1,
            "the planned gate must miss the stale row by spelling too"
        );
    }

    /// The point paths take their fingerprints through their own constructors; the retarget
    /// must move those too, on both independent implementations.
    #[cfg(unix)]
    #[test]
    fn a_retargeted_link_at_equal_stat_is_reread_by_point_refreshes() {
        for use_manifest in [false, true] {
            let dir = tempdir().unwrap();
            let outside = tempdir().unwrap();
            let workspace = dir.path();
            let (first, second) = equal_stat_targets(outside.path());
            let alias = workspace.join("Alias.bsl");
            std::os::unix::fs::symlink(&first, &alias).unwrap();

            let store = Store::open(&workspace.join("search.db")).unwrap();
            let roots = single_root(workspace);
            let manifest: HashMap<FileKey, String> = HashMap::new();
            let mut cache = WorkspaceOverlayCache::default();
            cache.enable_watcher_mode();
            if use_manifest {
                cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, true).unwrap();
            } else {
                cache
                    .refresh(&store, &roots, None, 32, BaselineHashMode::RawFileBytes, true)
                    .unwrap();
            }
            assert_eq!(cache.snapshot().lexical_documents[0].symbol_name, "ПерваяЦель");

            fs::remove_file(&alias).unwrap();
            std::os::unix::fs::symlink(&second, &alias).unwrap();
            cache.mark_dirty_path(key("Alias.bsl"));
            if use_manifest {
                cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, false).unwrap();
            } else {
                cache
                    .refresh(&store, &roots, None, 32, BaselineHashMode::RawFileBytes, false)
                    .unwrap();
            }
            assert_eq!(
                cache.snapshot().lexical_documents[0].symbol_name,
                "ВтораяЦель",
                "manifest={use_manifest}: the point path must see the retarget"
            );
        }
    }

    /// A row from before the spelling column existed carries an empty `canonical` and must
    /// never pass the gate, even at matching `(len, mtime)`: the file is re-read and the
    /// re-save fills the spelling in — old rows heal, they are not trusted.
    #[test]
    fn an_empty_canonical_row_is_never_a_gate_hit() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let file = workspace.join("Solo.bsl");
        fs::write(&file, "Процедура Настоящая()\nКонецПроцедуры").unwrap();
        let manifest = HashMap::from([(key("Solo.bsl"), "manifest-differs".to_owned())]);
        let store = Store::open(&workspace.join("search.db")).unwrap();
        // A stale pre-column row claiming the file EQUALS the manifest, at the file's real
        // (len, mtime) — everything matches except the spelling, which is empty.
        let metadata = fs::metadata(&file).unwrap();
        let (secs, nanos) = super::mtime_to_secs_nanos(metadata.modified().ok()).unwrap();
        store
            .save_overlay_fingerprint_cache(
                "",
                &HashMap::from([(
                    key("Solo.bsl"),
                    crate::store::PersistedFingerprint {
                        file_size: metadata.len(),
                        file_mtime_secs: secs,
                        file_mtime_nanos: nanos,
                        content_fingerprint: "manifest-differs".to_owned(),
                        canonical: String::new(),
                    },
                )]),
            )
            .unwrap();

        let plan = WorkspaceOverlayCache::plan_full_refresh_from_manifest(
            &manifest,
            &single_root(workspace),
            &store,
            &HashMap::new(),
            None,
            &HashSet::new(),
        )
        .unwrap();
        assert_eq!(plan.overlay_file_count(), 1, "the empty-spelling row must not be trusted");
        let mut cache = WorkspaceOverlayCache::default();
        cache
            .publish_plan(
                plan,
                HashMap::new(),
                &super::PublicationBaseline {
                    dirty: HashMap::new(),
                    fence: u64::MAX,
                    unread: HashSet::new(),
                },
                None,
                &store,
            )
            .unwrap();
        let rows = store.load_overlay_fingerprint_cache("").unwrap_or(None).unwrap_or_default();
        assert!(
            !rows.get(&key("Solo.bsl")).map(|row| row.canonical.as_str()).unwrap_or("").is_empty(),
            "the published re-read heals the row with the real spelling"
        );
    }

    /// A fresh `.bsl`-spelled link to a NON-source target must not become an overlay entry on
    /// the point path: the walk drops such files (the roles of the two spellings disagree), and
    /// the point path must serve the same universe. Both independent implementations.
    #[cfg(unix)]
    #[test]
    fn a_link_to_a_non_source_target_is_not_indexed_by_point_refreshes() {
        for use_manifest in [false, true] {
            let dir = tempdir().unwrap();
            let outside = tempdir().unwrap();
            let workspace = dir.path();
            let target = outside.path().join("Target.txt");
            fs::write(&target, "Процедура ТолькоЧерезСсылку()\nКонецПроцедуры").unwrap();

            let store = Store::open(&workspace.join("search.db")).unwrap();
            let roots = single_root(workspace);
            let manifest: HashMap<FileKey, String> = HashMap::new();
            let mut cache = WorkspaceOverlayCache::default();
            cache.enable_watcher_mode();
            if use_manifest {
                cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, true).unwrap();
            } else {
                cache
                    .refresh(&store, &roots, None, 32, BaselineHashMode::RawFileBytes, true)
                    .unwrap();
            }

            std::os::unix::fs::symlink(&target, workspace.join("Alias.bsl")).unwrap();
            cache.mark_dirty_path(key("Alias.bsl"));
            if use_manifest {
                cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, false).unwrap();
            } else {
                cache
                    .refresh(&store, &roots, None, 32, BaselineHashMode::RawFileBytes, false)
                    .unwrap();
            }
            assert_eq!(
                cache.snapshot().lexical_documents.len(),
                0,
                "manifest={use_manifest}: a .txt target is not a source file"
            );
        }
    }

    /// Retargeting an indexed `.bsl` link onto a NON-source file is positive evidence the
    /// source is gone: the point refresh removes the entry instead of serving the foreign
    /// contents. Both independent implementations.
    #[cfg(unix)]
    #[test]
    fn a_retarget_onto_a_non_source_file_removes_the_point_entry() {
        for use_manifest in [false, true] {
            let dir = tempdir().unwrap();
            let outside = tempdir().unwrap();
            let workspace = dir.path();
            let source = outside.path().join("Настоящий.bsl");
            fs::write(&source, "Процедура Настоящая()\nКонецПроцедуры").unwrap();
            let foreign = outside.path().join("Чужой.txt");
            fs::write(&foreign, "Процедура Чужая()\nКонецПроцедуры").unwrap();
            let alias = workspace.join("Alias.bsl");
            std::os::unix::fs::symlink(&source, &alias).unwrap();

            let store = Store::open(&workspace.join("search.db")).unwrap();
            let roots = single_root(workspace);
            let manifest: HashMap<FileKey, String> = HashMap::new();
            let mut cache = WorkspaceOverlayCache::default();
            cache.enable_watcher_mode();
            if use_manifest {
                cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, true).unwrap();
            } else {
                cache
                    .refresh(&store, &roots, None, 32, BaselineHashMode::RawFileBytes, true)
                    .unwrap();
            }
            assert_eq!(cache.snapshot().lexical_documents[0].symbol_name, "Настоящая");

            fs::remove_file(&alias).unwrap();
            std::os::unix::fs::symlink(&foreign, &alias).unwrap();
            cache.mark_dirty_path(key("Alias.bsl"));
            if use_manifest {
                cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, false).unwrap();
            } else {
                cache
                    .refresh(&store, &roots, None, 32, BaselineHashMode::RawFileBytes, false)
                    .unwrap();
            }
            assert_eq!(
                cache.snapshot().lexical_documents.len(),
                0,
                "manifest={use_manifest}: the retargeted link no longer names a source file"
            );
        }
    }

    /// A registered root that is a symlink onto a vanished target: the link's inode exists, but
    /// the tree behind it does not — the full walk calls that incomplete coverage, so the point
    /// path must keep the entry and retry, exactly as for a renamed plain root.
    #[cfg(unix)]
    #[test]
    fn a_dangling_root_link_keeps_the_point_entry() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let target = workspace.join("target");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("A.bsl"), "Процедура Живая()\nКонецПроцедуры").unwrap();
        let configuration = workspace.join("cf");
        std::os::unix::fs::symlink(&target, &configuration).unwrap();
        let manifest = HashMap::from([(key("A.bsl"), "manifest-differs".to_owned())]);
        let (roots, _) = WorkspaceRoots::build(workspace, &configuration, &[]);
        let store = Store::open(&workspace.join("search.db")).unwrap();
        let mut cache = WorkspaceOverlayCache::default();
        cache.enable_watcher_mode();
        cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, true).unwrap();
        assert_eq!(cache.snapshot().lexical_documents.len(), 1);

        fs::rename(&target, workspace.join("target.saved")).unwrap();
        cache.mark_dirty_path(key("A.bsl"));
        let point = cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, false);
        fs::rename(workspace.join("target.saved"), &target).unwrap();
        point.unwrap();

        assert_eq!(
            cache.snapshot().lexical_documents.len(),
            1,
            "a root whose link target vanished proves nothing about the file"
        );
        assert!(cache.dirty_paths_snapshot().contains_key(&key("A.bsl")));
    }

    /// A source file replaced by a DIRECTORY spelled the same: the walk yields only regular
    /// files, so the point path must read the replacement as the source file's removal — not
    /// spin its retry budget on `read_to_string` (a FIFO there would even block it).
    #[test]
    fn a_file_replaced_by_a_directory_is_removed_by_the_point_refresh() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let file = workspace.join("A.bsl");
        fs::write(&file, "Процедура Живая()\nКонецПроцедуры").unwrap();
        let manifest: HashMap<FileKey, String> = HashMap::new();
        let roots = single_root(workspace);
        let store = Store::open(&workspace.join("search.db")).unwrap();
        let mut cache = WorkspaceOverlayCache::default();
        cache.enable_watcher_mode();
        cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, true).unwrap();
        assert_eq!(cache.snapshot().lexical_documents.len(), 1);

        fs::remove_file(&file).unwrap();
        fs::create_dir(&file).unwrap();
        cache.mark_dirty_path(key("A.bsl"));
        cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, false).unwrap();

        assert_eq!(
            cache.snapshot().lexical_documents.len(),
            0,
            "a directory is not a source file; the walk would not yield it"
        );
        assert!(
            !cache.dirty_paths_snapshot().contains_key(&key("A.bsl")),
            "a settled removal consumes its mark instead of burning the retry budget"
        );
    }

    /// When the stale-row filter empties the plan's map entirely, the rows it filtered must
    /// still be retracted: a skipped save would keep exactly the row the filter rejected.
    #[test]
    fn a_filtered_out_last_fingerprint_row_is_still_retracted() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let file = workspace.join("A.bsl");
        let original = "Процедура Первая()\nКонецПроцедуры";
        fs::write(&file, original).unwrap();
        let manifest =
            HashMap::from([(key("A.bsl"), super::fingerprint_content(original, "A.bsl"))]);
        let roots = single_root(workspace);
        let store = Store::open(&workspace.join("search.db")).unwrap();
        let mut cache = WorkspaceOverlayCache::default();
        cache.enable_watcher_mode();
        cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, true).unwrap();

        let dirty_before = cache.publication_baseline();
        let plan = WorkspaceOverlayCache::plan_full_refresh_from_manifest(
            &manifest,
            &roots,
            &store,
            &HashMap::new(),
            None,
            &HashSet::new(),
        )
        .unwrap();
        // Mid-embed the file changes at the same (len, mtime) and gets marked: the plan's only
        // row is now stale, and the publish must not leave it behind just because the filtered
        // map came out empty.
        let mtime = fs::metadata(&file).unwrap().modified().unwrap();
        fs::write(&file, "Процедура Вторая()\nКонецПроцедуры").unwrap();
        fs::File::options().write(true).open(&file).unwrap().set_modified(mtime).unwrap();
        cache.mark_dirty_path(key("A.bsl"));
        cache.publish_plan(plan, HashMap::new(), &dirty_before, None, &store).unwrap();

        assert!(
            !store
                .load_overlay_fingerprint_cache("")
                .unwrap_or(None)
                .unwrap_or_default()
                .contains_key(&key("A.bsl")),
            "the filtered row must not survive to pass the gate after a restart"
        );
    }

    /// A SUCCESSFUL point re-read makes the old fingerprint row stale just as surely as a
    /// failed one: the row says "verified equal to the manifest", the re-read just proved
    /// otherwise, and at unchanged `(len, mtime, canonical)` the surviving row would suppress
    /// the published edit after a restart.
    #[test]
    fn a_successful_point_reread_retracts_the_old_fingerprint_row() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let file = workspace.join("A.bsl");
        let original = "Процедура Первая()\nКонецПроцедуры";
        fs::write(&file, original).unwrap();
        let manifest =
            HashMap::from([(key("A.bsl"), super::fingerprint_content(original, "A.bsl"))]);
        let roots = single_root(workspace);
        let store = Store::open(&workspace.join("search.db")).unwrap();
        let mut cache = WorkspaceOverlayCache::default();
        cache.enable_watcher_mode();
        cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, true).unwrap();
        assert_eq!(
            store.load_overlay_fingerprint_cache("").unwrap_or(None).unwrap_or_default().len(),
            1
        );

        let mtime = fs::metadata(&file).unwrap().modified().unwrap();
        fs::write(&file, "Процедура Вторая()\nКонецПроцедуры").unwrap();
        fs::File::options().write(true).open(&file).unwrap().set_modified(mtime).unwrap();
        cache.mark_dirty_path(key("A.bsl"));
        cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, false).unwrap();
        assert_eq!(
            cache.snapshot().lexical_documents[0].symbol_name,
            "Вторая",
            "the point re-read published the edit in-process"
        );

        // "Restart": a fresh plan must not let the stale row suppress the published edit.
        let plan = WorkspaceOverlayCache::plan_full_refresh_from_manifest(
            &manifest,
            &roots,
            &store,
            &HashMap::new(),
            None,
            &HashSet::new(),
        )
        .unwrap();
        assert_eq!(plan.overlay_file_count(), 1, "the edit survives the restart");
    }

    /// A failed row retraction must not pass for a done one: the dirty mark stays (with its
    /// budget) so a later pass retries the retraction, instead of the stale row silently
    /// outliving the process.
    #[test]
    fn a_failed_row_retraction_keeps_the_dirty_mark() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let file = workspace.join("A.bsl");
        fs::write(&file, "Процедура Живая()\nКонецПроцедуры").unwrap();
        let manifest = HashMap::from([(
            key("A.bsl"),
            super::fingerprint_content("Процедура Живая()\nКонецПроцедуры", "A.bsl"),
        )]);
        let roots = single_root(workspace);
        let db_path = workspace.join("search.db");
        let store = Store::open(&db_path).unwrap();
        let mut cache = WorkspaceOverlayCache::default();
        cache.enable_watcher_mode();
        cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, true).unwrap();
        assert_eq!(
            store.load_overlay_fingerprint_cache("").unwrap_or(None).unwrap_or_default().len(),
            1
        );

        // A second connection injects a trigger that makes every row deletion fail.
        let saboteur = rusqlite::Connection::open(&db_path).unwrap();
        saboteur
            .execute_batch(
                "CREATE TRIGGER deny_fp_delete BEFORE DELETE ON overlay_fingerprint_cache \
                 BEGIN SELECT RAISE(FAIL, 'deny'); END;",
            )
            .unwrap();
        fs::remove_file(&file).unwrap();
        cache.mark_dirty_path(key("A.bsl"));
        cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, false).unwrap();
        assert!(
            cache.dirty_paths_snapshot().contains_key(&key("A.bsl")),
            "an unretracted row keeps its key marked for the retry"
        );

        // With the sabotage removed, the retried pass settles the removal for real.
        saboteur.execute_batch("DROP TRIGGER deny_fp_delete;").unwrap();
        cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, false).unwrap();
        assert!(
            !store
                .load_overlay_fingerprint_cache("")
                .unwrap_or(None)
                .unwrap_or_default()
                .contains_key(&key("A.bsl")),
            "the retried retraction lands"
        );
        assert!(!cache.dirty_paths_snapshot().contains_key(&key("A.bsl")));
    }

    /// The retraction obligation left by a FAILED retract must be honoured by the retried
    /// pass: the mark that survived the failure brings the key back, the pass re-reads it
    /// (marked keys have no fast path) and settles the retraction before consuming the mark.
    #[test]
    fn a_failed_retraction_is_retried_on_the_next_pass() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let file = workspace.join("A.bsl");
        let original = "Процедура Первая()\nКонецПроцедуры";
        fs::write(&file, original).unwrap();
        let manifest =
            HashMap::from([(key("A.bsl"), super::fingerprint_content(original, "A.bsl"))]);
        let roots = single_root(workspace);
        let db_path = workspace.join("search.db");
        let store = Store::open(&db_path).unwrap();
        let mut cache = WorkspaceOverlayCache::default();
        cache.enable_watcher_mode();
        cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, true).unwrap();

        // The edit at unchanged (len, mtime): the re-read succeeds and publishes, but the
        // sabotaged retraction fails, leaving the mark as the only trace of the stale row.
        let mtime = fs::metadata(&file).unwrap().modified().unwrap();
        fs::write(&file, "Процедура Вторая()\nКонецПроцедуры").unwrap();
        fs::File::options().write(true).open(&file).unwrap().set_modified(mtime).unwrap();
        let saboteur = rusqlite::Connection::open(&db_path).unwrap();
        saboteur
            .execute_batch(
                "CREATE TRIGGER deny_fp_delete BEFORE DELETE ON overlay_fingerprint_cache \
                 BEGIN SELECT RAISE(FAIL, 'deny'); END;",
            )
            .unwrap();
        cache.mark_dirty_path(key("A.bsl"));
        cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, false).unwrap();
        assert_eq!(cache.snapshot().lexical_documents[0].symbol_name, "Вторая");
        assert!(cache.dirty_paths_snapshot().contains_key(&key("A.bsl")));

        // The retried pass meets the freshly-published entry (equal fingerprint, no read) and
        // must STILL settle the retraction before consuming the mark.
        saboteur.execute_batch("DROP TRIGGER deny_fp_delete;").unwrap();
        cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, false).unwrap();
        assert!(
            !store
                .load_overlay_fingerprint_cache("")
                .unwrap_or(None)
                .unwrap_or_default()
                .contains_key(&key("A.bsl")),
            "the obligation survives into the fast path"
        );
        let plan = WorkspaceOverlayCache::plan_full_refresh_from_manifest(
            &manifest,
            &roots,
            &store,
            &HashMap::new(),
            None,
            &HashSet::new(),
        )
        .unwrap();
        assert_eq!(plan.overlay_file_count(), 1, "the edit survives the restart");
    }

    /// A dirty mark is positive evidence the fingerprint must not be trusted: an edit at
    /// unchanged `(len, mtime, canonical)` with a watcher mark must be re-read by the POINT
    /// paths instead of being consumed through the equal-fingerprint fast path.
    #[test]
    fn a_marked_key_at_equal_stat_is_reread_by_point_refreshes() {
        for use_manifest in [false, true] {
            let dir = tempdir().unwrap();
            let workspace = dir.path();
            let file = workspace.join("A.bsl");
            let original = "Процедура Первая()\nКонецПроцедуры";
            fs::write(&file, original).unwrap();
            let manifest = HashMap::from([(key("A.bsl"), "manifest-differs".to_owned())]);
            let baseline = HashMap::from([(key("A.bsl"), b"baseline-differs".to_vec())]);
            let roots = single_root(workspace);
            let store = Store::open(&workspace.join("search.db")).unwrap();
            let mut cache = WorkspaceOverlayCache::default();
            cache.enable_watcher_mode();
            if use_manifest {
                cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, true).unwrap();
            } else {
                let gone = key("A.bsl");
                store.upsert_file(&gone.root_id, &gone.path, &baseline[&gone], "code").unwrap();
                cache
                    .refresh(&store, &roots, None, 32, BaselineHashMode::RawFileBytes, true)
                    .unwrap();
            }
            assert_eq!(cache.snapshot().lexical_documents[0].symbol_name, "Первая");

            let mtime = fs::metadata(&file).unwrap().modified().unwrap();
            fs::write(&file, "Процедура Вторая()\nКонецПроцедуры").unwrap();
            fs::File::options().write(true).open(&file).unwrap().set_modified(mtime).unwrap();
            cache.mark_dirty_path(key("A.bsl"));
            if use_manifest {
                cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, false).unwrap();
            } else {
                cache
                    .refresh(&store, &roots, None, 32, BaselineHashMode::RawFileBytes, false)
                    .unwrap();
            }
            assert_eq!(
                cache.snapshot().lexical_documents[0].symbol_name,
                "Вторая",
                "manifest={use_manifest}: the mark outranks the equal fingerprint"
            );
        }
    }

    /// The same evidence must outrank the equal-fingerprint entry gate of the IN-PLACE full
    /// refreshes — the raw entry check and the manifest persisted-row gate alike.
    #[test]
    fn a_marked_key_at_equal_stat_is_reread_by_full_refreshes() {
        // Raw arm: the equal in-memory entry must not shadow the marked edit.
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let file = workspace.join("A.bsl");
        let original = "Процедура Первая()\nКонецПроцедуры";
        fs::write(&file, original).unwrap();
        let baseline = HashMap::from([(key("A.bsl"), b"baseline-differs".to_vec())]);
        let roots = single_root(workspace);
        let mut cache = WorkspaceOverlayCache::default();
        cache
            .full_refresh(
                &baseline,
                &roots,
                None,
                32,
                BaselineHashMode::RawFileBytes,
                &Store::in_memory().unwrap(),
            )
            .unwrap();
        assert_eq!(cache.snapshot().lexical_documents[0].symbol_name, "Первая");
        let mtime = fs::metadata(&file).unwrap().modified().unwrap();
        fs::write(&file, "Процедура Вторая()\nКонецПроцедуры").unwrap();
        fs::File::options().write(true).open(&file).unwrap().set_modified(mtime).unwrap();
        cache.mark_dirty_path(key("A.bsl"));
        cache
            .full_refresh(
                &baseline,
                &roots,
                None,
                32,
                BaselineHashMode::RawFileBytes,
                &Store::in_memory().unwrap(),
            )
            .unwrap();
        assert_eq!(
            cache.snapshot().lexical_documents[0].symbol_name,
            "Вторая",
            "raw: the mark outranks the equal entry fingerprint"
        );

        // Manifest arm: the persisted-row gate must not shadow the marked edit either.
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let file = workspace.join("A.bsl");
        fs::write(&file, original).unwrap();
        let manifest =
            HashMap::from([(key("A.bsl"), super::fingerprint_content(original, "A.bsl"))]);
        let roots = single_root(workspace);
        let store = Store::open(&workspace.join("search.db")).unwrap();
        let mut cache = WorkspaceOverlayCache::default();
        cache.full_refresh_from_manifest(&manifest, &roots, None, 32, &store).unwrap();
        assert_eq!(cache.snapshot().lexical_documents.len(), 0, "baseline-equal at first");
        let mtime = fs::metadata(&file).unwrap().modified().unwrap();
        fs::write(&file, "Процедура Вторая()\nКонецПроцедуры").unwrap();
        fs::File::options().write(true).open(&file).unwrap().set_modified(mtime).unwrap();
        cache.mark_dirty_path(key("A.bsl"));
        cache.full_refresh_from_manifest(&manifest, &roots, None, 32, &store).unwrap();
        assert_eq!(
            cache.snapshot().lexical_documents.len(),
            1,
            "manifest: the mark outranks the persisted-row gate"
        );
        assert_eq!(cache.snapshot().lexical_documents[0].symbol_name, "Вторая");
    }

    /// The PLANNED path cannot read the mark set (it plans off-lock), so its publish must not
    /// CONSUME a live mark for a key the plan skipped by the persisted-row gate: the mark and
    /// the retracted row stay, and the next point pass delivers the edit.
    #[test]
    fn a_published_plan_leaves_gate_skipped_marked_keys_dirty() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let file = workspace.join("A.bsl");
        let original = "Процедура Первая()\nКонецПроцедуры";
        fs::write(&file, original).unwrap();
        let manifest =
            HashMap::from([(key("A.bsl"), super::fingerprint_content(original, "A.bsl"))]);
        let roots = single_root(workspace);
        let store = Store::open(&workspace.join("search.db")).unwrap();
        let mut cache = WorkspaceOverlayCache::default();
        cache.enable_watcher_mode();
        cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, true).unwrap();

        // The edit and its mark land BEFORE the plan is built: the plan's gate skips the key
        // by the stale row, and only the surviving mark can save the edit.
        let mtime = fs::metadata(&file).unwrap().modified().unwrap();
        fs::write(&file, "Процедура Вторая()\nКонецПроцедуры").unwrap();
        fs::File::options().write(true).open(&file).unwrap().set_modified(mtime).unwrap();
        cache.mark_dirty_path(key("A.bsl"));
        let dirty_before = cache.publication_baseline();
        let plan = WorkspaceOverlayCache::plan_full_refresh_from_manifest(
            &manifest,
            &roots,
            &store,
            &HashMap::new(),
            None,
            &HashSet::new(),
        )
        .unwrap();
        cache.publish_plan(plan, HashMap::new(), &dirty_before, None, &store).unwrap();

        assert!(
            cache.dirty_paths_snapshot().contains_key(&key("A.bsl")),
            "a gate-skipped marked key must not be consumed by the publish"
        );
        cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, false).unwrap();
        assert_eq!(
            cache.snapshot().lexical_documents[0].symbol_name,
            "Вторая",
            "the surviving mark delivers the edit on the next point pass"
        );
    }

    /// A proven deletion must HIDE the baseline copy, not unhide it: for a remote baseline the
    /// hidden set is the only filter, and dropping the key from it would resurrect the deleted
    /// file as a baseline hit the moment the local entry is gone.
    #[test]
    fn a_proven_removal_hides_the_baseline_copy() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let file = workspace.join("A.bsl");
        fs::write(&file, "Процедура Локальная()\nКонецПроцедуры").unwrap();
        let manifest = HashMap::from([(key("A.bsl"), "manifest-differs".to_owned())]);
        let roots = single_root(workspace);
        let store = Store::open(&workspace.join("search.db")).unwrap();
        let mut cache = WorkspaceOverlayCache::default();
        cache.enable_watcher_mode();
        cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, true).unwrap();
        assert!(cache.snapshot().hidden_paths.contains(&key("A.bsl")));

        // The root vanishes with the file; the removal channel is the only witness.
        fs::remove_file(&file).unwrap();
        cache.remove_known_deleted(&key("A.bsl"), true);
        cache.mark_dirty_path(key("A.bsl"));
        assert!(
            cache.snapshot().hidden_paths.contains(&key("A.bsl")),
            "the deleted file's baseline copy must stay hidden"
        );
    }

    /// A failed replace-save of the fingerprint table must not pass for a done one: the marks
    /// of the keys this pass re-read stay alive (their old rows survived), and the pending
    /// flag records that only a future complete pass settles the debt.
    #[test]
    fn a_failed_fingerprint_save_keeps_the_marks_alive() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let file = workspace.join("A.bsl");
        let original = "Процедура Первая()\nКонецПроцедуры";
        fs::write(&file, original).unwrap();
        let manifest =
            HashMap::from([(key("A.bsl"), super::fingerprint_content(original, "A.bsl"))]);
        let roots = single_root(workspace);
        let db_path = workspace.join("search.db");
        let store = Store::open(&db_path).unwrap();
        let mut cache = WorkspaceOverlayCache::default();
        cache.enable_watcher_mode();
        cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, true).unwrap();

        let mtime = fs::metadata(&file).unwrap().modified().unwrap();
        fs::write(&file, "Процедура Вторая()\nКонецПроцедуры").unwrap();
        fs::File::options().write(true).open(&file).unwrap().set_modified(mtime).unwrap();
        let saboteur = rusqlite::Connection::open(&db_path).unwrap();
        saboteur
            .execute_batch(
                "CREATE TRIGGER deny_fp_delete BEFORE DELETE ON overlay_fingerprint_cache \
                 BEGIN SELECT RAISE(FAIL, 'deny'); END;",
            )
            .unwrap();
        cache.mark_dirty_path(key("A.bsl"));
        cache.full_refresh_from_manifest(&manifest, &roots, None, 32, &store).unwrap();
        assert_eq!(
            cache.snapshot().lexical_documents[0].symbol_name,
            "Вторая",
            "the re-read published in-process"
        );
        assert!(
            cache.dirty_paths_snapshot().contains_key(&key("A.bsl")),
            "the unsaved row keeps its key marked"
        );

        saboteur.execute_batch("DROP TRIGGER deny_fp_delete;").unwrap();
        cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, false).unwrap();
        let plan = WorkspaceOverlayCache::plan_full_refresh_from_manifest(
            &manifest,
            &roots,
            &store,
            &HashMap::new(),
            None,
            &HashSet::new(),
        )
        .unwrap();
        assert_eq!(plan.overlay_file_count(), 1, "the edit survives the restart");
    }

    /// The baseline manifest only names which fingerprint cache and baseline identity the
    /// overlay belongs to. An unreadable manifest leaves that identity unknown; it must not
    /// abort the refresh that keeps the overlay current.
    #[test]
    fn an_unreadable_manifest_leaves_identity_unknown_and_refreshes() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let original = "Процедура Первая()\nКонецПроцедуры";
        fs::write(workspace.join("A.bsl"), original).unwrap();
        let manifest =
            HashMap::from([(key("A.bsl"), super::fingerprint_content(original, "A.bsl"))]);
        let roots = single_root(workspace);
        let db_path = workspace.join("search.db");
        let store = Store::open(&db_path).unwrap();
        let mut cache = WorkspaceOverlayCache::default();
        cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, true).unwrap();
        fs::write(workspace.join("A.bsl"), "Процедура Вторая()\nКонецПроцедуры").unwrap();
        let saboteur = rusqlite::Connection::open(&db_path).unwrap();
        saboteur.execute_batch("ALTER TABLE baseline_manifest RENAME TO hidden_manifest;").unwrap();
        assert!(store.load_baseline_manifest().is_err(), "the fixture must break the read");

        cache.mark_dirty_path(key("A.bsl"));
        cache.full_refresh_from_manifest(&manifest, &roots, None, 32, &store).unwrap();
        assert_eq!(cache.snapshot().lexical_documents[0].symbol_name, "Вторая");
        assert!(cache.baseline_identity.is_none());
        let plan = WorkspaceOverlayCache::plan_full_refresh_from_manifest(
            &manifest,
            &roots,
            &store,
            &HashMap::new(),
            None,
            &HashSet::new(),
        )
        .unwrap();
        assert_eq!(plan.overlay_file_count(), 1);
    }

    /// Failed row retractions are a STORE fault, not a per-path one: they must not eat the
    /// bounded stat/read budget, or three SQLite hiccups would drop the mark and let the
    /// stale row outlive every retry.
    #[test]
    fn failed_retractions_do_not_charge_the_dirty_budget() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let file = workspace.join("A.bsl");
        let original = "Процедура Первая()\nКонецПроцедуры";
        fs::write(&file, original).unwrap();
        let manifest =
            HashMap::from([(key("A.bsl"), super::fingerprint_content(original, "A.bsl"))]);
        let roots = single_root(workspace);
        let db_path = workspace.join("search.db");
        let store = Store::open(&db_path).unwrap();
        let mut cache = WorkspaceOverlayCache::default();
        cache.enable_watcher_mode();
        cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, true).unwrap();

        let mtime = fs::metadata(&file).unwrap().modified().unwrap();
        fs::write(&file, "Процедура Вторая()\nКонецПроцедуры").unwrap();
        fs::File::options().write(true).open(&file).unwrap().set_modified(mtime).unwrap();
        let saboteur = rusqlite::Connection::open(&db_path).unwrap();
        saboteur
            .execute_batch(
                "CREATE TRIGGER deny_fp_delete BEFORE DELETE ON overlay_fingerprint_cache \
                 BEGIN SELECT RAISE(FAIL, 'deny'); END;",
            )
            .unwrap();
        cache.mark_dirty_path(key("A.bsl"));
        for _ in 0..(MAX_DIRTY_REFRESH_FAILURES + 1) {
            cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, false).unwrap();
        }
        assert!(
            cache.dirty_paths_snapshot().contains_key(&key("A.bsl")),
            "store faults must not exhaust the per-path budget"
        );

        saboteur.execute_batch("DROP TRIGGER deny_fp_delete;").unwrap();
        cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, false).unwrap();
        let plan = WorkspaceOverlayCache::plan_full_refresh_from_manifest(
            &manifest,
            &roots,
            &store,
            &HashMap::new(),
            None,
            &HashSet::new(),
        )
        .unwrap();
        assert_eq!(plan.overlay_file_count(), 1, "the edit survives once the store recovers");
    }

    /// `publish_plan` reports how many marked keys its gate skipped unread — the warmup
    /// outcome needs the number, because a plan that is EMPTY only because its gate trusted a
    /// stale row proves nothing about the working tree.
    #[test]
    fn publish_plan_reports_gate_deferred_marked_keys() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let file = workspace.join("A.bsl");
        let original = "Процедура Первая()\nКонецПроцедуры";
        fs::write(&file, original).unwrap();
        let manifest =
            HashMap::from([(key("A.bsl"), super::fingerprint_content(original, "A.bsl"))]);
        let roots = single_root(workspace);
        let store = Store::open(&workspace.join("search.db")).unwrap();
        let mut cache = WorkspaceOverlayCache::default();
        cache.enable_watcher_mode();
        cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, true).unwrap();

        let mtime = fs::metadata(&file).unwrap().modified().unwrap();
        fs::write(&file, "Процедура Вторая()\nКонецПроцедуры").unwrap();
        fs::File::options().write(true).open(&file).unwrap().set_modified(mtime).unwrap();
        cache.mark_dirty_path(key("A.bsl"));
        let dirty_before = cache.publication_baseline();
        let plan = WorkspaceOverlayCache::plan_full_refresh_from_manifest(
            &manifest,
            &roots,
            &store,
            &HashMap::new(),
            None,
            &HashSet::new(),
        )
        .unwrap();
        let deferred =
            cache.publish_plan(plan, HashMap::new(), &dirty_before, None, &store).unwrap();
        assert_eq!(
            deferred,
            super::PublishOutcome::Applied {
                gate_deferred: 1,
                persist_ok: true,
                overlay_files: 0,
                deleted_files: 0,
                unread_keys: 0
            },
            "one marked key was skipped unread by the gate"
        );

        // A publish with nothing deferred reports zero.
        cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, false).unwrap();
        let plan = WorkspaceOverlayCache::plan_full_refresh_from_manifest(
            &manifest,
            &roots,
            &store,
            &HashMap::new(),
            None,
            &HashSet::new(),
        )
        .unwrap();
        let deferred = cache
            .publish_plan(
                plan,
                HashMap::new(),
                &super::PublicationBaseline {
                    dirty: HashMap::new(),
                    fence: u64::MAX,
                    unread: HashSet::new(),
                },
                None,
                &store,
            )
            .unwrap();
        assert_eq!(
            deferred,
            super::PublishOutcome::Applied {
                gate_deferred: 0,
                persist_ok: true,
                overlay_files: 1,
                deleted_files: 0,
                unread_keys: 0
            }
        );
    }

    /// The overlay must not walk the tree itself: the walk policy (which links
    /// are followed, how errors are classified) lives in `project-model`, and a
    /// private traversal here would quietly diverge from it again.
    #[test]
    fn the_overlay_does_not_carry_its_own_tree_walk() {
        let source = include_str!("workspace_overlay.rs");
        let needle: String = ["walk", "dir"].concat();
        assert!(
            !source.to_lowercase().contains(&needle),
            "workspace_overlay.rs must scan through project_model::SourceSet::scan"
        );
    }

    /// A combined fault — the file read fails AND the row retraction is denied — is a STORE
    /// fault first: the mark must survive without the file budget being charged, or three
    /// such passes drop the mark while the stale row still needs its retry.
    #[test]
    fn a_combined_file_and_store_fault_does_not_charge_the_budget() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let file = workspace.join("A.bsl");
        let original = "Процедура Первая()\nКонецПроцедуры";
        fs::write(&file, original).unwrap();
        let manifest =
            HashMap::from([(key("A.bsl"), super::fingerprint_content(original, "A.bsl"))]);
        let roots = single_root(workspace);
        let db_path = workspace.join("search.db");
        let store = Store::open(&db_path).unwrap();
        let mut cache = WorkspaceOverlayCache::default();
        cache.enable_watcher_mode();
        cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, true).unwrap();

        fs::write(&file, [0xff, 0xfe]).unwrap();
        let saboteur = rusqlite::Connection::open(&db_path).unwrap();
        saboteur
            .execute_batch(
                "CREATE TRIGGER deny_fp_delete BEFORE DELETE ON overlay_fingerprint_cache \
                 BEGIN SELECT RAISE(FAIL, 'deny'); END;",
            )
            .unwrap();
        cache.mark_dirty_path(key("A.bsl"));
        for _ in 0..(MAX_DIRTY_REFRESH_FAILURES + 1) {
            cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, false).unwrap();
        }
        assert!(
            cache.dirty_paths_snapshot().contains_key(&key("A.bsl")),
            "a store fault outweighs the read fault: the mark survives unbudgeted"
        );
    }

    /// The same combined fault under a DEAD root: the stat fails, the root is unreachable, and
    /// the retraction is denied. The store fault must still outweigh the file budget — the
    /// unreadable-root branch is the last one where it used to charge.
    #[test]
    fn a_stat_fault_under_a_dead_root_with_a_store_fault_keeps_the_mark() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let configuration = workspace.join("cf");
        fs::create_dir(&configuration).unwrap();
        fs::write(configuration.join("A.bsl"), "Процедура Живая()\nКонецПроцедуры").unwrap();
        let manifest = HashMap::from([(key("A.bsl"), "manifest-differs".to_owned())]);
        let (roots, _) = WorkspaceRoots::build(workspace, &configuration, &[]);
        let db_path = workspace.join("search.db");
        let store = Store::open(&db_path).unwrap();
        let mut cache = WorkspaceOverlayCache::default();
        cache.enable_watcher_mode();
        cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, true).unwrap();

        fs::rename(&configuration, workspace.join("cf.saved")).unwrap();
        let saboteur = rusqlite::Connection::open(&db_path).unwrap();
        saboteur
            .execute_batch(
                "CREATE TRIGGER deny_fp_delete BEFORE DELETE ON overlay_fingerprint_cache \
                 BEGIN SELECT RAISE(FAIL, 'deny'); END;",
            )
            .unwrap();
        cache.mark_dirty_path(key("A.bsl"));
        for _ in 0..(MAX_DIRTY_REFRESH_FAILURES + 1) {
            cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, false).unwrap();
        }
        assert!(
            cache.dirty_paths_snapshot().contains_key(&key("A.bsl")),
            "the denied retraction outweighs the dead-root stat fault"
        );
    }

    /// A build failure (the embedder is down) is an infrastructure fault of ONE key: its mark
    /// survives unbudgeted, and the rest of the batch still settles instead of dying on `?`.
    #[test]
    fn a_build_fault_keeps_the_mark_and_settles_the_rest() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let changed = workspace.join("A.bsl");
        let reverted = workspace.join("B.bsl");
        let b_text = "Процедура Вторая()\nКонецПроцедуры";
        fs::write(&changed, "Процедура Первая()\nКонецПроцедуры").unwrap();
        fs::write(&reverted, b_text).unwrap();
        let manifest = HashMap::from([
            (key("A.bsl"), "manifest-differs".to_owned()),
            (key("B.bsl"), super::fingerprint_content(b_text, "B.bsl")),
        ]);
        let roots = single_root(workspace);
        let store = Store::open(&workspace.join("search.db")).unwrap();
        let mut cache = WorkspaceOverlayCache::default();
        cache.enable_watcher_mode();
        cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, true).unwrap();

        let embedder = super::Embedder::new(crate::EmbedderConfig {
            base_url: "http://127.0.0.1:1".to_owned(),
            model: "test-model".to_owned(),
            dim: Some(3),
            api_key: None,
            provider: None,
            ..Default::default()
        });
        let result = cache.refresh_dirty_paths_from_manifest(
            vec![key("A.bsl"), key("B.bsl")],
            super::ManifestBaseline { fingerprints: &manifest, store: &store },
            &roots,
            Some(&embedder),
            32,
            &HashMap::new(),
        );
        assert_eq!(
            result.unwrap_err().embedding_failure().unwrap().code,
            crate::EmbeddingFailureCode::EmbeddingTransportError,
            "the settled batch must still report its embedding failure",
        );
        assert!(
            cache.dirty_paths_snapshot().contains_key(&key("A.bsl")),
            "the build-faulted key stays marked for the retry"
        );
        assert!(
            !cache.dirty_paths_snapshot().contains_key(&key("B.bsl")),
            "the baseline-equal neighbour settles despite the fault"
        );

        for _ in 0..(MAX_DIRTY_REFRESH_FAILURES + 1) {
            cache
                .refresh_dirty_paths_from_manifest(
                    cache.dirty_paths_snapshot().keys().cloned().collect(),
                    super::ManifestBaseline { fingerprints: &manifest, store: &store },
                    &roots,
                    Some(&embedder),
                    32,
                    &HashMap::new(),
                )
                .unwrap_err();
        }
        assert!(
            cache.dirty_paths_snapshot().contains_key(&key("A.bsl")),
            "an infrastructure fault never eats the file budget"
        );
    }

    /// A deletion proven by the caller does NOT get to hide the baseline copy of a key that has
    /// no baseline copy: with a dead root the parasitic hiding would be unremovable and the
    /// deleted-files stat inflated forever.
    #[test]
    fn a_delete_outside_the_baseline_hides_nothing() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let configuration = workspace.join("cf");
        fs::create_dir(&configuration).unwrap();
        fs::write(configuration.join("A.bsl"), "Процедура Локальная()\nКонецПроцедуры").unwrap();
        let manifest: HashMap<FileKey, String> = HashMap::new();
        let (roots, _) = WorkspaceRoots::build(workspace, &configuration, &[]);
        let store = Store::open(&workspace.join("search.db")).unwrap();
        let mut cache = WorkspaceOverlayCache::default();
        cache.enable_watcher_mode();
        cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, true).unwrap();
        assert_eq!(cache.stats().overlay_files, 1, "the local-only file is an overlay entry");

        fs::rename(&configuration, workspace.join("cf.saved")).unwrap();
        cache.remove_known_deleted(&key("A.bsl"), false);
        cache.mark_dirty_path(key("A.bsl"));
        for _ in 0..(MAX_DIRTY_REFRESH_FAILURES + 1) {
            cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, false).unwrap();
        }
        assert_eq!(
            cache.stats().deleted_files,
            0,
            "a key outside the baseline has nothing to hide, dead root or not"
        );
    }

    /// Phase C surfaces Store failure and leaves the entire staged map unpublished, including
    /// the processed mark that tells the next externally-triggered pass not to trust the row.
    #[test]
    fn a_failed_planned_save_keeps_the_marks_alive() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let file = workspace.join("A.bsl");
        let original = "Процедура Первая()\nКонецПроцедуры";
        fs::write(&file, original).unwrap();
        let manifest =
            HashMap::from([(key("A.bsl"), super::fingerprint_content(original, "A.bsl"))]);
        let roots = single_root(workspace);
        let db_path = workspace.join("search.db");
        let store = Store::open(&db_path).unwrap();
        let mut cache = WorkspaceOverlayCache::default();
        cache.enable_watcher_mode();
        cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, true).unwrap();

        fs::write(&file, "Процедура Вторая()\nКонецПроцедуры").unwrap();
        cache.mark_dirty_path(key("A.bsl"));
        let dirty_before = cache.publication_baseline();
        let plan = WorkspaceOverlayCache::plan_full_refresh_from_manifest(
            &manifest,
            &roots,
            &store,
            &HashMap::new(),
            None,
            &HashSet::new(),
        )
        .unwrap();
        assert_eq!(plan.overlay_file_count(), 1, "the edit is read by the plan");

        let saboteur = rusqlite::Connection::open(&db_path).unwrap();
        saboteur
            .execute_batch(
                "CREATE TRIGGER deny_fp_delete BEFORE DELETE ON overlay_fingerprint_cache \
                 BEGIN SELECT RAISE(FAIL, 'deny'); END;",
            )
            .unwrap();
        assert!(cache.publish_plan(plan, HashMap::new(), &dirty_before, None, &store).is_err());
        assert!(
            cache.dirty_paths_snapshot().contains_key(&key("A.bsl")),
            "a failed persist must not consume the processed mark"
        );
        assert!(!cache.needs_full_rescan(), "the failed staged map was never published");
    }

    /// A failed persist never charges the read budget: with prior failures at MAX-1 and both
    /// the file AND the store refusing, the mark must survive every in-place full pass.
    #[test]
    fn a_failed_persist_never_charges_the_read_budget() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let file = workspace.join("A.bsl");
        let original = "Процедура Первая()\nКонецПроцедуры";
        fs::write(&file, original).unwrap();
        let manifest =
            HashMap::from([(key("A.bsl"), super::fingerprint_content(original, "A.bsl"))]);
        let roots = single_root(workspace);
        let db_path = workspace.join("search.db");
        let store = Store::open(&db_path).unwrap();
        let mut cache = WorkspaceOverlayCache::default();
        cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, true).unwrap();

        fs::write(&file, [0xff, 0xfe]).unwrap();
        let saboteur = rusqlite::Connection::open(&db_path).unwrap();
        saboteur
            .execute_batch(
                "CREATE TRIGGER deny_fp_delete BEFORE DELETE ON overlay_fingerprint_cache \
                 BEGIN SELECT RAISE(FAIL, 'deny'); END;",
            )
            .unwrap();
        cache.mark_dirty_path(key("A.bsl"));
        for _ in 0..(MAX_DIRTY_REFRESH_FAILURES + 1) {
            cache.full_refresh_from_manifest(&manifest, &roots, None, 32, &store).unwrap();
        }
        assert!(
            cache.dirty_paths_snapshot().contains_key(&key("A.bsl")),
            "the full tail never charges the point budget"
        );
        assert_eq!(cache.unread_keys_count(), 1, "the unread file is a durable signal");
    }

    /// A build fault inside a FULL pass still reaches the publication tail, and the faulted
    /// key's prior entry AND hiding survive the whole-replace — otherwise the baseline and the
    /// stale overlay version would serve at once.
    #[test]
    fn a_full_pass_build_fault_still_reaches_the_tail() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let unreadable = workspace.join("A.bsl");
        let faulted = workspace.join("B.bsl");
        fs::write(&unreadable, "Процедура Первая()\nКонецПроцедуры").unwrap();
        fs::write(&faulted, "Процедура Вторая()\nКонецПроцедуры").unwrap();
        let manifest = HashMap::from([
            (key("A.bsl"), "manifest-differs-a".to_owned()),
            (key("B.bsl"), "manifest-differs-b".to_owned()),
        ]);
        let roots = single_root(workspace);
        let store = Store::open(&workspace.join("search.db")).unwrap();
        let mut cache = WorkspaceOverlayCache::default();
        cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, true).unwrap();
        assert!(cache.snapshot().hidden_paths.contains(&key("B.bsl")));

        fs::write(&unreadable, [0xff, 0xfe]).unwrap();
        fs::write(&faulted, "Процедура Третья()\nКонецПроцедуры").unwrap();
        let embedder = super::Embedder::new(crate::EmbedderConfig {
            base_url: "http://127.0.0.1:1".to_owned(),
            model: "test-model".to_owned(),
            dim: Some(3),
            api_key: None,
            provider: None,
            ..Default::default()
        });
        let result =
            cache.full_refresh_from_manifest(&manifest, &roots, Some(&embedder), 32, &store);
        assert_eq!(
            result.unwrap_err().embedding_failure().unwrap().code,
            crate::EmbeddingFailureCode::EmbeddingTransportError,
            "the full pass reports its failure after settling the publication tail",
        );
        assert!(
            cache.dirty_paths_snapshot().contains_key(&key("A.bsl")),
            "the unreadable file keeps its retry mark"
        );
        assert_eq!(cache.unread_keys_count(), 1, "the unreadable file is a durable signal");
        assert!(
            cache.dirty_paths_snapshot().contains_key(&key("B.bsl")),
            "the build-faulted file keeps its retry mark"
        );
        let snapshot = cache.snapshot();
        assert!(
            snapshot.hidden_paths.contains(&key("B.bsl")),
            "the faulted key's hiding survives the whole-replace"
        );
        assert!(
            snapshot.lexical_documents.iter().any(|document| document.symbol_name == "Вторая"),
            "the faulted key's prior entry keeps serving"
        );
    }

    /// A clean full plan REPLACES the row table even when it planned nothing: the table must
    /// hold exactly what the pass proved, so the row of a file deleted while marked cannot
    /// vouch for a same-stat namesake later.
    #[test]
    fn a_clean_empty_plan_retracts_unseen_rows() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let file = workspace.join("Gone.bsl");
        let first = "Процедура Первая()\nКонецПроцедуры";
        fs::write(&file, first).unwrap();
        let manifest = HashMap::from([(key("Gone.bsl"), "manifest-differs".to_owned())]);
        let roots = single_root(workspace);
        let store = Store::open(&workspace.join("search.db")).unwrap();
        let mut cache = WorkspaceOverlayCache::default();
        cache.enable_watcher_mode();
        cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, true).unwrap();
        let mtime = fs::metadata(&file).unwrap().modified().unwrap();
        assert_eq!(
            store.load_overlay_fingerprint_cache("").unwrap_or(None).unwrap_or_default().len(),
            1,
            "the warm pass persisted the row"
        );

        fs::remove_file(&file).unwrap();
        cache.mark_dirty_path(key("Gone.bsl"));
        let dirty_before = cache.publication_baseline();
        let plan = WorkspaceOverlayCache::plan_full_refresh_from_manifest(
            &manifest,
            &roots,
            &store,
            &HashMap::new(),
            None,
            &HashSet::new(),
        )
        .unwrap();
        cache.publish_plan(plan, HashMap::new(), &dirty_before, None, &store).unwrap();
        assert!(
            store.load_overlay_fingerprint_cache("").unwrap_or(None).unwrap_or_default().is_empty(),
            "the clean replace-save must run even with nothing to write"
        );

        let second = "Процедура Вторая()\nКонецПроцедуры";
        fs::write(&file, second).unwrap();
        fs::File::options().write(true).open(&file).unwrap().set_modified(mtime).unwrap();
        let replan = WorkspaceOverlayCache::plan_full_refresh_from_manifest(
            &manifest,
            &roots,
            &store,
            &HashMap::new(),
            None,
            &HashSet::new(),
        )
        .unwrap();
        assert_eq!(replan.overlay_file_count(), 1, "the same-stat namesake is re-read");
    }

    /// When live marks filter out EVERY planned row, the replace-save still runs: the row of
    /// an unseen, unmarked key (a deleted file) has no other retraction channel.
    #[test]
    fn a_publish_whose_rows_were_all_filtered_still_saves() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let kept = workspace.join("A.bsl");
        let gone = workspace.join("U.bsl");
        fs::write(&kept, "Процедура Первая()\nКонецПроцедуры").unwrap();
        fs::write(&gone, "Процедура Ушедшая()\nКонецПроцедуры").unwrap();
        let manifest = HashMap::from([
            (key("A.bsl"), "manifest-differs-a".to_owned()),
            (key("U.bsl"), "manifest-differs-u".to_owned()),
        ]);
        let roots = single_root(workspace);
        let store = Store::open(&workspace.join("search.db")).unwrap();
        let mut cache = WorkspaceOverlayCache::default();
        cache.enable_watcher_mode();
        cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, true).unwrap();
        assert_eq!(
            store.load_overlay_fingerprint_cache("").unwrap_or(None).unwrap_or_default().len(),
            2,
            "the warm pass persisted both rows"
        );

        fs::remove_file(&gone).unwrap();
        let dirty_before = cache.publication_baseline();
        let plan = WorkspaceOverlayCache::plan_full_refresh_from_manifest(
            &manifest,
            &roots,
            &store,
            &HashMap::new(),
            None,
            &HashSet::new(),
        )
        .unwrap();
        // A watcher re-mark AFTER the plan was built: the plan's row for A is stale and gets
        // filtered, leaving the planned row set empty.
        cache.mark_dirty_path(key("A.bsl"));
        cache.publish_plan(plan, HashMap::new(), &dirty_before, None, &store).unwrap();
        assert!(
            store.load_overlay_fingerprint_cache("").unwrap_or(None).unwrap_or_default().is_empty(),
            "the replace-save runs even with every planned row filtered, killing U's row"
        );
    }

    /// A clean full scan extinguishes `unread_keys` for keys it proved ABSENT: without this
    /// a deleted-after-fault file would keep the retry condition true forever.
    #[test]
    fn a_clean_pass_extinguishes_unread_keys_of_deleted_files() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let file = workspace.join("A.bsl");
        fs::write(&file, [0xff, 0xfe]).unwrap();
        let manifest = HashMap::from([(key("A.bsl"), "manifest-differs".to_owned())]);
        let roots = single_root(workspace);
        let store = Store::open(&workspace.join("search.db")).unwrap();
        let mut cache = WorkspaceOverlayCache::default();
        cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, true).unwrap();
        assert_eq!(cache.unread_keys_count(), 1, "the unreadable file is recorded");

        fs::remove_file(&file).unwrap();
        cache.full_refresh_from_manifest(&manifest, &roots, None, 32, &store).unwrap();
        assert_eq!(
            cache.unread_keys_count(),
            0,
            "absence from a clean scan settles the unread obligation"
        );
    }

    /// When the transactional replace-save fails, both the OLD table and live map survive
    /// intact. The retained mark distrusts that old row on the retry.
    #[test]
    fn a_failed_replace_save_rolls_back_and_retains_the_retry_mark() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let file = workspace.join("A.bsl");
        let first = "Процедура Первая()\nКонецПроцедуры";
        fs::write(&file, first).unwrap();
        let manifest = HashMap::from([(key("A.bsl"), "manifest-differs".to_owned())]);
        let roots = single_root(workspace);
        let db_path = workspace.join("search.db");
        let store = Store::open(&db_path).unwrap();
        let mut cache = WorkspaceOverlayCache::default();
        cache.enable_watcher_mode();
        cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, true).unwrap();
        let mtime = fs::metadata(&file).unwrap().modified().unwrap();

        // A same-stat edit, marked; the plan reads it and its mark is consumed on publish.
        fs::write(&file, "Процедура Вторая()\nКонецПроцедуры").unwrap();
        fs::File::options().write(true).open(&file).unwrap().set_modified(mtime).unwrap();
        cache.mark_dirty_path(key("A.bsl"));
        let dirty_before = cache.publication_baseline();
        let plan = WorkspaceOverlayCache::plan_full_refresh_from_manifest(
            &manifest,
            &roots,
            &store,
            &HashMap::new(),
            None,
            &HashSet::new(),
        )
        .unwrap();
        assert_eq!(plan.overlay_file_count(), 1, "the marked edit is re-read by the plan");

        // The INSERT of A's fresh row is denied: the entire replace-save rolls back.
        let saboteur = rusqlite::Connection::open(&db_path).unwrap();
        saboteur
            .execute_batch(
                "CREATE TRIGGER deny_a_insert BEFORE INSERT ON overlay_fingerprint_cache \
                 WHEN NEW.path = 'A.bsl' BEGIN SELECT RAISE(FAIL, 'deny'); END;",
            )
            .unwrap();
        assert!(cache.publish_plan(plan, HashMap::new(), &dirty_before, None, &store).is_err());
        assert!(
            !store
                .load_overlay_fingerprint_cache("")
                .unwrap_or(None)
                .unwrap_or_default()
                .is_empty(),
            "atomic rollback keeps the prior table"
        );
        assert!(cache.dirty_paths_snapshot().contains_key(&key("A.bsl")));
        // Retry control: the retained debt makes a fresh plan re-read the same-stat file.
        saboteur.execute_batch("DROP TRIGGER deny_a_insert;").unwrap();
        let replan = WorkspaceOverlayCache::plan_full_refresh_from_manifest(
            &manifest,
            &roots,
            &store,
            &HashMap::new(),
            None,
            &cache.publication_baseline().retry_distrusted(),
        )
        .unwrap();
        assert_eq!(replan.overlay_file_count(), 1, "nothing on disk suppresses the re-read");
    }

    /// И-а1: a deletion proven through the removal channel (dead root, `remove_known_deleted`)
    /// survives the publication of a plan built BEFORE it — on every carrier, and even after
    /// the point budget gives up on the dead root.
    #[test]
    fn a_planned_publish_does_not_resurrect_a_proven_delete() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let configuration = workspace.join("cf");
        fs::create_dir(&configuration).unwrap();
        fs::write(configuration.join("A.bsl"), "Процедура Живая()\nКонецПроцедуры").unwrap();
        let manifest = HashMap::from([(key("A.bsl"), "manifest-differs".to_owned())]);
        let (roots, _) = WorkspaceRoots::build(workspace, &configuration, &[]);
        let store = Store::open(&workspace.join("search.db")).unwrap();
        let mut cache = WorkspaceOverlayCache::default();
        cache.enable_watcher_mode();
        cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, true).unwrap();

        let baseline = cache.publication_baseline();
        let plan = WorkspaceOverlayCache::plan_full_refresh_from_manifest(
            &manifest,
            &roots,
            &store,
            &HashMap::new(),
            None,
            &HashSet::new(),
        )
        .unwrap();
        assert_eq!(plan.overlay_file_count(), 1, "the plan reads the then-live file");

        // The whole root vanishes with the file; the removal channel proves the deletion.
        fs::rename(&configuration, workspace.join("cf.saved")).unwrap();
        cache.remove_known_deleted(&key("A.bsl"), true);
        cache.mark_dirty_path(key("A.bsl"));

        cache.publish_plan(plan, HashMap::new(), &baseline, None, &store).unwrap();
        let snapshot = cache.snapshot();
        assert!(
            snapshot.hidden_paths.contains(&key("A.bsl")),
            "the proven deletion's hiding survives the stale plan"
        );
        assert!(
            snapshot.lexical_documents.is_empty(),
            "the stale plan's entry must not resurrect the deleted file"
        );
        assert!(
            store.load_overlay_fingerprint_cache("").unwrap_or(None).unwrap_or_default().is_empty(),
            "the stale plan's row must not land in the store"
        );

        // The dead root exhausts the point budget; the hiding still stands.
        for _ in 0..(MAX_DIRTY_REFRESH_FAILURES + 1) {
            cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, false).unwrap();
        }
        assert!(
            cache.snapshot().hidden_paths.contains(&key("A.bsl")),
            "budget exhaustion must not undo the proven deletion"
        );
    }

    /// И-а2: an edit the point path already published between Phase A and Phase C survives the
    /// publication of the older plan on S and R — and after a "restart" (a fresh plan over the
    /// same store) nothing suppresses the re-read.
    #[test]
    fn a_point_reindex_between_phases_survives_the_publish() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let file = workspace.join("A.bsl");
        fs::write(&file, "Процедура Первая()\nКонецПроцедуры").unwrap();
        let manifest = HashMap::from([(key("A.bsl"), "manifest-differs".to_owned())]);
        let roots = single_root(workspace);
        let store = Store::open(&workspace.join("search.db")).unwrap();
        let mut cache = WorkspaceOverlayCache::default();
        cache.enable_watcher_mode();
        cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, true).unwrap();

        let baseline = cache.publication_baseline();
        let plan = WorkspaceOverlayCache::plan_full_refresh_from_manifest(
            &manifest,
            &roots,
            &store,
            &HashMap::new(),
            None,
            &HashSet::new(),
        )
        .unwrap();

        // The edit lands mid-pass and the point path publishes it (mark consumed).
        fs::write(&file, "Процедура Вторая()\nКонецПроцедуры").unwrap();
        cache.mark_dirty_path(key("A.bsl"));
        cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, false).unwrap();
        assert!(cache.dirty_paths_snapshot().is_empty(), "the point pass consumed the mark");

        cache.publish_plan(plan, HashMap::new(), &baseline, None, &store).unwrap();
        assert!(
            cache
                .snapshot()
                .lexical_documents
                .iter()
                .any(|document| document.symbol_name == "Вторая"),
            "the older plan must not roll the published edit back"
        );
        assert!(
            store.load_overlay_fingerprint_cache("").unwrap_or(None).unwrap_or_default().is_empty(),
            "the stale plan's row for the out-fenced key is dropped"
        );
        let replan = WorkspaceOverlayCache::plan_full_refresh_from_manifest(
            &manifest,
            &roots,
            &store,
            &HashMap::new(),
            None,
            &HashSet::new(),
        )
        .unwrap();
        assert_eq!(replan.overlay_file_count(), 1, "nothing on disk suppresses the re-read");
    }

    /// И-а2б: a proven point outcome fences the plan even when its S mutation was a NO-OP —
    /// here a BaselineEqual settling a key S never held. The proof itself is fresh knowledge.
    #[test]
    fn an_idempotent_baseline_equal_settles_the_fence() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let file = workspace.join("A.bsl");
        let base = "Процедура Базовая()\nКонецПроцедуры";
        fs::write(&file, base).unwrap();
        let manifest = HashMap::from([(key("A.bsl"), super::fingerprint_content(base, "A.bsl"))]);
        let roots = single_root(workspace);
        let store = Store::open(&workspace.join("search.db")).unwrap();
        let mut cache = WorkspaceOverlayCache::default();
        cache.enable_watcher_mode();
        cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, true).unwrap();
        assert_eq!(cache.stats().overlay_files, 0, "baseline-equal: no entry");

        // The plan reads a TEMPORARY local edit...
        fs::write(&file, "Процедура Временная()\nКонецПроцедуры").unwrap();
        let baseline = cache.publication_baseline();
        let plan = WorkspaceOverlayCache::plan_full_refresh_from_manifest(
            &manifest,
            &roots,
            &store,
            &HashMap::new(),
            None,
            &HashSet::new(),
        )
        .unwrap();
        assert_eq!(plan.overlay_file_count(), 1, "the plan reads the temporary edit");

        // ...which is REVERTED before Phase C; the point path proves baseline-equality (a
        // no-op on S: there was no entry to remove).
        fs::write(&file, base).unwrap();
        cache.mark_dirty_path(key("A.bsl"));
        cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, false).unwrap();

        cache.publish_plan(plan, HashMap::new(), &baseline, None, &store).unwrap();
        assert_eq!(
            cache.stats().overlay_files,
            0,
            "the stale plan must not resurrect the reverted edit"
        );
    }

    /// И-а3 + инвалидаторы: every wholesale invalidation between Phase A and Phase C makes
    /// the older plan `Superseded` — nothing of it applies.
    #[test]
    fn a_wholesale_invalidation_supersedes_an_older_plan() {
        let build = |workspace: &Path, store: &Store, cache: &mut WorkspaceOverlayCache| {
            let manifest = HashMap::from([(key("A.bsl"), "manifest-differs".to_owned())]);
            let roots = single_root(workspace);
            cache.refresh_with_manifest(&manifest, &roots, None, 32, store, true).unwrap();
            let baseline = cache.publication_baseline();
            let plan = WorkspaceOverlayCache::plan_full_refresh_from_manifest(
                &manifest,
                &roots,
                store,
                &HashMap::new(),
                None,
                &HashSet::new(),
            )
            .unwrap();
            (manifest, roots, baseline, plan)
        };

        // In-place full refresh between the phases.
        {
            let dir = tempdir().unwrap();
            let workspace = dir.path();
            fs::write(workspace.join("A.bsl"), "Процедура Первая()\nКонецПроцедуры").unwrap();
            let store = Store::open(&workspace.join("search.db")).unwrap();
            let mut cache = WorkspaceOverlayCache::default();
            cache.enable_watcher_mode();
            let (manifest, roots, baseline, plan) = build(workspace, &store, &mut cache);
            fs::write(workspace.join("A.bsl"), "Процедура Вторая()\nКонецПроцедуры").unwrap();
            cache.full_refresh_from_manifest(&manifest, &roots, None, 32, &store).unwrap();
            let outcome =
                cache.publish_plan(plan, HashMap::new(), &baseline, None, &store).unwrap();
            assert_eq!(outcome, super::PublishOutcome::Superseded);
            assert!(
                cache
                    .snapshot()
                    .lexical_documents
                    .iter()
                    .any(|document| document.symbol_name == "Вторая"),
                "the in-place publication's fresher state stands"
            );
        }
        // clear() between the phases.
        {
            let dir = tempdir().unwrap();
            let workspace = dir.path();
            fs::write(workspace.join("A.bsl"), "Процедура Первая()\nКонецПроцедуры").unwrap();
            let store = Store::open(&workspace.join("search.db")).unwrap();
            let mut cache = WorkspaceOverlayCache::default();
            cache.enable_watcher_mode();
            let (_, _, baseline, plan) = build(workspace, &store, &mut cache);
            cache.clear();
            let outcome =
                cache.publish_plan(plan, HashMap::new(), &baseline, None, &store).unwrap();
            assert_eq!(outcome, super::PublishOutcome::Superseded);
        }
        // mark_initialized_clean() between the phases.
        {
            let dir = tempdir().unwrap();
            let workspace = dir.path();
            fs::write(workspace.join("A.bsl"), "Процедура Первая()\nКонецПроцедуры").unwrap();
            let store = Store::open(&workspace.join("search.db")).unwrap();
            let mut cache = WorkspaceOverlayCache::default();
            cache.enable_watcher_mode();
            let (_, _, baseline, plan) = build(workspace, &store, &mut cache);
            cache.mark_initialized_clean();
            let outcome =
                cache.publish_plan(plan, HashMap::new(), &baseline, None, &store).unwrap();
            assert_eq!(outcome, super::PublishOutcome::Superseded);
        }
        // Both provider installation variants between the phases.
        for replace_only in [false, true] {
            struct NoContext;
            impl crate::ports::GraphContextProvider for NoContext {
                fn graph_context(
                    &self,
                    _rel_path: &str,
                    _symbol_name: &str,
                    _kind: &str,
                ) -> Option<String> {
                    None
                }
            }
            let dir = tempdir().unwrap();
            let workspace = dir.path();
            fs::write(workspace.join("A.bsl"), "Процедура Первая()\nКонецПроцедуры").unwrap();
            let store = Store::open(&workspace.join("search.db")).unwrap();
            let mut cache = WorkspaceOverlayCache::default();
            cache.enable_watcher_mode();
            let (_, _, baseline, plan) = build(workspace, &store, &mut cache);
            if replace_only {
                cache.replace_graph_context_provider(std::sync::Arc::new(NoContext));
            } else {
                cache.set_graph_context_provider(std::sync::Arc::new(NoContext));
            }
            let outcome =
                cache.publish_plan(plan, HashMap::new(), &baseline, None, &store).unwrap();
            assert_eq!(outcome, super::PublishOutcome::Superseded);
            if !replace_only {
                assert!(!cache.initialized, "the invalidation's re-init obligation stands");
            }
        }
    }

    /// И-а4: a key the plan's gate skipped by a LYING row (its retraction was denied, its mark
    /// is alive) keeps its point-published state through the publish — the fence cannot
    /// protect it (the settlement predates the fence), the live mark must.
    #[test]
    fn a_gate_skipped_key_with_a_live_mark_keeps_its_point_state() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let file = workspace.join("A.bsl");
        let base = "Процедура Первая()\nКонецПроцедуры";
        fs::write(&file, base).unwrap();
        let manifest = HashMap::from([(key("A.bsl"), super::fingerprint_content(base, "A.bsl"))]);
        let roots = single_root(workspace);
        let db_path = workspace.join("search.db");
        let store = Store::open(&db_path).unwrap();
        let mut cache = WorkspaceOverlayCache::default();
        cache.enable_watcher_mode();
        cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, true).unwrap();
        let mtime = fs::metadata(&file).unwrap().modified().unwrap();

        // A same-stat edit; the point pass publishes it, but the row retraction is denied —
        // the row survives, lying "baseline-equal", and the mark stays alive.
        fs::write(&file, "Процедура Вторая()\nКонецПроцедуры").unwrap();
        fs::File::options().write(true).open(&file).unwrap().set_modified(mtime).unwrap();
        let saboteur = rusqlite::Connection::open(&db_path).unwrap();
        saboteur
            .execute_batch(
                "CREATE TRIGGER deny_fp_delete BEFORE DELETE ON overlay_fingerprint_cache \
                 BEGIN SELECT RAISE(FAIL, 'deny'); END;",
            )
            .unwrap();
        cache.mark_dirty_path(key("A.bsl"));
        cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, false).unwrap();
        assert_eq!(cache.stats().overlay_files, 1, "the point pass published the edit");
        assert!(
            cache.dirty_paths_snapshot().contains_key(&key("A.bsl")),
            "the denied retraction keeps the mark"
        );

        // The plan's gate trusts the surviving row and skips the file unread.
        let baseline = cache.publication_baseline();
        let plan = WorkspaceOverlayCache::plan_full_refresh_from_manifest(
            &manifest,
            &roots,
            &store,
            &HashMap::new(),
            None,
            &HashSet::new(),
        )
        .unwrap();
        assert_eq!(plan.overlay_file_count(), 0, "the lying row hid the edit from the plan");

        saboteur.execute_batch("DROP TRIGGER deny_fp_delete;").unwrap();
        cache.publish_plan(plan, HashMap::new(), &baseline, None, &store).unwrap();
        assert_eq!(
            cache.stats().overlay_files,
            1,
            "the point-published edit survives the gate-skipped publish"
        );
        assert!(
            cache.dirty_paths_snapshot().contains_key(&key("A.bsl")),
            "the mark survives to finish the row's retraction"
        );
        assert!(
            store.load_overlay_fingerprint_cache("").unwrap_or(None).unwrap_or_default().is_empty(),
            "the lying row does not survive the publish"
        );
    }

    /// И-а5: the FAULT settlements fence the plan too. After the point budget drops the mark,
    /// `unread_keys` (FileFault) or the settled S (ProvenGone) may be the only witnesses — a
    /// stale clean plan must not erase them. (A BuildFault leg is deliberately absent: its
    /// mark is retained UNCHARGED and never exhausts, so the mark itself always outlives any
    /// plan and a fence-specific failure cannot be constructed.)
    #[test]
    fn a_point_fault_between_phases_still_fences_the_publish() {
        // FileFault leg: the budget-exhausted unreadable file keeps its unread signal.
        {
            let dir = tempdir().unwrap();
            let workspace = dir.path();
            let file = workspace.join("A.bsl");
            fs::write(&file, "Процедура Первая()\nКонецПроцедуры").unwrap();
            let manifest = HashMap::from([(key("A.bsl"), "manifest-differs".to_owned())]);
            let roots = single_root(workspace);
            let store = Store::open(&workspace.join("search.db")).unwrap();
            let mut cache = WorkspaceOverlayCache::default();
            cache.enable_watcher_mode();
            cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, true).unwrap();

            let baseline = cache.publication_baseline();
            let plan = WorkspaceOverlayCache::plan_full_refresh_from_manifest(
                &manifest,
                &roots,
                &store,
                &HashMap::new(),
                None,
                &HashSet::new(),
            )
            .unwrap();

            fs::write(&file, [0xff, 0xfe]).unwrap();
            cache.mark_dirty_path(key("A.bsl"));
            for _ in 0..(MAX_DIRTY_REFRESH_FAILURES + 1) {
                cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, false).unwrap();
            }
            assert!(cache.dirty_paths_snapshot().is_empty(), "the budget dropped the mark");
            assert_eq!(cache.unread_keys_count(), 1, "unread is the surviving witness");

            cache.publish_plan(plan, HashMap::new(), &baseline, None, &store).unwrap();
            assert_eq!(
                cache.unread_keys_count(),
                1,
                "the stale clean plan must not erase the unread witness"
            );
        }
        // ProvenGone leg: the point-proven deletion is not resurrected.
        {
            let dir = tempdir().unwrap();
            let workspace = dir.path();
            let file = workspace.join("A.bsl");
            fs::write(&file, "Процедура Первая()\nКонецПроцедуры").unwrap();
            let manifest = HashMap::from([(key("A.bsl"), "manifest-differs".to_owned())]);
            let roots = single_root(workspace);
            let store = Store::open(&workspace.join("search.db")).unwrap();
            let mut cache = WorkspaceOverlayCache::default();
            cache.enable_watcher_mode();
            cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, true).unwrap();

            let baseline = cache.publication_baseline();
            let plan = WorkspaceOverlayCache::plan_full_refresh_from_manifest(
                &manifest,
                &roots,
                &store,
                &HashMap::new(),
                None,
                &HashSet::new(),
            )
            .unwrap();
            assert_eq!(plan.overlay_file_count(), 1);

            fs::remove_file(&file).unwrap();
            cache.mark_dirty_path(key("A.bsl"));
            cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, false).unwrap();
            assert_eq!(cache.stats().overlay_files, 0, "the point pass proved the deletion");

            cache.publish_plan(plan, HashMap::new(), &baseline, None, &store).unwrap();
            let snapshot = cache.snapshot();
            assert!(
                snapshot.lexical_documents.is_empty(),
                "the stale plan must not resurrect the deleted file"
            );
            assert!(
                snapshot.hidden_paths.contains(&key("A.bsl")),
                "the proven deletion's hiding stands"
            );
        }
    }

    /// A PARTIALLY vectorized entry counts as unembedded: `build_overlay_vectors` returns
    /// vectors only for warm-cached chunks, so emptiness alone would hide a half-embedded
    /// file from the retry driver.
    #[test]
    fn indexing_overlay_identity_follows_full_publication_not_new_manifest() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("search.db")).unwrap();
        let roots = single_root(dir.path());
        let mut cache = WorkspaceOverlayCache::default();
        let save = |id: &str, fp: &str| {
            store
                .save_baseline_manifest(&crate::WorkspaceBaselineManifest {
                    snapshot_id: id.into(),
                    snapshot_fingerprint: Some(fp.into()),
                    files: vec![],
                })
                .unwrap()
        };
        save("A", "fp-a");
        cache.full_refresh_from_manifest(&HashMap::new(), &roots, None, 32, &store).unwrap();
        assert_eq!(cache.baseline_identity, Some(("A".into(), Some("fp-a".into()))));
        let baseline = cache.publication_baseline();
        save("B", "fp-b");
        let plan = WorkspaceOverlayCache::plan_full_refresh_from_manifest(
            &HashMap::new(),
            &roots,
            &store,
            &HashMap::new(),
            None,
            &HashSet::new(),
        )
        .unwrap();
        save("B", "fp-c");
        assert_eq!(cache.baseline_identity, Some(("A".into(), Some("fp-a".into()))));
        cache.publish_plan(plan, HashMap::new(), &baseline, None, &store).unwrap();
        assert_eq!(cache.baseline_identity, Some(("B".into(), Some("fp-b".into()))));
        cache.clear();
        assert!(cache.baseline_identity.is_none());
    }

    #[test]
    fn indexing_local_qualification_tracks_partial_full_dirty_delete_and_clear() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let two_chunks = "Процедура Первая()\nКонецПроцедуры\nПроцедура Вторая()\nКонецПроцедуры";
        fs::write(workspace.join("A.bsl"), two_chunks).unwrap();
        let manifest = HashMap::from([(key("A.bsl"), "manifest-differs".to_owned())]);
        let roots = single_root(workspace);
        let store = Store::open(&workspace.join("search.db")).unwrap();
        let mut cache = WorkspaceOverlayCache::default();
        let baseline = cache.publication_baseline();
        let plan = WorkspaceOverlayCache::plan_full_refresh_from_manifest(
            &manifest,
            &roots,
            &store,
            &HashMap::new(),
            None,
            &HashSet::new(),
        )
        .unwrap();
        let missing: Vec<String> = plan.missing_embeddings().keys().cloned().collect();
        assert_eq!(missing.len(), 2, "two chunks, two missing embeddings");

        // Phase B delivered only ONE of the two vectors (say, the endpoint died mid-pass).
        let partial = HashMap::from([(missing[0].clone(), vec![1.0f32, 0.0, 0.0])]);
        cache.publish_plan(plan, partial, &baseline, None, &store).unwrap();
        assert_eq!(cache.unembedded_entry_count(), 1, "one vector of two is NOT a finished entry");
        let assert_count = |cache: &WorkspaceOverlayCache| {
            assert_eq!(
                cache.unembedded_entry_count(),
                cache
                    .entries
                    .values()
                    .filter(|entry| entry.vector_documents.len() < entry.embedding_inputs.len())
                    .count()
            );
            assert_eq!(cache.dirty_paths_count(), cache.dirty_paths_snapshot().len());
        };
        assert_count(&cache);
        cache.mark_dirty_path(key("A.bsl"));
        assert_eq!(cache.dirty_paths_count(), 1);
        let baseline = cache.publication_baseline();
        let plan = WorkspaceOverlayCache::plan_full_refresh_from_manifest(
            &manifest,
            &roots,
            &store,
            &cache.embedding_cache_snapshot(),
            None,
            &HashSet::new(),
        )
        .unwrap();
        let remaining = plan
            .missing_embeddings()
            .keys()
            .map(|key| (key.clone(), vec![1.0f32, 0.0, 0.0]))
            .collect();
        cache.publish_plan(plan, remaining, &baseline, None, &store).unwrap();
        assert_count(&cache);
        assert_eq!(cache.unembedded_entry_count(), 0);
        assert_eq!(cache.dirty_paths_count(), 0);
        assert!(cache.is_initialized());
        cache.remove_known_deleted(&key("A.bsl"), true);
        assert_count(&cache);
        cache.mark_initialized_clean();
        assert_count(&cache);
        cache.mark_dirty_path(key("A.bsl"));
        cache.clear();
        assert_count(&cache);
        assert!(!cache.is_initialized());
    }

    /// The unread debt VETOES the full pass's equal-fingerprint gates: after the point budget
    /// is exhausted the debt is the only witness, and a same-stat gate hit would settle the
    /// key without ever reading it — publishing yesterday's content forever.
    #[test]
    fn the_unread_debt_forces_the_full_pass_to_reread() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let file = workspace.join("A.bsl");
        let first = "Процедура Первая()\nКонецПроцедуры";
        fs::write(&file, first).unwrap();
        let manifest = HashMap::from([(key("A.bsl"), "manifest-differs".to_owned())]);
        let roots = single_root(workspace);
        let store = Store::open(&workspace.join("search.db")).unwrap();
        let mut cache = WorkspaceOverlayCache::default();
        cache.enable_watcher_mode();
        cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, true).unwrap();
        let mtime = fs::metadata(&file).unwrap().modified().unwrap();

        // A same-stat corruption exhausts the point budget; the debt survives the marks.
        fs::write(&file, vec![0xff; first.len()]).unwrap();
        fs::File::options().write(true).open(&file).unwrap().set_modified(mtime).unwrap();
        cache.mark_dirty_path(key("A.bsl"));
        for _ in 0..(MAX_DIRTY_REFRESH_FAILURES + 1) {
            cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, false).unwrap();
        }
        assert!(cache.dirty_paths_snapshot().is_empty(), "the budget dropped the mark");
        assert_eq!(cache.unread_keys_count(), 1, "the debt is the surviving witness");

        // Still unreadable: the full pass must TRY the read (and fail), not gate past it.
        cache.full_refresh_from_manifest(&manifest, &roots, None, 32, &store).unwrap();
        assert_eq!(
            cache.unread_keys_count(),
            1,
            "a gate hit is not a read; the debt survives the full pass"
        );

        // Readable again at the same stat: the veto forces the re-read and the fresh content
        // is published, settling the debt.
        let second = "Процедура Вторая()\nКонецПроцедуры";
        fs::write(&file, second).unwrap();
        fs::File::options().write(true).open(&file).unwrap().set_modified(mtime).unwrap();
        cache.full_refresh_from_manifest(&manifest, &roots, None, 32, &store).unwrap();
        assert_eq!(cache.unread_keys_count(), 0, "the successful read settles the debt");
        assert!(
            cache
                .snapshot()
                .lexical_documents
                .iter()
                .any(|document| document.symbol_name == "Вторая"),
            "the same-stat recovery is published, not gated away"
        );
    }

    /// Two plans in flight: a NEWER plan (whose fence postdates the point settlement)
    /// publishing first must not strip the fence the OLDER plan still needs — the library
    /// does not enforce the driver's single-flight.
    #[test]
    fn an_overlapping_newer_publish_keeps_the_fence_for_the_older_plan() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let file = workspace.join("A.bsl");
        fs::write(&file, "Процедура Первая()\nКонецПроцедуры").unwrap();
        let manifest = HashMap::from([(key("A.bsl"), "manifest-differs".to_owned())]);
        let roots = single_root(workspace);
        let store = Store::open(&workspace.join("search.db")).unwrap();
        let mut cache = WorkspaceOverlayCache::default();
        cache.enable_watcher_mode();
        cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, true).unwrap();

        let old_baseline = cache.publication_baseline();
        let old_plan = WorkspaceOverlayCache::plan_full_refresh_from_manifest(
            &manifest,
            &roots,
            &store,
            &HashMap::new(),
            None,
            &HashSet::new(),
        )
        .unwrap();

        // The point path publishes the edit; a NEWER plan (fence after the settlement)
        // publishes before the older one.
        fs::write(&file, "Процедура Вторая()\nКонецПроцедуры").unwrap();
        cache.mark_dirty_path(key("A.bsl"));
        cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, false).unwrap();
        let newer_baseline = cache.publication_baseline();
        let newer_plan = WorkspaceOverlayCache::plan_full_refresh_from_manifest(
            &manifest,
            &roots,
            &store,
            &HashMap::new(),
            None,
            &HashSet::new(),
        )
        .unwrap();
        cache.publish_plan(newer_plan, HashMap::new(), &newer_baseline, None, &store).unwrap();

        cache.publish_plan(old_plan, HashMap::new(), &old_baseline, None, &store).unwrap();
        assert!(
            cache
                .snapshot()
                .lexical_documents
                .iter()
                .any(|document| document.symbol_name == "Вторая"),
            "the newer publish must not strip the fence the older plan still needs"
        );
    }

    /// A stale plan's read failure must not re-open an obligation a fresher point settlement
    /// already resolved: the fenced key keeps its settled state on the marks and the debt.
    #[test]
    fn a_stale_read_failure_does_not_reopen_a_fresh_settlement() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let file = workspace.join("A.bsl");
        fs::write(&file, [0xff, 0xfe]).unwrap();
        let manifest = HashMap::from([(key("A.bsl"), "manifest-differs".to_owned())]);
        let roots = single_root(workspace);
        let store = Store::open(&workspace.join("search.db")).unwrap();
        let mut cache = WorkspaceOverlayCache::default();
        cache.enable_watcher_mode();
        cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, true).unwrap();
        assert_eq!(cache.unread_keys_count(), 1, "the warm pass records the failure");

        let baseline = cache.publication_baseline();
        let plan = WorkspaceOverlayCache::plan_full_refresh_from_manifest(
            &manifest,
            &roots,
            &store,
            &HashMap::new(),
            None,
            &HashSet::new(),
        )
        .unwrap();
        assert_eq!(plan.read_failure_count(), 1, "the plan saw the same failure");

        // The file recovers and the point path settles everything.
        fs::write(&file, "Процедура Ожила()\nКонецПроцедуры").unwrap();
        cache.mark_dirty_path(key("A.bsl"));
        cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, false).unwrap();
        assert_eq!(cache.unread_keys_count(), 0);
        assert!(cache.dirty_paths_snapshot().is_empty());

        cache.publish_plan(plan, HashMap::new(), &baseline, None, &store).unwrap();
        assert_eq!(
            cache.unread_keys_count(),
            0,
            "the stale plan's failure must not re-open the settled debt"
        );
        assert!(
            cache.dirty_paths_snapshot().is_empty(),
            "the stale plan's failure must not re-mark the settled key"
        );
    }

    /// An UNCLEAN stale plan must not extinguish the unread debt of a key whose point
    /// settlement out-fenced it: the plan's old successful sighting is not a read of the
    /// CURRENT content.
    #[test]
    fn a_fenced_unread_debt_survives_an_unclean_plan() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let file = workspace.join("A.bsl");
        fs::write(&file, "Процедура Первая()\nКонецПроцедуры").unwrap();
        let manifest = HashMap::from([(key("A.bsl"), "manifest-differs".to_owned())]);
        let roots = single_root(workspace);
        let store = Store::open(&workspace.join("search.db")).unwrap();
        let mut cache = WorkspaceOverlayCache::default();
        cache.enable_watcher_mode();
        cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, true).unwrap();

        let baseline = cache.publication_baseline();
        let plan = WorkspaceOverlayCache::plan_full_refresh_from_manifest_scanned(
            &manifest,
            scanned_with(&[(&key("A.bsl"), &file)], 1, 0),
            &store,
            &HashMap::new(),
            None,
            &HashSet::new(),
        )
        .unwrap();
        assert!(!plan.scan_is_clean(), "the plan is deliberately unclean");

        // The file breaks AFTER the plan read it; the point budget runs dry and the debt is
        // the only witness left.
        fs::write(&file, [0xff, 0xfe]).unwrap();
        cache.mark_dirty_path(key("A.bsl"));
        for _ in 0..(MAX_DIRTY_REFRESH_FAILURES + 1) {
            cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, false).unwrap();
        }
        assert!(cache.dirty_paths_snapshot().is_empty());
        assert_eq!(cache.unread_keys_count(), 1);

        cache.publish_plan(plan, HashMap::new(), &baseline, None, &store).unwrap();
        assert_eq!(
            cache.unread_keys_count(),
            1,
            "the stale sighting is not a read; the debt survives the unclean publish"
        );
    }

    /// The unread debt distrusts even a REPOPULATED fingerprint row: the local retraction
    /// landed at fault time, but the shared table may be rewritten by a neighbouring daemon
    /// whose knowledge predates the fault — a stat-matching row is exactly what the debt
    /// says cannot be believed, so Phase A must read the file.
    #[test]
    fn the_unread_debt_distrusts_a_repopulated_row() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let file = workspace.join("A.bsl");
        let base = "Процедура Первая()\nКонецПроцедуры";
        fs::write(&file, base).unwrap();
        let manifest = HashMap::from([(key("A.bsl"), super::fingerprint_content(base, "A.bsl"))]);
        let roots = single_root(workspace);
        let store = Store::open(&workspace.join("search.db")).unwrap();
        let mut cache = WorkspaceOverlayCache::default();
        cache.enable_watcher_mode();
        cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, true).unwrap();
        let mtime = fs::metadata(&file).unwrap().modified().unwrap();
        let initial_rows =
            store.load_overlay_fingerprint_cache("").unwrap_or(None).unwrap_or_default();
        assert_eq!(initial_rows.len(), 1, "the baseline-equal row is persisted");

        // A same-stat corruption exhausts the point budget; only the debt survives.
        fs::write(&file, vec![0xff; base.len()]).unwrap();
        fs::File::options().write(true).open(&file).unwrap().set_modified(mtime).unwrap();
        cache.mark_dirty_path(key("A.bsl"));
        for _ in 0..(MAX_DIRTY_REFRESH_FAILURES + 1) {
            cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, false).unwrap();
        }
        assert!(cache.dirty_paths_snapshot().is_empty());
        assert_eq!(cache.unread_keys_count(), 1);

        // A neighbouring daemon repopulates the table with its pre-fault knowledge, and the
        // file recovers with DIFFERENT content at the same stat.
        store.save_overlay_fingerprint_cache("", &initial_rows).unwrap();
        let second = "Процедура Вторая()\nКонецПроцедуры";
        fs::write(&file, second).unwrap();
        fs::File::options().write(true).open(&file).unwrap().set_modified(mtime).unwrap();

        let baseline = cache.publication_baseline();
        let plan = WorkspaceOverlayCache::plan_full_refresh_from_manifest(
            &manifest,
            &roots,
            &store,
            &HashMap::new(),
            None,
            baseline.distrusted(),
        )
        .unwrap();
        assert_eq!(plan.overlay_file_count(), 1, "the distrusted row does not gate the read");
        cache.publish_plan(plan, HashMap::new(), &baseline, None, &store).unwrap();
        assert_eq!(cache.unread_keys_count(), 0, "the successful read settles the debt");
        assert!(
            cache
                .snapshot()
                .lexical_documents
                .iter()
                .any(|document| document.symbol_name == "Вторая"),
            "the recovered content is published, not gated away by the repopulated row"
        );
    }

    /// A deletion-only applied state is NOT "no local diffs": the publish outcome carries
    /// the deletion count, so the caller cannot claim the tree matches the baseline while a
    /// hidden baseline file is being filtered out.
    #[test]
    fn a_deletion_only_publish_reports_the_deletion() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let file = workspace.join("A.bsl");
        fs::write(&file, "Процедура Первая()\nКонецПроцедуры").unwrap();
        let manifest = HashMap::from([(key("A.bsl"), "manifest-differs".to_owned())]);
        let roots = single_root(workspace);
        let store = Store::open(&workspace.join("search.db")).unwrap();
        let mut cache = WorkspaceOverlayCache::default();
        cache.enable_watcher_mode();
        cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, true).unwrap();

        fs::remove_file(&file).unwrap();
        let baseline = cache.publication_baseline();
        let plan = WorkspaceOverlayCache::plan_full_refresh_from_manifest(
            &manifest,
            &roots,
            &store,
            &HashMap::new(),
            None,
            baseline.distrusted(),
        )
        .unwrap();
        let outcome = cache.publish_plan(plan, HashMap::new(), &baseline, None, &store).unwrap();
        assert_eq!(
            outcome,
            super::PublishOutcome::Applied {
                gate_deferred: 0,
                persist_ok: true,
                overlay_files: 0,
                deleted_files: 1,
                unread_keys: 0
            },
            "zero entries with a local deletion is still a local diff"
        );
    }

    /// A published plan prunes CARRIER-LESS settlements at its fence: a workspace rotating
    /// unique paths must not grow the fence map with its history. Settlements whose key
    /// still carries state survive (the overlapping-plans protection).
    #[test]
    fn a_publish_prunes_carrierless_settlements() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let manifest: HashMap<FileKey, String> = HashMap::new();
        let roots = single_root(workspace);
        let store = Store::open(&workspace.join("search.db")).unwrap();
        let mut cache = WorkspaceOverlayCache::default();
        cache.enable_watcher_mode();
        cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, true).unwrap();

        for i in 0..100 {
            cache.remove_known_deleted(&key(&format!("Tmp{i}.bsl")), false);
        }
        assert_eq!(cache.settled_seq.len(), 100, "the deletions are fenced while unpublished");

        let baseline = cache.publication_baseline();
        let plan = WorkspaceOverlayCache::plan_full_refresh_from_manifest(
            &manifest,
            &roots,
            &store,
            &HashMap::new(),
            None,
            baseline.distrusted(),
        )
        .unwrap();
        cache.publish_plan(plan, HashMap::new(), &baseline, None, &store).unwrap();
        assert_eq!(
            cache.settled_seq.len(),
            0,
            "traceless keys leave no settlement behind once a newer plan published"
        );
    }

    /// A stale plan's old SUCCESS must not erase the failure streak a fresher point
    /// settlement recorded: the budget belongs to the newest knowledge.
    #[test]
    fn a_stale_success_preserves_a_fresher_failure_budget() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let file = workspace.join("A.bsl");
        fs::write(&file, "Процедура Первая()\nКонецПроцедуры").unwrap();
        let manifest = HashMap::from([(key("A.bsl"), "manifest-differs".to_owned())]);
        let roots = single_root(workspace);
        let store = Store::open(&workspace.join("search.db")).unwrap();
        let mut cache = WorkspaceOverlayCache::default();
        cache.enable_watcher_mode();
        cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, true).unwrap();

        let baseline = cache.publication_baseline();
        let plan = WorkspaceOverlayCache::plan_full_refresh_from_manifest(
            &manifest,
            &roots,
            &store,
            &HashMap::new(),
            None,
            baseline.distrusted(),
        )
        .unwrap();

        // A fresher point failure records one strike after the plan's successful read.
        fs::write(&file, [0xff, 0xfe]).unwrap();
        cache.mark_dirty_path(key("A.bsl"));
        cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, false).unwrap();
        assert_eq!(cache.dirty_failure_count(&key("A.bsl")), 1);

        cache.publish_plan(plan, HashMap::new(), &baseline, None, &store).unwrap();
        assert_eq!(
            cache.dirty_failure_count(&key("A.bsl")),
            1,
            "the stale success must not reset the fresher streak"
        );
    }

    /// One unverified file is ONE unread file in the outcome: a key both gate-deferred and
    /// freshly unread must not be counted twice by the publish result.
    #[test]
    fn a_deferred_key_that_went_unread_is_counted_once() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let file = workspace.join("A.bsl");
        let base = "Процедура Первая()\nКонецПроцедуры";
        fs::write(&file, base).unwrap();
        let manifest = HashMap::from([(key("A.bsl"), super::fingerprint_content(base, "A.bsl"))]);
        let roots = single_root(workspace);
        let store = Store::open(&workspace.join("search.db")).unwrap();
        let mut cache = WorkspaceOverlayCache::default();
        cache.enable_watcher_mode();
        cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, true).unwrap();
        let mtime = fs::metadata(&file).unwrap().modified().unwrap();

        // The plan trusts the baseline-equal row and skips the marked key unread...
        fs::write(&file, "Процедура Вторая()\nКонецПроцедуры").unwrap();
        fs::File::options().write(true).open(&file).unwrap().set_modified(mtime).unwrap();
        cache.mark_dirty_path(key("A.bsl"));
        let baseline = cache.publication_baseline();
        let plan = WorkspaceOverlayCache::plan_full_refresh_from_manifest(
            &manifest,
            &roots,
            &store,
            &HashMap::new(),
            None,
            baseline.distrusted(),
        )
        .unwrap();
        assert_eq!(plan.overlay_file_count(), 0, "the gate skipped the marked key");

        // ...and the SAME key goes unread through the point path before Phase C.
        fs::write(&file, [0xff, 0xfe]).unwrap();
        cache.refresh_with_manifest(&manifest, &roots, None, 32, &store, false).unwrap();
        assert_eq!(cache.unread_keys_count(), 1);

        let outcome = cache.publish_plan(plan, HashMap::new(), &baseline, None, &store).unwrap();
        let super::PublishOutcome::Applied { gate_deferred, unread_keys, .. } = outcome else {
            panic!("the plan applies");
        };
        assert_eq!(
            unread_keys + gate_deferred,
            1,
            "one unverified file must not be reported as two"
        );
    }

    #[test]
    fn a_root_transition_supersedes_an_old_lock_free_plan() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        fs::write(workspace.join("Module.bsl"), "Процедура Старая()\nКонецПроцедуры").unwrap();
        let store = Store::open(&workspace.join("search.db")).unwrap();
        let roots = single_root(workspace);
        let plan = WorkspaceOverlayCache::plan_full_refresh_from_manifest(
            &HashMap::new(),
            &roots,
            &store,
            &HashMap::new(),
            None,
            &HashSet::new(),
        )
        .unwrap();
        let mut cache = WorkspaceOverlayCache::default();
        let baseline = cache.publication_baseline();

        cache.transition_roots(
            &HashSet::new(),
            &HashSet::new(),
            &HashSet::new(),
            &HashSet::new(),
            Vec::new(),
        );
        let outcome = cache.publish_plan(plan, HashMap::new(), &baseline, None, &store).unwrap();
        assert_eq!(outcome, PublishOutcome::Superseded);
    }

    #[test]
    fn replacing_published_graph_provider_fences_plans_but_keeps_stable_lexical_entries() {
        struct Provider;
        impl crate::ports::GraphContextProvider for Provider {
            fn graph_context(&self, _: &str, _: &str, _: &str) -> Option<String> {
                None
            }
        }

        let mut cache = WorkspaceOverlayCache::default();
        let stable = key("Stable.bsl");
        cache.insert_entry(
            stable.clone(),
            super::OverlayFileEntry {
                fingerprint: super::FileFingerprint {
                    len: 1,
                    modified: None,
                    canonical: PathBuf::from("Stable.bsl"),
                },
                file_hash: vec![1],
                lexical_documents: Vec::new(),
                vector_documents: Vec::new(),
                embedding_inputs: Vec::new(),
            },
        );
        let before = cache.publication_baseline();
        cache.replace_graph_context_provider(std::sync::Arc::new(Provider));

        assert!(cache.entries.contains_key(&stable), "lexical state remains valid");
        assert!(
            cache.wholesale_seq > before.fence,
            "semantic plans from the previous graph source are fenced"
        );
    }

    #[test]
    fn a_rebound_unread_key_inherits_neither_entry_nor_baseline_hiding() {
        let mut cache = WorkspaceOverlayCache::default();
        let rebound = FileKey::new("rebound", "Module.bsl");
        cache.insert_entry(
            rebound.clone(),
            super::OverlayFileEntry {
                fingerprint: super::FileFingerprint {
                    len: 1,
                    modified: None,
                    canonical: PathBuf::from("old/Module.bsl"),
                },
                file_hash: vec![1],
                lexical_documents: Vec::new(),
                vector_documents: Vec::new(),
                embedding_inputs: Vec::new(),
            },
        );
        cache.hidden_paths.insert(rebound.clone());

        cache.transition_roots(
            &HashSet::from(["rebound".to_owned()]),
            &HashSet::from([rebound.clone()]),
            &HashSet::new(),
            &HashSet::from([rebound.clone()]),
            Vec::new(),
        );

        assert!(!cache.entries.contains_key(&rebound));
        assert!(!cache.hidden_paths.contains(&rebound));
        assert!(cache.unread_keys.contains(&rebound));
        assert!(cache.dirty_paths.contains_key(&rebound));
    }

    #[test]
    fn a_root_transition_preserves_stable_cache_state() {
        let mut cache = WorkspaceOverlayCache::default();
        cache.enable_watcher_mode();
        cache.initialized = true;
        cache.full_rescan_pending = true;
        let stable = key("Stable.bsl");
        cache.mark_dirty_path(stable.clone());
        cache.insert_entry(
            stable.clone(),
            super::OverlayFileEntry {
                fingerprint: super::FileFingerprint {
                    len: 1,
                    modified: None,
                    canonical: PathBuf::from("Stable.bsl"),
                },
                file_hash: vec![1],
                lexical_documents: Vec::new(),
                vector_documents: Vec::new(),
                embedding_inputs: Vec::new(),
            },
        );
        cache.embedding_cache.insert("warm".to_owned(), vec![1.0, 2.0]);
        let obsolete = FileKey::new("removed-root", "Obsolete.bsl");
        cache.mark_dirty_path(obsolete.clone());

        let changed_ids = HashSet::from(["removed-root".to_owned()]);
        let cleanup = HashSet::from([obsolete.clone()]);
        cache.transition_roots(
            &changed_ids,
            &cleanup,
            &HashSet::new(),
            &HashSet::new(),
            Vec::new(),
        );

        assert!(cache.watcher_mode);
        assert!(cache.initialized);
        assert!(
            cache.full_rescan_pending,
            "an affected-root transition is not a full publication of stable roots"
        );
        assert!(cache.dirty_paths.contains_key(&stable));
        assert!(cache.entries.contains_key(&stable));
        assert!(!cache.dirty_paths.contains_key(&obsolete));
        assert_eq!(cache.embedding_cache.get("warm"), Some(&vec![1.0, 2.0]));

        let mut cold = WorkspaceOverlayCache::default();
        cold.transition_roots(
            &HashSet::new(),
            &HashSet::new(),
            &HashSet::new(),
            &HashSet::new(),
            Vec::new(),
        );
        assert!(
            !cold.initialized,
            "a transition of affected roots cannot initialize untouched stable roots"
        );
        assert!(!cold.full_rescan_pending, "an absent obligation stays absent");
    }
}
